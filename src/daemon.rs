use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::broadcast;

use crate::config::flock::Flock;
use crate::config::{PastorConfig, Paths};
use crate::dispatch::{MachineView, pick_machine};
use crate::herdr::{Connector, Endpoint};
use crate::ipc::{DaemonProbe, IpcRequest, IpcResponse};
use crate::machine::{MachineHandle, MachineSettings, PastorEvent, spawn_machine};
use crate::scheduler::{Scheduler, SchedulerHandle};
use crate::store::{NewTask, Store};

/// The machines plus the one lock every dispatch pass takes. Shared by the
/// daemon (a `pastor task run` dispatches inline) and the scheduler (each tick, and
/// after a job run queues tasks), so two passes never read the same capacity
/// snapshot and both fill the last slot.
pub struct Fleet {
    machines: Vec<MachineHandle>,
    store: Arc<Store>,
    dispatch_lock: tokio::sync::Mutex<()>,
}

impl Fleet {
    pub fn new(machines: Vec<MachineHandle>, store: Arc<Store>) -> Fleet {
        Fleet {
            machines,
            store,
            dispatch_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Every machine in flock order. Read-only: the set is fixed for the
    /// daemon's lifetime, so callers iterate and never hold a slot.
    pub fn machines(&self) -> &[MachineHandle] {
        &self.machines
    }

    pub fn get(&self, name: &str) -> Option<&MachineHandle> {
        self.machines.iter().find(|h| h.name == name)
    }

    pub fn views(&self) -> Vec<MachineView> {
        self.machines
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
        connectors: Option<Vec<Arc<dyn Connector>>>,
    ) -> anyhow::Result<Daemon> {
        paths.ensure()?;
        let store = Arc::new(Store::open(&paths.db_file())?);
        let (events, log_rx) = broadcast::channel(1024);
        // Plugin event hooks read the broadcast on their own, subscribed here
        // for the same reason as the log: before any actor can emit.
        let hooks_rx = events.subscribe();
        let settings = MachineSettings {
            settle: config.settle_duration(),
            reconcile_every: config.reconcile_duration(),
            request_timeout: config.request_timeout_duration(),
            agent_ready_timeout: config.agent_ready_timeout_duration(),
            poll_every: config.tick_duration(),
            ..Default::default()
        };
        let connectors: Vec<Arc<dyn Connector>> = match connectors {
            Some(c) => c,
            None => flock
                .machines
                .iter()
                .map(|m| Arc::new(Endpoint::from_machine(m, &paths)) as Arc<dyn Connector>)
                .collect(),
        };
        anyhow::ensure!(
            connectors.len() == flock.machines.len(),
            "one connector per machine"
        );
        let machines = flock
            .machines
            .iter()
            .zip(connectors)
            .map(|(m, c)| {
                spawn_machine(
                    m.name.clone(),
                    m.max_agents,
                    m.tags.clone(),
                    c,
                    store.clone(),
                    settings.clone(),
                    events.clone(),
                )
            })
            .collect();
        let fleet = Arc::new(Fleet::new(machines, store.clone()));
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
        connectors: Option<Vec<Arc<dyn Connector>>>,
    ) -> anyhow::Result<(Daemon, tokio::net::UnixListener)> {
        paths.ensure()?;
        let listener = Daemon::bind_socket(&paths.socket_file()).await?;
        let daemon = Daemon::start(paths, config, flock, connectors).await?;
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
    use crate::herdr::fake::FakeHerdr;
    use crate::scheduler::RunOutcome;
    use crate::store::NewTask;
    use crate::store::TaskFilter;
    use crate::task::{DispatchSpec, TaskState};
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

    async fn daemon(fakes: &[(&str, u32, FakeHerdr)]) -> (Daemon, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        let flock = Flock {
            machines: fakes.iter().map(|(n, max, _)| machine(n, *max)).collect(),
        };
        let connectors = fakes
            .iter()
            .map(|(_, _, f)| Arc::new(f.clone()) as Arc<dyn Connector>)
            .collect();
        let config = PastorConfig {
            settle: "1s".into(),
            ..Default::default()
        };
        let d = Daemon::start(paths, config, flock, Some(connectors))
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
        let connectors: Vec<Arc<dyn Connector>> = vec![Arc::new(fake.clone())];
        let err =
            match Daemon::bind_and_start(paths, PastorConfig::default(), flock, Some(connectors))
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
}
