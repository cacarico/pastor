use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;

use super::Connection;
use crate::config::Paths;
use crate::config::flock::MachineConfig;
use crate::ssh::{SHORTER_STATE_DIR, check_socket_path, fitting_control_path};

/// How long an ssh `ControlMaster` sticks around with no channels open. Every
/// request opens a connection, so the master is what makes them cheap: the same
/// value herdr's own remote transport uses (`src/remote/attach.rs`).
const CONTROL_PERSIST: &str = "600";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    Local {
        session: String,
    },
    Ssh {
        target: String,
        session: String,
        /// Socket for the shared ssh `ControlMaster`, under pastor's state dir.
        /// `None` when that path would not fit in a unix socket name: the
        /// connection then works without multiplexing rather than not at all.
        control_path: Option<PathBuf>,
    },
    Command {
        argv: Vec<String>,
    },
}

impl Endpoint {
    pub fn from_machine(m: &MachineConfig, paths: &Paths) -> Endpoint {
        if let Some(argv) = &m.command {
            Endpoint::Command { argv: argv.clone() }
        } else if let Some(target) = &m.ssh {
            let control_path = fitting_control_path(paths, &m.name);
            if control_path.is_none() {
                // Decided once, when the endpoint is built, so this is said once
                // per machine instead of once per request.
                tracing::warn!(
                    machine = %m.name,
                    path = %paths.ssh_control_path("").display(),
                    "ssh ControlPath is too long for a unix socket even without the machine name; connecting without multiplexing (every request pays a full ssh handshake). {SHORTER_STATE_DIR}"
                );
            }
            Endpoint::Ssh {
                target: target.clone(),
                session: m.session.clone(),
                control_path,
            }
        } else {
            Endpoint::Local {
                session: m.session.clone(),
            }
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Endpoint::Local { session } => format!("local herdr session {session}"),
            Endpoint::Ssh {
                target, session, ..
            } => format!("ssh {target} (session {session})"),
            Endpoint::Command { argv } => format!("command {}", argv.join(" ")),
        }
    }

