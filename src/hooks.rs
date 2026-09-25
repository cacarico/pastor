//! Plugin event hooks: every `[[events]]` entry whose `on` lists an event's
//! type runs on the head with that event's `EventRecord` JSON on stdin and the
//! same environment as the plugin's connector. Hooks of different plugins run
//! concurrently; one plugin's hooks run one at a time, in event order, so a
//! plugin never races itself. A failed or timed-out hook is logged and never
//! retried. Hooks read the daemon's broadcast on their own; they never write
//! the events log.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, Weak};

use tokio::sync::{Notify, broadcast};
use tokio::task::JoinHandle;

use crate::config::Paths;
use crate::events::{EventRecord, MachineLookup};
use crate::machine::PastorEvent;
use crate::plugin::exec::{self, Invocation, RunLog};
use crate::plugin::manifest::Hook;
use crate::plugin::{Discovered, Plugin, discover};
use crate::store::Store;

/// The connector a job file names, read without validating the rest of the
/// file: ownership only needs `connector.use`. `None` for a one-off task's
/// `run`, a missing or unreadable file, or a name that is not a job name.
pub fn job_connector(paths: &Paths, job: &str) -> Option<String> {
    crate::config::job::check_name(job).ok()?;
    let path = crate::config::job::job_path(&paths.jobs_dir(), job);
    let text = std::fs::read_to_string(path).ok()?;
    let v: toml::Value = toml::from_str(&text).ok()?;
    v.get("connector")?.get("use")?.as_str().map(str::to_string)
}

/// Does `hook` of `plugin` want `rec`? Its `on` must list the type. With
/// `only_own`, a record about a job (a task event, `job.failed`) must be
/// about a job whose connector is this plugin; a record about no job
/// (`machine.*`) is nobody's and passes.
pub fn wants(paths: &Paths, plugin: &Plugin, hook: &Hook, rec: &EventRecord) -> bool {
    if !hook.on.contains(&rec.kind) {
        return false;
    }
    if !hook.only_own {
        return true;
    }
    let job = rec
        .job
        .clone()
        .or_else(|| rec.task.as_ref().map(|t| t.job.clone()));
    match job {
        None => true,
        Some(job) => job_connector(paths, &job).as_deref() == Some(plugin.id.as_str()),
    }
}

/// Run one hook for one record. Its output (stdout and stderr, redacted)
/// goes to a run log under `runs/@<plugin id>/`, apart from the job's
/// connector logs so hooks never prune those.
pub async fn run_hook(paths: &Paths, plugin: &Plugin, hook: &Hook, rec: &EventRecord) {
    let job = rec
        .job
        .clone()
        .or_else(|| rec.task.as_ref().map(|t| t.job.clone()));
    let job = job.filter(|j| crate::config::job::check_name(j).is_ok());
    let on = hook.on.join(",");
    let prepared = plugin
        .command_env(paths, job.as_deref())
        .and_then(|(env, redactor)| {
            let log = RunLog::create(&paths.runs_dir(&format!("@{}", plugin.id)), redactor)?;
            Ok((env, log.shared()))
        });
    let (env, log) = match prepared {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(plugin = %plugin.id, event = %rec.kind, hook = %on, err = %format!("{err:#}"), "hook not run");
            return;
        }
    };
    let mut stdin = serde_json::to_vec(rec).expect("an EventRecord serializes");
    stdin.push(b'\n');
    let inv = Invocation {
        argv: hook.command.clone(),
        cwd: plugin.dir.clone(),
        env,
        stdin,
        timeout: Some(hook.timeout),
    };
    let out_log = log.clone();
    let done = exec::run(inv, log.clone(), |line| {
        out_log
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .line(&format!("stdout: {line}"));
    })
    .await;
    let path = log
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .path()
        .to_path_buf();
    if done.exit.success() {
        tracing::debug!(plugin = %plugin.id, event = %rec.kind, hook = %on, "hook ran");
    } else {
        // `reason` carries the stderr tail, already redacted by the run log.
        tracing::warn!(
            plugin = %plugin.id, event = %rec.kind, hook = %on,
            reason = %done.reason(), log = %path.display(),
            "hook failed; not retried"
        );
    }
}

struct Work {
    plugin: Arc<Plugin>,
    hooks: Vec<Hook>,
    rec: Arc<EventRecord>,
}

