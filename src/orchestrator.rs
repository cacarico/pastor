//! Orchestrators: one TOML file each in `~/.config/pastor/orchestrators/`,
//! run by the head. A file names its `kind`: a `scheduled` orchestrator runs
//! a pre script on a schedule and starts one agent, with the role
//! `orchestrator`, only when the script prints lines that need judgment; a
//! `session` orchestrator is one agent kept running through set hours. This
//! version checks both kinds' files and runs the scheduled kind.
//!
//! `Orchestrator::parse` is the file; `State` is what the head keeps between
//! runs under `state/orchestrators/<name>/` (no table in the store); `Runner`
//! is the head's loop that reads the files, runs what is due, and runs the
//! post script once a run's agent has ended.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Local, NaiveTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::config::{AgentChoice, Paths, parse_duration};
use crate::connector::env::Redactor;
use crate::connector::exec::{self, Invocation, RunLog};
use crate::daemon::{Fleet, QueueError};
use crate::machine::PastorEvent;
use crate::schedule::Schedule;
use crate::store::{Store, TaskFilter};
use crate::task::{DispatchSpec, Task, TaskRole, TaskState};

/// Set for an orchestrator's pre and post scripts to its name. The CLI
/// passes it on to the head (`ipc::FROM_ORCHESTRATOR_FIELD`), which applies
/// the orchestrator role's table to the request. Like `ipc::TASK_ENV`, a
/// guard against mistakes, not a boundary.
pub const ORCHESTRATOR_ENV: &str = "PASTOR_ORCHESTRATOR";

/// The orchestrator's scratch dir, kept between runs, for its scripts.
pub const SCRATCH_ENV: &str = "PASTOR_ORCHESTRATOR_STATE_DIR";

/// How long a pre or post script may run when the file sets no `timeout`.
pub const DEFAULT_SCRIPT_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// How long a session orchestrator gets to end after its last message.
pub const DEFAULT_STOP_GRACE: Duration = Duration::from_secs(5 * 60);

/// The longest handover note kept, in bytes; a longer one is cut.
pub const NOTE_MAX: usize = 4096;

/// How long the head waits after a quota error whose message names no reset
/// time.
pub const QUOTA_WAIT: Duration = Duration::from_secs(3600);

/// Runs kept in `State::runs` for `orchestrator describe`.
const RUNS_KEPT: usize = 10;

/// Recent events `orchestrator describe` shows.
const EVENTS_SHOWN: usize = 10;

/// What pastor asks of every orchestrator agent at the end of its prompt.
pub const SUMMARY_ASK: &str = "When you finish, run `pastor task done --summary-file -` with a short summary on stdin: first line `done`, `partial`, `blocked` or `nothing to do`; then up to five short lines: what changed, where (branch, PR, files or notes), what is left.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Scheduled,
    Session,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Scheduled => "scheduled",
            Kind::Session => "session",
        }
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The file's shape. Both kinds' keys are here so that a key of the other
/// kind is named in the error rather than reported as unknown.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OrchestratorFile {
    kind: Option<String>,
    description: Option<String>,
    enabled: Option<bool>,
    model: Option<String>,
    skill: Option<String>,
    prompt: Option<String>,
    repo: Option<String>,
    every: Option<String>,
    cron: Option<String>,
    pre: Option<Vec<String>>,
    post: Option<Vec<String>>,
    timeout: Option<String>,
    hours: Option<Hours>,
    stop_grace: Option<String>,
}

/// A session orchestrator's `hours`, local `HH:MM` each.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hours {
    pub start: String,
    pub stop: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Orchestrator {
    pub name: String,
    pub description: Option<String>,
    pub enabled: bool,
    /// A `[models]` name for its agents.
    pub model: Option<String>,
    /// A skill its agents are told to use.
    pub skill: Option<String>,
    pub prompt: String,
    /// The repo its agents work in, each in a worktree of its own; `None`
    /// starts them in the home directory.
    pub repo: Option<String>,
    /// The directory of its file: scripts' paths are relative to it, and an
    /// `.env` there is read into their environment.
    pub dir: PathBuf,
    pub plan: Plan,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    Scheduled(Scheduled),
    Session(Session),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scheduled {
    pub schedule: Schedule,
    pub pre: Vec<String>,
    pub post: Option<Vec<String>>,
    /// For the pre and the post script each.
    pub timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub hours: Hours,
    pub stop_grace: Duration,
}

impl Orchestrator {
    pub fn kind(&self) -> Kind {
        match self.plan {
            Plan::Scheduled(_) => Kind::Scheduled,
            Plan::Session(_) => Kind::Session,
        }
    }

    /// Parse and check one file's text. `stem` is the file name without
    /// `.toml`, the orchestrator's name; `dir` is the file's directory.
    pub fn parse(text: &str, stem: &str, dir: &Path) -> Result<Orchestrator, String> {
        check_name(stem)?;
        let file: OrchestratorFile = toml::from_str(text).map_err(|e| e.to_string())?;
        let kind = match file.kind.as_deref() {
            None => {
                return Err("kind is required: \"scheduled\" or \"session\"".into());
            }
            Some("scheduled") => Kind::Scheduled,
            Some("session") => Kind::Session,
            Some(other) => {
                return Err(format!(
                    "unknown kind {other:?}: \"scheduled\" or \"session\""
                ));
            }
        };
        let other_kind: &[(&str, bool)] = match kind {
            Kind::Scheduled => &[
                ("hours", file.hours.is_some()),
                ("stop_grace", file.stop_grace.is_some()),
            ],
            Kind::Session => &[
                ("every", file.every.is_some()),
                ("cron", file.cron.is_some()),
                ("pre", file.pre.is_some()),
                ("post", file.post.is_some()),
                ("timeout", file.timeout.is_some()),
            ],
        };
        if let Some((key, _)) = other_kind.iter().find(|(_, set)| *set) {
            let theirs = match kind {
                Kind::Scheduled => Kind::Session,
                Kind::Session => Kind::Scheduled,
            };
            return Err(format!(
                "{key} is a {theirs} orchestrator's key, and this file's kind is {kind:?}",
                kind = kind.as_str()
            ));
        }
        let prompt = file.prompt.unwrap_or_default();
        if prompt.trim().is_empty() {
            return Err("prompt is required".into());
        }
        if let Some(model) = &file.model {
            crate::config::check_model_name(model).map_err(|e| format!("model: {e}"))?;
        }
        if let Some(skill) = &file.skill
            && skill.trim().is_empty()
        {
            return Err("skill must not be empty".into());
        }
        let plan = match kind {
            Kind::Scheduled => {
                let schedule = Schedule::from_fields(file.every.as_deref(), file.cron.as_deref())?;
                let pre = file
                    .pre
                    .ok_or("pre is required: the script that runs first")?;
                check_command("pre", &pre)?;
                if let Some(post) = &file.post {
                    check_command("post", post)?;
                }
                let timeout = match file.timeout.as_deref() {
                    Some(t) => parse_duration(t).map_err(|e| format!("timeout: {e}"))?,
                    None => DEFAULT_SCRIPT_TIMEOUT,
                };
                if timeout.is_zero() {
                    return Err("timeout: must not be zero".into());
                }
                Plan::Scheduled(Scheduled {
                    schedule,
                    pre,
                    post: file.post,
                    timeout,
                })
            }
            Kind::Session => {
                let hours = file
                    .hours
                    .ok_or("hours is required: { start = \"22:00\", stop = \"08:00\" }")?;
                for (key, value) in [("start", &hours.start), ("stop", &hours.stop)] {
                    parse_clock(value).ok_or(format!("hours.{key}: {value:?} is not HH:MM"))?;
                }
                let stop_grace = match file.stop_grace.as_deref() {
                    Some(t) => parse_duration(t).map_err(|e| format!("stop_grace: {e}"))?,
                    None => DEFAULT_STOP_GRACE,
                };
                Plan::Session(Session { hours, stop_grace })
            }
        };
        Ok(Orchestrator {
            name: stem.to_string(),
            description: crate::config::clean_description(file.description.as_deref()),
            enabled: file.enabled.unwrap_or(true),
            model: file.model,
            skill: file.skill.map(|s| s.trim().to_string()),
            prompt,
            repo: file.repo.filter(|r| !r.trim().is_empty()),
            dir: dir.to_path_buf(),
            plan,
        })
    }

    /// When a scheduled orchestrator's next run is due, given its state and
    /// when the head first read its file: the same rule as a job's (a
    /// backoff is a retry time; a new `every` runs at once, a new `cron`
    /// waits for its first time). `None` for a disabled or session one.
    pub fn next_run(
        &self,
        state: &State,
        first_seen: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        let Plan::Scheduled(s) = &self.plan else {
            return None;
        };
        if !self.enabled {
            return None;
        }
        if let Some(until) = state.backoff_until {
            return Some(until);
        }
        match (&s.schedule, state.last_run_at) {
            (Schedule::Every(_), None) => Some(now),
            (Schedule::Cron(_), None) => s.schedule.next_after(first_seen),
            (_, Some(last)) => s.schedule.next_after(last),
        }
    }

    /// The prompt of the agent a run starts: the file's prompt, the skill,
    /// the handover note, every line the pre script printed, then the ask
    /// for a summary.
    pub fn agent_prompt(&self, note: Option<&str>, lines: &[String]) -> String {
        let mut out = self.prompt.trim_end().to_string();
        if let Some(skill) = &self.skill {
            out.push_str(&format!("\n\nUse your {skill} skill."));
        }
        if let Some(note) = note.map(str::trim).filter(|n| !n.is_empty()) {
            out.push_str(&format!(
                "\n\nThe handover note the last run left (`pastor orchestrator note` replaces it):\n{}",
                crate::template::strip_controls(note)
            ));
        }
        if !lines.is_empty() {
            out.push_str(
                "\n\nThe pre script printed these lines, one per thing that needs judgment:\n",
            );
            out.push_str(&lines.join("\n"));
        }
        out.push_str("\n\n");
        out.push_str(SUMMARY_ASK);
        out
    }
}

/// `HH:MM` (or `H:MM`) as a time of day.
fn parse_clock(s: &str) -> Option<NaiveTime> {
    let (h, m) = s.trim().split_once(':')?;
    if m.len() != 2 || h.is_empty() || h.len() > 2 {
        return None;
    }
    NaiveTime::from_hms_opt(h.parse().ok()?, m.parse().ok()?, 0)
}

fn check_command(key: &str, argv: &[String]) -> Result<(), String> {
    match argv.first() {
        Some(p) if !p.trim().is_empty() => Ok(()),
        _ => Err(format!("{key} must name a command, like [\"./{key}.sh\"]")),
    }
}

/// Orchestrator names are file names, directory names under the state dir
/// and appear in events, so they keep to a job's alphabet.
pub fn check_name(name: &str) -> Result<(), String> {
    let first_ok = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let rest_ok = name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_.-".contains(c));
    if first_ok && rest_ok && name.len() <= 64 {
        Ok(())
    } else {
        Err(format!(
            "orchestrator name {name:?} must match [a-z0-9][a-z0-9_.-]{{0,63}}"
        ))
    }
}

/// One file as read: the orchestrator, or why the file is invalid.
#[derive(Debug, Clone)]
pub enum Loaded {
    Valid(Box<Orchestrator>),
    Invalid { name: String, error: String },
}

impl Loaded {
    pub fn name(&self) -> &str {
        match self {
            Loaded::Valid(o) => &o.name,
            Loaded::Invalid { name, .. } => name,
        }
    }
}

/// Every `*.toml` in `dir`, by name; a missing dir has none.
pub fn load_dir(dir: &Path) -> anyhow::Result<Vec<Loaded>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", dir.display())),
    };
    let mut out = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        out.push(load_file(&path, stem));
    }
    out.sort_by(|a, b| a.name().cmp(b.name()));
    Ok(out)
}