    /// Where the machine lives, short enough for a table column: the ssh
    /// target as written, `local`, or the file name of a command machine's
    /// program (its full path and arguments are in `describe`).
    pub fn host(&self) -> String {
        match self {
            Endpoint::Local { .. } => "local".into(),
            Endpoint::Ssh { target, .. } => target.clone(),
            Endpoint::Command { argv } => argv
                .first()
                .map(|a| {
                    Path::new(a)
                        .file_name()
                        .map_or_else(|| a.clone(), |f| f.to_string_lossy().into_owned())
                })
                .unwrap_or_else(|| "-".into()),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ConnectError {
    pub message: String,
}

pub fn local_socket_path(session: &str) -> anyhow::Result<PathBuf> {
    // herdr keeps its socket under `~/.config/herdr` on macOS as on Linux,
    // not in `~/Library/Application Support`, which `dirs` would pick there.
    let base = crate::config::config_home().ok_or_else(|| anyhow::anyhow!("no config dir"))?;
    socket_path_under(&base, session)
}

/// herdr's socket for `session` under the config home `base`, refused when it
/// would not fit in `sun_path`.
fn socket_path_under(base: &Path, session: &str) -> anyhow::Result<PathBuf> {
    let base = base.join("herdr");
    let path = if session == "default" {
        base.join("herdr.sock")
    } else {
        base.join("sessions").join(session).join("herdr.sock")
    };
    check_socket_path(
        &path,
        "Set XDG_CONFIG_HOME to something shorter, or use a shorter session name.",
    )?;
    Ok(path)
}

/// The exact command herdr's own client runs on the remote host.
pub fn bridge_command(session: &str) -> String {
    format!("herdr --session {} remote-api-bridge", shell_quote(session))
}

/// Quote `s` as a single POSIX shell word, safe to splice into a command string
/// that a remote shell (e.g. one invoked via `ssh target <command>`) will parse.
/// Plain alphanumeric-plus-`-_.` strings pass through unquoted for readability;
/// anything else is wrapped in single quotes, with embedded single quotes
/// escaped the standard POSIX way (`'\''`).
pub fn shell_quote(s: &str) -> String {
    // `"".chars().all(..)` is vacuously true, so the safe-passthrough check alone
    // would return `""` (nothing) for an empty string, dropping it from the
    // command line and shifting every argv position after it.
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// ssh argv for a bridge, over the shared master when there is one.
fn ssh_argv(target: &str, session: &str, control_path: Option<&Path>) -> Vec<String> {
    ssh_argv_running(target, control_path, bridge_command(session))
}

/// `command` as a remote login shell should see it: `sh -c '<command>'`.
/// sshd hands the command to the user's login shell, which may be fish or
/// csh; wrapped, that shell parses only a word and one single-quoted string
/// (no backslash in it, which fish would read as an escape), and sh parses
/// the POSIX command inside. A session name with a backslash is refused by
/// `Flock::validate` for this reason; a repo path with one needs a POSIX
/// login shell.
pub fn posix_command(command: &str) -> String {
    format!("sh -c {}", shell_quote(command))
}

/// ssh argv that runs `remote` (a POSIX shell command, run through
/// `posix_command`) over the shared master when there is one.
fn ssh_argv_running(target: &str, control_path: Option<&Path>, remote: String) -> Vec<String> {
    let ssh = crate::ssh::Ssh {
        target,
        control_path,
        control_persist: CONTROL_PERSIST,
        keepalive: true,
        no_tty: true,
    };
    let mut argv = vec!["ssh".to_string()];
    argv.extend(ssh.args(&remote));
    argv
}

/// Every ssh that carries a ControlPath can be the one that starts the
/// master, so each of them makes its directory first (`ssh::ensure_control_dir`).
fn ensure_control_dir(control_path: Option<&Path>) -> Result<(), ConnectError> {
    crate::ssh::ensure_control_dir(control_path).map_err(|e| ConnectError {
        message: e.to_string(),
    })
}

/// The most a probe may write to stdout, and to stderr. Its answer is a
/// path, a version or `yes`/`no`, plus whatever the rc files print.
const PROBE_OUTPUT_LIMIT: usize = 64 * 1024;

/// How long a probe may take, ssh handshake included. ssh's own keepalive
/// gives up on a dead link after 45s; this also covers a machine whose shell
/// answers the connection and then never exits.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Run a probe to completion and collect its output, like `Command::output`
/// but refusing more than `PROBE_OUTPUT_LIMIT` bytes on either stream and
/// more than `PROBE_TIMEOUT` in all: the machine answering is not trusted to
/// stop, and one misbehaving machine must not exhaust the head's memory or
/// hold it forever. Over either limit the probe is killed.
async fn probe_output(argv: &[String]) -> Result<std::process::Output, ConnectError> {
    probe_output_within(argv, PROBE_TIMEOUT).await
}

async fn probe_output_within(
    argv: &[String],
    time: std::time::Duration,
) -> Result<std::process::Output, ConnectError> {
    use tokio::io::AsyncReadExt;
    let mut child = tokio::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| ConnectError {
            message: format!("spawn {}: {e}", argv[0]),
        })?;
    let read = |pipe: Option<Box<dyn tokio::io::AsyncRead + Unpin + Send>>| async move {
        let mut buf = Vec::new();
        if let Some(pipe) = pipe {
            pipe.take(PROBE_OUTPUT_LIMIT as u64 + 1)
                .read_to_end(&mut buf)
                .await?;
        }
        if buf.len() > PROBE_OUTPUT_LIMIT {
            // An error, so `try_join!` stops waiting on the other stream,
            // which the probe may hold open for as long as it likes.
            return Err(ProbeError::TooLarge);
        }
        Ok(buf)
    };
    let stdout = child.stdout.take().map(|p| Box::new(p) as _);
    let stderr = child.stderr.take().map(|p| Box::new(p) as _);
    let collected = tokio::time::timeout(time, async {
        let (stdout, stderr) = tokio::try_join!(read(stdout), read(stderr))?;
        let status = child.wait().await?;
        Ok::<_, ProbeError>(std::process::Output {
            status,
            stdout,
            stderr,
        })
    })
    .await;
    let message = match collected {
        Ok(Ok(out)) => return Ok(out),
        Ok(Err(ProbeError::TooLarge)) => {
            format!(
                "{}: probe wrote more than {PROBE_OUTPUT_LIMIT} bytes",
                argv[0]
            )
        }
        Ok(Err(ProbeError::Io(e))) => format!("{}: {e}", argv[0]),
        Err(_) => format!("{}: probe timed out after {time:?}", argv[0]),
    };
    // Kill now and reap it, rather than leave it to `kill_on_drop`: the
    // error is only returned once the probe is gone.
    let _ = child.start_kill();
    let _ = child.wait().await;
    Err(ConnectError { message })
}

enum ProbeError {
    TooLarge,
    Io(std::io::Error),
}

impl From<std::io::Error> for ProbeError {
    fn from(e: std::io::Error) -> Self {
        ProbeError::Io(e)
    }
}

/// A probe's answer and the machine it came from, for the reader's errors
/// and logs: the ssh target, or `local`.
struct Probed {
    target: String,
    out: std::process::Output,
}

/// Run `command` (a POSIX shell command) on the machine behind `ep` and
/// collect its output: under `sh -c` for a local machine, over ssh (the
/// shared master when there is one) for an ssh machine. `None` for a command
/// machine, whose bridge says nothing about what else is where it lands.
/// Every probe of the machine goes through here, so a change to how one
/// reaches a machine lands once.
async fn probe(ep: &Endpoint, command: String) -> Result<Option<Probed>, ConnectError> {
    let (target, argv) = match ep {
        Endpoint::Local { .. } => (
            "local".to_string(),
            vec!["sh".to_string(), "-c".into(), command],
        ),
        Endpoint::Ssh {
            target,
            control_path,
            ..
        } => {
            // Any probe may be the first ssh to this machine, and so start
            // the master.
            ensure_control_dir(control_path.as_deref())?;
            let argv = ssh_argv_running(target, control_path.as_deref(), command);
            (target.clone(), argv)
        }
        Endpoint::Command { .. } => return Ok(None),
    };
    let out = probe_output(&argv).await?;
    Ok(Some(Probed { target, out }))
}

/// Was the machine reached? 255 is ssh's own failure code, and no code at all
/// means the probe was killed: those are transport failures. Any other exit
/// came from the machine, and what it means is the reader's to say.
fn reached(target: &str, out: &std::process::Output) -> Result<(), ConnectError> {
    if matches!(out.status.code(), Some(255) | None) {
        return Err(ConnectError {
            message: format!(
                "ssh {target}: {} ({})",
                String::from_utf8_lossy(&out.stderr).trim(),
                out.status
            ),
        });
    }
    Ok(())
}

/// Reads the answer to `REMOTE_HOME_COMMAND`. Only ssh failing to reach the
/// machine is an error, and so a transport failure; a machine that answered
/// without a usable home is fine, its home is just unknown.
fn remote_home(target: &str, out: &std::process::Output) -> Result<Option<String>, ConnectError> {
    reached(target, out)?;
    let raw = String::from_utf8_lossy(&out.stdout);
    // Only the newline pastor's own `printf` may trail with is trimmed here;
    // `trim_end()` would also eat a tab or form feed, letting a control
    // character through disguised as trailing whitespace.
    let home = raw.trim_end_matches(['\r', '\n']);
    // `?raw` logs stdout escaped, so a control character cannot garble the
    // journal line.
    if !out.status.success() || !home.starts_with('/') || home.chars().any(char::is_control) {
        tracing::warn!(%target, status = %out.status, stdout = ?raw, "no usable $HOME from the remote shell");
        return Ok(None);
    }
    Ok(Some(home.to_string()))
}

/// Asks the remote shell for `$HOME`. ssh is spawned without a local shell, so
/// this string reaches the remote shell as written and it expands `$HOME`.
const REMOTE_HOME_COMMAND: &str = "printf %s \"$HOME\"";

pub type ConnectFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Connection, ConnectError>> + Send + 'a>>;

pub type HomeFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<String>, ConnectError>> + Send + 'a>>;

pub type DirFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<bool>, ConnectError>> + Send + 'a>>;

pub type VersionFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<String>, ConnectError>> + Send + 'a>>;

/// Anything that can open a fresh herdr connection. Endpoints for real use, FakeHerdr in tests.
///
/// A connection carries one request (see `Connection`), so this is called once
/// per request; `ConnectorExt` in `client.rs` has the request vocabulary built
/// on top of it.
pub trait Connector: Send + Sync {
    fn connect(&self) -> ConnectFuture<'_>;
    fn describe(&self) -> String;
    /// The short form of `describe` that `machine list` shows as HOST.
    fn host(&self) -> String {
        self.describe()
    }
    /// The home directory on the machine, for expanding `~` in a repo path:
    /// herdr takes a `cwd` literally and silently falls back to another
    /// directory when it does not exist. `None` when it cannot be known.
    fn home_dir(&self) -> HomeFuture<'_> {
        Box::pin(async { Ok(None) })
    }
    /// Whether `path` is a directory on the machine. herdr opens a workspace
    /// whose `cwd` does not exist somewhere else (the shell's home) without an
    /// error, so dispatch asks first. `None` when it cannot be known.
    fn dir_exists(&self, _path: &str) -> DirFuture<'_> {
        Box::pin(async { Ok(None) })
    }
    /// Whether `path` is a regular file on the machine: which of a repo's
    /// instruction files a profiled opencode task gets back
    /// (`config::opencode::instructions_content`). `None` when it cannot be
    /// known.
    fn file_exists(&self, _path: &str) -> DirFuture<'_> {
        Box::pin(async { Ok(None) })
    }
    /// Make the directory `path` on the machine, parents too, unless it is
    /// already there: where a task with no repo starts. `Some(true)` when it
    /// is there now, `Some(false)` when it could not be made, `None` when it
    /// cannot be known.
    fn ensure_dir(&self, _path: &str) -> DirFuture<'_> {
        Box::pin(async { Ok(None) })
    }
    /// Whether the git checkout at `path` on the machine has commits that are
    /// on no remote. herdr's `worktree.remove` refuses only uncommitted
    /// changes, so auto-close asks this first and keeps such a checkout.
    /// `None` when it cannot be known.
    fn unpushed_commits(&self, _path: &str) -> DirFuture<'_> {
        Box::pin(async { Ok(None) })
    }
    /// Put back a removed git worktree: `git worktree add <path> <branch>`
    /// in the checkout at `repo`, so `task attach` can resume a session
    /// Claude stored under that path. `Some(false)` when the branch is gone,
    /// `None` when it cannot be known or git failed.
    fn restore_worktree(&self, _repo: &str, _path: &str, _branch: &str) -> DirFuture<'_> {
        Box::pin(async { Ok(None) })
    }
    /// Whether the machine's own opencode config sets permission rules,
    /// which opencode would merge with a profile's (`config::opencode`).
    /// `None` when it cannot be known.
    fn opencode_permission_rules(&self) -> DirFuture<'_> {
        Box::pin(async { Ok(None) })
    }
    /// The version of pastor installed on the machine, for `machine list`: a
    /// fleet runs whatever each machine last installed, and a skill or CLI
    /// that an agent there calls is that version's. `None` when there is no
    /// pastor there or it cannot be known.
    fn pastor_version(&self) -> VersionFuture<'_> {
        Box::pin(async { Ok(None) })
    }
}

