use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{
    AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, Lines,
};

use super::transport::{ConnectError, Connector};
use super::{
    AgentInfo, AgentList, AgentResult, Created, Event, HerdrError, Incoming, PaneRead, Pong,
    Request, Response, WorktreeRemoved,
};

pub type BoxRead = Box<dyn AsyncRead + Unpin + Send>;
pub type BoxWrite = Box<dyn AsyncWrite + Unpin + Send>;

/// How long a dead child gets to flush its stderr and report an exit status
/// while `diagnose` builds an error message out of it.
const DIAGNOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// How much of a child's stderr is kept for that error message. Only the tail
/// is worth keeping: the last thing ssh or herdr said before dying.
const STDERR_LIMIT: usize = 8 * 1024;

/// A bridge process whose stdio this connection is speaking over. Kept so that a
/// connection that dies can name the command, its exit status and its stderr
/// (that is how a herdr missing from the remote `PATH` is diagnosed).
struct Bridge {
    argv: Vec<String>,
    child: tokio::process::Child,
    /// The tail of the child's stderr, filled by `reader` as it arrives.
    stderr: Arc<Mutex<Vec<u8>>>,
    /// Drains that stderr pipe for the child's whole life. Without it a
    /// long-lived bridge — the events subscription — blocks the moment ssh
    /// writes more diagnostics than a pipe buffer holds, and the event stream
    /// silently stops.
    reader: tokio::task::JoinHandle<()>,
}

/// Read `stderr` to EOF in the background, keeping only the last
/// `STDERR_LIMIT` bytes.
fn drain_stderr(
    mut stderr: tokio::process::ChildStderr,
) -> (Arc<Mutex<Vec<u8>>>, tokio::task::JoinHandle<()>) {
    let tail = Arc::new(Mutex::new(Vec::new()));
    let sink = tail.clone();
    let handle = tokio::spawn(async move {
        let mut chunk = [0u8; 4096];
        loop {
            match stderr.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    let mut buf = sink.lock().unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    let extra = buf.len().saturating_sub(STDERR_LIMIT);
                    if extra > 0 {
                        buf.drain(..extra);
                    }
                }
            }
        }
    });
    (tail, handle)
}

/// One herdr socket connection, which carries **at most one request and its
/// reply**, or one subscription. herdr's API server reads a single request line
/// per connection, answers it and closes (`src/api/server.rs`,
/// `handle_connection_with_stop`); only `events.subscribe` keeps the socket
/// open. `call` and `subscribe` therefore both consume the connection, so the
/// type itself rules out a second request.
pub struct Connection {
    reader: Lines<BufReader<BoxRead>>,
    writer: BoxWrite,
    bridge: Option<Bridge>,
}

impl Connection {
    pub fn new(reader: BoxRead, writer: BoxWrite) -> Connection {
        Connection {
            reader: BufReader::new(reader).lines(),
            writer,
            bridge: None,
        }
    }

    /// Keeps a bridge process alive for as long as this connection lives, and
    /// lets a failed request report what the process did. The spawner must set
    /// `kill_on_drop(true)` on the child so that dropping the connection also
    /// ends the bridge process.
    pub fn with_bridge(
        mut self,
        argv: Vec<String>,
        child: tokio::process::Child,
        stderr: tokio::process::ChildStderr,
    ) -> Connection {
        let (stderr, reader) = drain_stderr(stderr);
        self.bridge = Some(Bridge {
            argv,
            child,
            stderr,
            reader,
        });
        self
    }

    /// Sends `method` with `params` and waits for the matching response, then
    /// drops the connection. This waits indefinitely; the caller owns any
    /// timeout, e.g. by wrapping the call in `tokio::time::timeout`.
    pub async fn call(mut self, method: &str, params: Value) -> Result<Value, HerdrError> {
        match self.call_inner(method, params).await {
            Ok(v) => Ok(v),
            // An `error` reply is herdr answering, not the connection failing:
            // it keeps its code and never pays for `diagnose`.
            Err(err @ HerdrError::Api { .. }) => Err(err),
            Err(err) => Err(self.diagnose(err).await),
        }
    }

