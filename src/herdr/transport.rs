use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;

use super::Connection;
use crate::config::Paths;
use crate::config::flock::MachineConfig;

/// How long an ssh `ControlMaster` sticks around with no channels open. Every
/// request opens a connection, so the master is what makes them cheap: the same
/// value herdr's own remote transport uses (`src/remote/attach.rs`).
const CONTROL_PERSIST_SECS: u32 = 600;

/// OpenSSH creates the master socket at `<path>.XXXXXXXXXXXXXXXX` and renames it
/// into place, so the staged name is 17 bytes longer than the ControlPath.
const CONTROL_PATH_STAGING: usize = 17;

/// `sun_path` is 108 bytes on Linux, including the terminating NUL.
const UNIX_PATH_MAX: usize = 108;

/// `%C` expands to a hex SHA-1 of the connection parameters.
const EXPANDED_C: usize = 40;

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
                    "ssh ControlPath is too long for a unix socket even without the machine name; connecting without multiplexing (every request pays a full ssh handshake). Set PASTOR_STATE_DIR to something shorter."
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
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ConnectError {
    pub message: String,
}

pub fn local_socket_path(session: &str) -> anyhow::Result<PathBuf> {
    let base = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("no config dir"))?
        .join("herdr");
    Ok(if session == "default" {
        base.join("herdr.sock")
    } else {
        base.join("sessions").join(session).join("herdr.sock")
    })
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

/// The length of `path` as ssh will have expanded it: `%%` is one byte, `%C` is
/// a 40-byte hash. Other `%` tokens do not appear in paths pastor builds.
fn expanded_len(path: &Path) -> usize {
    let s = path.to_string_lossy();
    let mut len = 0usize;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            len += c.len_utf8();
            continue;
        }
        match chars.next() {
            Some('%') => len += 1,
            Some('C') => len += EXPANDED_C,
            Some(other) => len += 1 + other.len_utf8(),
            None => len += 1,
        }
    }
    len
}

/// The ControlPath for `machine`, with the name shortened until ssh's staged
/// socket name fits in `sun_path`. The name is only there to make the socket
/// recognisable in `ls`; `%C` (a hash of the destination) is what keeps
/// machines apart, so cutting the name loses nothing. `None` only when even a
/// bare `-%C` does not fit, which takes an unusually deep state dir.
fn fitting_control_path(paths: &Paths, machine: &str) -> Option<PathBuf> {
    let chars = machine.chars().count();
    (0..=chars).rev().find_map(|keep| {
        let short: String = machine.chars().take(keep).collect();
        let path = paths.ssh_control_path(&short);
        control_path_fits(&path).then_some(path)
    })
}

/// Would ssh's staged socket name fit in `sun_path`? A ControlPath that does not
/// makes every connection fail, so an endpoint whose path is too long drops the
/// multiplexing options instead.
fn control_path_fits(path: &Path) -> bool {
    // `+ 1` for the NUL would read better, but clippy prefers the strict form.
    expanded_len(path) + CONTROL_PATH_STAGING < UNIX_PATH_MAX
}

/// ssh argv for a bridge, over the shared master when there is one. Nothing here
/// goes through a shell: `target` and `control_path` are separate argv elements,
/// and only the remote command (which a remote shell does parse) is quoted, by
/// `bridge_command`.
fn ssh_argv(target: &str, session: &str, control_path: Option<&Path>) -> Vec<String> {
    ssh_argv_running(target, control_path, bridge_command(session))
}

/// ssh argv that runs `remote` (parsed by the remote shell) over the shared
/// master when there is one.
fn ssh_argv_running(target: &str, control_path: Option<&Path>, remote: String) -> Vec<String> {
    let mut argv = vec![
        "ssh".to_string(),
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        "ServerAliveInterval=15".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
    ];
    if let Some(control_path) = control_path {
        // One authenticated master per machine and destination, reused by every
        // request connection: without it each request would pay a full ssh
        // handshake.
        argv.extend([
            "-o".to_string(),
            "ControlMaster=auto".into(),
            "-o".into(),
            format!("ControlPath={}", control_path.display()),
            "-o".into(),
            format!("ControlPersist={CONTROL_PERSIST_SECS}"),
        ]);
    }
    argv.extend(["-T".to_string(), target.to_string(), remote]);
    argv
}

