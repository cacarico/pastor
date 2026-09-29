use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::herdr::AgentStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    Queued,
    Starting,
    Running,
    Blocked,
    Done,
    Stale,
    Failed,
    Closed,
    /// A `low` Claude task a `critical` one with `preempt` took the slot of:
    /// its pane is closed, its worktree kept, and it waits first among the
    /// `low` tasks, pinned to its machine, to resume its session there
    /// (`claude --resume`) when a slot frees.
    Paused,
}

/// States whose task holds a pane on its machine. Kept next to
/// `occupies_pane` so the two stay in sync; `Store::tasks_on_machine` filters
/// on this list in SQL instead of loading every historical task.
pub const PANE_OWNING_STATES: [TaskState; 5] = [
    TaskState::Starting,
    TaskState::Running,
    TaskState::Blocked,
    TaskState::Done,
    TaskState::Stale,
];

/// The states a task can be in while it still needs pastor or a human:
/// what `pastor task list` shows by default. Done, failed, stale and closed tasks
/// are finished; they appear only with `--all` (or `--done` for done ones).
pub const LIVE_STATES: [TaskState; 5] = [
    TaskState::Queued,
    TaskState::Starting,
    TaskState::Running,
    TaskState::Blocked,
    TaskState::Paused,
];

impl TaskState {
    pub fn occupies_pane(&self) -> bool {
        matches!(
            self,
            TaskState::Starting
                | TaskState::Running
                | TaskState::Blocked
                | TaskState::Done
                | TaskState::Stale
        )
    }
    /// May `pastor task retry` start this task over? Only a task that ended
    /// without its work done, or one that ran past its timeout.
    pub fn is_retryable(&self) -> bool {
        matches!(self, TaskState::Failed | TaskState::Stale)
    }
    /// May `pastor task prune` delete rows in this state? Only finished ones;
    /// anything still queued or working would lose its bookkeeping.
    pub fn is_prunable(&self) -> bool {
        matches!(
            self,
            TaskState::Done | TaskState::Failed | TaskState::Closed
        )
    }
    pub fn is_open(&self) -> bool {
        !matches!(self, TaskState::Failed | TaskState::Closed)
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskState::Queued => "queued",
            TaskState::Starting => "starting",
            TaskState::Running => "running",
            TaskState::Blocked => "blocked",
            TaskState::Done => "done",
            TaskState::Stale => "stale",
            TaskState::Failed => "failed",
            TaskState::Closed => "closed",
            TaskState::Paused => "paused",
        }
    }
}

impl std::fmt::Display for TaskState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for TaskState {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        serde_json::from_value(Value::String(s.to_string()))
            .map_err(|_| format!("unknown state {s}"))
    }
}

/// A queued task's level: dispatch takes queued tasks by level, highest
/// first, then by position, then by age (`Store::queued_tasks`). Settled
/// when the task is queued (`Defaults::resolve_priority`) and stored on it.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    Low,
    #[default]
    Normal,
    High,
    Critical,
}

/// The code of a level that is not one of `Priority`'s.
pub const UNKNOWN_PRIORITY: &str = "unknown_priority";

impl Priority {
    pub const ALL: [Priority; 4] = [
        Priority::Low,
        Priority::Normal,
        Priority::High,
        Priority::Critical,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Priority::Low => "low",
            Priority::Normal => "normal",
            Priority::High => "high",
            Priority::Critical => "critical",
        }
    }

    /// The level a queued task ages to after `age_after`: one up, never
    /// past `high`, so ageing never makes a task critical and lets it burst.
    /// `None` at `high` and `critical`, which do not age.
    pub fn aged(self) -> Option<Priority> {
        match self {
            Priority::Low => Some(Priority::Normal),
            Priority::Normal => Some(Priority::High),
            Priority::High | Priority::Critical => None,
        }
    }
}

impl std::fmt::Display for Priority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Priority {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        Priority::ALL
            .into_iter()
            .find(|p| p.as_str() == s)
            .ok_or_else(|| format!("unknown priority {s:?}; use low, normal, high or critical"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchSpec {
    pub agent: String,
    #[serde(default)]
    pub agent_args: Vec<String>,
    /// Tool patterns the agent may use without asking, and those it must
    /// never use, as resolved when the task was queued
    /// (`Defaults::resolve_agent`); dispatch turns them into the agent's
    /// own flags (`Agents::launch_args`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub worktree: bool,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub machine: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// The worktree this task works in, recorded by dispatch once herdr
    /// has made (or reopened) it, so a retry knows the checkout is this
    /// task's own. `None` until then, and on a task without a worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout: Option<Box<Checkout>>,
    /// The checkout of the failed task this one retries, set only by
    /// `Store::insert_retry` from that task's `checkout`. Dispatch reopens it
    /// only when it is still on disk at the same path, that task's agent is
    /// gone and no other agent is in its workspace (`dispatch::reopenable`);
    /// otherwise the retry gets a new branch and worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reopen: Option<Box<Reopen>>,
    /// What the task asked for about its agent, and where its `agent` and
    /// `agent_args` came from. Kept so dispatch can settle the agent again
    /// on the machine it picks (`Fleet::dispatch_queued`), since a machine
    /// can set its own. `None` on a task from a client or head that
    /// predates it: that task keeps the agent it was queued with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_source: Option<Box<AgentSource>>,
    /// Where on its machine's herdr the agent's pane goes (`Place`). Left out
    /// of the JSON when it is the default, so a spec from before it reads as
    /// `repo`.
    #[serde(default, skip_serializing_if = "Place::is_repo")]
    pub place: Place,
    /// The Claude session the agent was started on (`--session-id`),
    /// recorded by dispatch so `pastor task attach` can resume it once the
    /// pane is gone. `None` for another kind of agent, and when the task's
    /// own args pick the session (`picks_session`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// The label of the workspace dispatch makes for the task: the
    /// template, where it came from, and what the workspace was called.
    #[serde(default, skip_serializing_if = "WorkspaceLabel::is_unset")]
    pub label: WorkspaceLabel,
    /// Whether pastor asks the agent for a summary when it sends the
    /// prompt, and whether the task needs one to succeed (`SummaryMode`),
    /// as resolved when the task was queued. Left out of the JSON when it
    /// is `ask`, so a spec from before it reads as `ask`.
    #[serde(default, skip_serializing_if = "SummaryMode::is_ask")]
    pub summary: SummaryMode,
    /// The directory dispatch started a repo-less task in
    /// (`dispatch::no_repo_dir`): `~/pastor-tasks`, or the machine's home if
    /// that folder could not be made. Recorded so `pastor task attach` and a
    /// paused task's resume both go back to the same place rather than
    /// asking again, which can answer differently once the folder is fixed.
    /// `None` for a task with a `repo`, and on a task from before this field
    /// existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// `task run --now`: dispatch starts the task at once on its pinned
    /// machine, past that machine's `max_agents`, `job_slots`, `burst` and
    /// its flock's number there (`dispatch::now_machine`). Only the head
    /// sets it, from `Run::now`; a retry drops it. Left out of the JSON when
    /// false, so a spec from before it reads as a plain task.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub now: bool,
}

/// The label template a task's own workspace gets when no layer sets one.
/// With many flocks on one machine, the flock is what tells them apart in
/// herdr's sidebar.
pub const DEFAULT_LABEL: &str = "{{ flock }}/{{ task.id }}";

/// `WorkspaceLabel::note` for a task whose pane went into a workspace it
/// did not make, which keeps its own label.
pub const JOINED_WORKSPACE: &str = "joined workspace";

/// A task's workspace label. Only the workspace is named this way: the
/// herdr agent stays `t-N`, since pastor finds its agents by that name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceLabel {
    /// `--label`, a job's `[dispatch] label`, the flock's or `[defaults]`,
    /// settled when the task is queued; `None` is `DEFAULT_LABEL`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
    /// Where `template` came from, labelled like `AgentSource::agent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// The label of the workspace the task's pane is in, set by dispatch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Why `name` is not the template rendered: the task joined a
    /// workspace, or the template rendered to something herdr should not
    /// show and dispatch fell back to `t-N`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl WorkspaceLabel {
    pub fn is_unset(&self) -> bool {
        *self == WorkspaceLabel::default()
    }
}

/// Refuse a label template that could never name a workspace: bad
/// `{{ }}` syntax, a placeholder other than the five a label knows, an
/// empty one or a control character. Run where a template is written
/// (`--label`, a job file, `flock.toml`, `pastor.toml`).
pub fn check_label(template: &str) -> Result<(), String> {
    if template.trim().is_empty() {
        return Err("label must not be empty".into());
    }
    if template.chars().any(char::is_control) {
        return Err("label must not hold a control character".into());
    }
    for path in crate::template::placeholders(template).map_err(|e| format!("label: {e}"))? {
        if !LABEL_PLACEHOLDERS.contains(&path.as_str()) {
            return Err(format!(
                "label: unknown placeholder {{{{ {path} }}}}; use task.id, flock, machine, job or item.key"
            ));
        }
    }
    Ok(())
}

const LABEL_PLACEHOLDERS: [&str; 5] = ["task.id", "flock", "machine", "job", "item.key"];

/// `template` (or `DEFAULT_LABEL`) rendered for `task`. A placeholder with
/// nothing to fill it (no job, no item, no flock) renders empty, and
/// leading and trailing spaces and slashes are dropped, so
/// `{{ job }}/{{ task.id }}` reads `t-N` for a task with no job. Refused
/// when the result is empty or holds a control character (an item's key
/// can): the caller then names the workspace `t-N`.
pub fn render_label(template: Option<&str>, task: &Task) -> Result<String, String> {
    let ctx = serde_json::json!({
        "task": {"id": task.display_id()},
        "flock": task.flock,
        "machine": task.machine,
        "job": if task.from_job() { task.job.as_str() } else { "" },
        "item": {"key": task.item.get("key")},
    });
    let text = crate::template::render(template.unwrap_or(DEFAULT_LABEL), &ctx)?.text;
    if text.chars().any(char::is_control) {
        return Err("it holds a control character".into());
    }
    let text = text.trim_matches(|c: char| c.is_whitespace() || c == '/');
    if text.is_empty() {
        return Err("it renders empty".into());
    }
    Ok(text.to_string())
}

/// The `summary` setting: `task run --summary`, a job's `[dispatch]
/// summary`, a flock's or `[defaults]`, the most specific first
/// (`Defaults::resolve_summary`). `ask` adds `SUMMARY_ASK` to every prompt
/// pastor sends; `require` adds it and `SUMMARY_REQUIRE`, refuses the
/// agent's own `task done` without a summary and fails a task that goes
/// idle without one; `off` adds nothing and requires nothing.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum SummaryMode {
    /// Add the line that asks for a summary to the prompt
    #[default]
    Ask,
    /// Ask, and fail the task if its agent stops without one
    Require,
    /// Add nothing and require nothing
    Off,
}

/// The paragraph pastor adds to a task's prompt when it sends it, unless
/// the task's `summary` is `off`. Not stored in the task's prompt.
pub const SUMMARY_ASK: &str = "When you finish, run `pastor task done --summary-file -` with a short summary on stdin: first line `done`, `partial`, `blocked` or `nothing to do`; then up to five short lines: what changed, where (branch, PR, files or notes), what is left.";

/// Added after `SUMMARY_ASK` for a task whose `summary` is `require`.
pub const SUMMARY_REQUIRE: &str = "pastor fails this task if you stop without one.";

/// The error of a `require` task whose agent went idle without a summary.
pub const STOPPED_WITHOUT_SUMMARY: &str = "stopped without a summary";

/// The code of an agent's own `task done` without a summary on a
/// `require` task.
pub const SUMMARY_REQUIRED: &str = "summary_required";

/// The text of the round a person ends by hand (`task done t-N` from
/// outside the task's pane) on a `require` task, with no summary.
pub const ENDED_BY_HAND: &str = "no summary (ended by hand)";

impl SummaryMode {
    pub const ALL: [SummaryMode; 3] = [SummaryMode::Ask, SummaryMode::Require, SummaryMode::Off];