pub fn load_file(path: &Path, stem: &str) -> Loaded {
    let dir = path.parent().unwrap_or(Path::new("."));
    let parsed = std::fs::read_to_string(path)
        .map_err(|e| e.to_string())
        .and_then(|text| Orchestrator::parse(&text, stem, dir));
    match parsed {
        Ok(o) => Loaded::Valid(Box::new(o)),
        Err(error) => Loaded::Invalid {
            name: stem.to_string(),
            error,
        },
    }
}

/// The file of orchestrator `name`, refused for a name that could leave the
/// directory, and `orchestrator_not_found` when there is no such file.
pub fn file_of(paths: &Paths, name: &str) -> Result<PathBuf, (String, String)> {
    check_name(name).map_err(|e| ("orchestrator_not_found".into(), e))?;
    let path = paths.orchestrators_dir().join(format!("{name}.toml"));
    if path.exists() {
        Ok(path)
    } else {
        Err((
            "orchestrator_not_found".into(),
            format!(
                "no orchestrator named {name:?} ({} does not exist)",
                path.display()
            ),
        ))
    }
}

/// `orchestrator enable|disable`: the `enabled` line of its file, as `job
/// enable|disable` edits a job's.
pub fn set_enabled(paths: &Paths, name: &str, enabled: bool) -> Result<String, (String, String)> {
    let path = file_of(paths, name)?;
    crate::config::job::set_enabled(&path, enabled)
        .map_err(|e| ("runtime_error".to_string(), format!("{e:#}")))?;
    Ok(format!(
        "orchestrator {name} {}",
        if enabled { "enabled" } else { "disabled" }
    ))
}

/// How one scheduled run ended, for `orchestrator describe`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    /// The pre script printed nothing: no agent.
    NoLines,
    /// An agent started with the lines.
    Started,
    /// The last run's agent still worked: nothing ran.
    Skipped,
    /// Lines, but no agent: `max_orchestrators` or a quota wait.
    Held,
    /// The pre script failed, or the agent could not be queued.
    Failed,
}

impl std::fmt::Display for RunOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RunOutcome::NoLines => "no lines",
            RunOutcome::Started => "started",
            RunOutcome::Skipped => "skipped",
            RunOutcome::Held => "held",
            RunOutcome::Failed => "failed",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    pub at: DateTime<Utc>,
    pub outcome: RunOutcome,
    /// Why it was skipped, held or failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// What the pre script printed (dropped on a failure).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<String>,
    /// The agent it started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<i64>,
    /// The pre script's log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<PathBuf>,
}

/// What the head keeps for one orchestrator between runs, in
/// `state/orchestrators/<name>/state.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_result: Option<String>,
    /// Failed runs in a row; the backoff doubles with each.
    pub failures: u32,
    pub backoff_until: Option<DateTime<Utc>>,
    /// The last agent a run started.
    pub task: Option<i64>,
    /// That agent's post script has not run yet.
    pub post_pending: bool,
    /// The lines that agent was started with, for the post script.
    pub lines: Vec<String>,
    /// No agent starts before this: its last one stopped on a quota error.
    pub quota_until: Option<DateTime<Utc>>,
    /// The last runs, oldest first.
    pub runs: Vec<RunRecord>,
}

impl State {
    fn record(&mut self, run: RunRecord) {
        self.last_result = Some(match &run.detail {
            Some(d) => format!("{}: {d}", run.outcome),
            None => match run.task {
                Some(id) => format!("{} t-{id}", run.outcome),
                None => run.outcome.to_string(),
            },
        });
        self.runs.push(run);
        let extra = self.runs.len().saturating_sub(RUNS_KEPT);
        self.runs.drain(..extra);
    }
}

fn state_file(paths: &Paths, name: &str) -> PathBuf {
    paths.orchestrator_state_dir(name).join("state.json")
}

fn note_file(paths: &Paths, name: &str) -> PathBuf {
    paths.orchestrator_state_dir(name).join("note")
}

/// The orchestrator's state; a missing or unreadable file is a fresh one.
pub fn load_state(paths: &Paths, name: &str) -> State {
    let path = state_file(paths, name);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|err| {
            tracing::warn!(orchestrator = name, %err, "state unreadable; starting afresh");
            State::default()
        }),
        Err(_) => State::default(),
    }
}

