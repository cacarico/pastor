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

/// What pastor's own commands on an ssh machine start with: the directory of
/// mise's shims (`$MISE_DATA_DIR/shims`, else `~/.local/share/mise/shims`)
/// first on the PATH when it is there. ssh runs the command in a shell that
/// is neither a login nor an interactive one, so it reads none of the files
/// that put the shims on the PATH, and a harness or herdr that mise provides
/// is not found. The shell's files are left as they are.
pub(crate) const MISE_SHIMS: &str = r#"s="${MISE_DATA_DIR:-$HOME/.local/share/mise}/shims"; [ -d "$s" ] && PATH="$s:$PATH"; export PATH; "#;

/// ssh argv that runs `remote` (a POSIX shell command, run through
/// `posix_command`) over the shared master when there is one, with the mise
/// shims on its PATH (`MISE_SHIMS`).
fn ssh_argv_running(target: &str, control_path: Option<&Path>, remote: String) -> Vec<String> {
    let ssh = crate::ssh::Ssh {
        target,
        control_path,
        control_persist: CONTROL_PERSIST,
        keepalive: true,
        no_tty: true,
        identity: None,
        extra_identity: None,
        known_hosts: None,
        accept_new: false,
        config_file: None,
    };
    let mut argv = vec!["ssh".to_string()];
    argv.extend(ssh.args(&format!("{MISE_SHIMS}{remote}")));
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
pub(crate) const PROBE_OUTPUT_LIMIT: usize = 64 * 1024;

/// How long a probe may take, ssh handshake included. ssh's own keepalive
/// gives up on a dead link after 45s; this also covers a machine whose shell
/// answers the connection and then never exits.
pub(crate) const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

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
    probe_output_feeding(argv, &[], time).await
}

/// `probe_output_within` with `input` on the probe's stdin (none when empty),
/// for a probe that takes a file, such as `tar -xf -` unpacking a
/// provisioner. The input is written while the output is read, so a probe
/// that answers before it has read everything cannot deadlock on a full pipe;
/// one that stops reading early (it failed) only loses the rest.
pub(crate) async fn probe_output_feeding(
    argv: &[String],
    input: &[u8],
    time: std::time::Duration,
) -> Result<std::process::Output, ConnectError> {
    probe_output_limited(argv, input, time, PROBE_OUTPUT_LIMIT).await
}

/// The most a probe that reads a file may write: a harness's store grows by
/// an entry for every folder it has been in, and is already past
/// `PROBE_OUTPUT_LIMIT`.
pub(crate) const FILE_OUTPUT_LIMIT: usize = 16 * 1024 * 1024;

