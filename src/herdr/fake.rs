use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex};
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
    /// Open workspaces: id -> created by `worktree.create`. One pane each,
    /// `<id>:p1`, as herdr gives a new workspace.
    workspaces: HashMap<String, bool>,
    /// Worktree workspaces whose checkout has changes, so `worktree.remove`
    /// needs `force`.
    dirty: HashSet<String>,
    /// Every new worktree starts dirty (see `dirty_worktrees`).
    all_dirty: bool,
    agents: HashMap<String, AgentInfo>,
    /// pane id -> when `agent.start` ran, for the `ready_after` window.
    started: HashMap<String, Instant>,
    requests: Vec<Request>,
    /// Where `requests` is written, whole, each time one is received.
    request_log: Option<std::path::PathBuf>,
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
    /// Paths `Connector::dir_exists` reports missing; every other path exists.
    missing_dirs: HashSet<String>,
    /// What `Connector::pastor_version` reports.
    pastor_version: Option<String>,
    /// The started agent vanishes immediately, as it does when the agent binary
    /// is missing and the process exits the moment it is launched.
    exit_on_start: bool,
    /// The started agent's process dies at once but herdr keeps its pane in
    /// `agent.list`, with neither launch flag set, instead of dropping it.
    exit_listed: bool,
    /// `agent.prompt` is accepted but the agent never acts on it: it stays idle
    /// and its `state_change_seq` does not move.
    ignore_prompts: bool,
    /// How many of the next `agent.start` calls answer `agent_pane_busy`, as
    /// herdr does while a new pane's shell is still starting.
    pane_busy_for: u32,
    /// herdr's `next_agent_state_change_seq`: one counter for the whole server,
    /// bumped on every agent state change and stamped on the agent that changed.
    seq: u64,
    /// Closed just before the next `events.subscribe` that names it (see
    /// `close_pane_before_subscribe`).
    close_before_subscribe: Option<String>,
    /// Set silently just before the next `agent.list` that comes straight
    /// after another one, within 50ms (see `set_status_between_lists`).
    status_between_lists: Option<(String, AgentStatus)>,
    /// When the last `agent.list` arrived.
    last_list: Option<Instant>,
}

/// herdr 0.9.1 derives `agent_status` from a detected state (idle, working,
/// blocked, unknown) plus a per-pane `seen` flag: `done` is idle after a
/// completion nobody has looked at yet, `idle` is idle and seen. Only a change
/// of the detected state bumps `state_change_seq`; `done` -> `idle` (someone
/// looked) does not.
fn detected(status: AgentStatus) -> u8 {
    match status {
        AgentStatus::Idle | AgentStatus::Done => 0,
        AgentStatus::Working => 1,
        AgentStatus::Blocked => 2,
        AgentStatus::Unknown => 3,
    }
}

/// Set an agent's status the way herdr does, bumping the server-wide sequence
/// when the detected state changes. Returns the agent after the change.
fn change_status(s: &mut State, pane_id: &str, status: AgentStatus) -> Option<AgentInfo> {
    let bump = detected(s.agents.get(pane_id)?.agent_status) != detected(status);
    if bump {
        s.seq += 1;
    }
    let seq = s.seq;
    let a = s.agents.get_mut(pane_id)?;
    a.agent_status = status;
    if bump {
        a.state_change_seq = seq;
    }
    Some(a.clone())
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
    /// See `wedge_connects`.
    wedge: Arc<(Mutex<Wedge>, Condvar)>,
}

