//! `pastor join` on a new machine: join a head with a token as a pull
//! machine, then bootstrap it with herdr fetched from the head's cache.
//!
//! The token's private key is the one secret in here. It reaches ssh only as
//! a 0600 file in a 0700 dir of the state dir that goes whatever happens
//! (`PrivateDir`: an error, a panic, a signal), never as an argument, never
//! in a log line, and no file pastor keeps holds it. The token itself is
//! read from the terminal, stdin or the environment, and the environment
//! variable is removed before anything else runs.

use std::path::{Path, PathBuf};

use super::{Local, Options, Outcome, StepResult};
use crate::cli::CliError;
use crate::config::Paths;
use crate::ipc::{IpcRequest, IpcResponse};
use crate::join::Token;
use crate::setup::Runner;

/// What `pastor join` is asked besides the token.
#[derive(Debug, Clone)]
pub struct JoinOpts {
    /// The machine's name in the head's flock.toml.
    pub name: String,
    /// The pastor binary to install.
    pub pastor_from: PathBuf,
    /// Join and name the serve only, for a machine set up beforehand: no
    /// herdr is fetched and no service installed.
    pub skip_bootstrap: bool,
}

/// How `pastor join` reaches the head: ssh with one key and pastor's pinned
/// known_hosts. A seam, so the tests answer instead of an sshd.
pub trait HeadLink: Send + Sync {
    /// `line` to the head's `bridge` and its one reply line.
    fn request(
        &self,
        head: &str,
        key: &Path,
        known_hosts: &Path,
        line: &str,
    ) -> anyhow::Result<String>;
    /// `line`, an `ArtifactLocate`, to the head's `bridge`: the reply line,
    /// then the file it names written to `dest`, refused past `max` bytes.
    fn fetch(
        &self,
        head: &str,
        key: &Path,
        known_hosts: &Path,
        line: &str,
        dest: &Path,
        max: u64,
    ) -> anyhow::Result<Fetched>;
}

/// What the head said of an artifact it sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    pub sha256: String,
    pub size: u64,
}

/// The most a token read from stdin or the terminal may be: a few hundred
/// bytes in practice.
const MAX_TOKEN: u64 = 64 * 1024;

/// The variable `pastor join` reads the token from when `--token-stdin` is
/// not given and no terminal is there.
pub const TOKEN_ENV: &str = "PASTOR_JOIN_TOKEN";

/// The token from stdin with `--token-stdin`, else `PASTOR_JOIN_TOKEN`, else
/// what is typed at the terminal. Never an argument: `ps` shows those. The
/// text is dropped as soon as it is decoded, and a refusal never quotes it.
pub fn read_token(
    from_stdin: bool,
    env: Option<String>,
    stdin: &mut dyn std::io::Read,
    tty: &mut dyn FnMut() -> anyhow::Result<Option<String>>,
) -> anyhow::Result<Token> {
    let text = if from_stdin {
        let mut text = String::new();
        std::io::Read::read_to_string(&mut std::io::Read::take(stdin, MAX_TOKEN), &mut text)
            .map_err(|e| {
                CliError::err(
                    crate::join::INVALID,
                    format!("read the token from stdin: {e}"),
                )
            })?;
        text
    } else if let Some(env) = env.filter(|e| !e.trim().is_empty()) {
        env
    } else if let Some(typed) = tty()? {
        typed
    } else {
        return Err(CliError::err(
            "invalid_args",
            "pastor join needs a token: on stdin with --token-stdin, in PASTOR_JOIN_TOKEN, or typed at a terminal; it is never an argument, which `ps` shows",
        ));
    };
    Token::decode(text.trim())
}