fn save_state(paths: &Paths, name: &str, state: &State) {
    let result = (|| -> anyhow::Result<()> {
        let dir = paths.orchestrator_state_dir(name);
        crate::config::create_private_dir(&dir)?;
        let tmp = dir.join("state.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
        std::fs::rename(&tmp, state_file(paths, name))?;
        Ok(())
    })();
    if let Err(err) = result {
        tracing::error!(orchestrator = name, err = %format!("{err:#}"), "save orchestrator state");
    }
}

/// The orchestrator's handover note, if it has one.
pub fn read_note(paths: &Paths, name: &str) -> Option<String> {
    std::fs::read_to_string(note_file(paths, name))
        .ok()
        .filter(|n| !n.trim().is_empty())
}

/// Replace the orchestrator's handover note with `text`, trimmed and cut at
/// `NOTE_MAX` bytes; an empty text removes it. Answers what it did.
pub fn write_note(paths: &Paths, name: &str, text: &str) -> anyhow::Result<String> {
    let text = crate::template::strip_controls(text.trim());
    let path = note_file(paths, name);
    if text.is_empty() {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("remove {}", path.display())),
        }
        return Ok(format!("orchestrator {name}'s note removed"));
    }
    let mut end = text.len().min(NOTE_MAX);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    crate::config::create_private_dir(&paths.orchestrator_state_dir(name))?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &text[..end]).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(if end < text.len() {
        format!(
            "orchestrator {name}'s note kept, cut from {} to {NOTE_MAX} bytes",
            text.len()
        )
    } else {
        format!("orchestrator {name}'s note kept")
    })
}

/// Whether a task still counts as its orchestrator's agent at work: a run
/// is skipped while it does, and it holds a `max_orchestrators` slot.
pub fn works(task: &Task) -> bool {
    matches!(
        task.state,
        TaskState::Queued
            | TaskState::Starting
            | TaskState::Running
            | TaskState::Blocked
            | TaskState::Paused
    )
}

/// Orchestrator agents at work now, of both kinds and hand-started ones
/// too: what `max_orchestrators` counts.
pub fn working_orchestrators(store: &Store) -> anyhow::Result<usize> {
    let tasks = store.list_tasks(&TaskFilter {
        states: Some(vec![
            TaskState::Queued,
            TaskState::Starting,
            TaskState::Running,
            TaskState::Blocked,
            TaskState::Paused,
        ]),
        ..Default::default()
    })?;
    Ok(tasks
        .iter()
        .filter(|t| t.role == TaskRole::Orchestrator)
        .count())
}

/// When a quota lets an agent start again, if `text` (a task's error and
/// the last lines of its pane) says its agent stopped on one: the reset time
/// the message names (`|<unix time>`, or `resets 3am`, `resets at 15:30`,
/// the next such time of day in local time), else `QUOTA_WAIT` from `now`.
pub fn quota_reset(text: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    const MARKS: [&str; 5] = [
        "usage limit",
        "limit reached",
        "hit your limit",
        "quota exceeded",
        "exceeded your current quota",
    ];
    let lower = text.to_lowercase();
    let at = MARKS.iter().filter_map(|m| lower.rfind(m)).max()?;
    let rest = &lower[at..];
    let line = rest.lines().next().unwrap_or("");
    if let Some((_, epoch)) = line.split_once('|') {
        let digits: String = epoch.chars().take_while(char::is_ascii_digit).collect();
        if let Ok(secs) = digits.parse::<i64>()
            && let Some(t) = Utc.timestamp_opt(secs, 0).single()
            && t > now
        {
            return Some(t);
        }
    }
    if let Some(i) = line.find("reset")
        && let Some(time) = clock_after(&line[i + "reset".len()..])
    {
        let local = now.with_timezone(&Local);
        let mut day = local.date_naive();
        for _ in 0..3 {
            if let Some(t) = Local
                .from_local_datetime(&day.and_time(time))
                .earliest()
                .map(|t| t.with_timezone(&Utc))
                && t > now
            {
                return Some(t);
            }
            day = day.succ_opt()?;
        }
    }
    Some(now + chrono::Duration::from_std(QUOTA_WAIT).expect("an hour fits"))
}

/// The time of day at the start of `s` (after `reset`): `s at 3am`,
/// ` 3:30 pm`, ` at 15:00`.
fn clock_after(s: &str) -> Option<NaiveTime> {
    let s = s.strip_prefix('s').unwrap_or(s).trim_start();
    let s = s.strip_prefix("at ").unwrap_or(s).trim_start();
    let hour: String = s.chars().take_while(char::is_ascii_digit).collect();
    if hour.is_empty() || hour.len() > 2 {
        return None;
    }
    let mut rest = &s[hour.len()..];
    let mut minute = 0;
    if let Some(m) = rest.strip_prefix(':') {
        let digits: String = m.chars().take_while(char::is_ascii_digit).collect();
        if digits.len() != 2 {
            return None;
        }
        minute = digits.parse().ok()?;
        rest = &m[2..];
    }
    let mut hour: u32 = hour.parse().ok()?;
    let rest = rest.trim_start().replace('.', "");
    if rest.starts_with("am") || rest.starts_with("pm") {
        if !(1..=12).contains(&hour) {
            return None;
        }
        hour %= 12;
        if rest.starts_with("pm") {
            hour += 12;
        }
    } else if !s[..].contains(':') {
        // `resets 3` alone is too thin to read as a time.
        return None;
    }
    NaiveTime::from_hms_opt(hour, minute, 0)
}

/// One orchestrator as `orchestrator list` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorStatus {
    pub name: String,
    /// `None` when the file never parsed.
    pub kind: Option<Kind>,
    /// `idle`, `running`, `waiting for quota`, `off` or `invalid`.
    pub state: String,
    pub enabled: bool,
    /// `every 5m`, `cron ...`, or a session's `hours 22:00-08:00`.
    pub schedule: Option<String>,
    /// The current file's problem; with `kind` set, the last good version
    /// is what runs.
    pub error: Option<String>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_result: Option<String>,
    pub next_run: Option<DateTime<Utc>>,
    /// Its last agent's task.
    pub task: Option<i64>,
    pub quota_until: Option<DateTime<Utc>>,
    pub description: Option<String>,
}

/// `orchestrator describe`: the status, the file, the note, the last runs
/// with their lines, and recent events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestratorDescription {
    #[serde(flatten)]
    pub status: OrchestratorStatus,
    pub file: PathBuf,
    pub model: Option<String>,
    pub skill: Option<String>,
    pub prompt: Option<String>,
    pub repo: Option<String>,
    #[serde(default)]
    pub pre: Vec<String>,
    pub post: Option<Vec<String>>,
    pub timeout: Option<String>,
    pub hours: Option<Hours>,
    pub note: Option<String>,
    #[serde(default)]
    pub runs: Vec<RunRecord>,
    #[serde(default)]
    pub events: Vec<crate::events::EventRecord>,
}

/// The status of orchestrator `name`: `orch` its last good version (none if
/// its file never parsed), `error` the current file's problem.
fn status_of(
    paths: &Paths,
    store: Option<&Store>,
    name: &str,
    orch: Option<&Orchestrator>,
    error: Option<String>,
    first_seen: DateTime<Utc>,
    now: DateTime<Utc>,
) -> OrchestratorStatus {
    let state = load_state(paths, name);
    let at_work = state
        .task
        .zip(store)
        .and_then(|(id, s)| s.get_task(id).ok().flatten())
        .is_some_and(|t| works(&t));
    let quota = state.quota_until.filter(|u| *u > now);
    let label = match orch {
        None => "invalid",
        Some(o) if !o.enabled => "off",
        Some(_) if quota.is_some() => "waiting for quota",
        Some(_) if at_work => "running",
        Some(_) => "idle",
    };
    OrchestratorStatus {
        name: name.to_string(),
        kind: orch.map(Orchestrator::kind),
        state: label.into(),
        enabled: orch.is_some_and(|o| o.enabled),
        schedule: orch.map(|o| match &o.plan {
            Plan::Scheduled(s) => s.schedule.describe(),
            Plan::Session(s) => format!("hours {}-{}", s.hours.start, s.hours.stop),
        }),
        error,
        last_run_at: state.last_run_at,
        last_result: state.last_result.clone(),
        next_run: orch.and_then(|o| o.next_run(&state, first_seen, now)),
        task: state.task,
        quota_until: quota,
        description: orch.and_then(|o| o.description.clone()),
    }
}

