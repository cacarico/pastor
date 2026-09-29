//! `pastor watch`: one line per change an orchestrator acts on. `TASK` lines
//! come from the head's numbered events, `JOB` lines from `job list` (a job
//! whose last run failed), `HEAD` lines from whether the head answers, and
//! the lines of each connector's `[watch]` command after that, printed once.
//! `--now` prints what needs attention at this moment and exits.
//!
//! A watcher keeps a cursor under the state dir (`watch/<name>.json`): the
//! last event number it printed, the jobs and connectors it last saw failing,
//! whether the head was down, and the connector lines it has printed. A
//! watcher started again with the same name carries on from there, so it
//! repeats nothing; a new name, or `--reset`, starts from the end of the log.
//!
//! It only reads: the head's events, tasks and jobs, and this machine's
//! connectors. So an agent pastor started may run it.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::cli::{CliError, one_line};
use crate::config::{PastorConfig, Paths, parse_duration};
use crate::connector::exec::{self, Invocation, RunLog};
use crate::connector::{Discovered, discover};
use crate::events::{EventRecord, EventsPage};
use crate::ipc::{IpcRequest, IpcResponse};
use crate::scheduler::JobStatus;
use crate::store::TaskFilter;
use crate::task::{LIVE_STATES, Outcome, Task, TaskState};

/// The cursor a watcher with no `--name` keeps.
pub const DEFAULT_NAME: &str = "default";

/// Events asked for per request while catching up.
const PAGE: u32 = 500;

/// Connector lines remembered per connector, newest kept. A line that falls
/// out of it and comes back prints again.
const SEEN_MAX: usize = 1000;

/// The task states a watcher prints without `--all`: the ones an
/// orchestrator acts on.
pub const ATTENTION_STATES: [TaskState; 4] = [
    TaskState::Blocked,
    TaskState::Done,
    TaskState::Failed,
    TaskState::Stale,
];

#[derive(clap::Args, Debug)]
pub struct WatchArgs {
    /// Print what needs attention now (blocked, done, failed and stale tasks,
    /// failing jobs, a head that does not answer, connector lines) and exit
    #[arg(long)]
    pub now: bool,
    /// The watcher's name: its cursor is kept under the state dir, so a
    /// watcher started again with the same name repeats nothing
    #[arg(long, default_value = DEFAULT_NAME, conflicts_with = "now")]
    pub name: String,
    /// Forget the cursor and start from the end of the events log
    #[arg(long, conflicts_with = "now")]
    pub reset: bool,
    /// Every task state change, not only blocked, done, failed and stale
    /// (with --now: every live task too)
    #[arg(long)]
    pub all: bool,
    /// Print one JSON record per line: kind, the line's fields, and line,
    /// its text
    #[arg(long)]
    pub json: bool,
    /// How often to look, like 30s or 2m
    #[arg(long, default_value = "1m", value_parser = parse_interval)]
    pub interval: Duration,
    /// Run this connector's [watch] command (repeatable); replaces
    /// [[watch.connector]] in pastor.toml
    #[arg(long = "connector", value_name = "ID")]
    pub connectors: Vec<String>,
}

fn parse_interval(s: &str) -> Result<Duration, String> {
    let d = parse_duration(s)?;
    if d.is_zero() {
        return Err("must not be zero".into());
    }
    Ok(d)
}

/// One line of output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "UPPERCASE")]
pub enum Line {
    /// A task reached `state`. `outcome` is how its round ended, on a
    /// task done or failed.
    Task {
        task: String,
        state: TaskState,
        machine: Option<String>,
        job: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        outcome: Option<Outcome>,
        reason: Option<String>,
    },
    /// A job's last run failed (`failing`), or it ran fine again (`ok`).
    Job {
        job: String,
        state: String,
        reason: Option<String>,
    },
    /// The head stopped answering (`down`), answers again (`up`), events
    /// were rotated out of its log before this watcher read them (`gap`), or
    /// its log ends before this watcher's cursor (`reset`).
    Head {
        state: String,
        reason: Option<String>,
    },
    /// A connector's `[watch]` command failed (`failing`), or ran fine again
    /// (`ok`).
    Connector {
        connector: String,
        state: String,
        reason: Option<String>,
    },
    /// A line a connector's `[watch]` command printed, as it printed it.
    Output { connector: String, text: String },
}