    pub async fn call_as<T: DeserializeOwned>(
        self,
        method: &str,
        params: Value,
    ) -> Result<T, HerdrError> {
        let v = self.call(method, params).await?;
        serde_json::from_value(v).map_err(|e| HerdrError::Protocol(format!("{method} result: {e}")))
    }

    async fn call_inner(&mut self, method: &str, params: Value) -> Result<Value, HerdrError> {
        let id = self.write_request(method, params).await?;
        loop {
            let Some(line) = self.read_line().await? else {
                return Err(HerdrError::Closed);
            };
            match parse_incoming(&line) {
                Some(Incoming::Response(resp)) if resp.id() == id => {
                    return match resp {
                        Response::Success { result, .. } => Ok(result),
                        Response::Error { error, .. } => Err(HerdrError::Api {
                            code: error.code,
                            message: error.message,
                        }),
                    };
                }
                Some(other) => tracing::debug!(?other, "ignoring line while waiting for {id}"),
                None => {}
            }
        }
    }

    /// Turn this connection into an event stream. herdr dedicates the connection to
    /// events after `events.subscribe`, so no more requests can be sent on it. This
    /// waits indefinitely for the subscription to be acknowledged; the caller owns
    /// any timeout, e.g. by wrapping the call in `tokio::time::timeout`.
    pub async fn subscribe(mut self, subscriptions: Vec<Value>) -> Result<EventStream, HerdrError> {
        match self.subscribe_inner(subscriptions).await {
            Ok(()) => Ok(EventStream { conn: self }),
            Err(err @ HerdrError::Api { .. }) => Err(err),
            Err(err) => Err(self.diagnose(err).await),
        }
    }

    async fn subscribe_inner(&mut self, subscriptions: Vec<Value>) -> Result<(), HerdrError> {
        let id = self
            .write_request(
                "events.subscribe",
                serde_json::json!({"subscriptions": subscriptions}),
            )
            .await?;
        loop {
            let Some(line) = self.read_line().await? else {
                return Err(HerdrError::Closed);
            };
            match parse_incoming(&line) {
                Some(Incoming::Response(Response::Success { id: rid, .. })) if rid == id => {
                    return Ok(());
                }
                // herdr 0.9.1 refuses one subscription (a pane it does not
                // have) under `<id>:sub:<index>:probe`, then closes.
                Some(Incoming::Response(Response::Error { id: rid, error }))
                    if rid == id || rid.starts_with(&format!("{id}:sub:")) =>
                {
                    return Err(HerdrError::Api {
                        code: error.code,
                        message: error.message,
                    });
                }
                _ => {}
            }
        }
    }

    async fn write_request(&mut self, method: &str, params: Value) -> Result<String, HerdrError> {
        // One request per connection, so one id is enough. It still has to match
        // the reply: herdr echoes it back, and an event line must not be mistaken
        // for the answer.
        let id = "p1".to_string();
        let mut line = serde_json::to_string(&Request {
            id: id.clone(),
            method: method.into(),
            params,
        })
        .map_err(|e| HerdrError::Protocol(e.to_string()))?;
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.flush().await?;
        Ok(id)
    }

    /// Cancellation safe: `Lines::next_line` keeps its partial-line buffer inside
    /// the `Lines` reader itself, so dropping this future mid-read (e.g. losing a
    /// `tokio::select!` branch) does not discard any bytes already read.
    async fn read_line(&mut self) -> Result<Option<String>, HerdrError> {
        Ok(self.reader.next_line().await?)
    }