/// A line typed at `/dev/tty` with echo off; `None` when there is no
/// terminal to ask on.
pub fn read_tty() -> anyhow::Result<Option<String>> {
    use std::io::{BufRead, Read, Write};
    use std::os::fd::AsRawFd;
    let Ok(mut tty) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
    else {
        return Ok(None);
    };
    /// Echo back on, however the read ends.
    struct Echo(libc::c_int, libc::termios);
    impl Drop for Echo {
        fn drop(&mut self) {
            // SAFETY: the fd is the open tty and the struct came from it.
            unsafe { libc::tcsetattr(self.0, libc::TCSANOW, &self.1) };
        }
    }
    let fd = tty.as_raw_fd();
    // SAFETY: termios is plain data; tcgetattr fills it for the open tty.
    let mut term: libc::termios = unsafe { std::mem::zeroed() };
    let quiet = if unsafe { libc::tcgetattr(fd, &mut term) } == 0 {
        let mut quiet = term;
        quiet.c_lflag &= !libc::ECHO;
        // SAFETY: as above.
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &quiet) };
        Some(Echo(fd, term))
    } else {
        None
    };
    write!(tty, "join token: ")?;
    tty.flush()?;
    let mut line = String::new();
    std::io::BufReader::new(tty.try_clone()?.take(MAX_TOKEN)).read_line(&mut line)?;
    drop(quiet);
    writeln!(tty)?;
    Ok(Some(line))
}

/// `[shepherd] machine` in the pastor.toml `file`, keeping the rest. Returns
/// whether it wrote.
pub fn write_shepherd(file: &Path, name: &str) -> anyhow::Result<bool> {
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| CliError::err("config_error", format!("{}: {e}", file.display())))?;
    if doc
        .get("shepherd")
        .and_then(|t| t.get("machine"))
        .and_then(|m| m.as_str())
        == Some(name)
    {
        return Ok(false);
    }
    if !doc.contains_key("shepherd") {
        doc["shepherd"] = toml_edit::table();
    }
    doc["shepherd"]["machine"] = toml_edit::value(name);
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(file, doc.to_string())?;
    Ok(true)
}

/// Leave `outcome` in `state_dir` for this machine's shepherd, 0600 and
/// whole: a shepherd never reads half a file.
fn write_report(state_dir: &Path, outcome: &Outcome) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    crate::config::create_private_dir(state_dir)?;
    let file = super::report_file(state_dir);
    let tmp = state_dir.join(format!("bootstrap.json.{}", std::process::id()));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(serde_json::to_string(outcome)?.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, &file)?;
    Ok(())
}

/// The key types sshd presents a host key of.
const KEY_TYPES: [&str; 5] = [
    "ssh-ed25519",
    "ssh-rsa",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
];

/// Pin `line` (`host type key`, as `ssh-keyscan` prints it) for `head` in
/// the known_hosts `file`, once. Only that one host: a line with a marker, a
/// pattern or a list would pin other hosts, or another kind of trust.
/// Returns whether it wrote.
fn pin(file: &Path, line: &str, head: &str) -> anyhow::Result<bool> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let line = line.trim();
    let words: Vec<&str> = line.split_whitespace().collect();
    let plain = matches!(
        words[..],
        [name, kind, key]
            if name == crate::join::known_hosts_name(head)
                && KEY_TYPES.contains(&kind)
                && key
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'/' | b'='))
    ) && !line.contains(['\n', '\r']);
    if !plain {
        return Err(CliError::err(
            crate::join::INVALID,
            format!(
                "the token's head host key is not one known_hosts line for {head}: `host type key`, with no marker, pattern or list"
            ),
        ));
    }
    if let Some(dir) = file.parent() {
        crate::config::create_private_dir(dir)?;
    }
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    if text.lines().any(|l| l.trim() == line) {
        return Ok(false);
    }
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(file)?;
    if !text.is_empty() && !text.ends_with('\n') {
        f.write_all(b"\n")?;
    }
    writeln!(f, "{line}")?;
    Ok(true)
}

/// The prefixes of the dirs a join makes in the state dir.
const DIR_PREFIXES: [&str; 2] = ["join", "join-herdr"];

/// The [`PrivateDir`]s that exist now, for a signal to remove: a killed
/// process runs no `Drop`.
static LIVE_DIRS: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// The ssh children a join is running now: a killed join runs no `Drop`,
/// and its ssh would live on, orphaned.
static LIVE_CHILDREN: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

/// A child in `LIVE_CHILDREN` while this lives.
struct LiveChild(u32);

impl LiveChild {
    fn track(child: &std::process::Child) -> LiveChild {
        if let Ok(mut live) = LIVE_CHILDREN.lock() {
            live.push(child.id());
        }
        LiveChild(child.id())
    }
}

impl Drop for LiveChild {
    fn drop(&mut self) {
        if let Ok(mut live) = LIVE_CHILDREN.lock() {
            live.retain(|p| *p != self.0);
        }
    }
}