fn describe_of(
    paths: &Paths,
    status: OrchestratorStatus,
    orch: Option<&Orchestrator>,
) -> OrchestratorDescription {
    let state = load_state(paths, &status.name);
    let name = status.name.clone();
    let events = crate::events::read(&paths.events_file(), None)
        .unwrap_or_default()
        .into_iter()
        .filter(|r| {
            r.detail
                .as_ref()
                .and_then(|d| d.get("orchestrator"))
                .and_then(|o| o.as_str())
                == Some(name.as_str())
        })
        .collect::<Vec<_>>();
    let skip = events.len().saturating_sub(EVENTS_SHOWN);
    let (pre, post, timeout, hours) = match orch.map(|o| &o.plan) {
        Some(Plan::Scheduled(s)) => (
            s.pre.clone(),
            s.post.clone(),
            Some(crate::schedule::describe_duration(s.timeout)),
            None,
        ),
        Some(Plan::Session(s)) => (Vec::new(), None, None, Some(s.hours.clone())),
        None => (Vec::new(), None, None, None),
    };
    OrchestratorDescription {
        file: paths.orchestrators_dir().join(format!("{name}.toml")),
        model: orch.and_then(|o| o.model.clone()),
        skill: orch.and_then(|o| o.skill.clone()),
        prompt: orch.map(|o| o.prompt.clone()),
        repo: orch.and_then(|o| o.repo.clone()),
        pre,
        post,
        timeout,
        hours,
        note: read_note(paths, &name),
        runs: state.runs,
        events: events.into_iter().skip(skip).collect(),
        status,
    }
}

/// `orchestrator list` with no head: the files as they read now (no last
/// good version to fall back on), and the state the head left.
pub fn offline_statuses(
    paths: &Paths,
    store: Option<&Store>,
) -> anyhow::Result<Vec<OrchestratorStatus>> {
    let now = Utc::now();
    Ok(load_dir(&paths.orchestrators_dir())?
        .into_iter()
        .map(|l| match l {
            Loaded::Valid(o) => status_of(paths, store, &o.name, Some(&o), None, now, now),
            Loaded::Invalid { name, error } => {
                status_of(paths, store, &name, None, Some(error), now, now)
            }
        })
        .collect())
}

/// `orchestrator describe` with no head.
pub fn offline_describe(
    paths: &Paths,
    store: Option<&Store>,
    name: &str,
) -> Result<OrchestratorDescription, (String, String)> {
    let path = file_of(paths, name)?;
    let now = Utc::now();
    let (orch, error) = match load_file(&path, name) {
        Loaded::Valid(o) => (Some(*o), None),
        Loaded::Invalid { error, .. } => (None, Some(error)),
    };
    let status = status_of(paths, store, name, orch.as_ref(), error, now, now);
    Ok(describe_of(paths, status, orch.as_ref()))
}

/// One file as the runner holds it: its last good version, and the current
/// file's error if it stopped parsing.
#[derive(Debug, Clone)]
struct Entry {
    orch: Option<Orchestrator>,
    error: Option<String>,
    first_seen: DateTime<Utc>,
}

/// The head's orchestrators: it re-reads their files on each pass, runs the
/// scheduled ones that are due, and runs a post script once its agent has
/// ended. Runs of one orchestrator, and its post scripts, go one at a time.
pub struct Runner {
    paths: Paths,
    store: Arc<Store>,
    fleet: Arc<Fleet>,
    events: broadcast::Sender<PastorEvent>,
    entries: std::sync::Mutex<BTreeMap<String, Entry>>,
    locks: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Runner {
    pub fn new(
        paths: Paths,
        store: Arc<Store>,
        fleet: Arc<Fleet>,
        events: broadcast::Sender<PastorEvent>,
    ) -> Arc<Runner> {
        Arc::new(Runner {
            paths,
            store,
            fleet,
            events,
            entries: Default::default(),
            locks: Default::default(),
        })
    }

    /// Pass every `tick`, until the head stops.
    pub fn spawn(self: &Arc<Self>, tick: Duration) {
        let runner = self.clone();
        tokio::spawn(async move {
            let mut every = tokio::time::interval(tick);
            every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                every.tick().await;
                runner.pass(Utc::now());
            }
        });
    }

    /// Re-read the files. One that stopped parsing keeps its last good
    /// version, with the error beside it; a removed file drops it.
    pub fn reload(&self, now: DateTime<Utc>) {
        let loaded = match load_dir(&self.paths.orchestrators_dir()) {
            Ok(l) => l,
            Err(err) => {
                tracing::warn!(err = %format!("{err:#}"), "read orchestrators; keeping the last ones");
                return;
            }
        };
        let mut entries = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        let mut next = BTreeMap::new();
        for l in loaded {
            let name = l.name().to_string();
            let old = entries.remove(&name);
            let first_seen = old.as_ref().map_or(now, |e| e.first_seen);
            let entry = match l {
                Loaded::Valid(o) => Entry {
                    orch: Some(*o),
                    error: None,
                    first_seen,
                },
                Loaded::Invalid { name, error } => {
                    if old
                        .as_ref()
                        .is_none_or(|e| e.error.as_ref() != Some(&error))
                    {
                        tracing::warn!(orchestrator = %name, %error, "orchestrator file invalid");
                    }
                    Entry {
                        orch: old.and_then(|e| e.orch),
                        error: Some(error),
                        first_seen,
                    }
                }
            };
            next.insert(name, entry);
        }
        *entries = next;
    }

    fn entry(&self, name: &str) -> Option<Entry> {
        self.entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(name)
            .cloned()
    }

    /// Whether the head has a file for orchestrator `name`, valid or not:
    /// what `PASTOR_ORCHESTRATOR` must name for a script's request to pass.
    pub fn knows(&self, name: &str) -> bool {
        self.entry(name).is_some() || file_of(&self.paths, name).is_ok()
    }

    fn lock_of(&self, name: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(name.to_string())
            .or_default()
            .clone()
    }

    pub fn statuses(&self, now: DateTime<Utc>) -> Vec<OrchestratorStatus> {
        self.reload(now);
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        entries
            .iter()
            .map(|(name, e)| {
                status_of(
                    &self.paths,
                    Some(&self.store),
                    name,
                    e.orch.as_ref(),
                    e.error.clone(),
                    e.first_seen,
                    now,
                )
            })
            .collect()
    }

    pub fn describe(
        &self,
        name: &str,
        now: DateTime<Utc>,
    ) -> Result<OrchestratorDescription, (String, String)> {
        self.reload(now);
        let Some(e) = self.entry(name) else {
            return Err(file_of(&self.paths, name).err().unwrap_or((
                "orchestrator_not_found".into(),
                format!("no orchestrator named {name:?}"),
            )));
        };
        let status = status_of(
            &self.paths,
            Some(&self.store),
            name,
            e.orch.as_ref(),
            e.error.clone(),
            e.first_seen,
            now,
        );
        Ok(describe_of(&self.paths, status, e.orch.as_ref()))
    }

    /// The orchestrator whose last agent is task `id`: the one whose note
    /// that agent keeps.
    pub fn of_task(&self, id: i64) -> Option<String> {
        let names: Vec<String> = self
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .cloned()
            .collect();
        names
            .into_iter()
            .find(|n| load_state(&self.paths, n).task == Some(id))
    }

    /// One tick: re-read the files, run the post script of each agent that
    /// has ended, and start each run that is due. Both are spawned, so a
    /// slow script never holds the head; an orchestrator busy with either
    /// waits for the next tick.
    pub fn pass(self: &Arc<Self>, now: DateTime<Utc>) {
        self.reload(now);
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        for (name, e) in entries {
            let Some(orch) = e.orch else { continue };
            let state = load_state(&self.paths, &name);
            let ended = state.post_pending && !self.agent_works(&state);
            let due = orch
                .next_run(&state, e.first_seen, now)
                .is_some_and(|t| t <= now);
            if !(ended || due) {
                continue;
            }
            let Ok(guard) = self.lock_of(&name).try_lock_owned() else {
                continue;
            };
            let runner = self.clone();
            tokio::spawn(async move {
                if ended {
                    runner.finish(&orch, now).await;
                }
                if due && matches!(orch.plan, Plan::Scheduled(_)) {
                    runner.run(&orch, now).await;
                }
                drop(guard);
            });
        }
    }

    /// `pastor orchestrator run`: a run of `name` now, whatever its schedule
    /// and `enabled`, after a run or post script of it already going. The
    /// busy rule still holds: while its last agent works, the run is
    /// skipped.
    pub fn fire(self: &Arc<Self>, name: &str) -> Result<String, (String, String)> {
        let now = Utc::now();
        self.reload(now);
        let orch = self.runnable(name)?;
        let runner = self.clone();
        tokio::spawn(async move {
            let _ = runner.run_now(&orch).await;
        });
        Ok(format!(
            "started a run of orchestrator {name}; `pastor orchestrator describe {name}` shows how it went"
        ))
    }