/// `probe_output_feeding` with its own limit on each output stream.
pub(crate) async fn probe_output_limited(
    argv: &[String],
    input: &[u8],
    time: std::time::Duration,
    limit: usize,
) -> Result<std::process::Output, ConnectError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut child = tokio::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(if input.is_empty() {
            Stdio::null()
        } else {
            Stdio::piped()
        })
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
            pipe.take(limit as u64 + 1).read_to_end(&mut buf).await?;
        }
        if buf.len() > limit {
            // An error, so `try_join!` stops waiting on the other stream,
            // which the probe may hold open for as long as it likes.
            return Err(ProbeError::TooLarge);
        }
        Ok(buf)
    };
    let stdout = child.stdout.take().map(|p| Box::new(p) as _);
    let stderr = child.stderr.take().map(|p| Box::new(p) as _);
    let stdin = child.stdin.take();
    let write = async move {
        if let Some(mut stdin) = stdin {
            // A broken pipe is the probe's own failure, which its exit
            // status reports; dropping stdin closes it, so `tar` sees the end.
            let _ = stdin.write_all(input).await;
        }
        Ok::<_, ProbeError>(())
    };
    let collected = tokio::time::timeout(time, async {
        let (stdout, stderr, ()) = tokio::try_join!(read(stdout), read(stderr), write)?;
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
            format!("{}: probe wrote more than {limit} bytes", argv[0])
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
    probe_with(ep, command, &[], PROBE_OUTPUT_LIMIT).await
}

/// `probe` with `input` on the command's stdin and its own output limit: how
/// a file's content goes to a machine and comes back, never on a command
/// line.
async fn probe_with(
    ep: &Endpoint,
    command: String,
    input: &[u8],
    limit: usize,
) -> Result<Option<Probed>, ConnectError> {
    let Some(argv) = shell_argv(ep, command)? else {
        return Ok(None);
    };
    let target = match ep {
        Endpoint::Ssh { target, .. } => target.clone(),
        _ => "local".to_string(),
    };
    let out = probe_output_limited(&argv, input, PROBE_TIMEOUT, limit).await?;
    Ok(Some(Probed { target, out }))
}

/// The argv that runs `command` (a POSIX shell command) on the machine
/// behind `ep`: `sh -c` for a local machine, ssh (the shared master when
/// there is one) for an ssh machine, `None` for a command machine. Any of
/// them may be the first ssh to the machine, and so start the master.
pub(crate) fn shell_argv(
    ep: &Endpoint,
    command: String,
) -> Result<Option<Vec<String>>, ConnectError> {
    match ep {
        Endpoint::Local { .. } => Ok(Some(vec!["sh".to_string(), "-c".into(), command])),
        Endpoint::Ssh {
            target,
            control_path,
            ..
        } => {
            ensure_control_dir(control_path.as_deref())?;
            Ok(Some(ssh_argv_running(
                target,
                control_path.as_deref(),
                command,
            )))
        }
        Endpoint::Command { .. } => Ok(None),
    }
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

/// What `Connector::fetch_base` found: `None` for a machine that cannot be
/// asked.
pub type FetchFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<Fetched>, ConnectError>> + Send + 'a>>;

/// The answer to fetching a repo's base ref before a worktree is made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fetched {
    /// The remote (named) was fetched.
    Done(String),
    /// The repo has no remote to fetch from: nothing to do, nothing wrong.
    NoRemote,
    /// git could not fetch the remote (named); why.
    Failed(String, String),
}

pub type VersionFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<String>, ConnectError>> + Send + 'a>>;

pub type AgySettingsFuture<'a> = Pin<
    Box<
        dyn Future<Output = Result<Option<crate::provision::agy::AgySettings>, ConnectError>>
            + Send
            + 'a,
    >,
>;

pub type AgyWriteFuture<'a> = Pin<
    Box<
        dyn Future<Output = Result<Option<crate::provision::agy::AgyWrite>, ConnectError>>
            + Send
            + 'a,
    >,
>;

pub type UsageFuture<'a> = Pin<
    Box<dyn Future<Output = Result<Option<crate::usage::TaskUsage>, ConnectError>> + Send + 'a>,
>;

pub type InboxFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<crate::inbox::Outcome>, ConnectError>> + Send + 'a>>;

pub type RoomFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<crate::machine::Room>, ConnectError>> + Send + 'a>>;
pub type MachineDenyKeysFuture<'a> = Pin<
    Box<
        dyn Future<Output = Result<crate::config::opencode::MachineDenyKeys, ConnectError>>
            + Send
            + 'a,
    >,
>;

pub type FindFuture<'a> = Pin<
    Box<dyn Future<Output = Result<Option<Vec<crate::reap::Candidate>>, ConnectError>> + Send + 'a>,
>;

pub type ReapFuture<'a> = Pin<
    Box<dyn Future<Output = Result<Option<Vec<crate::reap::Killed>>, ConnectError>> + Send + 'a>,
>;

pub type ConfigCheckFuture<'a> = Pin<
    Box<
        dyn Future<
                Output = Result<Option<Vec<crate::config::opencode::ConfigCheckHit>>, ConnectError>,
            > + Send
            + 'a,
    >,
>;

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
    /// Whether the git checkout at `path` has uncommitted or untracked files
    /// (`git status --porcelain`; ignored files, a build's `target/`
    /// among them, do not count): what `git worktree remove` refuses on,
    /// asked first so a listing can say so without removing anything.
    /// `None` when it cannot be known.
    fn checkout_dirty(&self, _path: &str) -> DirFuture<'_> {
        Box::pin(async { Ok(None) })
    }
    /// Fetch the remote the repo's current branch tracks (else `origin`)
    /// before a worktree is made off it, so the base is not days old.
    /// Nothing is merged or moved: only the remote-tracking refs change.
    /// `None` when it cannot be known (a machine that takes no shell
    /// command).
    fn fetch_base(&self, _repo: &str) -> FetchFuture<'_> {
        Box::pin(async { Ok(None) })
    }
    /// Put back a removed git worktree: `git worktree add <path> <branch>`
    /// in the checkout at `repo`, so `task attach` can resume a session
    /// Claude stored under that path. `Some(false)` when the branch is gone,
    /// `None` when it cannot be known or git failed.
    fn restore_worktree(&self, _repo: &str, _path: &str, _branch: &str) -> DirFuture<'_> {
        Box::pin(async { Ok(None) })
    }
    /// Remove the git worktree at `path` with `git worktree remove`, never
    /// `--force`, for a checkout whose herdr workspace is gone. Its branch
    /// stays. `Some(true)` when it is gone (or was already), `Some(false)`
    /// when git refused (uncommitted or untracked files, or it is not a
    /// linked worktree), `None` when it cannot be known.
    fn remove_checkout(&self, _path: &str) -> DirFuture<'_> {
        Box::pin(async { Ok(None) })
    }
    /// List what runs under a pane's shell and, for a checkout going away,
    /// in it (`reap`), without signalling anything. `None` when the machine
    /// takes no shell command.
    fn find_processes(&self, _spec: &crate::reap::Spec) -> FindFuture<'_> {
        Box::pin(async { Ok(None) })
    }
    /// Signal those of `candidates` that still run, as the same processes
    /// (`reap::end_command`). The ones that were running; `None` when the
    /// machine takes no shell command.
    fn end_processes(&self, _candidates: &[crate::reap::Candidate]) -> ReapFuture<'_> {
        Box::pin(async { Ok(None) })
    }
    /// Every hit `config::opencode::CONFIG_CHECK_COMMAND` finds in the
    /// machine's managed opencode config, or on macOS its managed
    /// preferences: those are merged after `OPENCODE_CONFIG_CONTENT`, so
    /// pastor cannot neutralise them (`config::opencode`). `Some(vec![])`
    /// when it is clean; `None` when it cannot be known.
    fn opencode_permission_rules(&self) -> ConfigCheckFuture<'_> {
        Box::pin(async { Ok(None) })
    }
    /// agy's global settings file on the machine
    /// (`provision::agy::SETTINGS_FILE`), for the agy provisioner. `None`
    /// when it cannot be known.
    fn agy_settings(&self) -> AgySettingsFuture<'_> {
        Box::pin(async { Ok(None) })
    }
    /// Put `new` in place of agy's global settings file on the machine when
    /// it still holds `expected` (`None`: there is still no file), atomically
    /// (`provision::agy::write_command`). `None` when it cannot be done or
    /// known.
    fn write_agy_settings(&self, _expected: Option<&str>, _new: &str) -> AgyWriteFuture<'_> {
        Box::pin(async { Ok(None) })
    }
    /// The file at `path` on the machine (`~/` is its home), as
    /// `agy_settings` reads agy's: its content, and whether it is a symlink.
    /// For the stores of the harnesses' trust (`trust_write`). `None` when it
    /// cannot be known (a `command` machine).
    fn home_file(&self, _path: &str) -> AgySettingsFuture<'_> {
        Box::pin(async { Ok(None) })
    }
    /// `write_agy_settings` for the file at `path`; with `lock`, holding the
    /// lock a program that saves it takes (`<path>.lock`, a directory).
    fn write_home_file(
        &self,
        _path: &str,
        _expected: Option<&str>,
        _new: &str,
        _lock: bool,
    ) -> AgyWriteFuture<'_> {
        Box::pin(async { Ok(None) })
    }
    /// What agy's settings on the machine say, as `config::agy::RULES_COMMAND`
    /// prints it, for `config::agy::parse_denies`. `None` when the machine
    /// takes no shell command.
    fn agy_rules(&self) -> VersionFuture<'_> {
        Box::pin(async { Ok(None) })
    }
    /// The machine's own global opencode config and `~/.opencode`'s
    /// permission keys, which a profiled opencode task must always deny
    /// (`config::opencode::MachineDenyKeys`). `None` when it cannot be
    /// known (a `command` machine).
    fn opencode_config_permission_keys(&self) -> MachineDenyKeysFuture<'_> {
        Box::pin(async { Ok(crate::config::opencode::MachineDenyKeys::None) })
    }
    /// The version of pastor installed on the machine, for `machine list`: a
    /// fleet runs whatever each machine last installed, and a skill or CLI
    /// that an agent there calls is that version's. `None` when there is no
    /// pastor there or it cannot be known.
    fn pastor_version(&self) -> VersionFuture<'_> {
        Box::pin(async { Ok(None) })
    }
    /// The version of the CLI `harness` describes installed on the machine,
    /// for the pins a flock sets, read as its manifest says. `None` when it
    /// is not there, the manifest has no version command, or it cannot be
    /// known.
    fn harness_version(&self, _harness: &crate::harness_manifest::Manifest) -> VersionFuture<'_> {
        Box::pin(async { Ok(None) })
    }
    /// What the machine has free: disk under `$HOME` and `/tmp` and memory
    /// (`machine::Room`), read before placing a task there. `None` when it
    /// cannot be known. The numbers only; the caller stamps `asked_at`.
    fn room(&self) -> RoomFuture<'_> {
        Box::pin(async { Ok(None) })
    }
    /// The tokens Claude session `session` used, read from its files on the
    /// machine under `config_dir` (the agent's `CLAUDE_CONFIG_DIR` as
    /// written, `~/.claude` when `None`) by `usage::usage_command`. `None`
    /// when there is no such session there or it cannot be known.
    fn claude_usage(&self, _config_dir: Option<&str>, _session: &str) -> UsageFuture<'_> {
        Box::pin(async { Ok(None) })
    }
    /// Hand `text` to Claude session `session` on the machine through its
    /// inbox socket (`inbox::send`), for an agent whose `CLAUDE_CONFIG_DIR`
    /// is `config_dir` as written (`~/.claude` when `None`), launched in
    /// permission class `mode`. `None` when the machine cannot send it that
    /// way (a `command` machine, no pastor there that has `inbox send`, a
    /// text too long for a remote command): the caller types it instead.
    fn inbox_send(
        &self,
        _config_dir: Option<&str>,
        _session: &str,
        _mode: crate::inbox::Mode,
        _text: &str,
    ) -> InboxFuture<'_> {
        Box::pin(async { Ok(None) })
    }
    /// Whether `command`, run in the machine's shell, exits 0: an agent's
    /// login check (`Agents::login_check_command`). `None` when it cannot
    /// be known.
    fn command_succeeds(&self, _command: &str) -> DirFuture<'_> {
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
    fn checkout_dirty(&self, path: &str) -> DirFuture<'_> {
        let command = remote_dirty_command(path);
        Box::pin(async move {
            match probe(self, command).await? {
                Some(p) => remote_dirty_answer(&p.target, &p.out),
                None => Ok(None),
            }
        })
    }
    fn fetch_base(&self, repo: &str) -> FetchFuture<'_> {
        let command = remote_fetch_command(repo);
        Box::pin(async move {
            Ok(match probe(self, command).await? {
                Some(p) => Some(remote_fetch_answer(&p.target, &p.out)?),
                None => None,
            })
        })
    }
    fn restore_worktree(&self, repo: &str, path: &str, branch: &str) -> DirFuture<'_> {
        let command = remote_restore_command(repo, path, branch);
        Box::pin(async move { restore_worktree(self, command).await })
    }
    fn remove_checkout(&self, path: &str) -> DirFuture<'_> {
        let command = remote_remove_checkout_command(path);
        Box::pin(async move { remove_checkout(self, command).await })
    }
    fn find_processes(&self, spec: &crate::reap::Spec) -> FindFuture<'_> {
        let command = crate::reap::find_command(spec);
        Box::pin(async move {
            Ok(probe(self, command)
                .await?
                .map(|p| crate::reap::parse_candidates(&String::from_utf8_lossy(&p.out.stdout))))
        })
    }
    fn end_processes(&self, candidates: &[crate::reap::Candidate]) -> ReapFuture<'_> {
        let command = crate::reap::end_command(candidates, crate::reap::GRACE);
        Box::pin(async move {
            Ok(probe(self, command)
                .await?
                .map(|p| crate::reap::parse(&String::from_utf8_lossy(&p.out.stdout))))
        })
    }
    fn opencode_permission_rules(&self) -> ConfigCheckFuture<'_> {
        Box::pin(opencode_permission_rules(self))
    }
    fn agy_settings(&self) -> AgySettingsFuture<'_> {
        Box::pin(agy_settings(self))
    }
    fn home_file(&self, path: &str) -> AgySettingsFuture<'_> {
        let command = crate::provision::agy::read_command_of(path);
        Box::pin(async move { read_settings_file(self, command).await })
    }
    fn write_home_file(
        &self,
        path: &str,
        expected: Option<&str>,
        new: &str,
        lock: bool,
    ) -> AgyWriteFuture<'_> {
        let command = crate::provision::agy::write_command_of(path, expected, new, lock);
        let path = path.to_string();
        Box::pin(async move { write_file(self, &path, command).await })
    }
    fn write_agy_settings(&self, expected: Option<&str>, new: &str) -> AgyWriteFuture<'_> {
        let command = crate::provision::agy::write_command(expected, new);
        Box::pin(
            async move { write_file(self, crate::provision::agy::SETTINGS_FILE, command).await },
        )
    }
    fn agy_rules(&self) -> VersionFuture<'_> {
        Box::pin(agy_rules(self))
    }
    fn opencode_config_permission_keys(&self) -> MachineDenyKeysFuture<'_> {
        Box::pin(opencode_config_permission_keys(self))
    }
    fn pastor_version(&self) -> VersionFuture<'_> {
        Box::pin(pastor_version(self))
    }
    fn harness_version(&self, harness: &crate::harness_manifest::Manifest) -> VersionFuture<'_> {
        let harness = harness.clone();
        Box::pin(async move { harness_version(self, &harness).await })
    }
    fn room(&self) -> RoomFuture<'_> {
        Box::pin(room(self))
    }
    fn claude_usage(&self, config_dir: Option<&str>, session: &str) -> UsageFuture<'_> {
        let command = crate::usage::usage_command(config_dir, session);
        Box::pin(async move { claude_usage(self, command).await })
    }
    fn inbox_send(
        &self,
        config_dir: Option<&str>,
        session: &str,
        mode: crate::inbox::Mode,
        text: &str,
    ) -> InboxFuture<'_> {
        let (config_dir, session, text) = (
            config_dir.map(str::to_string),
            session.to_string(),
            text.to_string(),
        );
        Box::pin(
            async move { inbox_send(self, config_dir.as_deref(), &session, mode, &text).await },
        )
    }

    fn command_succeeds(&self, command: &str) -> DirFuture<'_> {
        let command = remote_succeeds_command(command);
        Box::pin(async move {
            match probe(self, command).await? {
                // The same one-word answer as the repo check.
                Some(p) => remote_dir_answer(&p.target, &p.out),
                None => Ok(None),
            }
        })
    }
}

