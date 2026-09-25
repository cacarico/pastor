//! The tick. `run_job` is one job's pass: ask its connector, drop seen keys,
//! render and queue a task per new item, record the run. `Scheduler` (below,
//! Task 11) owns the loop that calls it.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::config::job::{Job, Loaded, load_dir};
use crate::config::{Defaults, PastorConfig, Paths};
use crate::connector::{Builtins, Catalog, ItemSource, RunInput};
use crate::daemon::Fleet;
use crate::machine::PastorEvent;
use crate::schedule::Schedule;
use crate::store::{JobState, Store};
use crate::task::DispatchSpec;
use crate::template;

/// A task queued longer than this gets one warning in the log: no machine has
/// had capacity for an hour is worth a human's eye.
pub const QUEUED_WARN_AFTER: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    Ran,
    DryRun,
    Failed,
    /// Due, but the previous run was still going.
    Skipped,
    NotDue,
    Disabled,
    Invalid,
    Unknown,
}

impl std::fmt::Display for RunOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = serde_json::to_value(self).expect("unit variant");
        f.write_str(s.as_str().unwrap_or("?"))
    }
}

/// What one `run_job` did, for the log and for `pastor tick`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRunReport {
    pub job: String,
    pub outcome: RunOutcome,
    /// Items the connector emitted, before any filtering.
    pub items: usize,
    /// Task ids created (`t-3`); on a dry run, the item keys that would have.
    pub created: Vec<String>,
    pub skipped_seen: usize,
    /// New items beyond `max_tasks_per_run`, left unseen for the next run.
    pub deferred: usize,
    pub error: Option<String>,
}

impl JobRunReport {
    pub fn new(job: &str, outcome: RunOutcome) -> JobRunReport {
        JobRunReport {
            job: job.into(),
            outcome,
            items: 0,
            created: Vec::new(),
            skipped_seen: 0,
            deferred: 0,
            error: None,
        }
    }
}

/// Connector failures: a minute, doubling, capped at an hour. `failures` is the
/// count including this one.
pub fn backoff_for(failures: u32) -> Duration {
    let steps = failures.saturating_sub(1).min(6);
    Duration::from_secs(60u64 << steps).min(Duration::from_secs(3600))
}

/// Why `value`, substituted from an item into `repo` or `branch`, is unsafe:
/// it would climb or cross directories, read as an option to git or ssh, or
/// carry control characters to the machine's shell.
fn path_value_problem(value: &str) -> Option<&'static str> {
    if value.contains('/') || value.contains('\\') {
        Some("contains a path separator")
    } else if value.contains("..") {
        Some("contains \"..\"")
    } else if value.starts_with('-') {
        Some("starts with '-'")
    } else if value.chars().any(char::is_control) {
        Some("contains a control character")
    } else {
        None
    }
}

/// Item fields are untrusted (a chat message, an issue title). Every
/// `{{ item.* }}` value that `repo` or `branch` would take is checked before
/// rendering; the job's own literal text around it, such as `~/work/`, is
/// the user's and is not.
pub fn check_item_paths(job: &Job, item: &Value) -> Result<(), String> {
    let ctx = serde_json::json!({ "item": item });
    for (field, text) in [
        ("repo", job.spec.repo.as_deref()),
        ("branch", job.spec.branch.as_deref()),
    ] {
        let Some(text) = text else { continue };
        for path in template::placeholders(text).map_err(|e| format!("{field}: {e}"))? {
            if !path.starts_with("item.") {
                continue;
            }
            let value = template::render(&format!("{{{{ {path} }}}}"), &ctx)
                .map_err(|e| format!("{field}: {e}"))?
                .text;
            if let Some(why) = path_value_problem(&value) {
                return Err(format!("{field}: {{{{ {path} }}}} = {value:?} {why}"));
            }
        }
    }
    Ok(())
}

/// Render prompt, repo and branch for one task. A placeholder with no value
/// renders empty and is logged: an item that failed to render would stay
/// unseen and fail again every run.
pub fn render_task(job: &Job, item: &Value, id: i64) -> Result<(String, DispatchSpec), String> {
    let ctx = serde_json::json!({
        "item": item,
        "job": {"name": job.name},
        "task": {"id": format!("t-{id}")},
    });
    let mut missing: Vec<String> = Vec::new();
    let mut render = |field: &str, text: &str| -> Result<String, String> {
        let r = template::render(text, &ctx).map_err(|e| format!("{field}: {e}"))?;
        missing.extend(r.missing.into_iter().map(|m| format!("{field}: {m}")));
        Ok(r.text)
    };
    check_item_paths(job, item)?;
    let prompt = render("prompt", &job.prompt)?;
    let repo = job
        .spec
        .repo
        .as_deref()
        .map(|r| render("repo", r))
        .transpose()?;
    let branch = job
        .spec
        .branch
        .as_deref()
        .map(|b| render("branch", b))
        .transpose()?;
    if !missing.is_empty() {
        tracing::warn!(job = %job.name, task = %format!("t-{id}"), ?missing, "placeholders with no value rendered empty");
    }
    let spec = DispatchSpec {
        repo,
        branch,
        ..job.spec.clone()
    };
    Ok((prompt, spec))
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(120).collect()
}

/// One pass of one job. Never panics and never returns early without a
/// report; the report is what the log and `pastor tick` show. With `dry_run`
/// nothing is written: no tasks, no seen keys, no job state, no event.
pub async fn run_job(
    store: &Store,
    job: &Job,
    source: &dyn ItemSource,
    events: &broadcast::Sender<PastorEvent>,
    now: DateTime<Utc>,
    dry_run: bool,
) -> JobRunReport {
    let mut report = JobRunReport::new(
        &job.name,
        if dry_run {
            RunOutcome::DryRun
        } else {
            RunOutcome::Ran
        },
    );
    let mut state = match store.job_state(&job.name) {
        Ok(s) => s.unwrap_or_else(|| JobState {
            name: job.name.clone(),
            ..Default::default()
        }),
        Err(e) => {
            report.outcome = RunOutcome::Failed;
            report.error = Some(format!("job state: {e:#}"));
            return report;
        }
    };
    let backfill =
        chrono::Duration::from_std(job.backfill).unwrap_or_else(|_| chrono::Duration::zero());
    let input = RunInput {
        config: job.connector_config.clone(),
        cursor: state.cursor.clone(),
        since: state.last_ok_at.unwrap_or(now - backfill),
        now,
    };
    let output = match source.run(input).await {
        Ok(o) => o,
        Err(err) => {
            tracing::warn!(job = %job.name, %err, "connector failed");
            report.outcome = RunOutcome::Failed;
            report.error = Some(err.clone());
            if !dry_run {
                state.failures += 1;
                state.last_run_at = Some(now);
                let wait = chrono::Duration::from_std(backoff_for(state.failures))
                    .unwrap_or_else(|_| chrono::Duration::zero());
                state.backoff_until = Some(now + wait);
                state.last_result = Some(format!(
                    "failed ({}x): {}",
                    state.failures,
                    first_line(&err)
                ));
                state.last_error = Some(err);
                if let Err(e) = store.save_job_state(&state) {
                    tracing::error!(job = %job.name, %e, "save job state");
                }
                let _ = events.send(PastorEvent {
                    kind: "job.failed".into(),
                    task_id: None,
                    machine: None,
                    job: Some(job.name.clone()),
                });
            }
            return report;
        }
    };
    for line in &output.logs {
        tracing::info!(job = %job.name, "{line}");
    }
    report.items = output.items.len();
    let mut in_run: HashSet<&str> = HashSet::new();
    let mut insert_failed = false;
    // Rejected items and failed inserts, for `report.error` and `job list`.
    let mut problems: Vec<String> = Vec::new();
    for item in &output.items {
        if item.key.is_empty() {
            tracing::warn!(job = %job.name, "item without a key skipped");
            continue;
        }
        if !in_run.insert(item.key.as_str()) {
            continue; // duplicate key in one run: first wins
        }
        match store.is_seen(&job.name, &item.key) {
            Ok(true) => {
                report.skipped_seen += 1;
                continue;
            }
            Ok(false) => {}
            Err(e) => {
                report.outcome = RunOutcome::Failed;
                report.error = Some(format!("seen-store: {e:#}"));
                return report;
            }
        }
        let value = item.as_value();
        // The item's own fault, and it will not change on a retry: report it
        // and move on, so one bad message cannot hold the job's cursor.
        if let Err(why) = check_item_paths(job, &value) {
            tracing::warn!(job = %job.name, key = %item.key, %why, "item rejected");
            problems.push(format!("{}: rejected: {why}", item.key));
            continue;
        }
        if report.created.len() as u32 >= job.max_tasks_per_run {
            report.deferred += 1;
            continue;
        }
        if dry_run {
            report.created.push(item.key.clone());
            continue;
        }
        match store.insert_job_task(&job.name, &value, |id| render_task(job, &value, id)) {
            Ok(t) => {
                tracing::info!(job = %job.name, task = %t.display_id(), key = %item.key, "task queued");
                let _ = events.send(PastorEvent {
                    kind: "task.queued".into(),
                    task_id: Some(t.id),
                    machine: None,
                    job: Some(t.job.clone()),
                });
                report.created.push(t.display_id());
            }
            Err(e) => {
                tracing::error!(job = %job.name, key = %item.key, %e, "create task");
                problems.push(format!("{}: {e:#}", item.key));
                insert_failed = true;
            }
        }
    }
    if report.deferred > 0 {
        tracing::info!(
            job = %job.name,
            deferred = report.deferred,
            max = job.max_tasks_per_run,
            "max_tasks_per_run reached; the rest stay unseen for the next run"
        );
    }
    if !problems.is_empty() {
        report.error = Some(problems.join("; "));
    }
    if insert_failed {
        report.outcome = RunOutcome::Failed;
    }
    if !dry_run {
        // The connector answered, so its failure count and backoff reset even
        // when an insert failed: backoff is for a connector that errors, and a
        // local insert failure is retried on the next scheduled run instead.
        state.failures = 0;
        state.backoff_until = None;
        state.last_run_at = Some(now);
        // The cursor and `since` move past every item the connector returned,
        // so they only advance when every new item became a task. A deferred
        // or failed item must be asked for again; the seen-store drops the
        // ones that did land. A rejected item is the item's own fault and a
        // retry cannot fix it, so it does not hold them. The same rule acks a
        // stream's batch: until then it hands the items out again.
        if report.deferred == 0 && !insert_failed {
            source.ack();
            state.last_ok_at = Some(now);
            if output.cursor.is_some() {
                state.cursor = output.cursor;
            }
        }
        if insert_failed {
            let err = report.error.clone().unwrap_or_default();
            state.last_result = Some(format!(
                "failed: {} items, {} tasks: {}",
                report.items,
                report.created.len(),
                first_line(&err)
            ));
        } else {
            state.last_result = Some(format!(
                "ok: {} items, {} tasks",
                report.items,
                report.created.len()
            ));
        }
        // Rejected or uninserted items, if any; `job list` shows it.
        state.last_error = report.error.clone();
        if let Err(e) = store.save_job_state(&state) {
            tracing::error!(job = %job.name, %e, "save job state");
        }
    }
    report
}