    fn runnable(&self, name: &str) -> Result<Orchestrator, (String, String)> {
        let entry = self.entry(name).ok_or_else(|| {
            file_of(&self.paths, name).err().unwrap_or((
                "orchestrator_not_found".into(),
                format!("no orchestrator named {name:?}"),
            ))
        })?;
        let orch = entry.orch.ok_or_else(|| {
            (
                "orchestrator_invalid".to_string(),
                format!(
                    "orchestrator {name}'s file has never been valid: {}",
                    entry.error.unwrap_or_default()
                ),
            )
        })?;
        if orch.kind() == Kind::Session {
            return Err((
                "orchestrator_kind".into(),
                format!(
                    "{name} is a session orchestrator; this version checks session files but does not run them"
                ),
            ));
        }
        Ok(orch)
    }

    /// One run of `orch` now, waiting for one already going, and its record:
    /// the post script of an agent that ended first, as a pass would.
    pub async fn run_now(&self, orch: &Orchestrator) -> RunRecord {
        let lock = self.lock_of(&orch.name);
        let _turn = lock.lock().await;
        let now = Utc::now();
        let state = load_state(&self.paths, &orch.name);
        if state.post_pending && !self.agent_works(&state) {
            self.finish(orch, now).await;
        }
        self.run(orch, now).await
    }

    /// Run the post script of `orch`'s ended agent now, if it is due, as a
    /// pass would; for tests and callers that wait on it.
    pub async fn finish_now(&self, orch: &Orchestrator) {
        let lock = self.lock_of(&orch.name);
        let _turn = lock.lock().await;
        let state = load_state(&self.paths, &orch.name);
        if state.post_pending && !self.agent_works(&state) {
            self.finish(orch, Utc::now()).await;
        }
    }

    fn agent_works(&self, state: &State) -> bool {
        state
            .task
            .and_then(|id| self.store.get_task(id).ok().flatten())
            .is_some_and(|t| works(&t))
    }

    fn emit(&self, kind: &str, name: &str, task: Option<i64>, extra: serde_json::Value) {
        let mut detail = serde_json::json!({ "orchestrator": name });
        if let (Some(d), serde_json::Value::Object(extra)) = (detail.as_object_mut(), extra) {
            d.extend(extra);
        }
        let _ = self.events.send(PastorEvent {
            kind: format!("orchestrator.{kind}"),
            task_id: task,
            machine: None,
            job: None,
            detail: Some(detail),
            summary: None,
        });
    }

    /// One scheduled run, under the orchestrator's lock: skip if its last
    /// agent still works, run the pre script, and with lines start one agent
    /// unless `max_orchestrators` or a quota wait holds it back.
    async fn run(&self, orch: &Orchestrator, now: DateTime<Utc>) -> RunRecord {
        let name = orch.name.as_str();
        let Plan::Scheduled(sched) = &orch.plan else {
            unreachable!("only a scheduled orchestrator runs");
        };
        let mut state = load_state(&self.paths, name);
        state.last_run_at = Some(now);
        let mut run = RunRecord {
            at: now,
            outcome: RunOutcome::NoLines,
            detail: None,
            lines: Vec::new(),
            task: None,
            log: None,
        };
        if let Some(t) = state
            .task
            .and_then(|id| self.store.get_task(id).ok().flatten())
            .filter(works)
        {
            tracing::info!(orchestrator = name, task = %t.display_id(), "run skipped: its agent still works");
            run.outcome = RunOutcome::Skipped;
            run.detail = Some(format!("{} still works", t.display_id()));
            self.emit(
                "skipped",
                name,
                Some(t.id),
                serde_json::json!({"reason": "busy"}),
            );
            return self.save_run(name, state, run);
        }
        let pre = self
            .script(orch, "pre", &sched.pre, sched.timeout, Vec::new())
            .await;
        run.log = pre.log.clone();
        let lines = match pre.result {
            Ok(lines) => lines,
            Err(reason) => {
                state.failures += 1;
                let wait =
                    chrono::Duration::from_std(crate::scheduler::backoff_for(state.failures))
                        .unwrap_or_else(|_| chrono::Duration::zero());
                state.backoff_until = Some(now + wait);
                tracing::warn!(orchestrator = name, %reason, failures = state.failures, "pre script failed; backing off");
                run.outcome = RunOutcome::Failed;
                run.detail = Some(format!("pre ({}x): {reason}", state.failures));
                self.emit(
                    "failed",
                    name,
                    None,
                    serde_json::json!({"stage": "pre", "error": reason, "failures": state.failures}),
                );
                return self.save_run(name, state, run);
            }
        };
        state.failures = 0;
        state.backoff_until = None;
        if lines.is_empty() {
            return self.save_run(name, state, run);
        }
        run.lines = lines.clone();
        if let Some(until) = state.quota_until.filter(|u| *u > now) {
            run.outcome = RunOutcome::Held;
            run.detail = Some(format!(
                "waiting for the quota until {}",
                until.to_rfc3339()
            ));
            self.emit(
                "held",
                name,
                None,
                serde_json::json!({"reason": "quota", "until": until}),
            );
            return self.save_run(name, state, run);
        }
        let max = self.fleet.max_orchestrators() as usize;
        match working_orchestrators(&self.store) {
            Ok(n) if n >= max => {
                run.outcome = RunOutcome::Held;
                run.detail = Some(format!(
                    "max_orchestrators = {max}, and {n} orchestrator agent{} already work",
                    if n == 1 { "" } else { "s" }
                ));
                self.emit(
                    "held",
                    name,
                    None,
                    serde_json::json!({"reason": "max_orchestrators", "max": max}),
                );
                return self.save_run(name, state, run);
            }
            Ok(_) => {}
            Err(err) => {
                run.outcome = RunOutcome::Failed;
                run.detail = Some(format!("agent: count orchestrators: {err:#}"));
                self.emit(
                    "failed",
                    name,
                    None,
                    serde_json::json!({"stage": "agent", "error": format!("{err:#}")}),
                );
                return self.save_run(name, state, run);
            }
        }
        match self.start_agent(orch, &lines).await {
            Ok(task) => {
                tracing::info!(orchestrator = name, task = %task.display_id(), lines = lines.len(), "orchestrator agent queued");
                run.outcome = RunOutcome::Started;
                run.task = Some(task.id);
                state.task = Some(task.id);
                state.post_pending = true;
                state.lines = lines;
                self.emit(
                    "started",
                    name,
                    Some(task.id),
                    serde_json::json!({"lines": run.lines.len()}),
                );
            }
            Err(reason) => {
                tracing::warn!(orchestrator = name, %reason, "orchestrator agent not queued");
                run.outcome = RunOutcome::Failed;
                run.detail = Some(format!("agent: {reason}"));
                self.emit(
                    "failed",
                    name,
                    None,
                    serde_json::json!({"stage": "agent", "error": reason}),
                );
            }
        }
        self.save_run(name, state, run)
    }

    fn save_run(&self, name: &str, mut state: State, run: RunRecord) -> RunRecord {
        state.record(run.clone());
        save_state(&self.paths, name, &state);
        run
    }

    /// Queue the run's agent on the head's own machine, with the role, the
    /// file's model and repo, and the prompt with every line.
    async fn start_agent(&self, orch: &Orchestrator, lines: &[String]) -> Result<Task, String> {
        let flock = self.fleet.flock();
        let head = flock
            .machines
            .iter()
            .find(|m| m.local)
            .map(|m| m.name.clone())
            .ok_or("no machine in flock.toml is the head's own (local = true), and an orchestrator's agent runs only there")?;
        let defaults = self.fleet.defaults();
        let ask = AgentChoice {
            model: orch.model.clone(),
            ..Default::default()
        };
        let pick = defaults.resolve_agent(&ask, None);
        let timeout = parse_duration(&defaults.timeout)
            .unwrap_or(Duration::from_secs(2 * 60 * 60))
            .as_secs();
        let spec = DispatchSpec {
            agent: pick.agent,
            agent_args: pick.agent_args,
            allow: pick.allow,
            deny: pick.deny,
            repo: orch.repo.clone(),
            worktree: orch.repo.is_some(),
            branch: None,
            machine: Some(head),
            tags: Vec::new(),
            timeout_secs: timeout,
            checkout: None,
            reopen: None,
            agent_source: None,
            place: defaults.place.clone(),
            session_id: None,
        };
        let note = read_note(&self.paths, &orch.name);
        let prompt = orch.agent_prompt(note.as_deref(), lines);
        let description = format!(
            "orchestrator {}: {} line{}",
            orch.name,
            lines.len(),
            if lines.len() == 1 { "" } else { "s" }
        );
        let task = self
            .fleet
            .queue_run_as(
                prompt,
                spec,
                None,
                Some(&ask),
                None,
                TaskRole::Orchestrator,
                Some(description),
                false,
            )
            .await
            .map_err(|e| match e {
                QueueError::UnknownMachine(m) => format!("machine {m} is not in the flock"),
                QueueError::Flock(e) => e.to_string(),
                QueueError::Agent(e) => format!("{}: {}", e.code, e.message),
                QueueError::Store(e) => format!("{e:#}"),
            })?;
        let _ = self.events.send(PastorEvent {
            kind: "task.queued".into(),
            task_id: Some(task.id),
            machine: None,
            job: Some(task.job.clone()),
            detail: None,
            summary: None,
        });
        let fleet = self.fleet.clone();
        tokio::spawn(async move { fleet.dispatch_queued().await });
        Ok(task)
    }