impl Line {
    /// The text form: a word for what it is about, then its name and state,
    /// then `: reason`. Connector output is printed as the connector wrote it.
    pub fn text(&self) -> String {
        let reason =
            |r: &Option<String>| r.as_deref().map(|r| format!(": {r}")).unwrap_or_default();
        let dash = |v: &Option<String>| v.clone().unwrap_or_else(|| "-".into());
        let line = match self {
            Line::Task {
                task,
                state,
                machine,
                job,
                outcome,
                reason: r,
            } => format!(
                "TASK {task} {state} {} {}{}{}",
                dash(machine),
                dash(job),
                outcome.map(outcome_field).unwrap_or_default(),
                reason(r)
            ),
            Line::Job {
                job,
                state,
                reason: r,
            } => format!("JOB {job} {state}{}", reason(r)),
            Line::Head { state, reason: r } => format!("HEAD {state}{}", reason(r)),
            Line::Connector {
                connector,
                state,
                reason: r,
            } => format!("CONNECTOR {connector} {state}{}", reason(r)),
            Line::Output { text, .. } => text.clone(),
        };
        one_line(&line)
    }

    fn head(state: &str, reason: Option<String>) -> Line {
        Line::Head {
            state: state.into(),
            reason,
        }
    }
}

/// What a watcher remembers between runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Cursor {
    /// The last event number read; `None` until the head first answers.
    pub seq: Option<u64>,
    /// Why the head was down when last asked; `None` while it answers.
    pub head_down: Option<String>,
    /// Failing jobs, with the reason last printed.
    pub jobs: BTreeMap<String, String>,
    pub connectors: BTreeMap<String, ConnectorMemory>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectorMemory {
    /// Why its last run failed; `None` when it ran fine.
    pub failing: Option<String>,
    /// Lines already printed, oldest first, at most `SEEN_MAX`.
    pub seen: VecDeque<String>,
}

impl Cursor {
    pub fn path(paths: &Paths, name: &str) -> PathBuf {
        paths.state_dir.join("watch").join(format!("{name}.json"))
    }

    /// The cursor at `path`; a missing one is a fresh cursor, and so is one
    /// that does not read, with a warning: a watcher must keep watching.
    pub fn load(path: &Path) -> Cursor {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|err| {
                tracing::warn!(path = %path.display(), %err, "watch cursor unreadable; starting over");
                Cursor::default()
            }),
            Err(_) => Cursor::default(),
        }
    }

    /// Write it through a temporary file, so a watcher stopped mid-write
    /// leaves the old cursor whole.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            crate::config::create_private_dir(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// The head answered (`Ok`) or did not (`Err(reason)`): a line when that
    /// changed. A head that was never seen down is taken to have been up, so
    /// a first look at a live head prints nothing.
    pub fn head(&mut self, now: Result<(), String>) -> Option<Line> {
        match (now, self.head_down.take()) {
            (Ok(()), None) => None,
            (Ok(()), Some(_)) => Some(Line::head("up", None)),
            (Err(reason), None) => {
                self.head_down = Some(reason.clone());
                Some(Line::head("down", Some(reason)))
            }
            (Err(_), Some(was)) => {
                self.head_down = Some(was);
                None
            }
        }
    }

    /// The lines for a page of events read after `self.seq`, and the cursor
    /// moved past them.
    pub fn events(&mut self, page: &EventsPage, all: bool) -> Vec<Line> {
        let after = self.seq.unwrap_or(0);
        let mut out = Vec::new();
        if let Some(end) = page.ends_before(after) {
            // Never replayed: what is in the log now was either printed
            // under the old numbers or happened before this watcher looked.
            self.seq = Some(end);
            out.push(Line::head(
                "reset",
                Some(format!(
                    "the head's log ends at event {end}, before this watcher's {after}; going on from its end"
                )),
            ));
            return out;
        }
        if page.gap
            && let Some(oldest) = page.oldest
        {
            out.push(Line::head(
                "gap",
                Some(format!(
                    "events {}..{} were rotated out of the log before this watcher read them",
                    after + 1,
                    oldest - 1
                )),
            ));
        }
        for rec in &page.events {
            if rec.seq > after {
                self.seq = Some(rec.seq);
            }
            out.extend(task_line(rec, all));
        }
        out
    }

    /// The lines for the jobs as `job list` has them now: a job whose last
    /// run failed, when that is news, and one that ran fine again. A disabled
    /// or removed job is forgotten quietly.
    pub fn jobs(&mut self, jobs: &[JobStatus]) -> Vec<Line> {
        let mut out = Vec::new();
        let mut now = BTreeMap::new();
        for j in jobs.iter().filter(|j| j.enabled) {
            match failing(j) {
                Some(reason) => {
                    if self.jobs.get(&j.name) != Some(&reason) {
                        out.push(job_failing(&j.name, &reason));
                    }
                    now.insert(j.name.clone(), reason);
                }
                None if self.jobs.contains_key(&j.name) => out.push(Line::Job {
                    job: j.name.clone(),
                    state: "ok".into(),
                    reason: None,
                }),
                None => {}
            }
        }
        self.jobs = now;
        out
    }

    /// The lines for one run of connector `id`'s `[watch]` command: those it
    /// has not printed before, or a line saying it fails or works again.
    pub fn connector(&mut self, id: &str, ran: Result<Vec<String>, String>) -> Vec<Line> {
        let mem = self.connectors.entry(id.to_string()).or_default();
        let mut out = Vec::new();
        match ran {
            Err(reason) => {
                if mem.failing.is_none() {
                    out.push(connector_state(id, "failing", Some(reason.clone())));
                }
                mem.failing = Some(reason);
            }
            Ok(lines) => {
                if mem.failing.take().is_some() {
                    out.push(connector_state(id, "ok", None));
                }
                for text in lines {
                    if mem.seen.contains(&text) {
                        continue;
                    }
                    if mem.seen.len() == SEEN_MAX {
                        mem.seen.pop_front();
                    }
                    mem.seen.push_back(text.clone());
                    out.push(Line::Output {
                        connector: id.to_string(),
                        text,
                    });
                }
            }
        }
        out
    }
}