    pub fn as_str(&self) -> &'static str {
        match self {
            SummaryMode::Ask => "ask",
            SummaryMode::Require => "require",
            SummaryMode::Off => "off",
        }
    }

    pub fn is_ask(&self) -> bool {
        *self == SummaryMode::Ask
    }

    /// The paragraph that asks for a summary, `None` for `off`.
    pub fn ask_line(&self) -> Option<String> {
        match self {
            SummaryMode::Ask => Some(SUMMARY_ASK.to_string()),
            SummaryMode::Require => Some(format!("{SUMMARY_ASK} {SUMMARY_REQUIRE}")),
            SummaryMode::Off => None,
        }
    }

    /// What `task describe` says about it.
    pub fn describe(&self) -> String {
        match self {
            SummaryMode::Ask => "ask (line added to the prompt)".into(),
            SummaryMode::Require => "require (line added to the prompt; fails without one)".into(),
            SummaryMode::Off => "off (nothing added to the prompt)".into(),
        }
    }
}

impl std::fmt::Display for SummaryMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for SummaryMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        SummaryMode::ALL
            .into_iter()
            .find(|m| m.as_str() == s)
            .ok_or_else(|| format!("unknown summary {s:?}; use ask, require or off"))
    }
}

/// Where dispatch puts a task's pane: `--place`, a job's `[dispatch] place`
/// or `[defaults] place`. Written as `repo`, `own`, `pastor` or
/// `pane:<workspace>`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Place {
    /// Under the repo it works on: a worktree task in its own worktree
    /// workspace (herdr shows it under the repo's), a task whose repo a
    /// workspace already shows in a new pane there, anything else in its
    /// own workspace.
    #[default]
    Repo,
    /// Always a new workspace named after the task.
    Own,
    /// A pane in the machine's one workspace named `pastor`, made on first
    /// use. A worktree is still made on disk.
    Pastor,
    /// A pane in the herdr workspace with this label; refused if the machine
    /// has none.
    Pane(String),
}

impl Place {
    pub fn is_repo(&self) -> bool {
        *self == Place::Repo
    }

    /// Does the pane go into a workspace the task did not make, whatever
    /// the machine shows? A worktree task placed so has no workspace of its
    /// own on its checkout.
    pub fn is_shared(&self) -> bool {
        matches!(self, Place::Pastor | Place::Pane(_))
    }
}

impl std::str::FromStr for Place {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "repo" => Ok(Place::Repo),
            "own" => Ok(Place::Own),
            "pastor" => Ok(Place::Pastor),
            _ => match s.strip_prefix("pane:") {
                Some(ws) if !ws.trim().is_empty() => Ok(Place::Pane(ws.to_string())),
                Some(_) => Err("place pane: needs a workspace, like pane:work".into()),
                None => Err(format!(
                    "unknown place {s}; use repo, own, pastor or pane:<workspace>"
                )),
            },
        }
    }
}

impl std::fmt::Display for Place {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Place::Repo => f.write_str("repo"),
            Place::Own => f.write_str("own"),
            Place::Pastor => f.write_str("pastor"),
            Place::Pane(ws) => write!(f, "pane:{ws}"),
        }
    }
}

impl TryFrom<String> for Place {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        s.parse()
    }
}

impl From<Place> for String {
    fn from(p: Place) -> String {
        p.to_string()
    }
}

/// See `DispatchSpec::agent_source`. The labels read like `machine own`,
/// `flock personal`, `defaults`, `task run` or `job <name>`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSource {
    pub ask: crate::config::AgentChoice,
    pub agent: String,
    /// `None` when the agent runs with no args because no layer set any
    /// for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_args: Option<String>,
    /// The `[models]` name the task runs, whose args lead `agent_args`;
    /// `None` when no layer names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Where `model` came from, labelled like `agent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_from: Option<String>,
    /// The `[models]` names the task may fall back to, in order, as
    /// settled (`Models::fallback`); empty when it has none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback: Vec<String>,
    /// Where `fallback` came from, labelled like `agent`; set too when that
    /// layer's list is `[]`, and `None` when no layer sets one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_from: Option<String>,
    /// The permission profile the task runs under, whose lists are in the
    /// spec's `allow` and `deny`; `None` when no layer names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Where `profile` came from, labelled like `agent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_from: Option<String>,
    /// Where the spec's `timeout_secs` came from, labelled like `agent`;
    /// `None` for `[defaults]`, or a task queued before flocks had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_from: Option<String>,
    /// Where the spec's `place` came from, like `timeout_from`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place_from: Option<String>,
}

/// A worktree herdr made for a task: its branch and where it is on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkout {
    pub branch: String,
    pub path: String,
    /// herdr's `worktree.open` answered a workspace already showing the
    /// checkout when dispatch reached it: the task joined a workspace pastor
    /// did not make, and removing the checkout would close it. Missing from
    /// rows written before it, which count as pastor's own.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub already_open: bool,
}

/// A checkout a retry may go back to, and the agent that owned it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reopen {
    pub branch: String,
    pub path: String,
    pub agent: String,
}

impl DispatchSpec {
    /// The permission profile the task runs under, if it runs one.
    pub fn profile(&self) -> Option<&str> {
        self.agent_source.as_ref()?.profile.as_deref()
    }
}

fn default_timeout() -> u64 {
    2 * 60 * 60
}

/// The most characters a task summary keeps (`cap_summary`).
pub const SUMMARY_MAX: usize = 2000;