    /// The agent `orch`'s last run started has ended: note a quota error,
    /// then run the post script once, with the end state, the summary and
    /// the lines. A task closed before pastor saw it end runs nothing.
    async fn finish(&self, orch: &Orchestrator, now: DateTime<Utc>) {
        let name = orch.name.as_str();
        let mut state = load_state(&self.paths, name);
        let Some(id) = state.task.filter(|_| state.post_pending) else {
            return;
        };
        let task = match self.store.get_task(id) {
            Ok(Some(t)) => t,
            Ok(None) => {
                state.post_pending = false;
                save_state(&self.paths, name, &state);
                return;
            }
            Err(err) => {
                tracing::warn!(orchestrator = name, %err, "read its agent's task; post script waits");
                return;
            }
        };
        let end = match task.state {
            TaskState::Done => "done",
            TaskState::Failed => "failed",
            TaskState::Stale => "stale",
            TaskState::Closed => {
                tracing::info!(orchestrator = name, task = %task.display_id(), "agent closed before it ended; no post script");
                state.post_pending = false;
                save_state(&self.paths, name, &state);
                return;
            }
            _ => return,
        };
        let last_output = self.store.pane_tail(id).unwrap_or_default();
        let said = format!("{}\n{last_output}", task.error.as_deref().unwrap_or(""));
        if let Some(until) = quota_reset(&said, now) {
            tracing::warn!(orchestrator = name, task = %task.display_id(), until = %until, "agent stopped on a quota; no agent before the reset");
            state.quota_until = Some(until);
            self.emit(
                "quota",
                name,
                Some(task.id),
                serde_json::json!({"until": until}),
            );
        }
        if let Plan::Scheduled(sched) = &orch.plan
            && let Some(post) = &sched.post
        {
            let mut input =
                crate::hooks::task_end_object(&task, end, &last_output, task.summary.as_ref());
            if let Some(obj) = input.as_object_mut() {
                obj.insert("orchestrator".into(), name.into());
                obj.insert("lines".into(), state.lines.clone().into());
            }
            let mut stdin = serde_json::to_vec(&input).expect("a task-end object serializes");
            stdin.push(b'\n');
            let ran = self.script(orch, "post", post, sched.timeout, stdin).await;
            if let Err(reason) = ran.result {
                tracing::warn!(orchestrator = name, task = %task.display_id(), %reason, "post script failed; not retried");
                self.emit(
                    "failed",
                    name,
                    Some(task.id),
                    serde_json::json!({"stage": "post", "error": reason}),
                );
            }
        }
        state.post_pending = false;
        save_state(&self.paths, name, &state);
    }

    /// Run one of `orch`'s scripts in its file's directory, with its `.env`
    /// and pastor's variables, `stdin`, bounded by `timeout`. Its stderr and
    /// stdout go to a run log, redacted of every `.env` value; the result
    /// is its stdout lines, blank ones and control characters (tab aside)
    /// dropped, or why it failed.
    async fn script(
        &self,
        orch: &Orchestrator,
        stage: &str,
        argv: &[String],
        timeout: Duration,
        stdin: Vec<u8>,
    ) -> ScriptRun {
        match self.try_script(orch, stage, argv, timeout, stdin).await {
            Ok(ran) => ran,
            Err(err) => ScriptRun {
                result: Err(format!("not run: {err:#}")),
                log: None,
            },
        }
    }

    async fn try_script(
        &self,
        orch: &Orchestrator,
        stage: &str,
        argv: &[String],
        timeout: Duration,
        stdin: Vec<u8>,
    ) -> anyhow::Result<ScriptRun> {
        let dotenv_file = orch.dir.join(".env");
        let dotenv = if dotenv_file.exists() {
            crate::connector::env::load(&dotenv_file)?
        } else {
            Vec::new()
        };
        let redactor = Redactor::new(dotenv.iter().map(|(k, _)| k.as_str()), &dotenv);
        let state_dir = self.paths.orchestrator_state_dir(&orch.name);
        let scratch = state_dir.join("scratch");
        crate::config::create_private_dir(&scratch)?;
        let dir = |p: &Path| p.to_string_lossy().into_owned();
        let mut env = dotenv;
        env.push((ORCHESTRATOR_ENV.into(), orch.name.clone()));
        env.push((SCRATCH_ENV.into(), dir(&scratch)));
        env.push(("PASTOR_CONFIG_DIR".into(), dir(&self.paths.config_dir)));
        env.push(("PASTOR_STATE_DIR".into(), dir(&self.paths.state_dir)));
        // A head started from an agent's pane must not lend the script that
        // agent's rights: `PASTOR_TASK` wins over `PASTOR_ORCHESTRATOR`.
        env.push((crate::ipc::TASK_ENV.into(), String::new()));
        // The `pastor` the scripts call is this one.
        if let Ok(exe) = std::env::current_exe()
            && let Some(bin) = exe.parent()
        {
            let path = std::env::var("PATH").unwrap_or_default();
            env.push(("PATH".into(), format!("{}:{path}", bin.display())));
        }
        let log = RunLog::create(&state_dir.join("runs"), redactor)?;
        let path = log.path().to_path_buf();
        let log = log.shared();
        log.lock()
            .unwrap_or_else(|p| p.into_inner())
            .line(&format!("{stage} (orchestrator {})", orch.name));
        let inv = Invocation {
            argv: argv.to_vec(),
            cwd: orch.dir.clone(),
            env,
            stdin,
            timeout: Some(timeout),
        };
        let mut lines = Vec::new();
        let out_log = log.clone();
        let done = exec::run(inv, log.clone(), |line| {
            let mut l = out_log.lock().unwrap_or_else(|p| p.into_inner());
            l.line(&format!("stdout: {line}"));
            let clean = crate::template::strip_controls(&l.redact(line)).replace('\n', "");
            if !clean.trim().is_empty() {
                lines.push(clean);
            }
        })
        .await;
        Ok(ScriptRun {
            result: if done.exit.success() {
                Ok(lines)
            } else {
                Err(done.reason())
            },
            log: Some(path),
        })
    }
}

struct ScriptRun {
    result: Result<Vec<String>, String>,
    log: Option<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::flock::Flock;
    use std::os::unix::fs::PermissionsExt;

    const SCHEDULED: &str = r#"
kind = "scheduled"
every = "5m"
pre = ["./pre.sh"]
post = ["./post.sh"]
model = "sonnet"
skill = "orchestrating-pastor"
prompt = "Decide what to do with each line below."
"#;

    /// `SCHEDULED` without its model, which these tests' `[models]` lack.
    fn scheduled() -> String {
        SCHEDULED.replace("model = \"sonnet\"\n", "")
    }

    const SESSION: &str = r#"
kind = "session"
hours = { start = "22:00", stop = "08:00" }
prompt = "You are the night orchestrator."
"#;

    fn parse(text: &str) -> Result<Orchestrator, String> {
        Orchestrator::parse(text, "merge", Path::new("/o"))
    }

