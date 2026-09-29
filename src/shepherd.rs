//! A headless `pastor serve`, the shepherd: with a head set on another
//! machine, this one runs its own jobs and connector hooks, and the tasks
//! the head gives it as a pull machine. No queue: the items a job run finds
//! go to the head in one `IpcRequest::JobSubmit`, and the head's events come
//! back through `EventsSince` for the hooks here. Its own small database
//! (`Paths::shepherd_db_file`) keeps the jobs' state and seen keys, how far
//! it has read the head's events, and the rows of the tasks it runs.
//!
//! When the head's flock.toml has this machine as `pull = true`, each tick
//! also asks for tasks (`IpcRequest::TaskClaim`) and runs them with the same
//! machine actor the head runs for a local machine, on this machine's herdr;
//! every change the actor sees goes back as `IpcRequest::TaskReport`
//! (`Puller`).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc};

use crate::cli::CliError;
use crate::config::{PastorConfig, Paths};
use crate::daemon::{Answer, Daemon, Fleet, answer_on, jobs_answer};
use crate::events::{EventRecord, EventsPage};
use crate::head::RemoteHead;
use crate::herdr::{Connector, Endpoint};
use crate::ipc::{IPC_PROTOCOL, IpcRequest, IpcResponse, SHEPHERD_ROLE, request_line};
use crate::machine::{MachineHandle, MachineSettings, PastorEvent};
use crate::scheduler::{ConfigFingerprint, Scheduler, SchedulerHandle};
use crate::store::Store;
use crate::task::{Task, TaskState};

/// The meta key that holds the last head event handed to the hooks.
const CURSOR_KEY: &str = "head_event_seq";

/// How many head events one `EventsSince` asks for.
const PAGE: u32 = 200;

/// How many tasks one `TaskClaim` asks for. The head's `max_agents` for the
/// machine is what bounds how many run here at once.
const CLAIM_AT_ONCE: u32 = 4;

/// How long a request to the head may take. A `JobSubmit` waits for the
/// head's dispatch pass, agent readiness included.
const HEAD_TIMEOUT: Duration = Duration::from_secs(60);

/// One request to the head and its reply; an unreachable head, or an error
/// reply, is an `Err` with its code (`CliError`).
pub type Ask = Arc<
    dyn Fn(IpcRequest) -> Pin<Box<dyn Future<Output = anyhow::Result<IpcResponse>> + Send>>
        + Send
        + Sync,
>;

/// `Ask` over ssh to `head`, through `pastor bridge` there.
pub fn ask_remote(head: RemoteHead) -> Ask {
    let head = Arc::new(head);
    Arc::new(move |req| {
        let head = head.clone();
        Box::pin(async move {
            let line = request_line(&req, None)?;
            match head.request(&line, HEAD_TIMEOUT).await {
                Ok(IpcResponse::Error { code, message }) => Err(CliError::err(&code, message)),
                Ok(resp) => Ok(resp),
                Err(err) => Err(crate::head::failure(&err)),
            }
        })
    })
}

/// The shepherd's side of the socket: its own jobs, and a refusal for the
/// rest, which is the head's.
pub struct Shepherd {
    paths: Paths,
    scheduler: SchedulerHandle,
    head: String,
}

impl Answer for Shepherd {
    async fn answer(&self, req: IpcRequest, from: crate::ipc::Caller) -> IpcResponse {
        // The rule the head applies (`Daemon::handle_as`), from this
        // machine's pastor.toml. Nothing a headless serve answers is on the
        // orchestrator role's table, and orchestrators run on the head.
        let refusal = match (&from.task, &from.orchestrator) {
            (Some(task), _) => Some(crate::daemon::agent_refusal(task)),
            (None, Some(o)) => Some(crate::daemon::script_refusal(o)),
            (None, None) => None,
        };
        if let Some(refusal) = refusal
            && req.changes_fleet()
            && !PastorConfig::load(&self.paths.config_file()).is_ok_and(|c| c.agents_change_fleet)
        {
            return IpcResponse::error("agent_refused", refusal);
        }
        if let IpcRequest::Ping = req {
            return IpcResponse::Pong {
                version: env!("CARGO_PKG_VERSION").into(),
                protocol: IPC_PROTOCOL,
                role: Some(SHEPHERD_ROLE.into()),
            };
        }
        match jobs_answer(&self.scheduler, req).await {
            Some(resp) => resp,
            None => IpcResponse::error(
                "shepherd_unsupported",
                format!(
                    "this is a headless pastor serve: it runs this machine's jobs and hooks, and answers only ping, tick and job list, run and reload for them; ask the head on {}",
                    self.head
                ),
            ),
        }
    }
}

/// `pastor serve` with a head set on another machine.
pub async fn serve(paths: Paths, head: RemoteHead) -> anyhow::Result<()> {
    let ssh = head.ssh.clone();
    run(paths, ssh, ask_remote(head)).await
}

