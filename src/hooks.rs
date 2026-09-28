//! Connector event hooks: every `[[events]]` entry whose `on` lists an event's
//! type runs on the head with that event's `EventRecord` JSON on stdin and the
//! same environment as the connector's command. Hooks of different connectors run
//! concurrently; one connector's hooks run one at a time, in event order, so a
//! connector never races itself. A failed or timed-out hook is logged and never
//! retried. Hooks get each record from the events log task once it is in the
//! log, with the same sequence number; they never write the events log.
//!
//! A connector's `[finish]` command rides the same queue: the first time a
//! task of one of its jobs is seen `done` or `failed`, it runs once with a
//! JSON object about the task on stdin (`finish_input`). It never changes the
//! task; a failure or timeout is logged and announced as
//! `connector.finish_failed`.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use tokio::sync::{Notify, broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::config::Paths;
use crate::connector::exec::{self, Invocation, RunLog};
use crate::connector::manifest::{Finish, Hook};
use crate::connector::{Connector, Discovered, discover};
use crate::events::EventRecord;
use crate::machine::PastorEvent;
use crate::store::Store;
use crate::task::Task;

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

/// Does `hook` of `connector` want `rec`? Its `on` must list the type. With
/// `only_own`, a record about a job (a task event, `job.failed`) must be
/// about a job that uses this connector; a record about no job
/// (`machine.*`) is nobody's and passes.
pub fn wants(paths: &Paths, connector: &Connector, hook: &Hook, rec: &EventRecord) -> bool {
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
        Some(job) => job_connector(paths, &job).as_deref() == Some(connector.id.as_str()),
    }
}

/// `rec` as a hook of a connector that does not own its job sees it: the task
/// without its item (`null`), prompt (empty) and summary text (empty; the
/// outcome stays). Those carry the text of
/// another connector's items, private messages or issues, which a notifier
/// has no need to send off the host; the task's id, state, job and machine
/// stay.
fn without_task_content(rec: &EventRecord) -> EventRecord {
    let mut rec = rec.clone();
    if let Some(task) = rec.task.as_mut() {
        task.item = serde_json::Value::Null;
        task.prompt.clear();
        if let Some(s) = task.summary.as_mut() {
            s.text.clear();
        }
    }
    if let Some(s) = rec.summary.as_mut() {
        s.text.clear();
    }
    rec
}

/// What one run of a connector's command left behind.
struct Ran {
    done: exec::Finished,
    log: std::path::PathBuf,
}

/// What one connector command runs and hears.
struct Spec<'a> {
    argv: &'a [String],
    timeout: std::time::Duration,
    stdin: Vec<u8>,
    /// A first line for the run log; none when empty.
    heading: String,
}

/// Run `spec` as `connector`: in its directory, with its env (the job's
/// scratch dir when `owns_job`, else its own `@<id>` one). Its
/// output (stdout and stderr, redacted) goes to a run log under
/// `runs/@<connector id>/`, apart from the job's connector logs so these runs
/// never prune those. `Err` when the env or the log could not be made and
/// nothing ran.
async fn run_command(
    paths: &Paths,
    connector: &Connector,
    job: Option<&str>,
    owns_job: bool,
    spec: Spec<'_>,
) -> anyhow::Result<Ran> {
    let (env, redactor) = connector.command_env(paths, job, owns_job)?;
    let log = RunLog::create(&paths.runs_dir(&format!("@{}", connector.id)), redactor)?.shared();
    if !spec.heading.is_empty() {
        log.lock()
            .unwrap_or_else(|p| p.into_inner())
            .line(&spec.heading);
    }
    let inv = Invocation {
        argv: spec.argv.to_vec(),
        cwd: connector.dir.clone(),
        env,
        stdin: spec.stdin,
        timeout: Some(spec.timeout),
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
    Ok(Ran { done, log: path })
}

/// Run one hook for one record.
pub async fn run_hook(paths: &Paths, connector: &Connector, hook: &Hook, rec: &EventRecord) {
    let job = rec
        .job
        .clone()
        .or_else(|| rec.task.as_ref().map(|t| t.job.clone()));
    let job = job.filter(|j| crate::config::job::check_name(j).is_ok());
    let owns_job = job
        .as_deref()
        .is_some_and(|j| job_connector(paths, j).as_deref() == Some(connector.id.as_str()));
    let on = hook.on.join(",");
    let mut stdin = if owns_job {
        serde_json::to_vec(rec)
    } else {
        serde_json::to_vec(&without_task_content(rec))
    }
    .expect("an EventRecord serializes");
    stdin.push(b'\n');
    let ran = run_command(
        paths,
        connector,
        job.as_deref(),
        owns_job,
        Spec {
            argv: &hook.command,
            timeout: hook.timeout,
            stdin,
            heading: String::new(),
        },
    )
    .await;
    let ran = match ran {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!(connector = %connector.id, event = %rec.kind, hook = %on, err = %format!("{err:#}"), "hook not run");
            return;
        }
    };
    if ran.done.exit.success() {
        tracing::debug!(connector = %connector.id, event = %rec.kind, hook = %on, "hook ran");
    } else {
        // `reason` carries the stderr tail, already redacted by the run log.
        tracing::warn!(
            connector = %connector.id, event = %rec.kind, hook = %on,
            reason = %ran.done.reason(), log = %ran.log.display(),
            "hook failed; not retried"
        );
    }
}