    #[test]
    fn a_file_names_its_kind_and_takes_that_kinds_keys() {
        let o = parse(SCHEDULED).unwrap();
        assert_eq!(o.kind(), Kind::Scheduled);
        let Plan::Scheduled(s) = &o.plan else {
            panic!()
        };
        assert_eq!(s.pre, ["./pre.sh"]);
        assert_eq!(s.timeout, DEFAULT_SCRIPT_TIMEOUT);
        assert!(o.enabled);
        let o = parse(SESSION).unwrap();
        assert_eq!(o.kind(), Kind::Session);
        let Plan::Session(s) = &o.plan else { panic!() };
        assert_eq!(s.stop_grace, DEFAULT_STOP_GRACE);
    }

    #[test]
    fn a_file_without_kind_or_with_the_other_kinds_key_is_invalid() {
        let err = parse("every = \"5m\"\npre = [\"./p\"]\nprompt = \"p\"\n").unwrap_err();
        assert!(err.contains("kind is required"), "{err}");
        let err = parse(&SCHEDULED.replace("\"scheduled\"", "\"nightly\"")).unwrap_err();
        assert!(err.contains("unknown kind \"nightly\""), "{err}");
        let err = parse(&format!("{SESSION}pre = [\"./p\"]\n")).unwrap_err();
        assert!(err.contains("pre") && err.contains("\"session\""), "{err}");
        let err = parse(&format!(
            "{SCHEDULED}hours = {{ start = \"22:00\", stop = \"08:00\" }}\n"
        ))
        .unwrap_err();
        assert!(
            err.contains("hours") && err.contains("\"scheduled\""),
            "{err}"
        );
        let err = parse(&format!("{SCHEDULED}machine = \"pi-1\"\n")).unwrap_err();
        assert!(err.contains("machine"), "{err}");
        let err = parse(&SCHEDULED.replace("pre = [\"./pre.sh\"]", "")).unwrap_err();
        assert!(err.contains("pre is required"), "{err}");
        let err = parse(&SCHEDULED.replace("every = \"5m\"", "")).unwrap_err();
        assert!(err.contains("every or cron"), "{err}");
        let err = parse(&SESSION.replace("08:00", "8am")).unwrap_err();
        assert!(err.contains("hours.stop"), "{err}");
        assert!(Orchestrator::parse(SCHEDULED, "../x", Path::new("/o")).is_err());
    }

    #[test]
    fn the_agents_prompt_is_the_prompt_skill_note_lines_and_summary_ask() {
        let o = parse(SCHEDULED).unwrap();
        let p = o.agent_prompt(
            Some("merged #31\n"),
            &["PR #32 x".into(), "TASK t-4 blocked".into()],
        );
        let prompt = p.find("Decide what").unwrap();
        let skill = p.find("Use your orchestrating-pastor skill.").unwrap();
        let note = p.find("merged #31").unwrap();
        let line = p.find("PR #32 x\nTASK t-4 blocked").unwrap();
        let ask = p.find("pastor task done --summary-file -").unwrap();
        assert!(
            prompt < skill && skill < note && note < line && line < ask,
            "{p}"
        );
        assert!(!o.agent_prompt(None, &[]).contains("handover"));
    }

    #[test]
    fn a_quota_message_names_its_reset_or_waits_an_hour() {
        let now = Utc.with_ymd_and_hms(2026, 9, 28, 12, 0, 0).unwrap();
        assert_eq!(quota_reset("all good, merged #31", now), None);
        assert_eq!(
            quota_reset("Claude AI usage limit reached|1800000000", now),
            Utc.timestamp_opt(1_800_000_000, 0).single()
        );
        let hour = quota_reset("You've hit your limit · resets 3am (Europe/Lisbon)", now).unwrap();
        let local = hour.with_timezone(&Local);
        assert_eq!(
            (
                chrono::Timelike::hour(&local),
                chrono::Timelike::minute(&local)
            ),
            (3, 0)
        );
        assert!(hour > now && hour - now <= chrono::Duration::hours(24));
        let at = quota_reset("5-hour limit reached ∙ resets at 15:30", now).unwrap();
        let local = at.with_timezone(&Local);
        assert_eq!(
            (
                chrono::Timelike::hour(&local),
                chrono::Timelike::minute(&local)
            ),
            (15, 30)
        );
        assert_eq!(
            quota_reset("quota exceeded, try later", now),
            Some(now + chrono::Duration::hours(1))
        );
    }