/// How a round of a task ended, from the first line of its summary
/// (`Outcome::parse`). `NoSummary` is a round that ended with none, whose
/// row holds the pane's last lines instead; `Unknown` is a summary whose
/// first line names none of the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    #[serde(rename = "done")]
    Done,
    #[serde(rename = "partial")]
    Partial,
    #[serde(rename = "blocked")]
    Blocked,
    #[serde(rename = "nothing to do")]
    NothingToDo,
    #[serde(rename = "no summary")]
    NoSummary,
    #[serde(rename = "unknown")]
    Unknown,
}

impl Outcome {
    /// The outcomes an agent may name, longest first so `nothing to do`
    /// is not read as anything shorter.
    const NAMED: [Outcome; 4] = [
        Outcome::NothingToDo,
        Outcome::Partial,
        Outcome::Blocked,
        Outcome::Done,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Outcome::Done => "done",
            Outcome::Partial => "partial",
            Outcome::Blocked => "blocked",
            Outcome::NothingToDo => "nothing to do",
            Outcome::NoSummary => "no summary",
            Outcome::Unknown => "unknown",
        }
    }

    /// The outcome a summary's first line names, ignoring case: the line
    /// is the outcome, or starts with it and then something that is not a
    /// letter (`done: pushed`, `Partial - tests left`). `Unknown` otherwise.
    pub fn parse(summary: &str) -> Outcome {
        let first = summary.trim_start().lines().next().unwrap_or("").trim();
        let first = first.to_lowercase();
        Outcome::NAMED
            .into_iter()
            .find(|o| {
                first.strip_prefix(o.as_str()).is_some_and(|rest| {
                    !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_')
                })
            })
            .unwrap_or(Outcome::Unknown)
    }
}

impl std::str::FromStr for Outcome {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "done" => Outcome::Done,
            "partial" => Outcome::Partial,
            "blocked" => Outcome::Blocked,
            "nothing to do" => Outcome::NothingToDo,
            "no summary" => Outcome::NoSummary,
            "unknown" => Outcome::Unknown,
            other => return Err(format!("unknown outcome {other:?}")),
        })
    }
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Who wrote a summary: the agent (`task done --summary`), or pastor from
/// the pane's last lines when the round ended with none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SummarySource {
    Agent,
    Pane,
}

impl SummarySource {
    pub fn as_str(&self) -> &'static str {
        match self {
            SummarySource::Agent => "agent",
            SummarySource::Pane => "pane",
        }
    }
}

/// How one round of a task ended: from the prompt (or the input that
/// reopened it) to `done` or `failed`. One row per round in
/// `task_summaries`, numbered from 1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSummary {
    pub round: u32,
    pub outcome: Outcome,
    pub text: String,
    pub source: SummarySource,
    pub at: DateTime<Utc>,
}

/// `text` trimmed and cut to `SUMMARY_MAX` characters, its start kept.
pub fn cap_summary(text: &str) -> String {
    text.trim().chars().take(SUMMARY_MAX).collect()
}

/// The last `SUMMARY_MAX` characters of a pane's text, trimmed: what a
/// round that ended with no summary keeps.
pub fn cap_pane_tail(text: &str) -> String {
    let text = text.trim();
    let n = text.chars().count();
    text.chars().skip(n.saturating_sub(SUMMARY_MAX)).collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: i64,
    pub job: String,
    pub item: Value,
    pub prompt: String,
    pub spec: DispatchSpec,
    pub machine: Option<String>,
    pub workspace_id: Option<String>,
    pub pane_id: Option<String>,
    pub agent_name: Option<String>,
    pub state: TaskState,
    pub error: Option<String>,
    /// herdr's `state_change_seq` for this task's agent when it was given the
    /// task (the reply to `agent.prompt`), or when it last finished: the
    /// baseline a completion must move past. The name predates herdr 0.9.1,
    /// which has no `completion_seq`; the column is kept to keep the schema.
    /// `None` on rows written before this was recorded, read as 0.
    pub last_completion_seq: Option<u64>,
    /// The agent has never seen the prompt: herdr refused it at dispatch with
    /// `agent_blocked`, or reconcile adopted the agent while it was still
    /// launching. The machine sends it once the agent is past both.
    #[serde(default)]
    pub prompt_pending: bool,
    /// pastor has seen this task's agent `working` or `blocked` since the
    /// prompt went in (or since it last finished). A newer `state_change_seq`
    /// alone does not prove work: herdr stamps one on every change of the
    /// detected state, `unknown` included, so `idle -> unknown -> idle` moves
    /// it too. herdr's `agent.prompt --wait` gates on the same activity
    /// (`prompt_activity_statuses` in `src/api/wait.rs`). Stored with the
    /// task, so an actor that replaces the one that saw the work (a daemon
    /// restart, a flock or settings reload) still completes it once the agent
    /// settles idle. Left out of the JSON the CLI prints.
    #[serde(skip)]
    pub activity_seen: bool,
    /// The agent said it is finished (`pastor task done` from its own pane).
    /// The task is `Done` and stays so while the agent finishes its last
    /// turn: a `working` or `blocked` sighting no longer reopens it, and
    /// auto-close takes it once the agent is idle after `close_done_after`.
    /// Cleared when someone types into the pane (`pastor task send`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ended: bool,
    /// The task this one retries (`pastor task retry`). A retry is a new row
    /// with a new id, because the old agent `t-<id>` may still be alive.
    #[serde(default)]
    pub retry_of: Option<i64>,
    /// The flock whose machines may take this task. `None` only on a row
    /// from before flocks until `Store::adopt_default_flock` fills it in;
    /// it reads as the default flock.
    #[serde(default)]
    pub flock: Option<String>,
    /// The task's level in the queue. `normal` on a row from before it.
    #[serde(default)]
    pub priority: Priority,
    /// Where `priority` came from, labelled like `AgentSource::agent`
    /// (`task run`, `job <name>`, `machine <name>`, `flock <name>`,
    /// `defaults`, `task priority`); `None` when no layer set one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_from: Option<String>,
    /// The task's position among queued tasks of its level, lowest first;
    /// its id unless something has moved it.
    #[serde(default)]
    pub queue_pos: i64,
    /// The level the task had before it aged (`Store::age_queued`): set on
    /// its first step up, kept through later ones, cleared when someone sets
    /// its level by hand. `None` on a task that has not aged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aged_from: Option<Priority>,
    /// When the task last aged a level; its next step is `age_after` from
    /// this, or from when it was queued before its first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aged_at: Option<DateTime<Utc>>,
    /// What the head lets the task's agent change (`TaskRole`). `agent` on
    /// every row from before roles.
    #[serde(default)]
    pub role: TaskRole,
    /// One line on what the task is about, fixed when it was queued: `task
    /// run --description`, or its job's `[dispatch] description` rendered
    /// for its item. `None` when neither said anything, and on rows from
    /// before it: those read as the prompt's first line
    /// (`description_text`).
    #[serde(default)]
    pub description: Option<String>,
    /// Whether the task may pause a `low` one to start, and whether it was
    /// paused itself (`Preemption`).
    #[serde(flatten, default)]
    pub pause: Preemption,
    /// How the task's last round ended (`TaskSummary`), on a task that is
    /// done, failed or closed and has one. Not a column: the store reads it
    /// from `task_summaries` with the row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<TaskSummary>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

/// The code of `--preempt` (or a job's `preempt`) on a task below critical.
pub const PREEMPT_NEEDS_CRITICAL: &str = "preempt_needs_critical";

/// How long a task that resumed after a pause is safe from another: long
/// enough for it to get back into its work before a second critical task
/// takes its slot again.
pub const RESUME_GRACE: chrono::Duration = chrono::Duration::minutes(10);

/// What a paused task's agent is told once its session is open again: it
/// was stopped mid-turn, and the prompt is already in its conversation.
/// The text dispatch sends a task's agent: its prompt, then, after a
/// blank line, the paragraph its `summary` setting adds (`ask_line`).
pub fn prompt_to_send(task: &Task) -> String {
    match task.spec.summary.ask_line() {
        Some(line) => format!("{}\n\n{line}", task.prompt.trim_end()),
        None => task.prompt.clone(),
    }
}

pub const RESUME_PROMPT: &str = "pastor paused this session for a critical task and has now resumed it; carry on where you left off.";

/// A task's part in pausing: whether it may pause a `low` task to start
/// (`task run --preempt`, a job's `[dispatch] preempt`), and, on a task that
/// was paused, when, for which task, and when it last resumed. Flattened
/// into the task's JSON; each field is left out while unset.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preemption {
    /// Only ever set on a `critical` task: `task run` and `task priority`
    /// refuse it below, and a job's tasks keep it only when they settle at
    /// critical.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub preempt: bool,
    /// When the task was last paused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_at: Option<DateTime<Utc>>,
    /// The task that paused it last.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_for: Option<i64>,
    /// When it last resumed after a pause.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_at: Option<DateTime<Utc>>,
}