/// On the way out of a signal: kill and reap every ssh, remove every dir.
fn clean_up_after_a_signal() {
    let children = LIVE_CHILDREN.lock().map(|l| l.clone()).unwrap_or_default();
    for pid in children {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            continue;
        };
        // SAFETY: a pid this process spawned and has not reaped yet (the
        // entry goes once it is waited for); kill and waitpid take plain
        // integers and a null status pointer.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
            libc::waitpid(pid, std::ptr::null_mut(), 0);
        }
    }
    let dirs = LIVE_DIRS.lock().map(|l| l.clone()).unwrap_or_default();
    for dir in dirs {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// A 0700 dir of this user's, removed with all in it when dropped, so the
/// token's key and a fetched herdr never outlive the join, whatever fails.
struct PrivateDir(PathBuf);

impl PrivateDir {
    fn new(state_dir: &Path, prefix: &str) -> anyhow::Result<PrivateDir> {
        use std::os::unix::fs::DirBuilderExt;
        crate::config::create_private_dir(state_dir)?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let dir = state_dir.join(format!("{prefix}-{}-{nanos}", std::process::id()));
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
        if let Ok(mut live) = LIVE_DIRS.lock() {
            live.push(dir.clone());
        }
        Ok(PrivateDir(dir))
    }

    /// `name` in the dir, made 0600 with `text`.
    fn file(&self, name: &str, text: &str) -> anyhow::Result<PathBuf> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let path = self.0.join(name);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        f.write_all(text.as_bytes())?;
        // ssh-keygen ends the key with one; a key without it fails to load.
        if !text.ends_with('\n') {
            f.write_all(b"\n")?;
        }
        Ok(path)
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
        if let Ok(mut live) = LIVE_DIRS.lock() {
            live.retain(|d| *d != self.0);
        }
    }
}

/// The pid in `name` when it is a [`PrivateDir`]'s: a prefix, then
/// `-<pid>-<nanos>`.
fn private_dir_pid(name: &str) -> Option<i32> {
    DIR_PREFIXES.iter().find_map(|prefix| {
        let rest = name.strip_prefix(prefix)?.strip_prefix('-')?;
        let (pid, nanos) = rest.split_once('-')?;
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
        (digits(pid) && digits(nanos)).then(|| pid.parse().ok())?
    })
}

/// Remove the [`PrivateDir`]s an earlier join left in `state_dir` when it
/// was killed past catching: only dirs (not links) named as pastor names
/// them, owned by this user, of a process that is gone, so a join running
/// beside this one keeps its own.
fn remove_stale_dirs(state_dir: &Path) {
    use std::os::unix::fs::MetadataExt;
    let Ok(entries) = std::fs::read_dir(state_dir) else {
        return;
    };
    // SAFETY: geteuid has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(private_dir_pid) else {
            continue;
        };
        let Ok(meta) = entry.path().symlink_metadata() else {
            continue;
        };
        // SAFETY: signal 0 sends nothing; it only checks that pid exists.
        let alive = pid > 0
            && (unsafe { libc::kill(pid, 0) } == 0
                || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM));
        if !meta.is_dir() || meta.uid() != me || alive {
            continue;
        }
        if let Err(err) = std::fs::remove_dir_all(entry.path()) {
            tracing::warn!(dir = %entry.path().display(), %err, "remove a dir an interrupted join left");
        }
    }
}

/// `ssh` to the head with one key, the pinned known_hosts and nothing else:
/// no master to share, no other key, and `bridge` as the remote words, which
/// the head's forced command ignores. `ssh` is the program, `ssh` but in
/// tests.
pub struct SshLink {
    ssh: PathBuf,
}

impl Default for SshLink {
    fn default() -> SshLink {
        SshLink::new("ssh")
    }
}

impl SshLink {
    /// The link through the `ssh` program at `ssh`.
    pub fn new(ssh: impl Into<PathBuf>) -> SshLink {
        SshLink { ssh: ssh.into() }
    }
}

