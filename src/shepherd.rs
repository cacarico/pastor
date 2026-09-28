//! A headless `pastor serve`, the shepherd: with a head set on another
//! machine, this one runs its own jobs and connector hooks and nothing else.
//! No queue, no machine actors, no task store: each item a job finds goes to
//! the head as `IpcRequest::JobTask`, and the head's events come back through
//! `EventsSince` for the hooks here. Its own small database
//! (`Paths::shepherd_db_file`) keeps the jobs' state and seen keys and how far
//! it has read the head's events.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc};

use crate::cli::CliError;
use crate::config::{PastorConfig, Paths};
use crate::daemon::{Answer, Daemon, Fleet, JobTaskForward, answer_on, jobs_answer};
use crate::events::{EventRecord, EventsPage};
use crate::head::RemoteHead;
use crate::ipc::{IPC_PROTOCOL, IpcRequest, IpcResponse, SHEPHERD_ROLE, request_line};
use crate::scheduler::{ConfigFingerprint, Scheduler, SchedulerHandle};
use crate::store::Store;
use crate::task::Task;

/// The meta key that holds the last head event handed to the hooks.
const CURSOR_KEY: &str = "head_event_seq";

/// How many head events one `EventsSince` asks for.
const PAGE: u32 = 200;

/// How long a request to the head may take. A `JobTask` waits for the
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

/// What a job run's fleet sends each item through: a `JobTask` to the head,
/// which answers the task it queued.
fn forward(ask: Ask) -> JobTaskForward {
    Arc::new(move |req| {
        let ask = ask.clone();
        Box::pin(async move {
            match ask(req).await? {
                IpcResponse::Task(t) => Ok::<Task, anyhow::Error>(t),
                other => anyhow::bail!("the head answered a job task with {other:?}"),
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
    async fn answer(&self, req: IpcRequest, from_task: Option<String>) -> IpcResponse {
        // The rule the head applies (`Daemon::handle_from`), from this
        // machine's pastor.toml.
        if let Some(task) = &from_task
            && req.changes_fleet()
            && !PastorConfig::load(&self.paths.config_file()).is_ok_and(|c| c.agents_change_fleet)
        {
            return IpcResponse::error("agent_refused", crate::daemon::agent_refusal(task));
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
    let every = config.tick_duration();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        loop {
            tick.tick().await;
            follow.pass().await;
        }
    });
    let fleet = Arc::new(Fleet::headless(store.clone(), forward(ask)));
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
            } else {
                if page.gap {
                    tracing::warn!(
                        after,
                        oldest = ?page.oldest,
                        "head events past the cursor were rotated out of its log; the hooks missed them"
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
                let log = log.lock().unwrap();
                let newest = log.iter().map(|r| r.seq).max();
                let events: Vec<EventRecord> = log
                    .iter()
                    .filter(|r| r.seq > after)
                    .take(limit as usize)
                    .cloned()
                    .collect();
                Ok(IpcResponse::Events(EventsPage {
                    events,
                    gap: false,
                    oldest: None,
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
        let fleet = Arc::new(Fleet::headless(store.clone(), forward(ask)));
        let config = PastorConfig::default();
        let scheduler = Scheduler::new(paths.clone(), &config, store, fleet, events)
            .headless()
            .spawn();
        let s = Shepherd {
            paths,
            scheduler,
            head: "user@pi-1".into(),
        };
        let IpcResponse::Pong { role, protocol, .. } = s.answer(IpcRequest::Ping, None).await
        else {
            panic!()
        };
        assert_eq!(role.as_deref(), Some(SHEPHERD_ROLE));
        assert_eq!(protocol, IPC_PROTOCOL);
        assert!(matches!(
            s.answer(IpcRequest::JobList, None).await,
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
                None,
            )
            .await,
        );
        assert_eq!(c, "shepherd_unsupported");
        assert!(m.contains("user@pi-1"), "{m}");
        let (c, _) = code(
            s.answer(IpcRequest::JobRun { name: "x".into() }, Some("t-3".into()))
                .await,
        );
        assert_eq!(c, "agent_refused");
    }
}
