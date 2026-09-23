use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use super::{
    AgentInfo, AgentList, AgentResult, Created, Event, HerdrError, Incoming, PaneRead, Pong,
    Request, Response,
};

pub type BoxRead = Box<dyn AsyncRead + Unpin + Send>;
pub type BoxWrite = Box<dyn AsyncWrite + Unpin + Send>;

/// One herdr socket connection. Requests are sequential: one in flight at a time.
pub struct Connection {
    reader: BufReader<BoxRead>,
    writer: BoxWrite,
    next_id: u64,
    _child: Option<tokio::process::Child>,
}

impl Connection {
    pub fn new(reader: BoxRead, writer: BoxWrite) -> Connection {
        Connection {
            reader: BufReader::new(reader),
            writer,
            next_id: 1,
            _child: None,
        }
    }

    pub fn with_child(mut self, child: tokio::process::Child) -> Connection {
        self._child = Some(child);
        self
    }

    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value, HerdrError> {
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

    pub async fn call_as<T: DeserializeOwned>(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<T, HerdrError> {
        let v = self.call(method, params).await?;
        serde_json::from_value(v).map_err(|e| HerdrError::Protocol(format!("{method} result: {e}")))
    }

    pub async fn ping(&mut self) -> Result<Pong, HerdrError> {
        self.call_as("ping", serde_json::json!({})).await
    }

    pub async fn agent_list(&mut self) -> Result<Vec<AgentInfo>, HerdrError> {
        Ok(self
            .call_as::<AgentList>("agent.list", serde_json::json!({}))
            .await?
            .agents)
    }

    pub async fn workspace_create(
        &mut self,
        cwd: Option<&str>,
        label: &str,
    ) -> Result<Created, HerdrError> {
        self.call_as(
            "workspace.create",
            serde_json::json!({"cwd": cwd, "label": label, "focus": false}),
        )
        .await
    }

    pub async fn worktree_create(
        &mut self,
        cwd: &str,
        branch: &str,
        label: &str,
    ) -> Result<Created, HerdrError> {
        self.call_as(
            "worktree.create",
            serde_json::json!({"cwd": cwd, "branch": branch, "label": label, "focus": false}),
        )
        .await
    }

    pub async fn agent_start(
        &mut self,
        name: &str,
        kind: &str,
        pane_id: &str,
        args: &[String],
    ) -> Result<AgentInfo, HerdrError> {
        Ok(self
            .call_as::<AgentResult>(
                "agent.start",
                serde_json::json!({"name": name, "kind": kind, "pane_id": pane_id, "args": args}),
            )
            .await?
            .agent)
    }

    pub async fn agent_prompt(
        &mut self,
        target: &str,
        text: &str,
    ) -> Result<AgentInfo, HerdrError> {
        Ok(self
            .call_as::<AgentResult>(
                "agent.prompt",
                serde_json::json!({"target": target, "text": text}),
            )
            .await?
            .agent)
    }

    pub async fn agent_read(&mut self, target: &str, lines: u32) -> Result<String, HerdrError> {
        Ok(self
            .call_as::<PaneRead>(
                "agent.read",
                serde_json::json!({"target": target, "source": "recent_unwrapped", "lines": lines}),
            )
            .await?
            .read
            .text)
    }

    /// Turn this connection into an event stream. herdr dedicates the connection to
    /// events after `events.subscribe`, so no more requests can be sent on it.
    pub async fn subscribe(mut self, subscriptions: Vec<Value>) -> Result<EventStream, HerdrError> {
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
                    return Ok(EventStream { conn: self });
                }
                Some(Incoming::Response(Response::Error { id: rid, error })) if rid == id => {
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
        let id = format!("p{}", self.next_id);
        self.next_id += 1;
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

    async fn read_line(&mut self) -> Result<Option<String>, HerdrError> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        Ok(Some(line))
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
    pub async fn next(&mut self) -> Result<Event, HerdrError> {
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
    async fn call_returns_result_and_maps_errors() {
        let (mut client, mut sr, mut sw) = pipe();
        let server = tokio::spawn(async move {
            let mut line = String::new();
            sr.read_line(&mut line).await.unwrap();
            let req: Request = serde_json::from_str(&line).unwrap();
            assert_eq!(req.method, "ping");
            sw.write_all(format!("{{\"id\":\"{}\",\"result\":{{\"type\":\"pong\",\"version\":\"0.9.1\",\"protocol\":22}}}}\n", req.id).as_bytes()).await.unwrap();
            let mut line = String::new();
            sr.read_line(&mut line).await.unwrap();
            let req: Request = serde_json::from_str(&line).unwrap();
            sw.write_all(format!("{{\"id\":\"{}\",\"error\":{{\"code\":\"agent_blocked\",\"message\":\"no\"}}}}\n", req.id).as_bytes()).await.unwrap();
        });
        let pong = client.ping().await.unwrap();
        assert_eq!(pong.protocol, 22);
        let err = client.agent_prompt("x", "hi").await.unwrap_err();
        assert_eq!(err.code(), Some("agent_blocked"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn call_reports_closed_on_eof() {
        let (mut client, _sr, mut sw) = pipe();
        // `sw` and `_sr` are split halves of the same underlying duplex stream, so
        // dropping `sw` alone does not signal EOF to the peer's read side while
        // `_sr` keeps the shared stream alive: shut it down explicitly instead.
        sw.shutdown().await.unwrap();
        let err = client.ping().await.unwrap_err();
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
