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
use crate::store::{NewTask, Store};

pub struct Daemon {
    paths: Paths,
    config: PastorConfig,
    store: Arc<Store>,
    machines: Vec<MachineHandle>,
    events: broadcast::Sender<PastorEvent>,
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
        let (events, _) = broadcast::channel(1024);
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
        Ok(Daemon {
            paths,
            config,
            store,
            machines,
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
    pub async fn run_with_listener(self, listener: tokio::net::UnixListener) -> anyhow::Result<()> {
        let socket = self.socket_path();
        let daemon = Arc::new(self);
        let mut tick = tokio::time::interval(daemon.config.tick_duration());
        let mut events = daemon.events.subscribe();
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
                _ = tick.tick() => daemon.dispatch_queued().await,
                ev = events.recv() => match ev {
                    Ok(ev) => tracing::info!(kind = %ev.kind, task = ?ev.task_id, machine = ?ev.machine, job = ?ev.job, "pastor event"),
                    Err(broadcast::error::RecvError::Lagged(n)) => tracing::warn!(n, "event log lagged"),
                    Err(_) => {}
                },
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("shutting down; agents keep running");
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
                    && !self.machines.iter().any(|h| &h.name == m)
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
                self.dispatch_queued().await;
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
                let Some(handle) = task
                    .machine
                    .as_ref()
                    .and_then(|m| self.machines.iter().find(|h| &h.name == m))
                else {
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
                IpcResponse::Machines(self.machines.iter().map(|m| m.snapshot()).collect())
            }
        }
    }

    /// Try to place every queued task, oldest first. Called on each tick and after `run`,
    /// and concurrent callers already exist today: each accepted IPC connection is its
    /// own spawned task, so a tick and any number of in-flight `Run` requests can all be
    /// awaiting this at once. No task is ever dispatched twice because `run_dispatch`
    /// re-checks the task is still `Queued` inside the actor's own serialized command
    /// loop, not because of anything here. The capacity snapshot this loop's
    /// `pick_machine` reads can still be stale under that concurrency and over-dispatch
    /// past `max_agents` before the next tick's `refresh_live` catches up; that's a known
    /// gap, not something this comment claims is handled.
    pub async fn dispatch_queued(&self) {
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
            let Some(handle) = self.machines.iter().find(|h| h.name == name) else {
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

    fn views(&self) -> Vec<MachineView> {
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
}

pub async fn serve(paths: Paths) -> anyhow::Result<()> {
    let config = PastorConfig::load(&paths.config_file())?;
    let flock = Flock::load(&paths.flock_file())?;
    anyhow::ensure!(
        !flock.machines.is_empty(),
        "flock is empty; add a machine with `pastor machine add`"
    );
    let (daemon, listener) = Daemon::bind_and_start(paths, config, flock, None).await?;
    tracing::info!(socket = %daemon.socket_path().display(), machines = daemon.machines.len(), "pastor serve");
    daemon.run_with_listener(listener).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::flock::MachineConfig;
    use crate::herdr::fake::FakeHerdr;
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
        while d.views().iter().any(|v| !v.healthy) {
            assert!(Instant::now() < deadline, "machines never connected");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        (d, tmp)
    }

    #[tokio::test]
    async fn run_dispatches_immediately() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
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
        d.dispatch_queued().await;
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
        d.dispatch_queued().await;
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
    /// removal of the socket is not exercised here: `run` only exits on ctrl-c,
    /// and sending that signal from a test would affect the whole test process.
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
}
