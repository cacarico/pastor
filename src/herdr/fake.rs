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

/// What a pane was sent through `pane.send_text` or `pane.send_keys`, in
/// the order it arrived. See `FakeHerdr::pane_input`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneInput {
    Text(String),
    Keys(Vec<String>),
}

#[derive(Default)]
struct State {
    next_ws: u32,
    /// Open workspaces: id -> created by `worktree.create`. Each opens with
    /// one pane, `<id>:p1`, as herdr gives a new workspace.
    workspaces: HashMap<String, bool>,
    /// Open workspace -> its open panes, in order. `pane.split` adds one;
    /// `pane.close` of the last closes the workspace, as in herdr.
    panes: HashMap<String, Vec<String>>,
    /// Workspace id -> its label, as `workspace.list` reports it. Ids are
    /// never reused, so a closed workspace's entry is harmless.
    labels: HashMap<String, String>,
    /// Workspace id -> the directory `workspace.list` reports as its
    /// checkout: the `cwd` it was created with, or its worktree's path. The
    /// fake treats every directory as a git checkout.
    dirs: HashMap<String, String>,
    /// The `env` each pane was created with (`workspace.create`, `pane.split`).
    pane_env: HashMap<String, Value>,
    /// Worktree workspaces whose checkout has changes, so `worktree.remove`
    /// needs `force`.
    dirty: HashSet<String>,
    /// Every new worktree, and every workspace opened on one, starts dirty
    /// (see `dirty_worktrees`).
    all_dirty: bool,
    /// Worktree checkouts on disk, as (repo cwd, branch) -> path. They outlive
    /// their workspace, as git worktrees do: only `worktree.remove` deletes one.
    checkouts: HashMap<(String, String), String>,
    /// Open worktree workspace -> the checkout (repo cwd, branch) it shows.
    workspace_checkout: HashMap<String, (String, String)>,
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
    /// A method name whose next reply parses as JSON-RPC but whose `result`
    /// decodes into nothing pastor expects: a garbled response, not herdr
    /// refusing the call. See `set_malformed_reply`.
    malformed_reply: Option<String>,
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
    /// Checkouts, by path, holding commits that are on no remote: what
    /// `Connector::unpushed_commits` reports.
    unpushed: HashSet<String>,
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
    /// pane id -> what `agent.read` shows of it (`set_pane_text`).
    pane_text: HashMap<String, String>,
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
    /// Everything typed into each pane, by pane id.
    pane_input: HashMap<String, Vec<PaneInput>>,
    /// A started agent sits `blocked` on its folder-trust question until
    /// `pane.send_keys` sends exactly these keys (see `set_trust_prompt`).
    trust_prompt: Option<Vec<String>>,
    /// How long after its trust question is answered an agent still redraws:
    /// `agent.prompt` is accepted in that window but the text is lost, as
    /// Claude loses input typed while it replaces the dialog with its prompt
    /// box. herdr already reports it idle and ready by then. Zero by default.
    trust_redraw: Duration,
    /// pane id -> when its trust question was answered.
    trust_answered: HashMap<String, Instant>,
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
    /// The checkout at `path` has commits on no remote.
    pub fn set_unpushed(&self, path: &str) {
        self.state.lock().unwrap().unpushed.insert(path.to_string());
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
    /// The next request for `method` gets a reply that is valid JSON-RPC but
    /// whose `result` cannot be decoded into the type the caller expects, as
    /// a truncated or version-skewed herdr response might. One-shot, like
    /// `hang_method`. Lets tests exercise a decoding failure apart from an
    /// API error (which carries a code) or a transport outage.
    pub fn set_malformed_reply(&self, method: &str) {
        self.state.lock().unwrap().malformed_reply = Some(method.into());
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
    /// Open a workspace the way someone at the machine would, with a label
    /// and a directory, as `workspace.create` does. Answers its id.
    pub fn open_user_workspace(&self, label: &str, dir: Option<&str>) -> String {
        let mut s = self.state.lock().unwrap();
        s.next_ws += 1;
        let ws = format!("w{}", s.next_ws);
        Self::open_workspace(&mut s, &ws, false);
        s.labels.insert(ws.clone(), label.into());
        if let Some(dir) = dir {
            s.dirs.insert(ws.clone(), dir.into());
        }
        ws
    }
    /// The open panes of workspace `ws`, in order; empty once it is closed.
    pub fn panes(&self, ws: &str) -> Vec<String> {
        let s = self.state.lock().unwrap();
        s.panes.get(ws).cloned().unwrap_or_default()
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

    /// Everything `pane.send_text` and `pane.send_keys` delivered to this
    /// pane, in order.
    pub fn pane_input(&self, pane_id: &str) -> Vec<PaneInput> {
        self.state
            .lock()
            .unwrap()
            .pane_input
            .get(pane_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Agents started from now on stop at a folder-trust question: `blocked`
    /// until `pane.send_keys` sends exactly `keys` to their pane, then idle
    /// and ready, as Claude is once its trust dialog is answered. `None`
    /// turns it off.
    pub fn set_trust_prompt(&self, keys: Option<Vec<String>>) {
        self.state.lock().unwrap().trust_prompt = keys;
    }

    /// Prompts sent within `d` of the trust answer are accepted and lost (see
    /// `State::trust_redraw`).
    pub fn set_trust_redraw(&self, d: Duration) {
        self.state.lock().unwrap().trust_redraw = d;
    }

    /// `agent.prompt` is accepted from now on, but the agent never starts
    /// working on it, as if the text never reached it.
    pub fn ignore_prompts(&self, yes: bool) {
        self.state.lock().unwrap().ignore_prompts = yes;
    }

    /// What `agent.read` answers for the agent in `pane_id` from now on,
    /// instead of `fake output`.
    pub fn set_pane_text(&self, pane_id: &str, text: &str) {
        self.state
            .lock()
            .unwrap()
            .pane_text
            .insert(pane_id.into(), text.into());
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
        s.agents.contains_key(pane_id) || Self::pane_open(s, pane_id)
    }

    fn pane_open(s: &State, pane_id: &str) -> bool {
        let ws = pane_id.split(':').next().unwrap_or("");
        s.panes
            .get(ws)
            .is_some_and(|p| p.iter().any(|x| x == pane_id))
    }

    /// Open workspace `ws` with its root pane.
    fn open_workspace(s: &mut State, ws: &str, is_worktree: bool) {
        s.workspaces.insert(ws.to_string(), is_worktree);
        s.panes.insert(ws.to_string(), vec![format!("{ws}:p1")]);
    }

    /// The env the pane was created with, `Null` for none: what a test
    /// checks to see that an agent definition's env reached its pane.
    pub fn pane_env(&self, pane_id: &str) -> Value {
        let s = self.state.lock().unwrap();
        s.pane_env.get(pane_id).cloned().unwrap_or(Value::Null)
    }

    pub fn close_pane(&self, pane_id: &str) {
        let ws = pane_id.split(':').next().unwrap_or("w1").to_string();
        {
            let mut s = self.state.lock().unwrap();
            s.agents.remove(pane_id);
            s.workspaces.remove(&ws);
            s.panes.remove(&ws);
            // The checkout stays on disk.
            s.workspace_checkout.remove(&ws);
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
        let malformed = {
            let mut s = self.state.lock().unwrap();
            if s.malformed_reply.as_deref() == Some(req.method.as_str()) {
                // One-shot: only this one request gets the bad reply.
                s.malformed_reply = None;
                true
            } else {
                false
            }
        };
        if malformed {
            // A reply herdr's API server never sends for real, but a decoding
            // bug or protocol skew could hand pastor: syntactically fine,
            // shaped nothing like the result any caller decodes.
            let reply = json!({"id": req.id, "result": {"malformed": true}});
            let _ = send(&mut writer, &reply).await;
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
                let _ = send(&mut writer, &err).await;
                return;
            }
            let mut rx = self.events.subscribe();
            let ack = json!({"id": req.id, "result": {"type": "subscription_started"}});
            if send(&mut writer, &ack).await.is_err() {
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
                        let _ = send(&mut writer, &err).await;
                        return;
                    }
                    Err(_) => return,
                };
                if subscription_matches(&subs, &ev) {
                    let line = serde_json::to_string(&ev).unwrap();
                    if send(&mut writer, &line).await.is_err() {
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
        let _ = send(&mut writer, &reply).await;
        // Returning here drops `writer`, closing the connection: one reply is all
        // a herdr connection ever carries.
    }

    fn handle(&self, req: &Request) -> Result<Value, (String, String)> {
        let mut s = self.state.lock().unwrap();
        let p = &req.params;
        match req.method.as_str() {
            "ping" => Ok(json!({"type": "pong", "version": "fake", "protocol": s.protocol})),
            "workspace.create" | "worktree.create" => {
                let checkout = (
                    p["cwd"].as_str().unwrap_or("").to_string(),
                    p["branch"].as_str().unwrap_or("").to_string(),
                );
                if req.method == "worktree.create" {
                    // herdr passes git's own refusal through.
                    if let Some(path) = s.checkouts.get(&checkout) {
                        return Err((
                            "worktree_create_failed".into(),
                            format!("fatal: '{path}' already exists"),
                        ));
                    }
                    let path = format!("/fake/worktrees/{}", checkout.1.replace('/', "-"));
                    s.checkouts.insert(checkout.clone(), path);
                }
                s.next_ws += 1;
                let ws = format!("w{}", s.next_ws);
                let dir = if req.method == "worktree.create" {
                    s.checkouts.get(&checkout).cloned()
                } else {
                    p["cwd"].as_str().map(str::to_string)
                };
                if let Some(dir) = dir {
                    s.dirs.insert(ws.clone(), dir);
                }
                if let Some(label) = p["label"].as_str() {
                    s.labels.insert(ws.clone(), label.to_string());
                }
                let pane = format!("{ws}:p1");
                let is_worktree = req.method == "worktree.create";
                Self::open_workspace(&mut s, &ws, is_worktree);
                if let Some(env) = p.get("env").filter(|e| !e.is_null()) {
                    s.pane_env.insert(pane.clone(), env.clone());
                }
                if is_worktree {
                    s.workspace_checkout.insert(ws.clone(), checkout);
                }
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
            // herdr 0.9.1: `worktree.list {cwd}` lists the repo's checkouts,
            // each with the workspace showing it, if one is open.
            "worktree.list" => {
                let cwd = p["cwd"].as_str().unwrap_or("");
                let worktrees: Vec<Value> = s
                    .checkouts
                    .iter()
                    .filter(|((repo, _), _)| repo == cwd)
                    .map(|((repo, branch), path)| {
                        let key = (repo.clone(), branch.clone());
                        let open = s
                            .workspace_checkout
                            .iter()
                            .find(|(_, c)| **c == key)
                            .map(|(ws, _)| ws.clone());
                        json!({"branch": branch, "path": path, "open_workspace_id": open,
                               "is_bare": false, "is_detached": false, "is_prunable": false,
                               "is_linked_worktree": true, "label": "r"})
                    })
                    .collect();
                Ok(json!({"type": "worktree_list", "worktrees": worktrees}))
            }
            // herdr 0.9.1: `worktree.open {cwd, branch, label}` opens a
            // workspace on an existing checkout, or answers the one already
            // showing it with `already_open`.
            "worktree.open" => {
                let checkout = (
                    p["cwd"].as_str().unwrap_or("").to_string(),
                    p["branch"].as_str().unwrap_or("").to_string(),
                );
                let Some(path) = s.checkouts.get(&checkout).cloned() else {
                    return Err((
                        "worktree_not_found".into(),
                        format!("no worktree for branch {}", checkout.1),
                    ));
                };
                let open = s
                    .workspace_checkout
                    .iter()
                    .find(|(_, c)| **c == checkout)
                    .map(|(ws, _)| ws.clone());
                let already_open = open.is_some();
                let ws = match open {
                    Some(ws) => ws,
                    None => {
                        s.next_ws += 1;
                        let ws = format!("w{}", s.next_ws);
                        Self::open_workspace(&mut s, &ws, true);
                        s.workspace_checkout.insert(ws.clone(), checkout.clone());
                        s.dirs.insert(ws.clone(), path.clone());
                        if s.all_dirty {
                            s.dirty.insert(ws.clone());
                        }
                        if let Some(label) = p["label"].as_str() {
                            s.labels.insert(ws.clone(), label.to_string());
                        }
                        ws
                    }
                };
                let label = p.get("label").cloned().unwrap_or(Value::Null);
                // An open workspace answers with a pane it still has: its
                // first may have been closed for a split (see `dispatch`).
                let root = s
                    .panes
                    .get(&ws)
                    .and_then(|p| p.first().cloned())
                    .unwrap_or_else(|| format!("{ws}:p1"));
                Ok(
                    json!({"type": "worktree_opened", "already_open": already_open,
                    "workspace": {"workspace_id": ws, "label": label}, "tab": {"tab_id": format!("{ws}:t1")},
                    "root_pane": {"pane_id": root, "workspace_id": ws},
                    "worktree": {"path": path, "branch": checkout.1}}),
                )
            }
            // herdr 0.9.1: `workspace.list` answers every open workspace;
            // one whose directory is a git checkout carries `worktree` with
            // its `checkout_path`.
            "workspace.list" => {
                let mut ids: Vec<&String> = s.workspaces.keys().collect();
                ids.sort_by_key(|ws| ws[1..].parse::<u32>().unwrap_or(0));
                let workspaces: Vec<Value> = ids
                    .into_iter()
                    .map(|ws| {
                        let mut w = json!({"workspace_id": ws, "label": s.labels.get(ws),
                            "pane_count": s.panes.get(ws).map_or(0, Vec::len)});
                        if let Some(dir) = s.dirs.get(ws) {
                            w["worktree"] = json!({"checkout_path": dir,
                                "is_linked_worktree": s.workspaces[ws]});
                        }
                        w
                    })
                    .collect();
                Ok(json!({"type": "workspace_list", "workspaces": workspaces}))
            }
            // herdr 0.9.1: `pane.list {workspace_id}` answers that
            // workspace's panes, `workspace_not_found` for one it lacks.
            "pane.list" => {
                let ws = p["workspace_id"].as_str().unwrap_or("");
                let Some(panes) = s.panes.get(ws) else {
                    return Err((
                        "workspace_not_found".into(),
                        format!("workspace {ws} not found"),
                    ));
                };
                let panes: Vec<Value> = panes
                    .iter()
                    .map(|p| json!({"pane_id": p, "workspace_id": ws}))
                    .collect();
                Ok(json!({"type": "pane_list", "panes": panes}))
            }
            "agent.start" => {
                let pane_id = p["pane_id"].as_str().unwrap_or("").to_string();
                // A pane is known only if a workspace was created with it or
                // split it off, and it is still open. A lookup, never a
                // slice, so non-ASCII input cannot panic with `state` held.
                if !Self::pane_open(&s, &pane_id) {
                    return Err(("pane_not_found".into(), pane_id));
                }
                if s.pane_busy_for > 0 {
                    s.pane_busy_for -= 1;
                    return Err((
                        "agent_pane_busy".into(),
                        format!("agent target pane {pane_id} is not an available shell"),
                    ));
                }
                let ws = pane_id.split(':').next().unwrap_or("").to_string();
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
                s.agents.insert(pane_id.clone(), info.clone());
                if s.trust_prompt.is_some() {
                    // Its first screen is the trust question, which herdr
                    // reports as `blocked`.
                    change_status(&mut s, &pane_id, AgentStatus::Blocked);
                }
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
                let redrawing = s
                    .trust_answered
                    .get(&found.pane_id)
                    .is_some_and(|at| at.elapsed() < s.trust_redraw);
                if s.ignore_prompts || redrawing {
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
            // herdr 0.9.1: `pane.send_text {pane_id, text}` and
            // `pane.send_keys {pane_id, keys}` answer `{"type": "ok"}`.
            "pane.send_text" | "pane.send_keys" => {
                let pane_id = p["pane_id"].as_str().unwrap_or("").to_string();
                if !Self::has_pane(&s, &pane_id) {
                    return Err(("pane_not_found".into(), format!("pane {pane_id} not found")));
                }
                let input = if req.method == "pane.send_text" {
                    PaneInput::Text(p["text"].as_str().unwrap_or("").to_string())
                } else {
                    let keys: Vec<String> = p["keys"]
                        .as_array()
                        .map(|ks| {
                            ks.iter()
                                .filter_map(|k| k.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default();
                    if keys.is_empty() || keys.iter().any(String::is_empty) {
                        return Err((
                            "invalid_keys".into(),
                            "no keys, or an empty key name".into(),
                        ));
                    }
                    PaneInput::Keys(keys)
                };
                let answered = matches!(&input, PaneInput::Keys(k) if s.trust_prompt.as_ref() == Some(k))
                    && s.agents
                        .get(&pane_id)
                        .is_some_and(|a| a.agent_status == AgentStatus::Blocked);
                s.pane_input.entry(pane_id.clone()).or_default().push(input);
                if answered {
                    s.trust_answered.insert(pane_id.clone(), Instant::now());
                    change_status(&mut s, &pane_id, AgentStatus::Idle);
                    let ws = pane_id.split(':').next().unwrap_or("w1").to_string();
                    drop(s);
                    let _ = self.events.send(Event {
                        event: "pane.agent_status_changed".into(),
                        data: json!({"pane_id": pane_id, "workspace_id": ws, "agent_status": "idle"}),
                    });
                }
                Ok(json!({"type": "ok"}))
            }
            "agent.read" => {
                let target = p["target"].as_str().unwrap_or("");
                let text = s
                    .agents
                    .values()
                    .find(|a| a.name.as_deref() == Some(target) || a.pane_id == target)
                    .and_then(|a| s.pane_text.get(&a.pane_id))
                    .map_or("fake output\n", String::as_str);
                Ok(json!({"type": "pane_read", "read": {"text": text}}))
            }
            // herdr 0.9.1: `pane.split {target_pane_id, direction, cwd, env}`
            // answers `pane_info` with the new pane, in the target's workspace.
            "pane.split" => {
                let target = p["target_pane_id"].as_str().unwrap_or("").to_string();
                if !Self::pane_open(&s, &target) {
                    return Err(("pane_not_found".into(), format!("pane {target} not found")));
                }
                let ws = target.split(':').next().unwrap_or("").to_string();
                let open = s.panes.get_mut(&ws).expect("pane_open checked it");
                let pane = (2..)
                    .map(|n| format!("{ws}:p{n}"))
                    .find(|id| !open.contains(id))
                    .unwrap();
                open.push(pane.clone());
                if let Some(env) = p.get("env").filter(|e| !e.is_null()) {
                    s.pane_env.insert(pane.clone(), env.clone());
                }
                Ok(
                    json!({"type": "pane_info", "pane": {"pane_id": pane, "workspace_id": ws,
                    "cwd": p.get("cwd").cloned().unwrap_or(Value::Null), "agent_status": "unknown"}}),
                )
            }
            // herdr 0.9.1: `pane.close {pane_id}` answers `{"type": "ok"}`, and
            // closing a workspace's last pane closes the workspace.
            "pane.close" => {
                let pane_id = p["pane_id"].as_str().unwrap_or("").to_string();
                let ws = pane_id.split(':').next().unwrap_or("").to_string();
                if !Self::pane_open(&s, &pane_id) {
                    return Err(("pane_not_found".into(), format!("pane {pane_id} not found")));
                }
                s.agents.remove(&pane_id);
                let left = s.panes.get_mut(&ws).map(|panes| {
                    panes.retain(|x| *x != pane_id);
                    panes.len()
                });
                if left == Some(0) {
                    s.panes.remove(&ws);
                    s.workspaces.remove(&ws);
                    s.dirty.remove(&ws);
                    s.workspace_checkout.remove(&ws);
                }
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
                if let Some(checkout) = s.workspace_checkout.remove(&ws) {
                    s.checkouts.remove(&checkout);
                }
                let panes = s.panes.remove(&ws).unwrap_or_default();
                for pane_id in &panes {
                    s.agents.remove(pane_id);
                }
                drop(s);
                for pane_id in &panes {
                    self.pane_closed_event(pane_id, &ws);
                }
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
    fn unpushed_commits(&self, path: &str) -> super::transport::DirFuture<'_> {
        let unpushed = self.state.lock().unwrap().unpushed.contains(path);
        Box::pin(async move { Ok(Some(unpushed)) })
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

/// Writes one line of the protocol and flushes it. The stdio fake's writer is
/// tokio's stdout, which accepts a write before the bytes reach the pipe (a
/// blocking-pool thread does that later) and drops them if the process exits
/// first. Without the flush, a busy machine turns a reply into a clean EOF.
async fn send(writer: &mut BoxWrite, line: &impl std::fmt::Display) -> std::io::Result<()> {
    writer.write_all(format!("{line}\n").as_bytes()).await?;
    writer.flush().await
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
        let created = fake
            .workspace_create(Some("/tmp"), "t-1", &Default::default())
            .await
            .unwrap();
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
        let plain = fake
            .workspace_create(None, "t-1", &Default::default())
            .await
            .unwrap();
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
        let other = fake
            .workspace_create(None, "t-4", &Default::default())
            .await
            .unwrap();
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
        let created = fake
            .workspace_create(None, "x", &Default::default())
            .await
            .unwrap();
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
        let created = fake
            .workspace_create(None, "x", &Default::default())
            .await
            .unwrap();
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
        let a = fake
            .workspace_create(None, "a", &Default::default())
            .await
            .unwrap();
        let b = fake
            .workspace_create(None, "b", &Default::default())
            .await
            .unwrap();
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

        let ws = fake
            .workspace_create(None, "x", &Default::default())
            .await
            .unwrap();
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

    /// Stands in for `tokio::io::stdout`: a write is accepted at once but only
    /// reaches the peer on flush, and dropping the writer unflushed loses it.
    #[tokio::test]
    async fn a_reply_reaches_a_writer_that_only_delivers_on_flush() {
        use tokio::io::AsyncBufReadExt;
        let fake = FakeHerdr::new();
        let (a, b) = tokio::io::duplex(64 * 1024);
        let (ar, mut aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        aw.write_all(b"{\"id\":\"1\",\"method\":\"ping\",\"params\":{}}\n")
            .await
            .unwrap();
        // BufWriter does not flush on drop, which is the stdout behaviour that
        // matters here: an unflushed reply is simply gone.
        let writer = tokio::io::BufWriter::new(bw);
        fake.serve(Box::new(br), Box::new(writer)).await;
        let mut line = String::new();
        BufReader::new(ar).read_line(&mut line).await.unwrap();
        assert!(line.contains("pong"), "reply lost: {line:?}");
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
        let created = fake
            .workspace_create(None, "t-1", &Default::default())
            .await
            .unwrap();
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
        let created = fake
            .workspace_create(None, "t-1", &Default::default())
            .await
            .unwrap();
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
        let created = fake
            .workspace_create(None, "t-1", &Default::default())
            .await
            .unwrap();
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
        let created = fake
            .workspace_create(None, "t-1", &Default::default())
            .await
            .unwrap();
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
        fake.workspace_create(None, "x", &Default::default())
            .await
            .unwrap();
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

    #[tokio::test]
    async fn pane_input_is_recorded_per_pane_in_order() {
        let fake = FakeHerdr::new();
        let created = fake
            .workspace_create(None, "x", &Default::default())
            .await
            .unwrap();
        let pane = created.root_pane.pane_id;
        fake.pane_send_text(&pane, "yes please").await.unwrap();
        fake.pane_send_keys(&pane, &["Down".into(), "Enter".into()])
            .await
            .unwrap();
        assert_eq!(
            fake.pane_input(&pane),
            [
                PaneInput::Text("yes please".into()),
                PaneInput::Keys(vec!["Down".into(), "Enter".into()]),
            ]
        );
        let sent: Vec<_> = fake
            .requests()
            .into_iter()
            .filter(|r| r.method.starts_with("pane.send_"))
            .map(|r| (r.method, r.params))
            .collect();
        assert_eq!(
            sent,
            [
                (
                    "pane.send_text".to_string(),
                    json!({"pane_id": pane, "text": "yes please"})
                ),
                (
                    "pane.send_keys".to_string(),
                    json!({"pane_id": pane, "keys": ["Down", "Enter"]})
                ),
            ]
        );
    }

    #[tokio::test]
    async fn pane_input_to_an_unknown_pane_or_with_no_keys_is_refused() {
        let fake = FakeHerdr::new();
        let err = fake.pane_send_text("w9:p1", "hi").await.unwrap_err();
        assert_eq!(err.code(), Some("pane_not_found"));
        let created = fake
            .workspace_create(None, "x", &Default::default())
            .await
            .unwrap();
        let err = fake
            .pane_send_keys(&created.root_pane.pane_id, &[])
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some("invalid_keys"));
        assert!(fake.pane_input(&created.root_pane.pane_id).is_empty());
    }

    /// The folder-trust case: a started agent sits `blocked` on its startup
    /// question until it gets the keys that answer it.
    #[tokio::test]
    async fn a_trust_prompt_holds_the_agent_blocked_until_its_keys_arrive() {
        let fake = FakeHerdr::new();
        fake.set_trust_prompt(Some(vec!["Down".into(), "Enter".into()]));
        let created = fake
            .workspace_create(None, "x", &Default::default())
            .await
            .unwrap();
        let pane = created.root_pane.pane_id;
        fake.agent_start("t-1", "claude", &pane, &[]).await.unwrap();
        assert_eq!(
            fake.agent_list().await.unwrap()[0].agent_status,
            AgentStatus::Blocked
        );
        fake.pane_send_keys(&pane, &["Enter".into()]).await.unwrap();
        assert_eq!(
            fake.agent_list().await.unwrap()[0].agent_status,
            AgentStatus::Blocked
        );
        fake.pane_send_keys(&pane, &["Down".into(), "Enter".into()])
            .await
            .unwrap();
        let a = &fake.agent_list().await.unwrap()[0];
        assert_eq!(a.agent_status, AgentStatus::Idle);
        assert!(a.interactive_ready);
    }

    /// A prompt sent while the agent redraws after its trust answer is
    /// accepted and lost; one sent after it lands.
    #[tokio::test]
    async fn a_prompt_right_after_the_trust_answer_is_lost() {
        let fake = FakeHerdr::new();
        fake.set_trust_prompt(Some(vec!["Enter".into()]));
        fake.set_trust_redraw(Duration::from_millis(100));
        let created = fake
            .workspace_create(None, "x", &Default::default())
            .await
            .unwrap();
        let pane = created.root_pane.pane_id;
        fake.agent_start("t-1", "claude", &pane, &[]).await.unwrap();
        fake.pane_send_keys(&pane, &["Enter".into()]).await.unwrap();
        fake.agent_prompt("t-1", "go").await.unwrap();
        assert_eq!(
            fake.agent_list().await.unwrap()[0].agent_status,
            AgentStatus::Idle
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
        fake.agent_prompt("t-1", "go").await.unwrap();
        assert_eq!(
            fake.agent_list().await.unwrap()[0].agent_status,
            AgentStatus::Working
        );
    }
}