#[derive(Default)]
struct Wedge {
    on: bool,
    /// Threads blocked in `Connector::connect` right now.
    waiting: usize,
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
                pastor_version: Some("fake".into()),
                ..Default::default()
            })),
            events,
            kill,
            wedge: Arc::default(),
        }
    }

    pub fn set_start_behaviour(&self, b: StartBehaviour) {
        self.state.lock().unwrap().start = Some(b);
    }
    pub fn set_home(&self, home: Option<&str>) {
        self.state.lock().unwrap().home = home.map(str::to_string);
    }
    pub fn set_pastor_version(&self, version: Option<&str>) {
        self.state.lock().unwrap().pastor_version = version.map(str::to_string);
    }
    pub fn set_missing_dir(&self, path: &str) {
        self.state
            .lock()
            .unwrap()
            .missing_dirs
            .insert(path.to_string());
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
    /// The next `n` `agent.start` calls answer `agent_pane_busy`, the way
    /// herdr refuses a pane whose shell has not finished starting (t-42 and
    /// t-50 on the real fleet, 2026-09-25).
    pub fn set_pane_busy_for(&self, n: u32) {
        self.state.lock().unwrap().pane_busy_for = n;
    }
    /// The next request for `method` gets no reply; the connection just stops
    /// answering, as if the herdr process wedged. Lets tests exercise a client-side
    /// request timeout instead of a transport-level error.
    pub fn hang_method(&self, method: &str) {
        self.state.lock().unwrap().hang = Some(method.into());
    }
    /// Mark one worktree workspace as holding uncommitted changes: herdr
    /// then refuses `worktree.remove` without `force`.
    pub fn set_dirty(&self, workspace_id: &str) {
        self.state.lock().unwrap().dirty.insert(workspace_id.into());
    }
    /// Every worktree created from now on is dirty.
    pub fn dirty_worktrees(&self, yes: bool) {
        self.state.lock().unwrap().all_dirty = yes;
    }
    /// Open workspace ids, sorted.
    pub fn workspaces(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .state
            .lock()
            .unwrap()
            .workspaces
            .keys()
            .cloned()
            .collect();
        v.sort();
        v
    }
    /// While on, `Connector::connect` blocks the calling thread instead of
    /// yielding. `hang_method` wedges an await, which an abort still ends;
    /// this wedges a poll, which nothing can interrupt, so a test can have an
    /// aborted actor that has not ended. Needs a multi-thread runtime.
    pub fn wedge_connects(&self, on: bool) {
        let (lock, cvar) = &*self.wedge;
        lock.lock().unwrap().on = on;
        cvar.notify_all();
    }
    /// How many threads are blocked in `connect` by `wedge_connects`.
    pub fn wedged(&self) -> usize {
        self.wedge.0.lock().unwrap().waiting
    }
    fn wait_unwedged(&self) {
        let (lock, cvar) = &*self.wedge;
        let mut w = lock.lock().unwrap();
        w.waiting += 1;
        while w.on {
            w = cvar.wait(w).unwrap();
        }
        w.waiting -= 1;
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
    /// Write every request received so far to `path` as one JSON array, each
    /// time one arrives. Written when the request is recorded, before any
    /// reply, so a subscribe that stays open or a request that hangs is in the
    /// log as soon as it was received. Via a rename, so a reader never sees
    /// half a file; under the state lock, so two writers cannot reorder.
    pub fn set_request_log(&self, path: std::path::PathBuf) {
        self.state.lock().unwrap().request_log = Some(path);
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

    /// `agent.prompt` is accepted from now on, but the agent never starts
    /// working on it, as if the text never reached it.
    pub fn ignore_prompts(&self, yes: bool) {
        self.state.lock().unwrap().ignore_prompts = yes;
    }

    /// Change an agent's status and publish `pane.agent_status_changed`, as
    /// herdr does. There is no `completion_seq` in herdr 0.9.1: a finished
    /// task shows up as `idle` or `done` with a newer `state_change_seq`.
    pub fn set_status(&self, pane_id: &str, status: AgentStatus) {
        self.set_status_silently(pane_id, status);
        let ws = pane_id.split(':').next().unwrap_or("w1").to_string();
        let _ = self.events.send(Event {
            event: "pane.agent_status_changed".into(),
            data: json!({"pane_id": pane_id, "workspace_id": ws, "agent_status": status}),
        });
    }

    /// Change an agent's status without publishing an event, the way a change
    /// looks to a client whose event stream fell behind: only `agent.list`
    /// shows it.
    pub fn set_status_silently(&self, pane_id: &str, status: AgentStatus) {
        change_status(&mut self.state.lock().unwrap(), pane_id, status);
    }

    /// The pane goes away, as when a human closes it. Its workspace had only
    /// this pane, so it closes too, the way herdr closes a workspace whose
    /// last pane closes.
    /// The pane closes just before the next `events.subscribe` that names it
    /// arrives, so that subscribe is refused: the race of a pane that goes
    /// away between the `agent.list` that found it and the subscription.
    pub fn close_pane_before_subscribe(&self, pane_id: &str) {
        self.state.lock().unwrap().close_before_subscribe = Some(pane_id.into());
    }

    /// The agent on `pane_id` changes to `status`, silently, just before the
    /// next `agent.list` that follows another `agent.list` within 50ms with
    /// nothing in between: a reconcile's list and the fresh one of the
    /// auto-close that runs right after it, never two reconciles, which are
    /// `reconcile_every` apart. One-shot.
    pub fn set_status_between_lists(&self, pane_id: &str, status: AgentStatus) {
        self.state.lock().unwrap().status_between_lists = Some((pane_id.into(), status));
    }

    /// Does the fake have this pane: a workspace's root pane, or one with an
    /// agent on it.
    fn has_pane(s: &State, pane_id: &str) -> bool {
        let ws = pane_id.split(':').next().unwrap_or("");
        s.agents.contains_key(pane_id)
            || (pane_id.ends_with(":p1") && s.workspaces.contains_key(ws))
    }

    pub fn close_pane(&self, pane_id: &str) {
        let ws = pane_id.split(':').next().unwrap_or("w1").to_string();
        {
            let mut s = self.state.lock().unwrap();
            s.agents.remove(pane_id);
            s.workspaces.remove(&ws);
        }
        self.pane_closed_event(pane_id, &ws);
    }

    fn pane_closed_event(&self, pane_id: &str, ws: &str) {
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
        {
            let mut s = self.state.lock().unwrap();
            s.requests.push(req.clone());
            if req.method == "agent.list" {
                let back_to_back = s.requests.len() >= 2
                    && s.requests[s.requests.len() - 2].method == "agent.list"
                    && s.last_list
                        .is_some_and(|at| at.elapsed() < Duration::from_millis(50));
                if back_to_back && let Some((pane, status)) = s.status_between_lists.take() {
                    change_status(&mut s, &pane, status);
                }
                s.last_list = Some(Instant::now());
            }
            if let Some(path) = &s.request_log {
                write_request_log(path, &s.requests);
            }
        }
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
            let named = |pane: &str| subs.iter().any(|sub| sub["pane_id"] == pane);
            let vanish = {
                let mut s = self.state.lock().unwrap();
                match s.close_before_subscribe.take() {
                    Some(pane) if named(&pane) => Some(pane),
                    other => {
                        s.close_before_subscribe = other;
                        None
                    }
                }
            };
            if let Some(pane) = vanish {
                self.close_pane(&pane);
            }
            // herdr 0.9.1 refuses a subscription to a pane it does not have
            // under `<id>:sub:<index>:probe`, then closes the connection.
            let refused = {
                let s = self.state.lock().unwrap();
                subs.iter().enumerate().find_map(|(i, sub)| {
                    let pane = sub["pane_id"].as_str()?;
                    (!Self::has_pane(&s, pane)).then(|| (i, pane.to_string()))
                })
            };
            if let Some((i, pane)) = refused {
                let err = json!({"id": format!("{}:sub:{i}:probe", req.id), "error": {"code": "pane_not_found", "message": format!("pane {pane} not found")}});
                let _ = writer.write_all(format!("{err}\n").as_bytes()).await;
                return;
            }
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
                let is_worktree = req.method == "worktree.create";
                s.workspaces.insert(ws.clone(), is_worktree);
                if is_worktree && s.all_dirty {
                    s.dirty.insert(ws.clone());
                }
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
                let known = matches!((ws_num, pane_part), (Some(n), Some("p1")) if n >= 1 && n <= s.next_ws)
                    && s.workspaces.contains_key(ws_part);
                if !known {
                    return Err(("pane_not_found".into(), pane_id));
                }
                if s.pane_busy_for > 0 {
                    s.pane_busy_for -= 1;
                    return Err((
                        "agent_pane_busy".into(),
                        format!("agent target pane {pane_id} is not an available shell"),
                    ));
                }
                let ws = ws_part.to_string();
                match s.start.clone().unwrap_or(StartBehaviour::Ready) {
                    StartBehaviour::Fail(code) => return Err((code, "start failed".into())),
                    StartBehaviour::Ready => {}
                }
                // The launch itself (unknown -> idle) is a state change.
                s.seq += 1;
                let info = AgentInfo {
                    pane_id: pane_id.clone(),
                    workspace_id: ws.clone(),
                    tab_id: format!("{ws}:t1"),
                    name: p["name"].as_str().map(str::to_string),
                    agent: p["kind"].as_str().map(str::to_string),
                    agent_status: AgentStatus::Idle,
                    completion_seq: None,
                    state_change_seq: s.seq,
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
                if s.ignore_prompts {
                    return Ok(json!({"type": "agent_prompted", "agent": found}));
                }
                let info = change_status(&mut s, &found.pane_id, AgentStatus::Working)
                    .expect("found above, under the same lock");
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
            // herdr 0.9.1: `pane.close {pane_id}` answers `{"type": "ok"}`, and
            // closing a workspace's last pane closes the workspace.
            "pane.close" => {
                let pane_id = p["pane_id"].as_str().unwrap_or("").to_string();
                let ws = pane_id.split(':').next().unwrap_or("").to_string();
                if !pane_id.ends_with(":p1") || s.workspaces.remove(&ws).is_none() {
                    return Err(("pane_not_found".into(), format!("pane {pane_id} not found")));
                }
                s.agents.remove(&pane_id);
                s.dirty.remove(&ws);
                drop(s);
                self.pane_closed_event(&pane_id, &ws);
                Ok(json!({"type": "ok"}))
            }
            // herdr 0.9.1: `worktree.remove {workspace_id, force}` deletes the
            // checkout and closes its workspace. An unknown workspace is
            // `workspace_not_found`, a plain one `not_linked_worktree`,
            // uncommitted changes without `force` `dirty_worktree_requires_force`.
            "worktree.remove" => {
                let ws = p["workspace_id"].as_str().unwrap_or("").to_string();
                let force = p["force"].as_bool().unwrap_or(false);
                match s.workspaces.get(&ws) {
                    None => {
                        return Err((
                            "workspace_not_found".into(),
                            format!("workspace {ws} not found"),
                        ));
                    }
                    Some(false) => {
                        return Err((
                            "not_linked_worktree".into(),
                            "workspace is not a Herdr-managed worktree checkout".into(),
                        ));
                    }
                    Some(true) => {}
                }
                if s.dirty.contains(&ws) && !force {
                    return Err((
                        "dirty_worktree_requires_force".into(),
                        "contains modified or untracked files, use --force to delete it".into(),
                    ));
                }
                s.workspaces.remove(&ws);
                s.dirty.remove(&ws);
                let pane_id = format!("{ws}:p1");
                s.agents.remove(&pane_id);
                drop(s);
                self.pane_closed_event(&pane_id, &ws);
                Ok(
                    json!({"type": "worktree_removed", "workspace_id": ws, "forced": force, "path": format!("/fake/{ws}")}),
                )
            }
            other => Err(("unsupported_method".into(), other.into())),
        }
    }
}

impl super::transport::Connector for FakeHerdr {
    fn connect(&self) -> super::transport::ConnectFuture<'_> {
        Box::pin(async move {
            self.wait_unwedged();
            Ok(FakeHerdr::connect(self))
        })
    }
    fn describe(&self) -> String {
        "fake herdr".into()
    }
    fn host(&self) -> String {
        "fake".into()
    }
    fn home_dir(&self) -> super::transport::HomeFuture<'_> {
        let home = self.state.lock().unwrap().home.clone();
        Box::pin(async move { Ok(home) })
    }
    fn dir_exists(&self, path: &str) -> super::transport::DirFuture<'_> {
        let exists = !self.state.lock().unwrap().missing_dirs.contains(path);
        Box::pin(async move { Ok(Some(exists)) })
    }
    fn pastor_version(&self) -> super::transport::VersionFuture<'_> {
        let version = self.state.lock().unwrap().pastor_version.clone();
        Box::pin(async move { Ok(version) })
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

