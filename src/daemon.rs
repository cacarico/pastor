use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::broadcast;

use crate::config::flock::{Flock, MachineConfig};
use crate::config::{PastorConfig, Paths};
use crate::dispatch::{MachineView, pick_machine};
use crate::herdr::{Connector, Endpoint};
use crate::ipc::{DaemonProbe, IpcRequest, IpcResponse};
use crate::machine::{MachineHandle, MachineSettings, OrphanClosed, PastorEvent, spawn_machine};
use crate::scheduler::{Scheduler, SchedulerHandle};
use crate::store::{NewTask, RetryError, Store};
use crate::task::Task;
use crate::task::TaskState;

/// Builds a machine's transport from its flock entry. `serve` uses
/// `endpoint_factory`; tests hand out fakes by machine name.
pub type ConnectorFactory = Arc<dyn Fn(&MachineConfig) -> Arc<dyn Connector> + Send + Sync>;

/// The transports `pastor serve` uses: ssh, the local socket or a command.
pub fn endpoint_factory(paths: Paths) -> ConnectorFactory {
    Arc::new(move |m: &MachineConfig| {
        Arc::new(Endpoint::from_machine(m, &paths)) as Arc<dyn Connector>
    })
}

/// The actor timings `pastor.toml` sets. One place, so the daemon's start
/// and a reload build the same value and a reload can tell whether it changed.
pub fn machine_settings(config: &PastorConfig) -> MachineSettings {
    MachineSettings {
        settle: config.settle_duration(),
        reconcile_every: config.reconcile_duration(),
        request_timeout: config.request_timeout_duration(),
        agent_ready_timeout: config.agent_ready_timeout_duration(),
        poll_every: config.tick_duration(),
        ..Default::default()
    }
}

/// What `apply_flock` changed, by machine name, in flock order (`removed` in
/// the previous order).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FlockDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// Same name, different entry (target, session, capacity, tags) or
    /// different timings: the actor was replaced. Its tasks stay; the new
    /// actor reconciles them.
    pub retargeted: Vec<String>,
}

impl FlockDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.retargeted.is_empty()
    }
}

struct Member {
    handle: MachineHandle,
    /// What the actor was spawned from; `None` for handles given to
    /// `Fleet::new`, which `apply_flock` never manages.
    spawned_from: Option<(MachineConfig, MachineSettings)>,
}

struct Spawner {
    connect: ConnectorFactory,
    events: broadcast::Sender<PastorEvent>,
}

/// The machines plus the one lock every dispatch pass takes. Shared by the
/// daemon (a `pastor task run` dispatches inline) and the scheduler (each
/// tick, and after a job run queues tasks), so two passes never read the same
/// capacity snapshot and both fill the last slot.
///
/// The set can change while the daemon runs (`apply_flock`, from a reload of
/// `flock.toml` or `pastor.toml`). Readers take a snapshot: `machines()` and
/// `get` hand out clones, never a reference into the set.
pub struct Fleet {
    members: RwLock<Vec<Member>>,
    store: Arc<Store>,
    /// `None` for a fixed fleet (`Fleet::new`): tests and the daemon-less CLI.
    spawner: Option<Spawner>,
    dispatch_lock: tokio::sync::Mutex<()>,
}

