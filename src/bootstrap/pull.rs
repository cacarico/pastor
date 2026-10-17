//! `pastor join` on a new machine: join the fleet with a token as a pull
//! machine, then bootstrap it with herdr from the head's cache.

use std::path::{Path, PathBuf};

use super::steps::{self, Ctx, Step, StepOutcome, must, path, succeeded};
use super::{AGENT_USER, Mode, Options, Outcome, StepResult};
use crate::cli::CliError;
use crate::config::Paths;
use crate::ipc::{IpcRequest, IpcResponse};
use crate::join::Token;
use crate::setup::Runner;

/// What `pastor join` is asked for besides the token.
#[derive(Debug, Clone)]
pub struct JoinOpts {
    /// The machine's name in the head's flock.toml; the hostname when unset.
    pub name: Option<String>,
    pub sudo: bool,
    pub service_user: Option<String>,
    /// Join and name the shepherd only, for a machine set up beforehand.
    pub skip_bootstrap: bool,
    /// The pastor binary to install for each user.
    pub pastor_from: PathBuf,
}

/// What pull's own steps need from the join.
#[derive(Debug, Clone, Default)]
pub struct Pull {
    pub(super) head_host_key: String,
    pub(super) name: Option<String>,
}

/// How `pastor join` reaches the head: ssh with one key and pastor's pinned
/// known_hosts. A seam, so the tests answer instead of an sshd.
pub trait HeadLink: Send + Sync {
    /// `line` to `remote` on `head`, and its one reply line.
    fn request(
        &self,
        head: &str,
        key: &Path,
        known_hosts: &Path,
        remote: &[&str],
        line: &str,
    ) -> anyhow::Result<String>;
    /// `line` to `remote` on `head`, its stdout written to `dest`, and
    /// refused once it passes `max` bytes.
    #[allow(clippy::too_many_arguments)]
    fn download(
        &self,
        head: &str,
        key: &Path,
        known_hosts: &Path,
        remote: &[&str],
        line: &str,
        dest: &Path,
        max: u64,
    ) -> anyhow::Result<()>;
}

/// Where a pull machine's outcome waits until the head has it: `pastor
/// join` removes it once its own report is answered, else the shepherd
/// sends it and removes it then.
pub fn report_file(state_dir: &Path) -> PathBuf {
    state_dir.join("bootstrap.json")
}

/// The most a token read from stdin may be: a few hundred bytes in
/// practice.
const MAX_TOKEN: u64 = 64 * 1024;

/// The token from the argument, else stdin with `--token-stdin`, else
/// `PASTOR_JOIN_TOKEN`.
pub fn read_token(
    arg: Option<&str>,
    from_stdin: bool,
    env: Option<String>,
    stdin: &mut dyn std::io::Read,
) -> anyhow::Result<Token> {
    let text = match arg {
        Some(a) => a.to_string(),
        None if from_stdin => {
            let mut text = String::new();
            std::io::Read::read_to_string(&mut std::io::Read::take(stdin, MAX_TOKEN), &mut text)
                .map_err(|e| {
                    CliError::err(
                        crate::join::INVALID,
                        format!("read the token from stdin: {e}"),
                    )
                })?;
            text
        }
        None => match env.filter(|e| !e.trim().is_empty()) {
            Some(e) => e,
            None => {
                return Err(CliError::err(
                    "invalid_args",
                    "pastor join needs a token: on stdin with --token-stdin, in PASTOR_JOIN_TOKEN, or as its argument (which ps shows); or a subcommand: create, list or revoke",
                ));
            }
        },
    };
    Token::decode(text.trim())
}

/// Set `[shepherd] machine` in `file`, keeping the rest. Returns whether it
/// wrote.
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

/// The argv a root-mode shepherd reaches the agent user's herdr with, at
/// `herdr`: through the one command its sudoers drop-in allows
/// (`shepherd_herdr`). `-H` gives herdr the agent user's home, where its
/// socket is.
pub fn shepherd_herdr_argv(herdr: &str) -> Vec<String> {
    ["sudo", "-n", "-H", "-u", AGENT_USER, herdr]
        .into_iter()
        .chain(["--session", "default", "remote-api-bridge"])
        .map(String::from)
        .collect()
}

/// Set `[shepherd] command` in `file` to `shepherd_herdr_argv(herdr)`,
/// keeping the rest. Returns whether it wrote.
pub fn write_shepherd_herdr(file: &Path, herdr: &str) -> anyhow::Result<bool> {
    if !herdr.starts_with('/') || !plain_path(herdr) {
        return Err(CliError::err(
            "invalid_path",
            format!("{herdr:?} is not an absolute path of letters, digits, '.', '_', '/' and '-'"),
        ));
    }
    let argv = shepherd_herdr_argv(herdr);
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| CliError::err("config_error", format!("{}: {e}", file.display())))?;
    let have: Option<Vec<String>> = doc
        .get("shepherd")
        .and_then(|t| t.get("command"))
        .and_then(|c| c.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        });
    if have.as_ref() == Some(&argv) {
        return Ok(false);
    }
    if !doc.contains_key("shepherd") {
        doc["shepherd"] = toml_edit::table();
    }
    doc["shepherd"]["command"] = toml_edit::value(argv.iter().collect::<toml_edit::Array>());
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(file, doc.to_string())?;
    Ok(true)
}

/// Letters, digits, `.`, `_`, `/` and `-` only, and no leading `-`: safe in
/// a sudoers rule and on a command line unquoted.
fn plain_path(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b'-'))
}

/// Leave `json`, an [`Outcome`], in `state_dir` for this user's shepherd,
/// 0600 and whole: a shepherd never reads half a file.
pub fn write_report(state_dir: &Path, json: &str) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let outcome: Outcome = serde_json::from_str(json)
        .map_err(|e| CliError::err("invalid_args", format!("not a bootstrap outcome: {e}")))?;
    crate::config::create_private_dir(state_dir)?;
    let file = report_file(state_dir);
    let tmp = state_dir.join(format!("bootstrap.json.{}", std::process::id()));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(serde_json::to_string(&outcome)?.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, &file)?;
    Ok(())
}

/// Whether `line` is one plain known_hosts line for `head` (`user@host`):
/// `host type key` or `[host]:port type key`, no marker (`@cert-authority`,
/// `@revoked`), pattern, negation or list, and a key type sshd logs in
/// with. Anything more would pin other hosts, or another kind of trust.
fn pinnable(line: &str, head: &str) -> bool {
    let (host, _) = crate::ssh::destination_host_port(head);
    let words: Vec<&str> = line.split_whitespace().collect();
    let [name, kind, key] = words[..] else {
        return false;
    };
    let plain = |h: &str| {
        !h.is_empty()
            && !h.starts_with('-')
            && h.bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_' | b':'))
    };
    let named = name == host
        || name
            .strip_prefix('[')
            .and_then(|r| r.split_once("]:"))
            .is_some_and(|(h, port)| {
                h == host && !port.is_empty() && port.bytes().all(|c| c.is_ascii_digit())
            });
    named
        && plain(host)
        && crate::join::KEY_TYPES.contains(&kind)
        && !key.is_empty()
        && key
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'+' | b'/' | b'='))
}

/// Pin `line` (`host type key`) for `head` in the known_hosts `file`,
/// once. Returns whether it wrote.
fn pin(file: &Path, line: &str, head: &str) -> anyhow::Result<bool> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let line = line.trim();
    if line.contains(['\n', '\r']) || !pinnable(line, head) {
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

/// Remove every [`PrivateDir`] there is, on the way out of a signal.
fn remove_live_dirs() {
    let dirs = LIVE_DIRS.lock().map(|l| l.clone()).unwrap_or_default();
    for dir in dirs {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Children by pid, for a signal to stop.
struct Children(std::sync::Mutex<Vec<u32>>);

/// The ssh children a join is running now: a killed join runs no `Drop`,
/// and its ssh would live on, orphaned.
static LIVE_CHILDREN: Children = Children(std::sync::Mutex::new(Vec::new()));

/// A child in a [`Children`] while this lives; dropped once it was waited
/// for.
struct LiveChild<'a> {
    list: &'a Children,
    pid: u32,
}

impl Children {
    fn track(&self, child: &std::process::Child) -> LiveChild<'_> {
        let pid = child.id();
        if let Ok(mut live) = self.0.lock() {
            live.push(pid);
        }
        LiveChild { list: self, pid }
    }

    /// Kill and reap every child there is, on the way out of a signal.
    fn kill_all(&self) {
        let Ok(live) = self.0.lock() else {
            return;
        };
        kill_and_reap(&live);
    }
}

impl Drop for LiveChild<'_> {
    fn drop(&mut self) {
        if let Ok(mut live) = self.list.0.lock() {
            live.retain(|p| *p != self.pid);
        }
    }
}

