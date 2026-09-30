//! Orchestrators: one TOML file each in `~/.config/pastor/orchestrators/`,
//! run by the head. A file names its `kind`: a `scheduled` orchestrator runs
//! a pre script on a schedule and starts one agent, with the role
//! `orchestrator`, only when the script prints lines that need judgment; a
//! `session` orchestrator is one agent kept running through set hours.
//!
//! `Orchestrator::parse` is the file; `State` is what the head keeps between
//! runs under `state/orchestrators/<name>/` (no table in the store); `Runner`
//! is the head's loop that reads the files, runs what is due, runs the post
//! script once a run's agent has ended, and starts, restarts and stops
//! sessions (`Runner::session_step`).

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

/// The most a pre script may print, in bytes of kept lines. Its lines go
/// into the agent's prompt and `state.json`, so a looping script must not
/// grow either without bound; past this the run fails rather than hand the
/// agent part of the list.
pub const LINES_MAX_BYTES: usize = 64 * 1024;

/// How many times a session's agent is restarted in any hour; past that the
/// session waits for the oldest restart to be an hour old.
pub const RESTARTS_PER_HOUR: usize = 3;

/// Runs kept in `State::runs` for `orchestrator describe`.
const RUNS_KEPT: usize = 10;

/// Recent events `orchestrator describe` shows.
const EVENTS_SHOWN: usize = 10;

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
    /// starts them in `~/pastor-tasks`, as any task with no repo.
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
                let mut clocks = Vec::new();
                for (key, value) in [("start", &hours.start), ("stop", &hours.stop)] {
                    clocks.push(
                        parse_clock(value).ok_or(format!("hours.{key}: {value:?} is not HH:MM"))?,
                    );
                }
                if clocks[0] == clocks[1] {
                    return Err("hours.start and hours.stop must differ".into());
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
    /// the handover note, then every line the pre script printed. pastor adds
    /// the ask for a summary when it sends it (`task::prompt_to_send`), as for
    /// every task.
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
        out
    }
}

impl Orchestrator {
    /// The prompt of a session's agent, at its start and at each restart:
    /// the file's prompt, the skill, the handover note, and where to begin
    /// (`pastor watch --now`). `restart_of` is the agent it replaces.
    pub fn session_prompt(&self, note: Option<&str>, restart_of: Option<i64>) -> String {
        let mut out = self.agent_prompt(note, &[]);
        if let Some(id) = restart_of {
            out.push_str(&format!(
                "\n\nThis session's last agent, {}, ended before its hours did; you take over from it.",
                Task::agent_name_for(id)
            ));
        }
        out.push_str(
            "\n\nStart with `pastor watch --now`, which prints what needs attention at this moment, then keep watching with `pastor watch` until pastor tells you your hours are over.",
        );
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

/// The first local time of day `clock` after `now`. A time a clock change
/// skips that day is skipped with it; one it repeats counts once, at its
/// first occurrence. Counting the second too would start a session again
/// an hour after it stopped at a repeated `hours.stop`, and `in_hours`
/// already starts a head that comes up between the two at once.
fn next_clock(clock: NaiveTime, now: DateTime<Utc>) -> DateTime<Utc> {
    let mut day = now.with_timezone(&Local).date_naive();
    for _ in 0..4 {
        if let Some(t) = Local
            .from_local_datetime(&day.and_time(clock))
            .earliest()
            .map(|t| t.with_timezone(&Utc))
            && t > now
        {
            return t;
        }
        let Some(next) = day.succ_opt() else { break };
        day = next;
    }
    now + chrono::Duration::days(1)
}

impl Session {
    fn clock(value: &str) -> NaiveTime {
        parse_clock(value).expect("hours were checked when the file was read")
    }

    /// The next `hours.start` after `now`.
    pub fn next_start(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        next_clock(Self::clock(&self.hours.start), now)
    }

    /// The next `hours.stop` after `now`: when a session started at `now`
    /// stops.
    pub fn next_stop(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        next_clock(Self::clock(&self.hours.stop), now)
    }

    /// Whether `now` falls inside the hours: the next stop comes before the
    /// next start. Hours across midnight need nothing more.
    pub fn in_hours(&self, now: DateTime<Utc>) -> bool {
        self.next_stop(now) < self.next_start(now)
    }
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
    /// The last run's agent still worked, or its post script had not run:
    /// nothing ran.
    Skipped,
    /// Lines, but no agent: `max_orchestrators` or a quota wait.
    Held,
    /// The pre script failed, or the agent could not be queued.
    Failed,
    /// A session's agent ended before its hours did and a new one took over.
    Restarted,
    /// A session stopped.
    Stopped,
}

impl std::fmt::Display for RunOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RunOutcome::NoLines => "no lines",
            RunOutcome::Started => "started",
            RunOutcome::Skipped => "skipped",
            RunOutcome::Held => "held",
            RunOutcome::Failed => "failed",
            RunOutcome::Restarted => "restarted",
            RunOutcome::Stopped => "stopped",
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
    /// A session orchestrator's session, from its start to its stop.
    pub session: Option<SessionRun>,
    /// A session stopped by hand does not start on its hours before this.
    pub stopped_until: Option<DateTime<Utc>>,
    /// A session due on its hours has been held by `max_orchestrators`
    /// since then (`orchestrator.held` goes out once).
    pub held_since: Option<DateTime<Utc>>,
}

/// One session, kept from its start to its stop, restarts and quota waits
/// included: all that time it holds a `max_orchestrators` slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRun {
    pub started_at: DateTime<Utc>,
    /// `hours` or `hand`.
    pub started_by: String,
    /// When it stops: the first `hours.stop` after its start.
    pub until: DateTime<Utc>,
    /// Its restarts in the last hour, oldest first.
    #[serde(default)]
    pub restarts: Vec<DateTime<Utc>>,
    /// The restart cap held it (`orchestrator.held` went out).
    #[serde(default)]
    pub capped: bool,
    /// When its agent got its last message; it is closed `stop_grace` after.
    #[serde(default)]
    pub stopping_since: Option<DateTime<Utc>>,
    /// Why it stops: `hours` or `hand`.
    #[serde(default)]
    pub stop_reason: Option<String>,
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

/// The orchestrator's state; a missing file is a fresh one. A file that
/// cannot be read or parsed is an error that holds its runs: starting afresh
/// would forget a live agent (and start a second one) and a pending post
/// script.
pub fn load_state(paths: &Paths, name: &str) -> Result<State, String> {
    let path = state_file(paths, name);
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            serde_json::from_str(&text).map_err(|err| format!("parse {}: {err}", path.display()))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(err) => Err(format!("read {}: {err}", path.display())),
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
            | TaskState::Waiting
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
            TaskState::Waiting,
        ]),
        ..Default::default()
    })?;
    Ok(tasks
        .iter()
        .filter(|t| t.role == TaskRole::Orchestrator)
        .count())
}

/// One orchestrator as `orchestrator list` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestratorStatus {
    pub name: String,
    /// `None` when the file never parsed.
    pub kind: Option<Kind>,
    /// `idle`, `running`, `stopping`, `held`, `waiting for quota`, `off` or
    /// `invalid`.
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
    /// A session's stop, while it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<DateTime<Utc>>,
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
    #[serde(default)]
    pub stop_grace: Option<String>,
    /// The session running now.
    #[serde(default)]
    pub session: Option<SessionRun>,
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
    let (state, state_error) = match load_state(paths, name) {
        Ok(s) => (s, None),
        Err(e) => (State::default(), Some(e)),
    };
    let at_work = state
        .task
        .zip(store)
        .and_then(|(id, s)| s.get_task(id).ok().flatten())
        .is_some_and(|t| works(&t));
    let quota = state.quota_until.filter(|u| *u > now);
    let session = state.session.as_ref();
    let label = match orch {
        None => "invalid",
        Some(_) if state_error.is_some() => "held",
        Some(_) if session.is_some_and(|s| s.stopping_since.is_some()) => "stopping",
        Some(_) if session.is_some() && quota.is_some() => "waiting for quota",
        Some(_) if session.is_some() => "running",
        Some(o) if !o.enabled => "off",
        Some(_) if quota.is_some() => "waiting for quota",
        Some(_) if at_work => "running",
        Some(_) if state.held_since.is_some() => "held",
        Some(_) => "idle",
    };
    let next_run = match orch.map(|o| (o, &o.plan)) {
        Some((o, Plan::Session(s))) if o.enabled && session.is_none() => {
            let after = state.stopped_until.filter(|u| *u > now).unwrap_or(now);
            Some(if after == now && s.in_hours(now) {
                now
            } else {
                s.next_start(after)
            })
        }
        Some((_, Plan::Session(_))) => None,
        _ => orch.and_then(|o| o.next_run(&state, first_seen, now)),
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
        error: error.or(state_error),
        last_run_at: state.last_run_at,
        last_result: state.last_result.clone(),
        next_run,
        task: state.task,
        quota_until: quota,
        description: orch.and_then(|o| o.description.clone()),
        until: session.map(|s| s.until),
    }
}