/// Why `Task::pausable` says no.
pub fn why_not_pausable(task: &Task, kind: &str, now: DateTime<Utc>) -> Option<&'static str> {
    if task.state != TaskState::Running {
        return Some("only a running task can be paused");
    }
    if task.priority != Priority::Low {
        return Some("only a low task can be paused");
    }
    if kind != "claude" {
        return Some("only a Claude task can be paused");
    }
    if task.spec.session_id.is_none() {
        return Some("it recorded no Claude session to resume");
    }
    if task.ended {
        return Some("its agent said it is finished");
    }
    if task
        .pause
        .resumed_at
        .is_some_and(|at| now - at < RESUME_GRACE)
    {
        return Some("it resumed from a pause a moment ago");
    }
    None
}

impl Task {
    /// May a critical task with `preempt` pause this one? A running `low`
    /// task of an agent of kind `kind` that is `claude`, with a recorded
    /// session to resume, not ended, and not resumed within `RESUME_GRACE`.
    pub fn pausable(&self, kind: &str, now: DateTime<Utc>) -> bool {
        why_not_pausable(self, kind, now).is_none()
    }

    /// The machine the task must run on: the one it is paused on, else the
    /// one its spec pins.
    pub fn pinned_machine(&self) -> Option<&str> {
        match self.state {
            TaskState::Paused => self.machine.as_deref(),
            _ => self.spec.machine.as_deref(),
        }
    }

    /// Whether a job made this task; `pastor task run` tasks carry the job
    /// name `run`. Only these take a machine's job slots.
    pub fn from_job(&self) -> bool {
        self.job != "run"
    }

    pub fn display_id(&self) -> String {
        format!("t-{}", self.id)
    }
    pub fn agent_name_for(id: i64) -> String {
        format!("t-{id}")
    }
    /// The `[models]` name the task runs, if it runs one.
    pub fn model(&self) -> Option<&str> {
        self.spec.agent_source.as_ref()?.model.as_deref()
    }
    /// The `[models]` names the task may fall back to, in order.
    pub fn fallback(&self) -> &[String] {
        self.spec
            .agent_source
            .as_ref()
            .map_or(&[], |s| s.fallback.as_slice())
    }
    /// The permission profile the task runs under, if it runs one.
    pub fn profile(&self) -> Option<&str> {
        self.spec.profile()
    }
    /// The task's description, else its prompt's first line, trimmed.
    pub fn description_text(&self) -> String {
        match &self.description {
            Some(d) => d.clone(),
            None => self.prompt.lines().next().unwrap_or("").trim().to_string(),
        }
    }
    /// Where `description_text` came from: `--description` for a one-off
    /// task, `job <name>` for a job's, else `the prompt`.
    pub fn description_from(&self) -> String {
        match (&self.description, self.job.as_str()) {
            (None, _) => "the prompt".into(),
            (Some(_), "run") => "--description".into(),
            (Some(_), job) => format!("job {job}"),
        }
    }
    /// The task as `--json` prints it: its row, with `model` and `profile`
    /// beside it, and its description always a string, with where it came
    /// from.
    pub fn to_json(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or(Value::Null);
        if let Value::Object(o) = &mut v {
            o.insert(
                "model".into(),
                self.model().map_or(Value::Null, Value::from),
            );
            o.insert("fallback".into(), self.fallback().into());
            o.insert(
                "profile".into(),
                self.profile().map_or(Value::Null, Value::from),
            );
            o.insert("description".into(), self.description_text().into());
            o.insert("description_from".into(), self.description_from().into());
        }
        v
    }
}

/// What a task's agent may change through the head. A guard against an
/// agent's mistakes, not a boundary: the agent runs as the same user as
/// pastor.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum TaskRole {
    /// Reads, and `task done` for its own task; everything else is refused
    /// unless `agents_change_fleet` is on.
    #[default]
    Agent,
    /// Also runs, retries, types into and closes tasks and enables and
    /// disables jobs (`IpcRequest::orchestrator_may`). Only a person makes
    /// one: `task run --role orchestrator` from outside any task.
    Orchestrator,
}

impl TaskRole {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskRole::Agent => "agent",
            TaskRole::Orchestrator => "orchestrator",
        }
    }

    pub fn is_agent(&self) -> bool {
        *self == TaskRole::Agent
    }
}

impl std::fmt::Display for TaskRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for TaskRole {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "agent" => Ok(TaskRole::Agent),
            "orchestrator" => Ok(TaskRole::Orchestrator),
            other => Err(format!("unknown role {other:?}; agent or orchestrator")),
        }
    }
}

pub fn parse_task_id(s: &str) -> Option<i64> {
    s.strip_prefix("t-").unwrap_or(s).parse().ok()
}

/// What to say when `s` is not something `parse_task_id` takes.
pub fn bad_task_id(s: &str) -> String {
    format!("{s} is not a task id; write it like t-12 or 12")
}

/// Claude's flags that choose the session it starts on (`claude --help`).
/// Agent args with any of them keep their own, and pastor records none.
const CLAUDE_SESSION_FLAGS: [&str; 6] = [
    "--session-id",
    "--resume",
    "-r",
    "--continue",
    "-c",
    "--fork-session",
];

/// Do `args` already choose Claude's session, as `--flag value` or
/// `--flag=value`?
pub fn picks_session(args: &[String]) -> bool {
    args.iter().any(|a| {
        let flag = a.split_once('=').map_or(a.as_str(), |(f, _)| f);
        CLAUDE_SESSION_FLAGS.contains(&flag)
    })
}