/// SIGKILL each of `pids`, then wait for it.
fn kill_and_reap(pids: &[u32]) {
    for &pid in pids {
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
/// was killed: only dirs (not links) named as pastor names them, owned by
/// this user, of a process that is gone, so a join running beside this one
/// keeps its own.
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
        if !meta.is_dir() || meta.uid() != me || process_alive(pid) {
            continue;
        }
        if let Err(err) = std::fs::remove_dir_all(entry.path()) {
            tracing::warn!(dir = %entry.path().display(), %err, "remove a dir an interrupted join left");
        }
    }
}

/// Whether a process `pid` is there: one of another user's counts.
fn process_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 sends nothing; it only checks that pid exists.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// `ssh` to the head with one key, the pinned known_hosts and nothing else:
/// no master to share (each join's few connections are not worth one),
/// no other key, and the remote command as plain words, which the head's
/// forced command reads. `ssh` is the program, `ssh` but in tests.
pub struct SshLink {
    ssh: PathBuf,
}

impl Default for SshLink {
    fn default() -> SshLink {
        SshLink { ssh: "ssh".into() }
    }
}

fn ssh_argv(head: &str, key: &Path, known_hosts: &Path, remote: &[&str]) -> Vec<String> {
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
    }
    .args("");
    // `args` quotes the remote command into `sh -c '…'`; the forced command
    // the key runs reads SSH_ORIGINAL_COMMAND word by word instead.
    rest.pop();
    argv.extend(rest);
    argv.extend(remote.iter().map(|w| w.to_string()));
    argv
}

/// The longest reply line read from the head, as the head caps a request
/// line.
const MAX_REPLY: u64 = 64 * 1024;

/// The first line of `r` that is not blank, read no further than `max`
/// bytes in all; `None` when there is none.
fn read_reply(r: impl std::io::Read, max: u64) -> anyhow::Result<Option<String>> {
    use std::io::BufRead;
    let mut r = std::io::BufReader::new(r.take(max + 1));
    let mut total = 0;
    loop {
        let mut line = Vec::new();
        let n = r.read_until(b'\n', &mut line)?;
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

/// Copy `r` to `w`, failing once more than `max` bytes come; returns the
/// count.
fn copy_capped(
    r: impl std::io::Read,
    w: &mut impl std::io::Write,
    max: u64,
) -> anyhow::Result<u64> {
    let n = std::io::copy(&mut r.take(max + 1), w)?;
    if n > max {
        return Err(CliError::err(
            "artifact_checksum_mismatch",
            format!("the head sent more than the {max} bytes it answered"),
        ));
    }
    Ok(n)
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

impl HeadLink for SshLink {
    fn request(
        &self,
        head: &str,
        key: &Path,
        known_hosts: &Path,
        remote: &[&str],
        line: &str,
    ) -> anyhow::Result<String> {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut child = Command::new(&self.ssh)
            .args(ssh_argv(head, key, known_hosts, remote))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("run ssh: {e}"))?;
        let _live = LIVE_CHILDREN.track(&child);
        if let Some(mut stdin) = child.stdin.take() {
            // A bridge that already gave up closes the pipe; its reply, or
            // ssh's stderr, says why.
            let _ = stdin.write_all(line.trim_end().as_bytes());
            let _ = stdin.write_all(b"\n");
        }
        let stderr = drain_stderr(&mut child);
        let reply = match child.stdout.take() {
            Some(out) => read_reply(out, MAX_REPLY),
            None => Ok(None),
        };
        // A reply too long to read is not waited out.
        if reply.is_err() {
            let _ = child.kill();
        }
        let status = child.wait()?;
        let stderr = stderr.join().unwrap_or_default();
        match reply? {
            Some(reply) => Ok(reply),
            None => anyhow::bail!("{}", failure(stderr, status)),
        }
    }

    fn download(
        &self,
        head: &str,
        key: &Path,
        known_hosts: &Path,
        remote: &[&str],
        line: &str,
        dest: &Path,
        max: u64,
    ) -> anyhow::Result<()> {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut file = std::fs::File::create(dest)?;
        let mut child = Command::new(&self.ssh)
            .args(ssh_argv(head, key, known_hosts, remote))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("run ssh: {e}"))?;
        let _live = LIVE_CHILDREN.track(&child);
        if let Some(mut stdin) = child.stdin.take() {
            // The one request line, then end of input; a bridge that gave
            // up already says why on stderr.
            let _ = stdin.write_all(line.trim_end().as_bytes());
            let _ = stdin.write_all(b"\n");
        }
        let stderr = drain_stderr(&mut child);
        let copied = match child.stdout.take() {
            Some(out) => copy_capped(out, &mut file, max),
            None => Ok(0),
        };
        if copied.is_err() {
            let _ = child.kill();
            // Nothing past the cap stays on disk.
            drop(file);
            let _ = std::fs::remove_file(dest);
        }
        let status = child.wait()?;
        let err = stderr.join().unwrap_or_default();
        copied?;
        if status.success() {
            return Ok(());
        }
        // The bridge writes its own failure on stderr as `{code, message}`.
        if let Some(e) = err
            .lines()
            .find_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["code"].is_string())
        {
            return Err(CliError::err(
                e["code"].as_str().unwrap_or("runtime_error"),
                e["message"].as_str().unwrap_or_default().to_string(),
            ));
        }
        anyhow::bail!("{}", failure(err, status))
    }
}

/// The most herdr may be, whatever size the head answers.
const MAX_HERDR: u64 = 512 * 1024 * 1024;

/// The variable `pastor join` reads the token from when neither the
/// argument nor `--token-stdin` gives one.
pub const TOKEN_ENV: &str = "PASTOR_JOIN_TOKEN";