/// A local machine sends in this process, as the head and the session share
/// the machine; an ssh machine runs its own `pastor inbox send`.
async fn inbox_send(
    ep: &Endpoint,
    config_dir: Option<&str>,
    session: &str,
    mode: crate::inbox::Mode,
    text: &str,
) -> Result<Option<crate::inbox::Outcome>, ConnectError> {
    match ep {
        Endpoint::Local { .. } => {
            let Some(home) = dirs::home_dir() else {
                return Ok(None);
            };
            let dir = crate::inbox::config_dir_at(config_dir, &home);
            let out = crate::inbox::send(&dir, session, mode, text, crate::inbox::WAIT).await;
            Ok(Some(out))
        }
        Endpoint::Command { .. } => Ok(None),
        Endpoint::Ssh { target, .. } => {
            let command = crate::inbox::send_command(config_dir, session, mode);
            let Some(argv) = shell_argv(ep, command)? else {
                return Ok(None);
            };
            let out = probe_output_feeding(&argv, text.as_bytes(), PROBE_TIMEOUT).await?;
            reached(target, &out)?;
            let result = crate::inbox::parse_result(&String::from_utf8_lossy(&out.stdout));
            if result.is_none() {
                tracing::warn!(%target, status = %out.status, stderr = %String::from_utf8_lossy(&out.stderr).trim(), "no answer from pastor inbox send; typing the prompt");
            }
            Ok(result)
        }
    }
}

