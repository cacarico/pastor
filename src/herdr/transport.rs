use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;

use tokio::io::AsyncReadExt;

use super::Connection;
use crate::config::flock::MachineConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    Local { session: String },
    Ssh { target: String, session: String },
    Command { argv: Vec<String> },
}

impl Endpoint {
    pub fn from_machine(m: &MachineConfig) -> Endpoint {
        if let Some(argv) = &m.command {
            Endpoint::Command { argv: argv.clone() }
        } else if let Some(target) = &m.ssh {
            Endpoint::Ssh {
                target: target.clone(),
                session: m.session.clone(),
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
            Endpoint::Ssh { target, session } => format!("ssh {target} (session {session})"),
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

fn shell_quote(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

pub type ConnectFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Connection, ConnectError>> + Send + 'a>>;

/// Anything that can open a fresh herdr connection. Endpoints for real use, FakeHerdr in tests.
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
        Endpoint::Ssh { target, session } => {
            let argv = vec![
                "ssh".to_string(),
                "-o".into(),
                "BatchMode=yes".into(),
                "-o".into(),
                "ServerAliveInterval=15".into(),
                "-o".into(),
                "ServerAliveCountMax=3".into(),
                "-T".into(),
                target.clone(),
                bridge_command(session),
            ];
            spawn(&argv).await
        }
        Endpoint::Command { argv } => spawn(argv).await,
    }
}

/// Spawn argv with piped stdio, then prove the bridge is alive with a `ping`.
/// If the process exits before answering, report its exit status and stderr.
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
    let mut stderr = child.stderr.take().expect("piped stderr");
    let mut conn = Connection::new(Box::new(stdout), Box::new(stdin));
    match tokio::time::timeout(std::time::Duration::from_secs(30), conn.ping()).await {
        Ok(Ok(_)) => Ok(conn.with_child(child)),
        Ok(Err(err)) => {
            drop(conn); // closes the child's stdin so a live process can exit
            let mut err_text = String::new();
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                stderr.read_to_string(&mut err_text),
            )
            .await;
            let status =
                match tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await {
                    Ok(Ok(s)) => s.to_string(),
                    Ok(Err(e)) => e.to_string(),
                    Err(_) => {
                        let _ = child.kill().await;
                        "still running, killed".to_string()
                    }
                };
            Err(ConnectError {
                message: format!("{}: {err} ({status}) {}", argv.join(" "), err_text.trim()),
            })
        }
        Err(_) => Err(ConnectError {
            message: format!("{}: no ping reply within 30s", argv.join(" ")),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let err = connect(&ep).await.err().unwrap();
        assert!(err.message.contains("permission denied"), "{}", err.message);
        assert!(err.message.contains("255"), "{}", err.message);
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