/// `serve`, given how to reach the head, which `head` names in messages.
pub async fn run(paths: Paths, head: String, ask: Ask) -> anyhow::Result<()> {
    // Before the load, as for the head: an edit after it must still read as
    // a change on the scheduler's first pass.
    let on_disk = ConfigFingerprint::sample(&paths);
    let config = PastorConfig::load(&paths.config_file())?;
    paths.ensure()?;
    // Before the database, as the head does: a second serve bails here.
    let listener = Daemon::bind_socket(&paths.socket_file()).await?;
    let store = Arc::new(Store::open(&paths.shepherd_db_file())?);
    tracing::info!(socket = %paths.socket_file().display(), %head, "pastor serve (headless): this machine's jobs and hooks, for the head");
    let (events, _) = broadcast::channel(64);
    let (to_hooks, hooks_rx) = mpsc::channel(crate::events::HOOK_QUEUE_CAPACITY);
    crate::hooks::spawn(paths.clone(), store.clone(), events.downgrade(), hooks_rx);
    let mut follow = Follower {
        head: head.clone(),
        ask: ask.clone(),
        store: store.clone(),
        to_hooks,
        reachable: None,
    };
    // Once before the first job run, so the cursor starts before any task
    // that run queues and the hooks hear about it.
    follow.pass().await;
    let mut puller = Puller::new(
        config.shepherd.machine_name(),
        config.shepherd.flock_work(),
        shepherd_connector(&config),
        crate::daemon::machine_settings(&PastorConfig {
            head_address: None,
            ..config.clone()
        }),
        store.clone(),
        ask.clone(),
    );
    let every = config.tick_duration();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        puller.pass().await;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    follow.pass().await;
                    puller.pass().await;
                }
                // The actor saw a task change: tell the head now, not at
                // the next tick.
                () = puller.changed() => puller.report(false).await,
            }
        }
    });
    let fleet = Arc::new(Fleet::headless(store.clone(), ask));
    let scheduler = Scheduler::new(paths.clone(), &config, store, fleet, events)
        .with_connectors()
        .with_config_baseline(on_disk)
        .headless()
        .spawn();
    let socket = paths.socket_file();
    let shepherd = Shepherd {
        paths,
        scheduler,
        head,
    };
    answer_on(Arc::new(shepherd), listener, socket).await
}

/// This machine's own herdr, where the tasks it claims run: the local
/// session `default`, or `[shepherd] command`.
fn shepherd_connector(config: &PastorConfig) -> Arc<dyn Connector> {
    Arc::new(match &config.shepherd.command {
        Some(argv) => Endpoint::Command { argv: argv.clone() },
        None => Endpoint::Local {
            session: "default".into(),
        },
    })
}

/// What was last told to the head about a task: its state, pane and note.
type Reported = (TaskState, Option<String>, Option<String>);

/// Runs the tasks the head gives this machine as a pull machine: claims
/// them each tick, starts each with the machine actor, and reports every
/// change of their rows. The actor starts on the first claim the head
/// answers, or at once when the store still holds tasks from before a
/// restart: a machine the head does not have as a pull machine never
/// talks to its herdr.
struct Puller {
    machine: String,
    flock_work: bool,
    connector: Arc<dyn Connector>,
    settings: MachineSettings,
    store: Arc<Store>,
    ask: Ask,
    /// The actor's events; any of them means a row may have changed.
    events: broadcast::Sender<PastorEvent>,
    heard: broadcast::Receiver<PastorEvent>,
    actor: Option<MachineHandle>,
    /// Per task, what the head last accepted.
    sent: HashMap<i64, Reported>,
    /// Tasks whose dispatch is with the actor now, so a tick does not ask
    /// again while it waits in the actor's queue.
    dispatching: Arc<std::sync::Mutex<HashSet<i64>>>,
    /// The code of the last claim the head refused, so it is logged once.
    refused: Option<String>,
}

impl Puller {
    fn new(
        machine: String,
        flock_work: bool,
        connector: Arc<dyn Connector>,
        settings: MachineSettings,
        store: Arc<Store>,
        ask: Ask,
    ) -> Puller {
        let (events, heard) = broadcast::channel(256);
        Puller {
            machine,
            flock_work,
            connector,
            settings,
            store,
            ask,
            events,
            heard,
            actor: None,
            sent: HashMap::new(),
            dispatching: Default::default(),
            refused: None,
        }
    }

    /// Resolves when the actor has emitted an event since the last call.
    async fn changed(&mut self) {
        loop {
            match self.heard.recv().await {
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => return,
                // `self.events` keeps the channel open; never reached.
                Err(broadcast::error::RecvError::Closed) => std::future::pending::<()>().await,
            }
        }
    }

    /// The tasks this store holds: those claimed and not yet forgotten.
    fn tasks(&self) -> Vec<Task> {
        match self.store.list_tasks(&Default::default()) {
            Ok(tasks) => tasks,
            Err(err) => {
                tracing::error!(%err, "list the claimed tasks");
                Vec::new()
            }
        }
    }

    fn actor(&mut self) -> MachineHandle {
        self.actor
            .get_or_insert_with(|| {
                tracing::info!(machine = %self.machine, "running the head's tasks here, as pull machine {}", self.machine);
                crate::machine::spawn_machine(
                    self.machine.clone(),
                    CLAIM_AT_ONCE,
                    Vec::new(),
                    self.connector.clone(),
                    self.store.clone(),
                    self.settings.clone(),
                    self.events.clone(),
                )
            })
            .clone()
    }