/// Reads the answer to `REMOTE_HOME_COMMAND`. Only ssh failing to reach the
/// machine is an error, and so a transport failure; a machine that answered
/// without a usable home is fine, its home is just unknown.
fn remote_home(target: &str, out: &std::process::Output) -> Result<Option<String>, ConnectError> {
    // 255 is ssh's own failure code; no code at all means it was killed.
    if matches!(out.status.code(), Some(255) | None) {
        return Err(ConnectError {
            message: format!(
                "ssh {target}: {} ({})",
                String::from_utf8_lossy(&out.stderr).trim(),
                out.status
            ),
        });
    }
    let home = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() || !home.starts_with('/') {
        tracing::warn!(%target, status = %out.status, stdout = %home, "no usable $HOME from the remote shell");
        return Ok(None);
    }
    Ok(Some(home))
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

/// Anything that can open a fresh herdr connection. Endpoints for real use, FakeHerdr in tests.
///
/// A connection carries one request (see `Connection`), so this is called once
/// per request; `ConnectorExt` in `client.rs` has the request vocabulary built
/// on top of it.
pub trait Connector: Send + Sync {
    fn connect(&self) -> ConnectFuture<'_>;
    fn describe(&self) -> String;
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
}

impl Connector for Endpoint {
    fn connect(&self) -> ConnectFuture<'_> {
        Box::pin(connect(self))
    }
    fn describe(&self) -> String {
        Endpoint::describe(self)
    }
    fn home_dir(&self) -> HomeFuture<'_> {
        Box::pin(home_dir(self))
    }
    fn dir_exists(&self, path: &str) -> DirFuture<'_> {
        let path = path.to_string();
        Box::pin(async move { dir_exists(self, &path).await })
    }
}

async fn home_dir(ep: &Endpoint) -> Result<Option<String>, ConnectError> {
    match ep {
        // The head and this herdr share a machine, and so a home.
        Endpoint::Local { .. } => Ok(dirs::home_dir().map(|p| p.to_string_lossy().into_owned())),
        Endpoint::Ssh {
            target,
            control_path,
            ..
        } => {
            // May be the first ssh to this machine, so it may start the master.
            ensure_control_dir(control_path.as_deref())?;
            let argv = ssh_argv_running(
                target,
                control_path.as_deref(),
                REMOTE_HOME_COMMAND.to_string(),
            );
            let out = tokio::process::Command::new(&argv[0])
                .args(&argv[1..])
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output()
                .await
                .map_err(|e| ConnectError {
                    message: format!("spawn ssh: {e}"),
                })?;
            remote_home(target, &out)
        }
        // An arbitrary bridge command says nothing about where it lands.
        Endpoint::Command { .. } => Ok(None),
    }
}

async fn dir_exists(ep: &Endpoint, path: &str) -> Result<Option<bool>, ConnectError> {
    match ep {
        // The head and this herdr share a machine, and so a filesystem.
        Endpoint::Local { .. } => Ok(Some(std::path::Path::new(path).is_dir())),
        Endpoint::Ssh {
            target,
            control_path,
            ..
        } => {
            // A repo without `~` skips `home_dir`, so this may be the first ssh
            // to this machine and start the master.
            ensure_control_dir(control_path.as_deref())?;
            let argv = ssh_argv_running(target, control_path.as_deref(), remote_dir_command(path));
            let out = tokio::process::Command::new(&argv[0])
                .args(&argv[1..])
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output()
                .await
                .map_err(|e| ConnectError {
                    message: format!("spawn ssh: {e}"),
                })?;
            remote_dir_answer(target, &out)
        }
        // An arbitrary bridge command says nothing about where it lands.
        Endpoint::Command { .. } => Ok(None),
    }
}