/// What `pastor job list` shows for one job file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobStatus {
    pub name: String,
    /// `None` when the file never parsed (nothing to describe).
    pub schedule: Option<String>,
    pub enabled: bool,
    pub connector: Option<String>,
    /// The current file's problem. With `schedule` also set, the previous
    /// good version is what runs.
    pub error: Option<String>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_result: Option<String>,
    pub next_due: Option<DateTime<Utc>>,
    pub running: bool,
}

/// A job as the scheduler holds it: the last good parse, plus the current
/// file's error if it stopped parsing. A file that never parsed has no `job`.
#[derive(Debug, Clone)]
struct Entry {
    job: Option<Job>,
    error: Option<String>,
}

/// A run's place in its job's line, handed out by the scheduler actor in
/// command order. `wait` returns once the previous run of the job has ended;
/// dropping the turn (the run finished, or panicked) lets the next one go.
struct Turn {
    prev: Option<oneshot::Receiver<()>>,
    _done: oneshot::Sender<()>,
}

impl Turn {
    /// Queue a run of `name` behind the one queued before it.
    fn behind(last_turn: &mut HashMap<String, oneshot::Receiver<()>>, name: &str) -> Turn {
        let (done, next) = oneshot::channel();
        Turn {
            prev: last_turn.insert(name.to_string(), next),
            _done: done,
        }
    }

    async fn wait(&mut self) {
        if let Some(prev) = self.prev.take() {
            // Err means the sender was dropped, which is exactly the signal.
            let _ = prev.await;
        }
    }
}

enum Due {
    Now,
    At(DateTime<Utc>),
    Never,
}

pub enum SchedulerCommand {
    Tick {
        job: Option<String>,
        dry_run: bool,
        reply: oneshot::Sender<Vec<JobRunReport>>,
    },
    Fire {
        name: String,
        reply: oneshot::Sender<Result<String, String>>,
    },
    Reload {
        reply: oneshot::Sender<Vec<JobStatus>>,
    },
    JobList {
        reply: oneshot::Sender<Vec<JobStatus>>,
    },
}

#[derive(Clone)]
pub struct SchedulerHandle {
    tx: mpsc::Sender<SchedulerCommand>,
}

impl SchedulerHandle {
    async fn send<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<T>) -> SchedulerCommand,
    ) -> anyhow::Result<T> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(make(reply))
            .await
            .map_err(|_| anyhow::anyhow!("scheduler is gone"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("scheduler dropped the request"))
    }
    pub async fn tick(
        &self,
        job: Option<String>,
        dry_run: bool,
    ) -> anyhow::Result<Vec<JobRunReport>> {
        self.send(|reply| SchedulerCommand::Tick {
            job,
            dry_run,
            reply,
        })
        .await
    }
    pub async fn fire(&self, name: &str) -> anyhow::Result<Result<String, String>> {
        let name = name.to_string();
        self.send(|reply| SchedulerCommand::Fire { name, reply })
            .await
    }
    pub async fn reload(&self) -> anyhow::Result<Vec<JobStatus>> {
        self.send(|reply| SchedulerCommand::Reload { reply }).await
    }
    pub async fn job_list(&self) -> anyhow::Result<Vec<JobStatus>> {
        self.send(|reply| SchedulerCommand::JobList { reply }).await
    }
}

/// Connector id -> source, overriding the catalog's lookup. Tests use it to
/// swap in scripted connectors with `Scheduler::with_resolver`.
pub type Resolver = Box<dyn Fn(&str) -> Option<Arc<dyn ItemSource>> + Send + Sync>;

pub struct Scheduler {
    paths: Paths,
    defaults: Defaults,
    tick: Duration,
    store: Arc<Store>,
    fleet: Arc<Fleet>,
    events: broadcast::Sender<PastorEvent>,
    /// Which connectors exist: job files validate against it and runs
    /// resolve through it. `Builtins` until `with_plugins`.
    catalog: Arc<dyn Catalog>,
    /// Re-read the plugins dir on a forced reload (`pastor job reload`, which
    /// `plugin install|link|uninstall|unlink` send).
    plugins: bool,
    /// Replaces the catalog's lookup when set.
    resolve: Option<Resolver>,
    /// The CLI's one-pass scheduler: it exits after the pass, so it cannot
    /// host a stream connector.
    standalone: bool,
    entries: HashMap<String, Entry>,
    /// (file name, mtime, size) of every job file at the last load; `None`
    /// until the first.
    fingerprint: Option<Vec<(PathBuf, Option<SystemTime>, u64)>>,
    /// Runs in progress: a job may appear more than once only through `fire`.
    in_flight: Vec<(String, JoinHandle<JobRunReport>)>,
    /// Per job, the signal that its most recently queued run has ended.
    /// `run_job` reads the job state and later replaces the row, so runs of
    /// one job go one at a time in the order they were asked for; see `Turn`.
    last_turn: HashMap<String, oneshot::Receiver<()>>,
    /// When each job was first loaded; a cron job that never ran is due at its
    /// first occurrence after this.
    first_seen: HashMap<String, DateTime<Utc>>,
    warned_queued: HashSet<i64>,
}

impl Scheduler {
    pub fn new(
        paths: Paths,
        config: &PastorConfig,
        store: Arc<Store>,
        fleet: Arc<Fleet>,
        events: broadcast::Sender<PastorEvent>,
    ) -> Scheduler {
        Scheduler {
            paths,
            defaults: config.defaults.clone(),
            tick: config.tick_duration(),
            store,
            fleet,
            events,
            catalog: Arc::new(Builtins),
            plugins: false,
            resolve: None,
            standalone: false,
            entries: HashMap::new(),
            fingerprint: None,
            in_flight: Vec::new(),
            last_turn: HashMap::new(),
            first_seen: HashMap::new(),
            warned_queued: HashSet::new(),
        }
    }

    /// Replace the connector lookup. Builder style so `Scheduler::new(..)
    /// .with_resolver(..)` reads as one construction.
    pub fn with_resolver(mut self, resolve: Resolver) -> Self {
        self.resolve = Some(resolve);
        self
    }

    /// Validate and resolve against the builtins plus the plugins installed
    /// under the data dir. An unreadable plugins dir is logged and leaves the
    /// builtins, so a bad plugin never keeps the daemon from starting.
    pub fn with_plugins(mut self) -> Self {
        self.plugins = true;
        self.load_plugins();
        self
    }

    fn load_plugins(&mut self) {
        match crate::plugin::PluginCatalog::load(&self.paths) {
            Ok(c) => self.catalog = Arc::new(c),
            Err(err) => {
                tracing::error!(%err, "read plugins; only built-in connectors are available");
                self.catalog = Arc::new(Builtins);
            }
        }
    }

    /// The source a run of `job` uses for `connector`.
    pub fn source_for(&self, connector: &str, job: &str) -> Option<Arc<dyn ItemSource>> {
        match &self.resolve {
            Some(r) => r(connector),
            None => self.catalog.source_for_job(connector, job),
        }
    }

    /// For the CLI when no daemon runs: no machines to dispatch to, nobody
    /// listening for events. Tasks it queues wait for the next `pastor serve`.
    pub fn standalone(paths: Paths, config: &PastorConfig, store: Arc<Store>) -> Scheduler {
        let fleet = Arc::new(Fleet::new(Vec::new(), store.clone()));
        let (events, _) = broadcast::channel(1);
        Scheduler {
            standalone: true,
            ..Scheduler::new(paths, config, store, fleet, events)
        }
    }

    pub fn spawn(self) -> SchedulerHandle {
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(self.run(rx));
        SchedulerHandle { tx }
    }