/// The reason a job counts as failing: its last run failed.
fn failing(j: &JobStatus) -> Option<String> {
    j.last_result
        .as_deref()
        .filter(|r| r.starts_with("failed"))
        .map(str::to_string)
}

fn job_failing(name: &str, reason: &str) -> Line {
    Line::Job {
        job: name.to_string(),
        state: "failing".into(),
        reason: Some(reason.to_string()),
    }
}

fn connector_state(id: &str, state: &str, reason: Option<String>) -> Line {
    Line::Connector {
        connector: id.to_string(),
        state: state.into(),
        reason,
    }
}

/// ` outcome=partial`, quoted when the outcome is more than one word
/// (` outcome="nothing to do"`), so the line still splits on spaces.
fn outcome_field(o: Outcome) -> String {
    let o = o.as_str();
    if o.contains(' ') {
        format!(" outcome=\"{o}\"")
    } else {
        format!(" outcome={o}")
    }
}

/// How a task's round ended, on a line for it in `state`: only a done or
/// failed task has ended one.
fn task_outcome(task: &Task, state: TaskState) -> Option<Outcome> {
    matches!(state, TaskState::Done | TaskState::Failed)
        .then(|| task.summary.as_ref().map(|s| s.outcome))
        .flatten()
}

/// A `TASK` line for a task event that moved it to a state the watcher
/// prints (`ATTENTION_STATES`, or any with `all`). Events that are not a
/// state change (`task.started`, `task.input`) print nothing.
pub fn task_line(rec: &EventRecord, all: bool) -> Option<Line> {
    let state: TaskState = rec.kind.strip_prefix("task.")?.parse().ok()?;
    if !all && !ATTENTION_STATES.contains(&state) {
        return None;
    }
    let task = rec.task.as_ref()?;
    Some(Line::Task {
        task: task.display_id(),
        state,
        machine: task.machine.clone(),
        job: rec.job.clone(),
        outcome: matches!(state, TaskState::Done | TaskState::Failed)
            .then(|| rec.summary.as_ref().map(|s| s.outcome))
            .flatten()
            .or_else(|| task_outcome(task, state)),
        reason: task_reason(task, state),
    })
}

/// A task's error, only in a state where it is the reason: a task retried
/// or closed after a failure keeps the old error in its row.
fn task_reason(task: &Task, state: TaskState) -> Option<String> {
    matches!(state, TaskState::Failed | TaskState::Stale)
        .then(|| task.error.clone())
        .flatten()
}

