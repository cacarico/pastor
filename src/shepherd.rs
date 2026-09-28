//! A headless `pastor serve`, the shepherd: with a head set on another
//! machine, this one runs its own jobs and connector hooks and nothing else.
//! No queue, no machine actors, no task store: the items a job run finds go
//! to the head in one `IpcRequest::JobSubmit`, and the head's events come back through
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
use crate::daemon::{Answer, Daemon, Fleet, answer_on, jobs_answer};
use crate::events::{EventRecord, EventsPage};
use crate::head::RemoteHead;
use crate::ipc::{IPC_PROTOCOL, IpcRequest, IpcResponse, SHEPHERD_ROLE, request_line};
use crate::scheduler::{ConfigFingerprint, Scheduler, SchedulerHandle};
use crate::store::Store;

/// The meta key that holds the last head event handed to the hooks.
const CURSOR_KEY: &str = "head_event_seq";

/// How many head events one `EventsSince` asks for.
const PAGE: u32 = 200;

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