    /// Turn a transport-level failure on a bridge connection into an error that
    /// names the command, its exit status and its stderr. A bridge that never
    /// started (wrong path, `ssh` refusing the host, herdr missing on the remote)
    /// looks like a plain EOF otherwise, which says nothing about why.
    ///
    /// Only failures below the API come here: an `error` reply means herdr
    /// answered, and rewriting it would lose its code and declare a healthy
    /// machine dead.
    async fn diagnose(&mut self, err: HerdrError) -> HerdrError {
        let Some(mut bridge) = self.bridge.take() else {
            return err;
        };
        // Close our end of the child's stdin so a process that is still running
        // (waiting for more input it will never get) can exit and be reaped.
        let _ = self.writer.shutdown().await;
        let status = match tokio::time::timeout(DIAGNOSE_TIMEOUT, bridge.child.wait()).await {
            Ok(Ok(s)) => s.to_string(),
            Ok(Err(e)) => e.to_string(),
            Err(_) => {
                let _ = bridge.child.kill().await;
                "still running, killed".to_string()
            }
        };
        // The child is gone, so its stderr is at EOF and the drain task is about
        // to finish; give it that moment so the last words make it into the
        // message, then take whatever it collected.
        let _ = tokio::time::timeout(DIAGNOSE_TIMEOUT, &mut bridge.reader).await;
        let stderr = {
            let buf = bridge.stderr.lock().unwrap();
            String::from_utf8_lossy(&buf).into_owned()
        };
        HerdrError::Transport(format!(
            "{}: {err} ({status}) {}",
            bridge.argv.join(" "),
            stderr.trim()
        ))
    }
}

pub struct EventStream {
    conn: Connection,
}

impl std::fmt::Debug for EventStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventStream").finish_non_exhaustive()
    }
}

impl EventStream {
    /// Returns the next event. This is cancellation safe: if the returned future is
    /// dropped before it resolves (e.g. it loses a `tokio::select!` branch), no
    /// event data is lost and the next call to `next` resumes cleanly from where
    /// the read left off.
    pub async fn next(&mut self) -> Result<Event, HerdrError> {
        match self.next_inner().await {
            Ok(ev) => Ok(ev),
            // `events_lost` and friends arrive as `error` replies: they are herdr
            // talking, so they keep their code.
            Err(err @ HerdrError::Api { .. }) => Err(err),
            Err(err) => Err(self.conn.diagnose(err).await),
        }
    }

    async fn next_inner(&mut self) -> Result<Event, HerdrError> {
        loop {
            let Some(line) = self.conn.read_line().await? else {
                return Err(HerdrError::Closed);
            };
            match parse_incoming(&line) {
                Some(Incoming::Event(ev)) => return Ok(ev),
                Some(Incoming::Response(Response::Error { error, .. })) => {
                    return Err(HerdrError::Api {
                        code: error.code,
                        message: error.message,
                    });
                }
                Some(Incoming::Response(Response::Success { .. })) | None => {}
            }
        }
    }
}