    /// One tick: report every task, claim, and start what is queued here.
    async fn pass(&mut self) {
        if self.actor.is_none() && !self.tasks().is_empty() {
            self.actor();
        }
        self.report(true).await;
        self.claim().await;
        self.start_queued();
    }

    /// Ask the head for tasks and keep each as a queued row here.
    async fn claim(&mut self) {
        let req = IpcRequest::TaskClaim {
            machine: self.machine.clone(),
            free_slots: CLAIM_AT_ONCE,
            flock_work: self.flock_work,
        };
        let tasks = match (self.ask)(req).await {
            Ok(IpcResponse::Tasks(tasks)) => {
                if self.refused.take().is_some() {
                    tracing::info!(machine = %self.machine, "the head gives this machine tasks again");
                }
                tasks
            }
            Ok(other) => {
                tracing::warn!(?other, "the head answered TaskClaim with something else");
                return;
            }
            Err(err) => {
                // An unreachable head is `Follower`'s to report.
                if let Some(e) = err.downcast_ref::<CliError>()
                    && !matches!(e.code.as_str(), "head_unreachable" | "no_head")
                    && self.refused.as_deref() != Some(e.code.as_str())
                {
                    tracing::info!(
                        machine = %self.machine,
                        code = %e.code,
                        "the head gives this machine no tasks: {}",
                        e.message
                    );
                    self.refused = Some(e.code.clone());
                }
                return;
            }
        };
        self.actor();
        for t in tasks {
            match self.store.adopt_claimed(&t) {
                Ok(_) => tracing::info!(task = %t.display_id(), "claimed from the head"),
                Err(err) => tracing::error!(task = %t.display_id(), %err, "keep a claimed task"),
            }
        }
    }

    /// Hand every queued row to the actor, each once while it waits there.
    fn start_queued(&mut self) {
        let queued: Vec<i64> = self
            .tasks()
            .into_iter()
            .filter(|t| t.state == TaskState::Queued)
            .map(|t| t.id)
            .collect();
        if queued.is_empty() {
            return;
        }
        let actor = self.actor();
        for id in queued {
            if !self.dispatching.lock().unwrap().insert(id) {
                continue;
            }
            let (actor, dispatching) = (actor.clone(), self.dispatching.clone());
            tokio::spawn(async move {
                if let Err(err) = actor.dispatch(id).await {
                    tracing::warn!(task = %Task::agent_name_for(id), err = %format!("{err:#}"), "dispatch failed");
                }
                dispatching.lock().unwrap().remove(&id);
            });
        }
    }

    /// Tell the head about every row that changed since it last heard, or
    /// with `all` every row: the head answers an unchanged report with its
    /// row and nothing else, which is how a `task close` there reaches the
    /// pane here. A row the head has closed is closed here too; a closed or
    /// failed one the head has heard of is forgotten here. Stops at the
    /// first report that does not reach the head, to try again next tick.
    async fn report(&mut self, all: bool) {
        for t in self.tasks() {
            if t.state == TaskState::Queued {
                continue;
            }
            let now: Reported = (t.state, t.pane_id.clone(), t.error.clone());
            if !all && self.sent.get(&t.id) == Some(&now) {
                continue;
            }
            let req = IpcRequest::TaskReport {
                machine: self.machine.clone(),
                id: t.id,
                state: t.state,
                pane: t.pane_id.clone(),
                detail: t.error.clone(),
            };
            let head_row = match (self.ask)(req).await {
                Ok(IpcResponse::Task(row)) => row,
                Ok(other) => {
                    tracing::warn!(?other, "the head answered TaskReport with something else");
                    return;
                }
                Err(err) => {
                    let code = err.downcast_ref::<CliError>().map(|e| e.code.clone());
                    match code.as_deref() {
                        // The head has no such task here any more: nothing
                        // left to tell it.
                        Some("task_not_found" | "not_on_machine") => {
                            tracing::warn!(task = %t.display_id(), err = %format!("{err:#}"), "the head disowns a task this machine runs; forgetting it here");
                            self.forget(t.id);
                            continue;
                        }
                        _ => {
                            tracing::debug!(task = %t.display_id(), err = %format!("{err:#}"), "report not delivered; again next tick");
                            return;
                        }
                    }
                }
            };
            tracing::info!(task = %t.display_id(), state = %t.state, "reported to the head");
            self.sent.insert(t.id, now);
            if head_row.state == TaskState::Closed && t.state.is_open() {
                // Closed on the head (`task close`): the pane is here.
                let actor = self.actor();
                if let Err(err) = actor.close(t.id, false).await {
                    tracing::warn!(task = %t.display_id(), err = %format!("{err:#}"), "close a task the head closed");
                }
                continue;
            }
            if matches!(t.state, TaskState::Closed | TaskState::Failed) {
                self.forget(t.id);
            }
        }
    }