    async fn run(mut self, mut rx: mpsc::Receiver<SchedulerCommand>) {
        let mut tick = tokio::time::interval(self.tick);
        // A pass can wait on a whole dispatch round (connector runs are
        // spawned, but dispatch is awaited); missed ticks must not replay
        // back to back once the interval catches up.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = tick.tick() => self.pass(Utc::now()).await,
                cmd = rx.recv() => {
                    let Some(cmd) = cmd else { return };
                    match cmd {
                        SchedulerCommand::Tick { job, dry_run, reply } => {
                            // The runs are spawned; only collecting their
                            // reports waits, and that waits off this loop.
                            self.reap().await;
                            let reports = self.tick_start(job.as_deref(), dry_run, Utc::now());
                            tokio::spawn(async move {
                                let _ = reply.send(reports.await);
                            });
                        }
                        SchedulerCommand::Fire { name, reply } => {
                            self.reload();
                            self.reap().await;
                            let _ = reply.send(self.fire(&name, Utc::now()));
                        }
                        SchedulerCommand::Reload { reply } => {
                            // Force a re-read even if the fingerprint looks
                            // unchanged: this is the escape hatch when an edit
                            // lands within one mtime granule, or (before the
                            // fingerprint fix) behind a symlink.
                            self.force_reload();
                            self.reap().await;
                            let _ = reply.send(self.statuses(Utc::now()));
                        }
                        SchedulerCommand::JobList { reply } => {
                            self.reload();
                            self.reap().await;
                            let _ = reply.send(self.statuses(Utc::now()));
                        }
                    }
                }
            }
        }
    }

    /// Re-read the jobs directory if any file was added, removed or touched.
    /// A file that stopped parsing keeps its last good version and carries the
    /// error; a removed file removes the job. Returns whether anything was
    /// reloaded.
    pub fn reload(&mut self) -> bool {
        let dir = self.paths.jobs_dir();
        let fp = fingerprint(&dir);
        if self.fingerprint.as_ref() == Some(&fp) {
            return false;
        }
        self.fingerprint = Some(fp);
        let loaded = match load_dir(&dir, &self.defaults, self.catalog.as_ref()) {
            Ok(l) => l,
            Err(err) => {
                tracing::error!(%err, "read jobs directory");
                return true;
            }
        };
        let now = Utc::now();
        let mut next: HashMap<String, Entry> = HashMap::new();
        for l in loaded {
            let name = l.name().to_string();
            self.first_seen.entry(name.clone()).or_insert(now);
            let entry = match l {
                Loaded::Valid(job) => Entry {
                    job: Some(*job),
                    error: None,
                },
                Loaded::Invalid { error, .. } => {
                    let previous = self.entries.get(&name).and_then(|e| e.job.clone());
                    match &previous {
                        Some(_) => {
                            tracing::warn!(job = %name, %error, "job file invalid; previous version kept")
                        }
                        None => tracing::warn!(job = %name, %error, "job file invalid"),
                    }
                    Entry {
                        job: previous,
                        error: Some(error),
                    }
                }
            };
            next.insert(name, entry);
        }
        for gone in self.entries.keys().filter(|k| !next.contains_key(*k)) {
            tracing::info!(job = %gone, "job file removed");
        }
        // A removed or disabled job, or one moved to another connector, no
        // longer needs its source; a stream's would otherwise keep running
        // and buffering until the next forced reload.
        let keep: HashSet<(String, String)> = next
            .values()
            .filter_map(|e| e.job.as_ref())
            .filter(|j| j.enabled)
            .map(|j| (j.connector.clone(), j.name.clone()))
            .collect();
        self.catalog.retain_jobs(&keep);
        self.entries = next;
        true
    }

    /// Like `reload`, but always re-reads the jobs directory even when the
    /// fingerprint looks unchanged. What `pastor job reload` calls: the fingerprint
    /// is a cheap heuristic (mtime and size), not proof nothing changed.
    pub fn force_reload(&mut self) -> bool {
        // Plugins change only by command, and those end in this reload. A new
        // catalog also drops the old one's sources, which stops its streams.
        if self.plugins {
            self.load_plugins();
        }
        self.fingerprint = None;
        self.reload()
    }

    fn states(&self) -> HashMap<String, JobState> {
        match self.store.job_states() {
            Ok(v) => v.into_iter().map(|s| (s.name.clone(), s)).collect(),
            Err(err) => {
                tracing::error!(%err, "read job states");
                HashMap::new()
            }
        }
    }

    /// Queue a run of `name` behind the one queued before it. Called in the
    /// actor, before any spawn, so the order is the order of commands, not
    /// whatever order tokio happens to poll the spawned runs in.
    fn take_turn(&mut self, name: &str) -> Turn {
        Turn::behind(&mut self.last_turn, name)
    }

    fn is_running(&self, name: &str) -> bool {
        self.in_flight.iter().any(|(n, _)| n == name)
    }

    fn due_of(&self, job: &Job, state: Option<&JobState>, now: DateTime<Utc>) -> Due {
        if !job.enabled {
            return Due::Never;
        }
        // A backoff is a retry time, not only a gate: the failed attempt set
        // `last_run_at`, so falling through to the schedule would wait a whole
        // interval after the failure. A success clears it.
        if let Some(until) = state.and_then(|s| s.backoff_until) {
            return if now < until {
                Due::At(until)
            } else {
                Due::Now
            };
        }
        let last = state.and_then(|s| s.last_run_at);
        let next = match (&job.schedule, last) {
            // A new interval job runs at once (backfill says how far back it looks).
            (Schedule::Every(_), None) => return Due::Now,
            // A new cron job waits for its first occurrence; nothing was missed.
            (Schedule::Cron(_), None) => {
                let from = self.first_seen.get(&job.name).copied().unwrap_or(now);
                job.schedule.next_after(from)
            }
            // Overdue (daemon was down, or the tick is late) is simply due: it
            // runs once and the next occurrence is computed from now.
            (_, Some(last)) => job.schedule.next_after(last),
        };
        match next {
            Some(t) if t <= now => Due::Now,
            Some(t) => Due::At(t),
            None => Due::Never,
        }
    }

    pub fn statuses(&self, now: DateTime<Utc>) -> Vec<JobStatus> {
        let states = self.states();
        let mut out: Vec<JobStatus> = self
            .entries
            .iter()
            .map(|(name, e)| {
                let state = states.get(name);
                let next_due = e
                    .job
                    .as_ref()
                    .and_then(|j| match self.due_of(j, state, now) {
                        Due::Now => Some(now),
                        Due::At(t) => Some(t),
                        Due::Never => None,
                    });
                JobStatus {
                    name: name.clone(),
                    schedule: e.job.as_ref().map(|j| j.schedule.describe()),
                    enabled: e.job.as_ref().is_some_and(|j| j.enabled),
                    connector: e.job.as_ref().map(|j| j.connector.clone()),
                    error: e.error.clone(),
                    last_run_at: state.and_then(|s| s.last_run_at),
                    last_result: state.and_then(|s| s.last_result.clone()),
                    next_due,
                    running: self.is_running(name),
                }
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// One tick: reload, reap finished runs, start due jobs, dispatch, warn.
    pub async fn pass(&mut self, now: DateTime<Utc>) {
        self.reload();
        self.reap().await;
        let states = self.states();
        let due: Vec<Job> = self
            .entries
            .values()
            .filter_map(|e| e.job.clone())
            .filter(|j| matches!(self.due_of(j, states.get(&j.name), now), Due::Now))
            .collect();
        for job in due {
            if self.is_running(&job.name) {
                tracing::warn!(job = %job.name, "due, but the previous run is still going; skipped");
                continue;
            }
            if let Err(reason) = self.start_run(job.clone(), now) {
                tracing::warn!(job = %job.name, %reason, "run not started");
            }
        }
        self.fleet.dispatch_queued().await;
        self.warn_long_queued(now);
    }

    /// `pastor job run`: now, regardless of schedule, overlap and `enabled`.
    /// A fire while a run is going queues behind it (see `Turn`), so the
    /// second run starts from the state the first one saved.
    pub fn fire(&mut self, name: &str, now: DateTime<Utc>) -> Result<String, String> {
        let job = self
            .entries
            .get(name)
            .and_then(|e| e.job.clone())
            .ok_or_else(|| format!("no job named {name:?} (or its file has never parsed)"))?;
        if self.is_running(name) {
            tracing::info!(
                job = name,
                "fired while a previous run is still going; it waits for that one"
            );
        }
        self.start_run(job, now)?;
        Ok(format!("started job {name}"))
    }

    fn start_run(&mut self, job: Job, now: DateTime<Utc>) -> Result<(), String> {
        let source = self
            .source_for(&job.connector, &job.name)
            .ok_or_else(|| format!("connector {:?} is not available", job.connector))?;
        self.spawn_run(job, source, now, false, true);
        Ok(())
    }

    /// Run `job` in its own task, tracked in `in_flight` for the overlap rule
    /// and `running`. The report also goes to the returned receiver, for a
    /// caller that wants it (`pastor tick`). With `dispatch`, what the run
    /// queued is placed at once instead of on the next tick.
    fn spawn_run(
        &mut self,
        job: Job,
        source: Arc<dyn ItemSource>,
        now: DateTime<Utc>,
        dry_run: bool,
        dispatch: bool,
    ) -> oneshot::Receiver<JobRunReport> {
        let store = self.store.clone();
        let fleet = self.fleet.clone();
        let events = self.events.clone();
        let name = job.name.clone();
        // A dry run writes nothing, so it need not wait for a fired run.
        let mut turn = (!dry_run).then(|| self.take_turn(&name));
        let (tx, rx) = oneshot::channel();
        let handle = tokio::spawn(async move {
            if let Some(t) = turn.as_mut() {
                t.wait().await;
            }
            let report = run_job(&store, &job, source.as_ref(), &events, now, dry_run).await;
            drop(turn); // the next run of this job may start
            if dispatch && !dry_run && !report.created.is_empty() {
                // Do not wait for the next tick to place what this run queued.
                fleet.dispatch_queued().await;
            }
            let _ = tx.send(report.clone());
            report
        });
        self.in_flight.push((name, handle));
        rx
    }

    /// Log the reports of runs that finished since the last pass.
    async fn reap(&mut self) {
        let mut still = Vec::new();
        for (name, handle) in self.in_flight.drain(..) {
            if !handle.is_finished() {
                still.push((name, handle));
                continue;
            }
            match handle.await {
                Ok(r) => tracing::info!(
                    job = %r.job, outcome = %r.outcome, items = r.items, created = r.created.len(),
                    seen = r.skipped_seen, deferred = r.deferred, error = ?r.error, "job run finished"
                ),
                Err(err) => tracing::error!(job = %name, %err, "job run panicked"),
            }
        }
        self.in_flight = still;
    }

    /// `pastor tick`, awaited in place: what the CLI's standalone scheduler
    /// and the tests use.
    pub async fn tick_now(
        &mut self,
        only: Option<&str>,
        dry_run: bool,
        now: DateTime<Utc>,
    ) -> Vec<JobRunReport> {
        self.reap().await;
        self.tick_start(only, dry_run, now).await
    }

    /// `pastor tick`: start due jobs (or the one named, forced) and return a
    /// future of their reports, in job order. Each run is spawned, so the
    /// scheduler is free again as soon as this returns; a process connector
    /// can take a minute, and the loop must keep ticking and answering
    /// meanwhile. The future dispatches what the runs queued once they are
    /// all done. Call `reap` first, as `tick_now` does, so a finished run
    /// does not count as still going.
    pub fn tick_start(
        &mut self,
        only: Option<&str>,
        dry_run: bool,
        now: DateTime<Utc>,
    ) -> impl std::future::Future<Output = Vec<JobRunReport>> + Send + 'static {
        enum Slot {
            Ready(JobRunReport),
            Running(String, oneshot::Receiver<JobRunReport>),
        }
        self.reload();
        let states = self.states();
        let mut names: Vec<String> = self.entries.keys().cloned().collect();
        names.sort();
        let mut reports: Vec<Slot> = Vec::new();
        for name in &names {
            let name = name.as_str();
            if only.is_some_and(|o| o != name) {
                continue;
            }
            let entry = &self.entries[name];
            let Some(job) = entry.job.clone() else {
                let mut r = JobRunReport::new(name, RunOutcome::Invalid);
                r.error = entry.error.clone();
                reports.push(Slot::Ready(r));
                continue;
            };
            let forced = only.is_some();
            if !forced {
                match self.due_of(&job, states.get(name), now) {
                    Due::Now => {}
                    Due::Never if !job.enabled => {
                        reports.push(Slot::Ready(JobRunReport::new(name, RunOutcome::Disabled)));
                        continue;
                    }
                    _ => {
                        reports.push(Slot::Ready(JobRunReport::new(name, RunOutcome::NotDue)));
                        continue;
                    }
                }
                if self.is_running(name) {
                    reports.push(Slot::Ready(JobRunReport::new(name, RunOutcome::Skipped)));
                    continue;
                }
            }
            let Some(source) = self.source_for(&job.connector, &job.name) else {
                let mut r = JobRunReport::new(name, RunOutcome::Invalid);
                r.error = Some(format!("connector {:?} is not available", job.connector));
                reports.push(Slot::Ready(r));
                continue;
            };
            if self.standalone && source.long_lived() {
                let mut r = JobRunReport::new(name, RunOutcome::Failed);
                r.error = Some(format!(
                    "connector {:?} is a stream; its jobs run only under pastor serve",
                    job.connector
                ));
                reports.push(Slot::Ready(r));
                continue;
            }
            // `spawn_run` takes this run's turn now, in command order, so a
            // tick queues behind a fired run of the same job.
            let rx = self.spawn_run(job, source, now, dry_run, false);
            reports.push(Slot::Running(name.to_string(), rx));
        }
        if let Some(o) = only
            && !names.iter().any(|n| n == o)
        {
            let mut r = JobRunReport::new(o, RunOutcome::Unknown);
            r.error = Some(format!("no job named {o:?}"));
            reports.push(Slot::Ready(r));
        }
        let fleet = self.fleet.clone();
        async move {
            let mut out = Vec::with_capacity(reports.len());
            for slot in reports {
                out.push(match slot {
                    Slot::Ready(r) => r,
                    Slot::Running(name, rx) => rx.await.unwrap_or_else(|_| {
                        let mut r = JobRunReport::new(&name, RunOutcome::Failed);
                        r.error = Some("the run panicked".into());
                        r
                    }),
                });
            }
            if !dry_run {
                fleet.dispatch_queued().await;
            }
            out
        }
    }

    fn warn_long_queued(&mut self, now: DateTime<Utc>) {
        let Ok(queued) = self.store.queued_tasks() else {
            return;
        };
        // Only tasks still queued stay in the set: one that was dispatched,
        // closed or pruned is never warned about again anyway. Built once
        // per pass so the retain below is linear, not a scan of `queued`
        // per warned id.
        let queued_ids: HashSet<i64> = queued.iter().map(|t| t.id).collect();
        self.warned_queued.retain(|id| queued_ids.contains(id));
        let limit = chrono::Duration::from_std(QUEUED_WARN_AFTER).expect("1h fits");
        for t in queued {
            if let Some(m) = t
                .spec
                .machine
                .as_deref()
                .filter(|m| self.fleet.get(m).is_none())
            {
                // No pass can place it until that machine is back in the
                // flock; waiting an hour to say so helps nobody.
                if self.warned_queued.insert(t.id) {
                    tracing::warn!(
                        task = %t.display_id(),
                        job = %t.job,
                        machine = m,
                        "queued for a machine that is not in the flock; it stays queued until the machine is added back"
                    );
                }
                continue;
            }
            if now - t.created_at >= limit && self.warned_queued.insert(t.id) {
                tracing::warn!(
                    task = %t.display_id(),
                    job = %t.job,
                    queued_for = %crate::cli::age(t.created_at),
                    "no machine has had capacity; still queued"
                );
            }
        }
    }

    #[cfg(test)]
    fn set_source_for_tests(&mut self, id: &str, source: Arc<dyn ItemSource>) {
        let id = id.to_string();
        let previous = self.resolve.take();
        let catalog = self.catalog.clone();
        self.resolve = Some(Box::new(move |name| {
            if name == id {
                Some(source.clone())
            } else {
                match &previous {
                    Some(p) => p(name),
                    None => catalog.source(name),
                }
            }
        }));
    }

    #[cfg(test)]
    fn set_jobs_for_tests(&mut self, jobs: Vec<Job>) {
        let now = Utc::now();
        self.entries = jobs
            .into_iter()
            .map(|j| {
                self.first_seen.entry(j.name.clone()).or_insert(now);
                (
                    j.name.clone(),
                    Entry {
                        job: Some(j),
                        error: None,
                    },
                )
            })
            .collect();
        // Pretend the directory was read, so `pass` does not overwrite these.
        self.fingerprint = Some(fingerprint(&self.paths.jobs_dir()));
    }
}

/// Cheap change detection for the jobs directory: names, mtimes and sizes.
fn fingerprint(dir: &std::path::Path) -> Vec<(PathBuf, Option<SystemTime>, u64)> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            // `std::fs::metadata` follows symlinks (unlike `DirEntry::metadata`,
            // which reads the link itself on Unix); a job file managed as a
            // symlink into a dotfiles repo must fingerprint the target, or an
            // edit to the target is never noticed.
            let md = std::fs::metadata(&path).ok();
            out.push((
                path,
                md.as_ref().and_then(|m| m.modified().ok()),
                md.map(|m| m.len()).unwrap_or(0),
            ));
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_resolver_replaces_the_connector_lookup() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let s = Scheduler::standalone(paths, &PastorConfig::default(), store).with_resolver(
            Box::new(|id| (id == "only-this").then(|| crate::connector::builtin("clock").unwrap())),
        );
        assert!(s.source_for("only-this", "j").is_some());
        assert!(
            s.source_for("clock", "j").is_none(),
            "the default lookup is gone"
        );
    }

    /// With plugins on, job files validate against the plugin catalog: a job
    /// on an installed plugin is valid and resolves to that plugin, scoped to
    /// the job; one missing a required key is invalid with the manifest's
    /// reason. A plugin linked later is seen after a forced reload.
    #[test]
    fn with_plugins_validates_and_resolves_against_the_catalog() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        let jobs = paths.jobs_dir();
        std::fs::create_dir_all(&jobs).unwrap();
        let job = |conn: &str| {
            format!("every = \"1h\"\n[connector]\n{conn}\n[dispatch]\nprompt = \"p\"\n")
        };
        std::fs::write(
            jobs.join("ok.toml"),
            job("use = \"echo\"\nchannel = \"C1\""),
        )
        .unwrap();
        std::fs::write(jobs.join("bare.toml"), job("use = \"echo\"")).unwrap();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut s =
            Scheduler::standalone(paths.clone(), &PastorConfig::default(), store).with_plugins();
        s.reload();
        let st = s.statuses(Utc::now());
        assert!(
            st.iter().all(|j| j
                .error
                .as_deref()
                .is_some_and(|e| e.contains("not available"))),
            "{st:?}"
        );

        std::fs::create_dir_all(paths.plugins_dir()).unwrap();
        let fixture =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plugin/echo");
        std::os::unix::fs::symlink(fixture, paths.plugins_dir().join("echo")).unwrap();
        s.force_reload();
        let st = s.statuses(Utc::now());
        let bare = st.iter().find(|j| j.name == "bare").unwrap();
        assert!(
            bare.error
                .as_deref()
                .unwrap()
                .contains("requires connector.channel"),
            "{bare:?}"
        );
        let ok = st.iter().find(|j| j.name == "ok").unwrap();
        assert_eq!(ok.error, None);
        assert_eq!(s.source_for("echo", "ok").unwrap().id(), "echo");
        assert_eq!(s.source_for("clock", "ok").unwrap().id(), "clock");
    }
    use crate::connector::{Item, RunFuture, RunOutput};
    use crate::schedule::Schedule;
    use crate::store::TaskFilter;
    use crate::task::TaskState;
    use serde_json::{Map, Value, json};
    use std::sync::Mutex;

    /// A connector with a script: the items to emit, the cursor to return, or
    /// an error, plus a record of what it was asked.
    pub(super) struct Scripted {
        pub items: Mutex<Vec<Item>>,
        pub cursor: Mutex<Option<String>>,
        pub fail: Mutex<Option<String>>,
        pub inputs: Mutex<Vec<RunInput>>,
    }

    impl Scripted {
        pub fn with_keys(keys: &[&str]) -> Scripted {
            Scripted {
                items: Mutex::new(keys.iter().map(|k| item(k)).collect()),
                cursor: Mutex::new(None),
                fail: Mutex::new(None),
                inputs: Mutex::new(Vec::new()),
            }
        }
    }

    pub(super) fn item(key: &str) -> Item {
        let mut fields = Map::new();
        fields.insert("title".into(), Value::String(format!("title of {key}")));
        Item::new(key, fields)
    }

    impl ItemSource for Scripted {
        fn id(&self) -> &str {
            "scripted"
        }
        fn run<'a>(&'a self, input: RunInput) -> RunFuture<'a> {
            Box::pin(async move {
                self.inputs.lock().unwrap().push(input);
                if let Some(err) = self.fail.lock().unwrap().clone() {
                    return Err(err);
                }
                Ok(RunOutput {
                    items: self.items.lock().unwrap().clone(),
                    cursor: self.cursor.lock().unwrap().clone(),
                    logs: vec!["scripted ran".into()],
                })
            })
        }
    }

    pub(super) fn job(name: &str) -> Job {
        Job {
            name: name.into(),
            schedule: Schedule::Every(Duration::from_secs(60)),
            enabled: true,
            connector: "scripted".into(),
            connector_config: json!({"channel": "C1"}),
            prompt: "{{ job.name }}: {{ item.title }} ({{ task.id }})".into(),
            max_tasks_per_run: 5,
            backfill: Duration::from_secs(600),
            spec: DispatchSpec {
                agent: "claude".into(),
                agent_args: vec![],
                repo: Some("/srv/{{ job.name }}".into()),
                worktree: true,
                branch: Some("pastor/{{ item.key }}".into()),
                machine: None,
                tags: vec![],
                timeout_secs: 60,
            },
        }
    }

    fn events() -> (
        broadcast::Sender<PastorEvent>,
        broadcast::Receiver<PastorEvent>,
    ) {
        broadcast::channel(16)
    }

    #[tokio::test]
    async fn creates_one_task_per_new_item_with_rendered_templates() {
        let store = Store::open_in_memory().unwrap();
        let src = Scripted::with_keys(&["k1", "k2"]);
        *src.cursor.lock().unwrap() = Some("c1".into());
        let (tx, mut rx) = events();
        let now = Utc::now();
        let report = run_job(&store, &job("j"), &src, &tx, now, false).await;
        assert_eq!(report.outcome, RunOutcome::Ran);
        for id in [1, 2] {
            let ev = rx.try_recv().expect("task.queued emitted");
            assert_eq!(ev.kind, "task.queued");
            assert_eq!(ev.task_id, Some(id));
            assert_eq!(ev.job.as_deref(), Some("j"));
            assert!(ev.machine.is_none());
        }
        assert!(rx.try_recv().is_err(), "one event per created task");
        assert_eq!(report.items, 2);
        assert_eq!(report.created, vec!["t-1", "t-2"]);
        assert_eq!(report.skipped_seen, 0);
        assert_eq!(report.deferred, 0);
        assert!(report.error.is_none());

        let tasks = store.list_tasks(&TaskFilter::default()).unwrap();
        assert_eq!(tasks.len(), 2);
        let t1 = tasks.iter().find(|t| t.id == 1).unwrap();
        assert_eq!(t1.job, "j");
        assert_eq!(t1.state, TaskState::Queued);
        assert_eq!(t1.prompt, "j: title of k1 (t-1)");
        assert_eq!(t1.spec.repo.as_deref(), Some("/srv/j"));
        assert_eq!(t1.spec.branch.as_deref(), Some("pastor/k1"));
        assert!(t1.spec.worktree);
        assert_eq!(t1.item["key"], "k1");
        assert!(store.is_seen("j", "k1").unwrap() && store.is_seen("j", "k2").unwrap());

        let state = store.job_state("j").unwrap().unwrap();
        assert_eq!(state.cursor.as_deref(), Some("c1"));
        assert_eq!(state.failures, 0);
        assert_eq!(state.last_result.as_deref(), Some("ok: 2 items, 2 tasks"));
        assert!(state.last_run_at.is_some() && state.last_ok_at.is_some());

        // First run: since = now - backfill, cursor = null, config passed through.
        // Cloned out of the guard: `&...lock().unwrap()[0]` extends the guard
        // to the end of the function (temporary lifetime extension through
        // indexing), which would deadlock the second `run_job` call below
        // against `Scripted::run`'s own lock of the same mutex.
        let input = src.inputs.lock().unwrap()[0].clone();
        assert_eq!(input.since, now - chrono::Duration::seconds(600));
        assert!(input.cursor.is_none());
        assert_eq!(input.config["channel"], "C1");

        // Second run: since = last ok run, cursor = the persisted one.
        let later = now + chrono::Duration::seconds(60);
        run_job(&store, &job("j"), &src, &tx, later, false).await;
        let input = src.inputs.lock().unwrap()[1].clone();
        assert_eq!(input.since, state.last_ok_at.unwrap());
        assert_eq!(input.cursor.as_deref(), Some("c1"));
    }

    #[tokio::test]
    async fn seen_keys_and_in_run_duplicates_create_one_task() {
        let store = Store::open_in_memory().unwrap();
        let (tx, _rx) = events();
        let src = Scripted::with_keys(&["k1"]);
        run_job(&store, &job("j"), &src, &tx, Utc::now(), false).await;
        *src.items.lock().unwrap() = vec![item("k1"), item("k1"), item("k2"), item("k2")];
        let report = run_job(&store, &job("j"), &src, &tx, Utc::now(), false).await;
        assert_eq!(report.items, 4);
        assert_eq!(
            report.created,
            vec!["t-2"],
            "k2 once; k1 was seen, repeats collapse"
        );
        assert_eq!(report.skipped_seen, 1);
        assert_eq!(store.list_tasks(&TaskFilter::default()).unwrap().len(), 2);
        // Seen is per job: another job sees k1 as new.
        let report = run_job(
            &store,
            &job("other"),
            &Scripted::with_keys(&["k1"]),
            &tx,
            Utc::now(),
            false,
        )
        .await;
        assert_eq!(report.created.len(), 1);
    }

    #[tokio::test]
    async fn max_tasks_per_run_defers_the_rest_unseen() {
        let store = Store::open_in_memory().unwrap();
        let (tx, _rx) = events();
        let mut j = job("j");
        j.max_tasks_per_run = 2;
        let src = Scripted::with_keys(&["k1", "k2", "k3", "k4"]);
        let report = run_job(&store, &j, &src, &tx, Utc::now(), false).await;
        assert_eq!(report.created, vec!["t-1", "t-2"]);
        assert_eq!(report.deferred, 2);
        assert!(
            !store.is_seen("j", "k3").unwrap(),
            "deferred items stay unseen"
        );
        let report = run_job(&store, &j, &src, &tx, Utc::now(), false).await;
        assert_eq!(report.created, vec!["t-3", "t-4"]);
        assert_eq!(report.skipped_seen, 2);
        assert_eq!(report.deferred, 0);
    }

    #[tokio::test]
    async fn a_capped_run_keeps_the_old_cursor_and_since() {
        let store = Store::open_in_memory().unwrap();
        let (tx, _rx) = events();
        let t0 = Utc::now() - chrono::Duration::hours(1);
        store
            .save_job_state(&JobState {
                name: "j".into(),
                last_run_at: Some(t0),
                last_ok_at: Some(t0),
                cursor: Some("old".into()),
                ..Default::default()
            })
            .unwrap();
        let mut j = job("j");
        j.max_tasks_per_run = 2;
        let src = Scripted::with_keys(&["k1", "k2", "k3", "k4"]);
        *src.cursor.lock().unwrap() = Some("new".into());

        let t1 = Utc::now();
        let report = run_job(&store, &j, &src, &tx, t1, false).await;
        assert_eq!(report.deferred, 2);
        let s = store.job_state("j").unwrap().unwrap();
        assert_eq!(
            s.cursor.as_deref(),
            Some("old"),
            "a cursor past deferred items would lose them"
        );
        assert_eq!(s.last_ok_at, Some(t0), "since must still cover them");
        assert_eq!(s.last_run_at, Some(t1), "the schedule still advances");

        // The next run is asked from the old cursor and picks the rest up;
        // with nothing deferred, the new cursor is kept.
        let t2 = t1 + chrono::Duration::seconds(60);
        let report = run_job(&store, &j, &src, &tx, t2, false).await;
        let input = src.inputs.lock().unwrap()[1].clone();
        assert_eq!(input.cursor.as_deref(), Some("old"));
        assert_eq!(input.since, t0);
        assert_eq!(report.created, vec!["t-3", "t-4"]);
        assert_eq!(report.deferred, 0);
        let s = store.job_state("j").unwrap().unwrap();
        assert_eq!(s.cursor.as_deref(), Some("new"));
        assert_eq!(s.last_ok_at, Some(t2));
    }

    #[tokio::test]
    async fn a_failed_insert_keeps_the_old_cursor() {
        let store = Store::open_in_memory().unwrap();
        let (tx, _rx) = events();
        let mut j = job("j");
        j.prompt = "{{ unclosed".into();
        let src = Scripted::with_keys(&["k1"]);
        *src.cursor.lock().unwrap() = Some("new".into());
        let t1 = Utc::now();
        let report = run_job(&store, &j, &src, &tx, t1, false).await;
        assert!(report.error.is_some(), "{report:?}");
        let s = store.job_state("j").unwrap().unwrap();
        assert!(s.cursor.is_none(), "{s:?}");
        assert!(s.last_ok_at.is_none());
        assert_eq!(s.last_run_at, Some(t1));
    }

    #[tokio::test]
    async fn a_failed_insert_fails_the_run_without_backing_off() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (tx, mut rx) = events();
        let mut j = job("j");
        j.prompt = "{{ unclosed".into();
        let src = Scripted::with_keys(&["k1"]);
        let t1 = Utc::now();
        let report = run_job(&store, &j, &src, &tx, t1, false).await;
        assert_eq!(report.outcome, RunOutcome::Failed, "{report:?}");
        let s = store.job_state("j").unwrap().unwrap();
        assert!(s.last_error.as_deref().unwrap().contains("k1"), "{s:?}");
        assert!(
            s.last_result.as_deref().unwrap().starts_with("failed"),
            "{s:?}"
        );
        assert_eq!(
            s.failures, 0,
            "an insert failure is not a connector failure"
        );
        assert!(s.backoff_until.is_none());
        assert!(
            rx.try_recv().is_err(),
            "job.failed is for connector failures"
        );

        let (sched, _tmp) = scheduler_with(&store);
        assert!(
            matches!(
                sched.due_of(&j, Some(&s), t1 + chrono::Duration::seconds(30)),
                Due::At(u) if u == t1 + chrono::Duration::seconds(60)
            ),
            "the next run is due on schedule"
        );
    }

    #[tokio::test]
    async fn missing_item_field_renders_empty_and_warns() {
        let store = Store::open_in_memory().unwrap();
        let (tx, _rx) = events();
        let mut j = job("j");
        j.prompt = "[{{ item.title }}] {{ item.author }}!".into();
        let src = Scripted::with_keys(&["k1"]);
        let report = run_job(&store, &j, &src, &tx, Utc::now(), false).await;
        assert_eq!(report.created, vec!["t-1"]);
        let t = store.get_task(1).unwrap().unwrap();
        assert_eq!(t.prompt, "[title of k1] !");
        assert!(store.is_seen("j", "k1").unwrap());
    }

    #[tokio::test]
    async fn a_failing_connector_backs_off_keeps_cursor_and_emits_job_failed() {
        let store = Store::open_in_memory().unwrap();
        let (tx, mut rx) = events();
        let src = Scripted::with_keys(&["k1"]);
        *src.cursor.lock().unwrap() = Some("c1".into());
        let t0 = Utc::now();
        run_job(&store, &job("j"), &src, &tx, t0, false).await;
        assert_eq!(rx.try_recv().unwrap().kind, "task.queued");

        *src.fail.lock().unwrap() = Some("boom: 503 from upstream".into());
        let t1 = t0 + chrono::Duration::seconds(60);
        let report = run_job(&store, &job("j"), &src, &tx, t1, false).await;
        assert_eq!(report.outcome, RunOutcome::Failed);
        assert!(report.error.as_deref().unwrap().contains("boom"));
        let s = store.job_state("j").unwrap().unwrap();
        assert_eq!(s.failures, 1);
        assert_eq!(s.backoff_until, Some(t1 + chrono::Duration::seconds(60)));
        assert_eq!(s.cursor.as_deref(), Some("c1"), "cursor kept on failure");
        assert_eq!(s.last_ok_at, Some(t0), "since stays at the last success");
        assert_eq!(s.last_run_at, Some(t1));
        assert!(s.last_error.as_deref().unwrap().contains("boom"));
        assert!(s.last_result.as_deref().unwrap().starts_with("failed"));
        let ev = rx.try_recv().expect("job.failed emitted");
        assert_eq!(ev.kind, "job.failed");
        assert_eq!(ev.job.as_deref(), Some("j"));
        assert!(ev.machine.is_none() && ev.task_id.is_none());

        let t2 = t1 + chrono::Duration::seconds(120);
        run_job(&store, &job("j"), &src, &tx, t2, false).await;
        let s = store.job_state("j").unwrap().unwrap();
        assert_eq!(s.failures, 2);
        assert_eq!(s.backoff_until, Some(t2 + chrono::Duration::seconds(120)));

        *src.fail.lock().unwrap() = None;
        run_job(
            &store,
            &job("j"),
            &src,
            &tx,
            t2 + chrono::Duration::seconds(300),
            false,
        )
        .await;
        let s = store.job_state("j").unwrap().unwrap();
        assert_eq!(s.failures, 0);
        assert!(s.backoff_until.is_none() && s.last_error.is_none());
    }

    /// An item that could not be inserted is still unseen; if the cursor
    /// moved past it anyway, a connector that resumes from the cursor would
    /// never emit it again. The cursor and `since` hold until a run inserts
    /// everything it meant to.
    #[tokio::test]
    async fn a_failed_insert_keeps_the_cursor_and_since() {
        let store = Store::open_in_memory().unwrap();
        let (tx, _rx) = events();
        let t0 = Utc::now() - chrono::Duration::hours(1);
        store
            .save_job_state(&JobState {
                name: "j".into(),
                cursor: Some("c-old".into()),
                last_ok_at: Some(t0),
                ..Default::default()
            })
            .unwrap();
        let src = Scripted::with_keys(&["k1"]);
        *src.cursor.lock().unwrap() = Some("c-new".into());
        let mut j = job("j");
        // Valid when checked at load, but this Job is built by hand: rendering
        // fails inside the store's insert transaction.
        j.prompt = "{{ item.title".into();
        let now = Utc::now();
        let report = run_job(&store, &j, &src, &tx, now, false).await;
        assert!(report.created.is_empty());
        assert!(
            report.error.as_deref().unwrap().contains("k1"),
            "{report:?}"
        );
        let st = store.job_state("j").unwrap().unwrap();
        assert_eq!(st.cursor.as_deref(), Some("c-old"), "cursor held");
        assert_eq!(st.last_ok_at, Some(t0), "since held");
        assert_eq!(st.last_run_at, Some(now));
        assert!(
            st.last_result.as_deref().unwrap().starts_with("failed"),
            "{st:?}"
        );
        assert!(!store.is_seen("j", "k1").unwrap());

        // The next run inserts it; now the cursor moves.
        let report = run_job(&store, &job("j"), &src, &tx, now, false).await;
        assert_eq!(report.created.len(), 1);
        let st = store.job_state("j").unwrap().unwrap();
        assert_eq!(st.cursor.as_deref(), Some("c-new"));
        assert_eq!(st.last_ok_at, Some(now));
    }

    /// Item fields come from outside (a Slack message, an issue title). Put
    /// into `repo` or `branch` they must not climb directories, pose as an
    /// option, or smuggle control characters to the machine.
    #[test]
    fn render_task_rejects_unsafe_item_values_in_repo_and_branch() {
        let mut j = job("j");
        j.spec.repo = Some("~/work/{{ item.repo }}".into());
        j.spec.branch = Some("pastor/{{ item.key }}".into());
        let ok = json!({"key": "1727000123.000200", "repo": "api_v2-x"});
        let (_, spec) = render_task(&j, &ok, 1).unwrap();
        assert_eq!(spec.repo.as_deref(), Some("~/work/api_v2-x"));
        assert_eq!(spec.branch.as_deref(), Some("pastor/1727000123.000200"));
        for (field, value) in [
            ("repo", "../../etc"),
            ("repo", "a/b"),
            ("repo", "a\\b"),
            ("repo", ".."),
            ("key", "-oProxyCommand=x"),
            ("key", "x\ny"),
            ("key", "bell\u{7}"),
        ] {
            let mut item = ok.clone();
            item[field] = json!(value);
            let err = render_task(&j, &item, 1).unwrap_err();
            let target = if field == "repo" { "repo" } else { "branch" };
            assert!(
                err.contains(target) && err.contains(&format!("item.{field}")),
                "{value:?}: {err}"
            );
        }
        // The prompt is free text: anything goes there.
        let mut item = ok.clone();
        item["title"] = json!("../../etc\n-rf");
        assert!(render_task(&job("j"), &item, 1).is_ok());
    }

    /// A rejected item creates no task and is reported, but it is the item's
    /// fault, not a transient failure: the run still counts and the cursor
    /// moves, so one bad message cannot stall the job forever.
    #[tokio::test]
    async fn an_unsafe_item_is_skipped_and_reported_without_holding_the_cursor() {
        let store = Store::open_in_memory().unwrap();
        let (tx, _rx) = events();
        let mut bad = item("../evil");
        bad.fields.insert("title".into(), json!("t"));
        let src = Scripted {
            items: Mutex::new(vec![bad, item("good")]),
            cursor: Mutex::new(Some("c-1".into())),
            fail: Mutex::new(None),
            inputs: Mutex::new(Vec::new()),
        };
        let report = run_job(&store, &job("j"), &src, &tx, Utc::now(), false).await;
        assert_eq!(report.created.len(), 1);
        let err = report.error.as_deref().unwrap();
        assert!(err.contains("../evil") && err.contains("rejected"), "{err}");
        let st = store.job_state("j").unwrap().unwrap();
        assert_eq!(st.cursor.as_deref(), Some("c-1"));
        assert_eq!(store.list_tasks(&TaskFilter::default()).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn dry_run_writes_nothing() {
        let store = Store::open_in_memory().unwrap();
        let (tx, mut rx) = events();
        let src = Scripted::with_keys(&["k1", "k2"]);
        let report = run_job(&store, &job("j"), &src, &tx, Utc::now(), true).await;
        assert_eq!(report.outcome, RunOutcome::DryRun);
        assert_eq!(
            report.created,
            vec!["k1", "k2"],
            "keys, since no task ids exist"
        );
        assert!(store.list_tasks(&TaskFilter::default()).unwrap().is_empty());
        assert!(!store.is_seen("j", "k1").unwrap());
        assert!(store.job_state("j").unwrap().is_none());
        // A failing dry run does not back the job off either.
        *src.fail.lock().unwrap() = Some("boom".into());
        let report = run_job(&store, &job("j"), &src, &tx, Utc::now(), true).await;
        assert_eq!(report.outcome, RunOutcome::Failed);
        assert!(store.job_state("j").unwrap().is_none());
        assert!(rx.try_recv().is_err(), "no job.failed on a dry run");
    }

    /// The fixture stream connector for job `j`: one item `start-1` and
    /// cursor `cur-1` shortly after its first run starts it.
    fn fixture_stream(tmp: &std::path::Path) -> Arc<dyn ItemSource> {
        let paths = Paths::new(tmp.join("c"), tmp.join("s")).with_data_dir(tmp.join("d"));
        std::fs::create_dir_all(paths.plugins_dir()).unwrap();
        std::os::unix::fs::symlink(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plugin/stream"),
            paths.plugins_dir().join("stream"),
        )
        .unwrap();
        let catalog = crate::plugin::PluginCatalog::load(&paths).unwrap();
        catalog.source_for_job("stream", "j").unwrap()
    }

    /// Run `j` until the stream's item shows up in a report.
    async fn run_until_items(
        store: &Store,
        j: &Job,
        src: &dyn ItemSource,
        tx: &broadcast::Sender<PastorEvent>,
        dry_run: bool,
    ) -> JobRunReport {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let r = run_job(store, j, src, tx, Utc::now(), dry_run).await;
            if r.items > 0 {
                return r;
            }
            assert!(std::time::Instant::now() < deadline, "no items: {r:?}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// A stream hands its items over once. A dry run must not use them up:
    /// the real run after it still gets them and persists their cursor.
    #[tokio::test]
    async fn a_dry_run_leaves_a_streams_items_for_the_real_run() {
        let tmp = tempfile::tempdir().unwrap();
        let src = fixture_stream(tmp.path());
        let store = Store::open_in_memory().unwrap();
        let (tx, _rx) = events();
        let r = run_until_items(&store, &job("j"), src.as_ref(), &tx, true).await;
        assert_eq!(r.created, vec!["start-1"]);
        let r = run_job(&store, &job("j"), src.as_ref(), &tx, Utc::now(), false).await;
        assert_eq!(r.created, vec!["t-1"], "{r:?}");
        let st = store.job_state("j").unwrap().unwrap();
        assert_eq!(st.cursor.as_deref(), Some("cur-1"));
        let r = run_job(&store, &job("j"), src.as_ref(), &tx, Utc::now(), false).await;
        assert_eq!(r.items, 0, "a persisted batch is not handed over again");
    }

    /// Same after a run whose insert failed: the job's cursor held, and the
    /// stream must hold the items that go with it.
    #[tokio::test]
    async fn a_failed_insert_leaves_a_streams_items_for_the_next_run() {
        let tmp = tempfile::tempdir().unwrap();
        let src = fixture_stream(tmp.path());
        let store = Store::open_in_memory().unwrap();
        let (tx, _rx) = events();
        let mut broken = job("j");
        broken.prompt = "{{ item.title".into();
        let r = run_until_items(&store, &broken, src.as_ref(), &tx, false).await;
        assert!(r.created.is_empty(), "{r:?}");
        assert!(store.job_state("j").unwrap().unwrap().cursor.is_none());
        let r = run_job(&store, &job("j"), src.as_ref(), &tx, Utc::now(), false).await;
        assert_eq!(r.created, vec!["t-1"], "{r:?}");
        let st = store.job_state("j").unwrap().unwrap();
        assert_eq!(st.cursor.as_deref(), Some("cur-1"));
    }

    /// A stream belongs to its job: when the job file goes, the next reload
    /// drops the job's source and its process stops, without waiting for a
    /// forced plugin reload or a restart.
    #[tokio::test]
    async fn a_removed_job_stops_its_stream() {
        let tmp = tempfile::tempdir().unwrap();
        fixture_stream(tmp.path());
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"))
            .with_data_dir(tmp.path().join("d"));
        std::fs::create_dir_all(paths.jobs_dir()).unwrap();
        let job_file = paths.jobs_dir().join("j.toml");
        std::fs::write(
            &job_file,
            "every = \"1m\"\n[connector]\nuse = \"stream\"\n[dispatch]\nprompt = \"p\"\n",
        )
        .unwrap();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut s =
            Scheduler::standalone(paths.clone(), &PastorConfig::default(), store).with_plugins();
        s.reload();
        let src = s.source_for("stream", "j").unwrap();
        let _ = src
            .run(RunInput {
                config: json!({}),
                cursor: None,
                since: Utc::now(),
                now: Utc::now(),
            })
            .await;
        drop(src);
        let pid_file = paths.plugin_state_dir("j").join("pid");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let pid = loop {
            if let Ok(p) = std::fs::read_to_string(&pid_file)
                && !p.trim().is_empty()
            {
                break p.trim().to_string();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the stream never started"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        // Alive, or a zombie not yet reaped: either way not running.
        let running = || {
            std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|st| {
                st.rsplit(')')
                    .next()
                    .is_some_and(|r| !r.trim_start().starts_with('Z'))
            })
        };
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(running(), "up while its job exists");

        std::fs::remove_file(&job_file).unwrap();
        assert!(s.reload());
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while running() {
            assert!(
                std::time::Instant::now() < deadline,
                "stream {pid} still running"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[test]
    fn backoff_doubles_from_a_minute_and_caps_at_an_hour() {
        assert_eq!(backoff_for(1), Duration::from_secs(60));
        assert_eq!(backoff_for(2), Duration::from_secs(120));
        assert_eq!(backoff_for(6), Duration::from_secs(1920));
        assert_eq!(backoff_for(7), Duration::from_secs(3600));
        assert_eq!(backoff_for(40), Duration::from_secs(3600));
        assert_eq!(
            backoff_for(0),
            Duration::from_secs(60),
            "defensive: never zero"
        );
    }

    use crate::daemon::Fleet;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Takes a while and counts its runs: for the overlap rule.
    struct Slow {
        runs: Arc<AtomicUsize>,
        hold: Duration,
    }
    impl ItemSource for Slow {
        fn id(&self) -> &str {
            "slow"
        }
        fn run<'a>(&'a self, _input: RunInput) -> RunFuture<'a> {
            Box::pin(async move {
                self.runs.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(self.hold).await;
                Ok(RunOutput::default())
            })
        }
    }

    fn scheduler_with(store: &Arc<Store>) -> (Scheduler, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        let config = PastorConfig {
            tick: "1s".into(),
            ..Default::default()
        };
        let fleet = Arc::new(Fleet::new(vec![], store.clone()));
        let (events, _) = broadcast::channel(16);
        (
            Scheduler::new(paths, &config, store.clone(), fleet, events),
            tmp,
        )
    }

    fn write_job(paths: &Paths, name: &str, text: &str) {
        std::fs::create_dir_all(paths.jobs_dir()).unwrap();
        std::fs::write(crate::config::job::job_path(&paths.jobs_dir(), name), text).unwrap();
    }

    const CLOCK_JOB: &str = "every = \"5m\"\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"tick {{ item.key }}\"\n";

    #[tokio::test]
    async fn overlapping_run_is_skipped() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (mut s, _tmp) = scheduler_with(&store);
        let runs = Arc::new(AtomicUsize::new(0));
        let slow: Arc<dyn ItemSource> = Arc::new(Slow {
            runs: runs.clone(),
            hold: Duration::from_millis(300),
        });
        s.set_source_for_tests("slow", slow);
        let mut j = job("j");
        j.connector = "slow".into();
        j.schedule = Schedule::Every(Duration::from_secs(1));
        s.set_jobs_for_tests(vec![j]);

        let t0 = Utc::now();
        s.pass(t0).await;
        s.pass(t0 + chrono::Duration::seconds(5)).await; // due again, but still running
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "the second pass must not start a second run"
        );
        assert!(s.statuses(t0)[0].running);

        tokio::time::sleep(Duration::from_millis(400)).await;
        s.pass(t0 + chrono::Duration::seconds(10)).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            runs.load(Ordering::SeqCst),
            2,
            "once finished, the next due pass runs it"
        );
    }

    /// `pastor tick` against a slow connector must not hold the scheduler:
    /// while the tick waits for its run, other requests are answered and the
    /// run shows as `running` (so a pass would not start it again).
    #[tokio::test]
    async fn a_tick_waiting_on_a_slow_connector_does_not_block_the_scheduler() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (mut s, _tmp) = scheduler_with(&store);
        let runs = Arc::new(AtomicUsize::new(0));
        s.set_source_for_tests(
            "slow",
            Arc::new(Slow {
                runs: runs.clone(),
                hold: Duration::from_millis(800),
            }),
        );
        let mut j = job("j");
        j.connector = "slow".into();
        s.set_jobs_for_tests(vec![j]);
        let handle = s.spawn();
        let h = handle.clone();
        let tick = tokio::spawn(async move { h.tick(Some("j".into()), false).await });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let started = std::time::Instant::now();
        let jobs = tokio::time::timeout(Duration::from_millis(400), handle.job_list())
            .await
            .expect("job list answered while the tick runs")
            .unwrap();
        assert!(started.elapsed() < Duration::from_millis(400));
        assert!(jobs[0].running, "the tick's run counts as in flight");
        let reports = tick.await.unwrap().unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].outcome, RunOutcome::Ran);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn fire_ignores_schedule_overlap_and_enabled() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (mut s, _tmp) = scheduler_with(&store);
        let runs = Arc::new(AtomicUsize::new(0));
        s.set_source_for_tests(
            "slow",
            Arc::new(Slow {
                runs: runs.clone(),
                hold: Duration::from_millis(200),
            }),
        );
        let mut j = job("j");
        j.connector = "slow".into();
        j.enabled = false;
        s.set_jobs_for_tests(vec![j]);
        s.pass(Utc::now()).await;
        assert_eq!(
            runs.load(Ordering::SeqCst),
            0,
            "disabled jobs never run on a pass"
        );
        assert!(s.fire("j", Utc::now()).is_ok());
        assert!(
            s.fire("j", Utc::now()).is_ok(),
            "fire twice: overlap rule does not apply"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "the second fire waits for the first on the job's run lock"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(runs.load(Ordering::SeqCst), 2, "then it runs");
        assert!(s.fire("nope", Utc::now()).unwrap_err().contains("no job"));
    }

    /// First call is slow and returns cursor `c1`; later calls are instant and
    /// return `c2`. Records the cursor each call was given.
    struct SlowThenFast {
        calls: AtomicUsize,
        given: Mutex<Vec<Option<String>>>,
    }
    impl ItemSource for SlowThenFast {
        fn id(&self) -> &str {
            "slow-then-fast"
        }
        fn run<'a>(&'a self, input: RunInput) -> RunFuture<'a> {
            Box::pin(async move {
                self.given.lock().unwrap().push(input.cursor.clone());
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                Ok(RunOutput {
                    cursor: Some(if n == 0 { "c1" } else { "c2" }.into()),
                    ..Default::default()
                })
            })
        }
    }

    #[tokio::test]
    async fn overlapping_fires_do_not_let_an_older_run_overwrite_state() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (mut s, _tmp) = scheduler_with(&store);
        let src = Arc::new(SlowThenFast {
            calls: AtomicUsize::new(0),
            given: Mutex::new(Vec::new()),
        });
        s.set_source_for_tests("stf", src.clone());
        let mut j = job("j");
        j.connector = "stf".into();
        s.set_jobs_for_tests(vec![j]);

        let t1 = Utc::now();
        let t2 = t1 + chrono::Duration::seconds(1);
        s.fire("j", t1).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        s.fire("j", t2).unwrap();
        tokio::time::sleep(Duration::from_millis(600)).await;

        assert_eq!(src.calls.load(Ordering::SeqCst), 2, "both fires ran");
        let st = store.job_state("j").unwrap().unwrap();
        assert_eq!(st.last_run_at, Some(t2), "the newer run's state stands");
        assert_eq!(st.cursor.as_deref(), Some("c2"));
        assert_eq!(
            src.given.lock().unwrap().clone(),
            vec![None, Some("c1".into())],
            "the second run started from the first run's saved state"
        );
    }

    #[tokio::test]
    async fn a_run_waits_its_turn_even_when_polled_first() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (mut s, _tmp) = scheduler_with(&store);
        let first = s.take_turn("j");
        let mut second = s.take_turn("j");
        let mut other = s.take_turn("other");
        // Poll the later turn before the earlier one has even started, as a
        // tokio scheduler is free to do with two spawned runs.
        let second_ran = Arc::new(AtomicUsize::new(0));
        let flag = second_ran.clone();
        let waiter = tokio::spawn(async move {
            second.wait().await;
            flag.store(1, Ordering::SeqCst);
        });
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            second_ran.load(Ordering::SeqCst),
            0,
            "the later run must wait for the earlier one"
        );
        other.wait().await; // another job's line is independent
        drop(first); // the earlier run ends (or panics: the drop is the same)
        waiter.await.unwrap();
        assert_eq!(second_ran.load(Ordering::SeqCst), 1);
    }

    /// Holds its first run until the test opens the gate; later runs pass.
    struct Gated {
        gate: tokio::sync::Notify,
        calls: AtomicUsize,
    }
    impl ItemSource for Gated {
        fn id(&self) -> &str {
            "gated"
        }
        fn run<'a>(&'a self, _input: RunInput) -> RunFuture<'a> {
            Box::pin(async move {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    self.gate.notified().await;
                }
                Ok(RunOutput {
                    cursor: Some(format!("c{}", n + 1)),
                    ..Default::default()
                })
            })
        }
    }

    #[tokio::test]
    async fn a_later_fire_runs_after_the_earlier_one_and_its_state_wins() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (mut s, _tmp) = scheduler_with(&store);
        let src = Arc::new(Gated {
            gate: tokio::sync::Notify::new(),
            calls: AtomicUsize::new(0),
        });
        s.set_source_for_tests("gated", src.clone());
        let mut j = job("j");
        j.connector = "gated".into();
        s.set_jobs_for_tests(vec![j]);
        let t1 = Utc::now();
        let t2 = t1 + chrono::Duration::seconds(1);
        s.fire("j", t1).unwrap();
        s.fire("j", t2).unwrap();
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            src.calls.load(Ordering::SeqCst),
            1,
            "the second fire has not started while the first is held"
        );
        src.gate.notify_one();
        for (_, h) in s.in_flight.drain(..) {
            h.await.unwrap();
        }
        let st = store.job_state("j").unwrap().unwrap();
        assert_eq!(st.last_run_at, Some(t2), "the newer run wrote last");
        assert_eq!(st.cursor.as_deref(), Some("c2"));
    }

    #[tokio::test]
    async fn invalid_edit_keeps_previous_job_and_reports_error() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (mut s, tmp) = scheduler_with(&store);
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        write_job(&paths, "a", CLOCK_JOB);
        assert!(s.reload(), "first load counts as a change");
        assert!(!s.reload(), "unchanged directory is not reloaded");
        let now = Utc::now();
        let st = &s.statuses(now)[0];
        assert_eq!(st.name, "a");
        assert!(st.error.is_none());
        assert_eq!(st.schedule.as_deref(), Some("every 5m"));

        // Break the file: the previous version keeps running, the error shows.
        std::thread::sleep(Duration::from_millis(20)); // mtime granularity
        write_job(&paths, "a", "every = \"5m\"\n[connector\n");
        assert!(s.reload());
        let st = &s.statuses(now)[0];
        assert!(st.error.is_some(), "{st:?}");
        assert_eq!(
            st.schedule.as_deref(),
            Some("every 5m"),
            "previous version kept"
        );
        let reports = s.tick_now(Some("a"), true, now).await;
        assert_eq!(
            reports[0].outcome,
            RunOutcome::DryRun,
            "the kept version still runs"
        );

        // A brand-new invalid file has nothing to keep.
        write_job(&paths, "b", "nonsense");
        assert!(s.reload());
        let sts = s.statuses(now);
        assert_eq!(sts.len(), 2);
        assert!(sts[1].schedule.is_none() && sts[1].error.is_some());
        let reports = s.tick_now(Some("b"), false, now).await;
        assert_eq!(reports[0].outcome, RunOutcome::Invalid);

        // Removing a file removes the job.
        std::fs::remove_file(crate::config::job::job_path(&paths.jobs_dir(), "a")).unwrap();
        assert!(s.reload());
        assert_eq!(s.statuses(now).len(), 1);
    }

    #[tokio::test]
    async fn overdue_on_start_runs_once_and_missed_runs_are_not_replayed() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (mut s, tmp) = scheduler_with(&store);
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        write_job(&paths, "a", CLOCK_JOB);
        let now = Utc::now();
        store
            .save_job_state(&JobState {
                name: "a".into(),
                last_run_at: Some(now - chrono::Duration::hours(3)),
                last_ok_at: Some(now - chrono::Duration::hours(3)),
                ..Default::default()
            })
            .unwrap();
        let reports = s.tick_now(None, false, now).await;
        assert_eq!(
            reports[0].outcome,
            RunOutcome::Ran,
            "three hours overdue: runs once"
        );
        assert_eq!(reports[0].created.len(), 1);
        let reports = s
            .tick_now(None, false, now + chrono::Duration::seconds(1))
            .await;
        assert_eq!(reports[0].outcome, RunOutcome::NotDue, "not 36 times");
        let st = &s.statuses(now)[0];
        assert_eq!(st.next_due, Some(now + chrono::Duration::minutes(5)));
    }

    #[tokio::test]
    async fn a_new_every_job_runs_now_but_a_new_cron_job_waits() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (mut s, tmp) = scheduler_with(&store);
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        write_job(&paths, "e", CLOCK_JOB);
        write_job(
            &paths,
            "c",
            &CLOCK_JOB.replace("every = \"5m\"", "cron = \"0 0 1 1 *\""),
        );
        let now = Utc::now();
        let reports = s.tick_now(None, false, now).await;
        let of = |name: &str| reports.iter().find(|r| r.job == name).unwrap();
        assert_eq!(of("e").outcome, RunOutcome::Ran);
        assert_eq!(of("c").outcome, RunOutcome::NotDue);
        let sts = s.statuses(now);
        let c = sts.iter().find(|j| j.name == "c").unwrap();
        assert!(c.next_due.unwrap() > now);
        assert_eq!(c.schedule.as_deref(), Some("cron 0 0 1 1 *"));
    }

    #[tokio::test]
    async fn backoff_gates_a_due_job() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (mut s, tmp) = scheduler_with(&store);
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        write_job(&paths, "a", CLOCK_JOB);
        let now = Utc::now();
        store
            .save_job_state(&JobState {
                name: "a".into(),
                failures: 1,
                backoff_until: Some(now + chrono::Duration::seconds(30)),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            s.tick_now(None, false, now).await[0].outcome,
            RunOutcome::NotDue
        );
        assert_eq!(
            s.tick_now(None, false, now + chrono::Duration::seconds(31))
                .await[0]
                .outcome,
            RunOutcome::Ran
        );
        assert_eq!(
            s.tick_now(Some("a"), false, now).await[0].outcome,
            RunOutcome::Ran,
            "--job forces it regardless"
        );
    }

    #[tokio::test]
    async fn an_elapsed_backoff_retries_before_the_next_interval() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (mut s, _tmp) = scheduler_with(&store);
        let mut j = job("j");
        j.schedule = Schedule::Every(Duration::from_secs(3600));
        s.set_jobs_for_tests(vec![j.clone()]);
        let t = Utc::now();
        let failed = JobState {
            name: "j".into(),
            last_run_at: Some(t),
            failures: 1,
            backoff_until: Some(t + chrono::Duration::minutes(1)),
            ..Default::default()
        };
        assert!(matches!(
            s.due_of(&j, Some(&failed), t + chrono::Duration::seconds(30)),
            Due::At(u) if u == t + chrono::Duration::minutes(1)
        ));
        assert!(
            matches!(
                s.due_of(&j, Some(&failed), t + chrono::Duration::minutes(2)),
                Due::Now
            ),
            "the retry is due when the backoff ends, not an hour after the failure"
        );
        // Once a run succeeds (backoff cleared), the interval rules again.
        let ok = JobState {
            name: "j".into(),
            last_run_at: Some(t),
            ..Default::default()
        };
        assert!(matches!(
            s.due_of(&j, Some(&ok), t + chrono::Duration::minutes(2)),
            Due::At(_)
        ));
    }

    #[tokio::test]
    async fn unknown_job_on_tick_is_reported() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (mut s, _tmp) = scheduler_with(&store);
        let reports = s.tick_now(Some("ghost"), false, Utc::now()).await;
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].outcome, RunOutcome::Unknown);
    }

    #[tokio::test]
    async fn reload_follows_a_symlinked_job_file() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (mut s, tmp) = scheduler_with(&store);
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        std::fs::create_dir_all(paths.jobs_dir()).unwrap();

        // The job file lives outside the jobs dir; the jobs dir only holds a
        // symlink to it, as a dotfiles-managed job would.
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let target = elsewhere.join("a.toml");
        std::fs::write(&target, CLOCK_JOB).unwrap();
        let link = crate::config::job::job_path(&paths.jobs_dir(), "a");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(s.reload(), "first load counts as a change");
        assert_eq!(
            s.statuses(Utc::now())[0].schedule.as_deref(),
            Some("every 5m")
        );

        // Edit the symlink's target, not the link. `DirEntry::metadata` (the
        // bug) would report the link's own mtime/size, unchanged, and this
        // edit would never be noticed.
        std::thread::sleep(Duration::from_millis(20)); // mtime granularity
        std::fs::write(
            &target,
            CLOCK_JOB.replace("every = \"5m\"", "every = \"10m\""),
        )
        .unwrap();
        assert!(
            s.reload(),
            "an edit through a symlinked job file must be picked up"
        );
        assert_eq!(
            s.statuses(Utc::now())[0].schedule.as_deref(),
            Some("every 10m")
        );
    }

    #[tokio::test]
    async fn force_reload_re_reads_even_when_the_fingerprint_is_unchanged() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (mut s, tmp) = scheduler_with(&store);
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        write_job(&paths, "a", CLOCK_JOB);

        assert!(s.reload(), "first load counts as a change");
        assert!(
            !s.reload(),
            "nothing changed on disk: an ordinary reload is a no-op"
        );
        // `SchedulerCommand::Reload` (pastor job reload) calls this instead of
        // `reload()`, precisely so it is not fooled by an unchanged
        // fingerprint (a symlinked target edited within one mtime granule,
        // for instance).
        assert!(
            s.force_reload(),
            "pastor job reload must force a re-read regardless of the fingerprint"
        );
    }

    #[tokio::test]
    async fn job_list_reaps_a_finished_run_before_the_next_tick() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        // A long tick: if the running flag ever clears, it is because
        // `JobList` reaped on demand, not because a tick happened to land.
        let config = PastorConfig {
            tick: "1h".into(),
            ..Default::default()
        };
        let fleet = Arc::new(Fleet::new(vec![], store.clone()));
        let (events, _) = broadcast::channel(16);
        write_job(&paths, "a", CLOCK_JOB);
        let scheduler = Scheduler::new(paths, &config, store.clone(), fleet, events);
        let handle = scheduler.spawn();

        let fired = handle.fire("a").await.unwrap();
        assert!(fired.is_ok(), "{fired:?}");
        // The clock connector's run is near-instant; give the spawned task a
        // moment to finish without racing it.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let statuses = handle.job_list().await.unwrap();
        let a = statuses.iter().find(|s| s.name == "a").unwrap();
        assert!(
            !a.running,
            "job list must reap a finished run itself, not wait for the next (1h) tick"
        );
    }

    #[tokio::test]
    async fn queued_over_an_hour_is_warned_once() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (mut s, _tmp) = scheduler_with(&store);
        let t = store
            .insert_task(crate::store::NewTask {
                job: "run".into(),
                item: Value::Null,
                prompt: "p".into(),
                spec: job("j").spec,
            })
            .unwrap();
        let now = Utc::now();
        s.warn_long_queued(now);
        assert!(
            s.warned_queued.is_empty(),
            "fresh tasks are not warned about"
        );
        let later = now + chrono::Duration::hours(2);
        s.warn_long_queued(later);
        s.warn_long_queued(later);
        assert_eq!(s.warned_queued.len(), 1);
        assert!(s.warned_queued.contains(&t.id));
    }

    #[tokio::test]
    async fn warned_queued_forgets_tasks_that_left_the_queue() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (mut s, _tmp) = scheduler_with(&store);
        let t = store
            .insert_task(crate::store::NewTask {
                job: "run".into(),
                item: Value::Null,
                prompt: "p".into(),
                spec: job("j").spec,
            })
            .unwrap();
        let later = Utc::now() + chrono::Duration::hours(2);
        s.warn_long_queued(later);
        assert!(s.warned_queued.contains(&t.id));
        // Dispatched: claimed off the queue.
        store.claim_task(t.id, "m").unwrap().unwrap();
        s.warn_long_queued(later);
        assert!(s.warned_queued.is_empty(), "{:?}", s.warned_queued);
    }

    /// Review Focus 5: no dispatch pass can place a task pinned to a machine
    /// the flock does not have, so it is warned about on the next pass, once.
    #[tokio::test]
    async fn a_task_pinned_to_a_missing_machine_is_warned_at_once() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (mut s, _tmp) = scheduler_with(&store);
        let t = store
            .insert_task(crate::store::NewTask {
                job: "run".into(),
                item: Value::Null,
                prompt: "p".into(),
                spec: DispatchSpec {
                    machine: Some("gone".into()),
                    ..job("j").spec
                },
            })
            .unwrap();
        let now = Utc::now();
        s.warn_long_queued(now);
        assert!(
            s.warned_queued.contains(&t.id),
            "no hour's wait for a machine that is not there"
        );
        s.warn_long_queued(now);
        assert_eq!(s.warned_queued.len(), 1);
    }
}