/// A task's end, as its connector's finish command hears it on stdin: the
/// task row (its `item` too), `state` (`done` or `failed`), the job, the
/// branch its agent worked on (`null` when it had none), `last_output`,
/// the last lines pastor read from the agent's pane, empty when it read none,
/// and `summary`, how the round ended (`TaskSummary`, `null` when unknown).
fn finish_input(
    task: &Task,
    state: &str,
    last_output: &str,
    summary: Option<&crate::task::TaskSummary>,
) -> Vec<u8> {
    let branch = task
        .spec
        .checkout
        .as_ref()
        .map(|c| c.branch.as_str())
        .or(task.spec.branch.as_deref());
    let mut v = serde_json::to_vec(&serde_json::json!({
        "task": task,
        "state": state,
        "job": task.job,
        "branch": branch,
        "last_output": last_output,
        "summary": summary,
    }))
    .expect("a task-end object serializes");
    v.push(b'\n');
    v
}

/// What a finish command needs beyond its manifest entry.
struct FinishJob {
    finish: Finish,
    task: Task,
    state: &'static str,
    last_output: String,
    summary: Option<crate::task::TaskSummary>,
}

/// Run a connector's finish command for a task that ended. A failure of any
/// kind is logged and sent on `events` as `connector.finish_failed`; the task
/// is never touched.
async fn run_finish(
    paths: &Paths,
    connector: &Connector,
    f: &FinishJob,
    events: Option<&broadcast::Sender<PastorEvent>>,
) {
    let task = &f.task;
    let heading = format!(
        "finish {} ({}, job {})",
        task.display_id(),
        f.state,
        task.job
    );
    let ran = run_command(
        paths,
        connector,
        Some(&task.job),
        true,
        Spec {
            argv: &f.finish.command,
            timeout: f.finish.timeout,
            stdin: finish_input(task, f.state, &f.last_output, f.summary.as_ref()),
            heading,
        },
    )
    .await;
    let reason = match ran {
        Ok(r) if r.done.exit.success() => {
            tracing::debug!(connector = %connector.id, task = %task.display_id(), "finish ran");
            return;
        }
        Ok(r) => {
            let reason = r.done.reason();
            tracing::warn!(
                connector = %connector.id, task = %task.display_id(),
                reason = %reason, log = %r.log.display(),
                "finish failed; not retried"
            );
            reason
        }
        Err(err) => {
            let reason = format!("not run: {err:#}");
            tracing::warn!(connector = %connector.id, task = %task.display_id(), %reason, "finish not run");
            reason
        }
    };
    if let Some(events) = events {
        let _ = events.send(PastorEvent {
            kind: "connector.finish_failed".into(),
            task_id: Some(task.id),
            machine: None,
            job: Some(task.job.clone()),
            detail: Some(serde_json::json!({"connector": connector.id, "reason": reason})),
            summary: None,
        });
    }
}

struct Work {
    connector: Arc<Connector>,
    hooks: Vec<Hook>,
    finish: Option<FinishJob>,
    rec: Arc<EventRecord>,
}