pub(super) fn ssh_argv(head: &str, key: &Path, known_hosts: &Path) -> Vec<String> {
    let mut argv = vec!["-o".to_string(), "ConnectTimeout=30".to_string()];
    let mut rest = crate::ssh::Ssh {
        target: head,
        control_path: None,
        control_persist: "no",
        keepalive: true,
        no_tty: true,
        identity: Some(key),
        extra_identity: None,
        known_hosts: Some(known_hosts),
        accept_new: false,
        config_file: None,
    }
    .args("");
    // `args` quotes the remote command into `sh -c '…'`; the forced command
    // the key runs reads no command at all.
    rest.pop();
    argv.extend(rest);
    argv.push("bridge".into());
    argv
}

/// The longest reply line read from the head, as the head caps a request
/// line.
const MAX_REPLY: u64 = 64 * 1024;

/// The first line of `r` that is not blank, read no further than `max`
/// bytes in all; `None` when there is none.
fn read_reply(r: &mut impl std::io::BufRead, max: u64) -> anyhow::Result<Option<String>> {
    use std::io::{BufRead, Read};
    let mut total = 0;
    loop {
        let mut line = Vec::new();
        let n = r
            .by_ref()
            .take(max + 1 - total)
            .read_until(b'\n', &mut line)?;
        total += n as u64;
        if total > max {
            anyhow::bail!("the head's reply is longer than {} KiB", max / 1024);
        }
        if n == 0 {
            return Ok(None);
        }
        let text = String::from_utf8_lossy(&line);
        if !text.trim().is_empty() {
            return Ok(Some(text.trim_end_matches(['\n', '\r']).to_string()));
        }
    }
}

/// What ssh writes on stderr, read beside its stdout (so neither pipe
/// fills and stalls it), at most 64 KiB.
fn drain_stderr(child: &mut std::process::Child) -> std::thread::JoinHandle<String> {
    let stderr = child.stderr.take();
    std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(e) = stderr {
            let mut bytes = Vec::new();
            let _ = std::io::Read::read_to_end(&mut std::io::Read::take(e, MAX_REPLY), &mut bytes);
            text = String::from_utf8_lossy(&bytes).trim().to_string();
        }
        text
    })
}

/// What ssh wrote on stderr, or that it wrote nothing.
fn failure(stderr: String, status: std::process::ExitStatus) -> String {
    if stderr.is_empty() {
        format!("ssh exited with {status}")
    } else {
        stderr
    }
}

impl SshLink {
    /// ssh to `head` with `line` on its stdin, and `read` to take what it
    /// prints. Its stderr says why when `read` finds nothing.
    fn talk<T>(
        &self,
        head: &str,
        key: &Path,
        known_hosts: &Path,
        line: &str,
        read: impl FnOnce(
            &mut std::io::BufReader<std::process::ChildStdout>,
        ) -> anyhow::Result<Option<T>>,
    ) -> anyhow::Result<T> {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut child = Command::new(&self.ssh)
            .args(ssh_argv(head, key, known_hosts))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("run ssh: {e}"))?;
        let _live = LiveChild::track(&child);
        if let Some(mut stdin) = child.stdin.take() {
            // A bridge that already gave up closes the pipe; its reply, or
            // ssh's stderr, says why.
            let _ = stdin.write_all(line.trim_end().as_bytes());
            let _ = stdin.write_all(b"\n");
        }
        let stderr = drain_stderr(&mut child);
        let read = match child.stdout.take() {
            Some(out) => read(&mut std::io::BufReader::new(out)),
            None => Ok(None),
        };
        // A reply too long to read is not waited out.
        if read.is_err() {
            let _ = child.kill();
        }
        let status = child.wait()?;
        let stderr = stderr.join().unwrap_or_default();
        match read? {
            Some(got) => Ok(got),
            None => anyhow::bail!("{}", failure(stderr, status)),
        }
    }
}

impl HeadLink for SshLink {
    fn request(
        &self,
        head: &str,
        key: &Path,
        known_hosts: &Path,
        line: &str,
    ) -> anyhow::Result<String> {
        self.talk(head, key, known_hosts, line, |out| {
            read_reply(out, MAX_REPLY)
        })
    }