    fn forget(&mut self, id: i64) {
        self.sent.remove(&id);
        if let Err(err) = self.store.forget_task(id) {
            tracing::error!(task = %Task::agent_name_for(id), %err, "forget a claimed task");
        }
    }
}

/// Reads the head's events for this machine's hooks, from where the last
/// read stopped.
struct Follower {
    head: String,
    ask: Ask,
    store: Arc<Store>,
    to_hooks: mpsc::Sender<EventRecord>,
    /// Whether the last pass reached the head; `None` before the first.
    reachable: Option<bool>,
}

impl Follower {
    /// One catch-up, logging when the head stops or starts answering.
    async fn pass(&mut self) {
        match self.catch_up().await {
            Ok(()) => {
                if self.reachable == Some(false) {
                    tracing::info!(head = %self.head, "the head answers again");
                }
                self.reachable = Some(true);
            }
            Err(err) => {
                if self.reachable != Some(false) {
                    tracing::warn!(
                        code = "shepherd_needs_head",
                        head = %self.head,
                        err = %format!("{err:#}"),
                        "the head does not answer: this machine's jobs cannot queue tasks and its hooks hear nothing until it does; asking again each tick"
                    );
                }
                self.reachable = Some(false);
            }
        }
    }

    /// Every head event past the cursor, to the hooks in order, the cursor
    /// saved after each. With no cursor yet, the head's history is skipped:
    /// the hooks hear what happens from the first time this machine reached
    /// the head on.
    async fn catch_up(&mut self) -> anyhow::Result<()> {
        let cursor = self
            .store
            .meta(CURSOR_KEY)?
            .and_then(|v| v.parse::<u64>().ok());
        let skip = cursor.is_none();
        let mut after = cursor.unwrap_or(0);
        loop {
            let page = self.events_since(after).await?;
            let full = page.events.len() >= PAGE as usize;
            if skip {
                after = page.events.last().map_or(after, |r| r.seq);
                self.store.set_meta(CURSOR_KEY, &after.to_string())?;
            } else if let Some(end) = page.ends_before(after) {
                // Never replayed: the hooks would fire again for records
                // they may have heard under the old numbers.
                tracing::warn!(
                    code = "head_events_reset",
                    head = %self.head,
                    after,
                    end,
                    "the head's events log ends before this machine's cursor (the head moved or its state was wiped); the hooks go on from its end"
                );
                self.store.set_meta(CURSOR_KEY, &end.to_string())?;
                return Ok(());
            } else {
                // The records left still go to the hooks, from the oldest
                // the head returned: a lost stretch is logged, not retried.
                if page.gap {
                    tracing::warn!(
                        code = "head_events_gap",
                        head = %self.head,
                        after,
                        oldest = ?page.oldest,
                        "head events past the cursor were rotated out of its log before this machine read them; the hooks missed them and go on from the oldest record left"
                    );
                }
                for rec in page.events {
                    after = rec.seq;
                    if self.to_hooks.send(rec).await.is_err() {
                        anyhow::bail!("the hook runner stopped");
                    }
                    self.store.set_meta(CURSOR_KEY, &after.to_string())?;
                }
            }
            if !full {
                return Ok(());
            }
        }
    }