fn write_request_log(path: &std::path::Path, requests: &[Request]) {
    let tmp = path.with_extension("tmp");
    let text = serde_json::to_string(requests).expect("requests serialise");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
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
    async fn pane_close_and_worktree_remove() {
        let fake = FakeHerdr::new();
        let plain = fake.workspace_create(None, "t-1").await.unwrap();
        let wt = fake.worktree_create("/r", "b", "t-2").await.unwrap();
        let dirty = fake.worktree_create("/r", "c", "t-3").await.unwrap();
        fake.agent_start("t-1", "claude", &plain.root_pane.pane_id, &[])
            .await
            .unwrap();
        fake.set_dirty(&dirty.workspace.workspace_id);
        let mut stream = fake
            .subscribe(vec![super::super::subscription_lifecycle("pane.closed")])
            .await
            .unwrap();

        fake.pane_close(&plain.root_pane.pane_id).await.unwrap();
        assert!(fake.agents().is_empty());
        let ev = stream.next().await.unwrap();
        assert!(ev.is_pane_closed());
        assert_eq!(ev.pane_id(), Some(plain.root_pane.pane_id.as_str()));
        let err = fake.pane_close(&plain.root_pane.pane_id).await.unwrap_err();
        assert_eq!(err.code(), Some("pane_not_found"));

        let err = fake
            .worktree_remove(&plain.workspace.workspace_id, false)
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some("workspace_not_found"), "closed already");
        let other = fake.workspace_create(None, "t-4").await.unwrap();
        let err = fake
            .worktree_remove(&other.workspace.workspace_id, false)
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some("not_linked_worktree"));
        fake.pane_close(&other.root_pane.pane_id).await.unwrap();
        fake.worktree_remove(&wt.workspace.workspace_id, false)
            .await
            .unwrap();
        let err = fake
            .worktree_remove(&dirty.workspace.workspace_id, false)
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some("dirty_worktree_requires_force"));
        fake.worktree_remove(&dirty.workspace.workspace_id, true)
            .await
            .unwrap();
        assert!(fake.workspaces().is_empty());
        assert_eq!(
            fake.requests()
                .iter()
                .filter(|r| r.method == "worktree.remove")
                .map(|r| r.params["force"].as_bool())
                .collect::<Vec<_>>(),
            [
                Some(false),
                Some(false),
                Some(false),
                Some(false),
                Some(true)
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
        fake.set_status(&created.root_pane.pane_id, AgentStatus::Blocked);
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
        fake.set_status(&b.root_pane.pane_id, AgentStatus::Blocked); // filtered out
        fake.set_status(&a.root_pane.pane_id, AgentStatus::Working);
        fake.close_pane(&b.root_pane.pane_id);
        let e1 = stream.next().await.unwrap();
        assert_eq!(e1.pane_id(), Some(a.root_pane.pane_id.as_str()));
        assert_eq!(e1.agent_status(), Some(AgentStatus::Working));
        let e2 = stream.next().await.unwrap();
        assert!(e2.is_pane_closed());
        assert_eq!(e2.pane_id(), Some(b.root_pane.pane_id.as_str()));
    }

    /// The request log is written when a request is received, not when its
    /// connection ends: `events.subscribe` holds its connection open for as
    /// long as the subscription lives, and a hung request never ends at all.
    /// As herdr 0.9.1 does: a subscription to a pane it does not have is an
    /// API error, and `close_pane_before_subscribe` makes a known pane one.
    #[tokio::test]
    async fn subscribe_refuses_a_pane_it_does_not_have() {
        let fake = FakeHerdr::new();
        let Err(err) = fake
            .subscribe(vec![super::super::subscription_agent_status("w9:p1")])
            .await
        else {
            panic!("subscribed to a missing pane")
        };
        assert_eq!(err.code(), Some("pane_not_found"), "{err:?}");

        let ws = fake.workspace_create(None, "x").await.unwrap();
        let pane = ws.root_pane.pane_id;
        fake.close_pane_before_subscribe(&pane);
        fake.subscribe(vec![super::super::subscription_lifecycle("pane.closed")])
            .await
            .expect("a subscribe that does not name the pane is untouched");
        let Err(err) = fake
            .subscribe(vec![super::super::subscription_agent_status(&pane)])
            .await
        else {
            panic!("subscribed to a pane that closed")
        };
        assert_eq!(err.code(), Some("pane_not_found"), "{err:?}");
        assert!(fake.workspaces().is_empty());
    }

    #[tokio::test]
    async fn request_log_lists_a_subscribe_while_it_is_still_open() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("requests.json");
        let fake = FakeHerdr::new();
        fake.set_request_log(path.clone());
        let _stream = fake
            .connect()
            .subscribe(vec![super::super::subscription_lifecycle("pane.closed")])
            .await
            .unwrap();
        let logged: Vec<Request> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let methods: Vec<&str> = logged.iter().map(|r| r.method.as_str()).collect();
        assert_eq!(methods, ["events.subscribe"]);
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