/// The `TASK` lines of `--now`: each task in a state that needs attention,
/// or any live one too with `all`.
pub fn now_task_lines(tasks: &[Task], all: bool) -> Vec<Line> {
    tasks
        .iter()
        .filter(|t| ATTENTION_STATES.contains(&t.state) || (all && LIVE_STATES.contains(&t.state)))
        .map(|t| Line::Task {
            task: t.display_id(),
            state: t.state,
            machine: t.machine.clone(),
            job: Some(t.job.clone()),
            outcome: task_outcome(t, t.state),
            reason: task_reason(t, t.state),
        })
        .collect()
}

/// The `JOB` lines of `--now`: every enabled job whose last run failed.
pub fn now_job_lines(jobs: &[JobStatus]) -> Vec<Line> {
    jobs.iter()
        .filter(|j| j.enabled)
        .filter_map(|j| failing(j).map(|r| job_failing(&j.name, &r)))
        .collect()
}

/// A connector's `[watch]` command's lines as `Cursor::connector` takes
/// them, or its `CONNECTOR ... failing` line and nothing else.
pub fn now_connector_lines(id: &str, ran: Result<Vec<String>, String>) -> Vec<Line> {
    match ran {
        Err(reason) => vec![connector_state(id, "failing", Some(reason))],
        Ok(lines) => {
            let mut seen = BTreeSet::new();
            lines
                .into_iter()
                .filter(|l| seen.insert(l.clone()))
                .map(|text| Line::Output {
                    connector: id.to_string(),
                    text,
                })
                .collect()
        }
    }
}

/// Run connector `id`'s `[watch]` command once, in its directory with its
/// env and no job, and return the lines it printed (trimmed, empty and
/// duplicate ones dropped, capped at `SEEN_MAX` so a flooding connector
/// can't grow this without bound). `Err` says why there are none: no such
/// connector, no `[watch]` command, or a run that failed. Its stderr goes
/// to a run log under `runs/@<id>/`, as a hook's does.
pub async fn run_connector(paths: &Paths, id: &str) -> Result<Vec<String>, String> {
    let found = discover(paths)
        .map_err(|e| format!("{e:#}"))?
        .into_iter()
        .find(|d| d.id() == id);
    let connector = match found {
        Some(Discovered::Valid(c)) => c,
        Some(Discovered::Invalid { error, .. }) => return Err(format!("invalid: {error}")),
        None => {
            return Err(format!(
                "no connector {id:?} in {}",
                paths.connectors_dir().display()
            ));
        }
    };
    let Some(watch) = connector.manifest.watch.clone() else {
        return Err(format!("connector {id:?} has no [watch] command"));
    };
    let (env, redactor) = connector
        .command_env(paths, None, false)
        .map_err(|e| format!("{e:#}"))?;
    let log = RunLog::create(&paths.runs_dir(&format!("@{id}")), redactor)
        .map_err(|e| format!("{e:#}"))?
        .shared();
    log.lock().unwrap_or_else(|p| p.into_inner()).line("watch");
    let inv = Invocation {
        argv: watch.command.clone(),
        cwd: connector.dir.clone(),
        env,
        stdin: Vec::new(),
        timeout: Some(watch.timeout),
    };
    let mut lines = Vec::new();
    let mut seen = BTreeSet::new();
    let out_log = log.clone();
    let done = exec::run(inv, log, |line| {
        out_log
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .line(&format!("stdout: {line}"));
        let line = line.trim();
        if !line.is_empty() && lines.len() < SEEN_MAX && seen.insert(line.to_string()) {
            lines.push(line.to_string());
        }
    })
    .await;
    if !done.exit.success() {
        return Err(done.reason());
    }
    Ok(lines)
}

/// The connectors a watcher runs: `--connector`, else pastor.toml's
/// `[[watch.connector]]`. A pastor.toml that does not load gives none, with
/// a warning: the head's lines still come.
fn connectors(paths: &Paths, flags: &[String]) -> Vec<String> {
    if !flags.is_empty() {
        return flags.to_vec();
    }
    match PastorConfig::load(&paths.config_file()) {
        Ok(c) => c.watch.connector.into_iter().map(|c| c.name).collect(),
        Err(err) => {
            tracing::warn!(err = %format!("{err:#}"), "pastor watch: pastor.toml does not load; no connectors");
            Vec::new()
        }
    }
}

/// Why the head gave no answer, in a line.
enum Ask {
    /// No answer at all: the head is down, as far as a watcher can tell.
    Down(String),
    /// An answer that is an error: the head is up and refused.
    Refused(String, String),
}

