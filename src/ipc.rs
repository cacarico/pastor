use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::machine::MachineStatus;
use crate::store::TaskFilter;
use crate::task::{DispatchSpec, Task};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum IpcRequest {
    Ping,
    Run { prompt: String, spec: DispatchSpec },
    List { filter: TaskFilter },
    TaskShow { id: i64 },
    TaskRead { id: i64, lines: u32 },
    FlockList,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
// Adjacently tagged (`content = "data"`), not internally tagged: several variants
// here are newtypes wrapping a `Vec` or a `String` (`Tasks`, `Text`, `Machines`),
// and serde_json cannot serialize those under internal tagging (a sequence or a
// string cannot carry a `kind` field merged into it). Adjacent tagging works
// uniformly across struct and newtype variants alike.
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
// One response crosses the socket at a time (never in a hot loop), so the size
// difference between variants that clippy flags here doesn't matter in practice.
#[allow(clippy::large_enum_variant)]
pub enum IpcResponse {
    Pong { version: String },
    Task(Task),
    Tasks(Vec<Task>),
    Text(String),
    Machines(Vec<MachineStatus>),
    Error { code: String, message: String },
}

impl IpcResponse {
    pub fn error(code: &str, message: impl std::fmt::Display) -> IpcResponse {
        IpcResponse::Error {
            code: code.into(),
            message: message.to_string(),
        }
    }
}

/// Default bound on a full request/reply round trip. Deliberately longer than
/// `MachineSettings::request_timeout`'s default (60s): a `Run` that dispatches
/// inline (see `Daemon::handle`) must have room to finish before this gives up.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// `daemon_running` only needs to know whether something answers `Ping`; it
/// shouldn't itself hang for a minute against a wedged daemon.
const PING_TIMEOUT: Duration = Duration::from_secs(2);

/// One request, one reply, then the connection closes. Bounded by
/// `DEFAULT_REQUEST_TIMEOUT`; use `request_with_timeout` to choose a different bound.
pub async fn request(socket: &Path, req: &IpcRequest) -> anyhow::Result<IpcResponse> {
    request_with_timeout(socket, req, DEFAULT_REQUEST_TIMEOUT).await
}

/// Like `request`, but bounds the whole round trip (connect + write + read) by
/// `timeout` instead of the default. A daemon that accepts the connection but
/// never replies (wedged on a hung herdr request, or otherwise stuck) must not
/// hang the caller forever.
pub async fn request_with_timeout(
    socket: &Path,
    req: &IpcRequest,
    timeout: Duration,
) -> anyhow::Result<IpcResponse> {
    match tokio::time::timeout(timeout, request_once(socket, req)).await {
        Ok(result) => result,
        Err(_) => anyhow::bail!("pastor daemon did not respond within {timeout:?}"),
    }
}

async fn request_once(socket: &Path, req: &IpcRequest) -> anyhow::Result<IpcResponse> {
    let stream = tokio::net::UnixStream::connect(socket).await?;
    round_trip(stream, req).await
}

async fn round_trip(
    stream: tokio::net::UnixStream,
    req: &IpcRequest,
) -> anyhow::Result<IpcResponse> {
    let (r, mut w) = stream.into_split();
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
    w.flush().await?;
    let mut reply = String::new();
    BufReader::new(r).read_line(&mut reply).await?;
    anyhow::ensure!(
        !reply.trim().is_empty(),
        "daemon closed the connection without a reply"
    );
    Ok(serde_json::from_str(reply.trim())?)
}

/// What `probe_daemon` found at a socket path. Only `NotRunning` means it's safe to
/// unlink and replace the socket file: a connect refused (or a path that doesn't
/// exist) is the one signal that nothing is on the other end. `Running` and
/// `Unresponsive` both mean something is there — a busy daemon mid-request looks
/// exactly like a wedged one from the outside, so both must be left alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonProbe {
    /// Connected and got back a `Pong` within the probe window.
    Running,
    /// The connect failed the way an absent or abandoned socket does (refused, or
    /// the path doesn't exist).
    NotRunning,
    /// Something accepted the connection but didn't answer `Ping` within
    /// `PING_TIMEOUT`, or the connect failed some other way (e.g. permission
    /// denied). Either way, it is not safe to assume the socket is stale.
    Unresponsive,
}

pub async fn probe_daemon(socket: &Path) -> DaemonProbe {
    let stream = match tokio::net::UnixStream::connect(socket).await {
        Ok(s) => s,
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            return DaemonProbe::NotRunning;
        }
        Err(_) => return DaemonProbe::Unresponsive,
    };
    match tokio::time::timeout(PING_TIMEOUT, round_trip(stream, &IpcRequest::Ping)).await {
        Ok(Ok(IpcResponse::Pong { .. })) => DaemonProbe::Running,
        _ => DaemonProbe::Unresponsive,
    }
}

pub async fn daemon_running(socket: &Path) -> bool {
    matches!(probe_daemon(socket).await, DaemonProbe::Running)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[tokio::test]
    async fn request_with_timeout_bounds_a_daemon_that_never_replies() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("hung.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            // Accept the connection and then never write a reply.
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            std::future::pending::<()>().await;
            drop(stream);
        });

        let start = Instant::now();
        let err = request_with_timeout(&socket, &IpcRequest::Ping, Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "did not bound the round trip: took {:?}",
            start.elapsed()
        );
        assert!(err.to_string().contains("did not respond"), "{err}");
    }

    /// Every `IpcResponse` variant must round-trip through JSON: the newtype
    /// variants wrapping a `Vec` or `String` (`Tasks`, `Text`, `Machines`) broke
    /// under internal tagging (serde_json refuses to merge a `kind` field into a
    /// sequence or a string), which the adjacently-tagged `content = "data"`
    /// representation fixes for every shape uniformly.
    #[test]
    fn every_response_variant_round_trips_through_json() {
        let responses = vec![
            IpcResponse::Pong {
                version: "1".into(),
            },
            IpcResponse::Tasks(vec![]),
            IpcResponse::Text("hello".into()),
            IpcResponse::Machines(vec![]),
            IpcResponse::error("some_code", "some message"),
        ];
        for resp in responses {
            let json = serde_json::to_string(&resp).unwrap();
            let back: IpcResponse = serde_json::from_str(&json).unwrap();
            assert_eq!(format!("{resp:?}"), format!("{back:?}"), "{json}");
        }
    }

    #[tokio::test]
    async fn probe_daemon_reports_not_running_for_a_missing_socket() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("nothing-here.sock");
        assert_eq!(probe_daemon(&socket).await, DaemonProbe::NotRunning);
        assert!(!daemon_running(&socket).await);
    }
}
