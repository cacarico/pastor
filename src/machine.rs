use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::MIN_HERDR_PROTOCOL;
use crate::dispatch::dispatch;
use crate::herdr::{
    AgentInfo, Connection, Connector, EventStream, subscription_agent_status,
    subscription_lifecycle,
};
use crate::store::Store;
use crate::task::{Observed, Task, TaskState, next_state};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelState {
    Connecting,
    Connected,
    Reconnecting,
    Incompatible,
}

impl std::fmt::Display for ChannelState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ChannelState::Connecting => "connecting",
            ChannelState::Connected => "connected",
            ChannelState::Reconnecting => "reconnecting",
            ChannelState::Incompatible => "incompatible",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineStatus {
    pub name: String,
    pub endpoint: String,
    pub channel: ChannelState,
    pub herdr_version: Option<String>,
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
}

impl Default for MachineSettings {
    fn default() -> Self {
        MachineSettings {
            settle: Duration::from_secs(10),
            reconcile_every: Duration::from_secs(60),
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PastorEvent {
    pub kind: String,
    pub task_id: Option<i64>,
    pub machine: String,
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
        endpoint: connector.describe(),
        channel: ChannelState::Connecting,
        herdr_version: None,
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
        was_connected: false,
        failures: 0,
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
    /// task id -> (completion_seq observed, when). Confirmed as Done after `settle`.
    pending_done: HashMap<i64, (Option<u64>, Instant)>,
    was_connected: bool,
    failures: u32,
}

impl Actor {
    async fn run(mut self) {
        let mut backoff = self.settings.initial_backoff;
        loop {
            self.set_channel(ChannelState::Connecting, None);
            let mut req = match self.connector.connect().await {
                Ok(c) => c,
                Err(err) => {
                    self.connect_failed(err.message, &mut backoff).await;
                    continue;
                }
            };
            let pong = match req.ping().await {
                Ok(p) => p,
                Err(err) => {
                    self.connect_failed(err.to_string(), &mut backoff).await;
                    continue;
                }
            };
            {
                let mut s = self.status.write().unwrap();
                s.herdr_version = Some(pong.version.clone());
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
            if let Err(err) = self.reconcile(&mut req).await {
                self.connect_failed(format!("reconcile: {err}"), &mut backoff)
                    .await;
                continue;
            }
            let mut events = match self.open_events().await {
                Ok(s) => s,
                Err(err) => {
                    self.connect_failed(format!("events: {err}"), &mut backoff)
                        .await;
                    continue;
                }
            };
            backoff = self.settings.initial_backoff;
            self.failures = 0;
            self.set_channel(ChannelState::Connected, None);
            if self.was_connected {
                self.emit("machine.connected", None);
            }
            self.was_connected = true;
            self.refresh_live();

            let mut settle_tick =
                tokio::time::interval(Duration::from_millis(50).max(self.settings.settle / 4));
            let mut reconcile_tick = tokio::time::interval(self.settings.reconcile_every);
            reconcile_tick.tick().await; // first tick fires immediately; we just reconciled
            loop {
                tokio::select! {
                    cmd = self.rx.recv() => {
                        let Some(cmd) = cmd else { return };
                        let resubscribe = self.handle_command(cmd, &mut req).await;
                        if resubscribe {
                            match self.open_events().await {
                                Ok(s) => events = s,
                                Err(err) => { tracing::warn!(machine = %self.name, %err, "resubscribe failed"); break; }
                            }
                        }
                    }
                    ev = events.next() => match ev {
                        Ok(ev) => self.handle_event(&ev),
                        Err(err) => { tracing::warn!(machine = %self.name, %err, "event stream ended"); break; }
                    },
                    _ = settle_tick.tick() => {
                        if let Err(err) = self.confirm_pending_done(&mut req).await { tracing::warn!(machine = %self.name, %err, "settle check failed"); break; }
                    }
                    _ = reconcile_tick.tick() => {
                        if let Err(err) = self.reconcile(&mut req).await { tracing::warn!(machine = %self.name, %err, "reconcile failed"); break; }
                    }
                }
            }
            self.set_channel(ChannelState::Reconnecting, Some("connection lost".into()));
        }
    }

    async fn connect_failed(&mut self, message: String, backoff: &mut Duration) {
        self.failures += 1;
        tracing::warn!(machine = %self.name, %message, "connect failed");
        self.set_channel(ChannelState::Reconnecting, Some(message));
        if self.was_connected || self.failures == 2 {
            self.emit("machine.lost", None);
            self.was_connected = false;
            self.failures = 0;
        }
        self.drain_commands_while_down(*backoff).await;
        *backoff = (*backoff * 2).min(self.settings.max_backoff);
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
        let live = self
            .store
            .tasks_on_machine(&self.name)
            .map(|v| v.len())
            .unwrap_or(0);
        self.status.write().unwrap().live = live;
    }

    fn emit(&self, kind: &str, task_id: Option<i64>) {
        tracing::info!(machine = %self.name, kind, ?task_id, "event");
        let _ = self.events.send(PastorEvent {
            kind: kind.into(),
            task_id,
            machine: self.name.clone(),
        });
    }

    async fn open_events(&self) -> Result<EventStream, anyhow::Error> {
        let conn = self.connector.connect().await?;
        let mut subs = vec![
            subscription_lifecycle("pane.closed"),
            subscription_lifecycle("pane.exited"),
        ];
        for t in self.store.tasks_on_machine(&self.name)? {
            if let Some(p) = &t.pane_id {
                subs.push(subscription_agent_status(p));
            }
        }
        Ok(conn.subscribe(subs).await?)
    }

    /// Returns true when the tracked pane set changed and the event subscription must be reopened.
    async fn handle_command(&mut self, cmd: MachineCommand, req: &mut Connection) -> bool {
        match cmd {
            MachineCommand::Dispatch { task_id, reply } => {
                let result = self.run_dispatch(task_id, req).await;
                let changed = result.is_ok();
                let _ = reply.send(result);
                self.refresh_live();
                changed
            }
            MachineCommand::Read {
                task_id,
                lines,
                reply,
            } => {
                let result = match self.store.get_task(task_id) {
                    Ok(Some(t))
                        if t.machine.as_deref() == Some(&self.name) && t.state.occupies_pane() =>
                    {
                        req.agent_read(t.agent_name.as_deref().unwrap_or(""), lines)
                            .await
                            .map_err(Into::into)
                    }
                    Ok(_) => Err(anyhow::anyhow!(
                        "task {task_id} has no live agent on {}",
                        self.name
                    )),
                    Err(e) => Err(e),
                };
                let _ = reply.send(result);
                false
            }
        }
    }

    async fn run_dispatch(&mut self, task_id: i64, req: &mut Connection) -> anyhow::Result<Task> {
        let mut task = self
            .store
            .get_task(task_id)?
            .ok_or_else(|| anyhow::anyhow!("task {task_id} not found"))?;
        anyhow::ensure!(
            task.state == TaskState::Queued,
            "task {} is {}, not queued",
            task.display_id(),
            task.state
        );
        task.machine = Some(self.name.clone());
        let outcome = dispatch(req, &mut task).await;
        self.store.update_task(&task)?;
        match outcome {
            Ok(_) => {
                self.emit(&format!("task.{}", task.state), Some(task.id));
                Ok(task)
            }
            Err(err) => {
                self.emit("task.failed", Some(task.id));
                Err(anyhow::anyhow!("dispatch {}: {err}", task.display_id()))
            }
        }
    }

    fn handle_event(&mut self, ev: &crate::herdr::Event) {
        let Some(pane_id) = ev.pane_id() else { return };
        let Ok(Some(task)) = self.store.find_by_pane(&self.name, pane_id) else {
            return;
        };
        let observed = if ev.is_pane_closed() {
            Observed::PaneClosed
        } else if ev.is_pane_exited() {
            Observed::PaneExited
        } else if let Some(status) = ev.agent_status() {
            // Subscription events carry no completion_seq; treat idle as a candidate and let
            // the settle check read the real sequence from agent.list.
            Observed::Status {
                status,
                completion_seq: None,
            }
        } else {
            return;
        };
        match &observed {
            Observed::Status {
                status: crate::herdr::AgentStatus::Idle | crate::herdr::AgentStatus::Done,
                ..
            } => {
                self.pending_done
                    .insert(task.id, (task.last_completion_seq, Instant::now()));
            }
            _ => {
                self.pending_done.remove(&task.id);
                self.apply(task, &observed);
            }
        }
    }

    /// After the settle window, confirm with agent.list that the agent is still idle and
    /// its completion_seq advanced. Only then is the task done.
    async fn confirm_pending_done(&mut self, req: &mut Connection) -> anyhow::Result<()> {
        let due: Vec<i64> = self
            .pending_done
            .iter()
            .filter(|(_, (_, at))| at.elapsed() >= self.settings.settle)
            .map(|(id, _)| *id)
            .collect();
        if due.is_empty() {
            return Ok(());
        }
        let agents = req.agent_list().await?;
        for id in due {
            self.pending_done.remove(&id);
            let Ok(Some(task)) = self.store.get_task(id) else {
                continue;
            };
            let Some(agent) = agents
                .iter()
                .find(|a| Some(&a.pane_id) == task.pane_id.as_ref())
            else {
                continue;
            };
            let observed = Observed::Status {
                status: agent.agent_status,
                completion_seq: agent.completion_seq,
            };
            self.apply(task, &observed);
        }
        Ok(())
    }

    fn apply(&mut self, mut task: Task, observed: &Observed) {
        let Some(to) = next_state(&task, observed) else {
            return;
        };
        if let Observed::Status {
            completion_seq: Some(seq),
            ..
        } = observed
            && to == TaskState::Done
        {
            task.last_completion_seq = Some(*seq);
        }
        task.state = to;
        if matches!(to, TaskState::Done | TaskState::Failed | TaskState::Closed) {
            task.finished_at = Some(Utc::now());
        }
        if to == TaskState::Failed && task.error.is_none() {
            task.error = Some("agent process exited".into());
        }
        if let Err(err) = self.store.update_task(&task) {
            tracing::error!(%err, "update task");
            return;
        }
        self.emit(&format!("task.{to}"), Some(task.id));
        self.refresh_live();
    }

    /// Compare open tasks with live agents. Missing agent means the task failed while we
    /// were away; a present agent's status is applied like an event, except idle, which
    /// goes through the settle window. Long-running tasks become stale.
    async fn reconcile(&mut self, req: &mut Connection) -> anyhow::Result<()> {
        let agents: Vec<AgentInfo> = req.agent_list().await?;
        for task in self.store.tasks_on_machine(&self.name)? {
            let Some(pane_id) = task.pane_id.clone() else {
                continue;
            };
            match agents.iter().find(|a| a.pane_id == pane_id) {
                None => {
                    let mut t = task;
                    t.state = TaskState::Failed;
                    t.error = Some(format!(
                        "agent {} not found on machine {}",
                        t.agent_name.clone().unwrap_or_default(),
                        self.name
                    ));
                    t.finished_at = Some(Utc::now());
                    self.store.update_task(&t)?;
                    self.emit("task.failed", Some(t.id));
                }
                Some(agent) => {
                    let timed_out = task
                        .started_at
                        .map(|s| (Utc::now() - s).num_seconds() as u64 > task.spec.timeout_secs)
                        .unwrap_or(false);
                    if timed_out && matches!(task.state, TaskState::Running | TaskState::Blocked) {
                        let mut t = task;
                        t.state = TaskState::Stale;
                        self.store.update_task(&t)?;
                        self.emit("task.stale", Some(t.id));
                        continue;
                    }
                    if matches!(
                        agent.agent_status,
                        crate::herdr::AgentStatus::Idle | crate::herdr::AgentStatus::Done
                    ) {
                        // Never mark Done from a reconcile directly: the settle window applies here too.
                        self.pending_done
                            .entry(task.id)
                            .or_insert((task.last_completion_seq, Instant::now()));
                        continue;
                    }
                    let observed = Observed::Status {
                        status: agent.agent_status,
                        completion_seq: agent.completion_seq,
                    };
                    self.apply(task, &observed);
                }
            }
        }
        self.refresh_live();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::fake::FakeHerdr;
    use crate::herdr::{AgentStatus, ConnectError, ConnectFuture};
    use crate::store::NewTask;
    use crate::task::DispatchSpec;

    fn settings() -> MachineSettings {
        MachineSettings {
            settle: Duration::from_millis(100),
            reconcile_every: Duration::from_millis(200),
            initial_backoff: Duration::from_millis(50),
            max_backoff: Duration::from_millis(200),
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
        let (events, rx) = broadcast::channel(64);
        let h = spawn_machine(
            "m".into(),
            2,
            vec![],
            Arc::new(fake.clone()),
            store.clone(),
            settings(),
            events,
        );
        (h, rx)
    }

    fn state_of(store: &Store, id: i64) -> TaskState {
        store.get_task(id).unwrap().unwrap().state
    }

    #[tokio::test]
    async fn dispatch_then_events_drive_state() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = new_task(&store);
        let t = h.dispatch(t.id).await.unwrap();
        assert_eq!(t.state, TaskState::Running);
        assert_eq!(h.snapshot().live, 1);
        let pane = t.pane_id.clone().unwrap();

        fake.set_status(&pane, AgentStatus::Blocked, None);
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

        fake.set_status(&pane, AgentStatus::Working, None);
        wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;

        fake.set_status(&pane, AgentStatus::Idle, Some(1));
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            state_of(&store, t.id),
            TaskState::Running,
            "done waits for the settle window"
        );
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
        assert_eq!(
            store.get_task(t.id).unwrap().unwrap().last_completion_seq,
            Some(1)
        );

        fake.close_pane(&pane);
        wait_for("closed", || state_of(&store, t.id) == TaskState::Closed).await;
        wait_for("live 0", || h.snapshot().live == 0).await;
        let _ = h.read(t.id, 10).await.unwrap_err();
    }

    #[tokio::test]
    async fn done_is_cancelled_if_agent_resumes_within_settle() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Idle, Some(1));
        tokio::time::sleep(Duration::from_millis(30)).await;
        fake.set_status(&pane, AgentStatus::Working, Some(1));
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Running);
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
        store.update_task(&t).unwrap();
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
    async fn reconcile_adopts_live_agent_state() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut c = fake.connect();
        let created = c.workspace_create(None, "t-1").await.unwrap();
        c.agent_start("t-1", "claude", &created.root_pane.pane_id, &[])
            .await
            .unwrap();
        fake.set_status(&created.root_pane.pane_id, AgentStatus::Blocked, None);
        let mut t = new_task(&store);
        t.state = TaskState::Starting;
        t.machine = Some("m".into());
        t.pane_id = Some(created.root_pane.pane_id.clone());
        t.agent_name = Some("t-1".into());
        store.update_task(&t).unwrap();
        let (_h, _events) = spawn(&fake, &store);
        wait_for("blocked", || state_of(&store, t.id) == TaskState::Blocked).await;
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
        let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ev.kind, "machine.lost");
        assert_eq!(h.snapshot().live, 0);
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
        // The reconnecting state is too brief to observe reliably; the event proves the cycle.
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(5), events.recv())
                .await
                .expect("machine.connected within 5s")
                .unwrap();
            if ev.kind == "machine.connected" {
                break;
            }
        }
        assert_eq!(h.snapshot().channel, ChannelState::Connected);
        fake.set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Blocked, None);
        wait_for("blocked after resubscribe", || {
            state_of(&store, t.id) == TaskState::Blocked
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
}
