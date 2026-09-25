use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::MIN_HERDR_PROTOCOL;
use crate::dispatch::dispatch;
use crate::herdr::{
    AgentInfo, AgentStatus, CallError, Connector, ConnectorExt, EventStream,
    subscription_agent_status, subscription_lifecycle,
};
use crate::store::Store;
use crate::task::{Observed, Task, TaskState, next_state};

/// A herdr request that got no answer within `request_timeout`.
#[derive(Debug, thiserror::Error)]
#[error("{0} timed out after {1:?}")]
struct TimedOut(&'static str, Duration);

/// Whether an error from the connected loop means the machine is gone. Only a
/// transport failure or a request that never answered does; a herdr API error
/// or a local store error leaves the machine reachable, and reporting it as
/// `machine.lost` would announce an outage that is not happening.
fn is_outage(err: &anyhow::Error) -> bool {
    err.chain().any(|e| {
        e.is::<TimedOut>()
            || e.downcast_ref::<CallError>()
                .is_some_and(CallError::is_transport)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelState {
    Connecting,
    Connected,
    Reconnecting,
    /// Requests answer but `events.subscribe` will not open: tasks are tracked
    /// by `agent.list` every `poll_every` until a subscribe succeeds.
    Polling,
    Incompatible,
}

impl ChannelState {
    /// May the dispatcher place a task here? Only states in which requests are
    /// known to answer.
    pub fn accepts_dispatch(&self) -> bool {
        matches!(self, ChannelState::Connected | ChannelState::Polling)
    }
}

impl std::fmt::Display for ChannelState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ChannelState::Connecting => "connecting",
            ChannelState::Connected => "connected",
            ChannelState::Reconnecting => "reconnecting",
            ChannelState::Polling => "polling",
            ChannelState::Incompatible => "incompatible",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineStatus {
    pub name: String,
    /// `Connector::host`. Defaulted so a CLI can still read a head that
    /// predates the field.
    #[serde(default)]
    pub host: String,
    pub endpoint: String,
    pub channel: ChannelState,
    pub herdr_version: Option<String>,
    /// `Connector::pastor_version`, asked once per connect. Defaulted so a
    /// CLI can still read a head that predates the field.
    #[serde(default)]
    pub pastor_version: Option<String>,
    pub protocol: Option<u32>,
    pub error: Option<String>,
    pub live: usize,
    pub max_agents: u32,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct MachineSettings {
    pub settle: Duration,
    pub reconcile_every: Duration,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    /// Bound on any single herdr request (dispatch, read, ...), covering the
    /// connect as well as the reply. A machine that does not answer within this
    /// window is treated as lost: the request fails and the actor reconnects.
    pub request_timeout: Duration,
    /// How long a dispatch waits between `agent.start` and a prompt herdr
    /// accepts. Must stay below `request_timeout`, which bounds the dispatch as
    /// a whole: otherwise a slow agent surfaces as "request timed out" (and a
    /// reconnect) instead of the readiness failure it is.
    pub agent_ready_timeout: Duration,
    /// While `Polling`, how often `agent.list` reconciles. The daemon passes its
    /// `tick`, the spec's "each tick for a polling machine".
    pub poll_every: Duration,
}

impl Default for MachineSettings {
    fn default() -> Self {
        MachineSettings {
            settle: Duration::from_secs(10),
            reconcile_every: Duration::from_secs(60),
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
            request_timeout: Duration::from_secs(60),
            agent_ready_timeout: Duration::from_secs(30),
            poll_every: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PastorEvent {
    pub kind: String,
    #[serde(default)]
    pub task_id: Option<i64>,
    #[serde(default)]
    pub machine: Option<String>,
    #[serde(default)]
    pub job: Option<String>,
}

pub enum MachineCommand {
    Dispatch {
        task_id: i64,
        reply: oneshot::Sender<anyhow::Result<Task>>,
    },
    Read {
        task_id: i64,
        lines: u32,
        reply: oneshot::Sender<anyhow::Result<String>>,
    },
}

#[derive(Clone)]
pub struct MachineHandle {
    pub name: String,
    pub max_agents: u32,
    pub tags: Vec<String>,
    pub tx: mpsc::Sender<MachineCommand>,
    pub status: Arc<RwLock<MachineStatus>>,
}

impl MachineHandle {
    pub fn snapshot(&self) -> MachineStatus {
        self.status.read().unwrap().clone()
    }

    pub async fn dispatch(&self, task_id: i64) -> anyhow::Result<Task> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(MachineCommand::Dispatch { task_id, reply })
            .await
            .map_err(|_| anyhow::anyhow!("machine {} is gone", self.name))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("machine {} dropped the request", self.name))?
    }

    pub async fn read(&self, task_id: i64, lines: u32) -> anyhow::Result<String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(MachineCommand::Read {
                task_id,
                lines,
                reply,
            })
            .await
            .map_err(|_| anyhow::anyhow!("machine {} is gone", self.name))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("machine {} dropped the request", self.name))?
    }
}

pub fn spawn_machine(
    name: String,
    max_agents: u32,
    tags: Vec<String>,
    connector: Arc<dyn Connector>,
    store: Arc<Store>,
    settings: MachineSettings,
    events: broadcast::Sender<PastorEvent>,
) -> MachineHandle {
    let (tx, rx) = mpsc::channel(32);
    let status = Arc::new(RwLock::new(MachineStatus {
        name: name.clone(),
        host: connector.host(),
        endpoint: connector.describe(),
        channel: ChannelState::Connecting,
        herdr_version: None,
        pastor_version: None,
        protocol: None,
        error: None,
        live: 0,
        max_agents,
        tags: tags.clone(),
    }));
    let actor = Actor {
        name: name.clone(),
        connector,
        store,
        settings,
        events,
        status: status.clone(),
        rx,
        pending_done: HashMap::new(),
        activity_seen: HashSet::new(),
        was_connected: false,
        failures: 0,
        lost_announced: false,
    };
    tokio::spawn(actor.run());
    MachineHandle {
        name,
        max_agents,
        tags,
        tx,
        status,
    }
}

struct Actor {
    name: String,
    connector: Arc<dyn Connector>,
    store: Arc<Store>,
    settings: MachineSettings,
    events: broadcast::Sender<PastorEvent>,
    status: Arc<RwLock<MachineStatus>>,
    rx: mpsc::Receiver<MachineCommand>,
    /// task id -> (the agent's `state_change_seq` when it was seen idle, if the
    /// observation carried one; when). Confirmed as Done after `settle` if the
    /// agent is still idle at that same sequence; with no sequence, the first
    /// check records one and starts the window over.
    pending_done: HashMap<i64, (Option<u64>, Instant)>,
    /// Tasks whose agent this actor has seen `working` or `blocked` since the
    /// prompt went in or the task last finished (`Task::activity_seen`). Kept
    /// here, not in the store: `apply` copies it onto the task before each
    /// transition. A restarted daemon starts empty; reconcile refills it for
    /// agents it finds at work.
    activity_seen: HashSet<i64>,
    /// Has the actor connected successfully at least once (ever)?
    was_connected: bool,
    /// Consecutive connect-attempt failures since the last success. Only decides
    /// the cold-start case in `announce_lost`; once `was_connected` is true a
    /// single failure is enough.
    failures: u32,
    /// Set once `machine.lost` has been emitted for the outage in progress, so it
    /// is never repeated; cleared by `announce_connected`.
    lost_announced: bool,
}

/// What the inner loop should do after a command: nothing, reopen the event
/// subscription (the tracked pane set changed), or drop everything and reconnect
/// (a request failed below the API, so the machine looks lost).
enum CommandOutcome {
    Nothing,
    Resubscribe,
    Reconnect,
}

/// A pending `events.subscribe` call, boxed so `poll_until_subscribed` can hold
/// it across `select!` iterations without keeping `&self` (or `&mut self`)
/// borrowed for as long as the call takes to answer.
type SubscribeFuture = Pin<Box<dyn Future<Output = anyhow::Result<EventStream>> + Send>>;

/// How `poll_until_subscribed` ended: a fresh subscription, a request that
/// failed below the API while polling (carries the reason, same as
/// `connect_failed`'s message), or the command channel closing — the handle
/// was dropped, so nothing is asking about this machine any more and the actor
/// should stop instead of spinning trying to reconnect it.
enum PollExit {
    Subscribed(Box<EventStream>),
    Reconnect(String),
    Shutdown,
}

impl Actor {
    /// The machine's pastor version, for `machine list` only. The ping just
    /// proved the machine reachable, so a failure here, even an ssh one, is
    /// logged and read as unknown rather than failing the connect: the next
    /// request finds out soon enough if the machine really went away.
    async fn ask_pastor_version(&self) -> Option<String> {
        match tokio::time::timeout(
            self.settings.request_timeout,
            self.connector.pastor_version(),
        )
        .await
        {
            Ok(Ok(v)) => v,
            Ok(Err(err)) => {
                tracing::warn!(machine = %self.name, %err, "could not ask for the pastor version");
                None
            }
            Err(_) => {
                tracing::warn!(machine = %self.name, "pastor version check timed out");
                None
            }
        }
    }

    async fn run(mut self) {
        let mut backoff = self.settings.initial_backoff;
        loop {
            self.set_channel(ChannelState::Connecting, None);
            // Nothing is held open for requests: a ping is an ordinary call, on a
            // connection of its own, and it is what proves the machine reachable.
            let pong =
                match tokio::time::timeout(self.settings.request_timeout, self.connector.ping())
                    .await
                {
                    Ok(Ok(p)) => p,
                    Ok(Err(err)) => {
                        self.connect_failed(err.to_string(), &mut backoff).await;
                        continue;
                    }
                    // A ping that never answers is exactly as dead as one that errors: a
                    // herdr that accepted the connection and then stopped answering must
                    // not hang the actor (and with it `Daemon::dispatch_queued` and the
                    // accept loop) forever.
                    Err(_) => {
                        self.connect_failed(
                            format!("ping timed out after {:?}", self.settings.request_timeout),
                            &mut backoff,
                        )
                        .await;
                        continue;
                    }
                };
            let pastor_version = self.ask_pastor_version().await;
            {
                let mut s = self.status.write().unwrap();
                s.herdr_version = Some(pong.version.clone());
                s.pastor_version = pastor_version;
                s.protocol = Some(pong.protocol);
            }
            if pong.protocol < MIN_HERDR_PROTOCOL {
                self.set_channel(
                    ChannelState::Incompatible,
                    Some(format!(
                        "herdr protocol {} is older than {MIN_HERDR_PROTOCOL}; update herdr on this machine",
                        pong.protocol
                    )),
                );
                self.drain_commands_while_down(self.settings.max_backoff)
                    .await;
                continue;
            }
            if let Err(err) = self.reconcile().await {
                if is_outage(&err) {
                    self.connect_failed(format!("reconcile: {err}"), &mut backoff)
                        .await;
                    continue;
                }
                tracing::warn!(machine = %self.name, %err, "reconcile failed; staying connected");
            }
            let mut events = match self.open_events().await {
                Ok(s) => s,
                Err(err) => {
                    // ping and reconcile just succeeded: requests answer, only
                    // the subscription is missing. That is a machine to poll,
                    // not one to declare lost.
                    tracing::warn!(machine = %self.name, %err, "events will not open; polling");
                    self.set_channel(ChannelState::Polling, Some(format!("events: {err}")));
                    self.announce_connected();
                    self.refresh_live();
                    match self.poll_until_subscribed().await {
                        PollExit::Subscribed(s) => *s,
                        PollExit::Shutdown => return,
                        PollExit::Reconnect(reason) => {
                            // A request failed while polling: the machine is gone
                            // after all. Fall through to the reconnect path.
                            self.set_channel(ChannelState::Reconnecting, Some(reason));
                            self.announce_lost();
                            self.drain_commands_while_down(backoff).await;
                            backoff = (backoff * 2).min(self.settings.max_backoff);
                            continue;
                        }
                    }
                }
            };
            // Connected. `backoff` is deliberately left alone here: a flappy
            // connection that immediately breaks again must keep backing off, not
            // busy-spin at `initial_backoff` every time a connect attempt happens
            // to briefly succeed. It resets only once the connection proves itself
            // stable (below) or after a clean, command-driven resubscribe.
            self.set_channel(ChannelState::Connected, None);
            self.announce_connected();
            self.refresh_live();

            let connected_at = Instant::now();
            let mut settle_tick =
                tokio::time::interval(Duration::from_millis(50).max(self.settings.settle / 4));
            let mut reconcile_tick = tokio::time::interval(self.settings.reconcile_every);
            reconcile_tick.tick().await; // first tick fires immediately; we just reconciled
            loop {
                tokio::select! {
                    cmd = self.rx.recv() => {
                        let Some(cmd) = cmd else { return };
                        match self.handle_command(cmd).await {
                            CommandOutcome::Nothing => {}
                            CommandOutcome::Resubscribe => {
                                match self.open_events().await {
                                    Ok(s) => {
                                        events = s;
                                        // A fresh subscribe just succeeded: proof of health.
                                        backoff = self.settings.initial_backoff;
                                    }
                                    Err(err) => { tracing::warn!(machine = %self.name, %err, "resubscribe failed"); break; }
                                }
                            }
                            CommandOutcome::Reconnect => {
                                tracing::warn!(machine = %self.name, "a request failed at the transport level; reconnecting");
                                break;
                            }
                        }
                    }
                    ev = events.next() => match ev {
                        Ok(ev) => {
                            if let Err(err) = self.handle_event(&ev).await {
                                if is_outage(&err) { tracing::warn!(machine = %self.name, %err, "event handling failed"); break; }
                                tracing::warn!(machine = %self.name, %err, "event handling failed; staying connected");
                            }
                        }
                        Err(err) => { tracing::warn!(machine = %self.name, %err, "event stream ended"); break; }
                    },
                    _ = settle_tick.tick() => {
                        if let Err(err) = self.confirm_pending_done().await {
                            if is_outage(&err) { tracing::warn!(machine = %self.name, %err, "settle check failed"); break; }
                            tracing::warn!(machine = %self.name, %err, "settle check failed; staying connected");
                        }
                    }
                    _ = reconcile_tick.tick() => {
                        if let Err(err) = self.reconcile().await {
                            if is_outage(&err) { tracing::warn!(machine = %self.name, %err, "reconcile failed"); break; }
                            tracing::warn!(machine = %self.name, %err, "reconcile failed; staying connected");
                        }
                    }
                }
            }
            self.set_channel(ChannelState::Reconnecting, Some("connection lost".into()));
            if connected_at.elapsed() >= self.settings.max_backoff {
                // Lived long enough to count as a real recovery, not a flap: start
                // the next outage's backoff from the floor again.
                backoff = self.settings.initial_backoff;
            }
            self.announce_lost();
            self.drain_commands_while_down(backoff).await;
            backoff = (backoff * 2).min(self.settings.max_backoff);
        }
    }

    async fn connect_failed(&mut self, message: String, backoff: &mut Duration) {
        self.failures += 1;
        tracing::warn!(machine = %self.name, %message, "connect failed");
        self.set_channel(ChannelState::Reconnecting, Some(message));
        self.announce_lost();
        self.drain_commands_while_down(*backoff).await;
        *backoff = (*backoff * 2).min(self.settings.max_backoff);
    }

    /// Emits `machine.lost` once per outage: immediately if the machine was
    /// connected before, otherwise only after two consecutive failures from a
    /// cold start. Stays quiet for the rest of the outage; `announce_connected`
    /// clears the flag on recovery.
    fn announce_lost(&mut self) {
        if self.lost_announced {
            return;
        }
        if self.was_connected || self.failures >= 2 {
            self.emit("machine.lost", None);
            self.lost_announced = true;
        }
    }

    /// Emits `machine.connected` only when recovering from an announced outage,
    /// never on the very first connect.
    fn announce_connected(&mut self) {
        if self.lost_announced {
            self.emit("machine.connected", None);
        }
        self.lost_announced = false;
        self.was_connected = true;
        self.failures = 0;
    }

    /// While down, answer commands with an error instead of leaving callers hanging.
    async fn drain_commands_while_down(&mut self, wait: Duration) {
        let deadline = tokio::time::sleep(wait);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => return,
                cmd = self.rx.recv() => match cmd {
                    None => return,
                    Some(MachineCommand::Dispatch { reply, .. }) => { let _ = reply.send(Err(anyhow::anyhow!("machine {} is not connected", self.name))); }
                    Some(MachineCommand::Read { reply, .. }) => { let _ = reply.send(Err(anyhow::anyhow!("machine {} is not connected", self.name))); }
                },
            }
        }
    }

    fn set_channel(&self, channel: ChannelState, error: Option<String>) {
        let mut s = self.status.write().unwrap();
        s.channel = channel;
        s.error = error;
    }

    fn refresh_live(&self) {
        match self.store.tasks_on_machine(&self.name) {
            Ok(v) => self.status.write().unwrap().live = v.len(),
            Err(err) => {
                // A store error is not "zero live agents": that reads as idle
                // capacity and would let the picker over-dispatch. Leave the
                // previous count as-is and only log.
                tracing::error!(machine = %self.name, %err, "refresh_live: store error, keeping previous live count");
            }
        }
    }

    /// Task events carry the task's job, read from its row, so a consumer that
    /// only filters by job (hooks, `pastor events`) needs no store lookup of its
    /// own. A row that cannot be read leaves `job` empty rather than dropping
    /// the event.
    fn emit(&self, kind: &str, task_id: Option<i64>) {
        let job = task_id.and_then(|id| match self.store.get_task(id) {
            Ok(t) => t.map(|t| t.job),
            Err(err) => {
                tracing::warn!(machine = %self.name, %err, id, "emit: cannot read task row");
                None
            }
        });
        tracing::info!(machine = %self.name, kind, ?task_id, ?job, "event");
        let _ = self.events.send(PastorEvent {
            kind: kind.into(),
            task_id,
            machine: Some(self.name.clone()),
            job,
        });
    }

    /// Builds the `events.subscribe` call (bounded by `request_timeout`) as a
    /// free-standing future that owns everything it needs, rather than
    /// borrowing `self`: `poll_until_subscribed` races it against commands and
    /// poll ticks in a `select!`, so it must be possible to hold one in flight
    /// across many loop iterations without tying up the actor for as long as a
    /// wedged herdr leaves it hanging. Building `subs` (a store read) is kept
    /// synchronous and done eagerly, before the future is returned.
    fn subscribe_future(&self) -> anyhow::Result<SubscribeFuture> {
        let mut subs = vec![
            subscription_lifecycle("pane.closed"),
            subscription_lifecycle("pane.exited"),
        ];
        for t in self.store.tasks_on_machine(&self.name)? {
            if let Some(p) = &t.pane_id {
                subs.push(subscription_agent_status(p));
            }
        }
        let connector = self.connector.clone();
        let timeout = self.settings.request_timeout;
        Ok(Box::pin(async move {
            Ok(tokio::time::timeout(timeout, connector.subscribe(subs))
                .await
                .map_err(|_| TimedOut("events.subscribe", timeout))??)
        }))
    }

    /// The one connection pastor keeps open: herdr dedicates it to events and
    /// never serves a request on it.
    ///
    /// The subscribe is bounded here rather than at each call site, so a herdr
    /// that accepts the connection and never acknowledges the subscription is
    /// treated like any other wedged request: both callers turn the error into a
    /// reconnect with backoff.
    async fn open_events(&self) -> Result<EventStream, anyhow::Error> {
        self.subscribe_future()?.await
    }

    /// The `Polling` loop: serve commands, reconcile every `poll_every`, retry
    /// the subscription on its own backoff (independent of `run`'s reconnect
    /// `backoff`; see the comment where `PollExit::Reconnect` is handled).
    ///
    /// The subscribe attempt itself is never awaited inline: it is started as
    /// a free-standing future (`subscribe_future`) and raced in the `select!`
    /// below alongside commands and poll ticks. Polling claims to accept
    /// dispatch (`ChannelState::Polling.accepts_dispatch()`), so a herdr that
    /// accepts `events.subscribe` and then never acknowledges it must not
    /// block a dispatch reply for up to `request_timeout` — that would stall
    /// every machine behind a fleet-wide dispatch lock, not just this one.
    async fn poll_until_subscribed(&mut self) -> PollExit {
        let mut poll = tokio::time::interval(self.settings.poll_every);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        poll.tick().await; // fires immediately; we reconciled a moment ago

        // The subscribe retry delay is local to this loop: it must not leak
        // into `run`'s `backoff`, which times the reconnect path and is left
        // exactly as `run` already manages it once polling ends.
        let mut retry_delay = self.settings.initial_backoff;
        let retry = tokio::time::sleep(retry_delay);
        tokio::pin!(retry);
        let mut subscribing: Option<SubscribeFuture> = None;

        loop {
            tokio::select! {
                cmd = self.rx.recv() => {
                    let Some(cmd) = cmd else { return PollExit::Shutdown };
                    match self.handle_command(cmd).await {
                        CommandOutcome::Nothing => {}
                        CommandOutcome::Resubscribe => {
                            // The tracked pane set changed (a dispatch added one).
                            // If nothing is in flight, the pending retry timer
                            // covers it: `subscribe_future` reads the store fresh
                            // whenever it is called, so the next attempt already
                            // includes the new pane. But an attempt already in
                            // flight was built from the pane set as it stood
                            // before this command; left alone, it would land the
                            // machine on `Connected` with a subscription missing
                            // the new pane, seen only at the next `reconcile_tick`
                            // (60s default). Drop it and start a fresh one now.
                            if subscribing.is_some() {
                                subscribing = None;
                                match self.subscribe_future() {
                                    Ok(fut) => subscribing = Some(fut),
                                    Err(err) => {
                                        retry_delay = (retry_delay * 2).min(self.settings.max_backoff);
                                        tracing::debug!(machine = %self.name, %err, next_in = ?retry_delay, "could not prepare the subscribe request");
                                        retry.as_mut().reset(tokio::time::Instant::now() + retry_delay);
                                    }
                                }
                            }
                        }
                        CommandOutcome::Reconnect => {
                            let reason = "a request failed at the transport level while polling".to_string();
                            tracing::warn!(machine = %self.name, "{reason}");
                            return PollExit::Reconnect(reason);
                        }
                    }
                }
                _ = poll.tick() => {
                    // Same rule as the connected loop: only an outage (transport
                    // failure or a request that never answered) means the machine
                    // is gone. A herdr API error or a store error is logged and
                    // the next tick tries again.
                    if let Err(err) = self.reconcile().await {
                        tracing::warn!(machine = %self.name, %err, "poll reconcile failed");
                        if is_outage(&err) {
                            return PollExit::Reconnect(format!("poll reconcile failed: {err}"));
                        }
                    }
                    if let Err(err) = self.confirm_pending_done().await {
                        tracing::warn!(machine = %self.name, %err, "settle check failed");
                        if is_outage(&err) {
                            return PollExit::Reconnect(format!("settle check failed: {err}"));
                        }
                    }
                }
                // Only starts a new attempt once the previous one (if any) is
                // done: `subscribing` already holds one in flight otherwise.
                _ = &mut retry, if subscribing.is_none() => {
                    match self.subscribe_future() {
                        Ok(fut) => subscribing = Some(fut),
                        Err(err) => {
                            retry_delay = (retry_delay * 2).min(self.settings.max_backoff);
                            tracing::debug!(machine = %self.name, %err, next_in = ?retry_delay, "could not prepare the subscribe request");
                            retry.as_mut().reset(tokio::time::Instant::now() + retry_delay);
                        }
                    }
                }
                res = async { subscribing.as_mut().unwrap().await }, if subscribing.is_some() => {
                    subscribing = None;
                    match res {
                        Ok(s) => return PollExit::Subscribed(Box::new(s)),
                        Err(err) => {
                            retry_delay = (retry_delay * 2).min(self.settings.max_backoff);
                            tracing::debug!(machine = %self.name, %err, next_in = ?retry_delay, "subscribe still failing");
                            retry.as_mut().reset(tokio::time::Instant::now() + retry_delay);
                        }
                    }
                }
            }
        }
    }

    async fn handle_command(&mut self, cmd: MachineCommand) -> CommandOutcome {
        match cmd {
            MachineCommand::Dispatch { task_id, reply } => {
                let (result, dead) = self.run_dispatch(task_id).await;
                let changed = result.is_ok();
                self.refresh_live();
                let _ = reply.send(result);
                match (dead, changed) {
                    (true, _) => CommandOutcome::Reconnect,
                    (false, true) => CommandOutcome::Resubscribe,
                    (false, false) => CommandOutcome::Nothing,
                }
            }
            MachineCommand::Read {
                task_id,
                lines,
                reply,
            } => {
                let (result, dead) = match self.store.get_task(task_id) {
                    Ok(Some(t))
                        if t.machine.as_deref() == Some(&self.name) && t.state.occupies_pane() =>
                    {
                        let timeout = self.settings.request_timeout;
                        match tokio::time::timeout(
                            timeout,
                            self.connector
                                .agent_read(t.agent_name.as_deref().unwrap_or(""), lines),
                        )
                        .await
                        {
                            Ok(Ok(text)) => (Ok(text), false),
                            Ok(Err(err)) => {
                                let dead = err.is_transport();
                                (Err(err.into()), dead)
                            }
                            // A request that never answers within the bound is treated as a
                            // dead machine: the actor reconnects and backs off instead of
                            // hammering an apparently wedged herdr.
                            Err(_) => (
                                Err(anyhow::anyhow!("request timed out after {timeout:?}")),
                                true,
                            ),
                        }
                    }
                    Ok(_) => (
                        Err(anyhow::anyhow!(
                            "task {task_id} has no live agent on {}",
                            self.name
                        )),
                        false,
                    ),
                    Err(e) => (Err(e), false),
                };
                let _ = reply.send(result);
                if dead {
                    CommandOutcome::Reconnect
                } else {
                    CommandOutcome::Nothing
                }
            }
        }
    }

    /// Runs a dispatch and reports whether it failed below the API (a transport
    /// error, not a herdr error code): the caller must then treat the machine as
    /// lost and reconnect, not just resubscribe. The task's own outcome
    /// (including `Failed`, as `dispatch()` records it) is left to the store.
    async fn run_dispatch(&mut self, task_id: i64) -> (anyhow::Result<Task>, bool) {
        // The claim is the `Queued -> Starting` transition done as a conditional
        // UPDATE: a task another pass already took, or that finished meanwhile,
        // is simply not claimable. It also persists `machine` and `agent_name`
        // before the first herdr call, so a daemon crash mid-dispatch leaves a
        // row `reconcile` can adopt by agent name instead of one that still reads
        // `Queued` and gets dispatched twice.
        let mut task = match self.store.claim_task(task_id, &self.name) {
            Ok(Some(t)) => t,
            Ok(None) => {
                return (
                    Err(anyhow::anyhow!(
                        "task t-{task_id} is not queued (already claimed, finished, or unknown)"
                    )),
                    false,
                );
            }
            Err(e) => return (Err(e), false),
        };
        let timeout = self.settings.request_timeout;
        let outcome = match tokio::time::timeout(
            timeout,
            dispatch(
                self.connector.as_ref(),
                &mut task,
                self.settings.agent_ready_timeout,
            ),
        )
        .await
        {
            Ok(outcome) => outcome,
            // `dispatch()` itself never got to resolve, so its own Failed-recording
            // never ran; do the same bookkeeping it would have done on an error, and
            // force a reconnect: a herdr that stops answering is a machine that is gone.
            Err(_) => {
                let message = format!("request timed out after {timeout:?}");
                task.state = TaskState::Failed;
                task.error = Some(message.clone());
                task.finished_at = Some(Utc::now());
                if let Err(err) = self.store.update_task(&mut task) {
                    return (Err(err), true);
                }
                self.emit("task.failed", Some(task.id));
                return (
                    Err(anyhow::anyhow!("dispatch {}: {message}", task.display_id())),
                    true,
                );
            }
        };
        let dead = matches!(&outcome, Err(err) if err.is_transport());
        self.set_activity(&task);
        if let Err(err) = self.store.update_task(&mut task) {
            return (Err(err), dead);
        }
        match outcome {
            Ok(_) => {
                self.emit(&format!("task.{}", task.state), Some(task.id));
                (Ok(task), dead)
            }
            Err(err) => {
                self.emit("task.failed", Some(task.id));
                (
                    Err(anyhow::anyhow!("dispatch {}: {err}", task.display_id())),
                    dead,
                )
            }
        }
    }

    async fn handle_event(&mut self, ev: &crate::herdr::Event) -> anyhow::Result<()> {
        let Some(pane_id) = ev.pane_id() else {
            return Ok(());
        };
        let Ok(Some(task)) = self.store.find_by_pane(&self.name, pane_id) else {
            return Ok(());
        };
        let observed = if ev.is_pane_closed() {
            Observed::PaneClosed
        } else if ev.is_pane_exited() {
            Observed::PaneExited
        } else if let Some(status) = ev.agent_status() {
            // Subscription events carry no sequence; treat idle as a candidate and let
            // the settle check read the real one from agent.list.
            Observed::Status {
                status,
                state_change_seq: None,
                completion_seq: None,
            }
        } else {
            return Ok(());
        };
        if let Observed::Status { status, .. } = &observed
            && task.prompt_pending
            && self.deliver_pending_prompt(task.clone(), *status).await?
        {
            return Ok(());
        }
        match &observed {
            Observed::Status {
                status: crate::herdr::AgentStatus::Idle | crate::herdr::AgentStatus::Done,
                ..
            } => {
                // A fresh window on every idle event: an agent that went back to
                // work in between sent a working event, which cleared the old one.
                self.pending_done.insert(task.id, (None, Instant::now()));
            }
            _ => {
                self.pending_done.remove(&task.id);
                self.apply(task, &observed);
            }
        }
        Ok(())
    }

    /// Send the prompt the agent has not seen yet (see `Task::prompt_pending`)
    /// once it has left `blocked` or its launch. Returns whether it dealt with
    /// `status`; `false` leaves the observation to the usual state machine.
    ///
    /// Without this, a human who clears the agent's startup question sees the
    /// task flip to `running` while the agent sits idle, never having received
    /// the work it was started for.
    async fn deliver_pending_prompt(
        &mut self,
        task: Task,
        status: AgentStatus,
    ) -> anyhow::Result<bool> {
        if matches!(status, AgentStatus::Blocked | AgentStatus::Unknown) {
            return Ok(false);
        }
        let name = task
            .agent_name
            .clone()
            .unwrap_or_else(|| Task::agent_name_for(task.id));
        let timeout = self.settings.request_timeout;
        let result =
            tokio::time::timeout(timeout, self.connector.agent_prompt(&name, &task.prompt))
                .await
                .map_err(|_| TimedOut("agent.prompt", timeout))?;
        let id = task.id;
        // Only a row still waiting for this prompt, on a live pane, takes the
        // outcome. A row closed or failed meanwhile is left as it is.
        let still_pending = |t: &Task| t.prompt_pending && t.state.occupies_pane();
        let written = match result {
            Ok(agent) => write_task(&self.store, task, |t| {
                if !still_pending(t) {
                    return false;
                }
                t.prompt_pending = false;
                t.state = TaskState::Running;
                t.error = None;
                t.finished_at = None;
                // State changes before the agent had our prompt (its launch,
                // its startup question) are not this task's work: count from here.
                t.last_completion_seq = Some(agent.state_change_seq);
                // Set on the fresh row too: a re-read row never carries it.
                t.activity_seen = agent.agent_status.is_activity();
                true
            })?,
            // Blocked again, or between states: the next status event or
            // reconcile tries again.
            Err(err) if matches!(err.code(), Some("agent_blocked" | "agent_not_ready")) => {
                return Ok(true);
            }
            Err(err) if err.is_transport() => return Err(err.into()),
            Err(err) => {
                let message = format!("could not send the pending prompt: {err}");
                let now = Utc::now();
                write_task(&self.store, task, |t| {
                    if !still_pending(t) {
                        return false;
                    }
                    t.state = TaskState::Failed;
                    t.error = Some(message.clone());
                    t.finished_at = Some(now);
                    t.activity_seen = false;
                    true
                })?
            }
        };
        self.pending_done.remove(&id);
        // A row left alone keeps whatever the actor knew of it: nothing it saw
        // while the prompt was pending counted as activity (see `apply`).
        if let Some(t) = written {
            self.set_activity(&t);
            self.emit(&format!("task.{}", t.state), Some(t.id));
        }
        self.refresh_live();
        Ok(true)
    }

    /// After the settle window, confirm with agent.list that the agent is still idle, at
    /// the `state_change_seq` it had when it was seen going idle, and that the sequence
    /// is past the task's baseline (`next_state`). Only then is the task done. A newer
    /// sequence while still idle means it worked again in between, unseen by events
    /// (a stream that fell behind, or polling): the window starts over. So does an
    /// unknown one (an idle event carries none): the window restarts from the
    /// sequence listed now, so a done task always sat a full window at one sequence.
    async fn confirm_pending_done(&mut self) -> anyhow::Result<()> {
        let due: Vec<i64> = self
            .pending_done
            .iter()
            .filter(|(_, (_, at))| at.elapsed() >= self.settings.settle)
            .map(|(id, _)| *id)
            .collect();
        if due.is_empty() {
            return Ok(());
        }
        let timeout = self.settings.request_timeout;
        let agents = tokio::time::timeout(timeout, self.connector.agent_list())
            .await
            .map_err(|_| TimedOut("agent.list", timeout))??;
        for id in due {
            let Some((seen_seq, _)) = self.pending_done.remove(&id) else {
                continue;
            };
            let Ok(Some(task)) = self.store.get_task(id) else {
                continue;
            };
            let Some(agent) = agents
                .iter()
                .find(|a| Some(&a.pane_id) == task.pane_id.as_ref())
            else {
                continue;
            };
            let idle_like = matches!(agent.agent_status, AgentStatus::Idle | AgentStatus::Done);
            // No sequence yet (the candidate came from an event) is as good as a
            // moved one: the agent may have worked again inside the window.
            if idle_like && seen_seq != Some(agent.state_change_seq) {
                self.pending_done
                    .insert(id, (Some(agent.state_change_seq), Instant::now()));
                continue;
            }
            self.apply(task, &observed_from(agent));
        }
        Ok(())
    }

    /// Record whether `task` (just prompted, so its flag is fresh) has shown
    /// activity; see `Task::activity_seen`.
    fn set_activity(&mut self, task: &Task) {
        if task.activity_seen {
            self.activity_seen.insert(task.id);
        } else {
            self.activity_seen.remove(&task.id);
        }
    }

    fn apply(&mut self, mut task: Task, observed: &Observed) {
        // Any `working` or `blocked` after the prompt is activity, whether an
        // event or `agent.list` showed it; `unknown` never is. Before the
        // prompt reached the agent (`prompt_pending`) nothing counts.
        if let Observed::Status { status, .. } = observed
            && status.is_activity()
            && !task.prompt_pending
        {
            self.activity_seen.insert(task.id);
        }
        task.activity_seen = self.activity_seen.contains(&task.id);
        let Some(to) = next_state(&task, observed) else {
            return;
        };
        if to == TaskState::Done || !to.is_open() {
            // The next completion needs activity of its own.
            self.activity_seen.remove(&task.id);
        }
        if let Observed::Status {
            state_change_seq,
            completion_seq,
            ..
        } = observed
            && to == TaskState::Done
        {
            // The next completion of this task (a done agent that is given
            // more to do, say) has to move past this one.
            task.last_completion_seq = completion_seq.or(*state_change_seq);
        }
        let from = task.state;
        task.state = to;
        if matches!(to, TaskState::Done | TaskState::Failed | TaskState::Closed) {
            task.finished_at = Some(Utc::now());
        } else {
            // The task is open again (a `Done` agent that picked the work back
            // up, say): the old finish time belongs to a cycle that is over, and
            // leaving it there reads as "finished at ..." on a running task.
            task.finished_at = None;
        }
        if from == TaskState::Blocked && matches!(to, TaskState::Running | TaskState::Done) {
            // "agent blocked during startup; answer its prompt" was advice about
            // a state the task has left; it is not an error on a running task.
            task.error = None;
        }
        if to == TaskState::Failed && task.error.is_none() {
            task.error = Some("agent process exited".into());
        }
        if let Err(err) = self.store.update_task(&mut task) {
            tracing::error!(%err, "update task");
            return;
        }
        self.emit(&format!("task.{to}"), Some(task.id));
        self.refresh_live();
    }

    /// Compare open tasks with live agents. Missing agent means the task failed while we
    /// were away; a present agent's status is applied like an event, except idle, which
    /// goes through the settle window. Long-running tasks become stale.
    async fn reconcile(&mut self) -> anyhow::Result<()> {
        let timeout = self.settings.request_timeout;
        let agents: Vec<AgentInfo> = tokio::time::timeout(timeout, self.connector.agent_list())
            .await
            .map_err(|_| TimedOut("agent.list", timeout))??;
        for task in self.store.tasks_on_machine(&self.name)? {
            let Some(pane_id) = task.pane_id.clone() else {
                // Only a `Starting` task can occupy a pane slot with no pane recorded
                // (see `TaskState::occupies_pane`): `run_dispatch` persists `Starting`
                // with `machine` set before its first herdr call, so a daemon crash
                // mid-dispatch leaves exactly this row behind. Adopt the agent it
                // must have started, found by the name dispatch gives it (`t-<id>`);
                // if none exists the dispatch never got that far and the task failed.
                let name = Task::agent_name_for(task.id);
                let mut t = task;
                match agents
                    .iter()
                    .find(|a| a.name.as_deref() == Some(name.as_str()))
                {
                    Some(agent) => {
                        t.pane_id = Some(agent.pane_id.clone());
                        t.workspace_id = Some(agent.workspace_id.clone());
                        // Set from the same name just matched on, not read back from
                        // the row: adoption must not depend on `agent_name` already
                        // being persisted (belt and suspenders alongside
                        // `run_dispatch` persisting it up front; see the comment
                        // there).
                        t.agent_name = Some(name.clone());
                        // Whether dispatch got as far as the prompt is unknown, so
                        // count from now: only work seen after adoption completes it.
                        // An agent that had already finished is left to go stale.
                        t.last_completion_seq = Some(agent.state_change_seq);
                        // Still launching, it cannot have taken the prompt: its launch
                        // ending moves the sequence past the baseline above, which
                        // must not read as this task done. The pending delivery sends
                        // the prompt once it is ready and resets the baseline.
                        if agent.launch_pending {
                            t.prompt_pending = true;
                        }
                        let idle_like = matches!(
                            agent.agent_status,
                            crate::herdr::AgentStatus::Idle | crate::herdr::AgentStatus::Done
                        );
                        if idle_like {
                            // Adopting proves dispatch reached at least `agent.start`;
                            // land the task at `Running` immediately, the same target
                            // a successful dispatch would have recorded, instead of
                            // applying the agent's raw idle/done status straight
                            // through: that could mark it `Done` with no settle
                            // window, or leave it stuck as `Starting` forever
                            // otherwise (stale only covers Running/Blocked). An
                            // observation with no sequence forces exactly the
                            // `Running` transition; the agent's real sequence is
                            // picked up through the settle window below instead,
                            // same as any other task.
                            t.state = next_state(
                                &t,
                                &Observed::Status {
                                    status: agent.agent_status,
                                    state_change_seq: None,
                                    completion_seq: None,
                                },
                            )
                            .expect("adopting a Starting task always reaches Running");
                        }
                        if let Err(err) = self.store.update_task(&mut t) {
                            tracing::error!(%err, task = %t.display_id(), "adopt starting task");
                            continue;
                        }
                        if idle_like {
                            self.emit("task.running", Some(t.id));
                            // Never mark Done from a reconcile directly: the settle
                            // window applies here too, same as the pane-known branch
                            // below.
                            self.pending_done
                                .entry(t.id)
                                .or_insert((Some(agent.state_change_seq), Instant::now()));
                        } else {
                            self.apply(t, &observed_from(agent));
                        }
                    }
                    None => {
                        t.error = Some(format!(
                            "dispatch of {} was interrupted before an agent started",
                            t.display_id()
                        ));
                        self.apply(t, &Observed::PaneExited);
                    }
                }
                continue;
            };
            match agents.iter().find(|a| a.pane_id == pane_id) {
                None => {
                    // Route through the state machine instead of forcing `Failed`
                    // directly: a `Done` task whose pane later vanishes has already
                    // finished its work and must close cleanly, not be reported as
                    // failed.
                    let mut t = task;
                    if next_state(&t, &Observed::PaneExited) == Some(TaskState::Failed) {
                        t.error = Some(format!(
                            "agent {} not found on machine {}",
                            t.agent_name
                                .clone()
                                .unwrap_or_else(|| Task::agent_name_for(t.id)),
                            self.name
                        ));
                    }
                    self.apply(t, &Observed::PaneExited);
                }
                Some(agent) => {
                    // Before the timeout check: a task that sat blocked past its
                    // timeout has not started its work yet.
                    if task.prompt_pending
                        && self
                            .deliver_pending_prompt(task.clone(), agent.agent_status)
                            .await?
                    {
                        continue;
                    }
                    let timed_out = task
                        .started_at
                        .map(|s| (Utc::now() - s).num_seconds() as u64 > task.spec.timeout_secs)
                        .unwrap_or(false);
                    if timed_out && matches!(task.state, TaskState::Running | TaskState::Blocked) {
                        let written = write_task(&self.store, task, |t| {
                            let open = matches!(t.state, TaskState::Running | TaskState::Blocked);
                            if open {
                                t.state = TaskState::Stale;
                            }
                            open
                        });
                        match written {
                            Ok(Some(t)) => self.emit("task.stale", Some(t.id)),
                            Ok(None) => {}
                            // One row must not stop the rest of the pass, nor
                            // the live count after it.
                            Err(err) => tracing::error!(machine = %self.name, %err, "mark stale"),
                        }
                        continue;
                    }
                    if matches!(
                        agent.agent_status,
                        crate::herdr::AgentStatus::Idle | crate::herdr::AgentStatus::Done
                    ) {
                        // Never mark Done from a reconcile directly: the settle
                        // window applies here too. An entry from an idle event has
                        // no sequence yet; record the one seen now and start its
                        // window here, since what came before it is unknown.
                        let entry = self
                            .pending_done
                            .entry(task.id)
                            .or_insert((None, Instant::now()));
                        if entry.0.is_none() {
                            *entry = (Some(agent.state_change_seq), Instant::now());
                        }
                        continue;
                    }
                    self.apply(task, &observed_from(agent));
                }
            }
        }
        self.refresh_live();
        Ok(())
    }
}

/// `update_task` with one retry. On `Conflict` (the row was written since it
/// was read, e.g. by `pastor task close` or `retry`), re-read it and apply
/// `change` to the fresh copy. `change` returns `false` when the row no
/// longer wants the change; then nothing is written. Returns the row as
/// written, or `None` when nothing was. A second conflict, or any other
/// store error, is returned.
///
/// The fresh copy comes from the store, so its unstored fields
/// (`Task::activity_seen`) are at their defaults: `change` must set any it
/// relies on rather than expect them carried over.
fn write_task(
    store: &Store,
    mut task: Task,
    change: impl Fn(&mut Task) -> bool,
) -> anyhow::Result<Option<Task>> {
    if !change(&mut task) {
        return Ok(None);
    }
    match store.update_task(&mut task) {
        Ok(()) => return Ok(Some(task)),
        Err(err) if err.downcast_ref::<crate::store::Conflict>().is_none() => return Err(err),
        Err(_) => {}
    }
    let Some(mut fresh) = store.get_task(task.id)? else {
        return Ok(None);
    };
    if !change(&mut fresh) {
        return Ok(None);
    }
    store.update_task(&mut fresh)?;
    Ok(Some(fresh))
}

/// What `agent.list` says about an agent, as a state machine observation.
fn observed_from(agent: &AgentInfo) -> Observed {
    Observed::Status {
        status: agent.agent_status,
        state_change_seq: Some(agent.state_change_seq),
        completion_seq: agent.completion_seq,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::fake::FakeHerdr;
    use crate::herdr::{AgentStatus, ConnectError, ConnectFuture, Connection};
    use crate::store::NewTask;
    use crate::task::DispatchSpec;

    fn settings() -> MachineSettings {
        MachineSettings {
            settle: Duration::from_millis(100),
            reconcile_every: Duration::from_millis(200),
            initial_backoff: Duration::from_millis(50),
            max_backoff: Duration::from_millis(200),
            request_timeout: Duration::from_secs(5),
            agent_ready_timeout: Duration::from_millis(500),
            poll_every: Duration::from_millis(200),
        }
    }

    /// `agent_ready_timeout` has to stay under `request_timeout`, which bounds a
    /// whole dispatch: otherwise the readiness wait is cut short by the outer
    /// timeout and reported as a wedged machine instead of an agent that never
    /// came up. Pinned for the shipped defaults and for the test settings above.
    #[test]
    fn ready_timeout_stays_under_the_request_timeout() {
        for s in [MachineSettings::default(), settings()] {
            assert!(
                s.agent_ready_timeout < s.request_timeout,
                "{:?} >= {:?}",
                s.agent_ready_timeout,
                s.request_timeout
            );
        }
    }

    /// `settings()` with a wider settle window, for the couple of tests that sleep
    /// for a short, fixed time and assert the settle window has *not* fired yet: a
    /// 30ms sleep against a 100ms window is close enough to flake under load. 500ms
    /// gives a comfortable margin without slowing the rest of the suite, which
    /// doesn't wait out `settle` at all.
    fn settings_with_settle(settle: Duration) -> MachineSettings {
        MachineSettings {
            settle,
            ..settings()
        }
    }

    fn spec() -> DispatchSpec {
        DispatchSpec {
            agent: "claude".into(),
            agent_args: vec![],
            repo: None,
            worktree: false,
            branch: None,
            machine: None,
            tags: vec![],
            timeout_secs: 3600,
        }
    }

    fn new_task(store: &Store) -> Task {
        store
            .insert_task(NewTask {
                job: "run".into(),
                item: serde_json::Value::Null,
                prompt: "hi".into(),
                spec: spec(),
            })
            .unwrap()
    }

    async fn wait_for<F: Fn() -> bool>(what: &str, f: F) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !f() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn spawn(
        fake: &FakeHerdr,
        store: &Arc<Store>,
    ) -> (MachineHandle, broadcast::Receiver<PastorEvent>) {
        spawn_with_settings(fake, store, settings())
    }

    fn spawn_with_settings(
        fake: &FakeHerdr,
        store: &Arc<Store>,
        settings: MachineSettings,
    ) -> (MachineHandle, broadcast::Receiver<PastorEvent>) {
        let (events, rx) = broadcast::channel(64);
        let h = spawn_machine(
            "m".into(),
            2,
            vec![],
            Arc::new(fake.clone()),
            store.clone(),
            settings,
            events,
        );
        (h, rx)
    }

    fn state_of(store: &Store, id: i64) -> TaskState {
        store.get_task(id).unwrap().unwrap().state
    }

    /// Someone else wrote the row (task close, retry) between our read and
    /// our write: the change is re-applied to the fresh row, keeping their
    /// fields.
    #[test]
    fn write_task_retries_a_conflict_once_on_the_fresh_row() {
        let store = Store::open_in_memory().unwrap();
        let t = new_task(&store);
        let stale = t.clone();
        let mut other = t.clone();
        other.error = Some("written by someone else".into());
        store.update_task(&mut other).unwrap();

        let written = write_task(&store, stale, |t| {
            t.state = TaskState::Stale;
            true
        })
        .unwrap()
        .expect("the fresh row still wanted the change");
        assert_eq!(written.state, TaskState::Stale);
        assert_eq!(written.error.as_deref(), Some("written by someone else"));
        assert_eq!(state_of(&store, t.id), TaskState::Stale);
    }

    /// A row that moved on to a state the change does not apply to is left
    /// alone: a task closed meanwhile is never reopened.
    #[test]
    fn write_task_leaves_a_row_that_moved_on() {
        let store = Store::open_in_memory().unwrap();
        let t = new_task(&store);
        let stale = t.clone();
        // What `pastor task close` does, outside the actor.
        let mut closed = t.clone();
        closed.state = TaskState::Closed;
        closed.finished_at = Some(Utc::now());
        store.update_task(&mut closed).unwrap();
        let written = write_task(&store, stale, |t| {
            let open = t.state != TaskState::Closed;
            if open {
                t.state = TaskState::Stale;
            }
            open
        })
        .unwrap();
        assert!(written.is_none());
        assert_eq!(state_of(&store, t.id), TaskState::Closed);
    }

    /// `activity_seen` is not stored, so a re-read row always comes back
    /// without it. The retried write carries what the change set, which is
    /// what the caller copies into the actor's in-memory set.
    #[test]
    fn write_task_keeps_the_unstored_activity_flag_on_the_retry() {
        let store = Store::open_in_memory().unwrap();
        let mut t = new_task(&store);
        t.prompt_pending = true;
        store.update_task(&mut t).unwrap();
        let stale = t.clone();
        let mut other = t.clone();
        other.error = Some("written by someone else".into());
        store.update_task(&mut other).unwrap();

        let written = write_task(&store, stale, |t| {
            t.prompt_pending = false;
            t.activity_seen = true;
            true
        })
        .unwrap()
        .expect("the fresh row still wanted the change");
        assert!(written.activity_seen);
        assert!(!store.get_task(t.id).unwrap().unwrap().prompt_pending);
    }

    /// A second conflict is not retried again: it is returned to the caller.
    #[test]
    fn write_task_returns_a_second_conflict() {
        let store = Store::open_in_memory().unwrap();
        let t = new_task(&store);
        let stale = t.clone();
        let mut other = t.clone();
        store.update_task(&mut other).unwrap();
        let calls = std::cell::Cell::new(0);
        let err = write_task(&store, stale, |t| {
            calls.set(calls.get() + 1);
            if calls.get() == 2 {
                // Someone writes again between the re-read and the retry.
                let mut again = store.get_task(t.id).unwrap().unwrap();
                store.update_task(&mut again).unwrap();
            }
            t.state = TaskState::Stale;
            true
        })
        .unwrap_err();
        assert!(err.downcast_ref::<crate::store::Conflict>().is_some());
        assert_eq!(calls.get(), 2);
    }

    #[tokio::test]
    async fn dispatch_then_events_drive_state() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = spawn_with_settings(
            &fake,
            &store,
            settings_with_settle(Duration::from_millis(500)),
        );
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = new_task(&store);
        let t = h.dispatch(t.id).await.unwrap();
        assert_eq!(t.state, TaskState::Running);
        assert_eq!(h.snapshot().live, 1);
        let pane = t.pane_id.clone().unwrap();

        fake.set_status(&pane, AgentStatus::Blocked);
        wait_for("blocked", || state_of(&store, t.id) == TaskState::Blocked).await;
        let ev = loop {
            let ev = tokio::time::timeout(Duration::from_secs(2), events.recv())
                .await
                .unwrap()
                .unwrap();
            if ev.kind == "task.blocked" {
                break ev;
            }
            assert_eq!(
                ev.kind, "task.running",
                "only the dispatch event may precede task.blocked"
            );
        };
        assert_eq!(ev.task_id, Some(t.id));
        assert_eq!(
            ev.job.as_deref(),
            Some("run"),
            "task events carry the row's job"
        );

        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;

        fake.set_status(&pane, AgentStatus::Idle);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            state_of(&store, t.id),
            TaskState::Running,
            "done waits for the settle window"
        );
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
        assert_eq!(
            store.get_task(t.id).unwrap().unwrap().last_completion_seq,
            Some(fake.agents()[0].state_change_seq),
            "a done task's baseline moves to the change that finished it"
        );

        fake.close_pane(&pane);
        wait_for("closed", || state_of(&store, t.id) == TaskState::Closed).await;
        wait_for("live 0", || h.snapshot().live == 0).await;
        let _ = h.read(t.id, 10).await.unwrap_err();
    }

    /// herdr rejects a prompt to a blocked agent instead of queueing it, so
    /// dispatch must not send one: it leaves the task `blocked` with the
    /// prompt pending. Flipping the task to `running` on its own would leave
    /// the agent with nothing to do, so the prompt has to be sent once a
    /// human clears the block.
    #[tokio::test]
    async fn a_blocked_agent_is_not_prompted_at_dispatch_but_is_once_it_clears() {
        let fake = FakeHerdr::new();
        fake.set_ready_after(Duration::from_millis(150));
        let watcher = fake.clone();
        tokio::spawn(async move {
            loop {
                if let Some(a) = watcher.agents().first() {
                    watcher.set_status(&a.pane_id, AgentStatus::Blocked);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        assert_eq!(t.state, TaskState::Blocked);
        assert!(store.get_task(t.id).unwrap().unwrap().prompt_pending);
        let prompts = || {
            fake.requests()
                .iter()
                .filter(|r| r.method == "agent.prompt")
                .count()
        };
        assert_eq!(prompts(), 0, "a blocked agent is not prompted at dispatch");

        // The human answers the agent's startup question; it goes idle.
        fake.set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Idle);
        wait_for("running with the prompt sent", || {
            let t = store.get_task(t.id).unwrap().unwrap();
            t.state == TaskState::Running && !t.prompt_pending
        })
        .await;
        assert_eq!(
            prompts(),
            1,
            "the prompt is sent exactly once, after the block clears"
        );
        let t = store.get_task(t.id).unwrap().unwrap();
        assert_eq!(t.error, None, "the blocked advice is gone");
        assert_eq!(
            fake.agents()[0].agent_status,
            AgentStatus::Working,
            "the agent took the prompt"
        );
    }

    /// The folder trust dialog case end to end: the agent is blocked before it
    /// finishes launching, dispatch leaves the task `blocked` with the prompt
    /// pending and never calls `agent.prompt`, and once a human answers,
    /// herdr may still refuse with `agent_not_ready` for a moment (it is
    /// still within its launching window). Reconcile keeps retrying until the
    /// prompt lands, then the task runs.
    ///
    /// `ready_after` is well above `READY_POLL` (`dispatch.rs`) and above
    /// `agent_ready_timeout` in `settings()`: whichever `agent.list` poll
    /// dispatch's readiness wait makes, the agent is still inside its
    /// launching window, so `agent.list` reports it `blocked` with
    /// `launch_pending: true` every time. That is what forces dispatch
    /// through the blocked-while-launching path deterministically, instead
    /// of racing to see whether the launching window happens to have closed
    /// by the time dispatch polls again.
    #[tokio::test]
    async fn an_agent_blocked_at_launch_gets_its_prompt_once_answered() {
        let fake = FakeHerdr::new();
        fake.set_ready_after(Duration::from_secs(2));
        let watcher = fake.clone();
        tokio::spawn(async move {
            loop {
                if let Some(a) = watcher.agents().first() {
                    watcher.set_status(&a.pane_id, AgentStatus::Blocked);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        assert_eq!(t.state, TaskState::Blocked);
        assert!(t.prompt_pending);
        let prompts = || {
            fake.requests()
                .iter()
                .filter(|r| r.method == "agent.prompt")
                .count()
        };
        assert_eq!(
            prompts(),
            0,
            "a blocked-while-launching agent is not prompted at dispatch"
        );

        // Answered while the fake still counts the agent as launching: the
        // first delivery attempt is refused and has to be retried. `seq`
        // increments on `set_status` and on an *accepted* `agent.prompt`
        // (fake.rs only bumps it past the checks), never on a refusal, so the
        // delta below counts accepted state changes, not raw request counts.
        let seq_before_clear = fake.agents()[0].state_change_seq;
        fake.set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Idle);
        wait_for("running with the prompt sent", || {
            let t = store.get_task(t.id).unwrap().unwrap();
            t.state == TaskState::Running && !t.prompt_pending
        })
        .await;
        assert_eq!(fake.agents()[0].agent_status, AgentStatus::Working);
        assert_eq!(
            fake.agents()[0].state_change_seq,
            seq_before_clear + 2,
            "exactly two accepted changes: the block clearing to idle, then \
             one accepted prompt; every agent_not_ready retry in between left \
             the sequence untouched"
        );
        let sent = fake
            .requests()
            .into_iter()
            .rfind(|r| r.method == "agent.prompt")
            .expect("at least one agent.prompt request was made");
        assert_eq!(sent.params["text"], "hi", "our prompt, not a stray one");
    }

    /// A store error in reconcile is pastor's problem, not the machine's: the
    /// channel stays connected and no `machine.lost` goes out.
    #[tokio::test]
    async fn a_local_error_in_reconcile_is_not_an_outage() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = new_task(&store);
        store.execute_raw(&format!(
            "UPDATE tasks SET machine = 'm', state = 'running', created_at = 'not a date' WHERE id = {}",
            t.id
        ));
        // Several reconcile ticks (200ms each) run into the corrupt row.
        let deadline = Instant::now() + Duration::from_millis(800);
        while let Ok(ev) = tokio::time::timeout_at(deadline.into(), events.recv()).await {
            assert_ne!(ev.unwrap().kind, "machine.lost");
        }
        assert_eq!(h.snapshot().channel, ChannelState::Connected);
    }

    /// Recovering from `Blocked` must not carry the startup advice onto a task
    /// that is running again, and a `Done` task that goes back to work must not
    /// keep the finish time of the cycle it just left.
    #[tokio::test]
    async fn state_changes_clear_the_previous_state_s_metadata() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn_with_settings(
            &fake,
            &store,
            settings_with_settle(Duration::from_millis(100)),
        );
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();

        // A task that blocks carries dispatch's advice, then answers the prompt.
        fake.set_status(&pane, AgentStatus::Blocked);
        wait_for("blocked", || state_of(&store, t.id) == TaskState::Blocked).await;
        let mut blocked = store.get_task(t.id).unwrap().unwrap();
        blocked.error = Some("agent blocked during startup; answer its prompt".into());
        store.update_task(&mut blocked).unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("running again", || {
            state_of(&store, t.id) == TaskState::Running
        })
        .await;
        assert_eq!(
            store.get_task(t.id).unwrap().unwrap().error,
            None,
            "the blocked advice outlived the blocked state"
        );

        // A done task that goes back to work loses the old finish time.
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
        assert!(store.get_task(t.id).unwrap().unwrap().finished_at.is_some());
        fake.set_status(&pane, AgentStatus::Working);
        wait_for("working again", || {
            state_of(&store, t.id) == TaskState::Running
        })
        .await;
        assert_eq!(
            store.get_task(t.id).unwrap().unwrap().finished_at,
            None,
            "a running task kept the finish time of its last cycle"
        );
    }

    #[tokio::test]
    async fn done_is_cancelled_if_agent_resumes_within_settle() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn_with_settings(
            &fake,
            &store,
            settings_with_settle(Duration::from_millis(500)),
        );
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Idle);
        tokio::time::sleep(Duration::from_millis(30)).await;
        fake.set_status(&pane, AgentStatus::Working);
        // Long enough to clear the 500ms settle window measured from the original
        // Idle event: if `Working` had *not* cancelled the pending-done entry, the
        // settle check would have marked the task Done well before this returns.
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Running);
    }

    fn saw(events: &mut broadcast::Receiver<PastorEvent>, kind: &str, id: i64) -> bool {
        let mut found = false;
        while let Ok(ev) = events.try_recv() {
            found |= ev.kind == kind && ev.task_id == Some(id);
        }
        found
    }

    /// The fleet bug of 2026-09-25: herdr 0.9.1 has no `completion_seq`, so a
    /// finished agent only shows as `idle`/`done` with a newer
    /// `state_change_seq`. That alone, after `settle`, must make the task done.
    #[tokio::test]
    async fn an_agent_that_works_then_goes_idle_is_done_after_settle() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = spawn_with_settings(
            &fake,
            &store,
            settings_with_settle(Duration::from_millis(300)),
        );
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        assert_eq!(fake.agents()[0].agent_status, AgentStatus::Working);

        fake.set_status(&pane, AgentStatus::Idle);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            state_of(&store, t.id),
            TaskState::Running,
            "done waits for the settle window"
        );
        let list = fake.agent_list().await.unwrap();
        assert_eq!(list[0].agent_status, AgentStatus::Idle);
        assert_eq!(list[0].completion_seq, None, "herdr 0.9.1 has none");
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
        assert!(saw(&mut events, "task.done", t.id), "task.done was emitted");
        assert_eq!(h.snapshot().live, 1, "done keeps its pane until it closes");
    }

    /// herdr's `done` status (idle, not yet looked at) finishes a task the same
    /// way `idle` does.
    #[tokio::test]
    async fn an_agent_reporting_done_is_done_after_settle() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        fake.set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Done);
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
    }

    /// An idle event carries no sequence, so the first settle check cannot tell
    /// whether the agent worked again, unseen, inside the window. It must not
    /// mark the task done: it records the sequence `agent.list` shows and starts
    /// a new window from there. A moved sequence restarts the window again; an
    /// unchanged one at the next check is done.
    #[tokio::test]
    async fn an_idle_event_without_a_sequence_waits_a_window_from_the_listed_one() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = spawn_with_settings(
            &fake,
            &store,
            MachineSettings {
                // Keep reconcile out of the way: only settle checks list agents.
                reconcile_every: Duration::from_secs(60),
                ..settings_with_settle(Duration::from_millis(400))
            },
        );
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        let lists = || {
            fake.requests()
                .iter()
                .filter(|r| r.method == "agent.list")
                .count()
        };
        let before = lists();

        // Idle event, no sequence; the agent's sequence is past the baseline.
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("first settle check", || lists() > before).await;
        // The check is logged when it arrives; give it time to act.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            state_of(&store, t.id),
            TaskState::Running,
            "an unknown sequence starts a window, it does not finish one"
        );

        // Worked and went idle again inside the new window, unseen by events.
        fake.set_status_silently(&pane, AgentStatus::Working);
        fake.set_status_silently(&pane, AgentStatus::Idle);
        wait_for("second settle check", || lists() > before + 1).await;
        // The check is logged when it arrives; give it time to act.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            state_of(&store, t.id),
            TaskState::Running,
            "a moved sequence restarts the window"
        );
        assert!(!saw(&mut events, "task.done", t.id));

        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
        assert!(
            lists() > before + 2,
            "done needs a third check at an unchanged sequence"
        );
    }
    /// An agent that went idle and back to work within `settle` is not done,
    /// even when the flip back to work was missed on the event stream: the
    /// settle check sees `state_change_seq` moved past the value it recorded
    /// and waits another window.
    #[tokio::test]
    async fn idle_then_working_again_unseen_within_settle_does_not_complete() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let settle = Duration::from_millis(600);
        let (h, mut events) = spawn_with_settings(
            &fake,
            &store,
            MachineSettings {
                // Reconcile often, so it records the candidate's sequence early.
                reconcile_every: Duration::from_millis(50),
                ..settings_with_settle(settle)
            },
        );
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        // Unseen by events: only reconcile's agent.list notices the idle agent
        // and records the sequence it saw.
        fake.set_status_silently(&pane, AgentStatus::Idle);
        tokio::time::sleep(Duration::from_millis(200)).await;
        // Back to work and idle again before the window ends, still unseen.
        fake.set_status_silently(&pane, AgentStatus::Working);
        fake.set_status_silently(&pane, AgentStatus::Idle);
        // Past the first window, well inside the second.
        tokio::time::sleep(Duration::from_millis(550)).await;
        assert_eq!(
            state_of(&store, t.id),
            TaskState::Running,
            "a new state change inside settle restarts the window"
        );
        assert!(!saw(&mut events, "task.done", t.id));
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
    }

    /// An agent that is idle without ever having worked on the prompt has not
    /// done anything: `state_change_seq` never moved past the value recorded
    /// when the prompt was sent, so no amount of settling makes it done.
    #[tokio::test]
    async fn an_agent_idle_since_its_prompt_never_completes() {
        let fake = FakeHerdr::new();
        fake.ignore_prompts(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        assert_eq!(t.state, TaskState::Running);
        assert_eq!(fake.agents()[0].agent_status, AgentStatus::Idle);
        // An idle event with no state change behind it (herdr sends one when a
        // human looks at a `done` pane) is a candidate, not a completion.
        fake.set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Idle);
        // Several settle windows (100ms) and reconciles (200ms).
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Running);
        assert!(!saw(&mut events, "task.done", t.id));
    }

    /// Count of `agent.list` requests the fake has answered so far.
    fn lists(fake: &FakeHerdr) -> usize {
        fake.requests()
            .iter()
            .filter(|r| r.method == "agent.list")
            .count()
    }

    /// herdr stamps a new `state_change_seq` on `unknown` too, so an agent
    /// that flickers `idle -> unknown -> idle` sits idle past its baseline
    /// without having worked. With no `working` or `blocked` seen since the
    /// prompt, no amount of settling makes that done.
    #[tokio::test]
    async fn idle_unknown_idle_without_activity_never_completes() {
        let fake = FakeHerdr::new();
        fake.ignore_prompts(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        let baseline = t.last_completion_seq.unwrap();
        fake.set_status(&pane, AgentStatus::Unknown);
        fake.set_status(&pane, AgentStatus::Idle);
        assert!(fake.agents()[0].state_change_seq > baseline);
        // Several settle windows (100ms) and reconciles (200ms).
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Running);
        assert!(!saw(&mut events, "task.done", t.id));
    }

    /// A working spell only `agent.list` saw (the event was missed) counts as
    /// activity: the later idle completes the task after settle.
    #[tokio::test]
    async fn working_seen_only_by_reconcile_counts_as_activity() {
        let fake = FakeHerdr::new();
        fake.ignore_prompts(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status_silently(&pane, AgentStatus::Working);
        // Two lists: the first may have been in flight before the change.
        let before = lists(&fake);
        wait_for("a reconcile saw it working", || lists(&fake) > before + 1).await;
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
    }

    /// `blocked` after the prompt is activity too: the agent asked a human,
    /// got its answer and finished, so its idle completes the task.
    #[tokio::test]
    async fn blocked_then_idle_is_done_after_settle() {
        let fake = FakeHerdr::new();
        fake.ignore_prompts(true);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status_silently(&pane, AgentStatus::Blocked);
        wait_for("blocked", || state_of(&store, t.id) == TaskState::Blocked).await;
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
    }

    /// A task whose prompt herdr refused (blocked at startup) is not done when
    /// its agent goes idle while the prompt still cannot be delivered, even
    /// though the agent's `state_change_seq` has moved: the moves were its
    /// launch and its startup question, not our work.
    #[tokio::test]
    async fn an_agent_idle_before_its_prompt_is_delivered_never_completes() {
        let fake = FakeHerdr::new();
        // Still launching for the whole test: every delivery is `agent_not_ready`.
        fake.set_ready_after(Duration::from_secs(10));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut t = new_task(&store);
        let name = Task::agent_name_for(t.id);
        let created = fake.workspace_create(None, &name).await.unwrap();
        let pane = created.root_pane.pane_id.clone();
        fake.agent_start(&name, "claude", &pane, &[]).await.unwrap();
        fake.set_status(&pane, AgentStatus::Blocked);
        t.state = TaskState::Blocked;
        t.prompt_pending = true;
        t.machine = Some("m".into());
        t.pane_id = Some(pane.clone());
        t.agent_name = Some(name);
        store.update_task(&mut t).unwrap();
        let (h, mut events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        // The human answers; the agent is idle but herdr will not take input yet.
        fake.set_status(&pane, AgentStatus::Idle);
        tokio::time::sleep(Duration::from_millis(700)).await;
        let got = store.get_task(t.id).unwrap().unwrap();
        assert!(got.prompt_pending, "the prompt was never delivered");
        assert_ne!(got.state, TaskState::Done);
        assert!(!saw(&mut events, "task.done", t.id));
    }

    /// Rows left `running` by earlier builds have no baseline. Their agents
    /// did work (`state_change_seq` is past 0) and sit idle, so after this fix
    /// reconcile finishes them instead of holding their slots forever.
    #[tokio::test]
    async fn a_running_task_without_a_baseline_completes_when_its_agent_is_idle() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut t = new_task(&store);
        let name = Task::agent_name_for(t.id);
        let created = fake.workspace_create(None, &name).await.unwrap();
        let pane = created.root_pane.pane_id.clone();
        fake.agent_start(&name, "claude", &pane, &[]).await.unwrap();
        fake.set_status(&pane, AgentStatus::Working);
        fake.set_status(&pane, AgentStatus::Done);
        t.state = TaskState::Running;
        t.machine = Some("m".into());
        t.pane_id = Some(pane);
        t.agent_name = Some(name);
        store.update_task(&mut t).unwrap();
        assert_eq!(t.last_completion_seq, None);
        let (_h, _events) = spawn(&fake, &store);
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
    }

    #[tokio::test]
    async fn reconcile_marks_missing_agents_failed() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut t = new_task(&store);
        t.state = TaskState::Running;
        t.machine = Some("m".into());
        t.pane_id = Some("w9:p1".into());
        t.agent_name = Some("t-1".into());
        store.update_task(&mut t).unwrap();
        let (_h, _events) = spawn(&fake, &store);
        wait_for("failed", || state_of(&store, t.id) == TaskState::Failed).await;
        assert!(
            store
                .get_task(t.id)
                .unwrap()
                .unwrap()
                .error
                .unwrap()
                .contains("not found on machine")
        );
    }

    #[tokio::test]
    async fn reconcile_closes_done_tasks_whose_pane_vanished() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut t = new_task(&store);
        t.state = TaskState::Done;
        t.machine = Some("m".into());
        t.pane_id = Some("w9:p1".into());
        t.agent_name = Some("t-1".into());
        t.last_completion_seq = Some(1);
        store.update_task(&mut t).unwrap();
        let (_h, _events) = spawn(&fake, &store);
        wait_for("closed", || state_of(&store, t.id) == TaskState::Closed).await;
        assert!(
            store.get_task(t.id).unwrap().unwrap().error.is_none(),
            "a done task whose pane vanished did not fail; it just finished"
        );
    }

    #[tokio::test]
    async fn reconcile_adopts_live_agent_state() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let created = fake.workspace_create(None, "t-1").await.unwrap();
        fake.agent_start("t-1", "claude", &created.root_pane.pane_id, &[])
            .await
            .unwrap();
        fake.set_status(&created.root_pane.pane_id, AgentStatus::Blocked);
        let mut t = new_task(&store);
        t.state = TaskState::Starting;
        t.machine = Some("m".into());
        t.pane_id = Some(created.root_pane.pane_id.clone());
        t.agent_name = Some("t-1".into());
        store.update_task(&mut t).unwrap();
        let (_h, _events) = spawn(&fake, &store);
        wait_for("blocked", || state_of(&store, t.id) == TaskState::Blocked).await;
    }

    /// A `Starting` task with no pane recorded is what `run_dispatch` persists right
    /// before its first herdr call (see the comment there); a crash between that
    /// write and the pane/workspace ids being recorded leaves exactly this row.
    /// `reconcile` must find the agent dispatch would have started (named `t-<id>`)
    /// and adopt it instead of leaving the task stuck, or worse, re-dispatching it.
    /// The row here has no `agent_name` either, and the fake agent is left idle (no
    /// `agent.prompt`): both are deliberately the plainest shape a crash can leave
    /// (`agent_name` is now always set by `run_dispatch`'s pre-persist, but adoption
    /// must not *depend* on that; a crash between `agent.start` and `agent.prompt`
    /// is exactly as real), so this exercises adoption doing all the work itself,
    /// not a test fixture that already looks like a successful dispatch.
    #[tokio::test]
    async fn reconcile_adopts_a_starting_task_by_agent_name() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut t = new_task(&store);
        let name = Task::agent_name_for(t.id);
        let created = fake.workspace_create(None, &name).await.unwrap();
        fake.agent_start(&name, "claude", &created.root_pane.pane_id, &[])
            .await
            .unwrap();
        t.state = TaskState::Starting;
        t.machine = Some("m".into());
        // pane_id, workspace_id and agent_name are all left unset.
        store.update_task(&mut t).unwrap();
        let (_h, _events) = spawn_with_settings(
            &fake,
            &store,
            settings_with_settle(Duration::from_millis(500)),
        );
        wait_for("adopted and running", || {
            state_of(&store, t.id) == TaskState::Running
        })
        .await;
        let got = store.get_task(t.id).unwrap().unwrap();
        assert_eq!(
            got.pane_id.as_deref(),
            Some(created.root_pane.pane_id.as_str())
        );
        assert_eq!(
            got.workspace_id.as_deref(),
            Some(created.workspace.workspace_id.as_str())
        );
        assert_eq!(
            got.agent_name.as_deref(),
            Some(name.as_str()),
            "adoption must set agent_name itself, not rely on it already being there"
        );

        // Idle with no completed work must not fast-track to Done, and work
        // finished later must still wait out the settle window, same as the
        // pane-known path (mirrors `dispatch_then_events_drive_state`).
        // Adopted panes get no status events until the next reconnect, so
        // the working spell counts once a reconcile has listed it.
        fake.set_status(&created.root_pane.pane_id, AgentStatus::Working);
        let before = lists(&fake);
        wait_for("a reconcile saw it working", || lists(&fake) > before + 1).await;
        fake.set_status(&created.root_pane.pane_id, AgentStatus::Idle);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            state_of(&store, t.id),
            TaskState::Running,
            "done waits for the settle window"
        );
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
    }

    /// A crash can leave a `Starting` row whose agent is still launching:
    /// `agent.list` shows it `launch_pending`, so dispatch never sent it the
    /// prompt. Adoption must keep that delivery pending. When the launch ends
    /// the agent goes idle at a newer `state_change_seq` than the one adopted;
    /// that is its startup, not this task's work, so the task is not done:
    /// the prompt is sent then, and only idle past the prompt reply's sequence
    /// completes it.
    #[tokio::test]
    async fn reconcile_keeps_the_prompt_pending_for_a_launching_agent() {
        let fake = FakeHerdr::new();
        let ready_after = Duration::from_millis(600);
        fake.set_ready_after(ready_after);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut t = new_task(&store);
        let name = Task::agent_name_for(t.id);
        let created = fake.workspace_create(None, &name).await.unwrap();
        let pane = created.root_pane.pane_id.clone();
        fake.agent_start(&name, "claude", &pane, &[]).await.unwrap();
        let launched = Instant::now();
        // As herdr has it: nothing detected while launching, so the end of the
        // launch (unknown -> idle) is a state change with a new sequence.
        fake.set_status_silently(&pane, AgentStatus::Unknown);
        t.state = TaskState::Starting;
        t.machine = Some("m".into());
        store.update_task(&mut t).unwrap();
        let (_h, mut events) = spawn(&fake, &store);
        let prompts = || {
            fake.requests()
                .iter()
                .filter(|r| r.method == "agent.prompt")
                .count()
        };

        wait_for("adopted", || {
            store.get_task(t.id).unwrap().unwrap().pane_id.is_some()
        })
        .await;
        assert!(
            launched.elapsed() < ready_after,
            "adopted while the agent was still launching"
        );
        assert!(
            store.get_task(t.id).unwrap().unwrap().prompt_pending,
            "a launch-pending agent has not seen the prompt"
        );

        tokio::time::sleep(ready_after.saturating_sub(launched.elapsed())).await;
        assert_eq!(prompts(), 0, "nothing is sent before the agent is ready");
        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("running with the prompt sent", || {
            let t = store.get_task(t.id).unwrap().unwrap();
            t.state == TaskState::Running && !t.prompt_pending
        })
        .await;
        assert_eq!(prompts(), 1, "the prompt is sent once, after the launch");
        let got = store.get_task(t.id).unwrap().unwrap();
        assert_eq!(
            got.last_completion_seq,
            Some(fake.agents()[0].state_change_seq),
            "the baseline is the prompt reply's sequence"
        );

        // Well past settle and a reconcile: the agent is working, not done.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Running);
        assert!(!saw(&mut events, "task.done", t.id));

        fake.set_status(&pane, AgentStatus::Idle);
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
    }

    /// Same interrupted-dispatch shape, but no agent named `t-<id>` exists anywhere:
    /// the crash happened before `agent.start` even ran. Nothing to adopt, so the
    /// task must fail, not hang forever as `Starting`.
    #[tokio::test]
    async fn reconcile_fails_a_starting_task_with_no_agent() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut t = new_task(&store);
        t.state = TaskState::Starting;
        t.machine = Some("m".into());
        // agent_name is left unset: `reconcile` matches by the name it derives from
        // the task id, not by reading a persisted `agent_name` back.
        store.update_task(&mut t).unwrap();
        let (_h, _events) = spawn(&fake, &store);
        wait_for("failed", || state_of(&store, t.id) == TaskState::Failed).await;
        assert!(
            store
                .get_task(t.id)
                .unwrap()
                .unwrap()
                .error
                .unwrap()
                .contains("interrupted")
        );
    }

    struct Refusing;
    impl Connector for Refusing {
        fn connect(&self) -> ConnectFuture<'_> {
            Box::pin(async {
                Err(ConnectError {
                    message: "ssh: permission denied (255)".into(),
                })
            })
        }
        fn describe(&self) -> String {
            "refusing".into()
        }
    }

    #[tokio::test]
    async fn machine_reports_reconnecting_with_error() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (events, mut rx) = broadcast::channel(64);
        let h = spawn_machine(
            "m".into(),
            2,
            vec![],
            Arc::new(Refusing),
            store,
            settings(),
            events,
        );
        wait_for("reconnecting", || {
            h.snapshot().channel == ChannelState::Reconnecting
        })
        .await;
        assert!(h.snapshot().error.unwrap().contains("permission denied"));
        let ev = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ev.kind, "machine.lost");
        assert_eq!(h.snapshot().live, 0);
        // With a 50ms initial backoff (200ms max) there are many more connect
        // attempts within the next second; `machine.lost` must not repeat since
        // the machine never recovers.
        let extra = tokio::time::timeout(Duration::from_millis(800), rx.recv()).await;
        assert!(extra.is_err(), "machine.lost repeated: {extra:?}");
    }

    #[tokio::test]
    async fn disconnect_triggers_reconnect_and_resubscribe() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        fake.disconnect_all();
        // The reconnecting state is too brief to observe reliably; the events prove
        // the cycle instead. A disconnect after being connected always announces
        // the outage before the recovery (see `announce_lost`/`announce_connected`),
        // so a break followed by a successful reconnect emits a matched
        // `machine.lost` / `machine.connected` pair, never `connected` alone.
        let mut saw_lost = false;
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .expect("machine.connected within 5s")
                .unwrap();
            if ev.kind == "machine.lost" {
                saw_lost = true;
            } else if ev.kind == "machine.connected" {
                assert!(
                    saw_lost,
                    "machine.connected without a preceding machine.lost"
                );
                break;
            }
        }
        assert_eq!(h.snapshot().channel, ChannelState::Connected);
        fake.set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Blocked);
        wait_for("blocked after resubscribe", || {
            state_of(&store, t.id) == TaskState::Blocked
        })
        .await;
    }

    /// A CLI talks to whatever head is running, which may predate the field.
    #[test]
    fn a_status_from_an_older_head_reads_without_a_pastor_version() {
        let s: MachineStatus = serde_json::from_value(serde_json::json!({
            "name": "pi-3", "host": "fleet@pi-3", "endpoint": "ssh fleet@pi-3",
            "channel": "connected", "herdr_version": "0.9.1", "protocol": 22,
            "error": null, "live": 0, "max_agents": 2, "tags": []
        }))
        .unwrap();
        assert_eq!(s.pastor_version, None);
    }

    /// Each connect asks the machine for its pastor version once, and the
    /// status carries the answer until the next connect asks again.
    #[tokio::test]
    async fn status_carries_the_pastor_version_from_each_connect() {
        let fake = FakeHerdr::new();
        fake.set_pastor_version(Some("0.2.0"));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        assert_eq!(h.snapshot().pastor_version.as_deref(), Some("0.2.0"));

        fake.set_pastor_version(None);
        fake.disconnect_all();
        wait_for("unknown after a reconnect", || {
            h.snapshot().pastor_version.is_none() && h.snapshot().channel == ChannelState::Connected
        })
        .await;
    }

    #[tokio::test]
    async fn incompatible_protocol_is_reported_and_not_dispatched_to() {
        let fake = FakeHerdr::new();
        fake.set_protocol(20);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("incompatible", || {
            h.snapshot().channel == ChannelState::Incompatible
        })
        .await;
        assert_eq!(h.snapshot().protocol, Some(20));
        let err = h.dispatch(new_task(&store).id).await.unwrap_err();
        assert!(err.to_string().contains("not connected"), "{err}");
    }

    #[tokio::test]
    async fn stale_after_timeout() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = store
            .insert_task(NewTask {
                job: "run".into(),
                item: serde_json::Value::Null,
                prompt: "hi".into(),
                spec: DispatchSpec {
                    timeout_secs: 1,
                    ..spec()
                },
            })
            .unwrap();
        let t = h.dispatch(t.id).await.unwrap();
        assert_eq!(t.state, TaskState::Running);
        wait_for("stale", || state_of(&store, t.id) == TaskState::Stale).await;
        assert_eq!(fake.agents().len(), 1, "nothing was killed");
    }

    /// Every request is served by the fake except `events.subscribe`, which gets
    /// a connection that closes without answering until `subscribes` reaches
    /// `fail_until`. Selecting by method, not by call parity: a connect attempt
    /// makes several ordinary calls (ping, reconcile's `agent.list`) before it
    /// subscribes, so a parity rule would fail one of those instead and never
    /// reach `open_events` at all.
    ///
    /// Past `fail_until`, `wedge_forever` chooses what "past the failures"
    /// means: `false` (most tests) hands the subscription to the wrapped fake
    /// for real; `true` accepts the connection and then never acknowledges the
    /// subscribe at all — the shape of a herdr that is permanently wedged, not
    /// just failing, used to prove a stuck subscribe cannot block the poll loop.
    ///
    /// `slow_ack`, independent of `wedge_forever`, adds a fixed delay before
    /// acking and proxying to the fake: long enough for a test to act (e.g.
    /// dispatch a task) while this attempt is still in flight but not wedged
    /// forever, to prove a *later* attempt (built from the pane set as it
    /// stands then) is the one that actually lands the subscription.
    struct FlakyEvents {
        subscribes: Arc<std::sync::atomic::AtomicUsize>,
        fail_until: usize,
        wedge_forever: bool,
        slow_ack: Option<Duration>,
        fake: FakeHerdr,
    }

    impl Connector for FlakyEvents {
        fn connect(&self) -> ConnectFuture<'_> {
            let fake = self.fake.clone();
            let subscribes = self.subscribes.clone();
            let fail_until = self.fail_until;
            let wedge_forever = self.wedge_forever;
            let slow_ack = self.slow_ack;
            Box::pin(async move {
                let (a, b) = tokio::io::duplex(64 * 1024);
                let (ar, aw) = tokio::io::split(a);
                let (br, bw) = tokio::io::split(b);
                tokio::spawn(async move {
                    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
                    let mut reader = tokio::io::BufReader::new(br);
                    let mut writer = bw;
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let Ok(req) = serde_json::from_str::<crate::herdr::Request>(line.trim()) else {
                        return;
                    };
                    if req.method == "events.subscribe" {
                        let n = subscribes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if n < fail_until {
                            return; // dropping `writer` is the EOF the subscribe sees
                        }
                        if wedge_forever {
                            // Accept the connection but never write a reply: no
                            // EOF, no ack, nothing. Held here for the lifetime of
                            // the test.
                            std::future::pending::<()>().await;
                        }
                        if let Some(delay) = slow_ack {
                            tokio::time::sleep(delay).await;
                        }
                        // Past the failures (and any delay): hand the subscription to the fake for real.
                        let mut stream = match fake
                            .subscribe(
                                req.params
                                    .get("subscriptions")
                                    .and_then(|v| v.as_array())
                                    .cloned()
                                    .unwrap_or_default(),
                            )
                            .await
                        {
                            Ok(s) => s,
                            Err(_) => return,
                        };
                        let ack = serde_json::json!({"id": req.id, "result": {"type": "subscription_started"}});
                        if writer
                            .write_all(format!("{ack}\n").as_bytes())
                            .await
                            .is_err()
                        {
                            return;
                        }
                        while let Ok(ev) = stream.next().await {
                            let line = serde_json::to_string(&ev).unwrap();
                            if writer
                                .write_all(format!("{line}\n").as_bytes())
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                        return;
                    }
                    // Anything else is relayed to the fake on a connection of its
                    // own and the reply is handed back with the caller's id.
                    let reply = match fake.connect().call(&req.method, req.params).await {
                        Ok(result) => serde_json::json!({"id": req.id, "result": result}),
                        Err(crate::herdr::HerdrError::Api { code, message }) => {
                            serde_json::json!({"id": req.id, "error": {"code": code, "message": message}})
                        }
                        Err(_) => return,
                    };
                    let _ = writer.write_all(format!("{reply}\n").as_bytes()).await;
                });
                Ok(Connection::new(Box::new(ar), Box::new(aw)))
            })
        }
        fn describe(&self) -> String {
            "flaky events".into()
        }
    }

    #[tokio::test]
    async fn event_stream_failures_back_off() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (events, _rx) = broadcast::channel(64);
        let subscribes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let connector = FlakyEvents {
            subscribes: subscribes.clone(),
            fail_until: usize::MAX,
            wedge_forever: false,
            slow_ack: None,
            fake,
        };
        let _h = spawn_machine(
            "m".into(),
            2,
            vec![],
            Arc::new(connector),
            store,
            settings(),
            events,
        );
        // Busy-spinning would run this thousands of times in 300ms; exponential
        // backoff (50ms, 100ms, 200ms, 200ms, ...) keeps it to about 3 attempts
        // in that window. Bound generously above that to avoid flakes from
        // scheduling jitter while still catching a real spin — and from below,
        // so a test that stopped reaching the subscribe at all fails too.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let n = subscribes.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            (2..12).contains(&n),
            "events.subscribe attempted {n} times in 300ms: expected a few, backing off"
        );
    }

    /// Requests work (ping, agent.list) but the event subscription will not open:
    /// the spec's `polling` state. The machine stays dispatchable, tasks are
    /// tracked by reconcile every `poll_every`, no `machine.lost` is announced,
    /// and the first successful subscribe returns it to `connected`.
    ///
    /// `fail_until: 6` keeps the subscribe failing past the local retry delay's
    /// climb from `initial_backoff` (50ms) to `max_backoff` (200ms, both from
    /// `settings()`): attempts land at roughly 0 (the initial one, in `run`,
    /// before `Polling`), 50, 150, 350, 550, 750ms, and the 7th, at ~950ms,
    /// succeeds. That is comfortably past the 100ms `poll_every` below, so the
    /// Blocked status set right after dispatch is certain to be caught by a
    /// poll tick while still `Polling`, not by a lucky `Connected` reconcile.
    #[tokio::test]
    async fn a_machine_whose_events_will_not_open_polls_instead_of_dropping() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (events, mut rx) = broadcast::channel(64);
        let subscribes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut settings = settings();
        settings.poll_every = Duration::from_millis(100);
        let h = spawn_machine(
            "m".into(),
            2,
            vec![],
            Arc::new(FlakyEvents {
                subscribes: subscribes.clone(),
                fail_until: 6,
                wedge_forever: false,
                slow_ack: None,
                fake: fake.clone(),
            }),
            store.clone(),
            settings,
            events,
        );
        wait_for("polling", || h.snapshot().channel == ChannelState::Polling).await;
        assert!(
            h.snapshot()
                .error
                .as_deref()
                .unwrap_or("")
                .contains("events"),
            "{:?}",
            h.snapshot().error
        );

        // Dispatch works while polling.
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        assert_eq!(t.state, TaskState::Running);
        // Without an event stream, only the poll can see this change.
        fake.set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Blocked);
        wait_for("blocked via poll", || {
            state_of(&store, t.id) == TaskState::Blocked
        })
        .await;
        assert_eq!(
            h.snapshot().channel,
            ChannelState::Polling,
            "the change must have been caught by the poll tick, not a lucky connected reconcile"
        );

        wait_for("connected once the subscribe succeeds", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        // Never lost: the machine answered every request throughout.
        while let Ok(ev) = rx.try_recv() {
            assert_ne!(ev.kind, "machine.lost", "{ev:?}");
        }
    }

    /// A wedged `events.subscribe` (a herdr that accepts the connection and
    /// never acknowledges it, forever, not just for one attempt) must not block
    /// the poll loop: `dispatch` must still reply promptly, and the channel must
    /// stay `Polling`. `request_timeout` is set high (10s) so the subscribe's
    /// own internal timeout cannot rescue a blocking implementation within the
    /// window this test actually waits; the old inline `self.open_events().await`
    /// in the retry arm would have held up commands and poll ticks for up to
    /// that long.
    #[tokio::test]
    async fn a_wedged_subscribe_does_not_block_dispatch_while_polling() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (events, _rx) = broadcast::channel(64);
        let subscribes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut settings = settings();
        settings.request_timeout = Duration::from_secs(10);
        let h = spawn_machine(
            "m".into(),
            2,
            vec![],
            Arc::new(FlakyEvents {
                subscribes: subscribes.clone(),
                fail_until: 1,
                wedge_forever: true,
                slow_ack: None,
                fake,
            }),
            store.clone(),
            settings,
            events,
        );
        wait_for("polling", || h.snapshot().channel == ChannelState::Polling).await;
        // Wait until the retry timer has actually fired and the wedged attempt
        // (the one past `fail_until`) is in flight server-side, not just until
        // `Polling` is reached: dispatching too early would race the retry
        // timer and could get served before the wedge even starts, proving
        // nothing.
        wait_for("wedged subscribe attempt in flight", || {
            subscribes.load(std::sync::atomic::Ordering::SeqCst) >= 2
        })
        .await;

        let dispatched =
            tokio::time::timeout(Duration::from_secs(2), h.dispatch(new_task(&store).id))
                .await
                .expect("dispatch must not be blocked by a wedged subscribe attempt");
        assert_eq!(dispatched.unwrap().state, TaskState::Running);
        assert_eq!(
            h.snapshot().channel,
            ChannelState::Polling,
            "a wedged subscribe attempt must not be mistaken for a lost machine"
        );
    }

    /// A dispatch that adds a pane while a subscribe attempt is already in
    /// flight must not let that attempt land the machine on `Connected` with a
    /// stale subscription. The in-flight attempt was built (by
    /// `subscribe_future`) from the pane set as it stood *before* the dispatch;
    /// left alone, it would ack with a subscription missing the new pane, and
    /// that pane's status changes would go unseen until the next
    /// `reconcile_tick` — set to 30s here, far past this test's `wait_for`
    /// bound, so only the event stream delivering the Blocked transition can
    /// make the test pass.
    ///
    /// `slow_ack: 300ms` (not `wedge_forever`) is used so the attempt actually
    /// completes on its own if the resubscribe-on-`Dispatch` fix is missing:
    /// a wedged attempt would just hang forever and this test would time out
    /// either way, proving nothing about *which* subscription landed.
    #[tokio::test]
    async fn a_pane_added_mid_subscribe_is_not_lost_to_a_stale_subscription() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (events, _rx) = broadcast::channel(64);
        let subscribes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut settings = settings();
        settings.reconcile_every = Duration::from_secs(30);
        let h = spawn_machine(
            "m".into(),
            2,
            vec![],
            Arc::new(FlakyEvents {
                subscribes: subscribes.clone(),
                fail_until: 1,
                wedge_forever: false,
                slow_ack: Some(Duration::from_millis(300)),
                fake: fake.clone(),
            }),
            store.clone(),
            settings,
            events,
        );
        wait_for("polling", || h.snapshot().channel == ChannelState::Polling).await;
        // The attempt past `fail_until` (its request line has been read
        // server-side, so it is now in its 300ms `slow_ack` sleep) is the one
        // built without the pane the dispatch below is about to add.
        wait_for("subscribe attempt in flight", || {
            subscribes.load(std::sync::atomic::Ordering::SeqCst) >= 2
        })
        .await;

        let t = h.dispatch(new_task(&store).id).await.unwrap();
        assert_eq!(t.state, TaskState::Running);

        wait_for("connected once a subscribe lands", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;

        fake.set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Blocked);
        wait_for(
            "blocked via the event stream, not the 30s reconcile",
            || state_of(&store, t.id) == TaskState::Blocked,
        )
        .await;
    }

    /// The reply to a dispatch must carry an already-refreshed live count: a
    /// caller that reads `snapshot().live` the moment `dispatch` returns is the
    /// serialised dispatch pass deciding whether the machine has room left.
    #[tokio::test]
    async fn live_count_is_current_when_dispatch_replies() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        for expected in 1..=2 {
            h.dispatch(new_task(&store).id).await.unwrap();
            assert_eq!(h.snapshot().live, expected, "live count lagged the reply");
        }
    }

    #[test]
    fn channel_states_that_accept_dispatch() {
        assert!(ChannelState::Connected.accepts_dispatch());
        assert!(ChannelState::Polling.accepts_dispatch());
        for s in [
            ChannelState::Connecting,
            ChannelState::Reconnecting,
            ChannelState::Incompatible,
        ] {
            assert!(!s.accepts_dispatch(), "{s}");
        }
        assert_eq!(ChannelState::Polling.to_string(), "polling");
    }

    /// Wraps any connector and can be broken: while broken, every connection it
    /// hands out is already at EOF, so the first read of any request fails at
    /// the transport level. That is what a machine that went away looks like
    /// now that no connection is held between requests.
    struct Breakable {
        inner: Arc<dyn Connector>,
        broken: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Connector for Breakable {
        fn connect(&self) -> ConnectFuture<'_> {
            let broken = self.broken.load(std::sync::atomic::Ordering::SeqCst);
            let inner = self.inner.clone();
            Box::pin(async move {
                if broken {
                    Ok(Connection::new(
                        Box::new(tokio::io::empty()),
                        Box::new(tokio::io::sink()),
                    ))
                } else {
                    inner.connect().await
                }
            })
        }
        fn describe(&self) -> String {
            "breakable".into()
        }
    }

    #[tokio::test]
    async fn a_request_failing_at_the_transport_level_triggers_reconnect() {
        let fake = FakeHerdr::new();
        let broken = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (events, _rx) = broadcast::channel(64);
        let h = spawn_machine(
            "m".into(),
            2,
            vec![],
            Arc::new(Breakable {
                inner: Arc::new(fake.clone()),
                broken: broken.clone(),
            }),
            store.clone(),
            settings(),
            events,
        );
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        h.dispatch(new_task(&store).id).await.unwrap();

        broken.store(true, std::sync::atomic::Ordering::SeqCst);
        // The machine is gone: this dispatch must fail rather than hang or
        // silently succeed against a connection that answers nothing.
        let t2 = new_task(&store);
        let err = h.dispatch(t2.id).await;
        assert!(err.is_err(), "dispatch against a dead machine must fail");
        assert_eq!(state_of(&store, t2.id), TaskState::Failed);

        broken.store(false, std::sync::atomic::Ordering::SeqCst);
        wait_for("reconnected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t3 = h.dispatch(new_task(&store).id).await.unwrap();
        assert_eq!(t3.state, TaskState::Running);
    }

    /// A request failing at the transport level while `Polling` (not just while
    /// `Connected`) must take the same reconnect path: `Reconnecting`,
    /// `machine.lost`, then back to `Polling` (its subscribe never succeeds
    /// here) once requests work again, with `machine.connected` marking the
    /// recovery. `FlakyEvents` never lets the subscribe through
    /// (`fail_until: usize::MAX`), so the machine can only ever be `Polling` or
    /// `Reconnecting`, never `Connected`, which keeps this test about the
    /// poll loop's own `PollExit::Reconnect` path specifically.
    #[tokio::test]
    async fn a_broken_request_while_polling_reconnects_and_returns_to_polling() {
        let fake = FakeHerdr::new();
        let broken = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (events, mut rx) = broadcast::channel(64);
        let subscribes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut settings = settings();
        settings.poll_every = Duration::from_millis(50);
        let inner: Arc<dyn Connector> = Arc::new(FlakyEvents {
            subscribes: subscribes.clone(),
            fail_until: usize::MAX,
            wedge_forever: false,
            slow_ack: None,
            fake,
        });
        let h = spawn_machine(
            "m".into(),
            2,
            vec![],
            Arc::new(Breakable {
                inner,
                broken: broken.clone(),
            }),
            store,
            settings,
            events,
        );
        wait_for("polling", || h.snapshot().channel == ChannelState::Polling).await;

        broken.store(true, std::sync::atomic::Ordering::SeqCst);
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("machine.lost within 5s")
                .unwrap();
            if ev.kind == "machine.lost" {
                break;
            }
        }
        wait_for("reconnecting", || {
            h.snapshot().channel == ChannelState::Reconnecting
        })
        .await;

        broken.store(false, std::sync::atomic::Ordering::SeqCst);
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("machine.connected within 5s")
                .unwrap();
            if ev.kind == "machine.connected" {
                break;
            }
        }
        assert_eq!(h.snapshot().channel, ChannelState::Polling);
    }

    #[tokio::test]
    async fn dispatch_over_a_wedged_connection_times_out_fails_and_reconnects() {
        let fake = FakeHerdr::new();
        // `agent.start` is the first herdr call inside `dispatch()` that can hang;
        // hanging it exercises the timeout without needing the connect/ping/
        // reconcile/subscribe path (none of those are hung) to also cooperate.
        fake.hang_method("agent.start");
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut settings = settings();
        settings.request_timeout = Duration::from_millis(100);
        settings.agent_ready_timeout = Duration::from_millis(50);
        let (h, mut events) = spawn_with_settings(&fake, &store, settings);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;

        let t = new_task(&store);
        let err = h.dispatch(t.id).await.unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        assert_eq!(state_of(&store, t.id), TaskState::Failed);
        assert!(
            store
                .get_task(t.id)
                .unwrap()
                .unwrap()
                .error
                .unwrap()
                .contains("timed out")
        );

        // A request that stops answering is dead: the actor must reconnect, which
        // announces the outage (machine.lost) and its recovery (machine.connected),
        // just like `disconnect_triggers_reconnect_and_resubscribe`.
        let mut saw_lost = false;
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .expect("machine.connected within 5s")
                .unwrap();
            if ev.kind == "machine.lost" {
                saw_lost = true;
            } else if ev.kind == "machine.connected" {
                assert!(
                    saw_lost,
                    "machine.connected without a preceding machine.lost"
                );
                break;
            }
        }
        assert_eq!(h.snapshot().channel, ChannelState::Connected);
    }

    /// A herdr that accepts the events connection and never acknowledges the
    /// subscription must not hang the actor: `open_events` bounds the subscribe
    /// by `request_timeout` and the attempt is retried after backoff.
    /// `hang_method` is one-shot, so only the first attempt is wedged.
    #[tokio::test]
    async fn a_wedged_subscribe_reconnects() {
        let fake = FakeHerdr::new();
        fake.hang_method("events.subscribe");
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut settings = settings();
        settings.request_timeout = Duration::from_millis(100);
        settings.agent_ready_timeout = Duration::from_millis(50);
        let (h, _events) = spawn_with_settings(&fake, &store, settings);
        wait_for("connected despite the wedged subscribe", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
    }

    /// `reconcile` runs right after `ping` on every connect attempt, and its
    /// `agent.list` call had no timeout before this fix: a herdr that accepted the
    /// connection and then stopped answering there would hang the actor forever,
    /// which in turn blocks `Daemon::dispatch_queued` and the accept loop behind it.
    /// `hang_method` is one-shot, so this hangs only the first connect attempt's
    /// `agent.list`; the retry after backoff finds it answering normally again.
    #[tokio::test]
    async fn reconcile_over_a_wedged_connection_reconnects() {
        let fake = FakeHerdr::new();
        fake.hang_method("agent.list");
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut settings = settings();
        settings.request_timeout = Duration::from_millis(100);
        settings.agent_ready_timeout = Duration::from_millis(50);
        let (h, _events) = spawn_with_settings(&fake, &store, settings);
        wait_for("connected despite the wedged reconcile", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
    }
}
