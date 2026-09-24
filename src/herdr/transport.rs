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
            let control_path = paths.ssh_control_path(&m.name);
            let control_path = if control_path_fits(&control_path) {
                Some(control_path)
            } else {
                // Decided once, when the endpoint is built, so this is said once
                // per machine instead of once per request.
                tracing::warn!(
                    machine = %m.name,
                    path = %control_path.display(),
                    "ssh ControlPath is too long for a unix socket; connecting without multiplexing (every request pays a full ssh handshake). Set PASTOR_STATE_DIR to something shorter."
                );
                None
            };
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
    argv.extend([
        "-T".to_string(),
        target.to_string(),
        bridge_command(session),
    ]);
    argv
}

pub type ConnectFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Connection, ConnectError>> + Send + 'a>>;

/// Anything that can open a fresh herdr connection. Endpoints for real use, FakeHerdr in tests.
///
/// A connection carries one request (see `Connection`), so this is called once
/// per request; `ConnectorExt` in `client.rs` has the request vocabulary built
/// on top of it.
pub trait Connector: Send + Sync {
    fn connect(&self) -> ConnectFuture<'_>;
    fn describe(&self) -> String;
}

impl Connector for Endpoint {
    fn connect(&self) -> ConnectFuture<'_> {
        Box::pin(connect(self))
    }
    fn describe(&self) -> String {
        Endpoint::describe(self)
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
            // The master socket lives here; ssh creates the socket itself but not
            // the directory, and it must not be world-readable. The path is in
            // ssh's escaped form (`%%` for a literal `%`), so undo that before
            // touching the filesystem or a `%` in the state dir would create one
            // directory while ssh looks for another.
            if let Some(parent) = control_path.as_ref().and_then(|p| p.parent()) {
                let literal = PathBuf::from(parent.to_string_lossy().replace("%%", "%"));
                crate::config::create_private_dir(&literal).map_err(|e| ConnectError {
                    message: e.to_string(),
                })?;
            }
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