    fn fetch(
        &self,
        head: &str,
        key: &Path,
        known_hosts: &Path,
        line: &str,
        dest: &Path,
        max: u64,
    ) -> anyhow::Result<Fetched> {
        let got = self.talk(head, key, known_hosts, line, |out| {
            let Some(first) = read_reply(out, MAX_REPLY)? else {
                return Ok(None);
            };
            let (sha256, size) = match reply(&first)? {
                IpcResponse::Artifact { sha256, size, .. } => (sha256, size),
                other => anyhow::bail!(
                    "the head answered an artifact request with something else: {other:?}"
                ),
            };
            if size > max {
                return Err(CliError::err(
                    "artifact_checksum_mismatch",
                    format!("the head answered {size} bytes, over the {max} pastor fetches"),
                ));
            }
            let mut file = std::fs::File::create(dest)?;
            let got = std::io::copy(&mut std::io::Read::take(&mut *out, size), &mut file)?;
            if got != size {
                let _ = std::fs::remove_file(dest);
                return Err(CliError::err(
                    "artifact_checksum_mismatch",
                    format!("the head sent {got} of the {size} bytes it answered"),
                ));
            }
            Ok(Some(Fetched { sha256, size }))
        });
        if got.is_err() {
            let _ = std::fs::remove_file(dest);
        }
        got
    }
}

/// The most herdr may be, whatever size the head answers.
const MAX_HERDR: u64 = 512 * 1024 * 1024;

/// A reply line from the head: `Error` as its code, else the response.
fn reply(line: &str) -> anyhow::Result<IpcResponse> {
    match serde_json::from_str::<IpcResponse>(line.trim()) {
        Ok(IpcResponse::Error { code, message }) => Err(CliError::err(&code, message)),
        Ok(resp) => Ok(resp),
        Err(e) => anyhow::bail!(
            "the head's reply is not one pastor reads ({e}): {}",
            line.trim()
        ),
    }
}

/// The error a refused or failed join ssh reads as.
fn join_ssh_error(head: &str, err: &anyhow::Error) -> anyhow::Error {
    let text = format!("{err:#}");
    if text.contains("Permission denied") {
        return CliError::err(
            crate::join::INVALID,
            format!(
                "the head {head} refused the join token's key ({text}): the token was used, revoked or has expired; make a new one with `pastor join create` on the head. A new token refused the same way: is the head's sshd older than OpenSSH 8.2? It rejects the join line's `expiry-time`"
            ),
        );
    }
    CliError::err(
        "head_unreachable",
        format!("join over ssh to {head}: {text}"),
    )
}

/// The steps that make the keys the join sends, run alone before it.
const IDENTITY_STEPS: [&str; 4] = ["facts", "platform", "prerequisites", "keys"];

/// What a join of a machine set up beforehand runs: the keys, and after the
/// join the head in client.toml and the serve's name.
const SKIP_BEFORE: [&str; 2] = ["facts", "keys"];
const SKIP_AFTER: [&str; 4] = ["facts", "keys", "client", "shepherd_name"];