/// `command` in the machine's shell, answered like `remote_dir_command`:
/// `yes` when it exits 0, `no` otherwise. Its own output is dropped, so
/// stdout holds only the answer (and rc noise).
fn remote_succeeds_command(command: &str) -> String {
    format!("if {{ {command}\n}} >/dev/null 2>&1; then printf yes; else printf no; fi")
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

async fn remove_checkout(ep: &Endpoint, command: String) -> Result<Option<bool>, ConnectError> {
    match probe(ep, command).await? {
        Some(p) => remote_remove_checkout_answer(&p.target, &p.out),
        None => Ok(None),
    }
}

/// Removes the worktree at `path` through its repository (found from the
/// checkout itself, so a bare one works too), answering `removed`,
/// `refused` when git will not, or `gone` when nothing is at `path`.
fn remote_remove_checkout_command(path: &str) -> String {
    let path = shell_quote(path);
    format!(
        "if [ ! -e {path} ]; then echo gone; \
         else common=$(git -C {path} rev-parse --path-format=absolute --git-common-dir) && \
         if git --git-dir=\"$common\" worktree remove {path} >&2; \
         then echo removed; else echo refused; fi; fi"
    )
}

/// Reads the answer to `remote_remove_checkout_command`, the last line, as
/// `remote_restore_answer` does.
fn remote_remove_checkout_answer(
    target: &str,
    out: &std::process::Output,
) -> Result<Option<bool>, ConnectError> {
    reached(target, out)?;
    let raw = String::from_utf8_lossy(&out.stdout);
    match raw.trim_end().lines().last().unwrap_or("").trim() {
        "removed" | "gone" if out.status.success() => Ok(Some(true)),
        "refused" if out.status.success() => Ok(Some(false)),
        _ => {
            tracing::warn!(%target, status = %out.status, stdout = ?raw, stderr = %String::from_utf8_lossy(&out.stderr).trim(), "git could not tell whether it removed the worktree");
            Ok(None)
        }
    }
}

/// Fetches the remote `repo`'s current branch tracks, else `origin`, with
/// git told never to ask a person for anything. Answers `fetched <remote>`,
/// `none` when the repo has no remote, or `failed <remote>` after git's own
/// words. Nothing is merged or checked out.
fn remote_fetch_command(repo: &str) -> String {
    let repo = shell_quote(repo);
    format!(
        "cd {repo} || exit 3; \
         b=$(git symbolic-ref -q --short HEAD 2>/dev/null); \
         r=; [ -n \"$b\" ] && r=$(git config --get \"branch.$b.remote\" 2>/dev/null); \
         case \"$r\" in ''|.) r=; git remote 2>/dev/null | grep -qx origin && r=origin ;; esac; \
         [ -n \"$r\" ] || {{ echo none; exit 0; }}; \
         if out=$(GIT_TERMINAL_PROMPT=0 git fetch --quiet \"$r\" 2>&1); \
         then echo \"fetched $r\"; \
         else printf '%s\\n' \"$out\" | tail -n 3; echo \"failed $r\"; fi"
    )
}

/// Reads the answer to `remote_fetch_command`: its last line, and for a
/// failure the lines git said before it. Only ssh failing to reach the
/// machine is an error.
fn remote_fetch_answer(target: &str, out: &std::process::Output) -> Result<Fetched, ConnectError> {
    reached(target, out)?;
    let raw = String::from_utf8_lossy(&out.stdout);
    let mut lines: Vec<&str> = raw.trim_end().lines().collect();
    let last = lines.pop().unwrap_or("").trim();
    if out.status.success() {
        if last == "none" {
            return Ok(Fetched::NoRemote);
        }
        if let Some(remote) = last.strip_prefix("fetched ") {
            return Ok(Fetched::Done(remote.to_string()));
        }
        if let Some(remote) = last.strip_prefix("failed ") {
            let said: Vec<&str> = lines
                .iter()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty())
                .collect();
            return Ok(Fetched::Failed(remote.to_string(), said.join(" / ")));
        }
    }
    Ok(Fetched::Failed(
        "its remote".into(),
        format!(
            "git gave no answer ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    ))
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

/// Whether the checkout at `path` has uncommitted or untracked files, as
/// `git status --porcelain` lists them: `dirty`, `clean`, `gone` when
/// nothing is at `path`, and nothing when git could not say.
fn remote_dirty_command(path: &str) -> String {
    let path = shell_quote(path);
    format!(
        "if [ ! -e {path} ]; then echo gone; \
         elif out=$(git -C {path} status --porcelain 2>/dev/null); \
         then if [ -n \"$out\" ]; then echo dirty; else echo clean; fi; fi"
    )
}

/// Reads the answer to `remote_dirty_command`, its last line.
fn remote_dirty_answer(
    target: &str,
    out: &std::process::Output,
) -> Result<Option<bool>, ConnectError> {
    reached(target, out)?;
    let raw = String::from_utf8_lossy(&out.stdout);
    match raw.trim_end().lines().last().unwrap_or("").trim() {
        "dirty" if out.status.success() => Ok(Some(true)),
        "clean" | "gone" if out.status.success() => Ok(Some(false)),
        _ => {
            tracing::warn!(%target, status = %out.status, stdout = ?raw, "git could not tell whether the checkout is dirty");
            Ok(None)
        }
    }
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

async fn opencode_permission_rules(
    ep: &Endpoint,
) -> Result<Option<Vec<crate::config::opencode::ConfigCheckHit>>, ConnectError> {
    let command = crate::config::opencode::CONFIG_CHECK_COMMAND.to_string();
    match probe(ep, command).await? {
        Some(p) => config_check_answer(&p.target, &p.out),
        None => Ok(None),
    }
}

/// Reads the answer to `config::opencode::CONFIG_CHECK_COMMAND`: a line per
/// hit, or none at all when the managed config and preferences are clean.
/// The command always exits 0, hits or not, so only ssh failing to reach the
/// machine is an error; a shell that still failed outright is unknown.
fn config_check_answer(
    target: &str,
    out: &std::process::Output,
) -> Result<Option<Vec<crate::config::opencode::ConfigCheckHit>>, ConnectError> {
    reached(target, out)?;
    if !out.status.success() {
        tracing::warn!(%target, status = %out.status, stderr = %String::from_utf8_lossy(&out.stderr).trim(), "the opencode config check did not exit cleanly");
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(Some(crate::config::opencode::parse_config_check(&text)))
}

/// Reads agy's global settings through `probe`, on a local machine too.
/// Only ssh failing to reach the machine is an error; an answer pastor
/// cannot read is unknown.
async fn agy_settings(
    ep: &Endpoint,
) -> Result<Option<crate::provision::agy::AgySettings>, ConnectError> {
    read_settings_file(ep, crate::provision::agy::read_command()).await
}

/// Runs `command` (`provision::agy::read_command_of`) and reads the file it
/// prints.
async fn read_settings_file(
    ep: &Endpoint,
    command: String,
) -> Result<Option<crate::provision::agy::AgySettings>, ConnectError> {
    let Some(p) = probe_with(ep, command, &[], FILE_OUTPUT_LIMIT).await? else {
        return Ok(None);
    };
    reached(&p.target, &p.out)?;
    let raw = String::from_utf8_lossy(&p.out.stdout);
    match crate::provision::agy::parse_read(&raw) {
        Ok(read) if p.out.status.success() => Ok(Some(read)),
        Ok(_) | Err(_) => {
            tracing::warn!(target = %p.target, status = %p.out.status, stderr = %String::from_utf8_lossy(&p.out.stderr).trim(), "could not read the file");
            Ok(None)
        }
    }
}

/// Runs a write command with its content on stdin, and reads its answer.
async fn write_file(
    ep: &Endpoint,
    path: &str,
    write: crate::provision::agy::WriteCommand,
) -> Result<Option<crate::provision::agy::AgyWrite>, ConnectError> {
    let Some(p) = probe_with(
        ep,
        write.command,
        write.input.as_bytes(),
        PROBE_OUTPUT_LIMIT,
    )
    .await?
    else {
        return Ok(None);
    };
    reached(&p.target, &p.out)?;
    let answer = crate::provision::agy::parse_write(&String::from_utf8_lossy(&p.out.stdout));
    if answer.is_none() || !p.out.status.success() {
        tracing::warn!(target = %p.target, %path, status = %p.out.status, stderr = %String::from_utf8_lossy(&p.out.stderr).trim(), "could not write the file");
        return Ok(None);
    }
    Ok(answer)
}

async fn agy_rules(ep: &Endpoint) -> Result<Option<String>, ConnectError> {
    let command = crate::config::agy::RULES_COMMAND.to_string();
    match probe(ep, command).await? {
        Some(p) => {
            reached(&p.target, &p.out)?;
            Ok(Some(String::from_utf8_lossy(&p.out.stdout).into_owned()))
        }
        None => Ok(None),
    }
}

async fn opencode_config_permission_keys(
    ep: &Endpoint,
) -> Result<crate::config::opencode::MachineDenyKeys, ConnectError> {
    let command = crate::config::opencode::CONFIG_READ_COMMAND.to_string();
    match probe(ep, command).await? {
        Some(p) => {
            reached(&p.target, &p.out)?;
            let raw = String::from_utf8_lossy(&p.out.stdout);
            Ok(crate::config::opencode::parse_permission_keys(&raw))
        }
        None => Ok(crate::config::opencode::MachineDenyKeys::None),
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

/// Runs the manifest's version command on the machine, a local one under
/// `sh -c`, and reads the version it prints. A CLI that is not there is an
/// answer (`None`), as with `pastor_version`; only failing to reach the
/// machine is an error.
async fn harness_version(
    ep: &Endpoint,
    harness: &crate::harness_manifest::Manifest,
) -> Result<Option<String>, ConnectError> {
    let Some(command) = harness.version_command() else {
        return Ok(None);
    };
    match probe(ep, command).await? {
        Some(p) => {
            reached(&p.target, &p.out)?;
            Ok(harness.parse_version(&String::from_utf8_lossy(&p.out.stdout)))
        }
        None => Ok(None),
    }
}

async fn claude_usage(
    ep: &Endpoint,
    command: String,
) -> Result<Option<crate::usage::TaskUsage>, ConnectError> {
    // A local machine runs the same command here, under `sh -c`.
    let Some(p) = probe(ep, command).await? else {
        return Ok(None);
    };
    reached(&p.target, &p.out)?;
    let raw = String::from_utf8_lossy(&p.out.stdout);
    let usage = crate::usage::parse_usage(&raw, chrono::Utc::now());
    if usage.is_none() && !(p.out.status.success() && raw.trim_end().ends_with("none")) {
        tracing::warn!(target = %p.target, status = %p.out.status, stdout = ?raw, stderr = %String::from_utf8_lossy(&p.out.stderr).trim(), "no usage from the session files");
    }
    Ok(usage)
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

async fn room(ep: &Endpoint) -> Result<Option<crate::machine::Room>, ConnectError> {
    // A local machine runs the same command here, under `sh -c`: the head
    // and this herdr share a disk and a memory.
    match probe(ep, ROOM_COMMAND.to_string()).await? {
        Some(p) => remote_room(&p.target, &p.out),
        None => Ok(None),
    }
}

/// Asks the machine what it has free: `df -Pk` under `$HOME` and `/tmp`
/// (its 1024-byte blocks available, field 4 on GNU, BSD and macOS alike)
/// and `MemAvailable` from `/proc/meminfo`. Each number is its own `df` or
/// `awk`, printed with a label, so a home `df` cannot read, or a macOS with
/// no `/proc/meminfo`, leaves only that number out instead of shifting the
/// others; the command's exit status says nothing, since the last `awk`
/// fails on macOS.
pub const ROOM_COMMAND: &str = r#"df -Pk "$HOME" | awk 'NR==2 {print "home", $4}'; df -Pk /tmp | awk 'NR==2 {print "tmp", $4}'; awk '/^MemAvailable:/ {print "mem", $2}' /proc/meminfo 2>/dev/null"#;

/// Reads the answer to `ROOM_COMMAND`: `home N`, `tmp N` and `mem N` lines,
/// in any order, among whatever rc files print. A label without a whole
/// number after it is read as missing. As with `remote_home`, only ssh
/// failing to reach the machine is an error.
fn remote_room(
    target: &str,
    out: &std::process::Output,
) -> Result<Option<crate::machine::Room>, ConnectError> {
    reached(target, out)?;
    let raw = String::from_utf8_lossy(&out.stdout);
    let room = parse_room(&raw);
    if room.home_free.is_none() || room.tmp_free.is_none() {
        tracing::warn!(%target, status = %out.status, stdout = ?raw, stderr = %String::from_utf8_lossy(&out.stderr).trim(), "no free disk from the remote shell");
    }
    Ok(Some(room))
}

/// The numbers in `ROOM_COMMAND`'s output; the last line of each label
/// wins, so a label an rc file happens to print is overridden by the probe's
/// own, which comes after.
fn parse_room(raw: &str) -> crate::machine::Room {
    let mut room = crate::machine::Room::default();
    for line in raw.lines() {
        let mut words = line.split_whitespace();
        let (Some(label), Some(number), None) = (words.next(), words.next(), words.next()) else {
            continue;
        };
        let value = number.parse::<u64>().ok();
        match label {
            "home" => room.home_free = value,
            "tmp" => room.tmp_free = value,
            "mem" => room.mem_free = value,
            _ => {}
        }
    }
    room
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
            reserved: 0,
            provisioner: None,
            bundles: Vec::new(),
            bundle_apply: None,
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
            loads: vec![],
            flock: None,
            agent: None,
            agent_args: None,
            model: None,
            fallback: None,
            priority: None,
            agents: Default::default(),
            profile: None,
            min_disk: None,
            min_memory: None,
            unknown_room: None,
            pins: Default::default(),
            env: Default::default(),
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

    /// `describe` names the target and session, unlike `host`'s short form.
    #[test]
    fn describe_names_the_target_and_session() {
        let paths = Paths::new("/c", "/s");
        assert_eq!(
            Endpoint::from_machine(&ssh_machine("pi-3"), &paths).describe(),
            "ssh fleet@host (session default)"
        );
        let local = MachineConfig {
            local: true,
            ssh: None,
            ..ssh_machine("here")
        };
        assert_eq!(
            Endpoint::from_machine(&local, &paths).describe(),
            "local herdr session default"
        );
        let command = Endpoint::Command {
            argv: vec!["fake-herdr".into(), "--connect".into(), "/h.sock".into()],
        };
        assert_eq!(command.describe(), "command fake-herdr --connect /h.sock");
    }

    /// `Endpoint`'s `Connector` impl (used through `&dyn Connector`, unlike
    /// the tests above that call the inherent methods directly) matches the
    /// inherent methods it delegates to.
    #[test]
    fn the_connector_impl_matches_endpoint_directly() {
        let paths = Paths::new("/c", "/s");
        let ep = Endpoint::from_machine(&ssh_machine("pi-3"), &paths);
        let c: &dyn Connector = &ep;
        assert_eq!(c.describe(), ep.describe());
        assert_eq!(c.host(), ep.host());
    }

    /// A `Connector` that does not override `host` falls back to `describe`.
    struct DescribeOnly;
    impl Connector for DescribeOnly {
        fn connect(&self) -> ConnectFuture<'_> {
            Box::pin(async {
                Err(ConnectError {
                    message: "no".into(),
                })
            })
        }
        fn describe(&self) -> String {
            "described".into()
        }
    }

    #[test]
    fn a_connector_with_no_host_of_its_own_falls_back_to_describe() {
        assert_eq!(DescribeOnly.host(), "described");
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
            reserved: 0,
            provisioner: None,
            bundles: Vec::new(),
            bundle_apply: None,
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
            loads: vec![],
            flock: None,
            agent: None,
            agent_args: None,
            model: None,
            fallback: None,
            priority: None,
            agents: Default::default(),
            profile: None,
            min_disk: None,
            min_memory: None,
            unknown_room: None,
            pins: Default::default(),
            env: Default::default(),
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
            &posix_command(&format!(
                "{MISE_SHIMS}herdr --session default remote-api-bridge"
            ))
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
        assert_eq!(
            argv.last().unwrap(),
            &posix_command(&format!("{MISE_SHIMS}printf %s \"$HOME\""))
        );
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
            argv.last().unwrap().contains("PATH=\"$HOME/.cargo/bin:"),
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
        // "none" wins even when it happens to follow "pastor": read as the
        // no-pastor answer, never as a version literal spelled "none".
        assert_eq!(v(0, "pastor none"), None);
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

    /// A local machine runs the room probe here; a bridge command cannot
    /// know. The ssh command runs through `sh -c`, as the version one does.
    #[tokio::test]
    async fn room_per_endpoint() {
        let argv = ssh_argv_running(
            "fleet@pi-3",
            Some(Path::new("/tmp/s/ssh/pi-3-%C")),
            ROOM_COMMAND.to_string(),
        );
        assert!(
            argv.last().unwrap().contains("df -Pk \"$HOME\""),
            "{argv:?}"
        );
        let local = Endpoint::Local {
            session: "default".into(),
        };
        let room = local.room().await.unwrap().expect("a local room");
        assert!(room.home_free.is_some(), "{room:?}");
        assert!(room.tmp_free.is_some(), "{room:?}");
        if cfg!(target_os = "linux") {
            assert!(room.mem_free.is_some(), "{room:?}");
        }
        assert_eq!(room.asked_at, None, "the actor stamps the read");
        assert_eq!(room.error, None);
        let command = Endpoint::Command {
            argv: vec!["true".into()],
        };
        assert_eq!(command.room().await.unwrap(), None);
    }

    /// The command's own shell script, run here, answers the way the parser
    /// expects: a labelled number per line, memory left out where there is
    /// no `/proc/meminfo` (macOS) rather than failing the probe.
    #[test]
    fn the_room_command_answers_labelled_numbers() {
        let out = std::process::Command::new("sh")
            .args(["-c", ROOM_COMMAND])
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        let room = remote_room("t", &out).unwrap().expect("a room");
        assert!(room.home_free.is_some(), "{room:?}");
        assert!(room.tmp_free.is_some(), "{room:?}");
        let tmp = tempfile::tempdir().unwrap();
        let out = std::process::Command::new("sh")
            .args(["-c", ROOM_COMMAND])
            .env("HOME", tmp.path().join("missing"))
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        let room = remote_room("t", &out).unwrap().expect("a room");
        assert_eq!(room.home_free, None, "a home df cannot read is unknown");
        assert!(room.tmp_free.is_some(), "{room:?}");
    }

    /// Each number is its own labelled line, so one df failing leaves only
    /// its number unknown; rc-file noise and an odd number are ignored. Only
    /// ssh failing to reach the machine is an error.
    #[test]
    fn remote_room_reads_labelled_lines() {
        use std::os::unix::process::ExitStatusExt;
        let out = |code: i32, stdout: &str| std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: b"df: /tmp: No such file".to_vec(),
        };
        let r = |code: i32, stdout: &str| remote_room("t", &out(code, stdout)).unwrap().unwrap();
        let full = r(0, "home 123456\ntmp 7890\nmem 4096\n");
        assert_eq!(full.home_free, Some(123_456));
        assert_eq!(full.tmp_free, Some(7_890));
        assert_eq!(full.mem_free, Some(4_096));
        let noisy = r(2, "Welcome to Raspberry Pi\nhome 10\nmem 20\n");
        assert_eq!(noisy.home_free, Some(10));
        assert_eq!(noisy.tmp_free, None, "a missing line is unknown");
        assert_eq!(
            noisy.mem_free,
            Some(20),
            "a failed awk still counts the lines that came"
        );
        let odd = r(0, "home ten\ntmp 5 extra\nmem -1\nhome 3\n");
        assert_eq!(odd.home_free, Some(3), "the last good line wins");
        assert_eq!(odd.tmp_free, None);
        assert_eq!(odd.mem_free, None);
        let none = r(0, "");
        assert_eq!(
            (none.home_free, none.tmp_free, none.mem_free),
            (None, None, None)
        );
        assert!(remote_room("t", &out(255, "")).is_err());
        let killed = std::process::Output {
            status: std::process::ExitStatus::from_raw(9),
            stdout: vec![],
            stderr: vec![],
        };
        assert!(remote_room("t", &killed).is_err());
    }

    /// What pastor runs on an ssh machine sees the directory of mise's shims
    /// first on its PATH when the machine has one (ssh's shell reads none of
    /// the files that would put it there), and the PATH as it was when it
    /// has not; `MISE_DATA_DIR` moves the directory. A local machine runs its
    /// command as it is, and no shell file of the machine is touched.
    #[test]
    fn remote_commands_see_the_mise_shims_when_the_machine_has_them() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let run = |remote: &str, env: &[(&str, &std::path::Path)]| -> String {
            let argv = ssh_argv_running("fleet@pi-3", None, remote.to_string());
            let mut cmd = std::process::Command::new("sh");
            cmd.args(["-c", argv.last().unwrap()])
                .env_clear()
                .env("HOME", home.path())
                .env("PATH", "/usr/bin:/bin");
            for (k, v) in env {
                cmd.env(k, v);
            }
            let out = cmd.output().unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let shim = |dir: &std::path::Path| {
            std::fs::create_dir_all(dir).unwrap();
            let f = dir.join("claude");
            std::fs::write(&f, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
            f
        };
        // No shims directory: the PATH is what ssh gave, and claude is not on it.
        assert_eq!(run("echo \"$PATH\"", &[]), "/usr/bin:/bin");
        assert_eq!(run("command -v claude", &[]), "");
        // The default directory, first on the PATH.
        let found = shim(&home.path().join(".local/share/mise/shims"));
        let path = run("echo \"$PATH\"", &[]);
        assert_eq!(
            path,
            format!("{}:/usr/bin:/bin", found.parent().unwrap().display())
        );
        assert_eq!(run("command -v claude", &[]), found.display().to_string());
        // MISE_DATA_DIR moves it.
        let elsewhere = home.path().join("data");
        let moved = shim(&elsewhere.join("shims"));
        assert_eq!(
            run(
                "command -v claude",
                &[("MISE_DATA_DIR", elsewhere.as_path())]
            ),
            moved.display().to_string()
        );
        // Every way pastor reaches an ssh machine has them: the bridge and
        // the probes; a local machine's command is its own.
        let ep = Endpoint::from_machine(&ssh_machine("pi-3"), &Paths::new("/tmp/c", "/tmp/s"));
        for argv in [
            shell_argv(&ep, "true".into()).unwrap().unwrap(),
            ssh_argv("fleet@pi-3", "default", None),
        ] {
            assert!(
                argv.last().unwrap().contains(MISE_SHIMS.trim_end()),
                "{argv:?}"
            );
        }
        let local = Endpoint::Local {
            session: "default".into(),
        };
        assert_eq!(
            shell_argv(&local, "true".into()).unwrap().unwrap(),
            ["sh", "-c", "true"]
        );
        // Nothing was written into the machine's home.
        let files: Vec<_> = std::fs::read_dir(home.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(
            files.iter().all(|f| f == ".local" || f == "data"),
            "{files:?}"
        );
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
        assert!(remote.starts_with("sh -c '"), "{remote}");
        assert!(remote.contains("if test -d "), "{remote}");
        let out = std::process::Command::new("sh")
            .args(["-c", remote])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "yes");
        assert!(
            ssh_argv("fleet@pi-3", "default", None)
                .last()
                .unwrap()
                .contains("herdr --session default remote-api-bridge'")
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

    /// The command itself (every real source of a hit) is
    /// `config::opencode`'s own to test; this only proves the wiring here
    /// runs it and passes its answer through. The command reads opencode's
    /// config under `$HOME` and `$XDG_CONFIG_HOME`, and the environment is
    /// the whole test binary's, which other tests read, so the local half
    /// runs in a child of this binary with its own empty `HOME` and managed
    /// dir: a real config on the host running the suite cannot turn the
    /// first answer into a hit.
    #[tokio::test]
    async fn opencode_permission_rules_per_endpoint() {
        const CHILD: &str = "PASTOR_TEST_OPENCODE_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let managed = std::path::PathBuf::from(
                std::env::var_os("OPENCODE_TEST_MANAGED_CONFIG_DIR").unwrap(),
            );
            let local = Endpoint::Local {
                session: "s".into(),
            };
            assert_eq!(
                local.opencode_permission_rules().await.unwrap(),
                Some(vec![])
            );
            let file = managed.join("opencode.json");
            std::fs::write(&file, r#"{"permission": {"bash": "allow"}}"#).unwrap();
            assert_eq!(
                local.opencode_permission_rules().await.unwrap(),
                Some(vec![crate::config::opencode::ConfigCheckHit::Key {
                    file: file.display().to_string(),
                    key: "permission".into(),
                }])
            );
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let managed = tempfile::tempdir().unwrap();
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "herdr::transport::tests::opencode_permission_rules_per_endpoint",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .env("HOME", home.path())
            .env_remove("XDG_CONFIG_HOME")
            .env("OPENCODE_TEST_MANAGED_CONFIG_DIR", managed.path())
            .env(
                "PASTOR_TEST_MANAGED_PREFERENCES_DIR",
                managed.path().join("prefs"),
            )
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        // The filter matched and the child really ran the local half.
        assert!(stdout.contains("1 passed"), "{stdout}");
        let command = Endpoint::Command {
            argv: vec!["true".into()],
        };
        assert_eq!(command.opencode_permission_rules().await.unwrap(), None);
    }

    /// agy's settings are read and written under the machine's `HOME` on a
    /// local endpoint (run in a child process with a temp `HOME`), and are
    /// unknown on a command one.
    #[tokio::test]
    async fn agy_settings_per_endpoint() {
        use crate::provision::agy::{AgySettings, AgyWrite};
        const CHILD: &str = "PASTOR_TEST_AGY_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let local = Endpoint::Local {
                session: "s".into(),
            };
            let none = AgySettings {
                content: None,
                link: false,
            };
            assert_eq!(local.agy_settings().await.unwrap(), Some(none));
            assert_eq!(
                local.write_agy_settings(None, "{}\n").await.unwrap(),
                Some(AgyWrite::Written)
            );
            assert_eq!(
                local
                    .agy_settings()
                    .await
                    .unwrap()
                    .unwrap()
                    .content
                    .as_deref(),
                Some("{}\n")
            );
            assert_eq!(
                local.write_agy_settings(None, "{}\n").await.unwrap(),
                Some(AgyWrite::Changed)
            );
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "herdr::transport::tests::agy_settings_per_endpoint",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .env("HOME", home.path())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(stdout.contains("1 passed"), "{stdout}");
        assert!(
            home.path()
                .join(".gemini/antigravity-cli/settings.json")
                .is_file()
        );
        let command = Endpoint::Command {
            argv: vec!["true".into()],
        };
        assert_eq!(command.agy_settings().await.unwrap(), None);
        assert_eq!(command.write_agy_settings(None, "{}").await.unwrap(), None);
    }

    /// `agy_rules` prints agy's settings from the machine's `HOME`, and a
    /// command machine answers nothing. The local half runs in a child with
    /// its own `HOME`, as the opencode test does.
    #[tokio::test]
    async fn agy_rules_per_endpoint() {
        const CHILD: &str = "PASTOR_TEST_AGY_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let local = Endpoint::Local {
                session: "s".into(),
            };
            let out = local.agy_rules().await.unwrap().unwrap();
            assert_eq!(
                crate::config::agy::parse_denies(&out).unwrap(),
                vec!["command(sudo)"]
            );
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let cli = home.path().join(".gemini/antigravity-cli");
        std::fs::create_dir_all(&cli).unwrap();
        std::fs::write(
            cli.join("settings.json"),
            r#"{"permissions":{"deny":["command(sudo)"]}}"#,
        )
        .unwrap();
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "herdr::transport::tests::agy_rules_per_endpoint",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .env("HOME", home.path())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(stdout.contains("1 passed"), "{stdout}");
        let command = Endpoint::Command {
            argv: vec!["true".into()],
        };
        assert_eq!(command.agy_rules().await.unwrap(), None);
    }

    /// Same wiring, for the global config and `~/.opencode`: the command
    /// itself is `config::opencode`'s own to test. A child of this binary
    /// again, so the real `$HOME` running the suite cannot leak a key in.
    #[tokio::test]
    async fn opencode_config_permission_keys_per_endpoint() {
        const CHILD: &str = "PASTOR_TEST_OPENCODE_KEYS_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let local = Endpoint::Local {
                session: "s".into(),
            };
            assert_eq!(
                local.opencode_config_permission_keys().await.unwrap(),
                crate::config::opencode::MachineDenyKeys::None
            );
            let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
            let dir = home.join(".config/opencode");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("opencode.json"),
                r#"{"permission": {"bash": "allow"}}"#,
            )
            .unwrap();
            assert_eq!(
                local.opencode_config_permission_keys().await.unwrap(),
                crate::config::opencode::MachineDenyKeys::Keys(vec!["bash".into()])
            );
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "herdr::transport::tests::opencode_config_permission_keys_per_endpoint",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .env("HOME", home.path())
            .env_remove("XDG_CONFIG_HOME")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "{stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(stdout.contains("1 passed"), "{stdout}");
        let command = Endpoint::Command {
            argv: vec!["true".into()],
        };
        assert_eq!(
            command.opencode_config_permission_keys().await.unwrap(),
            crate::config::opencode::MachineDenyKeys::None
        );
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
        // "added" or "no-branch" only count when the shell that printed them
        // also exited zero: a nonzero exit with that word on stdout (a `set
        // -e` shell can still print before failing) is neither outcome.
        assert_eq!(
            remote_restore_answer("t", &out(1, "added\n")).unwrap(),
            None
        );
        assert_eq!(
            remote_restore_answer("t", &out(1, "no-branch\n")).unwrap(),
            None
        );
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
        // A parseable count on a nonzero exit is not a real answer either.
        assert_eq!(remote_unpushed_answer("t", &out(1, "2\n")).unwrap(), None);
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

    /// Against a real git and a local bare remote: the remote the current
    /// branch tracks is fetched (its tracking refs move, nothing is merged
    /// or checked out), else `origin`; a repo with no remote has nothing to
    /// fetch; a remote that cannot be reached, or no repo at all, is a
    /// failure that says so.
    #[tokio::test]
    async fn a_local_repo_fetches_the_remote_its_branch_tracks() {
        let dir = tempfile::tempdir().unwrap();
        let git = |cwd: &std::path::Path, args: &[&str]| -> String {
            let st = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "init.defaultBranch=main",
                    "-c",
                    "protocol.file.allow=always",
                ])
                .args(args)
                .current_dir(cwd)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(st.status.success(), "{args:?}: {st:?}");
            String::from_utf8_lossy(&st.stdout).trim().to_string()
        };
        let d = dir.path();
        git(d, &["init", "-q", "--bare", "up.git"]);
        git(d, &["init", "-q", "work dir"]);
        let work = d.join("work dir");
        git(&work, &["commit", "-q", "--allow-empty", "-m", "one"]);
        git(&work, &["remote", "add", "up", "../up.git"]);
        git(&work, &["push", "-q", "-u", "up", "HEAD:main"]);
        // Another clone pushes a commit this one has not seen.
        git(d, &["clone", "-q", "up.git", "other"]);
        git(
            &d.join("other"),
            &["commit", "-q", "--allow-empty", "-m", "two"],
        );
        git(&d.join("other"), &["push", "-q", "origin", "HEAD:main"]);
        let ahead = git(&d.join("other"), &["rev-parse", "HEAD"]);
        let mine = git(&work, &["rev-parse", "HEAD"]);
        assert_ne!(ahead, mine);

        let ep = Endpoint::Local {
            session: "s".into(),
        };
        // A path with a space reaches the shell quoted. The branch tracks
        // `up`, not `origin`, which this repo has not got.
        let got = ep.fetch_base(work.to_str().unwrap()).await.unwrap();
        assert_eq!(got, Some(Fetched::Done("up".into())));
        assert_eq!(git(&work, &["rev-parse", "up/main"]), ahead);
        assert_eq!(git(&work, &["rev-parse", "HEAD"]), mine, "nothing merged");

        // No upstream for the branch: `origin` when there is one.
        git(&work, &["branch", "--unset-upstream"]);
        git(&work, &["remote", "add", "origin", "../up.git"]);
        let got = ep.fetch_base(work.to_str().unwrap()).await.unwrap();
        assert_eq!(got, Some(Fetched::Done("origin".into())));
        assert_eq!(git(&work, &["rev-parse", "origin/main"]), ahead);

        // No remote at all: nothing to fetch, and nothing wrong.
        git(d, &["init", "-q", "alone"]);
        let alone = d.join("alone");
        assert_eq!(
            ep.fetch_base(alone.to_str().unwrap()).await.unwrap(),
            Some(Fetched::NoRemote)
        );

        // A remote that is gone: the failure says which and why.
        std::fs::remove_dir_all(d.join("up.git")).unwrap();
        let Some(Fetched::Failed(remote, why)) =
            ep.fetch_base(work.to_str().unwrap()).await.unwrap()
        else {
            panic!("a gone remote must fail");
        };
        assert_eq!(remote, "origin");
        assert!(!why.is_empty(), "git's own words");

        // Not a repo: the answer is a failure, never a panic or a hang.
        let missing = d.join("nothing here");
        assert!(matches!(
            ep.fetch_base(missing.to_str().unwrap()).await.unwrap(),
            Some(Fetched::Failed(..))
        ));
    }

    /// Everything that reaches the remote shell is quoted.
    #[test]
    fn remote_fetch_command_quotes_the_repo() {
        let c = remote_fetch_command("/r/my repo;rm");
        assert!(c.starts_with("cd '/r/my repo;rm' || exit 3"), "{c}");
        assert!(
            c.contains("GIT_TERMINAL_PROMPT=0"),
            "git never asks a person"
        );
    }

    /// Against a real git: an existing branch gets its worktree back, a
    /// gone branch is reported rather than failing.
    #[tokio::test]
    async fn a_local_checkout_restores_a_removed_worktree() {
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
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(st.status.success(), "{args:?}: {st:?}");
        };
        git(&["init", "-q", "work"]);
        git(&["-C", "work", "commit", "-q", "--allow-empty", "-m", "one"]);
        git(&["-C", "work", "branch", "feature"]);
        let ep = Endpoint::Local {
            session: "s".into(),
        };
        let repo = dir.path().join("work");
        let repo = repo.to_str().unwrap();
        let wt = dir.path().join("wt");
        assert_eq!(
            ep.restore_worktree(repo, wt.to_str().unwrap(), "feature")
                .await
                .unwrap(),
            Some(true)
        );
        assert!(wt.join(".git").exists());
        let wt2 = dir.path().join("wt2");
        assert_eq!(
            ep.restore_worktree(repo, wt2.to_str().unwrap(), "no-such-branch")
                .await
                .unwrap(),
            Some(false)
        );
        assert!(!wt2.exists());
    }

    #[test]
    fn remote_remove_checkout_answer_reads_the_outcome() {
        use std::os::unix::process::ExitStatusExt;
        let out = |code: i32, stdout: &str| std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: b"boom".to_vec(),
        };
        let answer = |code, stdout| remote_remove_checkout_answer("t", &out(code, stdout));
        assert_eq!(answer(0, "welcome\nremoved\n").unwrap(), Some(true));
        assert_eq!(answer(0, "gone\n").unwrap(), Some(true));
        assert_eq!(answer(0, "refused\n").unwrap(), Some(false));
        assert_eq!(answer(128, "").unwrap(), None);
        assert_eq!(answer(1, "removed\n").unwrap(), None);
        assert!(answer(255, "").is_err());
        let c = remote_remove_checkout_command("/w/t 1;rm");
        assert!(c.contains("worktree remove '/w/t 1;rm' >&2"), "{c}");
        assert!(!c.contains("--force"), "{c}");
    }

    /// Against a real git: a clean checkout goes and its branch stays, one
    /// with only ignored files (a build's `target/`) goes too, one with an
    /// untracked file is refused, and a path with nothing there is gone.
    #[tokio::test]
    async fn a_local_checkout_is_removed_unless_it_is_dirty() {
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
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(st.status.success(), "{args:?}: {st:?}");
            String::from_utf8_lossy(&st.stdout).into_owned()
        };
        git(&["init", "-q", "work"]);
        std::fs::write(dir.path().join("work/.gitignore"), "target/\n").unwrap();
        git(&["-C", "work", "add", ".gitignore"]);
        git(&["-C", "work", "commit", "-q", "-m", "one"]);
        let ep = Endpoint::Local {
            session: "s".into(),
        };
        for (name, extra) in [("clean", None), ("built", Some("target/x"))] {
            let wt = dir.path().join(name);
            git(&[
                "-C",
                "work",
                "worktree",
                "add",
                "-q",
                "-b",
                name,
                wt.to_str().unwrap(),
            ]);
            if let Some(file) = extra {
                std::fs::create_dir_all(wt.join(file).parent().unwrap()).unwrap();
                std::fs::write(wt.join(file), "x").unwrap();
            }
            assert_eq!(
                ep.remove_checkout(wt.to_str().unwrap()).await.unwrap(),
                Some(true),
                "{name}"
            );
            assert!(!wt.exists(), "{name}");
        }
        let branches = git(&["-C", "work", "branch", "--list"]);
        assert!(
            branches.contains("clean") && branches.contains("built"),
            "{branches}"
        );
        let wt = dir.path().join("dirty");
        git(&[
            "-C",
            "work",
            "worktree",
            "add",
            "-q",
            "-b",
            "dirty",
            wt.to_str().unwrap(),
        ]);
        std::fs::write(wt.join("notes.txt"), "x").unwrap();
        assert_eq!(
            ep.remove_checkout(wt.to_str().unwrap()).await.unwrap(),
            Some(false)
        );
        assert!(wt.join("notes.txt").exists());
        let gone = dir.path().join("never");
        assert_eq!(
            ep.remove_checkout(gone.to_str().unwrap()).await.unwrap(),
            Some(true)
        );
    }

    /// `checkout_dirty` against a real git: a clean checkout and one with
    /// only ignored files (a build's `target/`) are clean, an untracked file
    /// or a changed tracked file is dirty, a path with nothing there is not,
    /// and a directory that is no checkout cannot be told.
    #[tokio::test]
    async fn a_local_checkout_is_dirty_for_untracked_and_changed_files_only() {
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
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(st.status.success(), "{args:?}: {st:?}");
        };
        git(&["init", "-q", "work"]);
        std::fs::write(dir.path().join("work/.gitignore"), "target/\n").unwrap();
        std::fs::write(dir.path().join("work/a.txt"), "a").unwrap();
        git(&["-C", "work", "add", "."]);
        git(&["-C", "work", "commit", "-q", "-m", "one"]);
        let ep = Endpoint::Local {
            session: "s".into(),
        };
        let wt = dir.path().join("wt");
        git(&[
            "-C",
            "work",
            "worktree",
            "add",
            "-q",
            "-b",
            "wt",
            wt.to_str().unwrap(),
        ]);
        let path = wt.to_str().unwrap();
        assert_eq!(ep.checkout_dirty(path).await.unwrap(), Some(false));
        std::fs::create_dir_all(wt.join("target")).unwrap();
        std::fs::write(wt.join("target/x"), "built").unwrap();
        assert_eq!(
            ep.checkout_dirty(path).await.unwrap(),
            Some(false),
            "ignored"
        );
        std::fs::write(wt.join("a.txt"), "changed").unwrap();
        assert_eq!(
            ep.checkout_dirty(path).await.unwrap(),
            Some(true),
            "changed"
        );
        std::fs::write(wt.join("a.txt"), "a").unwrap();
        std::fs::write(wt.join("notes.txt"), "x").unwrap();
        assert_eq!(
            ep.checkout_dirty(path).await.unwrap(),
            Some(true),
            "untracked"
        );
        let nowhere = dir.path().join("never");
        assert_eq!(
            ep.checkout_dirty(nowhere.to_str().unwrap()).await.unwrap(),
            Some(false)
        );
        let plain = dir.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        assert_eq!(
            ep.checkout_dirty(plain.to_str().unwrap()).await.unwrap(),
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
        // Pinned against a literal, not the constant itself: every other
        // assertion here uses `PROBE_OUTPUT_LIMIT` symbolically, so a wrong
        // value for it would still pass them.
        assert_eq!(PROBE_OUTPUT_LIMIT, 65536);
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

    /// A probe fed a file gets all of it, more than a pipe holds, and one
    /// that exits without reading it is not waited on.
    #[tokio::test]
    async fn a_fed_probe_reads_its_input() {
        let argv = |script: &str| vec!["sh".to_string(), "-c".into(), script.into()];
        let input = vec![b'x'; 1024 * 1024];
        let out = probe_output_feeding(&argv("wc -c"), &input, PROBE_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "1048576");
        let out = probe_output_feeding(&argv("exit 4"), &input, PROBE_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(out.status.code(), Some(4));
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
            reserved: 0,
            provisioner: None,
            bundles: Vec::new(),
            bundle_apply: None,
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
            loads: vec![],
            flock: None,
            agent: None,
            agent_args: None,
            model: None,
            fallback: None,
            priority: None,
            agents: Default::default(),
            profile: None,
            min_disk: None,
            min_memory: None,
            unknown_room: None,
            pins: Default::default(),
            env: Default::default(),
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
            &posix_command(&format!(
                "{MISE_SHIMS}herdr --session default remote-api-bridge"
            ))
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