/// `test -d` in the remote shell, answered on stdout with one word. `test -d`
/// follows symlinks, as `cd` does.
fn remote_dir_command(path: &str) -> String {
    format!(
        "if test -d {}; then printf yes; else printf no; fi",
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
    if matches!(out.status.code(), Some(255) | None) {
        return Err(ConnectError {
            message: format!(
                "ssh {target}: {} ({})",
                String::from_utf8_lossy(&out.stderr).trim(),
                out.status
            ),
        });
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let text = text.trim_end();
    if out.status.success() && text.ends_with("yes") {
        Ok(Some(true))
    } else if out.status.success() && text.ends_with("no") {
        Ok(Some(false))
    } else {
        tracing::warn!(%target, status = %out.status, stdout = ?text, "no answer to the repo check from the remote shell");
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

/// The master socket lives in the ControlPath's directory; ssh creates the
/// socket itself but not the directory, and it must not be world-readable.
/// Every ssh that carries a ControlPath can be the one that starts the master,
/// so each of them calls this first. The path is in ssh's escaped form (`%%`
/// for a literal `%`), so undo that before touching the filesystem or a `%` in
/// the state dir would create one directory while ssh looks for another.
fn ensure_control_dir(control_path: Option<&Path>) -> Result<(), ConnectError> {
    let Some(parent) = control_path.and_then(|p| p.parent()) else {
        return Ok(());
    };
    let literal = PathBuf::from(parent.to_string_lossy().replace("%%", "%"));
    crate::config::create_private_dir(&literal).map_err(|e| ConnectError {
        message: e.to_string(),
    })
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
            name: name.into(),
            local: false,
            ssh: Some("fleet@host".into()),
            command: None,
            session: "default".into(),
            max_agents: 2,
            tags: vec![],
        }
    }

    /// The machine name in the ControlPath is only there to be recognisable;
    /// `%C` is the identity. A name that would push the socket name past
    /// `sun_path` must be shortened, not cost every request a full handshake.
    /// `pastor-sauron` under the default state dir is exactly that case.
    #[test]
    fn long_machine_names_are_shortened_to_keep_multiplexing() {
        let paths = Paths::new(
            "/home/cacarico/.config/pastor",
            "/home/cacarico/.local/state/pastor",
        );
        let Endpoint::Ssh { control_path, .. } =
            Endpoint::from_machine(&ssh_machine("pastor-sauron"), &paths)
        else {
            panic!("ssh machine")
        };
        let path = control_path.expect("a shortened path must still multiplex");
        assert!(control_path_fits(&path), "{}", path.display());
        let text = path.to_string_lossy();
        assert!(
            text.starts_with("/home/cacarico/.local/state/pastor/ssh/pastor"),
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

    #[test]
    fn socket_paths_and_bridge_command() {
        let p = local_socket_path("default").unwrap();
        assert!(p.ends_with("herdr/herdr.sock"), "{}", p.display());
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
            name: "pi-3".into(),
            local: false,
            ssh: Some("fleet@pi-3".into()),
            command: None,
            session: "default".into(),
            max_agents: 2,
            tags: vec![],
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
            "herdr --session default remote-api-bridge"
        );
        assert_eq!(argv[argv.len() - 2], "fleet@pi-3");
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
        assert_eq!(argv.last().unwrap(), "printf %s \"$HOME\"");
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

    #[test]
    fn remote_dir_command_quotes_the_path() {
        assert_eq!(
            remote_dir_command("/srv/my app/it's"),
            "if test -d '/srv/my app/it'\\''s'; then printf yes; else printf no; fi"
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

    /// Only ssh itself failing (255, or killed) means the machine was not
    /// reached. An unset or relative `$HOME`, or a remote command that fails,
    /// comes from a reachable machine and must not mark it lost.
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
        let killed = std::process::Output {
            status: std::process::ExitStatus::from_raw(9),
            stdout: vec![],
            stderr: vec![],
        };
        assert!(remote_home("t", &killed).is_err());
    }

    #[test]
    fn a_fresh_state_dir_gets_a_private_ssh_dir() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        // A `%` in the state dir is escaped in the ControlPath; the directory
        // made must be the literal one ssh will look in.
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s%1"));
        ensure_control_dir(Some(&paths.ssh_control_path("pi-3"))).unwrap();
        let md = std::fs::metadata(paths.ssh_dir()).unwrap();
        assert!(md.is_dir());
        assert_eq!(md.permissions().mode() & 0o777, 0o700);
        assert!(!tmp.path().join("s%%1").exists());
        ensure_control_dir(None).unwrap();
    }

    #[test]
    fn a_control_path_that_cannot_fit_drops_multiplexing() {
        // 40 bytes for %C, 17 for ssh's staging and a NUL leave about 50 for the
        // directory and the name, so a deep state dir runs out.
        let deep = format!("/tmp/{}", "d".repeat(80));
        let m = MachineConfig {
            name: "pi-3".into(),
            local: false,
            ssh: Some("fleet@pi-3".into()),
            command: None,
            session: "default".into(),
            max_agents: 2,
            tags: vec![],
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
        assert!(
            !argv.iter().any(|a| a.starts_with("ControlPath=")),
            "a machine still connects, just without multiplexing: {argv:?}"
        );
        assert_eq!(
            argv.last().unwrap(),
            "herdr --session default remote-api-bridge"
        );
    }

    #[test]
    fn expanded_len_counts_ssh_escapes() {
        assert_eq!(expanded_len(Path::new("/a/b")), 4);
        assert_eq!(expanded_len(Path::new("%%")), 1);
        assert_eq!(expanded_len(Path::new("/a-%C")), 3 + EXPANDED_C);
        // The guard's boundary: exactly `UNIX_PATH_MAX` staged bytes is fine.
        let fits = "x".repeat(UNIX_PATH_MAX - CONTROL_PATH_STAGING - 1);
        assert!(control_path_fits(Path::new(&fits)));
        assert!(!control_path_fits(Path::new(&format!("{fits}x"))));
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
}