impl Connector for Endpoint {
    fn connect(&self) -> ConnectFuture<'_> {
        Box::pin(connect(self))
    }
    fn describe(&self) -> String {
        Endpoint::describe(self)
    }
    fn host(&self) -> String {
        Endpoint::host(self)
    }
    fn home_dir(&self) -> HomeFuture<'_> {
        Box::pin(home_dir(self))
    }
    fn dir_exists(&self, path: &str) -> DirFuture<'_> {
        let path = path.to_string();
        Box::pin(async move { dir_exists(self, &path).await })
    }
    fn file_exists(&self, path: &str) -> DirFuture<'_> {
        let path = path.to_string();
        Box::pin(async move { file_exists(self, &path).await })
    }
    fn ensure_dir(&self, path: &str) -> DirFuture<'_> {
        let path = path.to_string();
        Box::pin(async move { ensure_dir(self, &path).await })
    }
    fn unpushed_commits(&self, path: &str) -> DirFuture<'_> {
        let path = path.to_string();
        Box::pin(async move { unpushed_commits(self, &path).await })
    }
    fn restore_worktree(&self, repo: &str, path: &str, branch: &str) -> DirFuture<'_> {
        let command = remote_restore_command(repo, path, branch);
        Box::pin(async move { restore_worktree(self, command).await })
    }
    fn opencode_permission_rules(&self) -> DirFuture<'_> {
        Box::pin(opencode_permission_rules(self))
    }
    fn pastor_version(&self) -> VersionFuture<'_> {
        Box::pin(pastor_version(self))
    }
}

async fn home_dir(ep: &Endpoint) -> Result<Option<String>, ConnectError> {
    // The head and this herdr share a machine, and so a home.
    if let Endpoint::Local { .. } = ep {
        return Ok(dirs::home_dir().map(|p| p.to_string_lossy().into_owned()));
    }
    match probe(ep, REMOTE_HOME_COMMAND.to_string()).await? {
        Some(p) => remote_home(&p.target, &p.out),
        None => Ok(None),
    }
}

async fn dir_exists(ep: &Endpoint, path: &str) -> Result<Option<bool>, ConnectError> {
    // The head and this herdr share a machine, and so a filesystem.
    if let Endpoint::Local { .. } = ep {
        return Ok(Some(std::path::Path::new(path).is_dir()));
    }
    match probe(ep, remote_dir_command(path)).await? {
        Some(p) => remote_dir_answer(&p.target, &p.out),
        None => Ok(None),
    }
}

async fn file_exists(ep: &Endpoint, path: &str) -> Result<Option<bool>, ConnectError> {
    match ep {
        // The head and this herdr share a machine, and so a filesystem.
        Endpoint::Local { .. } => Ok(Some(std::path::Path::new(path).is_file())),
        Endpoint::Ssh {
            target,
            control_path,
            ..
        } => {
            ensure_control_dir(control_path.as_deref())?;
            let argv = ssh_argv_running(target, control_path.as_deref(), remote_file_command(path));
            let out = probe_output(&argv).await?;
            remote_dir_answer(target, &out)
        }
        // An arbitrary bridge command says nothing about where it lands.
        Endpoint::Command { .. } => Ok(None),
    }
}

async fn ensure_dir(ep: &Endpoint, path: &str) -> Result<Option<bool>, ConnectError> {
    // The head and this herdr share a machine, and so a filesystem.
    if let Endpoint::Local { .. } = ep {
        return Ok(Some(std::fs::create_dir_all(path).is_ok()));
    }
    match probe(ep, remote_mkdir_command(path)).await? {
        // The answer reads like `remote_dir_command`'s, save that a failed
        // `mkdir` says `no` rather than nothing.
        Some(p) => Ok(remote_dir_answer(&p.target, &p.out)?.or(Some(false))),
        None => Ok(None),
    }
}

async fn unpushed_commits(ep: &Endpoint, path: &str) -> Result<Option<bool>, ConnectError> {
    // A local machine runs the same git command here.
    match probe(ep, remote_unpushed_command(path)).await? {
        Some(p) => remote_unpushed_answer(&p.target, &p.out),
        None => Ok(None),
    }
}

async fn restore_worktree(ep: &Endpoint, command: String) -> Result<Option<bool>, ConnectError> {
    match probe(ep, command).await? {
        Some(p) => remote_restore_answer(&p.target, &p.out),
        None => Ok(None),
    }
}

/// Re-adds the worktree at `path` on `branch` in `repo`, answering `added`,
/// or `no-branch` when the branch no longer exists. `git worktree prune`
/// first drops what git still remembers of a checkout deleted by hand.
fn remote_restore_command(repo: &str, path: &str, branch: &str) -> String {
    let (repo, path) = (shell_quote(repo), shell_quote(path));
    let branch_ref = shell_quote(&format!("refs/heads/{branch}"));
    let branch = shell_quote(branch);
    format!(
        "cd {repo} && git worktree prune && \
         if git rev-parse --verify --quiet {branch_ref} >/dev/null; \
         then git worktree add {path} {branch} >&2 && echo added; \
         else echo no-branch; fi"
    )
}

/// Reads the answer to `remote_restore_command`, the last line, as
/// `remote_unpushed_answer` does.
fn remote_restore_answer(
    target: &str,
    out: &std::process::Output,
) -> Result<Option<bool>, ConnectError> {
    reached(target, out)?;
    let raw = String::from_utf8_lossy(&out.stdout);
    match raw.trim_end().lines().last().unwrap_or("").trim() {
        "added" if out.status.success() => Ok(Some(true)),
        "no-branch" if out.status.success() => Ok(Some(false)),
        _ => {
            tracing::warn!(%target, status = %out.status, stdout = ?raw, stderr = %String::from_utf8_lossy(&out.stderr).trim(), "git did not re-create the worktree");
            Ok(None)
        }
    }
}

/// Counts the commits of the checkout's HEAD that no remote-tracking branch
/// has, answered on stdout as a number. A repo with no remote counts every
/// commit, so its checkouts are always kept.
fn remote_unpushed_command(path: &str) -> String {
    format!(
        "git -C {} rev-list --count HEAD --not --remotes",
        shell_quote(path)
    )
}