/// `cli::ask` for a watcher, which also needs to tell a head that gave no
/// answer from one that refused.
async fn ask(paths: &Paths, req: IpcRequest) -> Result<IpcResponse, Ask> {
    let got = crate::ipc::request_head(paths, &req).await;
    let down = matches!(&got, Err(e) if !matches!(e, crate::ipc::RequestError::Refused { .. }));
    crate::cli::reply(got).map_err(|e| {
        if down {
            Ask::Down(e.message)
        } else {
            Ask::Refused(e.code, e.message)
        }
    })
}

/// A refusal ends the watcher with the head's code: an agent's bridge or an
/// old head that will never answer it (`EventsSince` needs
/// `EVENTS_PROTOCOL`).
fn refused(code: String, message: String) -> anyhow::Error {
    CliError::err(&code, message)
}

/// Print `lines`; false once stdout is closed (`pastor watch | head`).
fn print(lines: &[Line], json: bool) -> bool {
    let mut out = std::io::stdout().lock();
    for l in lines {
        let text = if json {
            let mut v = serde_json::to_value(l).unwrap_or_default();
            v["line"] = l.text().into();
            v.to_string()
        } else {
            l.text()
        };
        if writeln!(out, "{text}").is_err() {
            return false;
        }
    }
    out.flush().is_ok()
}

/// `pastor watch`.
pub async fn cli(paths: &Paths, args: WatchArgs) -> anyhow::Result<()> {
    let ids = connectors(paths, &args.connectors);
    if args.now {
        let lines = now(paths, &ids, args.all).await?;
        print(&lines, args.json);
        return Ok(());
    }
    crate::config::job::check_name(&args.name)
        .map_err(|e| CliError::err("invalid_name", format!("--name: {e}")))?;
    let path = Cursor::path(paths, &args.name);
    let mut cursor = if args.reset {
        Cursor::default()
    } else {
        Cursor::load(&path)
    };
    loop {
        let lines = step(paths, &mut cursor, &ids, args.all).await?;
        if !print(&lines, args.json) {
            return Ok(());
        }
        cursor.save(&path)?;
        tokio::time::sleep(args.interval).await;
    }
}

/// One look: the head's events and jobs, then the connectors.
async fn step(
    paths: &Paths,
    cursor: &mut Cursor,
    ids: &[String],
    all: bool,
) -> anyhow::Result<Vec<Line>> {
    let mut lines = Vec::new();
    match head_lines(paths, cursor, all).await {
        Ok(more) => {
            lines.extend(cursor.head(Ok(())));
            lines.extend(more);
        }
        Err(Ask::Down(reason)) => lines.extend(cursor.head(Err(reason))),
        Err(Ask::Refused(code, message)) => return Err(refused(code, message)),
    }
    for id in ids {
        let ran = run_connector(paths, id).await;
        lines.extend(cursor.connector(id, ran));
    }
    Ok(lines)
}

/// The events since the cursor, then the jobs. A cursor that has never seen
/// the head starts at the end of its log.
async fn head_lines(paths: &Paths, cursor: &mut Cursor, all: bool) -> Result<Vec<Line>, Ask> {
    let mut lines = Vec::new();
    if cursor.seq.is_none() {
        cursor.seq = Some(log_end(paths).await?);
    }
    loop {
        let page = events(paths, cursor.seq.unwrap_or(0), PAGE).await?;
        let full = page.events.len() >= PAGE as usize;
        lines.extend(cursor.events(&page, all));
        if !full {
            break;
        }
    }
    let IpcResponse::Jobs(jobs) = ask(paths, IpcRequest::JobList).await? else {
        return Err(unexpected("job list"));
    };
    lines.extend(cursor.jobs(&jobs));
    Ok(lines)
}

fn unexpected(what: &str) -> Ask {
    Ask::Refused(
        "runtime_error".into(),
        format!("the head answered {what} with something else"),
    )
}

async fn events(paths: &Paths, after: u64, limit: u32) -> Result<EventsPage, Ask> {
    match ask(
        paths,
        IpcRequest::EventsSince {
            after,
            limit,
            task: None,
        },
    )
    .await?
    {
        IpcResponse::Events(page) => Ok(page),
        _ => Err(unexpected("events")),
    }
}