    async fn events_since(&self, after: u64) -> anyhow::Result<EventsPage> {
        match (self.ask)(IpcRequest::EventsSince {
            after,
            limit: PAGE,
            task: None,
        })
        .await?
        {
            IpcResponse::Events(page) => Ok(page),
            other => anyhow::bail!("the head answered EventsSince with {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn rec(seq: u64) -> EventRecord {
        EventRecord {
            summary: None,
            seq,
            at: chrono::Utc::now(),
            kind: "task.done".into(),
            task: None,
            model: None,
            job: None,
            machine: None,
            detail: None,
        }
    }

    /// A head whose events log is `log`, or that cannot be reached while
    /// `down` is set.
    fn head(log: Arc<Mutex<Vec<EventRecord>>>, down: Arc<Mutex<bool>>) -> Ask {
        Arc::new(move |req| {
            let log = log.clone();
            let down = *down.lock().unwrap();
            Box::pin(async move {
                if down {
                    return Err(CliError::err("head_unreachable", "ssh: no route"));
                }
                let IpcRequest::EventsSince { after, limit, .. } = req else {
                    panic!("{req:?}")
                };
                // As `events::since` answers: `gap` when the oldest record
                // left is past `after + 1`.
                let log = log.lock().unwrap();
                let newest = log.iter().map(|r| r.seq).max();
                let oldest = log.first().map(|r| r.seq);
                let events: Vec<EventRecord> = log
                    .iter()
                    .filter(|r| r.seq > after)
                    .take(limit as usize)
                    .cloned()
                    .collect();
                Ok(IpcResponse::Events(EventsPage {
                    events,
                    gap: oldest.is_some_and(|o| o > after + 1),
                    oldest,
                    newest,
                }))
            })
        })
    }

    fn follower(ask: Ask) -> (Follower, mpsc::Receiver<EventRecord>) {
        let (to_hooks, rx) = mpsc::channel(1024);
        let f = Follower {
            head: "user@pi-1".into(),
            ask,
            store: Arc::new(Store::open_in_memory().unwrap()),
            to_hooks,
            reachable: None,
        };
        (f, rx)
    }

    fn seqs(rx: &mut mpsc::Receiver<EventRecord>) -> Vec<u64> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .map(|r| r.seq)
            .collect()
    }

    /// The first reach skips the head's history, however many pages; after
    /// it, every new event goes to the hooks once, in order, and the cursor
    /// survives a head that stops answering.
    #[tokio::test]
    async fn the_hooks_hear_head_events_from_the_first_reach_on() {
        let log = Arc::new(Mutex::new((1..=450).map(rec).collect::<Vec<_>>()));
        let down = Arc::new(Mutex::new(true));
        let (mut f, mut rx) = follower(head(log.clone(), down.clone()));
        f.pass().await;
        assert_eq!(f.reachable, Some(false));
        assert_eq!(f.store.meta(CURSOR_KEY).unwrap(), None);

        *down.lock().unwrap() = false;
        f.pass().await;
        assert_eq!(f.reachable, Some(true));
        assert!(seqs(&mut rx).is_empty(), "history skipped");
        assert_eq!(f.store.meta(CURSOR_KEY).unwrap().as_deref(), Some("450"));

        log.lock().unwrap().extend((451..=700).map(rec));
        *down.lock().unwrap() = true;
        f.pass().await;
        *down.lock().unwrap() = false;
        f.pass().await;
        assert_eq!(seqs(&mut rx), (451..=700).collect::<Vec<_>>());
        f.pass().await;
        assert!(seqs(&mut rx).is_empty(), "each event once");
    }

    /// An empty head log still sets the cursor, so its first event is heard.
    #[tokio::test]
    async fn an_empty_head_log_starts_the_cursor_at_zero() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let (mut f, mut rx) = follower(head(log.clone(), Arc::new(Mutex::new(false))));
        f.pass().await;
        assert_eq!(f.store.meta(CURSOR_KEY).unwrap().as_deref(), Some("0"));
        log.lock().unwrap().push(rec(1));
        f.pass().await;
        assert_eq!(seqs(&mut rx), vec![1]);
    }

    /// Waits for `path`, one record per line, to hold as many records as
    /// `want`, and checks their sequence numbers.
    async fn heard(path: &std::path::Path, want: &[u64]) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let got: Vec<u64> = std::fs::read_to_string(path)
                .unwrap_or_default()
                .lines()
                .filter_map(|l| serde_json::from_str::<EventRecord>(l).ok())
                .map(|r| r.seq)
                .collect();
            if got.len() >= want.len() {
                assert_eq!(got, want, "{}", path.display());
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{}: {got:?}",
                path.display()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Records rotated out of the head's log past the cursor are lost to
    /// the hooks; the follower goes on from the oldest record the head
    /// returned, and each one after it is heard once.
    #[tokio::test]
    async fn a_gap_goes_on_from_the_oldest_record_returned() {
        let log = Arc::new(Mutex::new((1..=10).map(rec).collect::<Vec<_>>()));
        let (mut f, mut rx) = follower(head(log.clone(), Arc::new(Mutex::new(false))));
        f.pass().await;
        assert_eq!(f.store.meta(CURSOR_KEY).unwrap().as_deref(), Some("10"));

        // 11..=299 are written and rotated out before the next tick.
        *log.lock().unwrap() = (300..=320).map(rec).collect();
        f.pass().await;
        assert_eq!(f.reachable, Some(true), "a gap is not a failure");
        assert_eq!(seqs(&mut rx), (300..=320).collect::<Vec<_>>());
        assert_eq!(f.store.meta(CURSOR_KEY).unwrap().as_deref(), Some("320"));
        log.lock().unwrap().push(rec(321));
        f.pass().await;
        assert_eq!(seqs(&mut rx), vec![321]);
    }

    /// A cursor past the head's newest event (the head moved, or its state
    /// dir was wiped) moves to the head's end: nothing up to it is heard
    /// again, what comes after is.
    #[tokio::test]
    async fn a_cursor_past_the_heads_log_goes_on_from_its_end() {
        let log = Arc::new(Mutex::new((1..=20).map(rec).collect::<Vec<_>>()));
        let (mut f, mut rx) = follower(head(log.clone(), Arc::new(Mutex::new(false))));
        f.store.set_meta(CURSOR_KEY, "500").unwrap();
        f.pass().await;
        assert_eq!(f.reachable, Some(true));
        assert!(seqs(&mut rx).is_empty(), "no replay");
        assert_eq!(f.store.meta(CURSOR_KEY).unwrap().as_deref(), Some("20"));
        f.pass().await;
        assert!(seqs(&mut rx).is_empty());
        log.lock().unwrap().extend((21..=22).map(rec));
        f.pass().await;
        assert_eq!(seqs(&mut rx), vec![21, 22]);
    }

    #[tokio::test]
    async fn an_empty_head_log_under_an_old_cursor_goes_on_from_zero() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let (mut f, mut rx) = follower(head(log.clone(), Arc::new(Mutex::new(false))));
        f.store.set_meta(CURSOR_KEY, "500").unwrap();
        f.pass().await;
        assert_eq!(f.store.meta(CURSOR_KEY).unwrap().as_deref(), Some("0"));
        log.lock().unwrap().push(rec(1));
        f.pass().await;
        assert_eq!(seqs(&mut rx), vec![1]);
    }

