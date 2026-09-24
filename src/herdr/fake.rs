use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::broadcast;

use super::{AgentInfo, AgentStatus, BoxRead, BoxWrite, Connection, Event, Request};

/// How `agent.start` behaves. herdr's `agent.start` never reports readiness
/// (its error codes are `agent_pane_*`, `unsupported_agent_kind`,
/// `agent_name_taken`, ...): readiness only shows up later, through
/// `agent.list` and `agent.prompt`. See `ready_after`.
#[derive(Debug, Clone)]
pub enum StartBehaviour {
    Ready,
    Fail(String),
}

#[derive(Default)]
struct State {
    next_ws: u32,
    agents: HashMap<String, AgentInfo>,
    /// pane id -> when `agent.start` ran, for the `ready_after` window.
    started: HashMap<String, Instant>,
    requests: Vec<Request>,
    start: Option<StartBehaviour>,
    protocol: u32,
    /// A method name that, once received, gets no reply at all: the connection
    /// just stops answering, simulating a wedged herdr.
    hang: Option<String>,
    /// How long a freshly started agent stays unready: `agent.list` reports it
    /// `unknown` and `agent.prompt` answers `agent_not_ready`, the way herdr
    /// does while a managed agent is still launching — unless the agent was
    /// set `blocked`, which `agent.list` and `agent.prompt` report as such
    /// even during this window. Zero by default.
    ready_after: Duration,
    /// What `Connector::home_dir` reports; `None` like a `command` machine.
    home: Option<String>,
    /// The started agent vanishes immediately, as it does when the agent binary
    /// is missing and the process exits the moment it is launched.
    exit_on_start: bool,
    /// The started agent's process dies at once but herdr keeps its pane in
    /// `agent.list`, with neither launch flag set, instead of dropping it.
    exit_listed: bool,
}

#[derive(Clone)]
pub struct FakeHerdr {
    state: Arc<Mutex<State>>,
    events: broadcast::Sender<Event>,
    /// Sent by disconnect_all. Every serve loop holds its own receiver: `connect()`
    /// subscribes synchronously, before spawning the task that runs `serve`, so a
    /// `disconnect_all()` called right after `connect()` returns is never lost to a
    /// task that hasn't subscribed yet.
    kill: broadcast::Sender<()>,
}

impl Default for FakeHerdr {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeHerdr {
    pub fn new() -> FakeHerdr {
        let (events, _) = broadcast::channel(256);
        let (kill, _) = broadcast::channel(16);
        FakeHerdr {
            state: Arc::new(Mutex::new(State {
                protocol: 22,
                home: Some("/home/fake".into()),
                ..Default::default()
            })),
            events,
            kill,
        }
    }

    pub fn set_start_behaviour(&self, b: StartBehaviour) {
        self.state.lock().unwrap().start = Some(b);
    }
    pub fn set_home(&self, home: Option<&str>) {
        self.state.lock().unwrap().home = home.map(str::to_string);
    }
    pub fn set_protocol(&self, p: u32) {
        self.state.lock().unwrap().protocol = p;
    }
    /// A started agent reports `unknown` and refuses prompts for this long,
    /// like a managed agent herdr is still launching — except a `blocked`
    /// agent, which stays `blocked` and refuses with `agent_blocked` instead.
    pub fn set_ready_after(&self, d: Duration) {
        self.state.lock().unwrap().ready_after = d;
    }
    /// The next started agents disappear the moment they start: `agent.start`
    /// succeeds and `agent.list` never shows them again.
    pub fn exit_agents_on_start(&self, yes: bool) {
        self.state.lock().unwrap().exit_on_start = yes;
    }
    /// The next started agents die at once but their pane stays in `agent.list`
    /// with neither `launch_pending` nor `interactive_ready`, as herdr reports a
    /// managed agent whose process exited before becoming interactive.
    pub fn exit_agents_listed(&self, yes: bool) {
        self.state.lock().unwrap().exit_listed = yes;
    }
    /// The next request for `method` gets no reply; the connection just stops
    /// answering, as if the herdr process wedged. Lets tests exercise a client-side
    /// request timeout instead of a transport-level error.
    pub fn hang_method(&self, method: &str) {
        self.state.lock().unwrap().hang = Some(method.into());
    }
    pub fn agents(&self) -> Vec<AgentInfo> {
        self.state
            .lock()
            .unwrap()
            .agents
            .values()
            .cloned()
            .collect()
    }
    pub fn requests(&self) -> Vec<Request> {
        self.state.lock().unwrap().requests.clone()
    }