/// Records a plugin's queue holds while its hooks run. A hook may take its
/// whole timeout per record, so without a bound a slow or stuck hook under
/// a steady stream of events would grow the daemon without limit.
pub const HOOK_QUEUE_MAX: usize = 256;

/// One plugin's pending records. Full, it drops the oldest: hooks are
/// notifications, and the newest state matters more than a backlog.
struct Queue {
    plugin: String,
    max: usize,
    state: Mutex<QueueState>,
    ready: Notify,
}

#[derive(Default)]
struct QueueState {
    items: VecDeque<Work>,
    /// Dropped since the queue was last empty; one warning per episode.
    dropped: usize,
    /// The dispatcher is gone: finish what is queued, then stop.
    closed: bool,
}

impl Queue {
    fn lock(&self) -> std::sync::MutexGuard<'_, QueueState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn push(&self, work: Work) {
        let mut st = self.lock();
        if st.items.len() >= self.max {
            st.items.pop_front();
            st.dropped += 1;
            if st.dropped == 1 {
                tracing::warn!(
                    plugin = %self.plugin, max = self.max,
                    "hook queue full; dropping the oldest events until its hooks catch up"
                );
            }
        }
        st.items.push_back(work);
        drop(st);
        self.ready.notify_one();
    }

    /// The next record, waiting for one; `None` once closed and empty.
    async fn next(&self) -> Option<Work> {
        loop {
            {
                let mut st = self.lock();
                if let Some(w) = st.items.pop_front() {
                    if st.items.is_empty() && st.dropped > 0 {
                        tracing::info!(
                            plugin = %self.plugin, dropped = st.dropped,
                            "hook queue caught up"
                        );
                        st.dropped = 0;
                    }
                    return Some(w);
                }
                if st.closed {
                    return None;
                }
            }
            // `notify_one` keeps a permit when nobody waits, so a push
            // between the check above and this await is not lost.
            self.ready.notified().await;
        }
    }

    fn close(&self) {
        self.lock().closed = true;
        self.ready.notify_one();
    }
}

/// Hands each record to one worker per plugin. Plugins are re-read for every
/// record: events are rare next to a directory listing, and it means an
/// install or uninstall takes effect for hooks without a reload.
pub struct Dispatcher {
    paths: Paths,
    queue_max: usize,
    workers: HashMap<String, (Arc<Queue>, JoinHandle<()>)>,
}

impl Dispatcher {
    pub fn new(paths: Paths) -> Dispatcher {
        Dispatcher::with_queue_max(paths, HOOK_QUEUE_MAX)
    }

    pub fn with_queue_max(paths: Paths, queue_max: usize) -> Dispatcher {
        Dispatcher {
            paths,
            queue_max: queue_max.max(1),
            workers: HashMap::new(),
        }
    }

    pub fn deliver(&mut self, rec: EventRecord) {
        let plugins = match discover(&self.paths) {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(%err, "hooks: cannot read plugins");
                return;
            }
        };
        let rec = Arc::new(rec);
        for d in plugins {
            let Discovered::Valid(plugin) = d else {
                continue;
            };
            let plugin: Arc<Plugin> = Arc::from(plugin);
            let hooks: Vec<Hook> = plugin
                .manifest
                .events
                .iter()
                .filter(|h| wants(&self.paths, &plugin, h, &rec))
                .cloned()
                .collect();
            if hooks.is_empty() {
                continue;
            }
            let (queue, worker) = self.workers.entry(plugin.id.clone()).or_insert_with(|| {
                let q = Arc::new(Queue {
                    plugin: plugin.id.clone(),
                    max: self.queue_max,
                    state: Mutex::default(),
                    ready: Notify::new(),
                });
                let w = spawn_worker(self.paths.clone(), q.clone());
                (q, w)
            });
            // A worker only ends by panicking; a fresh one takes over the
            // same queue.
            if worker.is_finished() {
                *worker = spawn_worker(self.paths.clone(), queue.clone());
            }
            queue.push(Work {
                plugin: plugin.clone(),
                hooks,
                rec: rec.clone(),
            });
        }
    }
}

/// Queued hooks still run after the dispatcher goes; the workers stop once
/// their queues are empty.
impl Drop for Dispatcher {
    fn drop(&mut self) {
        for (queue, _) in self.workers.values() {
            queue.close();
        }
    }
}