    /// Head events reach this machine's hooks through the hook runner: an
    /// `only_own` hook hears only tasks of a job in this machine's `jobs/`
    /// that uses its connector, and records about no job; a hook without
    /// `only_own` hears every record in its `on`.
    #[tokio::test]
    async fn only_own_is_decided_by_this_machines_job_files() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        paths.ensure().unwrap();
        let out = tmp.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        let dir = paths.connectors_dir().join("note");
        std::fs::create_dir_all(&dir).unwrap();
        // Each hook appends the record it got, one JSON line.
        let hook = |name: &str| format!("cat >> '{}'", out.join(name).display());
        std::fs::write(dir.join("own.sh"), hook("own")).unwrap();
        std::fs::write(dir.join("all.sh"), hook("all")).unwrap();
        std::fs::write(
            dir.join(crate::connector::manifest::MANIFEST_FILE),
            "id = \"note\"\nversion = \"0.1.0\"\n[connector]\ncommand = [\"true\"]\n\
             [[events]]\non = [\"task.done\", \"machine.lost\"]\nonly_own = true\ncommand = [\"sh\", \"own.sh\"]\n\
             [[events]]\non = [\"task.done\"]\ncommand = [\"sh\", \"all.sh\"]\n",
        )
        .unwrap();
        std::fs::create_dir_all(paths.jobs_dir()).unwrap();
        std::fs::write(
            crate::config::job::job_path(&paths.jobs_dir(), "mine"),
            "every = \"1h\"\n[connector]\nuse = \"note\"\n[dispatch]\nprompt = \"p\"\n",
        )
        .unwrap();