/// Either end of a one-shot request: the connection could not be opened, or the
/// request itself failed once it was.
#[derive(Debug, thiserror::Error)]
pub enum CallError {
    #[error("connect: {0}")]
    Connect(#[from] ConnectError),
    #[error(transparent)]
    Herdr(#[from] HerdrError),
}

impl CallError {
    /// The herdr error code, for the API errors that carry one (`agent_not_ready`,
    /// `pane_not_found`, ...). `None` for anything transport-level.
    pub fn code(&self) -> Option<&str> {
        match self {
            CallError::Herdr(err) => err.code(),
            CallError::Connect(_) => None,
        }
    }

    /// Did this fail below the API — connect refused, socket closed, bridge
    /// process gone? The machine actor treats that as the machine being lost,
    /// while anything herdr actually answered is just this request failing.
    ///
    /// `Protocol` is deliberately not in here: a result pastor cannot parse, or
    /// one of its own validation errors, fails the task; it says nothing about
    /// whether the machine is reachable.
    pub fn is_transport(&self) -> bool {
        match self {
            CallError::Connect(_) => true,
            CallError::Herdr(err) => matches!(
                err,
                HerdrError::Io(_) | HerdrError::Closed | HerdrError::Transport(_)
            ),
        }
    }
}

/// The request vocabulary, on top of `Connector::connect`.
///
/// Every method opens a fresh connection, sends one request, reads one reply and
/// drops the connection, because that is all herdr serves per connection. Over
/// ssh the connections are cheap: `Endpoint::Ssh` multiplexes them through one
/// `ControlMaster` (see `transport.rs`). `subscribe` is the exception herdr also
/// makes: it keeps its connection for as long as the caller holds the stream.
///
/// This is an extension trait rather than part of `Connector` so that `Connector`
/// stays dyn-compatible: the machine actor holds an `Arc<dyn Connector>` and
/// calls these straight on it.
#[allow(async_fn_in_trait)] // Self is always a concrete connector; Send leaks through.
pub trait ConnectorExt: Connector {
    async fn call(&self, method: &str, params: Value) -> Result<Value, CallError> {
        Ok(self.connect().await?.call(method, params).await?)
    }

    async fn call_as<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> Result<T, CallError> {
        Ok(self.connect().await?.call_as(method, params).await?)
    }

    async fn ping(&self) -> Result<Pong, CallError> {
        self.call_as("ping", serde_json::json!({})).await
    }

    async fn agent_list(&self) -> Result<Vec<AgentInfo>, CallError> {
        Ok(self
            .call_as::<AgentList>("agent.list", serde_json::json!({}))
            .await?
            .agents)
    }

    async fn workspace_create(&self, cwd: Option<&str>, label: &str) -> Result<Created, CallError> {
        self.call_as(
            "workspace.create",
            serde_json::json!({"cwd": cwd, "label": label, "focus": false}),
        )
        .await
    }

    async fn worktree_create(
        &self,
        cwd: &str,
        branch: &str,
        label: &str,
    ) -> Result<Created, CallError> {
        self.call_as(
            "worktree.create",
            serde_json::json!({"cwd": cwd, "branch": branch, "label": label, "focus": false}),
        )
        .await
    }

    async fn agent_start(
        &self,
        name: &str,
        kind: &str,
        pane_id: &str,
        args: &[String],
    ) -> Result<AgentInfo, CallError> {
        Ok(self
            .call_as::<AgentResult>(
                "agent.start",
                serde_json::json!({"name": name, "kind": kind, "pane_id": pane_id, "args": args}),
            )
            .await?
            .agent)
    }

    async fn agent_prompt(&self, target: &str, text: &str) -> Result<AgentInfo, CallError> {
        Ok(self
            .call_as::<AgentResult>(
                "agent.prompt",
                serde_json::json!({"target": target, "text": text}),
            )
            .await?
            .agent)
    }

    async fn agent_read(&self, target: &str, lines: u32) -> Result<String, CallError> {
        Ok(self
            .call_as::<PaneRead>(
                "agent.read",
                serde_json::json!({"target": target, "source": "recent_unwrapped", "lines": lines}),
            )
            .await?
            .read
            .text)
    }

    /// `pane.close` (herdr 0.9.1): closes the pane and, with it, the agent in
    /// it. herdr closes a workspace whose last pane closes. `pane_not_found`
    /// if it is already gone.
    async fn pane_close(&self, pane_id: &str) -> Result<(), CallError> {
        self.call("pane.close", serde_json::json!({"pane_id": pane_id}))
            .await
            .map(drop)
    }

    /// `worktree.remove` (herdr 0.9.1): deletes the worktree checkout behind a
    /// workspace made by `worktree.create` and closes that workspace, panes
    /// and agent included. Without `force`, a checkout with uncommitted or
    /// untracked files is refused with `dirty_worktree_requires_force`; a
    /// plain workspace is `not_linked_worktree`, an unknown one
    /// `workspace_not_found`.
    async fn worktree_remove(&self, workspace_id: &str, force: bool) -> Result<(), CallError> {
        self.call_as::<WorktreeRemoved>(
            "worktree.remove",
            serde_json::json!({"workspace_id": workspace_id, "force": force}),
        )
        .await
        .map(drop)
    }

    /// Opens a connection and keeps it: the returned stream owns it until dropped.
    async fn subscribe(&self, subscriptions: Vec<Value>) -> Result<EventStream, CallError> {
        Ok(self.connect().await?.subscribe(subscriptions).await?)
    }
}

impl<T: Connector + ?Sized> ConnectorExt for T {}

fn parse_incoming(line: &str) -> Option<Incoming> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    match serde_json::from_str::<Incoming>(line) {
        Ok(v) => Some(v),
        Err(err) => {
            tracing::warn!(%err, line, "skipping unparsable line from herdr");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, duplex};

    /// Returns a client connection and the server side halves of a duplex pipe.
    fn pipe() -> (
        Connection,
        BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    ) {
        let (a, b) = duplex(64 * 1024);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        (
            Connection::new(Box::new(ar), Box::new(aw)),
            BufReader::new(br),
            bw,
        )
    }

    #[tokio::test]
    async fn call_returns_result() {
        let (client, mut sr, mut sw) = pipe();
        let server = tokio::spawn(async move {
            let mut line = String::new();
            sr.read_line(&mut line).await.unwrap();
            let req: Request = serde_json::from_str(&line).unwrap();
            assert_eq!(req.method, "ping");
            sw.write_all(format!("{{\"id\":\"{}\",\"result\":{{\"type\":\"pong\",\"version\":\"0.9.1\",\"protocol\":22}}}}\n", req.id).as_bytes()).await.unwrap();
        });
        let pong: Pong = client.call_as("ping", serde_json::json!({})).await.unwrap();
        assert_eq!(pong.protocol, 22);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn call_maps_api_errors() {
        let (client, mut sr, mut sw) = pipe();
        let server = tokio::spawn(async move {
            let mut line = String::new();
            sr.read_line(&mut line).await.unwrap();
            let req: Request = serde_json::from_str(&line).unwrap();
            assert_eq!(req.method, "agent.prompt");
            sw.write_all(format!("{{\"id\":\"{}\",\"error\":{{\"code\":\"agent_blocked\",\"message\":\"no\"}}}}\n", req.id).as_bytes()).await.unwrap();
        });
        let err = client
            .call("agent.prompt", serde_json::json!({"target": "x"}))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some("agent_blocked"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn call_reports_closed_on_eof() {
        let (client, _sr, mut sw) = pipe();
        // `sw` and `_sr` are split halves of the same underlying duplex stream, so
        // dropping `sw` alone does not signal EOF to the peer's read side while
        // `_sr` keeps the shared stream alive: shut it down explicitly instead.
        sw.shutdown().await.unwrap();
        let err = client
            .call("ping", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(matches!(err, HerdrError::Closed), "{err:?}");
    }

    #[tokio::test]
    async fn event_stream_skips_bad_lines() {
        let (client, mut sr, mut sw) = pipe();
        let server = tokio::spawn(async move {
            let mut line = String::new();
            sr.read_line(&mut line).await.unwrap();
            let req: Request = serde_json::from_str(&line).unwrap();
            assert_eq!(req.method, "events.subscribe");
            sw.write_all(
                format!(
                    "{{\"id\":\"{}\",\"result\":{{\"type\":\"subscription_started\"}}}}\n",
                    req.id
                )
                .as_bytes(),
            )
            .await
            .unwrap();
            sw.write_all(b"this is not json\n\n").await.unwrap();
            sw.write_all(b"{\"event\":\"pane_closed\",\"data\":{\"type\":\"pane_closed\",\"pane_id\":\"w1:p1\",\"workspace_id\":\"w1\"}}\n").await.unwrap();
            sw.write_all(format!("{{\"id\":\"{}\",\"error\":{{\"code\":\"events_lost\",\"message\":\"behind\"}}}}\n", req.id).as_bytes()).await.unwrap();
        });
        let mut stream = client
            .subscribe(vec![super::super::subscription_lifecycle("pane.closed")])
            .await
            .unwrap();
        let ev = stream.next().await.unwrap();
        assert!(ev.is_pane_closed());
        let err = stream.next().await.unwrap_err();
        assert_eq!(err.code(), Some("events_lost"));
        server.await.unwrap();
    }

    /// herdr 0.9.1 refuses a subscription to a pane it does not have with an
    /// error whose id is `<request id>:sub:<index>:probe`, then closes the
    /// connection. That is an API error, not a dead machine.
    #[tokio::test]
    async fn subscribe_maps_a_refused_subscription_to_an_api_error() {
        let (client, mut sr, mut sw) = pipe();
        let server = tokio::spawn(async move {
            let mut line = String::new();
            sr.read_line(&mut line).await.unwrap();
            let req: Request = serde_json::from_str(&line).unwrap();
            assert_eq!(req.method, "events.subscribe");
            sw.write_all(format!("{{\"id\":\"{}:sub:1:probe\",\"error\":{{\"code\":\"pane_not_found\",\"message\":\"pane w9:p1 not found\"}}}}\n", req.id).as_bytes()).await.unwrap();
            sw.shutdown().await.unwrap();
        });
        let Err(err) = client
            .subscribe(vec![
                super::super::subscription_lifecycle("pane.closed"),
                super::super::subscription_agent_status("w9:p1"),
            ])
            .await
        else {
            panic!("subscribed")
        };
        assert_eq!(err.code(), Some("pane_not_found"), "{err:?}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn next_is_cancellation_safe() {
        let (client, mut sr, mut sw) = pipe();
        let server = tokio::spawn(async move {
            let mut line = String::new();
            sr.read_line(&mut line).await.unwrap();
            let req: Request = serde_json::from_str(&line).unwrap();
            assert_eq!(req.method, "events.subscribe");
            sw.write_all(
                format!(
                    "{{\"id\":\"{}\",\"result\":{{\"type\":\"subscription_started\"}}}}\n",
                    req.id
                )
                .as_bytes(),
            )
            .await
            .unwrap();
            // Write the event line in two halves with a delay between them, so a
            // cancelled read races a partial line, not a fully-buffered one.
            sw.write_all(b"{\"event\":\"pane_closed\",\"data\":{\"type\":\"pane_closed\",")
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            sw.write_all(b"\"pane_id\":\"w1:p1\",\"workspace_id\":\"w1\"}}\n")
                .await
                .unwrap();
        });
        let mut stream = client
            .subscribe(vec![super::super::subscription_lifecycle("pane.closed")])
            .await
            .unwrap();

        // The second half of the line won't arrive for 50ms, so this sleep always
        // wins the race and cancels `stream.next()` mid-read.
        tokio::select! {
            _ = stream.next() => panic!("expected the sleep to win the race"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
        }

        let ev = stream.next().await.unwrap();
        assert!(ev.is_pane_closed());
        assert_eq!(ev.pane_id(), Some("w1:p1"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn subscribe_surfaces_setup_error() {
        let (client, mut sr, mut sw) = pipe();
        tokio::spawn(async move {
            let mut line = String::new();
            sr.read_line(&mut line).await.unwrap();
            let req: Request = serde_json::from_str(&line).unwrap();
            sw.write_all(format!("{{\"id\":\"{}\",\"error\":{{\"code\":\"pane_not_found\",\"message\":\"w9:p9\"}}}}\n", req.id).as_bytes()).await.unwrap();
        });
        let err = client
            .subscribe(vec![super::super::subscription_agent_status("w9:p9")])
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some("pane_not_found"));
    }
}