impl Fleet {
    /// A fixed set of machines; `apply_flock` leaves it alone.
    pub fn new(machines: Vec<MachineHandle>, store: Arc<Store>) -> Fleet {
        let members = machines
            .into_iter()
            .map(|handle| Member {
                handle,
                spawned_from: None,
            })
            .collect();
        Fleet {
            members: RwLock::new(members),
            store,
            spawner: None,
            dispatch_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// An empty fleet that `apply_flock` fills, spawning one actor per machine
    /// through `connect`.
    pub fn managed(
        store: Arc<Store>,
        events: broadcast::Sender<PastorEvent>,
        connect: ConnectorFactory,
    ) -> Fleet {
        Fleet {
            members: RwLock::new(Vec::new()),
            store,
            spawner: Some(Spawner { connect, events }),
            dispatch_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Every machine in flock order, as of now.
    pub fn machines(&self) -> Vec<MachineHandle> {
        self.members
            .read()
            .unwrap()
            .iter()
            .map(|m| m.handle.clone())
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<MachineHandle> {
        self.members
            .read()
            .unwrap()
            .iter()
            .find(|m| m.handle.name == name)
            .map(|m| m.handle.clone())
    }

    /// The flock as last applied: what a reload falls back to when
    /// `flock.toml` does not load.
    pub fn flock(&self) -> Flock {
        Flock {
            machines: self
                .members
                .read()
                .unwrap()
                .iter()
                .filter_map(|m| m.spawned_from.as_ref().map(|(c, _)| c.clone()))
                .collect(),
        }
    }

    /// Make the running set match `flock` and `settings`: spawn actors for
    /// new machines, stop the ones for machines that are gone, and replace
    /// the ones whose entry or timings changed. A machine whose entry is the
    /// same keeps its actor, connection and event stream.
    ///
    /// Takes the dispatch lock, so no pass is holding a handle that is being
    /// stopped. Stopping an actor touches nothing on its machine; tasks left
    /// there keep their last state.
    pub async fn apply_flock(&self, flock: &Flock, settings: &MachineSettings) -> FlockDiff {
        let Some(spawner) = &self.spawner else {
            return FlockDiff::default();
        };
        let _pass = self.dispatch_lock.lock().await;
        let mut diff = FlockDiff::default();
        let mut members = self.members.write().unwrap();
        let mut old: Vec<Member> = std::mem::take(&mut *members);
        for m in &flock.machines {
            let want = (m.clone(), settings.clone());
            match old.iter().position(|o| o.handle.name == m.name) {
                Some(i) if old[i].spawned_from.as_ref() == Some(&want) => {
                    members.push(old.remove(i));
                }
                Some(i) => {
                    old.remove(i).handle.shutdown();
                    diff.retargeted.push(m.name.clone());
                    members.push(self.spawn(spawner, m, settings));
                }
                None => {
                    diff.added.push(m.name.clone());
                    members.push(self.spawn(spawner, m, settings));
                }
            }
        }
        for gone in old {
            gone.handle.shutdown();
            diff.removed.push(gone.handle.name.clone());
        }
        diff
    }

    fn spawn(&self, spawner: &Spawner, m: &MachineConfig, settings: &MachineSettings) -> Member {
        let handle = spawn_machine(
            m.name.clone(),
            m.max_agents,
            m.tags.clone(),
            (spawner.connect)(m),
            self.store.clone(),
            settings.clone(),
            spawner.events.clone(),
        );
        Member {
            handle,
            spawned_from: Some((m.clone(), settings.clone())),
        }
    }

    pub fn views(&self) -> Vec<MachineView> {
        self.machines()
            .iter()
            .map(|m| {
                let s = m.snapshot();
                MachineView {
                    name: m.name.clone(),
                    max_agents: m.max_agents,
                    tags: m.tags.clone(),
                    live: s.live,
                    healthy: s.channel.accepts_dispatch(),
                }
            })
            .collect()
    }

    /// Try to place every queued task, oldest first. Serialised: a pass sees the
    /// live counts the previous pass left behind, because a machine actor
    /// refreshes its count before it answers a dispatch (see
    /// `Actor::handle_command`) and no two passes run at once. The claim inside
    /// the actor (`Store::claim_task`) is the second line of defence: it makes a
    /// double dispatch of one task impossible even if this lock were bypassed.
    pub async fn dispatch_queued(&self) {
        let _pass = self.dispatch_lock.lock().await;
        let queued = match self.store.queued_tasks() {
            Ok(q) => q,
            Err(err) => {
                tracing::error!(%err, "list queued");
                return;
            }
        };
        for task in queued {
            let Some(name) = pick_machine(&self.views(), &task.spec) else {
                continue;
            };
            let Some(handle) = self.get(&name) else {
                continue;
            };
            match handle.dispatch(task.id).await {
                Ok(t) => {
                    tracing::info!(task = %t.display_id(), machine = %name, state = %t.state, "dispatched")
                }
                Err(err) => {
                    tracing::warn!(task = %task.display_id(), machine = %name, %err, "dispatch failed")
                }
            }
        }
    }
}

pub struct Daemon {
    paths: Paths,
    store: Arc<Store>,
    fleet: Arc<Fleet>,
    scheduler: SchedulerHandle,
    events: broadcast::Sender<PastorEvent>,
}

/// SIGTERM and SIGHUP, alongside ctrl_c's SIGINT, so `run_with_listener` can
/// select over all three without an attribute on a `tokio::select!` branch
/// (the macro does not support `#[cfg(...)]` there). Unix-only underneath,
/// like the rest of this file's `tokio::net::UnixListener`; on any other
/// platform `recv` simply never resolves, leaving ctrl_c as the only way in.
#[cfg(unix)]
struct ExtraSignals {
    sigterm: tokio::signal::unix::Signal,
    sighup: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl ExtraSignals {
    fn new() -> std::io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(ExtraSignals {
            sigterm: signal(SignalKind::terminate())?,
            sighup: signal(SignalKind::hangup())?,
        })
    }

    /// The log line for whichever signal arrived first.
    async fn recv(&mut self) -> &'static str {
        tokio::select! {
            _ = self.sigterm.recv() => "shutting down on SIGTERM; agents keep running",
            _ = self.sighup.recv() => "shutting down on SIGHUP; agents keep running",
        }
    }
}

#[cfg(not(unix))]
struct ExtraSignals;

#[cfg(not(unix))]
impl ExtraSignals {
    fn new() -> std::io::Result<Self> {
        Ok(ExtraSignals)
    }

    async fn recv(&mut self) -> &'static str {
        std::future::pending().await
    }
}

impl Daemon {
    pub async fn start(
        paths: Paths,
        config: PastorConfig,
        flock: Flock,
        connect: Option<ConnectorFactory>,
    ) -> anyhow::Result<Daemon> {
        paths.ensure()?;
        let store = Arc::new(Store::open(&paths.db_file())?);
        let (events, log_rx) = broadcast::channel(1024);
        // Plugin event hooks read the broadcast on their own, subscribed here
        // for the same reason as the log: before any actor can emit.
        let hooks_rx = events.subscribe();
        let connect = connect.unwrap_or_else(|| endpoint_factory(paths.clone()));
        let fleet = Arc::new(Fleet::managed(store.clone(), events.clone(), connect));
        fleet.apply_flock(&flock, &machine_settings(&config)).await;
        // Subscribed in `start`, before any actor runs, so the log sees the
        // first events too. The log holds the fleet weakly (see `spawn_log`),
        // so dropping the daemon still winds the tasks down.
        let lookup: Arc<dyn crate::events::MachineLookup> = fleet.clone();
        crate::events::spawn_log(
            paths.events_file(),
            crate::events::DEFAULT_MAX_BYTES,
            store.clone(),
            Some(Arc::downgrade(&lookup)),
            log_rx,
        );
        crate::hooks::spawn(
            paths.clone(),
            store.clone(),
            Some(Arc::downgrade(&lookup)),
            hooks_rx,
        );
        let scheduler = Scheduler::new(
            paths.clone(),
            &config,
            store.clone(),
            fleet.clone(),
            events.clone(),
        )
        .with_plugins()
        .spawn();
        Ok(Daemon {
            paths,
            store,
            fleet,
            scheduler,
            events,
        })
    }

    pub fn socket_path(&self) -> PathBuf {
        self.paths.socket_file()
    }
    pub fn store(&self) -> Arc<Store> {
        self.store.clone()
    }
    pub fn subscribe(&self) -> broadcast::Receiver<PastorEvent> {
        self.events.subscribe()
    }
    pub fn fleet(&self) -> Arc<Fleet> {
        self.fleet.clone()
    }
    pub fn scheduler(&self) -> SchedulerHandle {
        self.scheduler.clone()
    }

    /// Take ownership of the daemon socket: refuse to steal it from a live or
    /// merely unresponsive daemon, replace it if nothing answers, bind and lock
    /// down its permissions. Split out of `run` so a second `pastor serve` can
    /// be refused here, before `start` spawns a single machine actor or touches
    /// the shared database — not after, which is what let a second daemon
    /// reconcile and mutate the store for up to the probe timeout before it
    /// finally bailed.
    async fn bind_socket(socket: &std::path::Path) -> anyhow::Result<tokio::net::UnixListener> {
        if socket.exists() {
            // Staleness is a property of the connect, not of the reply: a live
            // daemon mid-request (e.g. `dispatch_queued` against a slow or wedged
            // herdr) can go a while without answering a ping, and a busy daemon
            // looks exactly like a wedged one from the outside. Only a refused (or
            // absent) connect means nothing is actually listening; anything else
            // must be left alone rather than unlinked and stolen.
            match crate::ipc::probe_daemon(socket).await {
                DaemonProbe::Running => anyhow::bail!(
                    "another pastor daemon is already running on {}",
                    socket.display()
                ),
                DaemonProbe::Unresponsive => anyhow::bail!(
                    "a daemon is listening on {} but did not respond within 2s; \
                     remove the socket file by hand only if that daemon is dead",
                    socket.display()
                ),
                DaemonProbe::NotRunning => std::fs::remove_file(socket)?,
            }
        }
        let listener = tokio::net::UnixListener::bind(socket)?;
        std::fs::set_permissions(
            socket,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
        )?;
        Ok(listener)
    }

    /// Own the socket, then build the daemon: machine actors are only spawned
    /// once the socket is ours, so a second `pastor serve` bails on a live
    /// daemon before it ever reconciles or dispatches against the shared
    /// database. Used by both `serve` and anything that needs the same
    /// ordering under test.
    pub async fn bind_and_start(
        paths: Paths,
        config: PastorConfig,
        flock: Flock,
        connect: Option<ConnectorFactory>,
    ) -> anyhow::Result<(Daemon, tokio::net::UnixListener)> {
        paths.ensure()?;
        let listener = Daemon::bind_socket(&paths.socket_file()).await?;
        let daemon = Daemon::start(paths, config, flock, connect).await?;
        Ok((daemon, listener))
    }

    pub async fn run(self) -> anyhow::Result<()> {
        let listener = Daemon::bind_socket(&self.socket_path()).await?;
        self.run_with_listener(listener).await
    }

    /// The accept/tick/event loop, given a socket this daemon already owns.
    ///
    /// systemd counts SIGTERM, SIGHUP and SIGINT as a clean exit and will not
    /// restart a `Restart=on-failure` unit after any of them. Before this,
    /// `pastor serve` only handled SIGINT (ctrl-c), so a stray SIGTERM or
    /// SIGHUP from outside a terminal killed the head silently: no log line
    /// past the start message, the socket file left behind, and the unit
    /// stayed down. All three now take the same shutdown path.
    pub async fn run_with_listener(self, listener: tokio::net::UnixListener) -> anyhow::Result<()> {
        let socket = self.socket_path();
        let daemon = Arc::new(self);
        let mut extra_signals = ExtraSignals::new()?;
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, _) = accepted?;
                    let d = daemon.clone();
                    tokio::spawn(async move {
                        let (r, mut w) = stream.into_split();
                        let mut line = String::new();
                        if BufReader::new(r).read_line(&mut line).await.is_err() { return; }
                        let resp = match serde_json::from_str::<IpcRequest>(line.trim()) {
                            Ok(req) => d.handle(req).await,
                            Err(err) => IpcResponse::error("invalid_request", err),
                        };
                        let mut out = serde_json::to_string(&resp).unwrap_or_else(|e| format!("{{\"kind\":\"error\",\"code\":\"internal\",\"message\":\"{e}\"}}"));
                        out.push('\n');
                        let _ = w.write_all(out.as_bytes()).await;
                    });
                }
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("shutting down on SIGINT; agents keep running");
                    let _ = std::fs::remove_file(&socket);
                    return Ok(());
                }
                msg = extra_signals.recv() => {
                    tracing::info!("{msg}");
                    let _ = std::fs::remove_file(&socket);
                    return Ok(());
                }
            }
        }
    }

    pub async fn handle(&self, req: IpcRequest) -> IpcResponse {
        match req {
            IpcRequest::Ping => IpcResponse::Pong {
                version: env!("CARGO_PKG_VERSION").into(),
            },
            IpcRequest::Run { prompt, spec } => {
                // clap refuses this too; checked here as well so no other
                // client can queue a task dispatch can only fail.
                if spec.worktree && spec.repo.is_none() {
                    return IpcResponse::error(
                        "worktree_needs_repo",
                        "a worktree task needs a repo to branch from",
                    );
                }
                if let Some(m) = &spec.machine
                    && self.fleet.get(m).is_none()
                {
                    return IpcResponse::error(
                        "unknown_machine",
                        format!("machine {m} is not in the flock"),
                    );
                }
                let task = match self.store.insert_task(NewTask {
                    job: "run".into(),
                    item: serde_json::Value::Null,
                    prompt,
                    spec,
                }) {
                    Ok(t) => t,
                    Err(err) => return IpcResponse::error("store_error", err),
                };
                let _ = self.events.send(PastorEvent {
                    kind: "task.queued".into(),
                    task_id: Some(task.id),
                    machine: None,
                    job: Some(task.job.clone()),
                });
                self.fleet.dispatch_queued().await;
                match self.store.get_task(task.id) {
                    Ok(Some(t)) => IpcResponse::Task(t),
                    Ok(None) => IpcResponse::error("task_not_found", task.id),
                    Err(err) => IpcResponse::error("store_error", err),
                }
            }
            IpcRequest::List { filter } => match self.store.list_tasks(&filter) {
                Ok(ts) => IpcResponse::Tasks(ts),
                Err(err) => IpcResponse::error("store_error", err),
            },
            IpcRequest::TaskShow { id } => match self.store.get_task(id) {
                Ok(Some(t)) => IpcResponse::Task(t),
                Ok(None) => IpcResponse::error("task_not_found", format!("t-{id}")),
                Err(err) => IpcResponse::error("store_error", err),
            },
            IpcRequest::TaskRead { id, lines } => {
                let task = match self.store.get_task(id) {
                    Ok(Some(t)) => t,
                    Ok(None) => return IpcResponse::error("task_not_found", format!("t-{id}")),
                    Err(err) => return IpcResponse::error("store_error", err),
                };
                let Some(handle) = task.machine.as_ref().and_then(|m| self.fleet.get(m)) else {
                    return IpcResponse::error(
                        "no_machine",
                        format!("t-{id} is not on any machine"),
                    );
                };
                match handle.read(id, lines).await {
                    Ok(text) => IpcResponse::Text(text),
                    Err(err) => IpcResponse::error("read_failed", err),
                }
            }
            IpcRequest::FlockList => {
                IpcResponse::Machines(self.fleet.machines().iter().map(|m| m.snapshot()).collect())
            }
            IpcRequest::Tick { job, dry_run } => match self.scheduler.tick(job, dry_run).await {
                Ok(runs) => IpcResponse::Runs(runs),
                Err(err) => IpcResponse::error("scheduler_error", err),
            },
            IpcRequest::Reload => match self.scheduler.reload().await {
                Ok(jobs) => IpcResponse::Jobs(jobs),
                Err(err) => IpcResponse::error("scheduler_error", err),
            },
            IpcRequest::JobList => match self.scheduler.job_list().await {
                Ok(jobs) => IpcResponse::Jobs(jobs),
                Err(err) => IpcResponse::error("scheduler_error", err),
            },
            IpcRequest::JobRun { name } => match self.scheduler.fire(&name).await {
                Ok(Ok(msg)) => IpcResponse::Text(msg),
                Ok(Err(reason)) => IpcResponse::error("job_not_found", reason),
                Err(err) => IpcResponse::error("scheduler_error", err),
            },
            IpcRequest::TaskRetry { id } => self.retry(id).await,
            IpcRequest::TaskClose {
                id,
                remove_worktree,
            } => self.close(id, remove_worktree).await,
            IpcRequest::TaskPrune {
                states,
                older_than_secs,
            } => {
                if let Some(s) = states.iter().find(|s| !s.is_prunable()) {
                    return IpcResponse::error(
                        "not_prunable",
                        format!("{s} tasks cannot be pruned; only done, failed and closed"),
                    );
                }
                match self
                    .store
                    .prune(&states, std::time::Duration::from_secs(older_than_secs))
                {
                    Ok(out) => IpcResponse::Pruned(out),
                    Err(err) => IpcResponse::error("store_error", err),
                }
            }
        }
    }

    /// `TaskRetry`: a new queued row copying `id` (see `Store::insert_retry`),
    /// dispatched at once like a `Run`. Answers the new row as it stands after
    /// the dispatch pass.
    async fn retry(&self, id: i64) -> IpcResponse {
        // The store checks the state and copies in one statement; its error
        // says which check failed, so a row pruned by a concurrent request is
        // `task_not_found` and a storage failure is `store_error`.
        let task = match self.store.insert_retry(id) {
            Ok(t) => t,
            Err(err @ RetryError::NotFound(_)) => {
                return IpcResponse::error("task_not_found", err);
            }
            Err(err @ RetryError::NotRetryable { .. }) => {
                return IpcResponse::error("not_retryable", err);
            }
            Err(RetryError::Store(err)) => {
                return IpcResponse::error("store_error", format!("{err:#}"));
            }
        };
        let _ = self.events.send(PastorEvent {
            kind: "task.queued".into(),
            task_id: Some(task.id),
            machine: None,
            job: Some(task.job.clone()),
        });
        self.fleet.dispatch_queued().await;
        match self.store.get_task(task.id) {
            Ok(Some(t)) => IpcResponse::Task(t),
            Ok(None) => IpcResponse::error("task_not_found", task.display_id()),
            Err(err) => IpcResponse::error("store_error", err),
        }
    }

    /// `TaskClose`: through the actor of the task's machine, which closes the
    /// pane (or worktree) before the row. A task that never reached a machine,
    /// or whose machine has left the flock, only has its row closed, and a closed one is answered as it is. With
    /// no row, the machines are asked for an orphaned agent `t-<id>` (as
    /// their last reconcile found them).
    async fn close(&self, id: i64, remove_worktree: bool) -> IpcResponse {
        let row = match self.store.get_task(id) {
            Ok(r) => r,
            Err(err) => return IpcResponse::error("store_error", err),
        };
        let Some(t) = row else {
            let name = crate::task::Task::agent_name_for(id);
            let Some(handle) = self
                .fleet
                .machines()
                .into_iter()
                .find(|m| m.snapshot().orphans.contains(&name))
            else {
                return IpcResponse::error(
                    "task_not_found",
                    format!("{name}: no task row, and no machine reports an agent by that name"),
                );
            };
            return match handle.close(id, remove_worktree).await {
                Err(err) if err.downcast_ref::<OrphanClosed>().is_some() => {
                    IpcResponse::Text(err.to_string())
                }
                Ok(t) => IpcResponse::Task(t),
                Err(err) => IpcResponse::error("close_failed", format!("{err:#}")),
            };
        };
        self.close_row(t, remove_worktree).await
    }

    /// The rest of `close`, for the row `t` as it was read. A queued task
    /// can be claimed by a dispatch pass at any moment after that read, so
    /// its row is closed only while it is still queued (`close_queued`); on
    /// losing to a claim the row is read again and the close goes to the
    /// machine that took it, which closes the agent too.
    async fn close_row(&self, mut t: Task, remove_worktree: bool) -> IpcResponse {
        let id = t.id;
        // Runs twice at most: a row leaves `queued` only once, and a claim
        // sets `machine`, so the second pass routes to it.
        let machine = loop {
            if remove_worktree && !t.spec.worktree {
                return IpcResponse::error(
                    "no_worktree",
                    format!("{} has no worktree to remove", t.display_id()),
                );
            }
            // A closed task has no pane left to close, so repeating the close
            // answers the row without its machine, which may be gone or down.
            // A worktree removal still routes: the checkout may remain.
            if t.state == TaskState::Closed && !remove_worktree {
                return IpcResponse::Task(t);
            }
            if let Some(m) = t.machine.clone() {
                break m;
            }
            let was = t.state;
            let closed = if was == TaskState::Queued {
                match self.store.close_queued(id) {
                    Ok(Some(c)) => c,
                    // Claimed (or closed) since the read: read it again.
                    Ok(None) => match self.store.get_task(id) {
                        Ok(Some(fresh)) => {
                            t = fresh;
                            continue;
                        }
                        Ok(None) => return IpcResponse::error("task_not_found", t.display_id()),
                        Err(err) => return IpcResponse::error("store_error", err),
                    },
                    Err(err) => return IpcResponse::error("store_error", err),
                }
            } else {
                // Not queued and never on a machine: nothing can claim it.
                match self.store.close_task(id) {
                    Ok(c) => c,
                    Err(err) => return IpcResponse::error("store_error", err),
                }
            };
            if was != TaskState::Closed {
                let _ = self.events.send(PastorEvent {
                    kind: "task.closed".into(),
                    task_id: Some(id),
                    machine: None,
                    job: Some(closed.job.clone()),
                });
            }
            return IpcResponse::Task(closed);
        };
        let Some(handle) = self.fleet.get(&machine) else {
            // Its machine left the flock, so no actor owns the row and no
            // herdr can be asked: a plain close is only the row, but the
            // checkout lives on that machine and cannot be removed from here.
            if remove_worktree {
                return IpcResponse::error(
                    "unknown_machine",
                    format!(
                        "{} is on machine {machine}, which is not in the flock, so its worktree cannot be reached; close it without --remove-worktree",
                        t.display_id()
                    ),
                );
            }
            let closed = match self.store.close_task(id) {
                Ok(c) => c,
                Err(err) => return IpcResponse::error("store_error", err),
            };
            let _ = self.events.send(PastorEvent {
                kind: "task.closed".into(),
                task_id: Some(id),
                machine: Some(machine),
                job: Some(closed.job.clone()),
            });
            return IpcResponse::Task(closed);
        };
        match handle.close(id, remove_worktree).await {
            Ok(t) => IpcResponse::Task(t),
            Err(err) => IpcResponse::error("close_failed", format!("{err:#}")),
        }
    }
}