    /// A fresh connection, good for exactly one request (or one subscription).
    pub fn connect(&self) -> Connection {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let fake = self.clone();
        // Subscribed here, before the task is spawned, so a disconnect_all() that
        // races the spawned task (e.g. called on the very next line, with no await
        // in between) is never sent to zero receivers.
        let kill = self.kill.subscribe();
        tokio::spawn(async move { fake.serve_with_kill(Box::new(br), Box::new(bw), kill).await });
        Connection::new(Box::new(ar), Box::new(aw))
    }

    pub fn set_status(&self, pane_id: &str, status: AgentStatus, completion_seq: Option<u64>) {
        let mut s = self.state.lock().unwrap();
        if let Some(a) = s.agents.get_mut(pane_id) {
            a.agent_status = status;
            a.state_change_seq += 1;
            if completion_seq.is_some() {
                a.completion_seq = completion_seq;
            }
        }
        let ws = pane_id.split(':').next().unwrap_or("w1").to_string();
        let _ = self.events.send(Event {
            event: "pane.agent_status_changed".into(),
            data: json!({"pane_id": pane_id, "workspace_id": ws, "agent_status": status}),
        });
    }

    pub fn close_pane(&self, pane_id: &str) {
        self.state.lock().unwrap().agents.remove(pane_id);
        let ws = pane_id.split(':').next().unwrap_or("w1").to_string();
        let _ = self.events.send(Event {
            event: "pane_closed".into(),
            data: json!({"type": "pane_closed", "pane_id": pane_id, "workspace_id": ws}),
        });
    }

    pub fn exit_pane(&self, pane_id: &str) {
        self.state.lock().unwrap().agents.remove(pane_id);
        let ws = pane_id.split(':').next().unwrap_or("w1").to_string();
        let _ = self.events.send(Event {
            event: "pane_exited".into(),
            data: json!({"type": "pane_exited", "pane_id": pane_id, "workspace_id": ws}),
        });
    }

    pub fn disconnect_all(&self) {
        let _ = self.kill.send(());
    }

    /// Serve one connection (one request, or one subscription) on this reader
    /// and writer. Used by the `fake-herdr` binary.
    pub async fn serve(&self, reader: BoxRead, writer: BoxWrite) {
        let kill = self.kill.subscribe();
        self.serve_with_kill(reader, writer, kill).await;
    }