/// Records a connector's queue holds while its hooks run. A hook may take its
/// whole timeout per record, so without a bound a slow or stuck hook under
/// a steady stream of events would grow the daemon without limit.
pub const HOOK_QUEUE_MAX: usize = 256;

/// One connector's pending records. Full, it drops the oldest: hooks are
/// notifications, and the newest state matters more than a backlog. Work
/// carrying a finish command is kept over plain notifications.
struct Queue {
    connector: String,
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
            // A finish command runs once per task and is marked as queued, so
            // it is never the one evicted; only when it is the newcomer and
            // the queue holds nothing else droppable does the queue grow.
            match st.items.iter().position(|w| w.finish.is_none()) {
                Some(at) => {
                    st.items.remove(at);
                }
                None if work.finish.is_none() => return,
                None => {}
            }
            st.dropped += 1;
            if st.dropped == 1 {
                tracing::warn!(
                    connector = %self.connector, max = self.max,
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
                            connector = %self.connector, dropped = st.dropped,
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

/// Hands each record to one worker per connector. Connectors are re-read for every
/// record: events are rare next to a directory listing, and it means an
/// install or uninstall takes effect for hooks without a reload.
pub struct Dispatcher {
    paths: Paths,
    queue_max: usize,
    workers: HashMap<String, (Arc<Queue>, JoinHandle<()>)>,
    /// Where a finishing task's pane tail waits (`Store::note_pane_tail`).
    store: Option<Arc<Store>>,
    /// Where `connector.finish_failed` goes. Held weakly: the events log
    /// task ends when every sender is gone, and it feeds this dispatcher.
    events: Option<broadcast::WeakSender<PastorEvent>>,
    /// Tasks whose finish command has been queued, so it runs once per task.
    finished: Finished,
}

/// Task ids whose finish command was queued. Bounded like the queues: past
/// `FINISHED_MAX` the oldest are forgotten, and a task that old ending again
/// would be the only one to run twice.
#[derive(Default)]
struct Finished {
    order: VecDeque<i64>,
    seen: std::collections::HashSet<i64>,
}

const FINISHED_MAX: usize = 4096;

impl Finished {
    /// `true` the first time `id` is offered.
    fn first(&mut self, id: i64) -> bool {
        if !self.seen.insert(id) {
            return false;
        }
        self.order.push_back(id);
        if self.order.len() > FINISHED_MAX
            && let Some(old) = self.order.pop_front()
        {
            self.seen.remove(&old);
        }
        true
    }
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
            store: None,
            events: None,
            finished: Finished::default(),
        }
    }

    /// Give finish commands the pane text the store kept for their task.
    pub fn with_store(mut self, store: Arc<Store>) -> Dispatcher {
        self.store = Some(store);
        self
    }

    /// Announce a failed finish command on `events`.
    pub fn with_events(mut self, events: broadcast::WeakSender<PastorEvent>) -> Dispatcher {
        self.events = Some(events);
        self
    }

    /// The finish command `connector` should run for `rec`: only for a task
    /// that reached `done` or `failed` in a job this connector owns, and only
    /// the first time. `closed` and every other event runs nothing.
    fn finish_for(&mut self, connector: &Connector, rec: &EventRecord) -> Option<FinishJob> {
        let state = match rec.kind.as_str() {
            "task.done" => "done",
            "task.failed" => "failed",
            _ => return None,
        };
        let finish = connector.manifest.finish.as_ref()?;
        let task = rec.task.as_ref()?;
        if job_connector(&self.paths, &task.job).as_deref() != Some(connector.id.as_str())
            || !self.finished.first(task.id)
        {
            return None;
        }
        let last_output = self
            .store
            .as_ref()
            .and_then(|s| s.take_pane_tail(task.id))
            .unwrap_or_default();
        Some(FinishJob {
            finish: finish.clone(),
            task: task.clone(),
            state,
            last_output,
            summary: rec.summary.clone().or_else(|| task.summary.clone()),
        })
    }

    pub fn deliver(&mut self, rec: EventRecord) {
        let connectors = match discover(&self.paths) {
            Ok(p) => p,
            Err(err) => {
                tracing::warn!(%err, "hooks: cannot read connectors");
                return;
            }
        };
        let rec = Arc::new(rec);
        for d in connectors {
            let Discovered::Valid(connector) = d else {
                continue;
            };
            let connector: Arc<Connector> = Arc::from(connector);
            let hooks: Vec<Hook> = connector
                .manifest
                .events
                .iter()
                .filter(|h| wants(&self.paths, &connector, h, &rec))
                .cloned()
                .collect();
            let finish = self.finish_for(&connector, &rec);
            if hooks.is_empty() && finish.is_none() {
                continue;
            }
            let (queue, worker) = self.workers.entry(connector.id.clone()).or_insert_with(|| {
                let q = Arc::new(Queue {
                    connector: connector.id.clone(),
                    max: self.queue_max,
                    state: Mutex::default(),
                    ready: Notify::new(),
                });
                let w = spawn_worker(self.paths.clone(), q.clone(), self.events.clone());
                (q, w)
            });
            // A worker only ends by panicking; a fresh one takes over the
            // same queue.
            if worker.is_finished() {
                *worker = spawn_worker(self.paths.clone(), queue.clone(), self.events.clone());
            }
            queue.push(Work {
                connector: connector.clone(),
                hooks,
                finish,
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

/// One connector's worker: its hooks, then its finish command, one after
/// another, in event order.
fn spawn_worker(
    paths: Paths,
    queue: Arc<Queue>,
    events: Option<broadcast::WeakSender<PastorEvent>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(w) = queue.next().await {
            for hook in &w.hooks {
                run_hook(&paths, &w.connector, hook, &w.rec).await;
            }
            if let Some(f) = &w.finish {
                let events = events.as_ref().and_then(|e| e.upgrade());
                run_finish(&paths, &w.connector, f, events.as_ref()).await;
            }
        }
    })
}

/// The daemon's hook runner: every record the events log task built
/// (`events::spawn_log`), numbered and already in the log, delivered in
/// order. `rx` is bounded (`events::HOOK_QUEUE_CAPACITY`): the log task
/// drops a record rather than block if this loop ever falls behind, which in
/// practice it does not, since `Dispatcher::deliver` below only queues. Ends
/// when the log task drops its sender; queued hooks still run. `events` is
/// where `connector.finish_failed` goes, and `store` holds the pane text a
/// finish command gets.
pub fn spawn(
    paths: Paths,
    store: Arc<Store>,
    events: broadcast::WeakSender<PastorEvent>,
    mut rx: mpsc::Receiver<EventRecord>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut dispatcher = Dispatcher::new(paths).with_store(store).with_events(events);
        while let Some(rec) = rx.recv().await {
            dispatcher.deliver(rec);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::manifest::MANIFEST_FILE;
    use crate::machine::PastorEvent;
    use crate::store::Store;
    use crate::task::DispatchSpec;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};
    use tokio::sync::broadcast;

    struct Env {
        _tmp: tempfile::TempDir,
        paths: Paths,
        out: PathBuf,
        store: Arc<Store>,
    }

    fn env() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        let out = tmp.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        Env {
            paths,
            out,
            store: Arc::new(Store::open_in_memory().unwrap()),
            _tmp: tmp,
        }
    }

    impl Env {
        /// A connector whose hooks are shell snippets; each gets `$OUT` (the
        /// test's output dir) and a secret `TOKEN` from its `.env`.
        fn connector(&self, id: &str, connector: bool, hooks: &[(&str, bool, &str, &str)]) {
            let dir = self.paths.connectors_dir().join(id);
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
            let envf = self.paths.connector_env_file(id);
            std::fs::create_dir_all(envf.parent().unwrap()).unwrap();
            std::fs::write(
                envf,
                format!("OUT={}\nTOKEN=sekrit-{id}-token\n", self.out.display()),
            )
            .unwrap();
        }

        /// Adds a `[finish]` table to an installed connector's manifest.
        fn finish(&self, id: &str, timeout: &str, script: &str) {
            let dir = self.paths.connectors_dir().join(id);
            std::fs::write(dir.join("finish.sh"), script).unwrap();
            let file = dir.join(MANIFEST_FILE);
            let mut m = std::fs::read_to_string(&file).unwrap();
            m.push_str(&format!(
                "[finish]\ncommand = [\"sh\", \"finish.sh\"]\ntimeout = \"{timeout}\"\n"
            ));
            std::fs::write(file, m).unwrap();
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
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let t = self
                .store
                .insert_job_task(
                    job,
                    "default",
                    &serde_json::json!({"key": format!("k-{kind}-{n}"), "title": "an item"}),
                    None,
                    |_| Ok(("p".into(), spec())),
                )
                .unwrap();
            EventRecord {
                summary: None,
                seq: 0,
                detail: None,
                at: chrono::Utc::now(),
                kind: kind.into(),
                job: Some(t.job.clone()),
                task: Some(t),
                machine: None,
                model: None,
            }
        }

        fn connectors(&self) -> Vec<Arc<Connector>> {
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
            allow: vec![],
            deny: vec![],
            repo: None,
            worktree: false,
            branch: None,
            machine: None,
            tags: vec![],
            timeout_secs: 60,
            checkout: None,
            reopen: None,
            agent_source: None,
            place: Default::default(),
            session_id: None,
        }
    }

    fn machine_record(kind: &str) -> EventRecord {
        EventRecord {
            summary: None,
            seq: 0,
            detail: None,
            at: chrono::Utc::now(),
            kind: kind.into(),
            task: None,
            job: None,
            machine: None,
            model: None,
        }
    }

    #[test]
    fn on_and_only_own_decide_who_gets_a_record() {
        let e = env();
        e.connector(
            "slack",
            true,
            &[
                ("\"task.done\", \"task.blocked\"", true, "5s", "true"),
                ("\"task.done\", \"machine.lost\"", false, "5s", "true"),
            ],
        );
        e.job("support", "slack");
        e.job("other", "clock");
        let p = &e.connectors()[0];
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
    async fn a_hook_gets_the_record_on_stdin_with_the_connector_env_and_redacted_logs() {
        let e = env();
        e.connector(
            "slack",
            true,
            &[(
                "\"task.done\"",
                true,
                "5s",
                "cat > \"$OUT/stdin.tmp\"; echo \"$PASTOR_CONNECTOR_ID $PASTOR_JOB $(basename \"$PASTOR_CONNECTOR_STATE_DIR\") $(pwd)\" > \"$OUT/env\"; echo \"token $TOKEN\"; echo \"err $TOKEN\" >&2; mv \"$OUT/stdin.tmp\" \"$OUT/stdin\"",
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
        assert_eq!(got["task"]["item"], rec.task.as_ref().unwrap().item);
        assert_eq!(got["task"]["prompt"], "p");
        let env_line = std::fs::read_to_string(e.out.join("env")).unwrap();
        let dir = std::fs::canonicalize(e.paths.connectors_dir().join("slack")).unwrap();
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

    /// A hook hearing about a task of a job another connector owns gets the
    /// record without the task's item and prompt, and its own `@<id>`
    /// scratch dir rather than the job's, which belongs to the owner.
    #[tokio::test]
    async fn a_hook_of_another_connector_gets_no_item_prompt_or_scratch_dir() {
        let e = env();
        e.connector(
            "notify",
            false,
            &[(
                "\"task.done\"",
                false,
                "5s",
                "cat > \"$OUT/stdin.tmp\"; echo \"$PASTOR_JOB $(basename \"$PASTOR_CONNECTOR_STATE_DIR\")\" > \"$OUT/env\"; mv \"$OUT/stdin.tmp\" \"$OUT/stdin\"",
            )],
        );
        e.connector("slack", true, &[]);
        e.job("support", "slack");
        let mut d = Dispatcher::new(e.paths.clone());
        let mut rec = e.task_record("task.done", "support");
        assert!(!rec.task.as_ref().unwrap().prompt.is_empty());
        rec.summary = Some(crate::task::TaskSummary {
            round: 1,
            outcome: crate::task::Outcome::NoSummary,
            text: "private pane text".into(),
            source: crate::task::SummarySource::Pane,
            at: chrono::Utc::now(),
        });
        d.deliver(rec.clone());
        let got: serde_json::Value = serde_json::from_str(&e.wait_for("stdin").await).unwrap();
        assert_eq!(got["task"]["id"], rec.task.as_ref().unwrap().id);
        assert_eq!(got["task"]["item"], serde_json::Value::Null);
        assert_eq!(got["task"]["prompt"], "");
        // The outcome, not what the summary says.
        assert_eq!(got["summary"]["outcome"], "no summary");
        assert_eq!(got["summary"]["text"], "");
        assert_eq!(got["job"], "support");
        let env_line = std::fs::read_to_string(e.out.join("env")).unwrap();
        assert_eq!(env_line.trim(), "support @notify");
    }

    /// Two hooks of one connector run one after the other; another connector's
    /// hook runs meanwhile.
    #[tokio::test]
    async fn sequential_within_a_connector_concurrent_across_connectors() {
        let e = env();
        e.connector(
            "a",
            false,
            &[
                ("\"task.done\"", false, "5s", "touch \"$OUT/a1-start\"; sleep 0.5; touch \"$OUT/a1-end\""),
                ("\"task.done\"", false, "5s", "[ -e \"$OUT/a1-end\" ] && echo yes > \"$OUT/a2-after-a1\" || echo no > \"$OUT/a2-after-a1\""),
            ],
        );
        e.connector(
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
    /// overruns its timeout is killed and the connector's queue moves on.
    #[tokio::test]
    async fn failures_are_not_retried_and_timeouts_are_bounded() {
        let e = env();
        e.connector(
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

    /// A connector whose hook is slower than its events keeps only the newest
    /// `queue_max` waiting; the oldest go, so a stuck hook cannot grow the
    /// daemon's memory. (A current-thread runtime: the worker takes nothing
    /// until the test awaits, so all five are queued first.)
    #[tokio::test]
    async fn a_full_queue_drops_the_oldest_events() {
        let e = env();
        e.connector(
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

    const FINISH_SCRIPT: &str = "cat >> \"$OUT/finish.tmp\"; echo \"$PASTOR_CONNECTOR_ID $PASTOR_JOB $TOKEN\" >> \"$OUT/finish-env\"; echo \"said $TOKEN\"; mv \"$OUT/finish.tmp\" \"$OUT/finish\"";

    fn dispatcher(
        e: &Env,
    ) -> (
        Dispatcher,
        broadcast::Sender<PastorEvent>,
        broadcast::Receiver<PastorEvent>,
    ) {
        let (tx, rx) = broadcast::channel(16);
        let d = Dispatcher::new(e.paths.clone())
            .with_store(e.store.clone())
            .with_events(tx.downgrade());
        (d, tx, rx)
    }

    /// A finished task runs its connector's `[finish]` command once, with the
    /// connector's env, and the task-end object on stdin.
    #[tokio::test]
    async fn a_finishing_task_runs_the_finish_command_with_the_task_end_object() {
        let e = env();
        e.connector("gh", true, &[]);
        e.finish("gh", "5s", FINISH_SCRIPT);
        e.job("support", "gh");
        let (mut d, _tx, _rx) = dispatcher(&e);
        let mut rec = e.task_record("task.done", "support");
        let task = rec.task.as_mut().unwrap();
        task.spec.checkout = Some(Box::new(crate::task::Checkout {
            branch: "fix/it".into(),
            path: "/w".into(),
            already_open: false,
        }));
        e.store
            .note_pane_tail(task.id, "did the thing\nPR: https://example.org/pr/1\n");
        rec.summary = Some(crate::task::TaskSummary {
            round: 1,
            outcome: crate::task::Outcome::Done,
            text: "done: PR 1".into(),
            source: crate::task::SummarySource::Agent,
            at: chrono::Utc::now(),
        });
        let task = rec.task.as_ref().unwrap();
        let id = task.id;
        let item = task.item.clone();
        d.deliver(rec);
        let got: serde_json::Value = serde_json::from_str(&e.wait_for("finish").await).unwrap();
        assert_eq!(got["state"], "done");
        assert_eq!(got["job"], "support");
        assert_eq!(got["task"]["id"], id);
        assert_eq!(got["task"]["item"], item);
        assert_eq!(got["branch"], "fix/it");
        assert_eq!(got["summary"]["outcome"], "done");
        assert_eq!(got["summary"]["text"], "done: PR 1");
        assert_eq!(
            got["last_output"],
            "did the thing\nPR: https://example.org/pr/1"
        );
        assert_eq!(
            std::fs::read_to_string(e.out.join("finish-env"))
                .unwrap()
                .trim(),
            "gh support sekrit-gh-token"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
        let logs = std::fs::read_dir(e.paths.runs_dir("@gh")).unwrap();
        let text: String = logs
            .map(|f| std::fs::read_to_string(f.unwrap().path()).unwrap())
            .collect();
        assert!(text.contains("stdout: said [redacted:TOKEN]"), "{text}");
        assert!(!text.contains("sekrit"), "{text}");
    }

    /// No pane text and no branch: `last_output` is empty, `branch` null. A
    /// failed task runs the command too.
    #[tokio::test]
    async fn a_failed_task_runs_it_too_with_no_output_and_no_branch() {
        let e = env();
        e.connector("gh", true, &[]);
        e.finish("gh", "5s", FINISH_SCRIPT);
        e.job("support", "gh");
        let (mut d, _tx, _rx) = dispatcher(&e);
        d.deliver(e.task_record("task.failed", "support"));
        let got: serde_json::Value = serde_json::from_str(&e.wait_for("finish").await).unwrap();
        assert_eq!(got["state"], "failed");
        assert_eq!(got["last_output"], "");
        assert_eq!(got["branch"], serde_json::Value::Null);
    }

    /// Once per task: the same task reaching done again, or failed after
    /// done, or being closed, does not run it again. Another task does.
    #[tokio::test]
    async fn a_task_finishes_once_and_closed_runs_nothing() {
        let e = env();
        e.connector("gh", true, &[]);
        e.finish("gh", "5s", "echo x >> \"$OUT/runs\"");
        e.job("support", "gh");
        let (mut d, _tx, _rx) = dispatcher(&e);
        let done = e.task_record("task.done", "support");
        let mut again = done.clone();
        again.kind = "task.failed".into();
        let mut closed = done.clone();
        closed.kind = "task.closed".into();
        d.deliver(done.clone());
        d.deliver(closed);
        d.deliver(done);
        d.deliver(again);
        e.wait_for("runs").await;
        d.deliver(e.task_record("task.done", "support"));
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::fs::read_to_string(e.out.join("runs"))
            .unwrap_or_default()
            .lines()
            .count()
            < 2
        {
            assert!(Instant::now() < deadline, "the second task never ran it");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            std::fs::read_to_string(e.out.join("runs"))
                .unwrap()
                .lines()
                .count(),
            2,
            "one run per task"
        );
    }

    /// Only the connector that owns the job runs its finish command; a
    /// one-off task and another connector's job run nobody's.
    #[tokio::test]
    async fn only_the_owning_connector_finishes_a_task() {
        let e = env();
        e.connector("gh", true, &[]);
        e.finish("gh", "5s", "echo gh >> \"$OUT/runs\"");
        e.connector("other", true, &[]);
        e.finish("other", "5s", "echo other >> \"$OUT/runs\"");
        e.job("support", "other");
        e.job("mine", "gh");
        let (mut d, _tx, _rx) = dispatcher(&e);
        d.deliver(e.task_record("task.done", "support"));
        d.deliver(e.task_record("task.done", "run"));
        d.deliver(e.task_record("task.done", "mine"));
        e.wait_for("runs").await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        let runs = std::fs::read_to_string(e.out.join("runs")).unwrap();
        let mut runs: Vec<&str> = runs.lines().collect();
        runs.sort();
        assert_eq!(runs, ["gh", "other"]);
    }

    /// A timeout is logged and announced as `connector.finish_failed`, and the
    /// connector's queue moves on; the task row is not touched.
    #[tokio::test]
    async fn a_finish_timeout_is_logged_and_emits_finish_failed() {
        let e = env();
        e.connector(
            "gh",
            true,
            &[("\"task.queued\"", true, "5s", "echo hooked > \"$OUT/hook\"")],
        );
        e.finish("gh", "1s", "echo slow; sleep 30");
        e.job("support", "gh");
        let (mut d, _tx, mut rx) = dispatcher(&e);
        let rec = e.task_record("task.done", "support");
        let id = rec.task.as_ref().unwrap().id;
        d.deliver(rec);
        d.deliver(e.task_record("task.queued", "support"));
        let ev = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("finish_failed within the bound")
            .unwrap();
        assert_eq!(ev.kind, "connector.finish_failed");
        assert_eq!(ev.task_id, Some(id));
        assert_eq!(ev.job.as_deref(), Some("support"));
        let detail = ev.detail.unwrap();
        assert_eq!(detail["connector"], "gh");
        assert!(
            detail["reason"].as_str().unwrap().contains("timed out"),
            "{detail}"
        );
        assert_eq!(
            e.wait_for("hook").await.trim(),
            "hooked",
            "the queue went on"
        );
        let logs = std::fs::read_dir(e.paths.runs_dir("@gh")).unwrap();
        let text: String = logs
            .map(|f| std::fs::read_to_string(f.unwrap().path()).unwrap())
            .collect();
        assert!(text.contains("stdout: slow"), "{text}");
        assert_eq!(
            e.store.get_task(id).unwrap().unwrap().state,
            crate::task::TaskState::Queued,
            "the task is as it was"
        );
    }

    /// A command that exits non-zero is a failure too, with its stderr in the
    /// reason.
    #[tokio::test]
    async fn a_failing_finish_command_emits_finish_failed_with_its_stderr() {
        let e = env();
        e.connector("gh", true, &[]);
        e.finish("gh", "5s", "echo boom >&2; exit 3");
        e.job("support", "gh");
        let (mut d, _tx, mut rx) = dispatcher(&e);
        d.deliver(e.task_record("task.failed", "support"));
        let ev = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ev.kind, "connector.finish_failed");
        assert!(
            ev.detail.unwrap()["reason"]
                .as_str()
                .unwrap()
                .contains("boom")
        );
    }

    /// A connector with hooks or a command but no `[finish]` behaves as it
    /// did: its hook runs, nothing else does, and no event comes.
    #[tokio::test]
    async fn a_connector_without_finish_is_unchanged() {
        let e = env();
        e.connector(
            "gh",
            true,
            &[("\"task.done\"", true, "5s", "echo hooked > \"$OUT/hook\"")],
        );
        e.job("support", "gh");
        let (mut d, _tx, mut rx) = dispatcher(&e);
        d.deliver(e.task_record("task.done", "support"));
        assert_eq!(e.wait_for("hook").await.trim(), "hooked");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(rx.try_recv().is_err());
        assert_eq!(
            std::fs::read_dir(&e.out).unwrap().count(),
            1,
            "only the hook wrote"
        );
    }

    /// Hooks get the record the events log task built and wrote: the same
    /// sequence number as the log line.
    #[tokio::test]
    async fn spawn_runs_hooks_on_the_logged_record() {
        let e = env();
        e.connector(
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
            .insert_job_task(
                "run",
                "default",
                &serde_json::json!({"key": "k"}),
                None,
                |_| Ok(("p".into(), spec())),
            )
            .unwrap();
        let (tx, rx) = broadcast::channel(8);
        let (fwd, hooks_rx) = mpsc::channel(crate::events::HOOK_QUEUE_CAPACITY);
        let log_path = e.paths.events_file();
        std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        let log = crate::events::spawn_log(
            log_path.clone(),
            crate::events::DEFAULT_MAX_BYTES,
            store.clone(),
            None,
            rx,
            Some(fwd),
        );
        let h = spawn(e.paths.clone(), store.clone(), tx.downgrade(), hooks_rx);
        for kind in ["task.queued", "task.done"] {
            tx.send(PastorEvent {
                detail: None,
                kind: kind.into(),
                task_id: Some(rec.id),
                machine: None,
                job: None,
                summary: None,
            })
            .unwrap();
        }
        let got: serde_json::Value = serde_json::from_str(&e.wait_for("r").await).unwrap();
        assert_eq!(got["task"]["id"], rec.id);
        assert_eq!(got["job"], "run", "filled from the task row");
        assert_eq!(got["seq"], 2);
        let logged = crate::events::read(&log_path, None).unwrap();
        assert_eq!(logged.last().unwrap().seq, 2);
        drop(tx);
        for h in [log, h] {
            tokio::time::timeout(Duration::from_secs(5), h)
                .await
                .unwrap()
                .unwrap();
        }
    }
}