/// Entry point for `pastor join` with a token: join, bootstrap, print the
/// outcome. Runs before any runtime of its own, so the token is out of the
/// environment before a thread or a child exists.
pub fn cli(paths: &Paths, args: &crate::join::JoinArgs) -> anyhow::Result<()> {
    let env = std::env::var(TOKEN_ENV).ok();
    // So no child (a bootstrap command, ssh) has it in /proc/<pid>/environ.
    // SAFETY: no other thread runs yet: main starts none before it calls
    // this, and the runtime starts below.
    unsafe { std::env::remove_var(TOKEN_ENV) };
    let token = read_token(args.token_stdin, env, &mut std::io::stdin(), &mut read_tty)?;
    let name = args.name.clone().unwrap_or_else(crate::config::hostname);
    let config = crate::config::PastorConfig::load(&paths.config_file())?;
    let o = JoinOpts {
        name: name.clone(),
        pastor_from: std::env::current_exe()?,
        skip_bootstrap: args.skip_bootstrap,
    };
    let rt = tokio::runtime::Runtime::new()?;
    let outcome = rt.block_on(join(paths, token, o, config))?;
    if args.json {
        println!("{}", serde_json::to_string(&outcome)?);
    } else {
        print!("{}", super::human(&outcome));
    }
    match super::error(&outcome) {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// `pastor join` on this machine. The ssh blocks, so the work runs off the
/// async threads. SIGINT, SIGTERM or SIGHUP removes the token's key and a
/// fetched herdr, which no `Drop` would, then exits.
async fn join(
    paths: &Paths,
    token: Token,
    o: JoinOpts,
    config: crate::config::PastorConfig,
) -> anyhow::Result<Outcome> {
    use tokio::signal::unix::{SignalKind, signal};
    let (paths, handle) = (paths.clone(), tokio::runtime::Handle::current());
    let mut int = signal(SignalKind::interrupt())?;
    let mut term = signal(SignalKind::terminate())?;
    let mut hup = signal(SignalKind::hangup())?;
    let work = tokio::task::spawn_blocking(move || {
        let local = super::OnThisMachine {
            paths: &paths,
            name: o.name.clone(),
            config,
        };
        handle.block_on(join_with(
            &paths,
            &token,
            &o,
            &crate::setup::SystemRunner,
            &SshLink::default(),
            &local,
        ))
    });
    let code = tokio::select! {
        done = work => return done?,
        _ = int.recv() => 130,
        _ = term.recv() => 143,
        _ = hup.recv() => 129,
    };
    clean_up_after_a_signal();
    std::process::exit(code);
}

/// `join` with the commands it runs, the ssh it speaks and the machinery it
/// runs on this machine as seams.
pub async fn join_with(
    paths: &Paths,
    token: &Token,
    o: &JoinOpts,
    r: &dyn Runner,
    link: &dyn HeadLink,
    local: &dyn Local,
) -> anyhow::Result<Outcome> {
    let name = o.name.clone();
    if !crate::authkeys::valid_name(&name) {
        return Err(CliError::err(
            "invalid_machine",
            format!("{name:?} cannot be a machine name"),
        ));
    }
    let opts = Options {
        name: name.clone(),
        head: Some(token.head.clone()),
        // The head never logs in to a pull machine.
        head_key: None,
        pastor_from: o.pastor_from.clone(),
        paths: paths.clone(),
        // The head's cache gives herdr, at the version it names.
        herdr_pin: None,
    };
    remove_stale_dirs(&paths.state_dir);

    // 1. The head's host key, before the first ssh to it.
    let known_hosts = paths.known_hosts_file();
    pin(&known_hosts, &token.head_host_key, &token.head)?;

    // 2. The keys the join sends. A machine that cannot be set up stops
    // here, before the token is spent.
    let before: &[&str] = if o.skip_bootstrap {
        &SKIP_BEFORE
    } else {
        &IDENTITY_STEPS
    };
    let ids = super::run_steps(r, &opts, local, before).await;
    if let Some(err) = super::error(&ids) {
        return Err(err);
    }
    let (Some(machine_key), Some(agent_key)) = (ids.machine_key.clone(), ids.agent_key.clone())
    else {
        anyhow::bail!("the keys step made no machine or agent key");
    };
    let Some(host_key) = ids.host_key.clone() else {
        return Err(CliError::err(
            "bootstrap_missing",
            "this machine has no /etc/ssh/ssh_host_ed25519_key.pub, which the head records with the join; run `ssh-keygen -A` as root, then join again",
        ));
    };

    // 3. Join with the token's key, which is on disk only while ssh runs.
    let req = IpcRequest::Join {
        // The token's forced command names the id; whatever is here is
        // replaced.
        id: String::new(),
        name: name.clone(),
        machine_key,
        agent_key,
        host_key,
    };
    let line = crate::ipc::request_line(&req, None)?;
    let answered = {
        let dir = PrivateDir::new(&paths.state_dir, "join")?;
        let key = dir.file("join_key", &token.key)?;
        link.request(&token.head, &key, &known_hosts, &line)
    }
    .map_err(|e| join_ssh_error(&token.head, &e))?;
    let herdr_version = match reply(&answered)? {
        IpcResponse::Joined {
            name: joined,
            herdr_version,
        } if joined == name => herdr_version,
        other => anyhow::bail!("the head answered the join with something else: {other:?}"),
    };
    eprintln!("joined the head {} as machine {name}", token.head);

    // 4. herdr from the head's cache, then the rest of bootstrap with it;
    // for a machine set up beforehand, only the head and the serve's name.
    let outcome = if o.skip_bootstrap {
        super::run_steps(r, &opts, local, &SKIP_AFTER).await
    } else {
        let version =
            herdr_version.unwrap_or_else(|| crate::artifacts::DEFAULT_HERDR_VERSION.to_string());
        let facts = super::facts(r);
        let fetched = fetch_herdr(
            link,
            &token.head,
            &paths.machine_key(),
            &known_hosts,
            &facts,
            &version,
            &paths.state_dir,
        );
        match fetched {
            Ok(()) => super::run_steps(r, &opts, local, &super::PULL_STEPS).await,
            Err(err) => {
                let mut outcome = ids;
                outcome.steps.push(StepResult::failed(
                    "herdr",
                    format!("fetch herdr {version} from the head: {err:#}"),
                    format!(
                        "check the head can download herdr (`[artifacts] herdr`, its network), then join again with a token from `pastor join create --replace {name}`"
                    ),
                ));
                outcome.ok = false;
                outcome
            }
        }
    };

    // 5. The report: left in the state dir first, so it survives whatever
    // happens next, then sent to the head at once, so a failure before the
    // shepherd ever runs still reaches it. The file goes once the head has
    // it; otherwise the shepherd sends it.
    let left = write_report(&paths.state_dir, &outcome);
    let sent = send_report(link, token, paths, &known_hosts, &name, &outcome);
    match (sent, left) {
        (Ok(()), Ok(())) => {
            if let Err(err) = std::fs::remove_file(super::report_file(&paths.state_dir)) {
                tracing::warn!(%err, "remove the sent bootstrap report");
            }
        }
        (Ok(()), Err(err)) => {
            tracing::debug!(machine = %name, err = %format!("{err:#}"), "the head has the bootstrap report; none was left for the shepherd");
        }
        (Err(err), Ok(())) => {
            eprintln!(
                "the head did not take the bootstrap report yet ({err:#}); the shepherd sends it when it can"
            );
        }
        (Err(err), Err(again)) => {
            return Err(CliError::err(
                "runtime_error",
                format!(
                    "machine {name} joined, but its bootstrap result reached neither the head ({err:#}) nor a file for its shepherd ({again:#}); the head shows it pending: join again with a token from `pastor join create --replace {name}`"
                ),
            ));
        }
    }
    Ok(outcome)
}

/// `outcome` to the head over the machine key.
fn send_report(
    link: &dyn HeadLink,
    token: &Token,
    paths: &Paths,
    known_hosts: &Path,
    name: &str,
    outcome: &Outcome,
) -> anyhow::Result<()> {
    let req = IpcRequest::BootstrapReport {
        machine: name.to_string(),
        result: outcome.clone(),
    };
    let line = crate::ipc::request_line(&req, None)?;
    let answer = link.request(&token.head, &paths.machine_key(), known_hosts, &line)?;
    reply(&answer).map(|_| ())
}

/// herdr `version` for this machine's target, from the head's cache over the
/// machine key, installed in `~/.local/bin`: sized and summed as the head
/// says before anything installs it.
fn fetch_herdr(
    link: &dyn HeadLink,
    head: &str,
    machine_key: &Path,
    known_hosts: &Path,
    facts: &super::Facts,
    version: &str,
    state_dir: &Path,
) -> anyhow::Result<()> {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;
    let target = crate::artifacts::target_for(&facts.release(), "herdr")?;
    // The words go into a request line and a cache path: plain names only.
    crate::artifacts::Artifact::new("herdr", version, &target)
        .map_err(|e| CliError::err(e.code(), e))?;
    let req = IpcRequest::ArtifactLocate {
        name: "herdr".into(),
        version: version.into(),
        target: target.clone(),
    };
    let line = crate::ipc::request_line(&req, None)?;
    let dir = PrivateDir::new(state_dir, "join-herdr")?;
    let dest = dir.0.join("herdr");
    let Fetched { sha256, size } =
        link.fetch(head, machine_key, known_hosts, &line, &dest, MAX_HERDR)?;
    let mut hasher = Sha256::new();
    let got_size = std::io::copy(&mut std::fs::File::open(&dest)?, &mut hasher)?;
    let got = format!("{:x}", hasher.finalize());
    if got_size != size || !got.eq_ignore_ascii_case(sha256.trim()) {
        return Err(CliError::err(
            "artifact_checksum_mismatch",
            format!(
                "herdr {version} {target} from the head is {got_size} bytes with sha256 {got}, not the {size} bytes with sha256 {sha256} the head answered"
            ),
        ));
    }
    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))?;
    let to = facts.home.join(".local/bin/herdr");
    super::steps::install_file(&dest, &to)
        .map_err(|e| anyhow::anyhow!("install herdr at {}: {e}", to.display()))?;
    Ok(())
}
