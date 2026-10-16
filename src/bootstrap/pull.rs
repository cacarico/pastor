//! `pastor join` on a new machine: join the fleet with a token as a pull
//! machine, then bootstrap it with herdr from the head's cache.

use std::path::{Path, PathBuf};

use super::steps::{self, Ctx, Step, StepOutcome, must, path, succeeded};
use super::{Options, Outcome, StepResult};
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
    /// `remote` on `head`, its stdout written to `dest`.
    fn download(
        &self,
        head: &str,
        key: &Path,
        known_hosts: &Path,
        remote: &[&str],
        dest: &Path,
    ) -> anyhow::Result<()>;
}

/// Where a pull machine's outcome waits for its shepherd to report it.
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

/// Pin `line` (`host type key`) in the known_hosts `file`, once. Returns
/// whether it wrote.
fn pin(file: &Path, line: &str) -> anyhow::Result<bool> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let line = line.trim();
    if line.contains(['\n', '\r']) || line.split_whitespace().count() < 3 {
        return Err(CliError::err(
            crate::join::INVALID,
            "the token's head host key is not one known_hosts line",
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
    }
}

/// `ssh` to the head with one key, the pinned known_hosts and nothing else:
/// no master to share (each join's few connections are not worth one),
/// no other key, and the remote command as plain words, which the head's
/// forced command reads.
pub struct SshLink;

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

/// What ssh wrote on stderr, or that it wrote nothing.
fn stderr_of(out: &std::process::Output) -> String {
    let text = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if text.is_empty() {
        format!("ssh exited with {}", out.status)
    } else {
        text
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
        let mut child = Command::new("ssh")
            .args(ssh_argv(head, key, known_hosts, remote))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("run ssh: {e}"))?;
        if let Some(mut stdin) = child.stdin.take() {
            // A bridge that already gave up closes the pipe; its reply, or
            // ssh's stderr, says why.
            let _ = stdin.write_all(line.trim_end().as_bytes());
            let _ = stdin.write_all(b"\n");
        }
        let out = child.wait_with_output()?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        match stdout.lines().find(|l| !l.trim().is_empty()) {
            Some(reply) => Ok(reply.to_string()),
            None => anyhow::bail!("{}", stderr_of(&out)),
        }
    }

    fn download(
        &self,
        head: &str,
        key: &Path,
        known_hosts: &Path,
        remote: &[&str],
        dest: &Path,
    ) -> anyhow::Result<()> {
        use std::process::{Command, Stdio};
        let file = std::fs::File::create(dest)?;
        let out = Command::new("ssh")
            .args(ssh_argv(head, key, known_hosts, remote))
            .stdin(Stdio::null())
            .stdout(file)
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| anyhow::anyhow!("run ssh: {e}"))?;
        if out.status.success() {
            return Ok(());
        }
        // The bridge writes its own failure on stderr as `{code, message}`.
        let err = String::from_utf8_lossy(&out.stderr);
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
        anyhow::bail!("{}", stderr_of(&out))
    }
}

/// The variable `pastor join` reads the token from when neither the
/// argument nor `--token-stdin` gives one.
pub const TOKEN_ENV: &str = "PASTOR_JOIN_TOKEN";

