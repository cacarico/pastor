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
#[serde(tag = "kind", rename_all = "snake_case")]
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
        Err(_) => anyhow::bail!(
            "pastor daemon did not respond within {}s",
            timeout.as_secs()
        ),
    }
}

async fn request_once(socket: &Path, req: &IpcRequest) -> anyhow::Result<IpcResponse> {
    let stream = tokio::net::UnixStream::connect(socket).await?;
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

pub async fn daemon_running(socket: &Path) -> bool {
    matches!(
        request_with_timeout(socket, &IpcRequest::Ping, PING_TIMEOUT).await,
        Ok(IpcResponse::Pong { .. })
    )
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
}