/// Entry point for `pastor join` with a token: join, bootstrap, print the
/// outcome.
pub fn cli(args: &crate::join::JoinArgs) -> anyhow::Result<()> {
    let env = std::env::var(TOKEN_ENV).ok();
    // Out of the environment before anything runs, so no child (the
    // bootstrap's commands, ssh) has it in /proc/<pid>/environ.
    // SAFETY: no other thread runs yet: the tokio runtime starts below,
    // and main starts none before it calls this.
    unsafe { std::env::remove_var(TOKEN_ENV) };
    let token = read_token(
        args.token.as_deref(),
        args.token_stdin,
        env,
        &mut std::io::stdin(),
    )?;
    let paths = Paths::from_env()?;
    let o = JoinOpts {
        name: args.name.clone(),
        sudo: args.sudo,
        service_user: args.service_user.clone(),
        skip_bootstrap: args.skip_bootstrap,
        pastor_from: std::env::current_exe()?,
    };
    let rt = tokio::runtime::Runtime::new()?;
    let outcome = rt.block_on(join(&paths, token, o))?;
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

/// `pastor join` on this machine. The ssh and the bootstrap block, so they
/// run off the async threads. SIGINT, SIGTERM or SIGHUP removes the
/// token's key and a fetched herdr, which no `Drop` would, then exits.
pub async fn join(paths: &Paths, token: Token, o: JoinOpts) -> anyhow::Result<Outcome> {
    use tokio::signal::unix::{SignalKind, signal};
    let paths = paths.clone();
    let mut int = signal(SignalKind::interrupt())?;
    let mut term = signal(SignalKind::terminate())?;
    let mut hup = signal(SignalKind::hangup())?;
    let work = tokio::task::spawn_blocking(move || {
        join_with(
            &paths,
            &token,
            &o,
            &crate::setup::SystemRunner,
            &SshLink::default(),
        )
    });
    let code = tokio::select! {
        done = work => return done?,
        _ = int.recv() => 130,
        _ = term.recv() => 143,
        _ = hup.recv() => 129,
    };
    LIVE_CHILDREN.kill_all();
    remove_live_dirs();
    std::process::exit(code);
}

/// The steps that make the users and their keys, run alone before the join
/// so it has keys to send.
fn identity_steps() -> Vec<(&'static str, Step)> {
    let mut list: Vec<(&str, Step)> = ["facts", "platform", "users", "keys"]
        .into_iter()
        .map(|n| (n, steps::named(n)))
        .collect();
    list.push(("known_hosts", known_hosts));
    list
}

/// Every bootstrap step, with pull's own: the head pinned for each user,
/// the shepherd's way to the agent user's herdr once client.toml is
/// written, then the shepherd's service last, once the rest is there.
fn all_steps() -> Vec<(&'static str, Step)> {
    let mut list = steps::STEPS.to_vec();
    let after = |list: &[(&str, Step)], name: &str| {
        list.iter()
            .position(|(n, _)| *n == name)
            .map_or(list.len(), |i| i + 1)
    };
    let at = after(&list, "keys");
    list.insert(at, ("known_hosts", known_hosts));
    let at = after(&list, "client");
    list.insert(at, ("shepherd_herdr", shepherd_herdr));
    list.push(("shepherd", shepherd));
    list
}

/// The error a refused or failed join ssh reads as.
fn join_ssh_error(head: &str, err: &anyhow::Error) -> anyhow::Error {
    let text = format!("{err:#}");
    if text.contains("Permission denied") {
        return CliError::err(
            crate::join::INVALID,
            format!(
                "the head {head} refused the join token's key ({text}): the token was used, revoked or has expired; make a new one with `pastor join create` on the head"
            ),
        );
    }
    CliError::err(
        "head_unreachable",
        format!("join over ssh to {head}: {text}"),
    )
}

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

/// `join` with the commands it runs and the ssh it speaks as seams.
pub fn join_with(
    paths: &Paths,
    token: &Token,
    o: &JoinOpts,
    r: &dyn Runner,
    link: &dyn HeadLink,
) -> anyhow::Result<Outcome> {
    if let Some(user) = &o.service_user {
        super::check_user(user)?;
    }
    let name = o
        .name
        .clone()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(crate::config::hostname);
    // The head refuses it too, but not before this machine made users.
    crate::join::check_name(&name)?;
    // The fetch and the report go over the service user's machine key, in
    // its 0700 state dir: from a login that is neither that user nor root,
    // ssh could not read it, and the machine would stay pending. Refused
    // before the token is spent.
    if let Some(user) = &o.service_user {
        let uid = super::facts::stdout(r, "id", &["-u"]).unwrap_or_default();
        let login = super::facts::stdout(r, "id", &["-un"]).unwrap_or_default();
        if uid.trim() != "0" && login.trim() != user {
            return Err(CliError::err(
                "usage_error",
                format!(
                    "--service-user {user} needs pastor join to run as root (sudo pastor join ...): it reaches the head with {user}'s machine key, which only {user} and root can read"
                ),
            ));
        }
    }
    let opts = Options {
        sudo: o.sudo,
        service_user: o.service_user.clone(),
        head: token.head.clone(),
        // The head never logs in to a pull machine.
        head_key: None,
        herdr_from: None,
        pastor_from: o.pastor_from.clone(),
    };
    let mut pull = Pull {
        head_host_key: token.head_host_key.trim().to_string(),
        name: None,
    };

    remove_stale_dirs(&paths.state_dir);

    // 1. The head's host key, before the first ssh to it.
    let known_hosts = paths.known_hosts_file();
    pin(&known_hosts, &pull.head_host_key, &token.head)?;

    // 2. The users and their keys, and the head pinned for each.
    let ids = super::run_steps(r, &opts, Some(&pull), &identity_steps());
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
    let facts = super::facts(r);

    // 3. Join with the token's key, which is on disk only while ssh runs.
    let req = IpcRequest::Join {
        // The token's forced command names the id; whatever is here is
        // replaced.
        id: String::new(),
        name: name.clone(),
        machine_key,
        agent_key,
        host_key,
        facts: serde_json::json!({
            "os": facts.os,
            "arch": facts.arch,
            "userland_bits": facts.userland_bits,
            "hostname": facts.hostname,
        }),
        service_user: ids.service_user.clone(),
    };
    let line = crate::ipc::request_line(&req, None)?;
    let answered = {
        let dir = PrivateDir::new(&paths.state_dir, "join")?;
        let key = dir.file("join_key", &token.key)?;
        link.request(&token.head, &key, &known_hosts, &["bridge"], &line)
    };
    let answered = answered.map_err(|e| join_ssh_error(&token.head, &e))?;
    let (name, herdr_version) = match reply(&answered)? {
        IpcResponse::Joined {
            name,
            herdr_version,
        } => (name, herdr_version),
        other => anyhow::bail!("the head answered the join with something else: {other:?}"),
    };
    pull.name = Some(name.clone());
    eprintln!("joined the head {} as machine {name}", token.head);
    let service_state = super::with_ctx(r, &opts, Some(&pull), |c| c.service(r).map(|u| u.state));
    let service_state = service_state.map_err(|s| {
        anyhow::anyhow!(
            "machine {name} joined, but its service user is gone: {}",
            s.why.unwrap_or_default()
        )
    })?;
    let machine_key = service_state.join("machine_key");

    // 4. The shepherd's name, which survives its restarts.
    let named = super::run_steps(r, &opts, Some(&pull), &[("shepherd_name", shepherd_name)]);
    let mut outcome = ids;
    outcome.steps.extend(named.steps.clone());
    outcome.ok &= named.ok;

    // 5. The rest of bootstrap, with herdr from the head's cache.
    if outcome.ok && !o.skip_bootstrap {
        let version =
            herdr_version.unwrap_or_else(|| crate::artifacts::DEFAULT_HERDR_VERSION.to_string());
        let fetched = PrivateDir::new(&paths.state_dir, "join-herdr").and_then(|dir| {
            let herdr = fetch_herdr(
                link,
                &token.head,
                &machine_key,
                &known_hosts,
                &facts,
                &version,
                &dir.0,
            )?;
            Ok((dir, herdr))
        });
        outcome = match fetched {
            Ok((_dir, herdr)) => {
                let opts = Options {
                    herdr_from: Some(herdr),
                    ..opts.clone()
                };
                super::run_steps(r, &opts, Some(&pull), &all_steps())
            }
            Err(err) => {
                let mut failed = StepResult::failed(
                    format!("fetch herdr {version} from the head: {err:#}"),
                    format!(
                        "check the head can download herdr (`[provision] herdr_index`, its network), then join again with a token from `pastor join create --replace {name}`"
                    ),
                );
                failed.step = "herdr".into();
                outcome.steps.push(failed);
                outcome.ok = false;
                outcome
            }
        };
    }

    // 6. The report: left in the service user's state dir first, so it
    // survives whatever happens next, then sent to the head over the
    // machine key at once, so a failure before the shepherd ever runs still
    // reaches the head. The file goes once the head has it; otherwise the
    // shepherd sends it after its first claim.
    let json = serde_json::to_string(&outcome)?;
    let left = super::with_ctx(r, &opts, Some(&pull), |c| leave_report(r, c, &json))
        .map_err(|s| anyhow::anyhow!("{}", s.why.unwrap_or_default()));
    let send = || -> anyhow::Result<()> {
        let req = IpcRequest::BootstrapReport {
            machine: name.clone(),
            result: outcome.clone(),
        };
        let line = crate::ipc::request_line(&req, None)?;
        let answer = link.request(&token.head, &machine_key, &known_hosts, &["bridge"], &line)?;
        reply(&answer).map(|_| ())
    };
    match (send(), left) {
        (Ok(()), Ok(())) => {
            let _ = super::with_ctx(r, &opts, Some(&pull), |c| drop_report(r, c));
        }
        (Ok(()), Err(err)) => {
            tracing::debug!(machine = %name, err = %format!("{err:#}"), "the head has the bootstrap report; none was left for the shepherd");
        }
        (Err(err), Ok(())) => {
            eprintln!(
                "the head did not take the bootstrap report yet ({err:#}); the shepherd sends it after its first claim"
            );
        }
        (Err(err), Err(again)) => {
            return Err(CliError::err(
                "runtime_error",
                format!(
                    "machine {name} joined, but its bootstrap result reached neither the head ({err:#}) nor its shepherd ({again:#}); the head shows it pending: join again with a token from `pastor join create --replace {name}`"
                ),
            ));
        }
    }
    Ok(outcome)
}

/// herdr `version` for this machine's target, from the head's cache over
/// the machine key, into `dir`: sized and summed as the head says before
/// anything installs it.
fn fetch_herdr(
    link: &dyn HeadLink,
    head: &str,
    machine_key: &Path,
    known_hosts: &Path,
    facts: &super::Facts,
    version: &str,
    dir: &Path,
) -> anyhow::Result<PathBuf> {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;
    let target = super::target_for(facts, "herdr")?;
    // The words go into a request line and a cache path: plain names only.
    let artifact = crate::artifacts::Artifact::new("herdr", version, &target)
        .map_err(|e| CliError::err(e.code(), e))?;
    let req = IpcRequest::ArtifactLocate {
        name: "herdr".into(),
        version: version.into(),
        target: target.clone(),
    };
    let line = crate::ipc::request_line(&req, None)?;
    let (sha256, size) =
        match reply(&link.request(head, machine_key, known_hosts, &["bridge"], &line)?)? {
            IpcResponse::Artifact { sha256, size, .. } => (sha256, size),
            other => anyhow::bail!(
                "the head answered an artifact request with something else: {other:?}"
            ),
        };
    if size > MAX_HERDR {
        return Err(CliError::err(
            "artifact_checksum_mismatch",
            format!(
                "the head answered herdr {version} {target} is {size} bytes, over the {} MiB pastor fetches",
                MAX_HERDR / 1024 / 1024
            ),
        ));
    }
    let dest = dir.join("herdr");
    // sshd hands a forced command none of the client's argv, so the fetch
    // goes as the bridge's first request line.
    link.download(
        head,
        machine_key,
        known_hosts,
        &["bridge"],
        &crate::bridge::fetch_line(&artifact),
        &dest,
        size,
    )?;
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
    Ok(dest)
}

/// pastor's own pinned head in each user's known_hosts, which its ssh to
/// the head reads once it holds a pastor key.
fn known_hosts(r: &dyn Runner, c: &Ctx) -> StepResult {
    known_hosts_inner(r, c).unwrap_or_else(|e| e)
}

fn known_hosts_inner(r: &dyn Runner, c: &Ctx) -> StepOutcome {
    let Some(line) = c.pull.map(|p| p.head_host_key.as_str()) else {
        return Ok(StepResult::skipped("no head host key to pin"));
    };
    let mut changed = false;
    for u in c.users(r)? {
        let state = path(&u.state);
        let file = format!("{state}/known_hosts");
        if succeeded(c.as_user(r, &u, "grep", &["-qxF", "-e", line, "--", &file])) {
            continue;
        }
        let mkdir = ["-p", state.as_str()];
        must(c.as_user(r, &u, "mkdir", &mkdir), "mkdir", &mkdir, "rerun")?;
        // `$1` and `$2` keep the line out of the shell's parsing.
        let append = [
            "-c",
            "umask 077 && printf '%s\\n' \"$1\" >> \"$2\"",
            "sh",
            line,
            file.as_str(),
        ];
        must(c.as_user(r, &u, "sh", &append), "sh", &append[..2], "rerun")?;
        changed = true;
    }
    Ok(StepResult::changed(changed))
}

/// `[shepherd] machine` in the service user's pastor.toml, written by that
/// user's own pastor so the file stays theirs.
fn shepherd_name(r: &dyn Runner, c: &Ctx) -> StepResult {
    shepherd_name_inner(r, c).unwrap_or_else(|e| e)
}

fn shepherd_name_inner(r: &dyn Runner, c: &Ctx) -> StepOutcome {
    let Some(name) = c.pull.and_then(|p| p.name.as_deref()) else {
        return Ok(StepResult::skipped("not joined yet"));
    };
    let u = c.service(r)?;
    let pastor = c.readable(r, &path(&c.o.pastor_from), "pastor")?;
    let args = ["bootstrap", "--write-shepherd", name];
    let out = must(c.as_user(r, &u, &pastor, &args), &pastor, &args, "rerun")?;
    Ok(StepResult::changed(out.stdout.trim() != "unchanged"))
}

/// Root mode: the shepherd runs as the service user and herdr as the agent
/// user, so the local herdr session the shepherd would look for under its
/// own home is not there. A sudoers drop-in (`/etc/sudoers.d/pastor-<service
/// user>`, 0440, checked with `visudo -c`) lets the service user run exactly
/// one command as the agent user, NOPASSWD: that user's herdr as
/// `remote-api-bridge` on its default session. `[shepherd] command` in the
/// service user's pastor.toml runs it (`shepherd_herdr_argv`). User mode
/// needs neither: there both are the login user.
fn shepherd_herdr(r: &dyn Runner, c: &Ctx) -> StepResult {
    shepherd_herdr_inner(r, c).unwrap_or_else(|e| e)
}

fn shepherd_herdr_inner(r: &dyn Runner, c: &Ctx) -> StepOutcome {
    if c.mode != Mode::Root {
        return Ok(StepResult::skipped(
            "user mode: the shepherd reaches its own herdr",
        ));
    }
    let service = c.service(r)?;
    let agent = c.agent(r)?;
    if service.name == agent.name {
        return Ok(StepResult::skipped(
            "the shepherd and herdr run as one user",
        ));
    }
    let herdr = path(&agent.bin().join("herdr"));
    // Both go into the rule unquoted; a sudoers drop-in whose name holds a
    // `.` is skipped by sudo, and a user name never does.
    if !plain_path(&herdr) || !plain_path(&service.name) || service.name.contains(['.', '/']) {
        return Err(StepResult::failed(
            format!("{} or {herdr} cannot go in a sudoers rule", service.name),
            "pick a --service-user of lowercase letters, digits, _ and -",
        ));
    }
    if !succeeded(r.run("sh", &["-c", "command -v sudo && command -v visudo"])) {
        return Err(StepResult::failed(
            "sudo is not installed, and the shepherd reaches the agent user's herdr through it",
            "install sudo, then rerun",
        )
        .with_missing(vec!["sudo".into()]));
    }
    let mut changed = false;
    let file = format!("/etc/sudoers.d/pastor-{}", service.name);
    let rule = format!(
        "{} ALL=({AGENT_USER}) NOPASSWD: {herdr} --session default remote-api-bridge\n",
        service.name
    );
    let have = c
        .as_root(r, "cat", &["--", &file])
        .ok()
        .filter(|o| o.success)
        .map(|o| o.stdout);
    if have.as_deref().map(str::trim) != Some(rule.trim()) {
        // Written beside, checked, then renamed: a rule visudo refuses
        // never reaches the file sudo reads, which would lock sudo out.
        // `$1` and `$2` keep the rule out of the shell's parsing.
        let write = [
            "-c",
            "set -e; umask 0337; t=\"$2.pastor-new\"; printf '%s' \"$1\" > \"$t\"; chmod 0440 \"$t\"; \
             if visudo -cqf \"$t\"; then mv -f \"$t\" \"$2\"; else rm -f \"$t\"; exit 3; fi",
            "sh",
            rule.as_str(),
            file.as_str(),
        ];
        must(
            c.as_root(r, "sh", &write),
            "sh",
            &["-c", &format!("<write {file}>")],
            "check that /etc/sudoers.d exists, then rerun",
        )?;
        changed = true;
    }
    // sudo skips /etc/sudoers.d unless /etc/sudoers includes it.
    let check = [
        "-n",
        "-l",
        "-U",
        service.name.as_str(),
        herdr.as_str(),
        "--session",
        "default",
        "remote-api-bridge",
    ];
    must(
        c.as_root(r, "sudo", &check),
        "sudo",
        &check,
        "add `@includedir /etc/sudoers.d` to /etc/sudoers with visudo, then rerun",
    )?;
    let pastor = c.readable(r, &path(&c.o.pastor_from), "pastor")?;
    let args = ["bootstrap", "--write-shepherd-herdr", herdr.as_str()];
    let out = must(
        c.as_user(r, &service, &pastor, &args),
        &pastor,
        &args,
        "rerun",
    )?;
    changed |= out.stdout.trim() != "unchanged";
    Ok(StepResult::changed(changed))
}

/// The outcome in the service user's state dir, for its shepherd to report
/// after its first claim.
fn leave_report(r: &dyn Runner, c: &Ctx, json: &str) -> Result<(), StepResult> {
    let u = c.service(r)?;
    let pastor = c.readable(r, &path(&c.o.pastor_from), "pastor")?;
    let args = ["bootstrap", "--write-report", json];
    must(
        c.as_user(r, &u, &pastor, &args),
        &pastor,
        &["bootstrap", "--write-report"],
        "rerun",
    )?;
    Ok(())
}

/// The report `leave_report` left, gone once the head has it, so the
/// shepherd does not send it again. As the service user, whose dir it is.
fn drop_report(r: &dyn Runner, c: &Ctx) -> Result<(), StepResult> {
    let u = c.service(r)?;
    let file = path(&report_file(&u.state));
    let args = ["-f", "--", file.as_str()];
    must(c.as_user(r, &u, "rm", &args), "rm", &args, "rerun")?;
    Ok(())
}

/// The shepherd's service as the service user, through `pastor setup`,
/// which owns the unit file: on a pull machine it is what claims tasks.
fn shepherd(r: &dyn Runner, c: &Ctx) -> StepResult {
    shepherd_inner(r, c).unwrap_or_else(|e| e)
}

fn shepherd_inner(r: &dyn Runner, c: &Ctx) -> StepOutcome {
    let u = c.service(r)?;
    let pastor = path(&u.bin().join("pastor"));
    let args: &[&str] = if c.facts.os == "darwin" {
        &["setup", "launchd", "--yes"]
    } else {
        if !c.user_bus {
            return Err(StepResult::failed(
                "XDG_RUNTIME_DIR is unset, so `systemctl --user` has no user bus to reach",
                "rerun with --sudo, which gives each user its own bus, or from a login session",
            ));
        }
        steps::wait_runtime_dir(r, c, &u)?;
        // Lingering is on (the users step, in root mode) or setup refuses.
        &["setup", "systemd", "--yes", "--require-linger"]
    };
    let out = must(
        c.as_user(r, &u, &pastor, args),
        &pastor,
        args,
        "read the error above, fix it, then rerun",
    )?;
    Ok(StepResult::changed(!out.stdout.contains("is up to date")))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::bootstrap::StepStatus;
    use crate::bootstrap::tests::{FakeRunner, runs_as, step};

    fn token() -> Token {
        Token {
            head: "me@head".into(),
            head_host_key: "head ssh-ed25519 AAAAHEAD".into(),
            key: "-----BEGIN OPENSSH PRIVATE KEY-----\nSECRET\n-----END OPENSSH PRIVATE KEY-----\n"
                .into(),
        }
    }

    fn opts() -> JoinOpts {
        JoinOpts {
            name: Some("gpu-3".into()),
            sudo: false,
            service_user: None,
            skip_bootstrap: false,
            pastor_from: PathBuf::from("/tmp/cache/pastor"),
        }
    }

    fn paths() -> (tempfile::TempDir, Paths) {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        (tmp, p)
    }

    /// One ssh the join made.
    #[derive(Debug, Clone)]
    struct Seen {
        key: PathBuf,
        remote: Vec<String>,
        line: String,
        /// A download's cap.
        max: Option<u64>,
        /// The key file's mode and its dir's, and its text, while ssh ran.
        key_mode: u32,
        dir_mode: u32,
        key_text: String,
    }

    /// A head that answers the join with `joined`, an artifact request with
    /// the sum of `herdr` (or `sum` when set), and a fetch with `herdr`.
    struct FakeLink {
        joined: Result<String, String>,
        herdr: Vec<u8>,
        sum: Option<String>,
        /// The size the head answers, when not `herdr`'s.
        size: Option<u64>,
        /// The head does not take the bootstrap report.
        refuse_report: bool,
        seen: Mutex<Vec<Seen>>,
    }

    impl FakeLink {
        fn new(joined: Result<String, String>) -> FakeLink {
            FakeLink {
                joined,
                herdr: b"HERDR".to_vec(),
                sum: None,
                size: None,
                refuse_report: false,
                seen: Mutex::new(Vec::new()),
            }
        }

        fn joins(name: &str, herdr: Option<&str>) -> FakeLink {
            let reply = IpcResponse::Joined {
                name: name.into(),
                herdr_version: herdr.map(str::to_string),
            };
            FakeLink::new(Ok(serde_json::to_string(&reply).unwrap()))
        }

        fn seen(&self) -> Vec<Seen> {
            self.seen.lock().unwrap().clone()
        }

        fn record(&self, key: &Path, remote: &[&str], line: &str, max: Option<u64>) {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| {
                std::fs::metadata(p)
                    .map(|m| m.permissions().mode() & 0o777)
                    .unwrap_or(0)
            };
            self.seen.lock().unwrap().push(Seen {
                key: key.to_path_buf(),
                remote: remote.iter().map(|s| s.to_string()).collect(),
                line: line.to_string(),
                max,
                key_mode: mode(key),
                dir_mode: key.parent().map_or(0, mode),
                key_text: std::fs::read_to_string(key).unwrap_or_default(),
            });
        }
    }

    impl HeadLink for FakeLink {
        fn request(
            &self,
            _head: &str,
            key: &Path,
            _known_hosts: &Path,
            remote: &[&str],
            line: &str,
        ) -> anyhow::Result<String> {
            self.record(key, remote, line, None);
            let (req, _) = crate::ipc::parse_request_line(line.trim()).unwrap();
            let reply = match req {
                IpcRequest::Join { .. } => {
                    return self.joined.clone().map_err(anyhow::Error::msg);
                }
                IpcRequest::ArtifactLocate { .. } => {
                    use sha2::{Digest, Sha256};
                    let sum = self
                        .sum
                        .clone()
                        .unwrap_or_else(|| format!("{:x}", Sha256::digest(&self.herdr)));
                    IpcResponse::Artifact {
                        path: "/head/artifacts/herdr".into(),
                        sha256: sum,
                        size: self.size.unwrap_or(self.herdr.len() as u64),
                    }
                }
                IpcRequest::BootstrapReport { .. } if self.refuse_report => {
                    anyhow::bail!("ssh: connect to host head port 22: Connection timed out")
                }
                IpcRequest::BootstrapReport { machine, .. } => {
                    IpcResponse::Text(format!("{machine}: bootstrap recorded"))
                }
                other => panic!("unexpected request {other:?}"),
            };
            Ok(serde_json::to_string(&reply).unwrap())
        }

        fn download(
            &self,
            _head: &str,
            key: &Path,
            _known_hosts: &Path,
            remote: &[&str],
            line: &str,
            dest: &Path,
            max: u64,
        ) -> anyhow::Result<()> {
            self.record(key, remote, line, Some(max));
            std::fs::write(dest, &self.herdr)?;
            Ok(())
        }
    }

    fn code(err: &anyhow::Error) -> String {
        err.downcast_ref::<crate::cli::CliError>()
            .map(|e| e.code.clone())
            .unwrap_or_else(|| format!("not a CliError: {err:#}"))
    }

    /// The token's key is on disk only while the join's ssh runs, 0600 in a
    /// 0700 dir, and gone after, whether the join failed or not.
    #[test]
    fn temp_join_key_deleted_on_error() {
        let (_tmp, p) = paths();
        let r = FakeRunner::new(&[]);
        let link = FakeLink::new(Err(
            "ssh: connect to host head port 22: Connection refused".into()
        ));
        let err = join_with(&p, &token(), &opts(), &r, &link).unwrap_err();
        assert!(format!("{err:#}").contains("Connection refused"), "{err:#}");
        let seen = link.seen();
        assert_eq!(seen.len(), 1, "{seen:?}");
        let join = &seen[0];
        assert_eq!(join.remote, ["bridge"]);
        assert_eq!(join.key_text, token().key);
        assert_eq!(join.key_mode, 0o600);
        assert_eq!(join.dir_mode, 0o700);
        assert!(!join.key.exists(), "the key is gone");
        assert!(!join.key.parent().unwrap().exists(), "and its dir");

        // A join the head refuses, and one it takes, delete it too.
        let refused = IpcResponse::error("join_token_used", "join token 00 was used");
        let link = FakeLink::new(Ok(serde_json::to_string(&refused).unwrap()));
        let err = join_with(&p, &token(), &opts(), &r, &link).unwrap_err();
        assert_eq!(code(&err), "join_token_used");
        assert!(!link.seen()[0].key.exists());
        let link = FakeLink::joins("gpu-3", None);
        join_with(&p, &token(), &opts(), &r, &link).unwrap();
        assert!(!link.seen()[0].key.exists());
        // The secret never reaches an error or the outcome.
        assert!(!format!("{err:#}").contains("SECRET"));
    }

    /// An ssh refusal of the token's key is the token being spent, revoked
    /// or expired: sshd no longer has its line.
    #[test]
    fn a_refused_token_key_reads_as_an_invalid_token() {
        let (_tmp, p) = paths();
        let r = FakeRunner::new(&[]);
        let link = FakeLink::new(Err("me@head: Permission denied (publickey).".into()));
        let err = join_with(&p, &token(), &opts(), &r, &link).unwrap_err();
        assert_eq!(code(&err), "join_token_invalid", "{err:#}");
    }

    #[test]
    fn token_from_stdin() {
        let t = token();
        let text = format!("  {}\n", t.encode());
        let got = read_token(None, true, None, &mut text.as_bytes()).unwrap();
        assert_eq!(got, t);
        // The argument, then stdin, then the variable.
        let got = read_token(Some(&t.encode()), false, None, &mut "".as_bytes()).unwrap();
        assert_eq!(got, t);
        let got = read_token(None, false, Some(t.encode()), &mut "".as_bytes()).unwrap();
        assert_eq!(got, t);
        let got = read_token(None, true, Some("pjt_x".into()), &mut text.as_bytes()).unwrap();
        assert_eq!(got, t, "stdin, asked for, wins over the variable");
        let err = read_token(None, true, None, &mut "not a token\n".as_bytes()).unwrap_err();
        assert_eq!(code(&err), "join_token_invalid");
        let err = read_token(None, true, None, &mut "".as_bytes()).unwrap_err();
        assert_eq!(code(&err), "join_token_invalid");
        let err = read_token(None, false, Some(String::new()), &mut "".as_bytes()).unwrap_err();
        assert_eq!(code(&err), "invalid_args");
    }

    #[test]
    fn shepherd_name_written() {
        let (_tmp, p) = paths();
        let file = p.config_file();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            "tick = \"5s\"\n\n[shepherd]\ntakes_flock_work = true\n",
        )
        .unwrap();
        assert!(write_shepherd(&file, "gpu-3").unwrap());
        assert!(!write_shepherd(&file, "gpu-3").unwrap(), "unchanged");
        let c = crate::config::PastorConfig::load(&file).unwrap();
        assert_eq!(c.shepherd.machine.as_deref(), Some("gpu-3"));
        assert!(c.shepherd.takes_flock_work, "the rest is kept");
        assert_eq!(c.tick_duration(), std::time::Duration::from_secs(5));
        // No file yet: one is made.
        std::fs::remove_file(&file).unwrap();
        assert!(write_shepherd(&file, "pi-7").unwrap());
        let c = crate::config::PastorConfig::load(&file).unwrap();
        assert_eq!(c.shepherd.machine.as_deref(), Some("pi-7"));

        // The join writes the name the head gave back, as the service user.
        let r = FakeRunner::new(&[]);
        let link = FakeLink::joins("gpu-3", None);
        join_with(&p, &token(), &opts(), &r, &link).unwrap();
        assert!(
            r.calls()
                .contains(&"/tmp/cache/pastor bootstrap --write-shepherd gpu-3".to_string()),
            "{:#?}",
            r.calls()
        );
    }

    /// The head's host key is pinned before the first ssh, in this user's
    /// known_hosts and each pastor user's.
    #[test]
    fn the_head_host_key_is_pinned_first() {
        let (_tmp, p) = paths();
        let r = FakeRunner::new(&[]);
        let link = FakeLink::new(Err("down".into()));
        let _ = join_with(&p, &token(), &opts(), &r, &link);
        let text = std::fs::read_to_string(p.known_hosts_file()).unwrap();
        assert_eq!(text, "head ssh-ed25519 AAAAHEAD\n");
        assert!(
            r.calls()
                .iter()
                .any(|c| c.contains("/home/me/.local/state/pastor/known_hosts")
                    && c.contains("head ssh-ed25519 AAAAHEAD")),
            "{:#?}",
            r.calls()
        );
        // Twice is still one line.
        let _ = join_with(&p, &token(), &opts(), &r, &link);
        let text = std::fs::read_to_string(p.known_hosts_file()).unwrap();
        assert_eq!(text, "head ssh-ed25519 AAAAHEAD\n");
    }

    /// The join sends the keys the identity steps made, then bootstrap
    /// installs the herdr fetched with the machine key, and the outcome
    /// waits in bootstrap.json for the shepherd.
    #[test]
    fn join_fetches_herdr_with_the_machine_key_then_bootstraps() {
        let (_tmp, p) = paths();
        let r = FakeRunner::new(&[]);
        let link = FakeLink::joins("gpu-3", Some("0.9.6"));
        let out = join_with(&p, &token(), &opts(), &r, &link).unwrap();
        assert!(out.ok, "{out:#?}");
        let seen = link.seen();
        let (req, _) = crate::ipc::parse_request_line(seen[0].line.trim()).unwrap();
        match req {
            IpcRequest::Join {
                name,
                machine_key,
                host_key,
                service_user,
                ..
            } => {
                assert_eq!(service_user.as_deref(), Some("me"));
                assert_eq!(name, "gpu-3");
                assert_eq!(machine_key, "ssh-ed25519 KEY");
                assert_eq!(host_key, "ssh-ed25519 HOST root@box");
            }
            other => panic!("{other:?}"),
        }
        let machine_key = PathBuf::from("/home/me/.local/state/pastor/machine_key");
        // The fetch is a request line to the forced bridge, never argv,
        // which sshd would not pass on.
        let fetch = seen.iter().find(|s| s.max.is_some()).expect("a fetch");
        assert_eq!(fetch.remote, ["bridge"]);
        assert_eq!(
            crate::bridge::fetch_request(fetch.line.as_bytes()),
            Some(Ok(crate::artifacts::Artifact::new(
                "herdr",
                "0.9.6",
                "linux-x86_64"
            )
            .unwrap()))
        );
        assert_eq!(fetch.key, machine_key);
        let calls = r.calls();
        assert!(
            calls.iter().any(|c| c.starts_with("install -m 0755 ")
                && c.ends_with("/herdr /home/me/.local/bin/herdr")),
            "{calls:#?}"
        );
        assert!(
            calls.contains(
                &"/home/me/.local/bin/pastor setup systemd --yes --require-linger".to_string()
            ),
            "the shepherd's unit: {calls:#?}"
        );
        assert_eq!(step(&out, "shepherd").status, StepStatus::Done);
        let at = |part: &str| calls.iter().position(|c| c.starts_with(part));
        let report = at("/tmp/cache/pastor bootstrap --write-report ")
            .expect("the report is written for the shepherd");
        let json = calls[report].trim_start_matches("/tmp/cache/pastor bootstrap --write-report ");
        let written: Outcome = serde_json::from_str(json).unwrap();
        assert_eq!(written, out);
        // Sent to the head over the machine key too, then the file goes,
        // since the head has it.
        let sent = seen.iter().find(|s| {
            matches!(
                crate::ipc::parse_request_line(s.line.trim()),
                Ok((IpcRequest::BootstrapReport { .. }, _))
            )
        });
        assert_eq!(sent.expect("the report is sent").key, machine_key);
        let dropped = at("rm -f -- /home/me/.local/state/pastor/bootstrap.json")
            .expect("the sent report is removed");
        assert!(report < dropped, "{calls:#?}");
        // The fetched herdr and the token's key are gone with their dirs.
        let left: Vec<_> = std::fs::read_dir(&p.state_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("join"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    /// A herdr whose sum is not the head's is never installed, and the
    /// failure goes to the head at once: no shepherd runs to send it.
    #[test]
    fn a_herdr_with_the_wrong_sum_fails_the_bootstrap_and_is_reported() {
        let (_tmp, p) = paths();
        let r = FakeRunner::new(&[]);
        let mut link = FakeLink::joins("gpu-3", None);
        link.sum = Some("00".repeat(32));
        let out = join_with(&p, &token(), &opts(), &r, &link).unwrap();
        assert!(!out.ok);
        let failed = step(&out, "herdr");
        assert_eq!(failed.status, StepStatus::Failed);
        assert!(
            failed.why.as_deref().unwrap_or("").contains("sha256"),
            "{failed:?}"
        );
        assert!(
            !r.calls()
                .iter()
                .any(|c| c.ends_with("/home/me/.local/bin/herdr") && c.starts_with("install")),
            "{:#?}",
            r.calls()
        );
        let sent = link.seen().into_iter().any(|s| {
            matches!(
                crate::ipc::parse_request_line(s.line.trim()),
                Ok((IpcRequest::BootstrapReport { machine, result }, _))
                    if machine == "gpu-3" && !result.ok
            )
        });
        assert!(sent, "{:#?}", link.seen());
        // Left first, then removed once the head had it.
        let calls = r.calls();
        assert!(
            calls.iter().any(|c| c.contains("--write-report")),
            "{calls:#?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| c.ends_with("rm -f -- /home/me/.local/state/pastor/bootstrap.json")),
            "{calls:#?}"
        );
    }

    /// A report the head does not take stays in bootstrap.json for the
    /// shepherd, and the join still succeeds.
    #[test]
    fn a_report_the_head_does_not_take_is_kept_for_the_shepherd() {
        let (_tmp, p) = paths();
        let r = FakeRunner::new(&[]);
        let mut link = FakeLink::joins("gpu-3", None);
        link.refuse_report = true;
        let out = join_with(&p, &token(), &opts(), &r, &link).unwrap();
        assert!(out.ok, "{out:#?}");
        let calls = r.calls();
        assert!(
            calls.iter().any(|c| c.contains("--write-report")),
            "{calls:#?}"
        );
        assert!(
            !calls
                .iter()
                .any(|c| c.contains("rm -f -- /home/me/.local/state/pastor/bootstrap.json")),
            "{calls:#?}"
        );
    }

    /// `--service-user` from a login that is neither that user nor root is
    /// refused before the token is spent: the fetch and the report would go
    /// over that user's machine key, which such a login cannot read.
    #[test]
    fn a_service_user_from_a_non_root_login_is_refused_before_the_join() {
        let (_tmp, p) = paths();
        let r = FakeRunner::new(&[]);
        let link = FakeLink::joins("gpu-3", None);
        let o = JoinOpts {
            sudo: true,
            service_user: Some("pastor".into()),
            ..opts()
        };
        let err = join_with(&p, &token(), &o, &r, &link).unwrap_err();
        assert_eq!(code(&err), "usage_error", "{err:#}");
        assert!(format!("{err:#}").contains("as root"), "{err:#}");
        assert!(link.seen().is_empty(), "{:#?}", link.seen());
        assert!(
            !r.calls().iter().any(|c| c.contains("useradd")),
            "{:#?}",
            r.calls()
        );
    }

    /// A root join with no `--service-user` gets the `pastor` service
    /// user, and the join names it; from root, any other non-root service
    /// user passes the login check too, and `root` itself is refused.
    #[test]
    fn a_root_join_defaults_its_service_user_to_pastor() {
        let joined_as = |service_user: Option<&str>| {
            let (_tmp, p) = paths();
            let r = FakeRunner::new(&as_root());
            let link = FakeLink::joins("gpu-3", None);
            let o = JoinOpts {
                sudo: true,
                service_user: service_user.map(str::to_string),
                ..opts()
            };
            let out = join_with(&p, &token(), &o, &r, &link).unwrap();
            assert!(out.ok, "{out:#?}");
            let seen = link.seen();
            match crate::ipc::parse_request_line(seen[0].line.trim())
                .unwrap()
                .0
            {
                IpcRequest::Join { service_user, .. } => service_user,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(
            joined_as(None).as_deref(),
            Some(crate::bootstrap::ROOT_SERVICE_USER)
        );
        assert_eq!(joined_as(Some("pastor")).as_deref(), Some("pastor"));

        let (_tmp, p) = paths();
        let r = FakeRunner::new(&as_root());
        let link = FakeLink::joins("gpu-3", None);
        let o = JoinOpts {
            sudo: true,
            service_user: Some("root".into()),
            ..opts()
        };
        let err = join_with(&p, &token(), &o, &r, &link).unwrap_err();
        assert_eq!(code(&err), "usage_error", "{err:#}");
        assert!(link.seen().is_empty());
    }

    /// A name the head would refuse is refused here first, before any
    /// user or key is made.
    #[test]
    fn a_bad_name_is_refused_before_anything_runs() {
        let (_tmp, p) = paths();
        let r = FakeRunner::new(&[]);
        let link = FakeLink::joins("gpu-3", None);
        let o = JoinOpts {
            name: Some("a;id".into()),
            ..opts()
        };
        let err = join_with(&p, &token(), &o, &r, &link).unwrap_err();
        assert_eq!(code(&err), "machine_name_invalid", "{err:#}");
        assert!(link.seen().is_empty());
        assert!(r.calls().is_empty(), "{:#?}", r.calls());
    }

    /// `--skip-bootstrap` joins and names the shepherd, and leaves the
    /// identity steps' outcome for it to report.
    #[test]
    fn skip_bootstrap_joins_and_reports_the_identities() {
        let (_tmp, p) = paths();
        let r = FakeRunner::new(&[]);
        let link = FakeLink::joins("gpu-3", None);
        let o = JoinOpts {
            skip_bootstrap: true,
            ..opts()
        };
        let out = join_with(&p, &token(), &o, &r, &link).unwrap();
        assert!(out.ok, "{out:#?}");
        assert!(link.seen().iter().all(|s| s.remote == ["bridge"]));
        let calls = r.calls();
        assert!(
            !calls.iter().any(|c| c.contains("setup systemd")),
            "{calls:#?}"
        );
        assert!(calls.iter().any(|c| c.contains("--write-report")));
    }

    /// In root mode a service user's runtime dir may lag `enable-linger`:
    /// the shepherd step waits for it, then starts the user's manager, and
    /// says so when it never comes.
    #[test]
    fn the_shepherd_waits_for_its_runtime_dir() {
        let (_tmp, p) = paths();
        let mut rules = vec![("test -d /run/user/991", false, "")];
        rules.extend(as_root());
        let r = FakeRunner::new(&rules);
        let link = FakeLink::joins("gpu-3", None);
        let o = JoinOpts {
            sudo: true,
            service_user: Some("pastor".into()),
            ..opts()
        };
        let out = join_with(&p, &token(), &o, &r, &link).unwrap();
        let failed = step(&out, "shepherd");
        assert_eq!(failed.status, StepStatus::Failed, "{out:#?}");
        assert!(
            failed
                .why
                .as_deref()
                .unwrap_or("")
                .contains("/run/user/991"),
            "{failed:?}"
        );
        assert!(
            r.calls()
                .contains(&"systemctl start user@991.service".to_string()),
            "{:#?}",
            r.calls()
        );
    }

    /// `pastor join` run as root, which `--service-user` needs.
    fn as_root() -> Vec<(&'static str, bool, &'static str)> {
        vec![
            ("id -un", true, "root"),
            ("id -u pastor-agent", false, ""),
            ("id -u pastor", false, ""),
            ("id -u", true, "0"),
            (
                "getent passwd root",
                true,
                "root:x:0:0:root:/root:/bin/bash",
            ),
        ]
    }

    /// In root mode the shepherd (the service user) reaches herdr (the
    /// agent user) through one sudoers rule and `[shepherd] command`, both
    /// in place after client.toml and before either service starts.
    #[test]
    fn a_root_join_gives_the_shepherd_a_way_to_the_agents_herdr() {
        let (_tmp, p) = paths();
        let r = FakeRunner::new(&as_root());
        let link = FakeLink::joins("gpu-3", None);
        let o = JoinOpts {
            sudo: true,
            service_user: Some("pastor".into()),
            ..opts()
        };
        let out = join_with(&p, &token(), &o, &r, &link).unwrap();
        assert!(out.ok, "{out:#?}");
        assert_eq!(step(&out, "shepherd_herdr").status, StepStatus::Done);
        let calls = r.calls();
        let herdr = "/home/pastor-agent/.local/bin/herdr";
        let rule = format!(
            "pastor ALL=(pastor-agent) NOPASSWD: {herdr} --session default remote-api-bridge\n"
        );
        let at = |what: &dyn Fn(&str) -> bool| {
            calls
                .iter()
                .position(|c| what(c))
                .unwrap_or_else(|| panic!("{calls:#?}"))
        };
        let sudoers = at(&|c| {
            c.starts_with("sh -c set -e; umask 0337;")
                && c.ends_with(&format!("sh {rule} /etc/sudoers.d/pastor-pastor"))
        });
        let checked = at(&|c| {
            c == format!("sudo -n -l -U pastor {herdr} --session default remote-api-bridge")
        });
        let command = at(&|c| {
            runs_as(c) == "pastor"
                && c.ends_with(&format!("bootstrap --write-shepherd-herdr {herdr}"))
        });
        let client = at(&|c| c.ends_with("bootstrap --write-client me@head"));
        let herdr_unit = at(&|c| c.ends_with("setup systemd --herdr --yes --require-linger"));
        let shepherd_unit = at(&|c| c.ends_with("setup systemd --yes --require-linger"));
        assert!(
            client < sudoers && sudoers < checked && checked < command,
            "{calls:#?}"
        );
        assert!(
            command < herdr_unit && herdr_unit < shepherd_unit,
            "{calls:#?}"
        );
    }

    /// User mode needs no rule: the shepherd and herdr are the login user.
    #[test]
    fn a_user_mode_join_writes_no_sudoers_rule() {
        let (_tmp, p) = paths();
        let r = FakeRunner::new(&[]);
        let link = FakeLink::joins("gpu-3", None);
        let out = join_with(&p, &token(), &opts(), &r, &link).unwrap();
        assert_eq!(step(&out, "shepherd_herdr").status, StepStatus::Skipped);
        assert!(!r.calls().iter().any(|c| c.contains("sudoers")));
    }

    /// `[shepherd] command` is the sudo line, written once, beside what the
    /// file already holds; a path that is not plain is refused.
    #[test]
    fn write_shepherd_herdr_sets_the_command_once() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("pastor.toml");
        std::fs::write(&file, "[shepherd]\nmachine = \"gpu-3\"\n").unwrap();
        let herdr = "/home/pastor-agent/.local/bin/herdr";
        assert!(write_shepherd_herdr(&file, herdr).unwrap());
        assert!(!write_shepherd_herdr(&file, herdr).unwrap());
        let cfg = crate::config::PastorConfig::load(&file).unwrap();
        assert_eq!(cfg.shepherd.machine.as_deref(), Some("gpu-3"));
        assert_eq!(cfg.shepherd.command, Some(shepherd_herdr_argv(herdr)));
        assert_eq!(
            shepherd_herdr_argv(herdr),
            [
                "sudo",
                "-n",
                "-H",
                "-u",
                "pastor-agent",
                herdr,
                "--session",
                "default",
                "remote-api-bridge"
            ]
        );
        for bad in ["herdr", "/a b/herdr", "/a;b", "-x"] {
            assert!(write_shepherd_herdr(&file, bad).is_err(), "{bad}");
        }
    }

    /// The token's host key line is pinned only when it is one plain line
    /// for the token's own head: `host type key`, or `[host]:port`, with a
    /// key type sshd logs in with. A marker, a pattern, a negation or a
    /// list would pin more than the head, or something else.
    #[test]
    fn the_pinned_line_is_the_heads_alone() {
        let good = [
            "head ssh-ed25519 AAAAHEAD",
            "[head]:2222 ssh-ed25519 AAAAHEAD",
            "head ecdsa-sha2-nistp256 AAAAHEAD",
        ];
        for line in good {
            let (_tmp, p) = paths();
            let r = FakeRunner::new(&[]);
            let link = FakeLink::new(Err("down".into()));
            let t = Token {
                head_host_key: line.into(),
                ..token()
            };
            let _ = join_with(&p, &t, &opts(), &r, &link);
            let text = std::fs::read_to_string(p.known_hosts_file()).unwrap();
            assert_eq!(text, format!("{line}\n"));
        }
        // A head on port 2222, named in the URI form ssh takes.
        {
            let (_tmp, p) = paths();
            let r = FakeRunner::new(&[]);
            let link = FakeLink::new(Err("down".into()));
            let line = "[head]:2222 ssh-ed25519 AAAAHEAD";
            let t = Token {
                head: "ssh://me@head:2222".into(),
                head_host_key: line.into(),
                ..token()
            };
            let _ = join_with(&p, &t, &opts(), &r, &link);
            let text = std::fs::read_to_string(p.known_hosts_file()).unwrap();
            assert_eq!(text, format!("{line}\n"));
        }
        let bad = [
            "@cert-authority head ssh-ed25519 AAAAHEAD",
            "@revoked head ssh-ed25519 AAAAHEAD",
            "* ssh-ed25519 AAAAHEAD",
            "hea? ssh-ed25519 AAAAHEAD",
            "*.example ssh-ed25519 AAAAHEAD",
            "!other ssh-ed25519 AAAAHEAD",
            "head,other ssh-ed25519 AAAAHEAD",
            "other ssh-ed25519 AAAAHEAD",
            "[other]:2222 ssh-ed25519 AAAAHEAD",
            "[head]:x ssh-ed25519 AAAAHEAD",
            "head ssh-dss AAAAHEAD",
            "head ssh-ed25519 AAAAHEAD comment",
            "head ssh-ed25519",
            "head ssh-ed25519 AAAAHEAD\nother ssh-ed25519 AAAAOTHER",
        ];
        for line in bad {
            let (_tmp, p) = paths();
            let r = FakeRunner::new(&[]);
            let link = FakeLink::joins("gpu-3", None);
            let t = Token {
                head_host_key: line.into(),
                ..token()
            };
            let err = join_with(&p, &t, &opts(), &r, &link).unwrap_err();
            assert_eq!(code(&err), "join_token_invalid", "{line:?}: {err:#}");
            assert!(!p.known_hosts_file().exists(), "{line:?}");
            assert!(link.seen().is_empty(), "{line:?}");
        }
    }

    /// A join stopped by a signal takes its ssh down with it: the child is
    /// killed and reaped, not left running orphaned. One done with drops
    /// out of the list.
    #[test]
    fn a_signal_kills_and_reaps_the_joins_ssh() {
        // A list of its own: the joins other tests run are not this one's
        // to kill.
        let children = Children(std::sync::Mutex::new(Vec::new()));
        let mut done = std::process::Command::new("true").spawn().unwrap();
        {
            let _live = children.track(&done);
            done.wait().unwrap();
        }
        assert!(children.0.lock().unwrap().is_empty());
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        let live = children.track(&child);
        children.kill_all();
        // Reaped: no process, not even a zombie, has the pid now.
        // SAFETY: signal 0 sends nothing; it only checks that pid exists.
        let gone = unsafe { libc::kill(pid as libc::pid_t, 0) } != 0;
        assert!(gone, "ssh {pid} still there");
        assert!(child.wait().is_err(), "reaped already, nothing to wait for");
        drop(live);
        assert!(children.0.lock().unwrap().is_empty());
    }

    /// A pid no process has: one that ran and was reaped.
    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    /// The key dir of a join that was killed (no Drop ran) is removed by
    /// the next join; only pastor's own dirs go: named as it names them,
    /// real dirs of this user's, of a process that is gone.
    #[test]
    fn a_join_removes_the_dirs_an_interrupted_one_left() {
        let (_tmp, p) = paths();
        std::fs::create_dir_all(&p.state_dir).unwrap();
        let dead = dead_pid();
        let stale = [
            format!("join-{dead}-12345"),
            format!("join-herdr-{dead}-678"),
        ];
        for d in &stale {
            std::fs::create_dir(p.state_dir.join(d)).unwrap();
            std::fs::write(p.state_dir.join(d).join("join_key"), "SECRET").unwrap();
        }
        let live = format!("join-{}-1", std::process::id());
        let kept = ["join-notes", "joined", live.as_str()];
        for d in kept {
            std::fs::create_dir(p.state_dir.join(d)).unwrap();
        }
        // Not a dir: a file, and a link to a dir elsewhere.
        let file = format!("join-{dead}-2");
        std::fs::write(p.state_dir.join(&file), "x").unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let link_name = format!("join-{dead}-3");
        std::os::unix::fs::symlink(elsewhere.path(), p.state_dir.join(&link_name)).unwrap();

        let r = FakeRunner::new(&[]);
        let link = FakeLink::new(Err("down".into()));
        let _ = join_with(&p, &token(), &opts(), &r, &link);
        for d in &stale {
            assert!(!p.state_dir.join(d).exists(), "{d} removed");
        }
        for d in kept.iter().chain([&file.as_str(), &link_name.as_str()]) {
            assert!(p.state_dir.join(d).symlink_metadata().is_ok(), "{d} kept");
        }
        assert!(elsewhere.path().exists());
    }

    /// The download is capped at the size the head answered, and a size
    /// over the absolute cap is refused before anything is fetched.
    #[test]
    fn the_herdr_download_is_capped() {
        let (_tmp, p) = paths();
        let r = FakeRunner::new(&[]);
        let link = FakeLink::joins("gpu-3", None);
        join_with(&p, &token(), &opts(), &r, &link).unwrap();
        let fetch = link
            .seen()
            .into_iter()
            .find(|s| s.max.is_some())
            .expect("a fetch");
        assert_eq!(fetch.max, Some(5), "capped at the head's size");

        let (_tmp, p) = paths();
        let mut link = FakeLink::joins("gpu-3", None);
        link.size = Some(MAX_HERDR + 1);
        let out = join_with(&p, &token(), &opts(), &r, &link).unwrap();
        let failed = step(&out, "herdr");
        assert_eq!(failed.status, StepStatus::Failed);
        assert!(
            failed.why.as_deref().unwrap_or("").contains("MiB"),
            "{failed:?}"
        );
        assert!(
            !link.seen().iter().any(|s| s.max.is_some()),
            "nothing fetched: {:#?}",
            link.seen()
        );
    }

    /// An `ssh` for SshLink: a script in `dir` with `body`.
    fn fake_ssh(dir: &Path, body: &str) -> SshLink {
        use std::os::unix::fs::PermissionsExt;
        let ssh = dir.join("ssh");
        std::fs::write(&ssh, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        SshLink { ssh }
    }

    /// A reply longer than 64 KiB is refused, read no further than that,
    /// as the head refuses a request line; a shorter one is read.
    #[test]
    fn a_reply_line_is_capped() {
        let tmp = tempfile::tempdir().unwrap();
        let k = tmp.path().join("k");
        let link = fake_ssh(tmp.path(), "cat >/dev/null; printf '\\n{\"ok\":1}\\n'");
        let got = link.request("h", &k, &k, &["bridge"], "{}").unwrap();
        assert_eq!(got, "{\"ok\":1}");
        // 1 MiB, no newline, from a head that never stops.
        let link = fake_ssh(
            tmp.path(),
            "cat >/dev/null; head -c 1048576 /dev/zero | tr '\\0' x; sleep 30",
        );
        let started = std::time::Instant::now();
        let err = link.request("h", &k, &k, &["bridge"], "{}").unwrap_err();
        assert!(format!("{err:#}").contains("64 KiB"), "{err:#}");
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
    }

    /// A download that passes its cap stops there, and fails: what is on
    /// disk is never more than the cap.
    #[test]
    fn a_download_stops_at_its_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let k = tmp.path().join("k");
        let dest = tmp.path().join("herdr");
        let link = fake_ssh(tmp.path(), "printf 12345");
        link.download("h", &k, &k, &["bridge"], "{}", &dest, 5)
            .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"12345");
        let link = fake_ssh(tmp.path(), "head -c 10485760 /dev/zero; sleep 30");
        let started = std::time::Instant::now();
        let err = link
            .download("h", &k, &k, &["bridge"], "{}", &dest, 1024)
            .unwrap_err();
        assert_eq!(code(&err), "artifact_checksum_mismatch", "{err:#}");
        assert!(std::fs::metadata(&dest).map_or(0, |m| m.len()) <= 1024);
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
    }
}