/// Entry point for `pastor join` with a token: join, bootstrap, print the
/// outcome.
pub fn cli(args: &crate::join::JoinArgs) -> anyhow::Result<()> {
    let token = read_token(
        args.token.as_deref(),
        args.token_stdin,
        std::env::var(TOKEN_ENV).ok(),
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
/// run off the async threads.
pub async fn join(paths: &Paths, token: Token, o: JoinOpts) -> anyhow::Result<Outcome> {
    let paths = paths.clone();
    tokio::task::spawn_blocking(move || {
        join_with(&paths, &token, &o, &crate::setup::SystemRunner, &SshLink)
    })
    .await?
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
/// then the shepherd's service last, once the rest is there.
fn all_steps() -> Vec<(&'static str, Step)> {
    let mut list = steps::STEPS.to_vec();
    let at = list
        .iter()
        .position(|(n, _)| *n == "keys")
        .map_or(list.len(), |i| i + 1);
    list.insert(at, ("known_hosts", known_hosts));
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

    // 1. The head's host key, before the first ssh to it.
    let known_hosts = paths.known_hosts_file();
    pin(&known_hosts, &pull.head_host_key)?;

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

    // 6. The report: left for the shepherd when it will run, sent now when
    // it will not; each the other's fallback.
    let json = serde_json::to_string(&outcome)?;
    let leave = || {
        super::with_ctx(r, &opts, Some(&pull), |c| leave_report(r, c, &json))
            .map_err(|s| anyhow::anyhow!("{}", s.why.unwrap_or_default()))
    };
    let send = || -> anyhow::Result<()> {
        let req = IpcRequest::BootstrapReport {
            machine: name.clone(),
            result: outcome.clone(),
        };
        let line = crate::ipc::request_line(&req, None)?;
        let answer = link.request(&token.head, &machine_key, &known_hosts, &["bridge"], &line)?;
        reply(&answer).map(|_| ())
    };
    let first = if outcome.ok { leave() } else { send() };
    if let Err(err) = first {
        let fallback = if outcome.ok { send() } else { leave() };
        if let Err(again) = fallback {
            return Err(CliError::err(
                "runtime_error",
                format!(
                    "machine {name} joined, but its bootstrap result reached neither its shepherd ({err:#}) nor the head ({again:#}); the head shows it pending: join again with a token from `pastor join create --replace {name}`"
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
    // The words go on ssh's command line: plain names only.
    crate::artifacts::Artifact::new("herdr", version, &target)
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
    let dest = dir.join("herdr");
    link.download(
        head,
        machine_key,
        known_hosts,
        &["bridge", "--fetch", "herdr", version, &target],
        &dest,
    )?;
    let bytes = std::fs::read(&dest)?;
    let got = format!("{:x}", Sha256::digest(&bytes));
    if bytes.len() as u64 != size || !got.eq_ignore_ascii_case(sha256.trim()) {
        return Err(CliError::err(
            "artifact_checksum_mismatch",
            format!(
                "herdr {version} {target} from the head is {} bytes with sha256 {got}, not the {size} bytes with sha256 {sha256} the head answered",
                bytes.len()
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
    use crate::bootstrap::tests::{FakeRunner, step};

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
        seen: Mutex<Vec<Seen>>,
    }

    impl FakeLink {
        fn new(joined: Result<String, String>) -> FakeLink {
            FakeLink {
                joined,
                herdr: b"HERDR".to_vec(),
                sum: None,
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

        fn record(&self, key: &Path, remote: &[&str], line: &str) {
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
            self.record(key, remote, line);
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
                        size: self.herdr.len() as u64,
                    }
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
            dest: &Path,
        ) -> anyhow::Result<()> {
            self.record(key, remote, "");
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
                ..
            } => {
                assert_eq!(name, "gpu-3");
                assert_eq!(machine_key, "ssh-ed25519 KEY");
                assert_eq!(host_key, "ssh-ed25519 HOST root@box");
            }
            other => panic!("{other:?}"),
        }
        let machine_key = PathBuf::from("/home/me/.local/state/pastor/machine_key");
        let fetch = seen
            .iter()
            .find(|s| s.remote.get(1).is_some_and(|a| a == "--fetch"))
            .expect("a fetch");
        assert_eq!(
            fetch.remote,
            ["bridge", "--fetch", "herdr", "0.9.6", "linux-x86_64"]
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
        let report = calls
            .iter()
            .find(|c| c.starts_with("/tmp/cache/pastor bootstrap --write-report "))
            .expect("the report is written for the shepherd");
        let json = report.trim_start_matches("/tmp/cache/pastor bootstrap --write-report ");
        let written: Outcome = serde_json::from_str(json).unwrap();
        assert_eq!(written, out);
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
        assert!(
            !r.calls().iter().any(|c| c.contains("--write-report")),
            "sent, so not left for a shepherd"
        );
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
        let r = FakeRunner::new(&[("test -d /run/user/991", false, "")]);
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
                .contains(&"sudo -n systemctl start user@991.service".to_string()),
            "{:#?}",
            r.calls()
        );
    }
}