/// A new random (version 4) UUID for `claude --session-id`, from the
/// kernel's random source. `None` when it cannot be read: the task then
/// starts without a session of pastor's choosing, as before.
pub fn new_session_id() -> Option<String> {
    use std::io::Read;
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .ok()?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Some(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

/// Is `s` a UUID as Claude takes one: 8-4-4-4-12 hex digits. A recorded
/// session goes into a command line on another machine, so attach checks
/// it first.
pub fn is_session_id(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.iter().map(|p| p.len()).eq([8, 4, 4, 4, 12])
        && parts
            .iter()
            .all(|p| p.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observed {
    Status {
        status: AgentStatus,
        /// herdr's server-wide counter, stamped on the agent at each change of
        /// its detected state (idle, working, blocked, unknown). `None` when
        /// the observation carries none, as subscription events do.
        state_change_seq: Option<u64>,
        /// The change that made this idle a completion, in the same sequence.
        /// herdr 0.9.1 never reports it; used when a later herdr does.
        completion_seq: Option<u64>,
    },
    PaneClosed,
    /// The agent's process ended. `agent_idle`: the last status pastor saw
    /// for it was `idle` or `done`, so it exited between turns (a human typed
    /// `/exit` after the work) rather than in the middle of one.
    PaneExited {
        agent_idle: bool,
    },
    /// Dispatch is about to make its first herdr call. Not something herdr reports;
    /// the actor emits it itself so the one legal source transition (only a queued
    /// task may start) lives with the rest of the state machine instead of being
    /// hard-coded where dispatch happens.
    DispatchStarting,
}

/// Did an agent now idle (or done) finish work it was given? herdr 0.9.1 has
/// no completion counter; `agent_status` is idle or done either way (`done`
/// only means nobody has looked at the pane since, and herdr also shows it
/// after `unknown -> idle`). Two things together show work happened: pastor
/// saw the agent `working` or `blocked` after the prompt
/// (`Task::activity_seen`), and `state_change_seq` has moved past the value
/// it had when the prompt went in, so the agent left that state since. The
/// sequence alone is not enough: `unknown` moves it too. herdr's own
/// `agent.prompt --wait` makes the same two checks (`src/api/wait.rs`,
/// `prompt_activity_statuses` then `after_state_change_seq`). A prompt that
/// was never delivered has nothing to complete. A task with no baseline at
/// all is a row from a build that recorded neither; the sequence decides for
/// it. A `completion_seq`, when herdr reports one, is the same sequence
/// stamped only on real completions, so it is preferred and needs no
/// activity.
fn completed_since_prompt(
    task: &Task,
    state_change_seq: Option<u64>,
    completion_seq: Option<u64>,
) -> bool {
    if task.prompt_pending {
        return false;
    }
    let baseline = task.last_completion_seq.unwrap_or(0);
    if let Some(seq) = completion_seq {
        return seq > baseline;
    }
    let active = task.activity_seen || task.last_completion_seq.is_none();
    active && state_change_seq.is_some_and(|seq| seq > baseline)
}

/// What starts an agent's message in its pane: Claude draws `●` (`⏺` in
/// older releases) in the first column, and indents the rest of the message
/// by two spaces.
pub(crate) const MESSAGE_MARKERS: [char; 2] = ['●', '⏺'];

/// The question an agent's last message ends with, if it ends with one.
/// herdr reports an agent that stopped to ask the same way as one that
/// finished, idle, so the pane is the only place the difference shows.
/// `pane` is the tail of the pane; the last message is the last block that
/// starts with a message marker, and the question is its last paragraph
/// when that ends in `?`. What Claude draws after it (turn summary, recap,
/// input box, footer) starts in the first column, which ends the block, and
/// tool output (`⎿`) is not the agent speaking. A pane with no marker (an
/// agent that draws its messages some other way) never has a question.
pub fn trailing_question(pane: &str) -> Option<String> {
    let lines: Vec<&str> = pane.lines().collect();
    let start = lines.iter().rposition(|l| l.starts_with(MESSAGE_MARKERS))?;
    let mut paragraphs: Vec<Vec<&str>> = vec![vec![]];
    for (i, line) in lines[start..].iter().enumerate() {
        let text = if i == 0 {
            line.trim_start_matches(MESSAGE_MARKERS)
        } else if line.trim().is_empty() {
            if paragraphs.last().is_some_and(|p| !p.is_empty()) {
                paragraphs.push(vec![]);
            }
            continue;
        } else if let Some(rest) = line.strip_prefix("  ") {
            rest
        } else {
            break;
        };
        let text = text.trim();
        if !text.starts_with('⎿') {
            paragraphs.last_mut().expect("never empty").push(text);
        }
    }
    let last = paragraphs.iter().rev().find(|p| !p.is_empty())?.join(" ");
    last.trim_end_matches(['*', '_', '`'])
        .ends_with('?')
        .then_some(last)
}

/// Whether the agent's footer says a background shell it started is still
/// running. Claude Code can end its turn with a command (`make check`) left
/// running in the background, draws "1 shell still running" in its footer,
/// and takes the turn up again when the shell ends; herdr reads it as idle
/// all the while. Only the lines after the last input prompt (`❯`) are the
/// footer, so a message that quotes the phrase does not count; a pane with
/// no prompt is read whole.
pub fn background_shell_running(pane: &str) -> bool {
    let lines: Vec<&str> = pane.lines().collect();
    let start = lines
        .iter()
        .rposition(|l| l.trim_start_matches(['\u{a0}', ' ']).starts_with('❯'))
        .map_or(0, |i| i + 1);
    lines[start..].iter().any(|line| {
        let line = line.replace('\u{a0}', " ");
        ["shell still running", "shells still running"]
            .iter()
            .any(|phrase| {
                line.match_indices(phrase).any(|(at, _)| {
                    line[..at]
                        .trim_end()
                        .ends_with(|c: char| c.is_ascii_digit())
                })
            })
    })
}

/// Pure transition. `None` means no change. The settle window for `Done` is the
/// caller's job: it should confirm the agent is still idle after the window.
pub fn next_state(task: &Task, observed: &Observed) -> Option<TaskState> {
    use TaskState::*;
    if !task.state.is_open() {
        return None;
    }
    let to = match observed {
        Observed::DispatchStarting => {
            if task.state == Queued {
                Starting
            } else {
                return None;
            }
        }
        Observed::PaneClosed => Closed,
        Observed::PaneExited { agent_idle } => match task.state {
            Done => Closed,
            // An agent that ends between turns has finished what it was
            // given; one that ends while starting, blocked or working has not.
            // It must also have been seen working since its prompt: a prompt
            // it ignored, or went idle on at once, is not finished work.
            Running if *agent_idle && !task.prompt_pending && task.activity_seen => Done,
            _ => Failed,
        },
        Observed::Status {
            status,
            state_change_seq,
            completion_seq,
        } => match status {
            // The agent said it is finished (`pastor task done`), usually
            // mid-turn: the rest of that turn is not new work, and whatever
            // it shows next leaves the task done until auto-close takes it.
            _ if task.state == Done && task.ended => return None,
            // Stale is sticky: a working/blocked observation after a timeout must not
            // flip the task back to running or blocked, or it would oscillate with the
            // next timeout check. Only a real completion (below) or a pane event moves
            // it on from here.
            AgentStatus::Working if task.state == Stale => return None,
            AgentStatus::Blocked if task.state == Stale => return None,
            AgentStatus::Working => Running,
            AgentStatus::Blocked => Blocked,
            AgentStatus::Unknown => return None,
            AgentStatus::Idle | AgentStatus::Done => {
                if completed_since_prompt(task, *state_change_seq, *completion_seq) {
                    Done
                } else if task.state == Blocked
                    && task.last_completion_seq.is_some()
                    && completion_seq.or(*state_change_seq) == task.last_completion_seq
                {
                    // Blocked on the question it ended its turn with (see
                    // `trailing_question`): the agent still sits at the idle
                    // the block was recorded at, so nobody has answered yet.
                    return None;
                } else if task.state == Blocked {
                    // The human answered the prompt; the agent is idle again but has not
                    // produced completed work since. Treat it as running until it does.
                    Running
                } else if task.state == Starting {
                    // An agent found idle right after being adopted (see
                    // `Actor::reconcile`) proves dispatch reached at least
                    // `agent.start`; treat it as running, the same target a
                    // successful dispatch would have recorded, rather than leaving
                    // it stuck as `Starting` forever (stale only covers
                    // Running).
                    Running
                } else {
                    return None;
                }
            }
        },
    };
    if to == task.state { None } else { Some(to) }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A checkout recorded before `already_open` existed is pastor's own,
    /// and one that is pastor's own writes nothing new.
    #[test]
    fn a_checkout_without_already_open_is_pastor_s_own() {
        let old: Checkout = serde_json::from_str(r#"{"branch":"b","path":"/p"}"#).unwrap();
        assert!(!old.already_open);
        assert_eq!(
            serde_json::to_string(&old).unwrap(),
            r#"{"branch":"b","path":"/p"}"#
        );
        let joined: Checkout =
            serde_json::from_str(r#"{"branch":"b","path":"/p","already_open":true}"#).unwrap();
        assert!(joined.already_open);
    }

    /// A place reads and writes as the words `--place` and the TOML keys
    /// take, and a spec that says nothing is `repo`.
    #[test]
    fn place_round_trips_as_its_word() {
        for word in ["repo", "own", "pastor", "pane:work", "pane:my ws"] {
            let place: Place = word.parse().unwrap();
            assert_eq!(place.to_string(), word);
            let json = serde_json::to_value(&place).unwrap();
            assert_eq!(json, serde_json::json!(word));
            assert_eq!(serde_json::from_value::<Place>(json).unwrap(), place);
        }
        assert!(Place::Pastor.is_shared() && Place::Pane("w".into()).is_shared());
        assert!(!Place::Repo.is_shared() && !Place::Own.is_shared());
        for bad in ["", "pane:", "pane: ", "mine", "Repo"] {
            assert!(bad.parse::<Place>().is_err(), "{bad}");
        }
        let old: DispatchSpec =
            serde_json::from_value(serde_json::json!({"agent": "claude"})).unwrap();
        assert_eq!(old.place, Place::Repo);
        let text = serde_json::to_string(&old).unwrap();
        assert!(!text.contains("place"), "the default is left out: {text}");
    }

    /// The built-in label is `<flock>/t-N`; each placeholder renders from
    /// the task, and one with nothing to fill it leaves no stray slash.
    #[test]
    fn a_label_renders_every_placeholder() {
        let mut t = task(TaskState::Queued, None);
        t.id = 285;
        t.flock = Some("personal".into());
        t.machine = Some("pi-1".into());
        t.job = "board".into();
        t.item = serde_json::json!({"key": "card-9"});
        assert_eq!(render_label(None, &t).unwrap(), "personal/t-285");
        let one = |template: &str, t: &Task| render_label(Some(template), t).unwrap();
        assert_eq!(one("{{ task.id }}", &t), "t-285");
        assert_eq!(one("{{ flock }}", &t), "personal");
        assert_eq!(one("{{ machine }}", &t), "pi-1");
        assert_eq!(one("{{ job }}", &t), "board");
        assert_eq!(one("{{ item.key }}", &t), "card-9");
        assert_eq!(
            one("{{job}}:{{ item.key }} ({{ task.id }})", &t),
            "board:card-9 (t-285)"
        );

        // `task run` has no job and no item.
        t.job = "run".into();
        t.item = Value::Null;
        assert_eq!(one("{{ job }}/{{ task.id }}", &t), "t-285");
        assert_eq!(one("{{ item.key }}-x", &t), "-x");
        // A task from before flocks has none.
        t.flock = None;
        assert_eq!(render_label(None, &t).unwrap(), "t-285");
    }

    /// A label herdr would show badly, or not at all, is refused with the
    /// reason, and dispatch then names the workspace `t-N`.
    #[test]
    fn a_label_that_renders_empty_or_with_a_control_character_is_refused() {
        let mut t = task(TaskState::Queued, None);
        t.item = serde_json::json!({"key": "a\u{1b}[2Jb"});
        t.job = "board".into();
        let err = render_label(Some("{{ item.key }}"), &t).unwrap_err();
        assert!(err.contains("control character"), "{err}");
        t.item = serde_json::json!({"key": " / "});
        let err = render_label(Some("{{ item.key }}"), &t).unwrap_err();
        assert!(err.contains("empty"), "{err}");
        let err = render_label(Some("{{ item.key }}\n"), &t).unwrap_err();
        assert!(err.contains("control character"), "{err}");
    }

    /// Only the five placeholders are known, and a bad one is refused
    /// where the template is written, not when a task runs.
    #[test]
    fn check_label_knows_the_placeholders() {
        for ok in [
            "{{ flock }}/{{ task.id }}",
            "{{ machine }}-{{ job }}-{{ item.key }}",
            "plain",
        ] {
            check_label(ok).unwrap();
        }
        let err = check_label("{{ item.title }}").unwrap_err();
        assert!(err.contains("unknown placeholder"), "{err}");
        let err = check_label("{{ task.id ").unwrap_err();
        assert!(err.contains("unterminated"), "{err}");
        let err = check_label("  ").unwrap_err();
        assert!(err.contains("empty"), "{err}");
        let err = check_label("a\tb").unwrap_err();
        assert!(err.contains("control character"), "{err}");
    }

    /// A spec that says nothing about its label writes nothing, and reads
    /// back as the built-in.
    #[test]
    fn an_unset_label_is_left_out_of_the_spec() {
        let old: DispatchSpec =
            serde_json::from_value(serde_json::json!({"agent": "claude"})).unwrap();
        assert_eq!(old.label, WorkspaceLabel::default());
        let text = serde_json::to_string(&old).unwrap();
        assert!(!text.contains("label"), "{text}");
    }

    pub fn task(state: TaskState, last_completion_seq: Option<u64>) -> Task {
        let now = Utc::now();
        Task {
            description: None,
            id: 1,
            job: "run".into(),
            item: Value::Null,
            prompt: "p".into(),
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
                timeout_secs: 10,
                checkout: None,
                reopen: None,
                agent_source: None,
                place: Default::default(),
                session_id: None,
                label: Default::default(),
                summary: Default::default(),
                cwd: None,
            },
            machine: Some("pi-1".into()),
            workspace_id: Some("w1".into()),
            pane_id: Some("w1:p1".into()),
            agent_name: Some("t-1".into()),
            state,
            error: None,
            last_completion_seq,
            prompt_pending: false,
            activity_seen: false,
            ended: false,
            retry_of: None,
            priority: Default::default(),
            priority_from: None,
            queue_pos: 0,
            aged_from: None,
            aged_at: None,
            pause: Default::default(),
            summary: None,
            created_at: now,
            started_at: Some(now),
            finished_at: None,
            updated_at: now,
            flock: None,
            role: Default::default(),
        }
    }

    /// Ageing lifts one level at a time and stops at high: it never makes
    /// a task critical.
    #[test]
    fn a_level_ages_one_up_to_high() {
        assert_eq!(Priority::Low.aged(), Some(Priority::Normal));
        assert_eq!(Priority::Normal.aged(), Some(Priority::High));
        assert_eq!(Priority::High.aged(), None);
        assert_eq!(Priority::Critical.aged(), None);
    }

    /// A task its agent ended (`pastor task done`) stays done whatever the
    /// agent shows next, until its pane goes.
    #[test]
    fn an_ended_task_stays_done() {
        let ended = Task {
            ended: true,
            ..task(TaskState::Done, Some(5))
        };
        for status in [
            AgentStatus::Working,
            AgentStatus::Blocked,
            AgentStatus::Idle,
            AgentStatus::Unknown,
        ] {
            let seen = Observed::Status {
                status,
                state_change_seq: Some(9),
                completion_seq: None,
            };
            assert_eq!(next_state(&ended, &seen), None, "{status:?}");
        }
        assert_eq!(
            next_state(&ended, &Observed::PaneClosed),
            Some(TaskState::Closed)
        );
        // Not ended, the same `working` reopens it.
        let seen = Observed::Status {
            status: AgentStatus::Working,
            state_change_seq: Some(9),
            completion_seq: None,
        };
        assert_eq!(
            next_state(&task(TaskState::Done, Some(5)), &seen),
            Some(TaskState::Running)
        );
    }

    /// A task pastor has seen `working` or `blocked` since its prompt.
    fn active(state: TaskState, last_completion_seq: Option<u64>) -> Task {
        Task {
            activity_seen: true,
            ..task(state, last_completion_seq)
        }
    }

    /// An observation as `agent.list` reports it on herdr 0.9.1: a
    /// `state_change_seq`, no `completion_seq`.
    fn status(s: AgentStatus, seq: Option<u64>) -> Observed {
        Observed::Status {
            status: s,
            state_change_seq: seq,
            completion_seq: None,
        }
    }

    #[test]
    fn working_means_running_and_blocked_means_blocked() {
        assert_eq!(
            next_state(
                &task(TaskState::Starting, None),
                &status(AgentStatus::Working, None)
            ),
            Some(TaskState::Running)
        );
        assert_eq!(
            next_state(
                &task(TaskState::Done, Some(1)),
                &status(AgentStatus::Working, Some(1))
            ),
            Some(TaskState::Running)
        );
        assert_eq!(
            next_state(
                &task(TaskState::Running, None),
                &status(AgentStatus::Blocked, None)
            ),
            Some(TaskState::Blocked)
        );
        assert_eq!(
            next_state(
                &task(TaskState::Running, None),
                &status(AgentStatus::Working, None)
            ),
            None
        );
    }

    #[test]
    fn idle_is_done_only_when_state_change_seq_passes_the_baseline() {
        assert_eq!(
            next_state(
                &task(TaskState::Running, None),
                &status(AgentStatus::Idle, Some(1))
            ),
            Some(TaskState::Done)
        );
        assert_eq!(
            next_state(
                &active(TaskState::Running, Some(1)),
                &status(AgentStatus::Done, Some(2))
            ),
            Some(TaskState::Done)
        );
        assert_eq!(
            next_state(
                &active(TaskState::Running, Some(2)),
                &status(AgentStatus::Idle, Some(2))
            ),
            None
        );
        assert_eq!(
            next_state(
                &task(TaskState::Blocked, None),
                &status(AgentStatus::Idle, None)
            ),
            Some(TaskState::Running)
        );
    }

    /// A prompt herdr refused has not reached the agent, so nothing it does
    /// while the prompt is pending is this task's work.
    #[test]
    fn a_pending_prompt_is_never_done() {
        let mut t = task(TaskState::Blocked, None);
        t.prompt_pending = true;
        assert_ne!(
            next_state(&t, &status(AgentStatus::Idle, Some(7))),
            Some(TaskState::Done)
        );
        let mut t = task(TaskState::Running, Some(3));
        t.prompt_pending = true;
        assert_eq!(next_state(&t, &status(AgentStatus::Done, Some(9))), None);
    }

    /// herdr after 0.9.1 reports `completion_seq`, in the same sequence as
    /// `state_change_seq` and only on idle transitions that completed work.
    /// When present it decides; when absent `state_change_seq` does.
    #[test]
    fn completion_seq_is_preferred_when_herdr_reports_it() {
        let seen = |state_change_seq, completion_seq| Observed::Status {
            status: AgentStatus::Idle,
            state_change_seq: Some(state_change_seq),
            completion_seq,
        };
        let t = active(TaskState::Running, Some(5));
        assert_eq!(next_state(&t, &seen(6, Some(6))), Some(TaskState::Done));
        assert_eq!(next_state(&t, &seen(6, Some(4))), None);
        assert_eq!(next_state(&t, &seen(6, None)), Some(TaskState::Done));
        assert_eq!(next_state(&t, &seen(5, None)), None);
    }

    /// herdr stamps a new `state_change_seq` on every change of the detected
    /// state, `unknown` included: `idle -> unknown -> idle` leaves an agent
    /// idle past its baseline without doing any work. Without a `working` or
    /// `blocked` seen since the prompt, that is not a completion.
    #[test]
    fn a_newer_sequence_without_activity_is_not_done() {
        let t = task(TaskState::Running, Some(3));
        assert_eq!(next_state(&t, &status(AgentStatus::Idle, Some(5))), None);
        assert_eq!(next_state(&t, &status(AgentStatus::Done, Some(5))), None);
        assert_eq!(
            next_state(
                &active(TaskState::Running, Some(3)),
                &status(AgentStatus::Idle, Some(5))
            ),
            Some(TaskState::Done)
        );
        // Blocked counts as activity too: idle after the human answered is
        // done, not back to running, once the sequence moved.
        assert_eq!(
            next_state(
                &task(TaskState::Blocked, Some(3)),
                &status(AgentStatus::Idle, Some(5))
            ),
            Some(TaskState::Running)
        );
        assert_eq!(
            next_state(
                &active(TaskState::Blocked, Some(3)),
                &status(AgentStatus::Idle, Some(5))
            ),
            Some(TaskState::Done)
        );
        // A `completion_seq` is stamped only on real completions: it needs
        // no activity seen.
        let completed = Observed::Status {
            status: AgentStatus::Idle,
            state_change_seq: Some(5),
            completion_seq: Some(5),
        };
        assert_eq!(next_state(&t, &completed), Some(TaskState::Done));
    }

    /// A subscription event carries no sequence at all; it is never enough on
    /// its own to call a task done.
    #[test]
    fn an_observation_without_a_sequence_is_not_a_completion() {
        assert_eq!(
            next_state(
                &task(TaskState::Running, None),
                &status(AgentStatus::Done, None)
            ),
            None
        );
    }

    /// A `Starting` task adopted after a crash (see `Actor::reconcile`) can be
    /// found idle before it ever produces completed work: dispatch reached at
    /// least `agent.start`, so that counts as reaching `Running`, the same as a
    /// successful dispatch would have recorded, not as reaching `Done` (that
    /// still requires `state_change_seq` to pass the baseline) or being left stuck.
    #[test]
    fn starting_found_idle_or_done_means_running_unless_already_advanced() {
        assert_eq!(
            next_state(
                &task(TaskState::Starting, None),
                &status(AgentStatus::Idle, None)
            ),
            Some(TaskState::Running)
        );
        assert_eq!(
            next_state(
                &task(TaskState::Starting, None),
                &status(AgentStatus::Done, None)
            ),
            Some(TaskState::Running)
        );
        // A sequence past the baseline still wins: an adopted agent that has
        // already finished its work goes straight to Done, same as any other
        // state (this is `next_state`'s call; `Actor::reconcile` never lets an
        // adopted agent's sequence reach here directly — it passes `None` and
        // routes the real value through the settle window instead).
        assert_eq!(
            next_state(
                &task(TaskState::Starting, None),
                &status(AgentStatus::Idle, Some(1))
            ),
            Some(TaskState::Done)
        );
    }

    #[test]
    fn dispatch_starting_only_leaves_queued() {
        assert_eq!(
            next_state(&task(TaskState::Queued, None), &Observed::DispatchStarting),
            Some(TaskState::Starting)
        );
        for state in [
            TaskState::Starting,
            TaskState::Running,
            TaskState::Blocked,
            TaskState::Done,
            TaskState::Stale,
        ] {
            assert_eq!(
                next_state(&task(state, None), &Observed::DispatchStarting),
                None,
                "{state} must not restart dispatch"
            );
        }
    }

    #[test]
    fn unknown_changes_nothing_and_terminal_states_are_sticky() {
        assert_eq!(
            next_state(
                &task(TaskState::Running, None),
                &status(AgentStatus::Unknown, None)
            ),
            None
        );
        assert_eq!(
            next_state(
                &task(TaskState::Failed, None),
                &status(AgentStatus::Working, None)
            ),
            None
        );
        assert_eq!(
            next_state(
                &task(TaskState::Closed, None),
                &Observed::PaneExited { agent_idle: false }
            ),
            None
        );
    }

    #[test]
    fn pane_events() {
        assert_eq!(
            next_state(&task(TaskState::Running, None), &Observed::PaneClosed),
            Some(TaskState::Closed)
        );
        assert_eq!(
            next_state(&task(TaskState::Done, Some(1)), &Observed::PaneClosed),
            Some(TaskState::Closed)
        );
        assert_eq!(
            next_state(
                &task(TaskState::Running, None),
                &Observed::PaneExited { agent_idle: false }
            ),
            Some(TaskState::Failed)
        );
        assert_eq!(
            next_state(
                &task(TaskState::Done, Some(1)),
                &Observed::PaneExited { agent_idle: false }
            ),
            Some(TaskState::Closed)
        );
    }

    #[test]
    fn stale_is_sticky_until_completion_or_pane_event() {
        // Working and blocked observations must not pull a stale task back into
        // the live states, or it would oscillate with the next timeout check.
        assert_eq!(
            next_state(
                &task(TaskState::Stale, None),
                &status(AgentStatus::Working, None)
            ),
            None
        );
        assert_eq!(
            next_state(
                &task(TaskState::Stale, None),
                &status(AgentStatus::Blocked, None)
            ),
            None
        );
        // Idle/done with no newer state_change_seq is not a real completion either.
        assert_eq!(
            next_state(
                &task(TaskState::Stale, Some(1)),
                &status(AgentStatus::Idle, Some(1))
            ),
            None
        );
        // A real completion (activity, then state_change_seq moves on) still
        // moves it to Done.
        assert_eq!(
            next_state(
                &active(TaskState::Stale, Some(1)),
                &status(AgentStatus::Idle, Some(2))
            ),
            Some(TaskState::Done)
        );
        assert_eq!(
            next_state(
                &task(TaskState::Stale, None),
                &status(AgentStatus::Done, Some(1))
            ),
            Some(TaskState::Done)
        );
        // A pane event still moves a stale task on.
        assert_eq!(
            next_state(&task(TaskState::Stale, None), &Observed::PaneClosed),
            Some(TaskState::Closed)
        );
        assert_eq!(
            next_state(
                &task(TaskState::Stale, None),
                &Observed::PaneExited { agent_idle: false }
            ),
            Some(TaskState::Failed)
        );
    }

    /// An exit between turns ends a running task as done; an exit from
    /// anywhere else is a failure.
    #[test]
    fn pane_exited_is_done_only_for_an_idle_running_agent() {
        let exited = |agent_idle| Observed::PaneExited { agent_idle };
        assert_eq!(
            next_state(&active(TaskState::Running, Some(1)), &exited(true)),
            Some(TaskState::Done)
        );
        // Idle but never seen working: the prompt was not acted on.
        assert_eq!(
            next_state(&task(TaskState::Running, Some(1)), &exited(true)),
            Some(TaskState::Failed)
        );
        assert_eq!(
            next_state(&active(TaskState::Running, Some(1)), &exited(false)),
            Some(TaskState::Failed)
        );
        for state in [TaskState::Starting, TaskState::Blocked, TaskState::Stale] {
            assert_eq!(
                next_state(&task(state, Some(1)), &exited(true)),
                Some(TaskState::Failed),
                "{state}"
            );
        }
        let pending = Task {
            prompt_pending: true,
            ..task(TaskState::Running, Some(1))
        };
        assert_eq!(next_state(&pending, &exited(true)), Some(TaskState::Failed));
        assert_eq!(
            next_state(&task(TaskState::Done, Some(1)), &exited(true)),
            Some(TaskState::Closed)
        );
    }

    /// Every state crossed with every observation and every flag: the rules
    /// that hold whatever branch `next_state` grows next. Sequences sit
    /// before, at and past the baseline of 5, with no baseline too (a row
    /// from a build that recorded none).
    #[test]
    fn next_state_keeps_its_invariants_over_the_whole_table() {
        use TaskState::*;
        let states = [
            Queued, Starting, Running, Blocked, Done, Stale, Failed, Closed, Paused,
        ];
        let statuses = [
            AgentStatus::Idle,
            AgentStatus::Working,
            AgentStatus::Blocked,
            AgentStatus::Done,
            AgentStatus::Unknown,
        ];
        let seqs = [None, Some(4), Some(5), Some(6)];
        let mut observations = vec![
            Observed::PaneClosed,
            Observed::PaneExited { agent_idle: false },
            Observed::PaneExited { agent_idle: true },
            Observed::DispatchStarting,
        ];
        for status in statuses {
            for state_change_seq in seqs {
                for completion_seq in seqs {
                    observations.push(Observed::Status {
                        status,
                        state_change_seq,
                        completion_seq,
                    });
                }
            }
        }
        let mut checked = 0;
        for state in states {
            for baseline in [None, Some(5)] {
                for flags in 0..8 {
                    let t = Task {
                        ended: flags & 1 != 0,
                        activity_seen: flags & 2 != 0,
                        prompt_pending: flags & 4 != 0,
                        ..task(state, baseline)
                    };
                    for seen in &observations {
                        let got = next_state(&t, seen);
                        let case = format!(
                            "{state:?} baseline {baseline:?} ended {} activity_seen {} \
                             prompt_pending {} on {seen:?} gave {got:?}",
                            t.ended, t.activity_seen, t.prompt_pending
                        );
                        assert_ne!(got, Some(state), "no-op must be None: {case}");
                        if !state.is_open() {
                            assert_eq!(got, None, "closed states never move: {case}");
                        }
                        if let Observed::Status { status, .. } = seen {
                            if state == Done && t.ended {
                                assert_eq!(got, None, "an ended done task stays: {case}");
                            }
                            if state == Stale {
                                assert!(
                                    !matches!(got, Some(Running | Blocked)),
                                    "stale never goes back: {case}"
                                );
                            }
                            if *status == AgentStatus::Unknown {
                                assert_eq!(got, None, "unknown changes nothing: {case}");
                            }
                        }
                        let completion_seq = match seen {
                            Observed::Status { completion_seq, .. } => *completion_seq,
                            _ => None,
                        };
                        if got == Some(Done) {
                            assert!(
                                t.activity_seen || completion_seq.is_some() || baseline.is_none(),
                                "done needs activity or completion_seq: {case}"
                            );
                            assert!(!t.prompt_pending, "prompt_pending is never done: {case}");
                            // A status only completes on a sequence strictly
                            // past the baseline: the completion one when the
                            // agent gave it, the state-change one otherwise.
                            if let Observed::Status {
                                state_change_seq,
                                completion_seq,
                                ..
                            } = seen
                            {
                                let seq = completion_seq.or(*state_change_seq);
                                assert!(
                                    seq.is_some_and(|s| s > baseline.unwrap_or(0)),
                                    "done needs a sequence past the baseline: {case}"
                                );
                            }
                        }
                        checked += 1;
                    }
                }
            }
        }
        assert_eq!(checked, 9 * 2 * 8 * (4 + 5 * 4 * 4));
    }

    #[test]
    fn ids() {
        assert_eq!(parse_task_id("t-12"), Some(12));
        assert_eq!(parse_task_id("12"), Some(12));
        assert_eq!(parse_task_id("x"), None);
        let msg = bad_task_id("x");
        assert!(msg.contains("t-12") && msg.contains(" 12"), "{msg}");
        assert_eq!(Task::agent_name_for(7), "t-7");
        assert_eq!("blocked".parse::<TaskState>().unwrap(), TaskState::Blocked);
    }

    /// The tail of a Claude pane at the end of a turn, as herdr's
    /// `recent_unwrapped` read gives it: the last message, the turn summary,
    /// the recap, the input box and the footer (which has a `?` of its own).
    fn claude_pane(message: &str) -> String {
        format!(
            "● Bash(make check)\n  ⎿  ok\n\n{message}\n\n✻ Baked for 6m 23s · done 11:54 PM\n\n\
             ※ recap: Did the thing. Next: review\n  the diff. (disable recaps\n  in /config)\n\n\
             ─────────────────────────\n❯\u{a0}\n─────────────────────────\n\
             \u{a0} Ctx\u{a0}Used:\u{a0}14.0...\n\
             \u{a0} ⏵⏵ auto mode on (shift+tab to cycle) · ← for agents\n\
             \u{20}                new task? /clear to save 140.6k tokens\n"
        )
    }

    #[test]
    fn a_last_message_that_asks_is_a_question() {
        let pane = claude_pane(
            "● The fix is in, but the old flag is still read\n  by the installer.\n\n  \
             Should I remove it too, or keep it for\n  one more release?",
        );
        assert_eq!(
            trailing_question(&pane).as_deref(),
            Some("Should I remove it too, or keep it for one more release?")
        );
        // Markdown emphasis around the question and the older `⏺` marker.
        let pane = claude_pane("⏺ **Which branch should I use?**");
        assert_eq!(
            trailing_question(&pane).as_deref(),
            Some("**Which branch should I use?**")
        );
    }

    #[test]
    fn a_last_message_that_reports_is_not_a_question() {
        let pane = claude_pane("● Is the build green? Yes: pushed the branch and stopped.");
        assert_eq!(trailing_question(&pane), None);
        // A question earlier in the message, then a report.
        let pane = claude_pane("● Why did it fail?\n\n  The lock was held. Fixed and pushed.");
        assert_eq!(trailing_question(&pane), None);
        // A turn that ended on a tool call, and a pane with no message at all
        // (another agent, or nothing on screen yet).
        assert_eq!(trailing_question("● Bash(git push)\n  ⎿  done?\n"), None);
        assert_eq!(trailing_question(&claude_pane("")), None);
        assert_eq!(trailing_question("fake output\nready?\n"), None);
    }

    #[test]
    fn a_footer_with_a_background_shell_is_still_running() {
        let pane = claude_pane("● Running make check in the background.")
            .replace("← for agents", "← for agents · 1 shell still running");
        assert!(background_shell_running(&pane));
        let pane = claude_pane("● Waiting.")
            .replace("auto mode on", "auto mode on · 2 shells still running");
        assert!(background_shell_running(&pane));
        assert!(background_shell_running("1 shell still running\n"));
    }

    #[test]
    fn a_footer_without_a_background_shell_is_not_running() {
        assert!(!background_shell_running(&claude_pane(
            "● Pushed the branch."
        )));
        // The phrase in a message above the input box is not the footer.
        let pane = claude_pane("● It said 1 shell still running, now ended.");
        assert!(!background_shell_running(&pane));
        assert!(!background_shell_running("fake output\n"));
    }

    /// A task marked blocked on a question moves its baseline to the idle
    /// it was found at; that same idle, seen again, changes nothing, while a
    /// newer one (the agent answered and went back to work) runs again.
    #[test]
    fn a_blocked_task_stays_blocked_while_its_agent_sits_where_it_was() {
        let t = task(TaskState::Blocked, Some(9));
        assert_eq!(next_state(&t, &status(AgentStatus::Idle, Some(9))), None);
        assert_eq!(
            next_state(&t, &status(AgentStatus::Idle, Some(11))),
            Some(TaskState::Running)
        );
        assert_eq!(
            next_state(&t, &status(AgentStatus::Working, Some(10))),
            Some(TaskState::Running)
        );
    }

    #[test]
    fn a_summary_names_its_outcome_on_its_first_line() {
        for (text, want) in [
            ("done", Outcome::Done),
            ("Done: pushed pastor/t-4\nPR #12", Outcome::Done),
            ("  partial - tests left\n", Outcome::Partial),
            ("BLOCKED. needs a token", Outcome::Blocked),
            (
                "nothing to do\nthe card was already built",
                Outcome::NothingToDo,
            ),
            ("Nothing to do: already merged", Outcome::NothingToDo),
            ("doneish", Outcome::Unknown),
            ("pushed the branch\ndone", Outcome::Unknown),
            ("", Outcome::Unknown),
        ] {
            assert_eq!(Outcome::parse(text), want, "{text:?}");
        }
    }

    #[test]
    fn outcomes_round_trip_as_their_words() {
        for o in [
            Outcome::Done,
            Outcome::Partial,
            Outcome::Blocked,
            Outcome::NothingToDo,
            Outcome::NoSummary,
            Outcome::Unknown,
        ] {
            assert_eq!(o.as_str().parse::<Outcome>().unwrap(), o);
            assert_eq!(serde_json::to_value(o).unwrap(), o.as_str());
        }
    }

    #[test]
    fn summaries_are_capped_at_their_start_and_pane_tails_at_their_end() {
        let long: String = "ab".repeat(SUMMARY_MAX);
        let s = cap_summary(&format!("  {long}  "));
        assert_eq!(s.chars().count(), SUMMARY_MAX);
        assert!(s.starts_with("abab"));
        let tail = cap_pane_tail(&format!("x{long}y\n"));
        assert_eq!(tail.chars().count(), SUMMARY_MAX);
        assert!(tail.ends_with("aby"));
        assert_eq!(
            cap_summary("é".repeat(SUMMARY_MAX + 3).as_str())
                .chars()
                .count(),
            SUMMARY_MAX
        );
    }

    /// `summary` reads and writes as its word; a spec that says nothing is
    /// `ask`, and `ask` is left out of the JSON.
    #[test]
    fn summary_mode_round_trips_and_defaults_to_ask() {
        for m in SummaryMode::ALL {
            assert_eq!(m.as_str().parse::<SummaryMode>().unwrap(), m);
            assert_eq!(serde_json::to_value(m).unwrap(), m.as_str());
        }
        assert!("Ask".parse::<SummaryMode>().is_err());
        let old: DispatchSpec =
            serde_json::from_value(serde_json::json!({"agent": "claude"})).unwrap();
        assert_eq!(old.summary, SummaryMode::Ask);
        let text = serde_json::to_string(&old).unwrap();
        assert!(!text.contains("summary"), "the default is left out: {text}");
        let mut req = old.clone();
        req.summary = SummaryMode::Require;
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["summary"], "require");
    }

    /// The prompt pastor sends ends with the paragraph asking for a
    /// summary, after a blank line, for `ask` and `require`; `off` sends
    /// the prompt alone. The stored prompt never changes.
    #[test]
    fn the_prompt_sent_asks_for_a_summary_unless_off() {
        let mut t = task(TaskState::Queued, None);
        t.prompt = "fix it\n".into();
        assert_eq!(prompt_to_send(&t), format!("fix it\n\n{SUMMARY_ASK}"));
        assert!(SUMMARY_ASK.contains("pastor task done --summary-file -"));
        t.spec.summary = SummaryMode::Require;
        let sent = prompt_to_send(&t);
        assert!(
            sent.starts_with(&format!("fix it\n\n{SUMMARY_ASK}")),
            "{sent}"
        );
        assert!(sent.ends_with(SUMMARY_REQUIRE), "{sent}");
        t.spec.summary = SummaryMode::Off;
        assert_eq!(prompt_to_send(&t), "fix it\n");
        assert_eq!(t.prompt, "fix it\n");
    }
}