/// One plugin's worker: its hooks, one after another, in event order.
fn spawn_worker(paths: Paths, queue: Arc<Queue>) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(w) = queue.next().await {
            for hook in &w.hooks {
                run_hook(&paths, &w.plugin, hook, &w.rec).await;
            }
        }
    })
}

/// The daemon's hook runner: subscribe before any actor runs, build a record
/// per event (as the events log does), deliver it. The fleet is held weakly
/// for the same reason `events::spawn_log` does. Ends when the broadcast
/// closes; queued hooks still run.
pub fn spawn(
    paths: Paths,
    store: Arc<Store>,
    fleet: Option<Weak<dyn MachineLookup>>,
    mut rx: broadcast::Receiver<PastorEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut dispatcher = Dispatcher::new(paths);
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let lookup = fleet.as_ref().and_then(Weak::upgrade);
                    let rec = EventRecord::build(&ev, &store, lookup.as_deref());
                    dispatcher.deliver(rec);
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(n, "hooks lagged; their events were dropped")
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::manifest::MANIFEST_FILE;
    use crate::task::DispatchSpec;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    struct Env {
        _tmp: tempfile::TempDir,
        paths: Paths,
        out: PathBuf,
        store: Store,
    }

    fn env() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        let out = tmp.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        Env {
            paths,
            out,
            store: Store::open_in_memory().unwrap(),
            _tmp: tmp,
        }
    }

    impl Env {
        /// A plugin whose hooks are shell snippets; each gets `$OUT` (the
        /// test's output dir) and a secret `TOKEN` from its `.env`.
        fn plugin(&self, id: &str, connector: bool, hooks: &[(&str, bool, &str, &str)]) {
            let dir = self.paths.plugins_dir().join(id);
            std::fs::create_dir_all(&dir).unwrap();
            let mut m = format!("id = \"{id}\"\nversion = \"0.1.0\"\n[secrets.TOKEN]\n");
            if connector {
                m.push_str("[connector]\ncommand = [\"true\"]\n");
            }
            for (i, (on, only_own, timeout, script)) in hooks.iter().enumerate() {
                std::fs::write(dir.join(format!("h{i}.sh")), script).unwrap();
                m.push_str(&format!(
                    "[[events]]\non = [{on}]\nonly_own = {only_own}\ntimeout = \"{timeout}\"\ncommand = [\"sh\", \"h{i}.sh\"]\n"
                ));
            }
            std::fs::write(dir.join(MANIFEST_FILE), m).unwrap();
            let envf = self.paths.plugin_env_file(id);
            std::fs::create_dir_all(envf.parent().unwrap()).unwrap();
            std::fs::write(
                envf,
                format!("OUT={}\nTOKEN=sekrit-{id}-token\n", self.out.display()),
            )
            .unwrap();
        }

        fn job(&self, name: &str, connector: &str) {
            std::fs::create_dir_all(self.paths.jobs_dir()).unwrap();
            std::fs::write(
                crate::config::job::job_path(&self.paths.jobs_dir(), name),
                format!("every = \"1h\"\n[connector]\nuse = \"{connector}\"\n[dispatch]\nprompt = \"p\"\n"),
            )
            .unwrap();
        }

        fn task_record(&self, kind: &str, job: &str) -> EventRecord {
            let t = self
                .store
                .insert_job_task(
                    job,
                    "default",
                    &serde_json::json!({"key": format!("k-{kind}")}),
                    |_| Ok(("p".into(), spec())),
                )
                .unwrap();
            EventRecord {
                detail: None,
                at: chrono::Utc::now(),
                kind: kind.into(),
                job: Some(t.job.clone()),
                task: Some(t),
                machine: None,
            }
        }

        fn plugins(&self) -> Vec<Arc<Plugin>> {
            discover(&self.paths)
                .unwrap()
                .into_iter()
                .filter_map(|d| match d {
                    Discovered::Valid(p) => Some(Arc::from(p)),
                    _ => None,
                })
                .collect()
        }

        async fn wait_for(&self, file: &str) -> String {
            let path = self.out.join(file);
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Ok(s) = std::fs::read_to_string(&path)
                    && !s.is_empty()
                {
                    return s;
                }
                assert!(Instant::now() < deadline, "{file} never appeared");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
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
            checkout: None,
            reopen: None,
        }
    }

    fn machine_record(kind: &str) -> EventRecord {
        EventRecord {
            detail: None,
            at: chrono::Utc::now(),
            kind: kind.into(),
            task: None,
            job: None,
            machine: None,
        }
    }

    #[test]
    fn on_and_only_own_decide_who_gets_a_record() {
        let e = env();
        e.plugin(
            "slack",
            true,
            &[
                ("\"task.done\", \"task.blocked\"", true, "5s", "true"),
                ("\"task.done\", \"machine.lost\"", false, "5s", "true"),
            ],
        );
        e.job("support", "slack");
        e.job("other", "clock");
        let p = &e.plugins()[0];
        let (own, all) = (&p.manifest.events[0], &p.manifest.events[1]);
        let done_support = e.task_record("task.done", "support");
        let done_other = e.task_record("task.done", "other");
        let done_oneoff = e.task_record("task.done", "run");
        let queued = e.task_record("task.queued", "support");
        assert!(wants(&e.paths, p, own, &done_support));
        assert!(!wants(&e.paths, p, own, &done_other), "another job's task");
        assert!(
            !wants(&e.paths, p, own, &done_oneoff),
            "a one-off task is nobody's"
        );
        assert!(!wants(&e.paths, p, own, &queued), "not in `on`");
        assert!(wants(&e.paths, p, all, &done_other), "only_own = false");
        assert!(wants(&e.paths, p, all, &machine_record("machine.lost")));
        let mut own_lost = own.clone();
        own_lost.on.push("machine.lost".into());
        assert!(
            wants(&e.paths, p, &own_lost, &machine_record("machine.lost")),
            "a record about no job passes only_own"
        );
        assert_eq!(job_connector(&e.paths, "../support"), None);
    }

    #[tokio::test]
    async fn a_hook_gets_the_record_on_stdin_with_the_plugin_env_and_redacted_logs() {
        let e = env();
        e.plugin(
            "slack",
            true,
            &[(
                "\"task.done\"",
                true,
                "5s",
                "cat > \"$OUT/stdin.tmp\"; echo \"$PASTOR_PLUGIN_ID $PASTOR_JOB $(basename \"$PASTOR_PLUGIN_STATE_DIR\") $(pwd)\" > \"$OUT/env\"; echo \"token $TOKEN\"; echo \"err $TOKEN\" >&2; mv \"$OUT/stdin.tmp\" \"$OUT/stdin\"",
            )],
        );
        e.job("support", "slack");
        let mut d = Dispatcher::new(e.paths.clone());
        let rec = e.task_record("task.done", "support");
        d.deliver(rec.clone());
        let got: serde_json::Value = serde_json::from_str(&e.wait_for("stdin").await).unwrap();
        assert_eq!(got["type"], "task.done");
        assert_eq!(got["task"]["id"], rec.task.as_ref().unwrap().id);
        assert_eq!(got["job"], "support");
        let env_line = std::fs::read_to_string(e.out.join("env")).unwrap();
        let dir = std::fs::canonicalize(e.paths.plugins_dir().join("slack")).unwrap();
        assert_eq!(
            env_line.trim(),
            format!("slack support support {}", dir.display())
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        let logs = std::fs::read_dir(e.paths.runs_dir("@slack")).unwrap();
        let text: String = logs
            .map(|f| std::fs::read_to_string(f.unwrap().path()).unwrap())
            .collect();
        assert!(text.contains("stdout: token [redacted:TOKEN]"), "{text}");
        assert!(text.contains("err [redacted:TOKEN]"), "{text}");
        assert!(!text.contains("sekrit"), "{text}");
    }

    /// Two hooks of one plugin run one after the other; another plugin's
    /// hook runs meanwhile.
    #[tokio::test]
    async fn sequential_within_a_plugin_concurrent_across_plugins() {
        let e = env();
        e.plugin(
            "a",
            false,
            &[
                ("\"task.done\"", false, "5s", "touch \"$OUT/a1-start\"; sleep 0.5; touch \"$OUT/a1-end\""),
                ("\"task.done\"", false, "5s", "[ -e \"$OUT/a1-end\" ] && echo yes > \"$OUT/a2-after-a1\" || echo no > \"$OUT/a2-after-a1\""),
            ],
        );
        e.plugin(
            "b",
            false,
            &[(
                "\"task.done\"",
                false,
                "5s",
                "i=0; while [ ! -e \"$OUT/a1-start\" ] && [ $i -lt 100 ]; do sleep 0.01; i=$((i+1)); done; [ -e \"$OUT/a1-end\" ] && echo no > \"$OUT/b-during-a1\" || echo yes > \"$OUT/b-during-a1\"",
            )],
        );
        let mut d = Dispatcher::new(e.paths.clone());
        d.deliver(e.task_record("task.done", "run"));
        assert_eq!(e.wait_for("b-during-a1").await.trim(), "yes");
        assert_eq!(e.wait_for("a2-after-a1").await.trim(), "yes");
    }

    /// A failing hook runs once per event, never again; a hook that
    /// overruns its timeout is killed and the plugin's queue moves on.
    #[tokio::test]
    async fn failures_are_not_retried_and_timeouts_are_bounded() {
        let e = env();
        e.plugin(
            "a",
            false,
            &[
                (
                    "\"task.failed\"",
                    false,
                    "5s",
                    "echo x >> \"$OUT/fails\"; exit 3",
                ),
                ("\"task.blocked\"", false, "1s", "sleep 30"),
                (
                    "\"task.blocked\"",
                    false,
                    "5s",
                    "echo after > \"$OUT/after-timeout\"",
                ),
            ],
        );
        let mut d = Dispatcher::new(e.paths.clone());
        d.deliver(e.task_record("task.failed", "run"));
        e.wait_for("fails").await;
        let started = Instant::now();
        d.deliver(e.task_record("task.blocked", "run"));
        e.wait_for("after-timeout").await;
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(
            std::fs::read_to_string(e.out.join("fails")).unwrap(),
            "x\n",
            "one event, one run"
        );
    }

    /// A plugin whose hook is slower than its events keeps only the newest
    /// `queue_max` waiting; the oldest go, so a stuck hook cannot grow the
    /// daemon's memory. (A current-thread runtime: the worker takes nothing
    /// until the test awaits, so all five are queued first.)
    #[tokio::test]
    async fn a_full_queue_drops_the_oldest_events() {
        let e = env();
        e.plugin(
            "a",
            false,
            &[(
                "\"task.done\"",
                false,
                "5s",
                "r=$(cat); while [ ! -e \"$OUT/go\" ]; do sleep 0.02; done; echo \"$r\" | grep -o '\"job\":\"j[0-9]*\"' >> \"$OUT/ran\"",
            )],
        );
        let mut d = Dispatcher::with_queue_max(e.paths.clone(), 2);
        for i in 0..5 {
            let mut rec = machine_record("task.done");
            rec.job = Some(format!("j{i}"));
            d.deliver(rec);
        }
        std::fs::write(e.out.join("go"), "").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::fs::read_to_string(e.out.join("ran"))
            .unwrap_or_default()
            .lines()
            .count()
            < 2
        {
            assert!(Instant::now() < deadline, "the hooks never ran");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            std::fs::read_to_string(e.out.join("ran")).unwrap(),
            "\"job\":\"j3\"\n\"job\":\"j4\"\n",
            "the newest two ran, once each"
        );
    }

    #[tokio::test]
    async fn spawn_builds_records_from_the_broadcast() {
        let e = env();
        e.plugin(
            "a",
            false,
            &[(
                "\"task.done\"",
                false,
                "5s",
                "cat > \"$OUT/r.tmp\"; mv \"$OUT/r.tmp\" \"$OUT/r\"",
            )],
        );
        let store = Arc::new(Store::open_in_memory().unwrap());
        let rec = store
            .insert_job_task("run", "default", &serde_json::json!({"key": "k"}), |_| {
                Ok(("p".into(), spec()))
            })
            .unwrap();
        let (tx, rx) = broadcast::channel(8);
        let h = spawn(e.paths.clone(), store, None, rx);
        tx.send(PastorEvent {
            detail: None,
            kind: "task.done".into(),
            task_id: Some(rec.id),
            machine: None,
            job: None,
        })
        .unwrap();
        let got: serde_json::Value = serde_json::from_str(&e.wait_for("r").await).unwrap();
        assert_eq!(got["task"]["id"], rec.id);
        assert_eq!(got["job"], "run", "filled from the task row");
        drop(tx);
        tokio::time::timeout(Duration::from_secs(5), h)
            .await
            .unwrap()
            .unwrap();
    }
}
