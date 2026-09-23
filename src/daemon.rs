use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::broadcast;

use crate::config::flock::Flock;
use crate::config::{PastorConfig, Paths};
use crate::dispatch::{MachineView, pick_machine};
use crate::herdr::{Connector, Endpoint};
use crate::ipc::{IpcRequest, IpcResponse};
use crate::machine::{ChannelState, MachineHandle, MachineSettings, PastorEvent, spawn_machine};
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
            ..Default::default()
        };
        let connectors: Vec<Arc<dyn Connector>> = match connectors {
            Some(c) => c,
            None => flock
                .machines
                .iter()
                .map(|m| Arc::new(Endpoint::from_machine(m)) as Arc<dyn Connector>)
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

    pub async fn run(self) -> anyhow::Result<()> {
        let socket = self.socket_path();
        if socket.exists() {
            if crate::ipc::daemon_running(&socket).await {
                anyhow::bail!(
                    "another pastor serve is already listening on {}",
                    socket.display()
                );
            }
            std::fs::remove_file(&socket)?;
        }
        let listener = tokio::net::UnixListener::bind(&socket)?;
        std::fs::set_permissions(
            &socket,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
        )?;
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
                    Ok(ev) => tracing::info!(kind = %ev.kind, task = ?ev.task_id, machine = %ev.machine, "pastor event"),
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

    /// Try to place every queued task, oldest first. Called on each tick and after `run`.
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
                    healthy: s.channel == ChannelState::Connected,
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
        "flock is empty; add a machine with `pastor flock add`"
    );
    let daemon = Daemon::start(paths, config, flock, None).await?;
    tracing::info!(socket = %daemon.socket_path().display(), machines = daemon.machines.len(), "pastor serve");
    daemon.run().await
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
}