        let about = |seq: u64, kind: &str, job: Option<&str>| EventRecord {
            summary: None,
            kind: kind.into(),
            job: job.map(str::to_string),
            ..rec(seq)
        };
        let log = Arc::new(Mutex::new(vec![about(1, "task.done", None)]));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (events, _) = broadcast::channel(4);
        let (to_hooks, hooks_rx) = mpsc::channel(16);
        let runner =
            crate::hooks::spawn(paths.clone(), store.clone(), events.downgrade(), hooks_rx);
        let mut f = Follower {
            head: "user@pi-1".into(),
            ask: head(log.clone(), Arc::new(Mutex::new(false))),
            store,
            to_hooks,
            reachable: None,
        };
        f.pass().await;
        log.lock().unwrap().extend([
            about(2, "task.done", Some("mine")),
            about(3, "task.done", Some("theirs")),
            about(4, "machine.lost", None),
            about(5, "task.queued", Some("mine")),
        ]);
        f.pass().await;
        drop(f);
        tokio::time::timeout(Duration::from_secs(10), runner)
            .await
            .unwrap()
            .unwrap();
        // Queued hooks outlive the runner.
        heard(&out.join("own"), &[2, 4]).await;
        heard(&out.join("all"), &[2, 3]).await;
    }

    /// A head in this process, with `laptop` as a pull machine, and an
    /// `Ask` that reaches it as the bridge would.
    async fn pull_head(tmp: &std::path::Path) -> (Arc<Daemon>, Ask) {
        let paths = Paths::new(tmp.join("head/c"), tmp.join("head/s"));
        paths.ensure().unwrap();
        let flock: crate::config::flock::Flock =
            toml::from_str("[[machine]]\nname = \"laptop\"\npull = true\n").unwrap();
        flock.save(&paths.flock_file()).unwrap();
        let config = PastorConfig::default();
        std::fs::write(paths.config_file(), toml::to_string(&config).unwrap()).unwrap();
        let on_disk = ConfigFingerprint::sample(&paths);
        let head = Arc::new(
            Daemon::start(paths, config, flock, on_disk, None)
                .await
                .unwrap(),
        );
        let to = head.clone();
        let ask: Ask = Arc::new(move |req| {
            let head = to.clone();
            Box::pin(async move {
                match head.handle(req).await {
                    IpcResponse::Error { code, message } => Err(CliError::err(&code, message)),
                    resp => Ok(resp),
                }
            })
        });
        (head, ask)
    }

    fn fast() -> MachineSettings {
        MachineSettings {
            settle: Duration::from_millis(100),
            reconcile_every: Duration::from_millis(200),
            initial_backoff: Duration::from_millis(50),
            max_backoff: Duration::from_millis(200),
            request_timeout: Duration::from_secs(5),
            agent_ready_timeout: Duration::from_millis(500),
            poll_every: Duration::from_millis(200),
            close_done_after: None,
            ..Default::default()
        }
    }

    async fn run_pinned(head: &Daemon, machine: Option<&str>) -> Task {
        let req = IpcRequest::Run {
            preempt: false,
            prompt: "fix it".into(),
            spec: crate::task::DispatchSpec {
                agent: "claude".into(),
                agent_args: vec![],
                allow: vec![],
                deny: vec![],
                repo: None,
                worktree: false,
                branch: None,
                machine: machine.map(str::to_string),
                tags: vec![],
                timeout_secs: 3600,
                checkout: None,
                reopen: None,
                agent_source: None,
                place: Default::default(),
                session_id: None,
                label: Default::default(),
                summary: Default::default(),
            },
            flock: None,
            agent: None,
            priority: None,
            role: Default::default(),
            description: None,
            summary: None,
        };
        match head.handle(req).await {
            IpcResponse::Task(t) => t,
            other => panic!("{other:?}"),
        }
    }

    /// Passes and reports until the head's row of `t` is `want`.
    async fn head_reaches(p: &mut Puller, head: &Daemon, t: &Task, want: TaskState) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let state = head.store().get_task(t.id).unwrap().unwrap().state;
            if state == want {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "{state} not {want}");
            let _ = tokio::time::timeout(Duration::from_millis(100), p.changed()).await;
            p.pass().await;
        }
    }

    /// A shepherd that is a pull machine claims the task pinned to it, runs
    /// it with a machine actor on its own herdr and reports each change: the
    /// head's row and events follow it, as for a machine the head runs. A
    /// task nobody pinned is left alone until it asks for flock work, and
    /// the rows here are forgotten once the head heard the task close.
    #[tokio::test]
    async fn a_pull_machine_runs_the_heads_task_and_reports_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (head, ask) = pull_head(tmp.path()).await;
        let mut events = head.subscribe();
        let pinned = run_pinned(&head, Some("laptop")).await;
        let loose = run_pinned(&head, None).await;
        assert_eq!(pinned.state, TaskState::Queued);
        let fake = crate::herdr::fake::FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut p = Puller::new(
            "laptop".into(),
            false,
            Arc::new(fake.clone()),
            fast(),
            store.clone(),
            ask,
        );
        head_reaches(&mut p, &head, &pinned, TaskState::Running).await;
        let row = head.store().get_task(pinned.id).unwrap().unwrap();
        assert_eq!(row.machine.as_deref(), Some("laptop"));
        let pane = row.pane_id.clone().expect("the pane is reported");
        assert_eq!(
            head.store().get_task(loose.id).unwrap().unwrap().state,
            TaskState::Queued,
            "no flock work unasked"
        );

        fake.set_status(&pane, crate::herdr::AgentStatus::Idle);
        head_reaches(&mut p, &head, &pinned, TaskState::Done).await;
        let kinds: Vec<(String, Option<String>)> = std::iter::from_fn(|| events.try_recv().ok())
            .filter(|e| e.task_id == Some(pinned.id))
            .map(|e| (e.kind, e.machine))
            .collect();
        let lap = |k: &str| (k.to_string(), Some("laptop".to_string()));
        assert_eq!(
            kinds,
            [
                ("task.queued".to_string(), None),
                lap("task.running"),
                lap("task.done")
            ],
            "as the head's own actor would"
        );

        // Closed on the head: the pane here goes, and so does the row.
        let IpcResponse::Task(_) = head
            .handle(IpcRequest::TaskClose {
                id: pinned.id,
                remove_worktree: false,
            })
            .await
        else {
            panic!()
        };
        // The head closes the row alone; the pane goes at this machine's
        // next pass, which reports every task it runs.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while store.get_task(pinned.id).unwrap().is_some() {
            assert!(std::time::Instant::now() < deadline, "never forgotten");
            let _ = tokio::time::timeout(Duration::from_millis(100), p.changed()).await;
            p.pass().await;
        }
        assert!(fake.agents().is_empty(), "{:?}", fake.agents());

        p.flock_work = true;
        head_reaches(&mut p, &head, &loose, TaskState::Running).await;
    }

    /// A machine the head does not have as a pull machine never starts an
    /// actor, so its herdr is never asked anything.
    #[tokio::test]
    async fn a_machine_the_head_refuses_starts_no_actor() {
        let tmp = tempfile::tempdir().unwrap();
        let (_head, ask) = pull_head(tmp.path()).await;
        let fake = crate::herdr::fake::FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut p = Puller::new(
            "desk".into(),
            true,
            Arc::new(fake.clone()),
            fast(),
            store,
            ask,
        );
        p.pass().await;
        p.pass().await;
        assert!(p.actor.is_none());
        assert_eq!(p.refused.as_deref(), Some("unknown_machine"));
        assert!(fake.requests().is_empty());
    }

    /// Its own jobs' requests, a ping that says what it is, and a refusal
    /// for the rest, which is the head's; an agent pastor started may not
    /// run its jobs.
    #[tokio::test]
    async fn a_shepherd_answers_only_for_its_jobs() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        paths.ensure().unwrap();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (events, _) = broadcast::channel(4);
        let ask = head(Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(true)));
        let fleet = Arc::new(Fleet::headless(store.clone(), ask));
        let config = PastorConfig::default();
        let scheduler = Scheduler::new(paths.clone(), &config, store, fleet, events)
            .headless()
            .spawn();
        let s = Shepherd {
            paths,
            scheduler,
            head: "user@pi-1".into(),
        };
        let IpcResponse::Pong { role, protocol, .. } =
            s.answer(IpcRequest::Ping, Default::default()).await
        else {
            panic!()
        };
        assert_eq!(role.as_deref(), Some(SHEPHERD_ROLE));
        assert_eq!(protocol, IPC_PROTOCOL);
        assert!(matches!(
            s.answer(IpcRequest::JobList, Default::default()).await,
            IpcResponse::Jobs(j) if j.is_empty()
        ));
        let code = |r: IpcResponse| match r {
            IpcResponse::Error { code, message } => (code, message),
            other => panic!("{other:?}"),
        };
        let (c, m) = code(
            s.answer(
                IpcRequest::List {
                    filter: Default::default(),
                },
                Default::default(),
            )
            .await,
        );
        assert_eq!(c, "shepherd_unsupported");
        assert!(m.contains("user@pi-1"), "{m}");
        let (c, _) = code(
            s.answer(
                IpcRequest::JobRun { name: "x".into() },
                crate::ipc::Caller::task(Some("t-3")),
            )
            .await,
        );
        assert_eq!(c, "agent_refused");
    }

    /// A headless `pastor serve`'s `submit_to_head` refuses a head older
    /// than `PROFILE_PROTOCOL` before it ever sends the `JobSubmit`: such a
    /// head's `serde` drops `AgentChoice.profile` from it instead of
    /// refusing the request, so a job with a profile would start unenforced
    /// rather than fail loudly.
    #[tokio::test]
    async fn submit_to_head_refuses_a_head_that_predates_profiles() {
        let old_head: Ask = Arc::new(|req| {
            Box::pin(async move {
                assert!(matches!(req, IpcRequest::Ping), "{req:?}");
                Ok(IpcResponse::Pong {
                    version: "0.5.0".into(),
                    protocol: crate::ipc::PROFILE_PROTOCOL - 1,
                    role: None,
                })
            })
        });
        let store = Arc::new(Store::open_in_memory().unwrap());
        let fleet = Fleet::headless(store, old_head);
        let job = crate::config::job::Job::parse(
            "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nprofile = \"ci\"\nprompt = \"p\"\n",
            "x",
            &crate::config::Defaults::default(),
            &crate::connector::Builtins,
        )
        .unwrap();
        let err = fleet
            .submit_to_head(&job, vec![serde_json::json!({})])
            .await
            .unwrap_err();
        assert_eq!(
            err.downcast_ref::<CliError>().map(|e| e.code.as_str()),
            Some("head_too_old"),
            "{err}"
        );
    }

    /// The same refusal for `[dispatch] preempt = true`: a head before
    /// `PREEMPT_PROTOCOL` does not know the field and its `DispatchTable`
    /// would refuse the whole `JobSubmit` as `invalid_dispatch` instead of
    /// this clear `head_too_old`.
    #[tokio::test]
    async fn submit_to_head_refuses_a_head_that_predates_preempt() {
        let old_head: Ask = Arc::new(|req| {
            Box::pin(async move {
                assert!(matches!(req, IpcRequest::Ping), "{req:?}");
                Ok(IpcResponse::Pong {
                    version: "0.5.0".into(),
                    protocol: crate::ipc::PREEMPT_PROTOCOL - 1,
                    role: None,
                })
            })
        });
        let store = Arc::new(Store::open_in_memory().unwrap());
        let fleet = Fleet::headless(store, old_head);
        let job = crate::config::job::Job::parse(
            "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\npreempt = true\nprompt = \"p\"\n",
            "x",
            &crate::config::Defaults::default(),
            &crate::connector::Builtins,
        )
        .unwrap();
        let err = fleet
            .submit_to_head(&job, vec![serde_json::json!({})])
            .await
            .unwrap_err();
        assert_eq!(
            err.downcast_ref::<CliError>().map(|e| e.code.as_str()),
            Some("head_too_old"),
            "{err}"
        );
    }

    /// The same guard for a label: a head before `LABEL_PROTOCOL` drops
    /// `[dispatch] label` (serde skips the unknown field) and names the
    /// workspace by its own default instead of the job's chosen name.
    #[tokio::test]
    async fn submit_to_head_refuses_a_head_that_predates_labels() {
        let old_head: Ask = Arc::new(|req| {
            Box::pin(async move {
                assert!(matches!(req, IpcRequest::Ping), "{req:?}");
                Ok(IpcResponse::Pong {
                    version: "0.5.0".into(),
                    protocol: crate::ipc::LABEL_PROTOCOL - 1,
                    role: None,
                })
            })
        });
        let store = Arc::new(Store::open_in_memory().unwrap());
        let fleet = Fleet::headless(store, old_head);
        let job = crate::config::job::Job::parse(
            "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nlabel = \"{{ item.key }}\"\nprompt = \"p\"\n",
            "x",
            &crate::config::Defaults::default(),
            &crate::connector::Builtins,
        )
        .unwrap();
        let err = fleet
            .submit_to_head(&job, vec![serde_json::json!({})])
            .await
            .unwrap_err();
        assert_eq!(
            err.downcast_ref::<CliError>().map(|e| e.code.as_str()),
            Some("head_too_old"),
            "{err}"
        );
    }
}