/// Reads the answer to `remote_unpushed_command`. As with `remote_home`, only
/// ssh failing to reach the machine is an error; git failing (no checkout
/// there, no git on the PATH) is unknown. rc-file noise comes before the
/// answer, so the answer is the last line.
fn remote_unpushed_answer(
    target: &str,
    out: &std::process::Output,
) -> Result<Option<bool>, ConnectError> {
    reached(target, out)?;
    let raw = String::from_utf8_lossy(&out.stdout);
    let last = raw.trim_end().lines().last().unwrap_or("").trim();
    match last.parse::<u64>() {
        Ok(n) if out.status.success() => Ok(Some(n > 0)),
        _ => {
            tracing::warn!(%target, status = %out.status, stdout = ?raw, stderr = %String::from_utf8_lossy(&out.stderr).trim(), "no count of unpushed commits from git");
            Ok(None)
        }
    }
}

async fn opencode_permission_rules(ep: &Endpoint) -> Result<Option<bool>, ConnectError> {
    let command = crate::config::opencode::CONFIG_CHECK_COMMAND.to_string();
    match probe(ep, command).await? {
        // The same one-word answer as the repo check.
        Some(p) => remote_dir_answer(&p.target, &p.out),
        None => Ok(None),
    }
}

async fn pastor_version(ep: &Endpoint) -> Result<Option<String>, ConnectError> {
    // The head and this herdr share a machine, so the pastor there is this
    // one.
    if let Endpoint::Local { .. } = ep {
        return Ok(Some(env!("CARGO_PKG_VERSION").to_string()));
    }
    match probe(ep, REMOTE_PASTOR_VERSION_COMMAND.to_string()).await? {
        Some(p) => remote_pastor_version(&p.target, &p.out),
        None => Ok(None),
    }
}

/// Asks the remote machine for `pastor --version`. ssh runs its command in a
/// non-login shell, whose PATH usually lacks `~/.cargo/bin` (where `make
/// install` puts pastor) and `~/.local/bin`, so the command adds them. It
/// answers `none` when there is no pastor, so a missing binary is an answer
/// and not a failure.
const REMOTE_PASTOR_VERSION_COMMAND: &str = r#"PATH="$HOME/.cargo/bin:$HOME/.local/bin:$PATH"; command -v pastor >/dev/null 2>&1 && pastor --version || printf none"#;

/// Reads the answer to `REMOTE_PASTOR_VERSION_COMMAND`: `pastor 0.2.0` is
/// `0.2.0`, `none` is no pastor. As with `remote_home`, only ssh failing to
/// reach the machine is an error. rc-file noise comes before the answer, so
/// the answer is the last line; anything else is logged and read as unknown.
fn remote_pastor_version(
    target: &str,
    out: &std::process::Output,
) -> Result<Option<String>, ConnectError> {
    reached(target, out)?;
    let raw = String::from_utf8_lossy(&out.stdout);
    let last = raw.trim_end().lines().last().unwrap_or("");
    let words: Vec<&str> = last.split_whitespace().collect();
    if out.status.success() {
        match words.as_slice() {
            [.., "none"] => return Ok(None),
            [.., "pastor", v] if v.chars().all(|c| c.is_ascii_graphic()) => {
                return Ok(Some(v.to_string()));
            }
            _ => {}
        }
    }
    tracing::warn!(%target, status = %out.status, stdout = ?raw, "no pastor version from the remote shell");
    Ok(None)
}

/// `test -d` in the remote shell, answered on stdout with one word. `test -d`
/// follows symlinks, as `cd` does.
fn remote_dir_command(path: &str) -> String {
    format!(
        "if test -d {}; then printf yes; else printf no; fi",
        shell_quote(path)
    )
}

/// `test -f` on the machine, answered like `remote_dir_command`.
fn remote_file_command(path: &str) -> String {
    format!(
        "if test -f {}; then printf yes; else printf no; fi",
        shell_quote(path)
    )
}

/// `mkdir -p` on the machine, answered like `remote_dir_command`. Its own
/// complaint goes to stderr, so stdout holds only the answer (and rc noise).
fn remote_mkdir_command(path: &str) -> String {
    format!(
        "if mkdir -p {}; then printf yes; else printf no; fi",
        shell_quote(path)
    )
}

/// Reads the answer to `remote_dir_command`. As with `remote_home`, only ssh
/// failing to reach the machine is an error. Output from rc files comes
/// before the answer, so the answer is the end of stdout.
fn remote_dir_answer(
    target: &str,
    out: &std::process::Output,
) -> Result<Option<bool>, ConnectError> {
    reached(target, out)?;
    let text = String::from_utf8_lossy(&out.stdout);
    let text = text.trim_end();
    if out.status.success() && text.ends_with("yes") {
        Ok(Some(true))
    } else if out.status.success() && text.ends_with("no") {
        Ok(Some(false))
    } else {
        tracing::warn!(%target, status = %out.status, stdout = ?text, "no yes or no from the remote shell");
        Ok(None)
    }
}

pub async fn connect(ep: &Endpoint) -> Result<Connection, ConnectError> {
    match ep {
        Endpoint::Local { session } => {
            let path = local_socket_path(session).map_err(|e| ConnectError {
                message: e.to_string(),
            })?;
            let stream =
                tokio::net::UnixStream::connect(&path)
                    .await
                    .map_err(|e| ConnectError {
                        message: format!("connect {}: {e}", path.display()),
                    })?;
            let (r, w) = stream.into_split();
            Ok(Connection::new(Box::new(r), Box::new(w)))
        }
        Endpoint::Ssh {
            target,
            session,
            control_path,
        } => {
            ensure_control_dir(control_path.as_deref())?;
            spawn(&ssh_argv(target, session, control_path.as_deref())).await
        }
        Endpoint::Command { argv } => spawn(argv).await,
    }
}