    /// Serves exactly one request on this connection and returns, closing it,
    /// which is what herdr's API server does (`src/api/server.rs`,
    /// `handle_connection_with_stop`). `events.subscribe` is the one method that
    /// keeps the connection: it never returns until the stream ends.
    async fn serve_with_kill(
        &self,
        reader: BoxRead,
        mut writer: BoxWrite,
        mut kill: broadcast::Receiver<()>,
    ) {
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        // `biased` puts the kill branch first so a kill that raced a
        // simultaneously-ready read always wins the poll, instead of
        // `select!`'s default random pick servicing the request first.
        let read = tokio::select! {
            biased;
            _ = kill.recv() => return,
            r = reader.read_line(&mut line) => r,
        };
        match read {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let Ok(req) = serde_json::from_str::<Request>(line.trim()) else {
            return;
        };
        self.state.lock().unwrap().requests.push(req.clone());
        let hung = {
            let mut s = self.state.lock().unwrap();
            if s.hang.as_deref() == Some(req.method.as_str()) {
                // One-shot: only this one request hangs, as `hang_method` documents.
                s.hang = None;
                true
            } else {
                false
            }
        };
        if hung {
            // Wedged herdr: never reply, never close. Only the kill channel (a
            // test disconnecting the fake) ends this.
            let _ = kill.recv().await;
            return;
        }
        if req.method == "events.subscribe" {
            let subs: Vec<Value> = req
                .params
                .get("subscriptions")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let mut rx = self.events.subscribe();
            let ack = json!({"id": req.id, "result": {"type": "subscription_started"}});
            if writer
                .write_all(format!("{ack}\n").as_bytes())
                .await
                .is_err()
            {
                return;
            }
            loop {
                let ev = tokio::select! {
                    biased;
                    _ = kill.recv() => return,
                    ev = rx.recv() => ev,
                };
                let ev = match ev {
                    Ok(ev) => ev,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let err = json!({"id": req.id, "error": {"code": "events_lost", "message": "fell behind"}});
                        let _ = writer.write_all(format!("{err}\n").as_bytes()).await;
                        return;
                    }
                    Err(_) => return,
                };
                if subscription_matches(&subs, &ev) {
                    let line = serde_json::to_string(&ev).unwrap();
                    if writer
                        .write_all(format!("{line}\n").as_bytes())
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
        let reply = match self.handle(&req) {
            Ok(result) => json!({"id": req.id, "result": result}),
            Err((code, message)) => {
                json!({"id": req.id, "error": {"code": code, "message": message}})
            }
        };
        let _ = writer.write_all(format!("{reply}\n").as_bytes()).await;
        // Returning here drops `writer`, closing the connection: one reply is all
        // a herdr connection ever carries.
    }

    fn handle(&self, req: &Request) -> Result<Value, (String, String)> {
        let mut s = self.state.lock().unwrap();
        let p = &req.params;
        match req.method.as_str() {
            "ping" => Ok(json!({"type": "pong", "version": "fake", "protocol": s.protocol})),
            "workspace.create" | "worktree.create" => {
                s.next_ws += 1;
                let ws = format!("w{}", s.next_ws);
                let pane = format!("{ws}:p1");
                let label = p.get("label").cloned().unwrap_or(Value::Null);
                let kind = if req.method == "worktree.create" {
                    "worktree_created"
                } else {
                    "workspace_created"
                };
                let mut result = json!({"type": kind, "workspace": {"workspace_id": ws, "label": label}, "tab": {"tab_id": format!("{ws}:t1")}, "root_pane": {"pane_id": pane, "workspace_id": ws}});
                if kind == "worktree_created" {
                    result["worktree"] = json!({"path": p.get("cwd").cloned().unwrap_or(Value::Null), "branch": p.get("branch").cloned().unwrap_or(Value::Null)});
                }
                Ok(result)
            }
            "agent.start" => {
                let pane_id = p["pane_id"].as_str().unwrap_or("").to_string();
                // The fake creates exactly one pane per workspace ("w<N>:p1"), so a
                // pane is known only if it has that shape and N is a workspace that
                // was actually created. `strip_prefix` avoids byte-slicing the
                // workspace part directly (`ws[1..]`), which panics on non-ASCII
                // input and would poison `state`'s mutex while it's held.
                let mut parts = pane_id.split(':');
                let ws_part = parts.next().unwrap_or("");
                let pane_part = parts.next();
                let ws_num = ws_part
                    .strip_prefix('w')
                    .and_then(|n| n.parse::<u32>().ok());
                let known = matches!((ws_num, pane_part), (Some(n), Some("p1")) if n >= 1 && n <= s.next_ws);
                if !known {
                    return Err(("pane_not_found".into(), pane_id));
                }
                let ws = ws_part.to_string();
                match s.start.clone().unwrap_or(StartBehaviour::Ready) {
                    StartBehaviour::Fail(code) => return Err((code, "start failed".into())),
                    StartBehaviour::Ready => {}
                }
                let info = AgentInfo {
                    pane_id: pane_id.clone(),
                    workspace_id: ws.clone(),
                    tab_id: format!("{ws}:t1"),
                    name: p["name"].as_str().map(str::to_string),
                    agent: p["kind"].as_str().map(str::to_string),
                    agent_status: AgentStatus::Idle,
                    completion_seq: None,
                    state_change_seq: 1,
                    launch_pending: false,
                    interactive_ready: true,
                };
                if s.exit_listed {
                    // The agent process died on launch, but herdr keeps the pane
                    // in `agent.list` with neither launch flag set, the way it
                    // reports a managed agent that exited before becoming
                    // interactive.
                    let dead = AgentInfo {
                        interactive_ready: false,
                        launch_pending: false,
                        ..info.clone()
                    };
                    s.agents.insert(pane_id, dead.clone());
                    return Ok(json!({"type": "agent_started", "agent": dead, "argv": []}));
                }
                if s.exit_on_start {
                    // The agent process died on launch: herdr still reports the
                    // start it performed, and the agent is gone from then on.
                    return Ok(json!({"type": "agent_started", "agent": info, "argv": []}));
                }
                s.started.insert(pane_id.clone(), Instant::now());
                s.agents.insert(pane_id, info.clone());
                Ok(json!({"type": "agent_started", "agent": info, "argv": []}))
            }
            "agent.prompt" => {
                let target = p["target"].as_str().unwrap_or("");
                let ready_after = s.ready_after;
                let Some(found) = s
                    .agents
                    .values()
                    .find(|a| a.name.as_deref() == Some(target) || a.pane_id == target)
                    .cloned()
                else {
                    return Err(("agent_not_found".into(), target.into()));
                };
                // herdr checks `blocked` before it checks launch-pending or
                // readiness (`src/app/api/agents.rs`): an agent stuck on its
                // own startup question answers `agent_blocked` even while
                // herdr still counts it as launching.
                if found.agent_status == AgentStatus::Blocked {
                    return Err(("agent_blocked".into(), "agent is blocked".into()));
                }
                let launching = is_launching(&s.started, &found.pane_id, ready_after);
                if launching || !found.interactive_ready {
                    return Err((
                        "agent_not_ready".into(),
                        format!("agent {target} is not an active named agent"),
                    ));
                }
                let a = s
                    .agents
                    .get_mut(&found.pane_id)
                    .expect("found above, under the same lock");
                a.agent_status = AgentStatus::Working;
                a.state_change_seq += 1;
                let info = a.clone();
                let _ = self.events.send(Event {
                    event: "pane.agent_status_changed".into(),
                    data: json!({"pane_id": info.pane_id, "workspace_id": info.workspace_id, "agent_status": "working"}),
                });
                Ok(json!({"type": "agent_prompted", "agent": info}))
            }
            "agent.list" => {
                let ready_after = s.ready_after;
                let agents: Vec<AgentInfo> = s
                    .agents
                    .values()
                    .map(|a| {
                        if is_launching(&s.started, &a.pane_id, ready_after) {
                            AgentInfo {
                                // herdr keeps `launch_pending` set while an agent
                                // sits on its own startup question (a folder
                                // trust dialog, say) and reports it `blocked`.
                                agent_status: if a.agent_status == AgentStatus::Blocked {
                                    AgentStatus::Blocked
                                } else {
                                    AgentStatus::Unknown
                                },
                                launch_pending: true,
                                interactive_ready: false,
                                ..a.clone()
                            }
                        } else {
                            a.clone()
                        }
                    })
                    .collect();
                Ok(json!({"type": "agent_list", "agents": agents}))
            }
            "agent.read" => Ok(json!({"type": "pane_read", "read": {"text": "fake output\n"}})),
            other => Err(("unsupported_method".into(), other.into())),
        }
    }
}

impl super::transport::Connector for FakeHerdr {
    fn connect(&self) -> super::transport::ConnectFuture<'_> {
        Box::pin(async move { Ok(FakeHerdr::connect(self)) })
    }
    fn describe(&self) -> String {
        "fake herdr".into()
    }
    fn home_dir(&self) -> super::transport::HomeFuture<'_> {
        let home = self.state.lock().unwrap().home.clone();
        Box::pin(async move { Ok(home) })
    }
}

/// Is this agent still inside its `ready_after` window?
fn is_launching(started: &HashMap<String, Instant>, pane_id: &str, ready_after: Duration) -> bool {
    started
        .get(pane_id)
        .is_some_and(|at| at.elapsed() < ready_after)
}

fn subscription_matches(subs: &[Value], ev: &Event) -> bool {
    subs.iter().any(|s| {
        let t = s.get("type").and_then(Value::as_str).unwrap_or("");
        match t {
            "pane.agent_status_changed" => {
                ev.is_agent_status() && s.get("pane_id").and_then(Value::as_str) == ev.pane_id()
            }
            "pane.closed" => ev.is_pane_closed(),
            "pane.exited" => ev.is_pane_exited(),
            _ => false,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::ConnectorExt;

    #[tokio::test]
    async fn create_start_prompt_list_roundtrip() {
        // Each call here opens its own connection, exactly as it does against a
        // real herdr; the fake's state lives in the `FakeHerdr`, not the connection.
        let fake = FakeHerdr::new();
        assert_eq!(fake.ping().await.unwrap().protocol, 22);
        let created = fake.workspace_create(Some("/tmp"), "t-1").await.unwrap();
        assert_eq!(created.root_pane.pane_id, "w1:p1");
        let a = fake
            .agent_start("t-1", "claude", "w1:p1", &[])
            .await
            .unwrap();
        assert_eq!(a.name.as_deref(), Some("t-1"));
        assert_eq!(a.agent_status, AgentStatus::Idle);
        let a = fake.agent_prompt("t-1", "hello").await.unwrap();
        assert_eq!(a.agent_status, AgentStatus::Working);
        let list = fake.agent_list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(
            fake.requests()
                .iter()
                .map(|r| r.method.as_str())
                .collect::<Vec<_>>(),
            [
                "ping",
                "workspace.create",
                "agent.start",
                "agent.prompt",
                "agent.list"
            ]
        );
    }

    #[tokio::test]
    async fn start_behaviours() {
        let fake = FakeHerdr::new();
        let created = fake.workspace_create(None, "x").await.unwrap();
        fake.set_start_behaviour(StartBehaviour::Fail("unsupported_agent_kind".into()));
        let err = fake
            .agent_start("t-3", "nope", &created.root_pane.pane_id, &[])
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some("unsupported_agent_kind"));
        let err = fake
            .agent_start("t-4", "claude", "w9:p9", &[])
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some("pane_not_found"));
    }

    #[tokio::test]
    async fn prompt_on_blocked_agent_is_rejected() {
        let fake = FakeHerdr::new();
        let created = fake.workspace_create(None, "x").await.unwrap();
        fake.agent_start("t-1", "claude", &created.root_pane.pane_id, &[])
            .await
            .unwrap();
        fake.set_status(&created.root_pane.pane_id, AgentStatus::Blocked, None);
        let err = fake.agent_prompt("t-1", "hi").await.unwrap_err();
        assert_eq!(err.code(), Some("agent_blocked"));
    }

    #[tokio::test]
    async fn subscription_filters_by_pane_and_delivers_lifecycle() {
        let fake = FakeHerdr::new();
        let a = fake.workspace_create(None, "a").await.unwrap();
        let b = fake.workspace_create(None, "b").await.unwrap();
        fake.agent_start("t-1", "claude", &a.root_pane.pane_id, &[])
            .await
            .unwrap();
        fake.agent_start("t-2", "claude", &b.root_pane.pane_id, &[])
            .await
            .unwrap();
        let mut stream = fake
            .subscribe(vec![
                super::super::subscription_lifecycle("pane.closed"),
                super::super::subscription_agent_status(&a.root_pane.pane_id),
            ])
            .await
            .unwrap();
        fake.set_status(&b.root_pane.pane_id, AgentStatus::Blocked, None); // filtered out
        fake.set_status(&a.root_pane.pane_id, AgentStatus::Working, None);
        fake.close_pane(&b.root_pane.pane_id);
        let e1 = stream.next().await.unwrap();
        assert_eq!(e1.pane_id(), Some(a.root_pane.pane_id.as_str()));
        assert_eq!(e1.agent_status(), Some(AgentStatus::Working));
        let e2 = stream.next().await.unwrap();
        assert!(e2.is_pane_closed());
        assert_eq!(e2.pane_id(), Some(b.root_pane.pane_id.as_str()));
    }

    /// herdr's API server answers exactly one request per connection and then
    /// closes (`src/api/server.rs`, `handle_connection_with_stop`); only
    /// `events.subscribe` keeps the socket open. The fake must do the same, or
    /// every test in this crate passes against a server pastor will never meet.
    /// Written with the raw line protocol on purpose: `Connection::call` consumes
    /// the connection, so a second request on one connection is not expressible
    /// through the client at all.
    #[tokio::test]
    async fn second_request_on_one_connection_gets_eof() {
        use tokio::io::AsyncBufReadExt;
        let fake = FakeHerdr::new();
        let (a, b) = tokio::io::duplex(64 * 1024);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let server = fake.clone();
        tokio::spawn(async move { server.serve(Box::new(br), Box::new(bw)).await });
        let mut reader = BufReader::new(ar);
        let mut writer = aw;

        writer
            .write_all(b"{\"id\":\"1\",\"method\":\"ping\",\"params\":{}}\n")
            .await
            .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert!(line.contains("pong"), "{line}");

        // The reply above was the connection's whole life. A second request may
        // even fail to write (the peer is gone); what matters is that no second
        // reply ever arrives and the read side is at EOF.
        let _ = writer
            .write_all(b"{\"id\":\"2\",\"method\":\"ping\",\"params\":{}}\n")
            .await;
        let mut second = String::new();
        let n = reader.read_line(&mut second).await.unwrap();
        assert_eq!(n, 0, "expected EOF after one reply, got {second:?}");
    }

    /// herdr reports a managed agent as `unknown` and refuses prompts until it
    /// is the pane's foreground process; `ready_after` models that window.
    #[tokio::test]
    async fn an_agent_is_unready_for_ready_after() {
        let fake = FakeHerdr::new();
        fake.set_ready_after(Duration::from_millis(200));
        let created = fake.workspace_create(None, "t-1").await.unwrap();
        fake.agent_start("t-1", "claude", &created.root_pane.pane_id, &[])
            .await
            .unwrap();
        assert_eq!(
            fake.agent_list().await.unwrap()[0].agent_status,
            AgentStatus::Unknown
        );
        let err = fake.agent_prompt("t-1", "hi").await.unwrap_err();
        assert_eq!(err.code(), Some("agent_not_ready"));

        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(
            fake.agent_list().await.unwrap()[0].agent_status,
            AgentStatus::Idle
        );
        fake.agent_prompt("t-1", "hi").await.unwrap();
    }

    /// The agent binary was missing: the start call succeeds, the process is
    /// gone straight away and `agent.list` never shows it.
    #[tokio::test]
    async fn an_agent_can_exit_on_start() {
        let fake = FakeHerdr::new();
        fake.exit_agents_on_start(true);
        let created = fake.workspace_create(None, "t-1").await.unwrap();
        fake.agent_start("t-1", "claude", &created.root_pane.pane_id, &[])
            .await
            .unwrap();
        assert!(fake.agent_list().await.unwrap().is_empty());
    }

    /// The long-lived connection a disconnect can still cut is the event stream;
    /// request connections are gone by themselves after one reply.
    #[tokio::test]
    async fn disconnect_all_closes_connections() {
        let fake = FakeHerdr::new();
        let mut stream = fake
            .subscribe(vec![super::super::subscription_lifecycle("pane.closed")])
            .await
            .unwrap();
        fake.disconnect_all();
        let err = stream.next().await.unwrap_err();
        assert!(
            matches!(
                err,
                super::super::HerdrError::Closed | super::super::HerdrError::Io(_)
            ),
            "{err:?}"
        );

        // Immediate case: no request before the kill and no yield between
        // `connect()` and `disconnect_all()` — the kill receiver must already be
        // registered synchronously in `connect()`, not lazily inside the spawned
        // `serve` task, or this send races the task and can be lost.
        let c = fake.connect();
        fake.disconnect_all();
        assert!(c.call("ping", json!({})).await.is_err());
    }

    /// herdr keeps a pane whose managed agent died in `agent.list`, with neither
    /// launch flag set. The fake must model that shape, distinct from
    /// `exit_agents_on_start` (gone from the list altogether).
    #[tokio::test]
    async fn an_exited_agent_can_stay_listed_without_flags() {
        let fake = FakeHerdr::new();
        fake.exit_agents_listed(true);
        let created = fake.workspace_create(None, "t-1").await.unwrap();
        fake.agent_start("t-1", "claude", &created.root_pane.pane_id, &[])
            .await
            .unwrap();
        let list = fake.agent_list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert!(!list[0].launch_pending);
        assert!(!list[0].interactive_ready);
        assert_eq!(list[0].agent_status, AgentStatus::Idle);
        let err = fake.agent_prompt("t-1", "hi").await.unwrap_err();
        assert_eq!(err.code(), Some("agent_not_ready"));
    }

    #[tokio::test]
    async fn launch_flags_follow_the_ready_window() {
        let fake = FakeHerdr::new();
        fake.set_ready_after(Duration::from_millis(200));
        let created = fake.workspace_create(None, "t-1").await.unwrap();
        fake.agent_start("t-1", "claude", &created.root_pane.pane_id, &[])
            .await
            .unwrap();
        let a = &fake.agent_list().await.unwrap()[0];
        assert!(a.launch_pending && !a.interactive_ready, "{a:?}");
        tokio::time::sleep(Duration::from_millis(250)).await;
        let a = &fake.agent_list().await.unwrap()[0];
        assert!(!a.launch_pending && a.interactive_ready, "{a:?}");
    }

    #[tokio::test]
    async fn agent_start_rejects_malformed_and_unknown_panes() {
        let fake = FakeHerdr::new();
        fake.workspace_create(None, "x").await.unwrap();
        for pane in ["é:p1", "w1:p99", "w1"] {
            let err = fake
                .agent_start("t", "claude", pane, &[])
                .await
                .unwrap_err();
            assert_eq!(err.code(), Some("pane_not_found"), "pane {pane:?}");
        }
        // A non-ASCII workspace part must not panic while `state`'s mutex is held
        // (that would poison it); confirm it's still usable afterwards.
        assert!(fake.agents().is_empty());
    }
}