fn describe_of(
    paths: &Paths,
    status: OrchestratorStatus,
    orch: Option<&Orchestrator>,
) -> OrchestratorDescription {
    let state = load_state(paths, &status.name).unwrap_or_default();
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
    let (pre, post, timeout, hours, stop_grace) = match orch.map(|o| &o.plan) {
        Some(Plan::Scheduled(s)) => (
            s.pre.clone(),
            s.post.clone(),
            Some(crate::schedule::describe_duration(s.timeout)),
            None,
            None,
        ),
        Some(Plan::Session(s)) => (
            Vec::new(),
            None,
            None,
            Some(s.hours.clone()),
            Some(crate::schedule::describe_duration(s.stop_grace)),
        ),
        None => (Vec::new(), None, None, None, None),
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
        stop_grace,
        session: state.session.clone(),
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
    /// Held from counting working orchestrator agents to queueing one, so
    /// runs of different orchestrators cannot both see a free slot.
    admit: tokio::sync::Mutex<()>,
    /// Each orchestrator's bad state file error, once `orchestrator.failed`
    /// has told it.
    bad_state: std::sync::Mutex<HashMap<String, String>>,
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
            admit: Default::default(),
            bad_state: Default::default(),
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
            .find(|n| load_state(&self.paths, n).is_ok_and(|s| s.task == Some(id)))
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
            let Ok(state) = self.state_of(&name) else {
                continue;
            };
            if let Plan::Session(s) = &orch.plan {
                let due = state.session.is_some()
                    || state.held_since.is_some()
                    || (orch.enabled
                        && s.in_hours(now)
                        && state.stopped_until.is_none_or(|u| u <= now));
                if !due {
                    continue;
                }
                let Ok(guard) = self.lock_of(&name).try_lock_owned() else {
                    continue;
                };
                let runner = self.clone();
                tokio::spawn(async move {
                    runner.session_step(&orch, now).await;
                    drop(guard);
                });
                continue;
            }
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
        let orch = self.valid(name)?;
        if orch.kind() == Kind::Session {
            return Err((
                "orchestrator_kind".into(),
                format!(
                    "{name} is a session orchestrator; `pastor orchestrator start {name}` starts it"
                ),
            ));
        }
        Ok(orch)
    }

    /// Orchestrator `name`'s last good version, if it is a session one.
    fn session_of(&self, name: &str) -> Result<Orchestrator, (String, String)> {
        let orch = self.valid(name)?;
        if orch.kind() != Kind::Session {
            return Err((
                "orchestrator_kind".into(),
                format!(
                    "{name} is a scheduled orchestrator; only a session one starts and stops (`pastor orchestrator run {name}` runs it now)"
                ),
            ));
        }
        Ok(orch)
    }

    fn valid(&self, name: &str) -> Result<Orchestrator, (String, String)> {
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
        Ok(orch)
    }

    /// One run of `orch` now, waiting for one already going, and its record:
    /// the post script of an agent that ended first, as a pass would.
    pub async fn run_now(&self, orch: &Orchestrator) -> RunRecord {
        let lock = self.lock_of(&orch.name);
        let _turn = lock.lock().await;
        let now = Utc::now();
        if let Ok(state) = self.state_of(&orch.name)
            && state.post_pending
            && !self.agent_works(&state)
        {
            self.finish(orch, now).await;
        }
        self.run(orch, now).await
    }

    /// Run the post script of `orch`'s ended agent now, if it is due, as a
    /// pass would; for tests and callers that wait on it.
    pub async fn finish_now(&self, orch: &Orchestrator) {
        let lock = self.lock_of(&orch.name);
        let _turn = lock.lock().await;
        if let Ok(state) = self.state_of(&orch.name)
            && state.post_pending
            && !self.agent_works(&state)
        {
            self.finish(orch, Utc::now()).await;
        }
    }

    /// Orchestrator `name`'s state. A bad file is an error, told once by
    /// `orchestrator.failed` until it changes or loads again.
    fn state_of(&self, name: &str) -> Result<State, String> {
        let loaded = load_state(&self.paths, name);
        let mut bad = self.bad_state.lock().unwrap_or_else(|p| p.into_inner());
        match &loaded {
            Ok(_) => {
                bad.remove(name);
            }
            Err(err) if bad.get(name) != Some(err) => {
                tracing::error!(orchestrator = name, %err, "state file bad; runs held");
                self.emit(
                    "failed",
                    name,
                    None,
                    serde_json::json!({"stage": "state", "error": err}),
                );
                bad.insert(name.to_string(), err.clone());
            }
            Err(_) => {}
        }
        loaded
    }

    fn agent_works(&self, state: &State) -> bool {
        state
            .task
            .and_then(|id| self.store.get_task(id).ok().flatten())
            .is_some_and(|t| works(&t))
    }

    /// When a quota lets an agent start again, if the agent of `task` ended
    /// on a usage limit (`limit::limit_in`): the reset its message names,
    /// else an hour from `now`. The message is at the end of `pane`, the
    /// last lines of its pane, or in its error when it died at its start;
    /// they are read apart, since a prompt in the pane would put the error
    /// above the turn. A 429 or 529 is no quota: the agent is restarted like
    /// any other that ended.
    ///
    /// A weekday, date or time of day in the message is resolved against
    /// `task.finished_at`, not `now`: this can run well after the agent
    /// actually ended, and reading a message that says `resets Mon 9am`
    /// after that Monday 9am has come and gone would otherwise pick next
    /// week's, or next year's `resets Oct 6` once Oct 6 has passed. A reset
    /// resolved that way, but already behind `now` by the time this runs,
    /// means the quota is not held back at all.
    ///
    /// The limit also goes in the head's table (`Fleet::note_limit`), under
    /// the account the agent names on its machine, so no task starts on
    /// that account either; `orchestrator.quota` is as before.
    fn quota_until(&self, task: &Task, pane: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let agents = self.fleet.agents();
        let kind = agents.kind(&task.spec.agent);
        let read_at = task.finished_at.unwrap_or(now);
        let limit = [pane, task.error.as_deref().unwrap_or("")]
            .iter()
            .find_map(|said| {
                crate::limit::limit_in(kind, said, read_at).filter(|limit| limit.hard)
            })?;
        let until = limit.retry_at(now);
        if until <= now {
            return None;
        }
        if let Some(machine) = task.machine.as_deref()
            && let Err(err) = self.fleet.note_limit(task, machine, &limit, read_at)
        {
            tracing::warn!(task = %task.display_id(), %err, "keep the usage limit");
        }
        Some(until)
    }

    /// When the account `orch`'s agent would run on is exhausted, hold it
    /// as for a quota its own agent found: `quota_until` becomes the
    /// account's `retry_at`, unless a later wait already holds it.
    fn hold_for_account(&self, orch: &Orchestrator, state: &mut State, now: DateTime<Utc>) {
        if state.quota_until.is_some_and(|u| u > now) {
            return;
        }
        let flock = self.fleet.flock();
        let Some(head) = flock.machines.iter().find(|m| m.local) else {
            return;
        };
        let ask = AgentChoice {
            model: orch.model.clone(),
            ..Default::default()
        };
        let pick = self.fleet.defaults().resolve_agent(&ask, None);
        let model = pick.model.as_ref().map(|(m, _)| m.as_str());
        if let Some(row) = self
            .fleet
            .limit_holding(&head.name, &pick.agent, model, now)
        {
            tracing::info!(orchestrator = %orch.name, account = %row.account, until = %row.retry_at, "its agent's account is exhausted; no agent before the reset");
            state.quota_until = Some(row.retry_at);
        }
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
        let mut run = RunRecord {
            at: now,
            outcome: RunOutcome::NoLines,
            detail: None,
            lines: Vec::new(),
            task: None,
            log: None,
        };
        // Not saved: the bad file stays for someone to look at.
        let mut state = match self.state_of(name) {
            Ok(s) => s,
            Err(err) => {
                run.outcome = RunOutcome::Held;
                run.detail = Some(format!("state: {err}"));
                return run;
            }
        };
        state.last_run_at = Some(now);
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
        // `finish` left the last agent's post script pending (its task could
        // not be read): a new agent would take its place in the state and
        // the post script would never run.
        if state.post_pending
            && let Some(id) = state.task
        {
            tracing::warn!(
                orchestrator = name,
                task = id,
                "run skipped: its last agent's post script waits"
            );
            run.outcome = RunOutcome::Skipped;
            run.detail = Some(format!(
                "the post script of {} has not run yet",
                Task::agent_name_for(id)
            ));
            self.emit(
                "skipped",
                name,
                Some(id),
                serde_json::json!({"reason": "post_pending"}),
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
        self.hold_for_account(orch, &mut state, now);
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
        let _admit = self.admit.lock().await;
        let max = self.fleet.max_orchestrators() as usize;
        match self.slots_taken() {
            Ok(n) if n >= max => {
                run.outcome = RunOutcome::Held;
                run.detail = Some(limit_detail(max, n));
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
        let note = read_note(&self.paths, &orch.name);
        let description = format!(
            "orchestrator {}: {} line{}",
            orch.name,
            lines.len(),
            if lines.len() == 1 { "" } else { "s" }
        );
        match self
            .start_agent(
                orch,
                orch.agent_prompt(note.as_deref(), &lines),
                description,
                None,
            )
            .await
        {
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

    /// Queue an agent of `orch` on the head's own machine, with the role,
    /// the file's model and repo, `prompt` and `description`. `timeout` is
    /// the task's own, else the defaults'.
    async fn start_agent(
        &self,
        orch: &Orchestrator,
        prompt: String,
        description: String,
        timeout: Option<Duration>,
    ) -> Result<Task, String> {
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
        let timeout = timeout
            .unwrap_or_else(|| {
                parse_duration(&defaults.timeout).unwrap_or(Duration::from_secs(2 * 60 * 60))
            })
            .as_secs();
        let spec = DispatchSpec {
            now: false,
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
            label: Default::default(),
            summary: Default::default(),
            cwd: None,
            keep_pane: None,
            keep_pane_from: None,
            rounds: Default::default(),
        };
        let task = self
            .fleet
            .queue_run_as(crate::daemon::RunAsk {
                prompt,
                spec,
                flock: None,
                ask: Some(&ask),
                priority: None,
                role: TaskRole::Orchestrator,
                description: Some(description),
                preempt: false,
                summary: None,
            })
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

    /// The `max_orchestrators` slots taken: every orchestrator agent at
    /// work, and every session between agents (a restart due, a quota
    /// wait), which holds its slot from start to stop.
    fn slots_taken(&self) -> anyhow::Result<usize> {
        let names: Vec<String> = self
            .entries
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .keys()
            .cloned()
            .collect();
        let idle_sessions = names
            .iter()
            .filter_map(|n| load_state(&self.paths, n).ok())
            .filter(|s| s.session.is_some() && !self.agent_works(s))
            .count();
        Ok(working_orchestrators(&self.store)? + idle_sessions)
    }

    /// `pastor orchestrator start`: start session `name` now, inside its
    /// hours or not, `enabled` or not; it stops at the next `hours.stop`.
    pub async fn start_by_hand(
        &self,
        name: &str,
        now: DateTime<Utc>,
    ) -> Result<String, (String, String)> {
        self.reload(now);
        let orch = self.session_of(name)?;
        let lock = self.lock_of(name);
        let _turn = lock.lock().await;
        let mut state = self
            .state_of(name)
            .map_err(|err| ("orchestrator_held".to_string(), format!("state: {err}")))?;
        if let Some(run) = &state.session {
            return Ok(format!(
                "orchestrator {name} already runs, until {}",
                local_clock(run.until)
            ));
        }
        let started = self.start_session(&orch, &mut state, now, "hand").await;
        save_state(&self.paths, name, &state);
        match started {
            Started::Agent(t) => Ok(format!(
                "started orchestrator {name}: {}, until {}",
                t.display_id(),
                local_clock(state.session.as_ref().map_or(now, |s| s.until))
            )),
            Started::Waiting(until) => Ok(format!(
                "started orchestrator {name}; its agent waits for the quota until {}",
                local_clock(until)
            )),
            Started::Held(detail) => Err(("orchestrator_held".into(), detail)),
            Started::Failed(reason) => Err((
                "runtime_error".into(),
                format!(
                    "orchestrator {name} started, but its agent was not queued: {reason}; the head tries again"
                ),
            )),
        }
    }

    /// `pastor orchestrator stop`: send session `name`'s agent its last
    /// message and close it after `stop_grace`; the session does not start
    /// on its hours again before their next stop. Inside its hours with no
    /// session (held, say), it only keeps it from starting.
    pub async fn stop_by_hand(
        &self,
        name: &str,
        now: DateTime<Utc>,
    ) -> Result<String, (String, String)> {
        self.reload(now);
        let orch = self.session_of(name)?;
        let Plan::Session(sess) = &orch.plan else {
            unreachable!("session_of checked the kind");
        };
        let lock = self.lock_of(name);
        let _turn = lock.lock().await;
        let mut state = self
            .state_of(name)
            .map_err(|err| ("orchestrator_held".to_string(), format!("state: {err}")))?;
        let Some(mut run) = state.session.clone() else {
            if sess.in_hours(now) {
                let until = sess.next_stop(now);
                state.stopped_until = Some(until);
                state.held_since = None;
                save_state(&self.paths, name, &state);
                return Ok(format!(
                    "orchestrator {name} is not running, and will not start before {}",
                    local_clock(sess.next_start(until))
                ));
            }
            return Err((
                "orchestrator_not_running".into(),
                format!("orchestrator {name} is not running"),
            ));
        };
        if run.stopping_since.is_some() {
            return Ok(format!("orchestrator {name} is already stopping"));
        }
        run.stop_reason = Some("hand".into());
        state.stopped_until = Some(run.until);
        let said = self.begin_stop(&orch, sess, &mut state, run, now).await;
        save_state(&self.paths, name, &state);
        Ok(said)
    }

    /// Start a session of `orch` at `now`, `by` its hours or by hand, if a
    /// `max_orchestrators` slot is free: the session, then its first agent
    /// (none while a quota wait lasts; the session holds its slot anyway).
    async fn start_session(
        &self,
        orch: &Orchestrator,
        state: &mut State,
        now: DateTime<Utc>,
        by: &str,
    ) -> Started {
        let name = orch.name.as_str();
        let Plan::Session(sess) = &orch.plan else {
            unreachable!("only a session orchestrator starts a session");
        };
        let _admit = self.admit.lock().await;
        let max = self.fleet.max_orchestrators() as usize;
        match self.slots_taken() {
            Ok(n) if n >= max => {
                let detail = limit_detail(max, n);
                if state.held_since.is_none() {
                    tracing::info!(orchestrator = name, "session held: max_orchestrators");
                    self.emit(
                        "held",
                        name,
                        None,
                        serde_json::json!({"reason": "max_orchestrators", "max": max}),
                    );
                    state.record(RunRecord {
                        at: now,
                        outcome: RunOutcome::Held,
                        detail: Some(detail.clone()),
                        lines: Vec::new(),
                        task: None,
                        log: None,
                    });
                }
                state.held_since.get_or_insert(now);
                return Started::Held(detail);
            }
            Ok(_) => {}
            Err(err) => return Started::Failed(format!("count orchestrators: {err:#}")),
        }
        let until = sess.next_stop(now);
        state.session = Some(SessionRun {
            started_at: now,
            started_by: by.to_string(),
            until,
            restarts: Vec::new(),
            capped: false,
            stopping_since: None,
            stop_reason: None,
        });
        state.task = None;
        state.stopped_until = None;
        state.held_since = None;
        state.last_run_at = Some(now);
        self.hold_for_account(orch, state, now);
        if let Some(wait) = state.quota_until.filter(|u| *u > now) {
            state.record(RunRecord {
                at: now,
                outcome: RunOutcome::Held,
                detail: Some(format!("waiting for the quota until {}", wait.to_rfc3339())),
                lines: Vec::new(),
                task: None,
                log: None,
            });
            self.emit(
                "held",
                name,
                None,
                serde_json::json!({"reason": "quota", "until": wait}),
            );
            return Started::Waiting(wait);
        }
        match self.start_session_agent(orch, sess, state, now, None).await {
            Ok(t) => {
                self.emit(
                    "started",
                    name,
                    Some(t.id),
                    serde_json::json!({"by": by, "until": until}),
                );
                state.record(RunRecord {
                    at: now,
                    outcome: RunOutcome::Started,
                    detail: None,
                    lines: Vec::new(),
                    task: Some(t.id),
                    log: None,
                });
                Started::Agent(Box::new(t))
            }
            Err(reason) => Started::Failed(reason),
        }
    }

    /// Queue a session's agent, the first (`restart_of` none) or one that
    /// takes over. Its timeout reaches past the session's stop and grace,
    /// so only an agent stuck that long goes stale. A failure is recorded
    /// and emitted; the next pass tries again as a restart, under the cap.
    async fn start_session_agent(
        &self,
        orch: &Orchestrator,
        sess: &Session,
        state: &mut State,
        now: DateTime<Utc>,
        restart_of: Option<i64>,
    ) -> Result<Task, String> {
        let name = orch.name.as_str();
        let until = state.session.as_ref().expect("a session is running").until;
        let left = (until - now).to_std().unwrap_or_default() + sess.stop_grace;
        let note = read_note(&self.paths, name);
        let description = format!("orchestrator {name}: session until {}", local_clock(until));
        let queued = self
            .start_agent(
                orch,
                orch.session_prompt(note.as_deref(), restart_of),
                description,
                Some(left.max(Duration::from_secs(60))),
            )
            .await;
        match queued {
            Ok(t) => {
                tracing::info!(orchestrator = name, task = %t.display_id(), "session agent queued");
                state.task = Some(t.id);
                state.quota_until = None;
                Ok(t)
            }
            Err(reason) => {
                tracing::warn!(orchestrator = name, %reason, "session agent not queued");
                state.record(RunRecord {
                    at: now,
                    outcome: RunOutcome::Failed,
                    detail: Some(format!("agent: {reason}")),
                    lines: Vec::new(),
                    task: None,
                    log: None,
                });
                self.emit(
                    "failed",
                    name,
                    None,
                    serde_json::json!({"stage": "agent", "error": reason}),
                );
                Err(reason)
            }
        }
    }

    /// Send the session's agent its last message and start its grace; with
    /// no agent at work, stop the session now. Answers what it did.
    async fn begin_stop(
        &self,
        orch: &Orchestrator,
        sess: &Session,
        state: &mut State,
        mut run: SessionRun,
        now: DateTime<Utc>,
    ) -> String {
        let name = orch.name.as_str();
        let task = state
            .task
            .and_then(|id| self.store.get_task(id).ok().flatten());
        let Some(task) = task.filter(works) else {
            state.session = Some(run);
            self.end_session(orch, state, now).await;
            return format!("orchestrator {name} stopped");
        };
        let grace = crate::schedule::describe_duration(sess.stop_grace);
        let text = format!(
            "pastor: this orchestrator session ends now. Keep a handover note for the next agent with `pastor orchestrator note`, then end your turn; pastor closes this agent in {grace}."
        );
        if let Some(handle) = task.machine.as_deref().and_then(|m| self.fleet.get(m))
            && task.state.occupies_pane()
        {
            let input = crate::machine::SendInput {
                text: Some(text),
                enter: true,
                ..Default::default()
            };
            if let Err(err) = self
                .fleet
                .bounded(&handle.name, handle.send(task.id, input))
                .await
            {
                tracing::warn!(orchestrator = name, task = %task.display_id(), err = %format!("{err:#}"), "send the last message");
            }
        }
        let reason = run.stop_reason.clone().unwrap_or_else(|| "hours".into());
        run.stopping_since = Some(now);
        state.session = Some(run);
        self.emit(
            "stopping",
            name,
            Some(task.id),
            serde_json::json!({"reason": reason, "grace": grace}),
        );
        format!(
            "sent orchestrator {name}'s agent {} its last message; it is closed within {grace}",
            task.display_id()
        )
    }

    /// Close the session's agent if it still has a pane or a queue slot,
    /// and end the session.
    async fn end_session(&self, orch: &Orchestrator, state: &mut State, now: DateTime<Utc>) {
        let name = orch.name.as_str();
        let Some(run) = state.session.take() else {
            return;
        };
        if let Some(t) = state
            .task
            .and_then(|id| self.store.get_task(id).ok().flatten())
            && !self.close_agent(name, &t).await
        {
            // Its agent may still run: keep the session, and try again.
            state.session = Some(run);
            return;
        }
        let reason = run.stop_reason.unwrap_or_else(|| "hours".into());
        tracing::info!(orchestrator = name, %reason, "session stopped");
        self.emit(
            "stopped",
            name,
            state.task,
            serde_json::json!({"reason": reason}),
        );
        state.record(RunRecord {
            at: now,
            outcome: RunOutcome::Stopped,
            detail: Some(format!("by {reason}")),
            lines: Vec::new(),
            task: None,
            log: None,
        });
    }

    /// Close task `t` of orchestrator `name`: through its machine when it
    /// holds a pane there, else its row. A closed or ended-without-pane task
    /// is left as it is. A queued one a dispatch claims in between is read
    /// again and closed through its machine. Whether it is closed now: on
    /// `false` it may still run, so the caller must not start another agent
    /// and tries again on its next pass.
    async fn close_agent(&self, name: &str, t: &Task) -> bool {
        let mut t = t.clone();
        // A claim moves a task out of `queued` once; two reads settle it.
        for _ in 0..3 {
            let result = match t.state {
                TaskState::Queued | TaskState::Paused | TaskState::Waiting => {
                    match self.store.close_queued(t.id) {
                        Ok(Some(_)) => return true,
                        Ok(None) => match self.store.get_task(t.id) {
                            Ok(Some(fresh)) => {
                                t = fresh;
                                continue;
                            }
                            Ok(None) => return true,
                            Err(err) => Err(err),
                        },
                        Err(err) => Err(err),
                    }
                }
                s if s.occupies_pane() => {
                    match t.machine.as_deref().and_then(|m| self.fleet.get(m)) {
                        Some(h) => self
                            .fleet
                            .bounded(&h.name, h.close(t.id, false))
                            .await
                            .map(|_| ()),
                        None => self.store.close_task(t.id).map(|_| ()),
                    }
                }
                _ => return true,
            };
            return match result {
                Ok(()) => true,
                Err(err) => {
                    tracing::warn!(orchestrator = name, task = %t.display_id(), err = %format!("{err:#}"), "close the session's agent");
                    false
                }
            };
        }
        tracing::warn!(orchestrator = name, task = %t.display_id(), "the session's agent kept changing state while closing it");
        false
    }

    /// One pass of session `orch` at `now`, under its lock: start it on its
    /// hours (or wait for a free slot), stop it at `hours.stop` with its last
    /// message and grace, and restart its agent when it ended early, after a
    /// quota wait, at most `RESTARTS_PER_HOUR` times an hour.
    pub async fn session_step(&self, orch: &Orchestrator, now: DateTime<Utc>) {
        let name = orch.name.as_str();
        let Plan::Session(sess) = &orch.plan else {
            return;
        };
        let Ok(mut state) = self.state_of(name) else {
            return;
        };
        let before = state.clone();
        self.session_step_state(orch, sess, &mut state, now).await;
        if state != before {
            save_state(&self.paths, name, &state);
        }
    }

    async fn session_step_state(
        &self,
        orch: &Orchestrator,
        sess: &Session,
        state: &mut State,
        now: DateTime<Utc>,
    ) {
        let name = orch.name.as_str();
        let Some(mut run) = state.session.clone() else {
            let due =
                orch.enabled && sess.in_hours(now) && state.stopped_until.is_none_or(|u| u <= now);
            if due {
                self.start_session(orch, state, now, "hours").await;
            } else {
                state.held_since = None;
            }
            return;
        };
        let task = state
            .task
            .and_then(|id| self.store.get_task(id).ok().flatten());
        let at_work = task.as_ref().is_some_and(works);
        if let Some(since) = run.stopping_since {
            let grace = chrono::Duration::from_std(sess.stop_grace).unwrap_or_default();
            if !at_work || now >= since + grace {
                self.end_session(orch, state, now).await;
            }
            return;
        }
        if now >= run.until {
            run.stop_reason.get_or_insert_with(|| "hours".into());
            self.begin_stop(orch, sess, state, run, now).await;
            return;
        }
        if at_work {
            return;
        }
        // Its agent ended (or never started) before the hours did.
        match state.quota_until {
            Some(until) if until > now => return,
            Some(_) => {}
            None => {
                if let Some(t) = &task {
                    let pane = self.store.pane_tail(t.id).unwrap_or_default();
                    if let Some(until) = self.quota_until(t, &pane, now) {
                        tracing::warn!(orchestrator = name, task = %t.display_id(), until = %until, "session agent stopped on a quota; restarting at the reset");
                        state.quota_until = Some(until);
                        // A failed close is tried again before the restart.
                        self.close_agent(name, t).await;
                        self.emit(
                            "quota",
                            name,
                            Some(t.id),
                            serde_json::json!({"until": until}),
                        );
                        return;
                    }
                }
                self.hold_for_account(orch, state, now);
                if state.quota_until.is_some_and(|u| u > now) {
                    return;
                }
            }
        }
        let hour_ago = now - chrono::Duration::hours(1);
        run.restarts.retain(|t| *t > hour_ago);
        if run.restarts.len() >= RESTARTS_PER_HOUR {
            if !run.capped {
                run.capped = true;
                let next = run.restarts[0] + chrono::Duration::hours(1);
                tracing::warn!(
                    orchestrator = name,
                    "session agent restarted {RESTARTS_PER_HOUR} times this hour; waiting"
                );
                self.emit(
                    "held",
                    name,
                    state.task,
                    serde_json::json!({"reason": "restarts", "max": RESTARTS_PER_HOUR, "until": next}),
                );
                state.record(RunRecord {
                    at: now,
                    outcome: RunOutcome::Held,
                    detail: Some(format!(
                        "restarted {RESTARTS_PER_HOUR} times in the last hour; the next restart waits until {}",
                        next.to_rfc3339()
                    )),
                    lines: Vec::new(),
                    task: None,
                    log: None,
                });
            }
            state.session = Some(run);
            return;
        }
        run.capped = false;
        let old = task.as_ref().map(|t| t.id);
        if let Some(t) = &task
            && !self.close_agent(name, t).await
        {
            // The old agent may still run: no second one beside it.
            state.session = Some(run);
            return;
        }
        run.restarts.push(now);
        let restarts = run.restarts.len();
        state.session = Some(run);
        if let Ok(t) = self.start_session_agent(orch, sess, state, now, old).await {
            self.emit(
                "restarted",
                name,
                Some(t.id),
                serde_json::json!({"after": old, "restarts": restarts}),
            );
            state.record(RunRecord {
                at: now,
                outcome: RunOutcome::Restarted,
                detail: old.map(|id| format!("after {}", Task::agent_name_for(id))),
                lines: Vec::new(),
                task: Some(t.id),
                log: None,
            });
        }
    }

    /// The agent `orch`'s last run started has ended: note a quota error,
    /// then run the post script once, with the end state, the summary and
    /// the lines. A task closed before pastor saw it end runs nothing.
    async fn finish(&self, orch: &Orchestrator, now: DateTime<Utc>) {
        let name = orch.name.as_str();
        let Ok(mut state) = self.state_of(name) else {
            return;
        };
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
        if let Some(until) = self.quota_until(&task, &last_output, now) {
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
        let mut kept = 0;
        let mut overflow = false;
        let out_log = log.clone();
        let done = exec::run(inv, log.clone(), |line| {
            let mut l = out_log.lock().unwrap_or_else(|p| p.into_inner());
            l.line(&format!("stdout: {line}"));
            let clean = crate::template::strip_controls(&l.redact(line)).replace('\n', "");
            if clean.trim().is_empty() || overflow {
                return;
            }
            kept += clean.len();
            if kept > LINES_MAX_BYTES {
                overflow = true;
                lines.clear();
            } else {
                lines.push(clean);
            }
        })
        .await;
        Ok(ScriptRun {
            result: if !done.exit.success() {
                Err(done.reason())
            } else if overflow {
                Err(format!(
                    "printed more than {} KiB of lines",
                    LINES_MAX_BYTES / 1024
                ))
            } else {
                Ok(lines)
            },
            log: Some(path),
        })
    }
}

/// How `Runner::start_session` went.
enum Started {
    Agent(Box<Task>),
    /// The session started, and its agent waits for a quota reset.
    Waiting(DateTime<Utc>),
    /// `max_orchestrators` held it back, and why.
    Held(String),
    /// The session started, but its agent was not queued.
    Failed(String),
}

fn limit_detail(max: usize, n: usize) -> String {
    format!(
        "max_orchestrators = {max}, and {n} orchestrator slot{} already taken",
        if n == 1 { " is" } else { "s are" }
    )
}

/// `t` as local `HH:MM`, for messages.
fn local_clock(t: DateTime<Utc>) -> String {
    t.with_timezone(&Local).format("%H:%M").to_string()
}

struct ScriptRun {
    result: Result<Vec<String>, String>,
    log: Option<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PastorConfig;
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
    fn the_agents_prompt_is_the_prompt_skill_note_then_lines() {
        let o = parse(SCHEDULED).unwrap();
        let p = o.agent_prompt(
            Some("merged #31\n"),
            &["PR #32 x".into(), "TASK t-4 blocked".into()],
        );
        let prompt = p.find("Decide what").unwrap();
        let skill = p.find("Use your orchestrating-pastor skill.").unwrap();
        let note = p.find("merged #31").unwrap();
        let line = p.find("PR #32 x\nTASK t-4 blocked").unwrap();
        assert!(prompt < skill && skill < note && note < line, "{p}");
        assert!(p.ends_with("TASK t-4 blocked"), "{p}");
        assert!(!o.agent_prompt(None, &[]).contains("handover"));
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
            load_state(&self.paths, "merge").unwrap()
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
    async fn a_pending_post_script_holds_the_next_run() {
        let pre = "echo ran >> \"$PASTOR_ORCHESTRATOR_STATE_DIR/runs\"\necho 'PR #31'";
        let mut head = Head::new(&scheduled(), pre, "");
        let id = head.run().await.task.unwrap();
        head.set_state(id, TaskState::Done);
        head.kinds();
        // As when `finish` could not read the task: the post script waits,
        // and a new agent must not take the old one's place.
        let run = head.runner.run(&head.orch(), Utc::now()).await;
        assert_eq!(run.outcome, RunOutcome::Skipped, "{run:?}");
        assert!(run.detail.unwrap().contains("post script"));
        assert_eq!(head.kinds(), ["orchestrator.skipped"]);
        let state = head.state();
        assert_eq!(state.task, Some(id));
        assert!(state.post_pending);
        assert_eq!(state.lines, ["PR #31"]);
        assert_eq!(head.tasks().len(), 1);
        let runs = std::fs::read_to_string(
            head.paths
                .orchestrator_state_dir("merge")
                .join("scratch/runs"),
        )
        .unwrap();
        assert_eq!(runs.lines().count(), 1, "the pre script ran again");
    }

    /// A bad state file holds every run of `head`'s `merge`: `orchestrator.failed`
    /// once, with `want` in its error, no agent, and the file left as it is.
    async fn a_bad_state_file_holds_the_run(mut head: Head, want: &str) {
        let file = head
            .paths
            .orchestrator_state_dir("merge")
            .join("state.json");
        let before = std::fs::metadata(&file).unwrap().modified().unwrap();
        let tasks = head.tasks().len();
        head.kinds();
        let mut events = head.runner.events.subscribe();
        for _ in 0..2 {
            let run = head.run().await;
            assert_eq!(run.outcome, RunOutcome::Held, "{run:?}");
            let detail = run.detail.unwrap();
            assert!(
                detail.contains("state.json") && detail.contains(want),
                "{detail}"
            );
            head.runner.pass(Utc::now());
            tokio::task::yield_now().await;
        }
        assert_eq!(head.kinds(), ["orchestrator.failed"], "once per error");
        let failed = events.try_recv().unwrap().detail.unwrap();
        assert_eq!(failed["stage"], "state");
        assert!(
            failed["error"].as_str().unwrap().contains("state.json"),
            "{failed}"
        );
        assert_eq!(head.tasks().len(), tasks, "no second agent");
        assert_eq!(
            std::fs::metadata(&file).unwrap().modified().unwrap(),
            before
        );
        assert_eq!(head.runner.statuses(Utc::now())[0].state, "held");
    }

    #[tokio::test]
    async fn a_corrupt_state_file_holds_the_run() {
        let head = Head::new(&scheduled(), "echo 'PR #31'", "");
        assert_eq!(head.run().await.outcome, RunOutcome::Started);
        let file = head
            .paths
            .orchestrator_state_dir("merge")
            .join("state.json");
        std::fs::write(&file, "{\"task\": ").unwrap();
        a_bad_state_file_holds_the_run(head, "parse").await;
    }

    #[tokio::test]
    async fn an_unreadable_state_file_holds_the_run() {
        let head = Head::new(&scheduled(), "echo 'PR #31'", "");
        assert_eq!(head.run().await.outcome, RunOutcome::Started);
        let file = head
            .paths
            .orchestrator_state_dir("merge")
            .join("state.json");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&file).is_ok() {
            return; // root reads it anyway
        }
        a_bad_state_file_holds_the_run(head, "read").await;
    }

    #[test]
    fn a_missing_state_file_is_a_fresh_start() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        assert_eq!(load_state(&paths, "merge").unwrap(), State::default());
    }

    #[tokio::test]
    async fn a_pre_script_printing_too_much_fails_the_run() {
        let pre = format!(
            "i=0; while [ $i -lt {} ]; do echo 'PR #31 ci=failure'; i=$((i+1)); done",
            LINES_MAX_BYTES / 16
        );
        let mut head = Head::new(&scheduled(), &pre, "");
        let run = head.run().await;
        assert_eq!(run.outcome, RunOutcome::Failed, "{run:?}");
        assert!(run.lines.is_empty());
        assert!(run.detail.as_deref().unwrap().contains("64 KiB"), "{run:?}");
        assert!(head.tasks().is_empty());
        assert_eq!(head.kinds(), ["orchestrator.failed"]);
        assert!(head.state().lines.is_empty());
        assert_eq!(head.state().failures, 1);
    }

    #[tokio::test]
    async fn runs_of_two_orchestrators_share_the_limit() {
        let head = Head::new(&scheduled(), "echo 'PR #31'", "");
        std::fs::write(head.dir.join("other.toml"), scheduled()).unwrap();
        head.runner.reload(Utc::now());
        // Queueing waits on the dispatch lock: held here, both runs reach
        // it before either task exists, and only the limit's own lock keeps
        // the second from counting the same free slot.
        let pass = head.runner.fleet.hold_dispatch_lock().await;
        let [a, b] = ["merge", "other"].map(|name| {
            let runner = head.runner.clone();
            let orch = runner.runnable(name).unwrap();
            tokio::spawn(async move { runner.run_now(&orch).await })
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        drop(pass);
        let (a, b) = (a.await.unwrap(), b.await.unwrap());
        let mut outcomes = [a.outcome, b.outcome];
        outcomes.sort_by_key(|o| o.to_string());
        assert_eq!(
            outcomes,
            [RunOutcome::Held, RunOutcome::Started],
            "{a:?} {b:?}"
        );
        assert_eq!(head.tasks().len(), 1);
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
                        now: false,
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
                        cwd: None,
                        keep_pane: None,
                        keep_pane_from: None,
                        rounds: Default::default(),
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

    /// A quota its agent stopped on also goes in the head's table, under
    /// the account of the agent on the machine it ran on.
    #[tokio::test]
    async fn a_quota_its_agent_found_is_kept_for_the_account() {
        let mut head = Head::new(&scheduled(), "echo 'PR #31'", "");
        let id = head.run().await.task.unwrap();
        head.store
            .note_pane_tail(id, "working...\nClaude AI usage limit reached|4102444800\n");
        let mut t = head.store.get_task(id).unwrap().unwrap();
        t.machine = Some("head".into());
        t.state = TaskState::Done;
        head.store.update_task(&mut t).unwrap();
        head.runner.finish_now(&head.orch()).await;
        assert_eq!(head.kinds(), ["orchestrator.started", "orchestrator.quota"]);
        let rows = head.store.limits().unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].account, "head/claude");
        assert_eq!(rows[0].task_id, Some(id));
        assert_eq!(
            rows[0].retry_at,
            Utc.timestamp_opt(4_102_444_800, 0).single().unwrap()
        );
    }

    /// An orchestrator whose agent's account is exhausted on the head's
    /// machine starts no agent before the account's reset.
    #[tokio::test]
    async fn an_exhausted_account_holds_the_orchestrator() {
        let mut head = Head::new(&scheduled(), "echo 'PR #31'", "");
        let now = Utc::now();
        let reset = now + chrono::Duration::hours(2);
        head.store
            .record_limit(&crate::limit::AccountLimit {
                account: "head/claude".into(),
                model: None,
                hard: true,
                no_credit: false,
                until: Some(reset),
                retry_at: reset,
                line: "You've hit your limit".into(),
                task_id: None,
                machine: Some("head".into()),
                agent: Some("claude".into()),
                seen_at: now,
            })
            .unwrap();
        let run = head.run().await;
        assert_eq!(run.outcome, RunOutcome::Held, "{run:?}");
        assert!(head.tasks().is_empty());
        assert_eq!(head.kinds(), ["orchestrator.held"]);
        assert_eq!(head.state().quota_until, Some(reset));
        let st = head.runner.statuses(Utc::now());
        assert_eq!(st[0].state, "waiting for quota");
        // Cleared, it holds nothing.
        head.runner.fleet.clear_limits("head/claude", None).unwrap();
        let mut state = head.state();
        state.quota_until = None;
        save_state(&head.paths, "merge", &state);
        assert_eq!(head.run().await.outcome, RunOutcome::Started);
    }

    /// A scheduled orchestrator whose agent, run by `agent`, ended with
    /// `error` and `pane`: the events of its finish and the quota it holds.
    async fn ended_with(
        agent: &str,
        error: Option<&str>,
        pane: &str,
    ) -> (Vec<String>, Option<DateTime<Utc>>) {
        let mut head = Head::new(&scheduled(), "echo 'PR #31'", "");
        let config = "[agents.claude-personal]\nkind = \"claude\"\n";
        let config = PastorConfig::parse(Path::new("pastor.toml"), config).unwrap();
        head.runner.fleet.set_config(&config);
        let id = head.run().await.task.unwrap();
        let mut task = head.store.get_task(id).unwrap().unwrap();
        task.spec.agent = agent.into();
        task.error = error.map(str::to_string);
        task.state = TaskState::Done;
        head.store.update_task(&mut task).unwrap();
        head.store.note_pane_tail(id, pane);
        head.runner.finish_now(&head.orch()).await;
        (head.kinds(), head.state().quota_until)
    }

    #[tokio::test]
    async fn a_limit_that_did_not_end_the_turn_holds_nothing() {
        let limit = "Claude AI usage limit reached|4102444800";
        for pane in [
            // Above the last prompt.
            format!("● {limit}\n\n> carry on\n\n● Merged #31.\n"),
            // What a tool printed: pastor's own source has these words.
            format!("● Bash(grep -rn limit src/limit.rs)\n  ⎿  {limit}\n     {limit}\n"),
            format!("● Merged #31. The parser reads \"{limit}\".\n"),
            format!("● {limit}\n\n● Merged #31.\n"),
            // Busy, not out of tokens: restarted like any agent that ended.
            "● API Error: 529 {\"type\":\"error\"}\n".to_string(),
        ] {
            let (kinds, quota) = ended_with("claude", None, &pane).await;
            assert_eq!(kinds, ["orchestrator.started"], "{pane}");
            assert_eq!(quota, None, "{pane}");
        }
    }

    #[tokio::test]
    async fn a_limit_is_read_as_the_agents_kind_prints_it() {
        let reset = Utc.timestamp_opt(4_102_444_800, 0).single();
        let pane = "● Claude AI usage limit reached|4102444800\n\n────\n❯ \n────\n";
        let quota = ["orchestrator.started", "orchestrator.quota"];
        assert_eq!(
            ended_with("claude", None, pane).await,
            (quota.map(String::from).to_vec(), reset)
        );
        assert_eq!(
            ended_with("claude-personal", None, pane).await,
            (quota.map(String::from).to_vec(), reset)
        );
        // Only Claude's messages are known.
        assert_eq!(
            ended_with("opencode", None, pane).await,
            (vec!["orchestrator.started".to_string()], None)
        );
        // An agent that died at its start left its message in the error,
        // whatever its pane shows.
        let error = "Claude AI usage limit reached|4102444800";
        for pane in ["", "> Decide what to do.\n\n────\n❯ \n────\n"] {
            assert_eq!(
                ended_with("claude", Some(error), pane).await,
                (quota.map(String::from).to_vec(), reset),
                "{pane}"
            );
        }
    }

    #[tokio::test]
    async fn a_limit_that_names_no_reset_waits_an_hour() {
        let before = Utc::now();
        let (kinds, quota) = ended_with("claude", None, "● Credit balance is too low\n").await;
        assert_eq!(kinds, ["orchestrator.started", "orchestrator.quota"]);
        let wait = quota.unwrap() - before;
        assert!(
            wait >= chrono::Duration::hours(1) && wait < chrono::Duration::minutes(61),
            "{wait}"
        );
    }

    /// The head handles a finished agent later than it actually ended
    /// (`finished`): its message's weekday or date must resolve against
    /// that, not the head's own `now`, or a reset already behind `now`
    /// (`processed`) is read as next week's or next year's instead of
    /// already passed.
    async fn finished_late(pane: &str, finished: DateTime<Utc>, processed: DateTime<Utc>) -> State {
        let head = Head::new(&scheduled(), "echo 'PR #31'", "");
        let id = head.run().await.task.unwrap();
        head.store.note_pane_tail(id, pane);
        let mut t = head.store.get_task(id).unwrap().unwrap();
        t.state = TaskState::Done;
        t.finished_at = Some(finished);
        head.store.update_task(&mut t).unwrap();
        head.runner.finish(&head.orch(), processed).await;
        head.state()
    }

    #[tokio::test]
    async fn a_delayed_finish_does_not_push_a_weekday_reset_to_next_week() {
        // The agent's turn ended just before the reset it named; the head
        // handles it a minute later, after that Monday 9am has come.
        let finished = Utc.with_ymd_and_hms(2026, 10, 5, 8, 59, 0).unwrap();
        let processed = Utc.with_ymd_and_hms(2026, 10, 5, 9, 1, 0).unwrap();
        let state = finished_late(
            "● Weekly limit reached ∙ resets Mon 9am (UTC)\n",
            finished,
            processed,
        )
        .await;
        assert_eq!(state.quota_until, None);
    }

    #[tokio::test]
    async fn a_delayed_finish_does_not_push_a_date_reset_to_next_year() {
        // Same, a minute past a named date instead of a weekday.
        let finished = Utc.with_ymd_and_hms(2026, 10, 6, 8, 59, 0).unwrap();
        let processed = Utc.with_ymd_and_hms(2026, 10, 6, 9, 1, 0).unwrap();
        let state = finished_late(
            "● Opus weekly limit reached ∙ resets Oct 6, 9am (UTC)\n",
            finished,
            processed,
        )
        .await;
        assert_eq!(state.quota_until, None);
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

    /// Local `HH:MM` on 2026-09-`day`.
    fn at(day: u32, h: u32, m: u32) -> DateTime<Utc> {
        Local
            .with_ymd_and_hms(2026, 9, day, h, m, 0)
            .earliest()
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn hours_across_midnight_hold_both_sides_of_it() {
        let Plan::Session(s) = parse(SESSION).unwrap().plan else {
            panic!()
        };
        assert!(!s.in_hours(at(28, 21, 59)));
        assert!(s.in_hours(at(28, 22, 0)));
        assert!(s.in_hours(at(29, 3, 0)));
        assert!(!s.in_hours(at(29, 8, 0)));
        assert_eq!(s.next_stop(at(28, 22, 0)), at(29, 8, 0));
        assert_eq!(s.next_start(at(29, 8, 0)), at(29, 22, 0));
        let day = parse(&SESSION.replace("22:00", "09:00").replace("08:00", "17:00")).unwrap();
        let Plan::Session(d) = day.plan else { panic!() };
        assert!(d.in_hours(at(28, 12, 0)) && !d.in_hours(at(28, 18, 0)));
        let err = parse(&SESSION.replace("08:00", "22:00")).unwrap_err();
        assert!(err.contains("differ"), "{err}");
    }

    impl Head {
        async fn step(&self, now: DateTime<Utc>) {
            let orch = self.runner.session_of("merge").unwrap();
            self.runner.session_step(&orch, now).await;
        }

        fn agent(&self) -> Task {
            let id = self.state().task.expect("a session agent");
            self.store.get_task(id).unwrap().unwrap()
        }
    }

    #[tokio::test]
    async fn a_session_starts_on_its_hours_with_the_prompt_and_watch() {
        let mut head = Head::new(SESSION, "", "");
        head.step(at(28, 21, 0)).await;
        assert!(head.tasks().is_empty());
        assert_eq!(
            head.runner.statuses(at(28, 21, 0))[0].next_run,
            Some(at(28, 22, 0))
        );
        head.step(at(28, 22, 0)).await;
        let t = head.agent();
        assert_eq!(t.role, TaskRole::Orchestrator);
        assert_eq!(t.spec.machine.as_deref(), Some("head"));
        assert!(
            t.prompt.starts_with("You are the night orchestrator."),
            "{}",
            t.prompt
        );
        assert!(t.prompt.contains("`pastor watch --now`"), "{}", t.prompt);
        assert!(
            t.spec.timeout_secs >= 10 * 3600,
            "a session agent outlasts the defaults' timeout: {}",
            t.spec.timeout_secs
        );
        assert_eq!(head.kinds(), ["orchestrator.started"]);
        let run = head.state().session.unwrap();
        assert_eq!(run.until, at(29, 8, 0));
        assert_eq!(run.started_by, "hours");
        // Its agent at work: nothing more.
        head.step(at(28, 23, 0)).await;
        assert_eq!(head.tasks().len(), 1);
        assert!(head.kinds().is_empty());
        assert_eq!(head.runner.statuses(at(28, 23, 0))[0].state, "running");
        // `run` is for the scheduled kind.
        let err = head.runner.fire("merge").unwrap_err();
        assert_eq!(err.0, "orchestrator_kind");
    }

    #[tokio::test]
    async fn a_head_started_inside_the_hours_starts_the_session_at_once() {
        let head = Head::new(SESSION, "", "");
        head.step(at(29, 3, 0)).await;
        assert_eq!(head.state().session.unwrap().until, at(29, 8, 0));
        assert_eq!(head.tasks().len(), 1);
    }

    #[tokio::test]
    async fn a_session_started_early_by_hand_runs_to_the_stop() {
        let mut head = Head::new(SESSION, "", "");
        let said = head
            .runner
            .start_by_hand("merge", at(28, 20, 0))
            .await
            .unwrap();
        assert!(said.contains("t-") && said.contains("08:00"), "{said}");
        let run = head.state().session.unwrap();
        assert_eq!((run.started_by.as_str(), run.until), ("hand", at(29, 8, 0)));
        // Starting its hours does not start a second one.
        head.step(at(28, 22, 0)).await;
        assert_eq!(head.tasks().len(), 1);
        assert_eq!(head.kinds(), ["orchestrator.started"]);
        let again = head
            .runner
            .start_by_hand("merge", at(28, 22, 5))
            .await
            .unwrap();
        assert!(again.contains("already runs"), "{again}");
    }

    #[tokio::test]
    async fn a_session_stops_with_a_last_message_and_a_grace() {
        let mut head = Head::new(SESSION, "", "");
        head.step(at(28, 22, 0)).await;
        let id = head.agent().id;
        head.set_state(id, TaskState::Running);
        head.kinds();
        head.step(at(29, 8, 0)).await;
        assert_eq!(head.kinds(), ["orchestrator.stopping"]);
        assert_eq!(head.runner.statuses(at(29, 8, 0))[0].state, "stopping");
        head.step(at(29, 8, 4)).await;
        assert_eq!(head.agent().state, TaskState::Running, "within its grace");
        head.step(at(29, 8, 5)).await;
        assert_eq!(head.agent().state, TaskState::Closed);
        assert_eq!(head.kinds(), ["orchestrator.stopped"]);
        assert_eq!(head.state().session, None);
        // Out of hours: nothing starts until the next night.
        head.step(at(29, 12, 0)).await;
        assert_eq!(head.tasks().len(), 1);
        head.step(at(29, 22, 0)).await;
        assert_eq!(head.tasks().len(), 2);
    }

    #[tokio::test]
    async fn an_agent_that_ends_in_its_grace_is_closed_at_once() {
        let mut head = Head::new(SESSION, "", "");
        head.step(at(28, 22, 0)).await;
        let id = head.agent().id;
        head.step(at(29, 8, 0)).await;
        head.set_state(id, TaskState::Done);
        head.kinds();
        head.step(at(29, 8, 1)).await;
        assert_eq!(head.agent().state, TaskState::Closed);
        assert_eq!(head.kinds(), ["orchestrator.stopped"]);
    }

    #[tokio::test]
    async fn an_agent_claimed_while_closing_is_closed_on_its_machine() {
        let head = Head::new(SESSION, "", "");
        head.step(at(28, 22, 0)).await;
        let read = head.agent();
        assert_eq!(read.state, TaskState::Queued);
        // A dispatch claims it between that read and the close.
        let mut claimed = head.store.claim_task(read.id, "head").unwrap().unwrap();
        claimed.state = TaskState::Running;
        head.store.update_task(&mut claimed).unwrap();
        assert!(head.runner.close_agent("merge", &read).await);
        assert_eq!(head.agent().state, TaskState::Closed);
    }

    #[tokio::test]
    async fn a_session_stopped_by_hand_waits_for_its_next_hours() {
        let mut head = Head::new(SESSION, "", "");
        head.step(at(28, 22, 0)).await;
        head.kinds();
        let said = head
            .runner
            .stop_by_hand("merge", at(28, 22, 30))
            .await
            .unwrap();
        assert!(said.contains("last message"), "{said}");
        assert_eq!(head.kinds(), ["orchestrator.stopping"]);
        let since = head.state().session.unwrap().stopping_since.unwrap();
        head.step(since + chrono::Duration::minutes(6)).await;
        assert_eq!(head.agent().state, TaskState::Closed);
        assert_eq!(head.state().stopped_until, Some(at(29, 8, 0)));
        head.step(at(28, 23, 0)).await;
        assert_eq!(head.tasks().len(), 1, "not again this night");
        head.step(at(29, 22, 0)).await;
        assert_eq!(head.tasks().len(), 2);
        let err = head
            .runner
            .stop_by_hand("other", at(29, 22, 30))
            .await
            .unwrap_err();
        assert_eq!(err.0, "orchestrator_not_found");
    }

    #[tokio::test]
    async fn a_dead_agent_restarts_with_the_note() {
        let mut head = Head::new(SESSION, "", "");
        head.step(at(28, 22, 0)).await;
        let first = head.agent().id;
        head.set_state(first, TaskState::Done);
        write_note(&head.paths, "merge", "merged #31; #32 waits on review").unwrap();
        head.kinds();
        head.step(at(28, 23, 0)).await;
        assert_eq!(
            head.store.get_task(first).unwrap().unwrap().state,
            TaskState::Closed
        );
        let t = head.agent();
        assert_ne!(t.id, first);
        assert!(
            t.prompt.contains("merged #31; #32 waits on review"),
            "{}",
            t.prompt
        );
        assert!(
            t.prompt.contains(&Task::agent_name_for(first)),
            "{}",
            t.prompt
        );
        assert!(t.prompt.contains("`pastor watch --now`"), "{}", t.prompt);
        assert_eq!(head.kinds(), ["orchestrator.restarted"]);
        assert_eq!(
            head.state().last_result,
            Some(format!("restarted: after {}", Task::agent_name_for(first)))
        );
    }

    #[tokio::test]
    async fn restarts_are_capped_at_three_an_hour() {
        let mut head = Head::new(SESSION, "", "");
        head.step(at(28, 22, 0)).await;
        for m in [1, 2, 3] {
            head.set_state(head.agent().id, TaskState::Failed);
            head.step(at(28, 22, m)).await;
        }
        assert_eq!(head.tasks().len(), 4);
        head.kinds();
        head.set_state(head.agent().id, TaskState::Stale);
        head.step(at(28, 22, 30)).await;
        head.step(at(28, 22, 40)).await;
        assert_eq!(head.tasks().len(), 4);
        assert_eq!(head.kinds(), ["orchestrator.held"], "held once");
        head.step(at(28, 23, 0)).await;
        assert_eq!(
            head.tasks().len(),
            4,
            "the first restart is not an hour old yet"
        );
        head.step(at(28, 23, 1)).await;
        assert_eq!(head.tasks().len(), 5);
        assert_eq!(head.kinds(), ["orchestrator.restarted"]);
    }

    #[tokio::test]
    async fn a_quota_error_waits_for_the_reset_then_restarts() {
        let mut head = Head::new(SESSION, "", "");
        head.step(at(28, 22, 0)).await;
        let id = head.agent().id;
        let reset = at(29, 1, 0);
        head.store.note_pane_tail(
            id,
            &format!(
                "watching...\nClaude AI usage limit reached|{}\n",
                reset.timestamp()
            ),
        );
        head.set_state(id, TaskState::Done);
        head.kinds();
        head.step(at(28, 23, 0)).await;
        assert_eq!(head.kinds(), ["orchestrator.quota"]);
        assert_eq!(head.state().quota_until, Some(reset));
        assert_eq!(head.tasks().len(), 1);
        assert_eq!(
            head.runner.statuses(at(28, 23, 0))[0].state,
            "waiting for quota"
        );
        head.step(at(29, 0, 59)).await;
        assert_eq!(head.tasks().len(), 1);
        head.step(at(29, 1, 0)).await;
        assert_eq!(head.tasks().len(), 2);
        assert_eq!(head.kinds(), ["orchestrator.restarted"]);
        assert_eq!(head.state().quota_until, None);
    }

    #[tokio::test]
    async fn a_session_and_a_scheduled_orchestrator_share_the_limit() {
        let head = Head::new(SESSION, "", "");
        std::fs::write(head.dir.join("other.toml"), scheduled()).unwrap();
        std::fs::write(head.dir.join("pre.sh"), "#!/bin/sh\necho 'PR #31'\n").unwrap();
        head.runner.reload(Utc::now());
        let other = head.runner.runnable("other").unwrap();
        let mut events = head.runner.events.subscribe();
        // A scheduled agent works: the session due on its hours is held.
        let started = head.runner.run_now(&other).await;
        assert_eq!(started.outcome, RunOutcome::Started, "{started:?}");
        head.step(at(28, 22, 0)).await;
        head.step(at(28, 22, 1)).await;
        assert!(head.state().session.is_none());
        assert_eq!(head.runner.statuses(at(28, 22, 1))[0].state, "held");
        let err = head
            .runner
            .start_by_hand("merge", at(28, 22, 2))
            .await
            .unwrap_err();
        assert_eq!(err.0, "orchestrator_held");
        // It ends: the session starts on the next tick.
        head.set_state(started.task.unwrap(), TaskState::Done);
        head.runner.finish_now(&other).await;
        head.step(at(28, 22, 5)).await;
        assert!(head.state().session.is_some());
        let mut seen = Vec::new();
        while let Ok(e) = events.try_recv() {
            if !e.kind.starts_with("orchestrator.") {
                continue;
            }
            let who = e.detail.as_ref().unwrap()["orchestrator"]
                .as_str()
                .unwrap()
                .to_string();
            seen.push(format!("{who} {}", e.kind));
        }
        assert_eq!(
            seen,
            [
                "other orchestrator.started",
                "merge orchestrator.held",
                "merge orchestrator.started"
            ]
        );
        // The session runs: a scheduled run does its pre script and is held.
        let held = head.runner.run_now(&other).await;
        assert_eq!(held.outcome, RunOutcome::Held, "{held:?}");
        assert_eq!(held.lines, ["PR #31"]);
        // Between agents, on a quota wait, it keeps its slot.
        let id = head.agent().id;
        head.store
            .note_pane_tail(id, "You've hit your limit · resets 3am (Europe/Lisbon)");
        head.set_state(id, TaskState::Done);
        head.step(at(28, 23, 0)).await;
        assert!(head.state().quota_until.is_some());
        let held = head.runner.run_now(&other).await;
        assert_eq!(held.outcome, RunOutcome::Held, "{held:?}");
    }

    /// Every orchestrator file the docs show (a `toml` fence whose first line
    /// names a file under `orchestrators/`) is one the head would load, and
    /// every pre script they show (a `sh` fence starting `#!/bin/sh`) is one
    /// `sh -n` reads, so a copied example runs rather than failing on a key
    /// or a quote.
    #[test]
    fn the_docs_orchestrator_examples_parse() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        let (mut files, mut scripts) = (0, 0);
        for doc in [
            "docs/manual.md",
            "docs/website/content/docs/concepts/orchestrators.md",
            "docs/website/content/docs/examples/overnight.md",
        ] {
            let text = std::fs::read_to_string(repo.join(doc)).unwrap();
            for fence in text.split("```").skip(1).step_by(2) {
                let (lang, body) = fence.split_once('\n').unwrap_or((fence, ""));
                let first = body.lines().next().unwrap_or("");
                if lang == "toml" && first.starts_with("# ~/.config/pastor/orchestrators/") {
                    Orchestrator::parse(body, "example", Path::new("/o"))
                        .unwrap_or_else(|e| panic!("{doc}: {first}: {e}"));
                    files += 1;
                } else if lang == "sh" && first == "#!/bin/sh" {
                    let out = std::process::Command::new("sh")
                        .args(["-n", "-c", body])
                        .output()
                        .unwrap();
                    assert!(
                        out.status.success(),
                        "{doc}: {}",
                        String::from_utf8_lossy(&out.stderr)
                    );
                    scripts += 1;
                }
            }
        }
        // The manual's merge example, the page's, and the night watch.
        assert!(
            files >= 3 && scripts >= 2,
            "{files} files, {scripts} scripts"
        );
    }
}