    #[test]
    fn a_note_is_cut_at_its_cap_and_an_empty_one_removes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        write_note(&paths, "merge", "  merged #31  ").unwrap();
        assert_eq!(read_note(&paths, "merge").as_deref(), Some("merged #31"));
        let said = write_note(&paths, "merge", &"é".repeat(NOTE_MAX)).unwrap();
        assert!(said.contains("cut"), "{said}");
        assert!(read_note(&paths, "merge").unwrap().len() <= NOTE_MAX);
        write_note(&paths, "merge", " ").unwrap();
        assert_eq!(read_note(&paths, "merge"), None);
    }

    struct Head {
        _tmp: tempfile::TempDir,
        paths: Paths,
        store: Arc<Store>,
        runner: Arc<Runner>,
        events: broadcast::Receiver<PastorEvent>,
        dir: PathBuf,
    }

    impl Head {
        /// A head whose flock has its own machine, `head`, and an
        /// orchestrator `merge` of `text` with `pre.sh` and `post.sh`.
        fn new(text: &str, pre: &str, post: &str) -> Head {
            let tmp = tempfile::tempdir().unwrap();
            let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
            let dir = paths.orchestrators_dir();
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("merge.toml"), text).unwrap();
            for (name, body) in [("pre.sh", pre), ("post.sh", post)] {
                let path = dir.join(name);
                std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            let store = Arc::new(Store::open_in_memory().unwrap());
            let flock = Flock::parse(
                Path::new("flock.toml"),
                "[[machine]]\nname = \"head\"\nlocal = true\n\n[[machine]]\nname = \"pi-1\"\nssh = \"user@pi-1\"\n",
            )
            .unwrap();
            let fleet = Arc::new(Fleet::new(Vec::new(), store.clone()).with_flock(flock));
            let (tx, events) = broadcast::channel(64);
            let runner = Runner::new(paths.clone(), store.clone(), fleet, tx);
            runner.reload(Utc::now());
            Head {
                _tmp: tmp,
                paths,
                store,
                runner,
                events,
                dir,
            }
        }

        fn orch(&self) -> Orchestrator {
            self.runner.runnable("merge").unwrap()
        }

        async fn run(&self) -> RunRecord {
            self.runner.run_now(&self.orch()).await
        }

        fn state(&self) -> State {
            load_state(&self.paths, "merge")
        }

        fn kinds(&mut self) -> Vec<String> {
            let mut out = Vec::new();
            while let Ok(e) = self.events.try_recv() {
                if e.kind.starts_with("orchestrator.") {
                    assert_eq!(e.detail.as_ref().unwrap()["orchestrator"], "merge");
                    out.push(e.kind);
                }
            }
            out
        }

        fn tasks(&self) -> Vec<Task> {
            self.store.list_tasks(&TaskFilter::default()).unwrap()
        }

        fn set_state(&self, id: i64, state: TaskState) {
            let mut t = self.store.get_task(id).unwrap().unwrap();
            t.state = state;
            self.store.update_task(&mut t).unwrap();
        }
    }

    #[tokio::test]
    async fn a_run_with_no_lines_starts_no_agent() {
        let mut head = Head::new(&scheduled(), "echo checked >&2", "");
        let run = head.run().await;
        assert_eq!(run.outcome, RunOutcome::NoLines);
        assert!(head.tasks().is_empty());
        assert!(head.kinds().is_empty());
        let log = std::fs::read_to_string(run.log.unwrap()).unwrap();
        assert!(log.contains("checked"), "{log}");
        assert_eq!(head.state().last_result.as_deref(), Some("no lines"));
    }

    #[tokio::test]
    async fn lines_start_one_orchestrator_agent_on_the_heads_machine() {
        let pre = "printf 'PR #31 ci=failure\\n\\nTASK t-4 blocked\\033[1m\\n'\n\
                   echo \"$PASTOR_ORCHESTRATOR $PASTOR_TASK.\" > \"$PASTOR_ORCHESTRATOR_STATE_DIR/env\"";
        let mut head = Head::new(&scheduled(), pre, "");
        write_note(&head.paths, "merge", "merged #30").unwrap();
        let run = head.run().await;
        assert_eq!(run.outcome, RunOutcome::Started, "{run:?}");
        assert_eq!(run.lines, ["PR #31 ci=failure", "TASK t-4 blocked[1m"]);
        let tasks = head.tasks();
        let [t] = tasks.as_slice() else {
            panic!("{tasks:?}")
        };
        assert_eq!(Some(t.id), run.task);
        assert_eq!(t.role, TaskRole::Orchestrator);
        assert_eq!(t.spec.machine.as_deref(), Some("head"));
        assert!(
            t.prompt.contains("PR #31 ci=failure\nTASK t-4 blocked"),
            "{}",
            t.prompt
        );
        assert!(t.prompt.contains("merged #30"), "{}", t.prompt);
        assert_eq!(head.kinds(), ["orchestrator.started"]);
        let state = head.state();
        assert_eq!(state.task, Some(t.id));
        assert!(state.post_pending);
        let env = std::fs::read_to_string(
            head.paths
                .orchestrator_state_dir("merge")
                .join("scratch/env"),
        )
        .unwrap();
        assert_eq!(env.trim(), "merge .", "the script runs with no PASTOR_TASK");
    }

    #[tokio::test]
    async fn a_busy_run_is_skipped_with_its_pre_script() {
        let pre = "echo ran >> \"$PASTOR_ORCHESTRATOR_STATE_DIR/runs\"\necho 'PR #31'";
        let mut head = Head::new(&scheduled(), pre, "");
        assert_eq!(head.run().await.outcome, RunOutcome::Started);
        head.kinds();
        let run = head.run().await;
        assert_eq!(run.outcome, RunOutcome::Skipped);
        assert_eq!(head.kinds(), ["orchestrator.skipped"]);
        let runs = std::fs::read_to_string(
            head.paths
                .orchestrator_state_dir("merge")
                .join("scratch/runs"),
        )
        .unwrap();
        assert_eq!(runs.lines().count(), 1, "the pre script ran again");
        assert_eq!(head.tasks().len(), 1);
    }

    #[tokio::test]
    async fn a_failing_pre_script_backs_off_and_drops_its_lines() {
        let mut head = Head::new(&scheduled(), "echo 'PR #31'\necho broken >&2\nexit 3", "");
        let before = Utc::now();
        let run = head.run().await;
        assert_eq!(run.outcome, RunOutcome::Failed);
        assert!(run.lines.is_empty());
        assert!(
            run.detail.as_deref().unwrap().contains("exit 3: broken"),
            "{run:?}"
        );
        assert!(head.tasks().is_empty());
        assert_eq!(head.kinds(), ["orchestrator.failed"]);
        let state = head.state();
        assert_eq!(state.failures, 1);
        let until = state.backoff_until.unwrap();
        assert!(until >= before + chrono::Duration::seconds(60), "{until}");
        let orch = head.orch();
        assert_eq!(orch.next_run(&state, before, Utc::now()), Some(until));
        head.run().await;
        assert_eq!(head.state().failures, 2);
        let wait = head.state().backoff_until.unwrap() - Utc::now();
        assert!(wait > chrono::Duration::seconds(100), "{wait}");
    }

    #[tokio::test]
    async fn the_post_script_gets_stale_the_summary_and_the_lines() {
        let post = "cat > \"$PASTOR_ORCHESTRATOR_STATE_DIR/post.json\"";
        let mut head = Head::new(&scheduled(), "echo 'PR #31'", post);
        let id = head.run().await.task.unwrap();
        // Still at work: nothing to do yet.
        head.runner.finish_now(&head.orch()).await;
        assert!(head.state().post_pending);
        head.set_state(id, TaskState::Stale);
        head.runner.finish_now(&head.orch()).await;
        let got: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(
                head.paths
                    .orchestrator_state_dir("merge")
                    .join("scratch/post.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(got["state"], "stale");
        assert_eq!(got["orchestrator"], "merge");
        assert_eq!(got["lines"], serde_json::json!(["PR #31"]));
        assert_eq!(got["task"]["id"], id);
        assert!(got["summary"].is_null(), "{got}");
        assert!(!head.state().post_pending);
        // Once: a later end runs nothing.
        std::fs::remove_file(
            head.paths
                .orchestrator_state_dir("merge")
                .join("scratch/post.json"),
        )
        .unwrap();
        head.set_state(id, TaskState::Done);
        head.runner.finish_now(&head.orch()).await;
        assert!(
            !head
                .paths
                .orchestrator_state_dir("merge")
                .join("scratch/post.json")
                .exists()
        );
        assert!(!head.kinds().contains(&"orchestrator.failed".to_string()));
    }

    #[tokio::test]
    async fn a_failing_post_script_is_an_event_and_changes_nothing() {
        let mut head = Head::new(&scheduled(), "echo 'PR #31'", "exit 2");
        let id = head.run().await.task.unwrap();
        head.set_state(id, TaskState::Done);
        head.kinds();
        head.runner.finish_now(&head.orch()).await;
        assert_eq!(head.kinds(), ["orchestrator.failed"]);
        assert_eq!(
            head.store.get_task(id).unwrap().unwrap().state,
            TaskState::Done
        );
        assert_eq!(head.state().failures, 0);
    }

    #[tokio::test]
    async fn the_limit_holds_an_agent_back() {
        let mut head = Head::new(&scheduled(), "echo 'PR #31'", "");
        // A person's orchestrator already works; the default limit is 1.
        head.store
            .insert_task_preempting(
                crate::store::NewTask {
                    description: None,
                    job: "run".into(),
                    item: serde_json::Value::Null,
                    prompt: "plan".into(),
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
                    },
                    flock: "default".into(),
                },
                crate::task::Priority::Normal,
                None,
                TaskRole::Orchestrator,
                false,
            )
            .unwrap();
        let run = head.run().await;
        assert_eq!(run.outcome, RunOutcome::Held, "{run:?}");
        assert_eq!(run.lines, ["PR #31"]);
        assert!(run.detail.unwrap().contains("max_orchestrators = 1"));
        assert_eq!(head.tasks().len(), 1);
        assert_eq!(head.kinds(), ["orchestrator.held"]);
    }

    #[tokio::test]
    async fn an_agent_stopped_on_a_quota_holds_the_next_until_the_reset() {
        let mut head = Head::new(&scheduled(), "echo 'PR #31'", "");
        let id = head.run().await.task.unwrap();
        head.store
            .note_pane_tail(id, "working...\nClaude AI usage limit reached|4102444800\n");
        head.set_state(id, TaskState::Done);
        head.runner.finish_now(&head.orch()).await;
        assert_eq!(head.kinds(), ["orchestrator.started", "orchestrator.quota"]);
        let until = head.state().quota_until.unwrap();
        assert_eq!(until, Utc.timestamp_opt(4_102_444_800, 0).single().unwrap());
        let run = head.run().await;
        assert_eq!(run.outcome, RunOutcome::Held);
        assert_eq!(head.kinds(), ["orchestrator.held"]);
        let st = head.runner.statuses(Utc::now());
        assert_eq!(st[0].state, "waiting for quota");
    }

    #[tokio::test]
    async fn an_invalid_edit_keeps_the_last_good_file_and_shows_the_error() {
        let head = Head::new(&scheduled(), "", "");
        std::fs::write(
            head.dir.join("merge.toml"),
            SCHEDULED.replace("kind = \"scheduled\"\n", ""),
        )
        .unwrap();
        std::fs::write(
            head.dir.join("night.toml"),
            format!("{SESSION}pre = [\"./p\"]\n"),
        )
        .unwrap();
        let st = head.runner.statuses(Utc::now());
        let [merge, night] = st.as_slice() else {
            panic!("{st:?}")
        };
        assert_eq!(merge.kind, Some(Kind::Scheduled));
        assert_eq!(merge.state, "idle");
        assert!(merge.error.as_deref().unwrap().contains("kind is required"));
        assert_eq!(night.state, "invalid");
        assert!(night.error.as_deref().unwrap().contains("pre"));
        assert!(head.runner.runnable("merge").is_ok());
        let off = offline_statuses(&head.paths, None).unwrap();
        assert_eq!(off[0].state, "invalid");
    }

    #[tokio::test]
    async fn disabled_is_off_and_never_due() {
        let head = Head::new(&scheduled(), "", "");
        set_enabled(&head.paths, "merge", false).unwrap();
        let st = head.runner.statuses(Utc::now());
        assert_eq!(st[0].state, "off");
        assert_eq!(st[0].next_run, None);
        assert!(set_enabled(&head.paths, "nope", true).is_err());
    }
}