pub async fn serve(paths: Paths) -> anyhow::Result<()> {
    let config = PastorConfig::load(&paths.config_file())?;
    let flock = Flock::load(&paths.flock_file())?;
    anyhow::ensure!(
        !flock.machines.is_empty(),
        "flock is empty; add a machine with `pastor machine add`"
    );
    let (daemon, listener) = Daemon::bind_and_start(paths, config, flock, None).await?;
    tracing::info!(socket = %daemon.socket_path().display(), machines = daemon.fleet.machines().len(), "pastor serve");
    daemon.run_with_listener(listener).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::flock::MachineConfig;
    use crate::herdr::ConnectorExt;
    use crate::herdr::fake::FakeHerdr;
    use crate::scheduler::RunOutcome;
    use crate::store::NewTask;
    use crate::store::TaskFilter;
    use crate::task::DispatchSpec;
    use std::time::{Duration, Instant};

    fn machine(name: &str, max: u32) -> MachineConfig {
        MachineConfig {
            name: name.into(),
            local: false,
            ssh: None,
            command: Some(vec!["fake".into()]),
            session: "default".into(),
            max_agents: max,
            tags: vec![],
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
            timeout_secs: 60,
        }
    }

    /// The config every `daemon()` in these tests runs with; a test that
    /// applies a flock by hand passes `machine_settings(&test_config())` so
    /// it does not look like a timing change.
    fn test_config() -> PastorConfig {
        PastorConfig {
            settle: "1s".into(),
            ..Default::default()
        }
    }

    /// Hands out the fake registered under a machine's name, or a fresh one.
    fn factory(fakes: &[(&str, FakeHerdr)]) -> ConnectorFactory {
        let by_name: std::collections::HashMap<String, FakeHerdr> = fakes
            .iter()
            .map(|(n, f)| (n.to_string(), f.clone()))
            .collect();
        Arc::new(move |m: &MachineConfig| {
            Arc::new(by_name.get(&m.name).cloned().unwrap_or_else(FakeHerdr::new))
                as Arc<dyn Connector>
        })
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
        }
    }

    fn managed(fakes: &[(&str, FakeHerdr)]) -> (Arc<Fleet>, Arc<Store>) {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (events, _) = broadcast::channel(64);
        let fleet = Arc::new(Fleet::managed(store.clone(), events, factory(fakes)));
        (fleet, store)
    }

    fn flock_of(machines: &[(&str, u32)]) -> Flock {
        Flock {
            machines: machines.iter().map(|(n, max)| machine(n, *max)).collect(),
        }
    }

    async fn healthy(fleet: &Fleet, name: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !fleet.views().iter().any(|v| v.name == name && v.healthy) {
            assert!(Instant::now() < deadline, "{name} never connected");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn apply_flock_adds_removes_and_retargets() {
        let (fleet, _store) = managed(&[]);
        let d = fleet
            .apply_flock(&flock_of(&[("a", 2), ("b", 2)]), &fast())
            .await;
        assert_eq!(
            d,
            FlockDiff {
                added: vec!["a".into(), "b".into()],
                ..Default::default()
            }
        );
        let a = fleet.get("a").unwrap();
        let b = fleet.get("b").unwrap();

        let d = fleet.apply_flock(&flock_of(&[("a", 3)]), &fast()).await;
        assert_eq!(d.removed, vec!["b".to_string()]);
        assert_eq!(d.retargeted, vec!["a".to_string()], "max_agents changed");
        assert!(d.added.is_empty());
        assert!(fleet.get("b").is_none());
        assert_eq!(fleet.get("a").unwrap().max_agents, 3);
        assert_eq!(fleet.flock(), flock_of(&[("a", 3)]));
        wait_until("old actors stopped", || {
            a.tx.is_closed() && b.tx.is_closed()
        })
        .await;
    }

    /// Review Focus 2: an editor re-save, or `machine add other`, must not
    /// restart the actors of machines whose entry did not change.
    #[tokio::test]
    async fn unchanged_machines_keep_their_actor() {
        let (fleet, _store) = managed(&[]);
        fleet
            .apply_flock(&flock_of(&[("a", 2), ("b", 2)]), &fast())
            .await;
        let a = fleet.get("a").unwrap();
        let d = fleet
            .apply_flock(&flock_of(&[("a", 2), ("b", 2)]), &fast())
            .await;
        assert!(d.is_empty(), "{d:?}");
        let d = fleet
            .apply_flock(&flock_of(&[("a", 2), ("b", 2), ("c", 1)]), &fast())
            .await;
        assert_eq!(d.added, vec!["c".to_string()]);
        assert!(d.removed.is_empty() && d.retargeted.is_empty(), "{d:?}");
        assert!(
            a.tx.same_channel(&fleet.get("a").unwrap().tx),
            "a kept its actor"
        );
        assert!(!a.tx.is_closed());
    }

    #[tokio::test]
    async fn a_timing_change_replaces_every_actor() {
        let (fleet, _store) = managed(&[]);
        fleet.apply_flock(&flock_of(&[("a", 2)]), &fast()).await;
        let slower = MachineSettings {
            settle: Duration::from_millis(300),
            ..fast()
        };
        let d = fleet.apply_flock(&flock_of(&[("a", 2)]), &slower).await;
        assert_eq!(d.retargeted, vec!["a".to_string()]);
    }

    /// Review Focus 4: a machine removed and added back (or retargeted to a
    /// new address for the same host) gets its old tasks back: the new actor
    /// finds their panes, keeps them running and counts them again.
    #[tokio::test]
    async fn readding_a_machine_reconciles_its_old_tasks() {
        let fake = FakeHerdr::new();
        let (fleet, store) = managed(&[("a", fake.clone())]);
        fleet.apply_flock(&flock_of(&[("a", 2)]), &fast()).await;
        healthy(&fleet, "a").await;
        let t = store
            .insert_task(NewTask {
                job: "run".into(),
                item: serde_json::Value::Null,
                prompt: "p".into(),
                spec: spec(),
            })
            .unwrap();
        fleet.dispatch_queued().await;
        assert_eq!(
            store.get_task(t.id).unwrap().unwrap().state,
            TaskState::Running
        );

        let d = fleet.apply_flock(&Flock::default(), &fast()).await;
        assert_eq!(d.removed, vec!["a".to_string()]);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            store.get_task(t.id).unwrap().unwrap().state,
            TaskState::Running,
            "nothing watches a removed machine's tasks, so nothing changes them"
        );

        let d = fleet.apply_flock(&flock_of(&[("a", 2)]), &fast()).await;
        assert_eq!(d.added, vec!["a".to_string()]);
        healthy(&fleet, "a").await;
        tokio::time::sleep(Duration::from_millis(500)).await; // two reconciles
        assert_eq!(
            store.get_task(t.id).unwrap().unwrap().state,
            TaskState::Running
        );
        assert_eq!(fleet.get("a").unwrap().snapshot().live, 1);
    }

    #[tokio::test]
    async fn a_fixed_fleet_ignores_apply_flock() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let fleet = Fleet::new(vec![], store);
        assert!(
            fleet
                .apply_flock(&flock_of(&[("a", 2)]), &fast())
                .await
                .is_empty()
        );
        assert!(fleet.machines().is_empty());
    }

    async fn daemon(fakes: &[(&str, u32, FakeHerdr)]) -> (Daemon, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        let flock = Flock {
            machines: fakes.iter().map(|(n, max, _)| machine(n, *max)).collect(),
        };
        let named: Vec<(&str, FakeHerdr)> = fakes.iter().map(|(n, _, f)| (*n, f.clone())).collect();
        let d = Daemon::start(paths, test_config(), flock, Some(factory(&named)))
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while d.fleet().views().iter().any(|v| !v.healthy) {
            assert!(Instant::now() < deadline, "machines never connected");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        (d, tmp)
    }

    #[tokio::test]
    async fn run_dispatches_immediately() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let mut events = d.subscribe();
        let resp = d
            .handle(IpcRequest::Run {
                prompt: "hi".into(),
                spec: spec(),
            })
            .await;
        let IpcResponse::Task(t) = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(t.state, TaskState::Running);
        // Queued before dispatch picked it up.
        let ev = events.try_recv().expect("task.queued emitted");
        assert_eq!(ev.kind, "task.queued");
        assert_eq!(ev.task_id, Some(t.id));
        assert_eq!(ev.job.as_deref(), Some("run"));
        assert_eq!(events.try_recv().unwrap().kind, "task.running");
        assert_eq!(t.machine.as_deref(), Some("a"));
        let IpcResponse::Tasks(list) = d
            .handle(IpcRequest::List {
                filter: TaskFilter::default(),
            })
            .await
        else {
            panic!()
        };
        assert_eq!(list.len(), 1);
        let IpcResponse::Text(text) = d.handle(IpcRequest::TaskRead { id: t.id, lines: 5 }).await
        else {
            panic!()
        };
        assert!(text.contains("fake output"));
        let IpcResponse::Machines(ms) = d.handle(IpcRequest::FlockList).await else {
            panic!()
        };
        assert_eq!(ms[0].live, 1);
        assert_eq!(
            ms[0].pastor_version.as_deref(),
            Some("fake"),
            "the flock list carries what the machine answered on connect"
        );
    }

    #[tokio::test]
    async fn queued_task_dispatches_when_capacity_frees() {
        let fake = FakeHerdr::new();
        let (d, _tmp) = daemon(&[("a", 1, fake.clone())]).await;
        let IpcResponse::Task(first) = d
            .handle(IpcRequest::Run {
                prompt: "1".into(),
                spec: spec(),
            })
            .await
        else {
            panic!()
        };
        assert_eq!(first.state, TaskState::Running);
        let IpcResponse::Task(second) = d
            .handle(IpcRequest::Run {
                prompt: "2".into(),
                spec: spec(),
            })
            .await
        else {
            panic!()
        };
        assert_eq!(second.state, TaskState::Queued);
        d.fleet().dispatch_queued().await;
        assert_eq!(
            d.store.get_task(second.id).unwrap().unwrap().state,
            TaskState::Queued,
            "still no room"
        );
        fake.close_pane(first.pane_id.as_deref().unwrap());
        let deadline = Instant::now() + Duration::from_secs(5);
        while d.store.get_task(first.id).unwrap().unwrap().state != TaskState::Closed {
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        d.fleet().dispatch_queued().await;
        assert_eq!(
            d.store.get_task(second.id).unwrap().unwrap().state,
            TaskState::Running
        );
    }

    #[tokio::test]
    async fn pinned_unknown_machine_and_bad_ids_are_errors() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let resp = d
            .handle(IpcRequest::Run {
                prompt: "x".into(),
                spec: DispatchSpec {
                    machine: Some("zzz".into()),
                    ..spec()
                },
            })
            .await;
        let IpcResponse::Error { code, .. } = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(code, "unknown_machine");
        let IpcResponse::Error { code, .. } = d.handle(IpcRequest::TaskShow { id: 99 }).await
        else {
            panic!()
        };
        assert_eq!(code, "task_not_found");
    }

    #[tokio::test]
    async fn socket_round_trip() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let socket = d.socket_path();
        tokio::spawn(d.run());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !crate::ipc::daemon_running(&socket).await {
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let resp = crate::ipc::request(
            &socket,
            &IpcRequest::Run {
                prompt: "hi".into(),
                spec: spec(),
            },
        )
        .await
        .unwrap();
        assert!(matches!(resp, IpcResponse::Task(_)), "{resp:?}");
        // garbage in, error out, connection survives for the next client
        let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
        let (r, mut w) = stream.into_split();
        w.write_all(b"not json\n").await.unwrap();
        let mut line = String::new();
        BufReader::new(r).read_line(&mut line).await.unwrap();
        assert!(line.contains("invalid_request"), "{line}");
        assert!(crate::ipc::daemon_running(&socket).await);
    }

    /// `run` must replace a stale socket file left behind by an unclean shutdown
    /// (nothing is listening on it) instead of refusing to start. Shutdown-time
    /// removal of the socket is not exercised here: `run` only exits on
    /// ctrl-c/SIGTERM/SIGHUP, and sending one of those to this process would
    /// affect the whole test process. `tests/cli.rs` covers the SIGTERM path
    /// against a real `pastor serve` child instead.
    #[tokio::test]
    async fn run_replaces_a_stale_socket_file() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let socket = d.socket_path();
        std::fs::write(&socket, b"not a socket").unwrap();
        tokio::spawn(d.run());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !crate::ipc::daemon_running(&socket).await {
            assert!(Instant::now() < deadline, "daemon never started");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// A daemon that is mid-request and not answering pings (or a listener that
    /// never reads at all) must not have its socket unlinked by a second `pastor
    /// serve`: staleness is decided by whether the connect is refused, not by
    /// whether anything replies within the probe window.
    #[tokio::test]
    async fn run_refuses_to_replace_an_unresponsive_listener() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let socket = d.socket_path();
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            // Accept connections and never read or reply: unresponsive, not dead.
            let mut kept = Vec::new();
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => kept.push(stream),
                    Err(_) => return,
                }
            }
        });

        let err = tokio::time::timeout(Duration::from_secs(10), d.run())
            .await
            .expect("run must not hang waiting on the other daemon")
            .unwrap_err();
        assert!(err.to_string().contains("did not respond"), "{err}");
        assert!(
            socket.exists(),
            "an unresponsive daemon's socket file must not be removed"
        );
    }

    /// A second `pastor serve` must own the socket before it spawns a single
    /// machine actor: reconciling and mutating the shared database for up to
    /// the probe timeout before bailing (the old order) is the bug. With a
    /// live, responding daemon already on the socket, `bind_and_start` must
    /// fail before its own connectors are ever contacted.
    #[tokio::test]
    async fn bind_and_start_bails_before_spawning_actors_when_a_daemon_is_already_running() {
        let (first, tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let socket = first.socket_path();
        tokio::spawn(first.run());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !crate::ipc::daemon_running(&socket).await {
            assert!(Instant::now() < deadline, "first daemon never started");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        let flock = Flock {
            machines: vec![machine("a", 2)],
        };
        let fake = FakeHerdr::new();
        let connect = factory(&[("a", fake.clone())]);
        let err = match Daemon::bind_and_start(paths, PastorConfig::default(), flock, Some(connect))
            .await
        {
            Ok(_) => panic!("expected bind_and_start to bail on a live socket"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("already running"), "{err}");
        assert!(
            fake.requests().is_empty(),
            "the second daemon's machine actor must never have contacted its connector"
        );
    }

    /// Two passes at once (a tick and a `pastor task run`) against one machine with
    /// one free slot: the lock makes the second wait and see the first's task.
    #[tokio::test]
    async fn concurrent_dispatch_passes_do_not_over_dispatch() {
        let fake = FakeHerdr::new();
        fake.set_ready_after(Duration::from_millis(200));
        let (d, _tmp) = daemon(&[("a", 1, fake.clone())]).await;
        for p in ["1", "2"] {
            d.store()
                .insert_task(NewTask {
                    job: "run".into(),
                    item: serde_json::Value::Null,
                    prompt: p.into(),
                    spec: spec(),
                })
                .unwrap();
        }
        let fleet = d.fleet();
        tokio::join!(fleet.dispatch_queued(), fleet.dispatch_queued());
        let states: Vec<TaskState> = d
            .store()
            .list_tasks(&TaskFilter::default())
            .unwrap()
            .iter()
            .map(|t| t.state)
            .collect();
        assert_eq!(
            states.iter().filter(|s| **s == TaskState::Running).count(),
            1,
            "{states:?}"
        );
        assert_eq!(
            states.iter().filter(|s| **s == TaskState::Queued).count(),
            1,
            "{states:?}"
        );
        assert_eq!(
            fake.agents().len(),
            1,
            "one agent on a max_agents = 1 machine"
        );
    }

    #[tokio::test]
    async fn scheduler_ipc_ticks_lists_and_reloads() {
        let (d, tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        std::fs::create_dir_all(paths.jobs_dir()).unwrap();
        std::fs::write(
            paths.jobs_dir().join("clock.toml"),
            "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"tick {{ item.key }} {{ task.id }}\"\n",
        )
        .unwrap();
        let IpcResponse::Jobs(jobs) = d.handle(IpcRequest::Reload).await else {
            panic!()
        };
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "clock");
        let IpcResponse::Runs(runs) = d
            .handle(IpcRequest::Tick {
                job: Some("clock".into()),
                dry_run: true,
            })
            .await
        else {
            panic!()
        };
        assert_eq!(runs[0].outcome, RunOutcome::DryRun);
        assert_eq!(runs[0].created.len(), 1);
        let IpcResponse::Runs(runs) = d
            .handle(IpcRequest::Tick {
                job: Some("clock".into()),
                dry_run: false,
            })
            .await
        else {
            panic!()
        };
        assert_eq!(runs[0].outcome, RunOutcome::Ran);
        // The task was created and, the fleet having room, dispatched.
        let IpcResponse::Tasks(list) = d
            .handle(IpcRequest::List {
                filter: TaskFilter {
                    job: Some("clock".into()),
                    ..Default::default()
                },
            })
            .await
        else {
            panic!()
        };
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].state, TaskState::Running);
        assert!(
            list[0]
                .prompt
                .ends_with(&format!(" {}", list[0].display_id()))
        );
        let IpcResponse::Jobs(jobs) = d.handle(IpcRequest::JobList).await else {
            panic!()
        };
        assert!(jobs[0].last_result.as_deref().unwrap().starts_with("ok:"));
        let IpcResponse::Text(msg) = d
            .handle(IpcRequest::JobRun {
                name: "clock".into(),
            })
            .await
        else {
            panic!()
        };
        assert!(msg.contains("started"), "{msg}");
        let IpcResponse::Error { code, .. } = d
            .handle(IpcRequest::JobRun {
                name: "ghost".into(),
            })
            .await
        else {
            panic!()
        };
        assert_eq!(code, "job_not_found");
    }

    fn error_code(resp: IpcResponse) -> String {
        match resp {
            IpcResponse::Error { code, .. } => code,
            other => panic!("expected an error, got {other:?}"),
        }
    }

    async fn wait_until(what: &str, f: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !f() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn insert(d: &Daemon, state: TaskState) -> crate::task::Task {
        let mut t = d
            .store
            .insert_task(NewTask {
                job: "run".into(),
                item: serde_json::Value::Null,
                prompt: "p".into(),
                spec: spec(),
            })
            .unwrap();
        if state != TaskState::Queued {
            t.state = state;
            t.machine = Some("a".into());
            t.finished_at = Some(chrono::Utc::now() - chrono::Duration::days(5));
            d.store.update_task(&mut t).unwrap();
        }
        t
    }

    #[tokio::test]
    async fn retry_queues_a_copy_and_dispatches_it() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let failed = insert(&d, TaskState::Failed);
        let mut events = d.subscribe();
        let resp = d.handle(IpcRequest::TaskRetry { id: failed.id }).await;
        let IpcResponse::Task(t) = resp else {
            panic!("{resp:?}")
        };
        assert_ne!(t.id, failed.id);
        assert_eq!(t.retry_of, Some(failed.id));
        assert_eq!(t.state, TaskState::Running, "dispatched right away");
        let ev = events.try_recv().unwrap();
        assert_eq!((ev.kind.as_str(), ev.task_id), ("task.queued", Some(t.id)));

        assert_eq!(
            error_code(d.handle(IpcRequest::TaskRetry { id: t.id }).await),
            "not_retryable"
        );
        assert_eq!(
            error_code(d.handle(IpcRequest::TaskRetry { id: 99 }).await),
            "task_not_found"
        );
    }

    #[tokio::test]
    async fn retry_answers_a_storage_failure_as_store_error() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let failed = insert(&d, TaskState::Failed);
        // Reads work, only the insert fails: the way a full disk looks.
        d.store.execute_raw(
            "CREATE TRIGGER no_insert BEFORE INSERT ON tasks BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
        );
        assert_eq!(
            error_code(d.handle(IpcRequest::TaskRetry { id: failed.id }).await),
            "store_error"
        );
    }

    /// The close read the task queued, then a dispatch claimed and started
    /// it before the row was written. Closing the row alone would leave the
    /// agent running behind a closed task; the close must lose to the claim
    /// and go through the machine that took it.
    #[tokio::test]
    async fn closing_a_queued_task_that_a_dispatch_just_claimed_routes_to_its_machine() {
        let fake = FakeHerdr::new();
        let (d, _tmp) = daemon(&[("a", 2, fake.clone())]).await;
        let IpcResponse::Task(t) = d
            .handle(IpcRequest::Run {
                prompt: "x".into(),
                spec: spec(),
            })
            .await
        else {
            panic!()
        };
        assert_eq!(t.machine.as_deref(), Some("a"));
        assert_eq!(fake.agents().len(), 1);
        // What the close read before the claim landed.
        let mut seen = t.clone();
        seen.state = TaskState::Queued;
        seen.machine = None;
        seen.pane_id = None;
        seen.workspace_id = None;
        let resp = d.close_row(seen, false).await;
        let IpcResponse::Task(closed) = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(closed.state, TaskState::Closed);
        assert!(fake.agents().is_empty(), "the agent went with the task");
    }

    #[tokio::test]
    async fn close_goes_through_the_task_s_machine() {
        let fake = FakeHerdr::new();
        let (d, _tmp) = daemon(&[("a", 2, fake.clone())]).await;
        let IpcResponse::Task(t) = d
            .handle(IpcRequest::Run {
                prompt: "x".into(),
                spec: spec(),
            })
            .await
        else {
            panic!()
        };
        assert_eq!(
            error_code(
                d.handle(IpcRequest::TaskClose {
                    id: t.id,
                    remove_worktree: true
                })
                .await
            ),
            "no_worktree"
        );
        let resp = d
            .handle(IpcRequest::TaskClose {
                id: t.id,
                remove_worktree: false,
            })
            .await;
        let IpcResponse::Task(closed) = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(closed.state, TaskState::Closed);
        assert!(fake.agents().is_empty());

        // Never dispatched: only the row changes.
        let queued = insert(&d, TaskState::Queued);
        let IpcResponse::Task(c) = d
            .handle(IpcRequest::TaskClose {
                id: queued.id,
                remove_worktree: false,
            })
            .await
        else {
            panic!()
        };
        assert_eq!(c.state, TaskState::Closed);

        assert_eq!(
            error_code(
                d.handle(IpcRequest::TaskClose {
                    id: 99,
                    remove_worktree: false
                })
                .await
            ),
            "task_not_found"
        );
    }

    #[tokio::test]
    async fn closing_a_closed_task_again_needs_no_machine() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let mut t = insert(&d, TaskState::Closed);
        t.machine = Some("zzz".into());
        d.store.update_task(&mut t).unwrap();
        let t = d.store.get_task(t.id).unwrap().unwrap();
        let mut events = d.subscribe();
        let resp = d
            .handle(IpcRequest::TaskClose {
                id: t.id,
                remove_worktree: false,
            })
            .await;
        let IpcResponse::Task(again) = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(again.state, TaskState::Closed);
        assert_eq!(again.updated_at, t.updated_at, "the row is unchanged");
        assert!(events.try_recv().is_err(), "no second task.closed");

        // Its checkout may still be there, so removing it still needs the machine.
        let mut wt = insert(&d, TaskState::Closed);
        wt.spec.worktree = true;
        wt.machine = Some("zzz".into());
        d.store.update_task(&mut wt).unwrap();
        assert_eq!(
            error_code(
                d.handle(IpcRequest::TaskClose {
                    id: wt.id,
                    remove_worktree: true
                })
                .await
            ),
            "unknown_machine"
        );
    }

    #[tokio::test]
    async fn closing_a_task_on_a_removed_machine_closes_the_row_locally() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let mut gone = insert(&d, TaskState::Running);
        gone.machine = Some("zzz".into());
        d.store.update_task(&mut gone).unwrap();
        let finished = d.store.get_task(gone.id).unwrap().unwrap().finished_at;
        let mut events = d.subscribe();
        let resp = d
            .handle(IpcRequest::TaskClose {
                id: gone.id,
                remove_worktree: false,
            })
            .await;
        let IpcResponse::Task(closed) = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(closed.state, TaskState::Closed);
        assert_eq!(closed.finished_at, finished, "finished_at is kept");
        let stored = d.store.get_task(gone.id).unwrap().unwrap();
        assert_eq!(stored.state, TaskState::Closed);
        let ev = events.try_recv().expect("task.closed emitted");
        assert_eq!(ev.kind, "task.closed");
        assert_eq!(ev.task_id, Some(gone.id));
        assert_eq!(ev.job.as_deref(), Some("run"));
        assert!(events.try_recv().is_err(), "one task.closed only");

        // Its checkout is on a machine pastor cannot reach any more.
        let mut wt = insert(&d, TaskState::Running);
        wt.spec.worktree = true;
        wt.machine = Some("zzz".into());
        d.store.update_task(&mut wt).unwrap();
        let resp = d
            .handle(IpcRequest::TaskClose {
                id: wt.id,
                remove_worktree: true,
            })
            .await;
        let IpcResponse::Error { code, message } = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(code, "unknown_machine");
        assert!(
            message.contains("zzz") && message.contains("not in the flock"),
            "{message}"
        );
        assert!(message.contains("worktree"), "{message}");
        let stored = d.store.get_task(wt.id).unwrap().unwrap();
        assert_eq!(stored.state, TaskState::Running, "the row stays open");
    }

    #[tokio::test]
    async fn close_finds_an_orphan_with_no_row() {
        let fake = FakeHerdr::new();
        let ws = fake.workspace_create(None, "t-42").await.unwrap();
        fake.agent_start("t-42", "claude", &ws.root_pane.pane_id, &[])
            .await
            .unwrap();
        let (d, _tmp) = daemon(&[("a", 2, fake.clone())]).await;
        let fleet = d.fleet();
        wait_until("orphan", || {
            fleet.get("a").unwrap().snapshot().orphans == vec!["t-42".to_string()]
        })
        .await;
        let resp = d
            .handle(IpcRequest::TaskClose {
                id: 42,
                remove_worktree: false,
            })
            .await;
        let IpcResponse::Text(msg) = resp else {
            panic!("{resp:?}")
        };
        assert!(msg.contains("t-42") && msg.contains("orphan"), "{msg}");
        assert!(fake.agents().is_empty());
    }

    #[tokio::test]
    async fn prune_answers_the_count() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let old_done = insert(&d, TaskState::Done);
        let old_failed = insert(&d, TaskState::Failed);
        let resp = d
            .handle(IpcRequest::TaskPrune {
                states: vec![TaskState::Done],
                older_than_secs: 3 * 86400,
            })
            .await;
        let IpcResponse::Pruned(out) = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(out.pruned, 1);
        assert!(out.kept_worktrees.is_empty());
        assert!(d.store.get_task(old_done.id).unwrap().is_none());
        assert!(d.store.get_task(old_failed.id).unwrap().is_some());
        assert_eq!(
            error_code(
                d.handle(IpcRequest::TaskPrune {
                    states: vec![TaskState::Running],
                    older_than_secs: 1
                })
                .await
            ),
            "not_prunable"
        );
    }

    #[tokio::test]
    async fn run_refuses_a_worktree_without_a_repo() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let resp = d
            .handle(IpcRequest::Run {
                prompt: "x".into(),
                spec: DispatchSpec {
                    worktree: true,
                    ..spec()
                },
            })
            .await;
        assert_eq!(error_code(resp), "worktree_needs_repo");
        assert!(
            d.store
                .list_tasks(&TaskFilter::default())
                .unwrap()
                .is_empty(),
            "no row for a refused run"
        );
    }
}