/// Spawn argv with piped stdio. The bridge is not proven alive here: that is the
/// first request's job, and `Connection` reports the child's exit status and
/// stderr if it died before replying (see `Connection::diagnose`).
async fn spawn(argv: &[String]) -> Result<Connection, ConnectError> {
    let (program, args) = argv.split_first().ok_or_else(|| ConnectError {
        message: "empty command".into(),
    })?;
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| ConnectError {
            message: format!("spawn {program}: {e}"),
        })?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    Ok(
        Connection::new(Box::new(stdout), Box::new(stdin)).with_bridge(
            argv.to_vec(),
            child,
            stderr,
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ssh_machine(name: &str) -> MachineConfig {
        MachineConfig {
            pull: false,
            description: None,
            name: name.into(),
            local: false,
            ssh: Some("fleet@host".into()),
            command: None,
            session: "default".into(),
            max_agents: 2,
            job_slots: 1,
            burst: 1,
            tags: vec![],
            flock: None,
            agent: None,
            agent_args: None,
            model: None,
            priority: None,
            agents: Default::default(),
            profile: None,
        }
    }

    /// `machine list` shows where each machine lives in a few characters: the
    /// ssh target as written, `local`, or the program a command machine runs.
    #[test]
    fn host_is_the_ssh_target_local_or_the_command_program() {
        let paths = Paths::new("/c", "/s");
        assert_eq!(
            Endpoint::from_machine(&ssh_machine("pi-3"), &paths).host(),
            "fleet@host"
        );
        let local = MachineConfig {
            local: true,
            ssh: None,
            ..ssh_machine("here")
        };
        assert_eq!(Endpoint::from_machine(&local, &paths).host(), "local");
        let command = MachineConfig {
            ssh: None,
            command: Some(vec![
                "/opt/bin/fake-herdr".into(),
                "--connect".into(),
                "/tmp/h.sock".into(),
            ]),
            ..ssh_machine("fake")
        };
        assert_eq!(
            Endpoint::from_machine(&command, &paths).host(),
            "fake-herdr"
        );
        assert_eq!(Endpoint::Command { argv: vec![] }.host(), "-");
    }

    /// The machine name in the ControlPath is only there to be recognisable;
    /// `%C` is the identity. A name that would push the socket name past
    /// `sun_path` must be shortened, not cost every request a full handshake.
    /// `pastor-sauron` under the default state dir is exactly that case.
    #[test]
    fn long_machine_names_are_shortened_to_keep_multiplexing() {
        let paths = Paths::new(
            "/home/exampleuser/.config/pastor",
            "/home/exampleuser/.local/state/pastor",
        );
        let Endpoint::Ssh { control_path, .. } =
            Endpoint::from_machine(&ssh_machine("pastor-sauron"), &paths)
        else {
            panic!("ssh machine")
        };
        let path = control_path.expect("a shortened path must still multiplex");
        assert!(crate::ssh::control_path_fits(&path), "{}", path.display());
        let text = path.to_string_lossy();
        assert!(
            text.starts_with("/home/exampleuser/.local/state/pastor/ssh/pastor"),
            "{text}"
        );
        assert!(text.ends_with("-%C"), "{text}");
        assert!(
            text.len()
                < paths
                    .ssh_control_path("pastor-sauron")
                    .to_string_lossy()
                    .len()
        );

        // A short name is untouched.
        let Endpoint::Ssh { control_path, .. } =
            Endpoint::from_machine(&ssh_machine("cberry"), &paths)
        else {
            panic!()
        };
        assert_eq!(control_path, Some(paths.ssh_control_path("cberry")));

        // Two machines whose names only differ past the cut share nothing but
        // the directory: `%C` hashes the target, so distinct hosts stay apart.
        let Endpoint::Ssh {
            control_path: a, ..
        } = Endpoint::from_machine(&ssh_machine("pastor-sauron-one"), &paths)
        else {
            panic!()
        };
        assert!(a.is_some());

        // Only when even `-%C` alone will not fit does multiplexing go away.
        let deep = Paths::new("/c", format!("/{}", "x".repeat(70)));
        let Endpoint::Ssh { control_path, .. } = Endpoint::from_machine(&ssh_machine("m"), &deep)
        else {
            panic!()
        };
        assert_eq!(control_path, None);
    }
    use crate::herdr::ConnectorExt;

    /// herdr's socket is under `XDG_CONFIG_HOME`, so a path there past
    /// `sun_path` says to shorten that (or the session name), before any
    /// connect gets the OS's bare refusal.
    #[test]
    fn local_socket_path_refuses_a_path_past_sun_path() {
        let deep = PathBuf::from(format!("/{}", "x".repeat(crate::ssh::UNIX_PATH_MAX)));
        let err = socket_path_under(&deep, "default").unwrap_err();
        let e = err
            .downcast_ref::<crate::cli::CliError>()
            .expect("CliError");
        assert_eq!(e.code, "config_error");
        assert!(e.message.contains("XDG_CONFIG_HOME"), "{}", e.message);
        let p = socket_path_under(Path::new("/c"), "s").unwrap();
        assert_eq!(p, Path::new("/c/herdr/sessions/s/herdr.sock"));
    }

    #[test]
    fn socket_paths_and_bridge_command() {
        let p = local_socket_path("default").unwrap();
        assert!(p.ends_with("herdr/herdr.sock"), "{}", p.display());
        assert!(!p.to_string_lossy().contains("Library"), "{}", p.display());
        let p = local_socket_path("agents").unwrap();
        assert!(p.ends_with("herdr/sessions/agents/herdr.sock"));
        assert_eq!(
            bridge_command("default"),
            "herdr --session default remote-api-bridge"
        );
        assert_eq!(
            bridge_command("my session"),
            "herdr --session 'my session' remote-api-bridge"
        );
        assert_eq!(shell_quote(""), "''");
        assert_eq!(bridge_command(""), "herdr --session '' remote-api-bridge");
    }

    #[test]
    fn ssh_endpoint_multiplexes_through_one_master() {
        let m = MachineConfig {
            pull: false,
            description: None,
            name: "pi-3".into(),
            local: false,
            ssh: Some("fleet@pi-3".into()),
            command: None,
            session: "default".into(),
            max_agents: 2,
            job_slots: 1,
            burst: 1,
            tags: vec![],
            flock: None,
            agent: None,
            agent_args: None,
            model: None,
            priority: None,
            agents: Default::default(),
            profile: None,
        };
        let paths = Paths::new("/tmp/c", "/tmp/s");
        let ep = Endpoint::from_machine(&m, &paths);
        let Endpoint::Ssh {
            target,
            session,
            control_path,
        } = &ep
        else {
            panic!("expected an ssh endpoint, got {ep:?}");
        };
        // Named after the machine plus ssh's `%C`, so retargeting the machine
        // cannot reuse a master still attached to the old host.
        assert_eq!(
            control_path.as_deref(),
            Some(Path::new("/tmp/s/ssh/pi-3-%C"))
        );
        let argv = ssh_argv(target, session, control_path.as_deref());
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "-o" && w[1] == "ControlMaster=auto")
        );
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "-o" && w[1] == "ControlPath=/tmp/s/ssh/pi-3-%C")
        );
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "-o" && w[1] == "ControlPersist=600")
        );
        // The remote command is the only element a shell ever parses.
        assert_eq!(
            argv.last().unwrap(),
            "sh -c 'herdr --session default remote-api-bridge'"
        );
        assert_eq!(argv[argv.len() - 2], "fleet@pi-3");
        // `--` ends ssh's options, so the target is never read as one.
        assert_eq!(argv[argv.len() - 3], "--");
    }

    /// `~` in a repo path is the home on the machine: ssh asks the remote
    /// shell, over the same master as every request; a local machine is the
    /// head's own home; a bridge command cannot know.
    #[tokio::test]
    async fn home_dir_per_endpoint() {
        let argv = ssh_argv_running(
            "fleet@pi-3",
            Some(Path::new("/tmp/s/ssh/pi-3-%C")),
            REMOTE_HOME_COMMAND.to_string(),
        );
        assert_eq!(argv.last().unwrap(), "sh -c 'printf %s \"$HOME\"'");
        assert_eq!(argv[argv.len() - 2], "fleet@pi-3");
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "-o" && w[1] == "ControlPath=/tmp/s/ssh/pi-3-%C")
        );

        let local = Endpoint::Local {
            session: "default".into(),
        };
        assert_eq!(
            local.home_dir().await.unwrap(),
            dirs::home_dir().map(|p| p.to_string_lossy().into_owned())
        );
        let command = Endpoint::Command {
            argv: vec!["true".into()],
        };
        assert_eq!(command.home_dir().await.unwrap(), None);
    }

    #[tokio::test]
    async fn dir_exists_per_endpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo");
        std::fs::create_dir(&dir).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        let local = Endpoint::Local {
            session: "s".into(),
        };
        assert_eq!(
            local.dir_exists(dir.to_str().unwrap()).await.unwrap(),
            Some(true)
        );
        assert_eq!(
            local.dir_exists(link.to_str().unwrap()).await.unwrap(),
            Some(true),
            "a symlink to a directory is a directory to the shell too"
        );
        assert_eq!(
            local
                .dir_exists(tmp.path().join("nope").to_str().unwrap())
                .await
                .unwrap(),
            Some(false)
        );
        let command = Endpoint::Command {
            argv: vec!["true".into()],
        };
        assert_eq!(command.dir_exists("/").await.unwrap(), None);
    }

    #[tokio::test]
    async fn file_exists_per_endpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("AGENTS.md");
        std::fs::write(&file, "x").unwrap();
        let local = Endpoint::Local {
            session: "s".into(),
        };
        assert_eq!(
            local.file_exists(file.to_str().unwrap()).await.unwrap(),
            Some(true)
        );
        assert_eq!(
            local
                .file_exists(tmp.path().to_str().unwrap())
                .await
                .unwrap(),
            Some(false),
            "a directory is not a file"
        );
        let command = Endpoint::Command {
            argv: vec!["true".into()],
        };
        assert_eq!(command.file_exists("/").await.unwrap(), None);
    }

    /// A local machine shares the head's pastor; a bridge command cannot know.
    /// The ssh command runs through `sh -c` so the PATH it adds is POSIX
    /// whatever the login shell is (fish, for one).
    #[tokio::test]
    async fn pastor_version_per_endpoint() {
        let argv = ssh_argv_running(
            "fleet@pi-3",
            Some(Path::new("/tmp/s/ssh/pi-3-%C")),
            REMOTE_PASTOR_VERSION_COMMAND.to_string(),
        );
        assert!(
            argv.last()
                .unwrap()
                .starts_with("sh -c 'PATH=\"$HOME/.cargo/bin:"),
            "{argv:?}"
        );
        assert!(
            argv.windows(2)
                .any(|w| w[0] == "-o" && w[1] == "ControlPath=/tmp/s/ssh/pi-3-%C")
        );
        let local = Endpoint::Local {
            session: "default".into(),
        };
        assert_eq!(
            local.pastor_version().await.unwrap().as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
        let command = Endpoint::Command {
            argv: vec!["true".into()],
        };
        assert_eq!(command.pastor_version().await.unwrap(), None);
    }

    /// The command's own shell script, run here, answers the way the parser
    /// expects: `none` with no pastor on the PATH it builds.
    #[test]
    fn the_remote_command_answers_none_without_pastor() {
        let tmp = tempfile::tempdir().unwrap();
        let out = std::process::Command::new("sh")
            .args(["-c", REMOTE_PASTOR_VERSION_COMMAND])
            .env("HOME", tmp.path())
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        assert_eq!(remote_pastor_version("t", &out).unwrap(), None);
    }

    /// Only ssh failing to reach the machine is an error; an odd answer from a
    /// reachable one is an unknown version.
    #[test]
    fn remote_pastor_version_reads_the_last_line() {
        use std::os::unix::process::ExitStatusExt;
        let out = |code: i32, stdout: &str| std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: b"boom".to_vec(),
        };
        let v = |code: i32, stdout: &str| remote_pastor_version("t", &out(code, stdout)).unwrap();
        assert_eq!(v(0, "pastor 0.2.0\n").as_deref(), Some("0.2.0"));
        assert_eq!(v(0, "none"), None);
        assert_eq!(v(0, ""), None);
        assert_eq!(v(1, ""), None);
        assert_eq!(
            v(1, "pastor 0.2.0\n"),
            None,
            "a failed command is no answer"
        );
        assert_eq!(
            v(
                0,
                "Welcome to Raspberry Pi\nLast login: today\npastor 0.2.0\n"
            )
            .as_deref(),
            Some("0.2.0")
        );
        assert_eq!(v(0, "welcome\nnone"), None);
        assert_eq!(v(0, "pastor 0.2.0\nmotd after"), None);
        assert_eq!(v(0, "pastor 0.2\u{1b}[0m"), None);
        assert!(remote_pastor_version("t", &out(255, "")).is_err());
        let killed = std::process::Output {
            status: std::process::ExitStatus::from_raw(9),
            stdout: vec![],
            stderr: vec![],
        };
        assert!(remote_pastor_version("t", &killed).is_err());
    }

    /// ssh hands its command to the remote user's login shell, which may be
    /// fish or csh. Every remote command is wrapped as `sh -c '<command>'`,
    /// so that shell only parses one word and one single-quoted string, and
    /// sh parses the rest. Run here the way sshd runs it, through a shell's
    /// `-c`, the path comes out whole.
    #[test]
    fn remote_commands_run_under_sh_whatever_the_login_shell() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("my app's $HOME `x`");
        std::fs::create_dir(&dir).unwrap();
        let path = dir.to_str().unwrap();
        let argv = ssh_argv_running("fleet@pi-3", None, remote_dir_command(path));
        let remote = argv.last().unwrap();
        assert!(remote.starts_with("sh -c 'if test -d "), "{remote}");
        let out = std::process::Command::new("sh")
            .args(["-c", remote])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "yes");
        assert!(
            ssh_argv("fleet@pi-3", "default", None)
                .last()
                .unwrap()
                .starts_with("sh -c 'herdr --session default remote-api-bridge'")
        );
    }

    #[test]
    fn remote_dir_command_quotes_the_path() {
        assert_eq!(
            remote_dir_command("/srv/my app/it's"),
            "if test -d '/srv/my app/it'\\''s'; then printf yes; else printf no; fi"
        );
    }

    #[test]
    fn remote_file_command_quotes_the_path() {
        assert_eq!(
            remote_file_command("/srv/my app/AGENTS.md"),
            "if test -f '/srv/my app/AGENTS.md'; then printf yes; else printf no; fi"
        );
    }

    #[test]
    fn remote_mkdir_command_quotes_the_path() {
        assert_eq!(
            remote_mkdir_command("/home/u/pastor tasks"),
            "if mkdir -p '/home/u/pastor tasks'; then printf yes; else printf no; fi"
        );
    }

    #[tokio::test]
    async fn ensure_dir_per_endpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let local = Endpoint::Local {
            session: "s".into(),
        };
        let dir = tmp.path().join("a/pastor-tasks");
        assert_eq!(
            local.ensure_dir(dir.to_str().unwrap()).await.unwrap(),
            Some(true)
        );
        assert!(dir.is_dir());
        assert_eq!(
            local.ensure_dir(dir.to_str().unwrap()).await.unwrap(),
            Some(true),
            "already there is fine"
        );
        let file = tmp.path().join("file");
        std::fs::write(&file, "").unwrap();
        assert_eq!(
            local.ensure_dir(file.to_str().unwrap()).await.unwrap(),
            Some(false)
        );
        let command = Endpoint::Command {
            argv: vec!["true".into()],
        };
        assert_eq!(command.ensure_dir("/").await.unwrap(), None);
    }

    /// Only ssh failing to reach the machine is an error. rc-file noise comes
    /// before the answer, so the answer is read from the end of stdout.
    #[test]
    fn remote_dir_answer_reads_the_last_word() {
        use std::os::unix::process::ExitStatusExt;
        let out = |code: i32, stdout: &str| std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: b"boom".to_vec(),
        };
        assert_eq!(remote_dir_answer("t", &out(0, "yes")).unwrap(), Some(true));
        assert_eq!(remote_dir_answer("t", &out(0, "no")).unwrap(), Some(false));
        assert_eq!(
            remote_dir_answer("t", &out(0, "welcome to pi\nyes")).unwrap(),
            Some(true)
        );
        assert_eq!(remote_dir_answer("t", &out(0, "")).unwrap(), None);
        assert_eq!(remote_dir_answer("t", &out(1, "")).unwrap(), None);
        assert!(remote_dir_answer("t", &out(255, "")).is_err());
    }

    #[test]
    fn remote_restore_answer_reads_the_outcome() {
        use std::os::unix::process::ExitStatusExt;
        let out = |code: i32, stdout: &str| std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: b"boom".to_vec(),
        };
        assert_eq!(
            remote_restore_answer("t", &out(0, "welcome\nadded\n")).unwrap(),
            Some(true)
        );
        assert_eq!(
            remote_restore_answer("t", &out(0, "no-branch\n")).unwrap(),
            Some(false)
        );
        assert_eq!(remote_restore_answer("t", &out(128, "")).unwrap(), None);
        assert!(remote_restore_answer("t", &out(255, "")).is_err());
    }

    /// Everything that reaches the remote shell is quoted.
    #[test]
    fn remote_restore_command_quotes_what_it_is_given() {
        let c = remote_restore_command("/r/my repo", "/w/t 1", "pastor/t-1;rm");
        assert!(
            c.starts_with("cd '/r/my repo' && git worktree prune"),
            "{c}"
        );
        assert!(c.contains("--quiet 'refs/heads/pastor/t-1;rm'"), "{c}");
        assert!(
            c.contains("git worktree add '/w/t 1' 'pastor/t-1;rm' >&2"),
            "{c}"
        );
    }

    #[test]
    fn remote_unpushed_answer_reads_the_count() {
        use std::os::unix::process::ExitStatusExt;
        let out = |code: i32, stdout: &str| std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: b"boom".to_vec(),
        };
        assert_eq!(
            remote_unpushed_answer("t", &out(0, "2\n")).unwrap(),
            Some(true)
        );
        assert_eq!(
            remote_unpushed_answer("t", &out(0, "0\n")).unwrap(),
            Some(false)
        );
        assert_eq!(
            remote_unpushed_answer("t", &out(0, "welcome to pi\n0\n")).unwrap(),
            Some(false)
        );
        assert_eq!(remote_unpushed_answer("t", &out(128, "")).unwrap(), None);
        assert!(remote_unpushed_answer("t", &out(255, "")).is_err());
    }

    /// Against a real git: a commit is unpushed until a remote has it.
    #[tokio::test]
    async fn a_local_checkout_reports_unpushed_commits() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let st = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "init.defaultBranch=main",
                ])
                .args(args)
                .current_dir(dir.path())
                // The developer's own git config (commit signing, hooks) must
                // not reach a throwaway repo.
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(st.status.success(), "{args:?}: {st:?}");
        };
        git(&["init", "-q", "work"]);
        git(&["init", "-q", "--bare", "origin.git"]);
        git(&["-C", "work", "commit", "-q", "--allow-empty", "-m", "one"]);
        let ep = Endpoint::Local {
            session: "s".into(),
        };
        let work = dir.path().join("work");
        let work = work.to_str().unwrap();
        assert_eq!(ep.unpushed_commits(work).await.unwrap(), Some(true));
        git(&["-C", "work", "remote", "add", "origin", "../origin.git"]);
        git(&["-C", "work", "push", "-q", "origin", "HEAD:main"]);
        assert_eq!(ep.unpushed_commits(work).await.unwrap(), Some(false));
        let missing = dir.path().join("gone");
        assert_eq!(
            ep.unpushed_commits(missing.to_str().unwrap())
                .await
                .unwrap(),
            None
        );
    }

    /// Only ssh itself failing (255, or killed) means the machine was not
    /// reached. An unset or relative `$HOME`, or a remote command that fails,
    /// comes from a reachable machine and must not mark it lost.
    /// A probe answers a word or a path. A fleet machine that answers with
    /// an endless stream is cut off at the cap, not buffered on the head.
    #[tokio::test]
    async fn probe_output_is_capped() {
        let argv = |script: &str| vec!["sh".to_string(), "-c".into(), script.into()];
        let out = probe_output(&argv("printf /home/x; printf err >&2; exit 3"))
            .await
            .unwrap();
        assert_eq!(out.stdout, b"/home/x");
        assert_eq!(out.stderr, b"err");
        assert_eq!(out.status.code(), Some(3));

        let err = probe_output(&argv("yes")).await.unwrap_err();
        assert!(err.message.contains("more than"), "{}", err.message);
        let err = probe_output(&argv("yes >&2")).await.unwrap_err();
        assert!(err.message.contains("more than"), "{}", err.message);
        // Exactly at the cap is fine.
        let out = probe_output(&argv(&format!("head -c {PROBE_OUTPUT_LIMIT} /dev/zero")))
            .await
            .unwrap();
        assert_eq!(out.stdout.len(), PROBE_OUTPUT_LIMIT);
    }

    /// Past the cap on one stream, a probe that keeps its other pipe open
    /// is killed at once instead of holding the head until it exits.
    #[tokio::test]
    async fn an_overflowing_probe_is_killed_at_once() {
        let argv = |script: &str| vec!["sh".to_string(), "-c".into(), script.into()];
        for script in [
            format!("head -c {} /dev/zero; sleep 30", PROBE_OUTPUT_LIMIT + 1),
            format!("head -c {} /dev/zero >&2; sleep 30", PROBE_OUTPUT_LIMIT + 1),
        ] {
            let started = std::time::Instant::now();
            let err = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                probe_output(&argv(&script)),
            )
            .await
            .expect("probe_output hung past the cap")
            .unwrap_err();
            assert!(err.message.contains("more than"), "{}", err.message);
            assert!(started.elapsed() < std::time::Duration::from_secs(5));
        }
    }

    /// A probe that never answers, or never closes its pipes, is given up
    /// on after its time, not waited on forever.
    #[tokio::test]
    async fn a_silent_probe_times_out() {
        let argv = vec!["sh".to_string(), "-c".into(), "sleep 30".into()];
        let started = std::time::Instant::now();
        let err = probe_output_within(&argv, std::time::Duration::from_millis(200))
            .await
            .unwrap_err();
        assert!(err.message.contains("timed out"), "{}", err.message);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    /// Every probe goes through `probe`: the command under `sh -c` here,
    /// over ssh on an ssh machine, and nowhere for a command machine, whose
    /// bridge says nothing about where it lands.
    #[tokio::test]
    async fn probe_runs_the_command_where_the_machine_is() {
        let local = Endpoint::Local {
            session: "s".into(),
        };
        let answer = probe(&local, "printf yes; exit 3".into())
            .await
            .unwrap()
            .expect("a local machine runs the probe");
        assert_eq!(answer.target, "local");
        assert_eq!(answer.out.stdout, b"yes");
        assert_eq!(answer.out.status.code(), Some(3));
        let command = Endpoint::Command {
            argv: vec!["true".into()],
        };
        assert!(
            probe(&command, "printf yes".into())
                .await
                .unwrap()
                .is_none()
        );
    }

    /// ssh's own failure (255) or a killed probe means the machine was not
    /// reached, with ssh's stderr in the message; any other exit came from
    /// the machine and is for the caller to read.
    #[test]
    fn reached_is_only_an_error_when_ssh_failed() {
        use std::os::unix::process::ExitStatusExt;
        let out = |raw: i32| std::process::Output {
            status: std::process::ExitStatus::from_raw(raw),
            stdout: vec![],
            stderr: b"no route to host\n".to_vec(),
        };
        assert!(reached("t", &out(0)).is_ok());
        assert!(reached("t", &out(1 << 8)).is_ok());
        let err = reached("pi-3", &out(255 << 8)).unwrap_err();
        assert!(
            err.message.starts_with("ssh pi-3: no route to host ("),
            "{}",
            err.message
        );
        assert!(reached("t", &out(9)).is_err(), "killed");
    }

    #[test]
    fn remote_home_separates_unreachable_from_unknown() {
        use std::os::unix::process::ExitStatusExt;
        let out = |code: i32, stdout: &str| std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: b"boom".to_vec(),
        };
        assert_eq!(
            remote_home("t", &out(0, "/home/pi")).unwrap(),
            Some("/home/pi".into())
        );
        assert_eq!(remote_home("t", &out(0, "/")).unwrap(), Some("/".into()));
        assert_eq!(remote_home("t", &out(0, "")).unwrap(), None);
        assert_eq!(remote_home("t", &out(0, "home/pi")).unwrap(), None);
        assert_eq!(remote_home("t", &out(1, "")).unwrap(), None);
        assert!(remote_home("t", &out(255, "")).is_err());
        // A trailing newline or CRLF is trimmed; any other control character
        // makes the home unknown rather than part of a path handed to herdr.
        assert_eq!(
            remote_home("t", &out(0, "/home/pi\n")).unwrap(),
            Some("/home/pi".into())
        );
        assert_eq!(
            remote_home("t", &out(0, "/home/pi\r\n")).unwrap(),
            Some("/home/pi".into())
        );
        // A trailing tab or form feed is a control character, not part of
        // the newline pastor trims, so it must still reject the home.
        assert_eq!(remote_home("t", &out(0, "/home/pi\t\n")).unwrap(), None);
        assert_eq!(remote_home("t", &out(0, "/home/pi\x0c\n")).unwrap(), None);
        assert_eq!(
            remote_home("t", &out(0, "/home/p\u{1b}[0mi")).unwrap(),
            None
        );
        assert_eq!(remote_home("t", &out(0, "/home/pi\nmotd")).unwrap(), None);
        let killed = std::process::Output {
            status: std::process::ExitStatus::from_raw(9),
            stdout: vec![],
            stderr: vec![],
        };
        assert!(remote_home("t", &killed).is_err());
    }

    #[test]
    fn a_control_path_that_cannot_fit_drops_multiplexing() {
        // 40 bytes for %C, 17 for ssh's staging and a NUL leave about 50 for the
        // directory and the name, so a deep state dir runs out.
        let deep = format!("/tmp/{}", "d".repeat(80));
        let m = MachineConfig {
            pull: false,
            description: None,
            name: "pi-3".into(),
            local: false,
            ssh: Some("fleet@pi-3".into()),
            command: None,
            session: "default".into(),
            max_agents: 2,
            job_slots: 1,
            burst: 1,
            tags: vec![],
            flock: None,
            agent: None,
            agent_args: None,
            model: None,
            priority: None,
            agents: Default::default(),
            profile: None,
        };
        let paths = Paths::new("/tmp/c", &deep);
        let Endpoint::Ssh {
            target,
            session,
            control_path,
        } = Endpoint::from_machine(&m, &paths)
        else {
            panic!("expected an ssh endpoint");
        };
        assert!(control_path.is_none(), "{control_path:?}");
        let argv = ssh_argv(&target, &session, control_path.as_deref());
        for opt in ["ControlMaster=no", "ControlPath=none"] {
            assert!(
                argv.windows(2).any(|w| w == ["-o", opt]),
                "a machine still connects, just without multiplexing, whatever ~/.ssh/config says: {argv:?}"
            );
        }
        assert!(
            !argv.iter().any(|a| a.starts_with("ControlPersist")),
            "{argv:?}"
        );
        assert_eq!(
            argv.last().unwrap(),
            "sh -c 'herdr --session default remote-api-bridge'"
        );
    }

    #[tokio::test]
    async fn process_transport_reports_exit_and_stderr() {
        let ep = Endpoint::Command {
            argv: vec![
                "sh".into(),
                "-c".into(),
                "echo permission denied >&2; exit 255".into(),
            ],
        };
        // The connect itself succeeds now (it only spawns); the request is what
        // discovers the process is gone, and it must still name argv, the exit
        // status and stderr.
        let err = ep.ping().await.err().unwrap();
        let message = err.to_string();
        assert!(message.contains("permission denied"), "{message}");
        assert!(message.contains("255"), "{message}");
        assert!(message.contains("sh -c"), "{message}");
        assert!(err.is_transport(), "{message}");
    }

    #[tokio::test]
    async fn local_transport_reports_missing_socket() {
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", "/nonexistent-pastor-test");
        }
        let err = connect(&Endpoint::Local {
            session: "default".into(),
        })
        .await
        .err()
        .unwrap();
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
        }
        assert!(err.message.contains("herdr.sock"), "{}", err.message);
    }

    /// Run `script` in a real `/bin/sh` and return what it printed.
    fn sh_output(script: &str) -> Vec<u8> {
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .output()
            .expect("run /bin/sh");
        assert!(out.status.success(), "{script:?}: {out:?}");
        out.stdout
    }

    /// The words `/bin/sh` passes to `name` when it runs `command`, with
    /// `name` defined as a function that prints its arguments NUL-separated.
    fn words_for(name: &str, command: &str) -> Vec<String> {
        let script = format!("{name}() {{ printf '%s\\0' \"$@\"; }}\n{command}");
        let out = String::from_utf8(sh_output(&script)).expect("utf-8");
        out.split_terminator('\0').map(String::from).collect()
    }

    proptest::proptest! {
        // Each case spawns a shell.
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(64))]

        /// A shell prints back exactly what was quoted: NUL aside (no argv
        /// can hold one), no input escapes its single word.
        #[test]
        fn prop_shell_quote_round_trips_through_sh(s in "[^\\x00]*") {
            let out = sh_output(&format!("printf %s {}", shell_quote(&s)));
            proptest::prop_assert_eq!(out, s.as_bytes());
        }

        #[test]
        fn prop_posix_command_keeps_the_command_one_word(command in "[^\\x00]*") {
            proptest::prop_assert_eq!(
                words_for("sh", &posix_command(&command)),
                vec!["-c".to_string(), command]
            );
        }

        #[test]
        fn prop_bridge_command_keeps_the_session_one_word(session in "[^\\x00]*") {
            proptest::prop_assert_eq!(
                words_for("herdr", &bridge_command(&session)),
                vec!["--session".to_string(), session, "remote-api-bridge".to_string()]
            );
        }
    }
}