/// The newest event number in the head's log (0 for an empty log): asked
/// with a limit of 0, which a head that knows `EventsPage::newest` answers
/// at once. An older head is paged through to the end.
async fn log_end(paths: &Paths) -> Result<u64, Ask> {
    let (mut after, mut limit) = (0, 0);
    loop {
        let page = events(paths, after, limit).await?;
        if let Some(newest) = page.newest {
            return Ok(newest.max(after));
        }
        if let Some(last) = page.events.last() {
            after = last.seq;
        }
        if limit > 0 && page.events.len() < limit as usize {
            return Ok(after);
        }
        limit = PAGE;
    }
}

/// `--now`: what needs attention at this moment.
async fn now(paths: &Paths, ids: &[String], all: bool) -> anyhow::Result<Vec<Line>> {
    let mut lines = Vec::new();
    let states = if all {
        ATTENTION_STATES
            .iter()
            .chain(LIVE_STATES.iter())
            .copied()
            .collect()
    } else {
        ATTENTION_STATES.to_vec()
    };
    let filter = TaskFilter {
        states: Some(states),
        ..TaskFilter::default()
    };
    let head = async {
        let IpcResponse::Tasks(tasks) = ask(paths, IpcRequest::List { filter }).await? else {
            return Err(unexpected("task list"));
        };
        let IpcResponse::Jobs(jobs) = ask(paths, IpcRequest::JobList).await? else {
            return Err(unexpected("job list"));
        };
        Ok((tasks, jobs))
    };
    match head.await {
        Ok((tasks, jobs)) => {
            lines.extend(now_task_lines(&tasks, all));
            lines.extend(now_job_lines(&jobs));
        }
        Err(Ask::Down(reason)) => lines.push(Line::head("down", Some(reason))),
        Err(Ask::Refused(code, message)) => return Err(refused(code, message)),
    }
    for id in ids {
        lines.extend(now_connector_lines(id, run_connector(paths, id).await));
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::DispatchSpec;

    fn task(id: i64, state: TaskState) -> Task {
        let store = crate::store::Store::open_in_memory().unwrap();
        let mut t = store
            .insert_task(crate::store::NewTask {
                job: "nightly".into(),
                item: serde_json::json!({"key": "k"}),
                prompt: "p".into(),
                spec: DispatchSpec {
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
                    label: Default::default(),
                    summary: Default::default(),
                },
                flock: "default".into(),
                description: None,
            })
            .unwrap();
        t.id = id;
        t.state = state;
        t.machine = Some("pi-1".into());
        t
    }

    fn rec(seq: u64, kind: &str, t: &Task) -> EventRecord {
        EventRecord {
            summary: None,
            seq,
            at: chrono::Utc::now(),
            kind: kind.into(),
            task: Some(t.clone()),
            model: None,
            job: Some(t.job.clone()),
            machine: None,
            detail: None,
        }
    }

    fn page(events: Vec<EventRecord>) -> EventsPage {
        EventsPage {
            oldest: events.first().map(|r| r.seq),
            newest: events.last().map(|r| r.seq),
            events,
            gap: false,
        }
    }

    fn job(name: &str, last: Option<&str>) -> JobStatus {
        JobStatus {
            name: name.into(),
            schedule: Some("every 1h".into()),
            enabled: true,
            connector: Some("clock".into()),
            error: None,
            last_run_at: None,
            last_result: last.map(Into::into),
            next_due: None,
            running: false,
            flock: None,
            description: None,
        }
    }

    fn texts(lines: &[Line]) -> Vec<String> {
        lines.iter().map(Line::text).collect()
    }

    #[test]
    fn task_events_print_the_states_an_orchestrator_acts_on() {
        let mut failed = task(12, TaskState::Failed);
        failed.error = Some("agent exited".into());
        let running = task(13, TaskState::Running);
        let mut c = Cursor {
            seq: Some(0),
            ..Cursor::default()
        };
        let lines = c.events(
            &page(vec![
                rec(1, "task.running", &running),
                rec(2, "task.failed", &failed),
                rec(3, "task.started", &running),
                rec(4, "task.blocked", &task(14, TaskState::Blocked)),
            ]),
            false,
        );
        assert_eq!(
            texts(&lines),
            [
                "TASK t-12 failed pi-1 nightly: agent exited",
                "TASK t-14 blocked pi-1 nightly",
            ]
        );
        assert_eq!(c.seq, Some(4), "past every event read, printed or not");

        let mut c = Cursor::default();
        let lines = c.events(&page(vec![rec(5, "task.running", &running)]), true);
        assert_eq!(texts(&lines), ["TASK t-13 running pi-1 nightly"]);
    }

    /// A done or failed task's line says how its round ended.
    #[test]
    fn a_task_end_prints_its_outcome() {
        use crate::task::{Outcome, SummarySource, TaskSummary};
        let summary = |outcome| TaskSummary {
            round: 1,
            outcome,
            text: String::new(),
            source: SummarySource::Agent,
            at: chrono::Utc::now(),
        };
        let mut failed = task(12, TaskState::Failed);
        failed.error = Some("agent exited".into());
        let mut ended = rec(2, "task.failed", &failed);
        ended.summary = Some(summary(Outcome::NoSummary));
        let mut done = rec(3, "task.done", &task(13, TaskState::Done));
        done.summary = Some(summary(Outcome::Partial));
        let mut from_row = task(14, TaskState::Done);
        from_row.summary = Some(summary(Outcome::NothingToDo));
        let mut c = Cursor::default();
        let lines = c.events(
            &page(vec![ended, done, rec(4, "task.done", &from_row)]),
            false,
        );
        assert_eq!(
            texts(&lines),
            [
                "TASK t-12 failed pi-1 nightly outcome=\"no summary\": agent exited",
                "TASK t-13 done pi-1 nightly outcome=partial",
                "TASK t-14 done pi-1 nightly outcome=\"nothing to do\"",
            ]
        );
        assert_eq!(
            texts(&now_task_lines(&[from_row], false)),
            ["TASK t-14 done pi-1 nightly outcome=\"nothing to do\""]
        );
    }

    #[test]
    fn a_gap_in_the_log_is_a_head_line() {
        let mut c = Cursor {
            seq: Some(3),
            ..Cursor::default()
        };
        let mut p = page(vec![rec(9, "task.done", &task(1, TaskState::Done))]);
        p.gap = true;
        let lines = c.events(&p, false);
        assert_eq!(
            texts(&lines)[0],
            "HEAD gap: events 4..8 were rotated out of the log before this watcher read them"
        );
        assert_eq!(texts(&lines)[1], "TASK t-1 done pi-1 nightly");
    }

    /// A page from a head whose log holds `oldest..=newest`, read after a
    /// cursor past all of it.
    fn ended(oldest: Option<u64>, newest: Option<u64>) -> EventsPage {
        EventsPage {
            events: vec![],
            gap: false,
            oldest,
            newest,
        }
    }

    #[test]
    fn a_cursor_past_the_heads_log_resets_to_its_end_once() {
        let mut c = Cursor {
            seq: Some(500),
            ..Cursor::default()
        };
        let lines = c.events(&ended(Some(1), Some(20)), false);
        assert_eq!(
            texts(&lines),
            [
                "HEAD reset: the head's log ends at event 20, before this watcher's 500; going on from its end"
            ]
        );
        assert_eq!(c.seq, Some(20));
        let lines = c.events(
            &page(vec![rec(21, "task.done", &task(1, TaskState::Done))]),
            false,
        );
        assert_eq!(texts(&lines), ["TASK t-1 done pi-1 nightly"]);
        assert_eq!(c.seq, Some(21));
    }

    #[test]
    fn a_cursor_at_the_heads_newest_event_prints_nothing() {
        let mut c = Cursor {
            seq: Some(20),
            ..Cursor::default()
        };
        assert!(c.events(&ended(Some(1), Some(20)), false).is_empty());
        assert_eq!(c.seq, Some(20));
    }

    #[test]
    fn an_empty_head_log_under_an_old_cursor_resets_to_zero() {
        let mut c = Cursor {
            seq: Some(500),
            ..Cursor::default()
        };
        let lines = c.events(&ended(None, None), false);
        assert_eq!(
            texts(&lines),
            [
                "HEAD reset: the head's log ends at event 0, before this watcher's 500; going on from its end"
            ]
        );
        assert_eq!(c.seq, Some(0));
        assert!(c.events(&ended(None, None), false).is_empty(), "once");
        let lines = c.events(
            &page(vec![rec(1, "task.done", &task(1, TaskState::Done))]),
            false,
        );
        assert_eq!(texts(&lines), ["TASK t-1 done pi-1 nightly"]);
    }

    #[test]
    fn a_head_going_down_and_back_up_prints_each_change_once() {
        let mut c = Cursor::default();
        assert_eq!(c.head(Ok(())), None, "a first look at a live head");
        let down = c.head(Err("connection refused".into())).unwrap();
        assert_eq!(down.text(), "HEAD down: connection refused");
        assert_eq!(c.head(Err("timed out".into())), None, "still down");
        assert_eq!(c.head(Ok(())).unwrap().text(), "HEAD up");
        assert_eq!(c.head(Ok(())), None);
    }

    #[test]
    fn a_failing_job_prints_when_it_fails_again_differently_and_when_it_recovers() {
        let mut c = Cursor::default();
        let fail1 = [
            job("a", Some("failed (1x): boom")),
            job("b", Some("ok: 1 items, 1 tasks")),
        ];
        assert_eq!(texts(&c.jobs(&fail1)), ["JOB a failing: failed (1x): boom"]);
        assert!(c.jobs(&fail1).is_empty(), "nothing new");
        let fail2 = [job("a", Some("failed (2x): boom"))];
        assert_eq!(texts(&c.jobs(&fail2)), ["JOB a failing: failed (2x): boom"]);
        assert_eq!(
            texts(&c.jobs(&[job("a", Some("ok: 0 items, 0 tasks"))])),
            ["JOB a ok"]
        );
        // A disabled job is forgotten, without an `ok` it did not earn.
        c.jobs(&fail1);
        let mut off = job("a", Some("failed (1x): boom"));
        off.enabled = false;
        assert!(c.jobs(&[off]).is_empty());
        assert!(c.jobs.is_empty());
    }

    #[test]
    fn connector_lines_print_once_and_a_failure_says_so_once() {
        let mut c = Cursor::default();
        let run = |l: &[&str]| Ok(l.iter().map(|s| s.to_string()).collect());
        assert_eq!(
            texts(&c.connector("review", run(&["PR 3 open", "PR 3 open", "PR 4 open"]))),
            ["PR 3 open", "PR 4 open"]
        );
        assert!(
            c.connector("review", run(&["PR 3 open", "PR 4 open"]))
                .is_empty()
        );
        assert_eq!(
            texts(&c.connector("review", Err("exit 1: gh: not logged in".into()))),
            ["CONNECTOR review failing: exit 1: gh: not logged in"]
        );
        assert!(
            c.connector("review", Err("exit 1: again".into()))
                .is_empty()
        );
        assert_eq!(
            texts(&c.connector("review", run(&["PR 3 open", "PR 3 merged"]))),
            ["CONNECTOR review ok", "PR 3 merged"]
        );
    }

    #[test]
    fn the_seen_lines_are_bounded() {
        let mut c = Cursor::default();
        let many: Vec<String> = (0..SEEN_MAX + 5).map(|i| format!("l{i}")).collect();
        c.connector("x", Ok(many));
        let mem = &c.connectors["x"];
        assert_eq!(mem.seen.len(), SEEN_MAX);
        assert_eq!(mem.seen.front().unwrap(), "l5");
    }

    #[test]
    fn now_lists_what_needs_attention() {
        let tasks = [
            task(1, TaskState::Running),
            task(2, TaskState::Blocked),
            task(3, TaskState::Done),
            task(4, TaskState::Closed),
        ];
        assert_eq!(
            texts(&now_task_lines(&tasks, false)),
            [
                "TASK t-2 blocked pi-1 nightly",
                "TASK t-3 done pi-1 nightly"
            ]
        );
        assert_eq!(now_task_lines(&tasks, true).len(), 3, "running too");
        assert_eq!(
            texts(&now_job_lines(&[
                job("a", Some("failed: x")),
                job("b", None)
            ])),
            ["JOB a failing: failed: x"]
        );
        assert_eq!(
            texts(&now_connector_lines("r", Err("exit 2".into()))),
            ["CONNECTOR r failing: exit 2"]
        );
    }

    #[test]
    fn json_lines_carry_their_kind_and_text() {
        let l = Line::Output {
            connector: "review".into(),
            text: "PR 3 open".into(),
        };
        let v = serde_json::to_value(&l).unwrap();
        assert_eq!(v["kind"], "OUTPUT");
        assert_eq!(v["connector"], "review");
    }

    #[test]
    fn a_cursor_survives_a_save_and_a_bad_file_starts_over() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("watch/w.json");
        let mut c = Cursor {
            seq: Some(7),
            ..Cursor::default()
        };
        c.connector("x", Ok(vec!["a".into()]));
        c.save(&path).unwrap();
        assert_eq!(Cursor::load(&path), c);
        std::fs::write(&path, "{").unwrap();
        assert_eq!(Cursor::load(&path), Cursor::default());
    }
}
