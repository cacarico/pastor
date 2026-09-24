//! The tick. `run_job` is one job's pass: ask its connector, drop seen keys,
//! render and queue a task per new item, record the run. `Scheduler` (below,
//! Task 11) owns the loop that calls it.

use std::collections::HashSet;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

use crate::config::job::Job;
use crate::connector::{ItemSource, RunInput};
use crate::machine::PastorEvent;
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
                report.created.push(t.display_id());
            }
            Err(e) => {
                tracing::error!(job = %job.name, key = %item.key, %e, "create task");
                report.error = Some(format!("{}: {e:#}", item.key));
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
        state.last_ok_at = Some(now);
        if output.cursor.is_some() {
            state.cursor = output.cursor;
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

#[cfg(test)]
mod tests {
    use super::*;
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
        let (tx, _rx) = events();
        let now = Utc::now();
        let report = run_job(&store, &job("j"), &src, &tx, now, false).await;
        assert_eq!(report.outcome, RunOutcome::Ran);
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
}
