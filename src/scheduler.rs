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
use crate::connector;
use crate::connector::{ItemSource, RunInput};
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
        if report.created.len() as u32 >= job.max_tasks_per_run {
            report.deferred += 1;
            continue;
        }
        if dry_run {
            report.created.push(item.key.clone());
            continue;
        }
        let value = item.as_value();
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
                report.error = Some(format!("{}: {e:#}", item.key));
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
    if !dry_run {
        state.failures = 0;
        state.backoff_until = None;
        state.last_run_at = Some(now);
        // The cursor and `since` move past every item the connector returned,
        // so they only advance when every new item became a task. A deferred
        // or failed item must be asked for again; the seen-store drops the
        // ones that did land.
        if report.deferred == 0 && !insert_failed {
            state.last_ok_at = Some(now);
            if output.cursor.is_some() {
                state.cursor = output.cursor;
            }
        }
        state.last_result = Some(format!(
            "ok: {} items, {} tasks",
            report.items,
            report.created.len()
        ));
        state.last_error = None;
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

/// Connector id -> source. The daemon uses `connector::builtin`; a plugin
/// catalog (plan 3) or a test swaps its own in with `Scheduler::with_resolver`.
pub type Resolver = Box<dyn Fn(&str) -> Option<Arc<dyn ItemSource>> + Send + Sync>;

pub struct Scheduler {
    paths: Paths,
    defaults: Defaults,
    tick: Duration,
    store: Arc<Store>,
    fleet: Arc<Fleet>,
    events: broadcast::Sender<PastorEvent>,
    /// Connector id -> source. `connector::builtin` outside tests.
    resolve: Resolver,
    entries: HashMap<String, Entry>,
    /// (file name, mtime, size) of every job file at the last load; `None`
    /// until the first.
    fingerprint: Option<Vec<(PathBuf, Option<SystemTime>, u64)>>,
    /// Runs in progress: a job may appear more than once only through `fire`.
    in_flight: Vec<(String, JoinHandle<JobRunReport>)>,
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
            resolve: Box::new(connector::builtin),
            entries: HashMap::new(),
            fingerprint: None,
            in_flight: Vec::new(),
            first_seen: HashMap::new(),
            warned_queued: HashSet::new(),
        }
    }

    /// Replace the connector lookup. Builder style so `Scheduler::new(..)
    /// .with_resolver(..)` reads as one construction.
    pub fn with_resolver(mut self, resolve: Resolver) -> Self {
        self.resolve = resolve;
        self
    }

    /// For the CLI when no daemon runs: no machines to dispatch to, nobody
    /// listening for events. Tasks it queues wait for the next `pastor serve`.
    pub fn standalone(paths: Paths, config: &PastorConfig, store: Arc<Store>) -> Scheduler {
        let fleet = Arc::new(Fleet::new(Vec::new(), store.clone()));
        let (events, _) = broadcast::channel(1);
        Scheduler::new(paths, config, store, fleet, events)
    }

    pub fn spawn(self) -> SchedulerHandle {
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(self.run(rx));
        SchedulerHandle { tx }
    }

    async fn run(mut self, mut rx: mpsc::Receiver<SchedulerCommand>) {
        let mut tick = tokio::time::interval(self.tick);
        // A pass can wait on a whole dispatch round, and `Tick`/`Fire` run
        // connectors inline; missed ticks must not replay back to back once
        // the interval catches up.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = tick.tick() => self.pass(Utc::now()).await,
                cmd = rx.recv() => {
                    let Some(cmd) = cmd else { return };
                    match cmd {
                        SchedulerCommand::Tick { job, dry_run, reply } => {
                            let reports = self.tick_now(job.as_deref(), dry_run, Utc::now()).await;
                            let _ = reply.send(reports);
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
        let loaded = match load_dir(&dir, &self.defaults) {
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
        self.entries = next;
        true
    }

    /// Like `reload`, but always re-reads the jobs directory even when the
    /// fingerprint looks unchanged. What `pastor reload` calls: the fingerprint
    /// is a cheap heuristic (mtime and size), not proof nothing changed.
    pub fn force_reload(&mut self) -> bool {
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

    fn is_running(&self, name: &str) -> bool {
        self.in_flight.iter().any(|(n, _)| n == name)
    }

    fn due_of(&self, job: &Job, state: Option<&JobState>, now: DateTime<Utc>) -> Due {
        if !job.enabled {
            return Due::Never;
        }
        if let Some(until) = state.and_then(|s| s.backoff_until)
            && now < until
        {
            return Due::At(until);
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
    pub fn fire(&mut self, name: &str, now: DateTime<Utc>) -> Result<String, String> {
        let job = self
            .entries
            .get(name)
            .and_then(|e| e.job.clone())
            .ok_or_else(|| format!("no job named {name:?} (or its file has never parsed)"))?;
        if self.is_running(name) {
            tracing::info!(job = name, "fired while a previous run is still going");
        }
        self.start_run(job, now)?;
        Ok(format!("started job {name}"))
    }

    fn start_run(&mut self, job: Job, now: DateTime<Utc>) -> Result<(), String> {
        let source = (self.resolve)(&job.connector)
            .ok_or_else(|| format!("connector {:?} is not available", job.connector))?;
        let store = self.store.clone();
        let fleet = self.fleet.clone();
        let events = self.events.clone();
        let name = job.name.clone();
        let handle = tokio::spawn(async move {
            let report = run_job(&store, &job, source.as_ref(), &events, now, false).await;
            if !report.created.is_empty() {
                // Do not wait for the next tick to place what this run queued.
                fleet.dispatch_queued().await;
            }
            report
        });
        self.in_flight.push((name, handle));
        Ok(())
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

    /// `pastor tick`: run due jobs (or the one named, forced) inline and report.
    /// Inline so the reports are complete when this returns; a long connector
    /// holds the scheduler for that long, which is acceptable for a debugging
    /// command.
    pub async fn tick_now(
        &mut self,
        only: Option<&str>,
        dry_run: bool,
        now: DateTime<Utc>,
    ) -> Vec<JobRunReport> {
        self.reload();
        self.reap().await;
        let states = self.states();
        let mut names: Vec<&String> = self.entries.keys().collect();
        names.sort();
        let mut reports = Vec::new();
        for name in names {
            if only.is_some_and(|o| o != name) {
                continue;
            }
            let entry = &self.entries[name];
            let Some(job) = entry.job.clone() else {
                let mut r = JobRunReport::new(name, RunOutcome::Invalid);
                r.error = entry.error.clone();
                reports.push(r);
                continue;
            };
            let forced = only.is_some();
            if !forced {
                match self.due_of(&job, states.get(name), now) {
                    Due::Now => {}
                    Due::Never if !job.enabled => {
                        reports.push(JobRunReport::new(name, RunOutcome::Disabled));
                        continue;
                    }
                    _ => {
                        reports.push(JobRunReport::new(name, RunOutcome::NotDue));
                        continue;
                    }
                }
                if self.is_running(name) {
                    reports.push(JobRunReport::new(name, RunOutcome::Skipped));
                    continue;
                }
            }
            let Some(source) = (self.resolve)(&job.connector) else {
                let mut r = JobRunReport::new(name, RunOutcome::Invalid);
                r.error = Some(format!("connector {:?} is not available", job.connector));
                reports.push(r);
                continue;
            };
            reports.push(
                run_job(
                    &self.store,
                    &job,
                    source.as_ref(),
                    &self.events,
                    now,
                    dry_run,
                )
                .await,
            );
        }
        if let Some(o) = only
            && !reports.iter().any(|r| r.job == o)
        {
            let mut r = JobRunReport::new(o, RunOutcome::Unknown);
            r.error = Some(format!("no job named {o:?}"));
            reports.push(r);
        }
        if !dry_run {
            self.fleet.dispatch_queued().await;
        }
        reports
    }

    fn warn_long_queued(&mut self, now: DateTime<Utc>) {
        let Ok(queued) = self.store.queued_tasks() else {
            return;
        };
        let limit = chrono::Duration::from_std(QUEUED_WARN_AFTER).expect("1h fits");
        for t in queued {
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
        let previous = std::mem::replace(&mut self.resolve, Box::new(|_| None));
        self.resolve = Box::new(move |name| {
            if name == id {
                Some(source.clone())
            } else {
                previous(name)
            }
        });
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
        assert!((s.resolve)("only-this").is_some());
        assert!((s.resolve)("clock").is_none(), "the default lookup is gone");
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
        assert_eq!(runs.load(Ordering::SeqCst), 2);
        assert!(s.fire("nope", Utc::now()).unwrap_err().contains("no job"));
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
        // `SchedulerCommand::Reload` (pastor reload) calls this instead of
        // `reload()`, precisely so it is not fooled by an unchanged
        // fingerprint (a symlinked target edited within one mtime granule,
        // for instance).
        assert!(
            s.force_reload(),
            "pastor reload must force a re-read regardless of the fingerprint"
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
}
