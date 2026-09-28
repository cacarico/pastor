use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::config::AgentChoice;
use crate::machine::MachineStatus;
use crate::scheduler::{JobRunReport, JobStatus};
use crate::store::TaskFilter;
use crate::task::{DispatchSpec, Task, TaskRole, TaskState};

/// The head's IPC protocol, answered in `Pong`. Bumped when a request gains a
/// field an older head would silently ignore (serde skips unknown fields), so
/// the CLI can refuse to send it there. A head that answers no protocol is 0.
/// 1: flocks (`Run::flock`, `TaskFilter::flock`). 2: flock agents and tool
/// lists. 3: `TaskRetry::place`. 4: `EventsSince`. 5: flock and machine
/// edits (`FLEET_EDIT_PROTOCOL`). 6: `FileGet`, `FilePut`, `JobDescribe`,
/// `JobSetEnabled`. 7: `JobSubmit`. 8: named models (`AgentChoice::model`).
/// 9: `JobTask`, and `Pong::role`. 10: `TrustList`, `TrustAdd`,
/// `TrustRemove`, `FlockDescribe` and `MachineDescribe`. 11: task priority
/// (`Run::priority`, `TaskPriority`). 12: `Run::role`. 13: permission
/// profiles (`AgentChoice::profile`). 14: `Queue` and `QueueMove`. 15:
/// descriptions (`Run::description`, `FlockAdd::description`,
/// `JobTask::description`). 16: pausing (`Run::preempt`,
/// `TaskPriority::preempt`). 17: task summaries (`TaskDone::summary`,
/// `TaskSummaries`). 18: a machine in many flocks, each with its own number
/// (`FlockEntry::machines`). 19: workspace labels (`DispatchSpec::label`).
/// 20: the `summary` setting (`Run::summary`, a job's `[dispatch]
/// summary`). 21: pull machines (`TaskClaim`, `TaskReport`).
pub const IPC_PROTOCOL: u32 = 21;

/// The variable pastor sets in the pane of every agent it starts, to the
/// task's agent name (`t-7`). The CLI passes it on to the head as
/// `FROM_TASK_FIELD`, and the head refuses such a caller any request that
/// changes the fleet unless `agents_change_fleet` allows it. It is a guard
/// against an agent acting on its own, not a boundary: the agent runs as the
/// same user and can unset it.
pub const TASK_ENV: &str = "PASTOR_TASK";

/// Set in an agent's pane on a machine other than the head's: the ssh
/// destination (`head_address` in pastor.toml) that reaches the head.
pub const HEAD_ENV: &str = "PASTOR_HEAD";

/// The field beside a request's own that names the task it comes from.
pub const FROM_TASK_FIELD: &str = "from_task";

/// The first protocol whose head honours `flock` in a request.
pub const FLOCK_PROTOCOL: u32 = 1;

/// The first protocol whose head resolves a run's agent with its flock and
/// passes the tool allow and deny lists on. An older one would start the
/// agent without the deny list, and say nothing.
pub const AGENT_PROTOCOL: u32 = 2;

/// The first protocol whose head honours `TaskRetry::place`. An older one
/// would retry the task with its original place, and say it succeeded.
pub const PLACE_PROTOCOL: u32 = 3;

/// The first protocol whose head answers `EventsSince`. An older one fails
/// to read the request and answers an error.
pub const EVENTS_PROTOCOL: u32 = 4;

/// The first protocol whose head edits flock.toml itself for `flock
/// add|default` and `machine add|remove|move`. An older one does not know
/// those requests.
pub const FLEET_EDIT_PROTOCOL: u32 = 5;
/// The first protocol whose head takes `FileGet`, `FilePut`, `JobDescribe`
/// and `JobSetEnabled`. An older one refuses them as unknown requests.
pub const FILE_PROTOCOL: u32 = 6;
/// The first protocol whose head takes `JobTask`, what a headless serve
/// sends for each item its jobs find. An older one refuses it as unknown.
pub const SHEPHERD_PROTOCOL: u32 = 9;

/// The first protocol whose head's `FlockEntry` knows `machines`, a flock's
/// per-machine number. An older head's `FlockEntry` (`deny_unknown_fields`)
/// rejects the field on reload and keeps its previous, single-flock
/// membership instead, so a CLI new enough to write `machines` refuses to
/// send it work (see `multi_flock_declared` in `main.rs`).
pub const MULTI_FLOCK_PROTOCOL: u32 = 18;

/// The first protocol whose head knows pull machines: it answers
/// `TaskClaim` and `TaskReport`. An older one refuses them as unreadable.
pub const PULL_PROTOCOL: u32 = 21;

/// `Pong::role` of a headless `pastor serve`: it runs this machine's jobs
/// and hooks against a head elsewhere, and is not a head itself.
pub const SHEPHERD_ROLE: &str = "shepherd";

/// The error code a head answers a `JobTask` with when it queued that
/// job's key before and the task row is gone; a live row is answered
/// again instead.
pub const ALREADY_SEEN: &str = "already_seen";

/// The first protocol whose head answers `TrustList`, `TrustAdd`,
/// `TrustRemove`, `FlockDescribe` and `MachineDescribe`. An older one reads
/// them as an unknown request and answers `invalid_request`, so the CLI
/// refuses it with `head_too_old` first.
pub const HEAD_READS_PROTOCOL: u32 = 10;

/// The first protocol whose head honours `Run::role`. An older one would
/// queue a plain agent and say it succeeded.
pub const ROLE_PROTOCOL: u32 = 12;

/// The first protocol whose head keeps what `task run --description`,
/// `flock add --description` and `machine add --description` send. An older
/// one would drop it without a word.
pub const DESCRIPTION_PROTOCOL: u32 = 14;

/// The first protocol whose head settles and renders a task's workspace
/// label (`task run --label`). An older one would name the workspace `t-N`
/// without a word.
pub const LABEL_PROTOCOL: u32 = 19;

/// The first protocol whose head knows `JobSubmit`. An older one refuses the
/// request as unreadable; `check_protocol` says why before it is sent.
pub const JOB_SUBMIT_PROTOCOL: u32 = 7;

/// The first protocol whose head runs a task's named model (`--model`, a
/// flock's or job's `model`). An older one would drop it and start the agent
/// on its default model without a word.
pub const MODEL_PROTOCOL: u32 = 8;

/// The first protocol whose head honours `Run::priority` and knows
/// `TaskPriority`. An older one would queue the task at its own level
/// without a word, or refuse the request as unreadable.
pub const PRIORITY_PROTOCOL: u32 = 11;

/// The first protocol whose head runs a task under its permission profile
/// (`--profile`, a job's, machine's or flock's `profile`). An older one
/// would drop it and start the agent asking before every tool, or with
/// fewer denies, without a word.
pub const PROFILE_PROTOCOL: u32 = 13;

/// The first protocol whose head knows `Queue` and `QueueMove`; an older
/// one refuses them as unreadable.
pub const QUEUE_PROTOCOL: u32 = 14;

/// The first protocol whose head honours `Run::preempt` and
/// `TaskPriority::preempt`. An older one would queue the task without it,
/// and it would wait behind low work, without a word.
pub const PREEMPT_PROTOCOL: u32 = 16;

/// The first protocol whose head keeps what `task done --summary` sends and
/// knows `TaskSummaries`. An older one would drop the summary without a
/// word, or refuse the request as unreadable.
pub const SUMMARY_PROTOCOL: u32 = 17;

/// The first protocol whose head honours `Run::summary` and a job's
/// `[dispatch] summary`. An older one would ask for a summary on its own
/// terms, or refuse the job's dispatch table as unreadable.
pub const SUMMARY_MODE_PROTOCOL: u32 = 20;

/// `head_too_old` unless the head (its version and protocol, from `Pong`)
/// speaks at least `needed`; `what` names what the older head lacks.
pub fn check_protocol(version: &str, protocol: u32, needed: u32, what: &str) -> anyhow::Result<()> {
    if protocol >= needed {
        return Ok(());
    }
    Err(crate::cli::CliError::err(
        "head_too_old",
        format!("the running pastor serve ({version}) predates {what}; restart it"),
    ))
}

// One request is read per connection and dropped once answered, so the
// size of the largest variant (`Run`) costs nothing worth a box.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum IpcRequest {
    Ping,
    Run {
        prompt: String,
        spec: DispatchSpec,
        /// `None`: the flock of the pinned machine, or the default flock.
        #[serde(default)]
        flock: Option<String>,
        /// What the run's flags said about the agent; the head fills in
        /// the rest from the task's flock and `[defaults]`. `None` from a
        /// client that predates it: `spec` already holds the agent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<AgentChoice>,
        /// `--priority`; `None` lets the pinned machine, the flock or
        /// `[defaults]` set the level. Left out when not given, so an older
        /// head still reads the request.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        priority: Option<crate::task::Priority>,
        /// The task's role (`task run --role`). Left out for a plain agent,
        /// so an older head still reads the request; given, the CLI sends it
        /// only to a head of `ROLE_PROTOCOL` or later.
        #[serde(default, skip_serializing_if = "TaskRole::is_agent")]
        role: TaskRole,
        /// `task run --description`, trimmed; `None` reads as the prompt's
        /// first line.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        /// `task run --preempt`: the task, critical, may pause a low one to
        /// start. Left out when not given, so an older head still reads the
        /// request; given, the CLI sends it only to a head of
        /// `PREEMPT_PROTOCOL` or later.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        preempt: bool,
        /// `task run --summary`; `None` lets the flock or `[defaults]`
        /// decide. Left out when not given, so an older head still reads
        /// the request; given, the CLI sends it only to a head of
        /// `SUMMARY_MODE_PROTOCOL` or later.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<crate::task::SummaryMode>,
    },
    List {
        filter: TaskFilter,
    },
    TaskShow {
        id: i64,
    },
    /// Every round's summary of a task, the first first. Answers
    /// `Summaries`.
    TaskSummaries {
        id: i64,
    },
    TaskRead {
        id: i64,
        lines: u32,
    },
    FlockList,
    /// `flock remove`, done by the head so the queued-task check and the
    /// edit of flock.toml are one step against `Run`. Answers `Text`.
    FlockRemove {
        name: String,
    },
    /// `flock add`, done by the head so the queued-task check and the edit
    /// are one step against `Run`, as for `FlockRemove`. Answers `Text`.
    FlockAdd {
        name: String,
        default: bool,
        /// `flock add --description`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    /// `flock default`. Answers `Text`.
    FlockSetDefault {
        name: String,
    },
    /// `machine add`, the flock.toml part; `--herdr` stays with the CLI,
    /// whose herdr it is. Answers `Text`.
    MachineAdd {
        machine: crate::config::flock::MachineConfig,
    },
    /// `machine remove`, the flock.toml part, as for `MachineAdd`. Answers
    /// `Text`.
    MachineRemove {
        name: String,
    },
    /// `machine move`. Answers `Text`.
    MachineMove {
        name: String,
        flock: String,
    },
    /// One scheduler pass now; `job` forces that job regardless of schedule.
    Tick {
        job: Option<String>,
        dry_run: bool,
    },
    /// Re-read the jobs directory now.
    Reload,
    JobList,
    /// Fire a job now, ignoring schedule and `enabled`; it waits for a run
    /// already going.
    JobRun {
        name: String,
    },
    /// Queue a new task copying a failed or stale one, then dispatch.
    /// Answers `Task` (the new row).
    TaskRetry {
        id: i64,
        /// Where the copy's pane goes instead of the original's
        /// (`task retry --place`). Left out when not given, so an older head
        /// still reads the request; given, the CLI sends it only to a head
        /// of `PLACE_PROTOCOL` or later.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        place: Option<crate::task::Place>,
    },
    /// Put a queued task at another level (`task priority`). Refused
    /// `not_queued` for a task that has left the queue. Answers `Task`.
    TaskPriority {
        id: i64,
        priority: crate::task::Priority,
        /// `task priority --preempt`; without it the task's flag goes.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        preempt: bool,
    },
    /// The queued tasks in dispatch order, each with why it waits
    /// (`pastor queue`); `flock` and `machine` keep a flock's tasks or the
    /// ones pinned to a machine. Answers `Queue`.
    Queue {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        flock: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        machine: Option<String>,
    },
    /// Put a queued task elsewhere in the queue (`pastor queue move`).
    /// Refused `not_queued` when it, or the task it goes before or after,
    /// has left the queue. Answers `Moved`.
    QueueMove {
        id: i64,
        to: crate::queue::QueueSpot,
    },
    /// Close the task's pane (or, with `remove_worktree`, its worktree) and
    /// mark it closed. Answers `Task`, or `Text` for an orphaned agent with no
    /// row.
    TaskClose {
        id: i64,
        remove_worktree: bool,
    },
    /// Type into a live task's pane, through its machine's actor. Answers
    /// `Text`.
    TaskSend {
        id: i64,
        input: crate::machine::SendInput,
    },
    /// The task's agent says it is finished: mark the task done so its pane
    /// closes after `close_done_after`. The one change an agent may make
    /// without `agents_change_fleet`, and only to its own task
    /// (`ends_own_task`). Answers `Task`.
    TaskDone {
        id: i64,
        /// What the agent did (`task done --summary`); its first line names
        /// the outcome. `None` ends the round with the pane's last lines.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },
    /// The events log records numbered after `after`, oldest first, at most
    /// `limit`, only those about `task` if given, read from the log files.
    /// Answers `Events`, whose `gap` says records after `after` were rotated
    /// out of the log.
    EventsSince {
        after: u64,
        limit: u32,
        #[serde(default)]
        task: Option<i64>,
    },
    /// Delete rows in `states` that finished more than `older_than_secs`
    /// ago. Answers `Pruned`.
    TaskPrune {
        states: Vec<TaskState>,
        older_than_secs: u64,
    },
    /// The text of one of the head's config files, `flock`, `config` or
    /// `job:<name>` (`edit::ConfigFile`), with its hash. Answers `File`.
    FileGet {
        file: String,
    },
    /// Replace one of the head's config files with `text`, through
    /// `edit::put`: refused `invalid_edit` when the head would not load it,
    /// `edit_conflict` when the file's hash is no longer `base_hash`. A saved
    /// file reloads the head. Answers `Text`.
    FilePut {
        file: String,
        text: String,
        base_hash: String,
    },
    /// What `job describe --json` prints, from the head's files. Answers
    /// `Job`.
    JobDescribe {
        name: String,
    },
    /// `job enable|disable` on the head's job file, then a reload. Answers
    /// `Text`.
    JobSetEnabled {
        name: String,
        enabled: bool,
    },
    /// Items another machine's job found, to become tasks here: the head
    /// keeps the `seen` keys, so no item is queued twice. `dispatch` is the
    /// job file's `[dispatch]` table and `prompt` its template; each item is
    /// an object with a string `key`. Refused with `job_name_taken` when the
    /// head has a job file of that name. Answers `JobSubmitted`.
    JobSubmit {
        job: String,
        dispatch: serde_json::Value,
        prompt: String,
        items: Vec<serde_json::Value>,
    },
    /// One item a headless serve's job found, queued as that job's task on
    /// the head. `prompt` and `spec` are the job's unrendered templates: the
    /// head renders them with the id it gives the task, and resolves the
    /// agent from `agent` and `flock` as it would for its own job. Answers
    /// `Task`.
    JobTask {
        job: String,
        #[serde(default)]
        flock: Option<String>,
        agent: AgentChoice,
        prompt: String,
        spec: DispatchSpec,
        item: serde_json::Value,
        /// The job's `[dispatch] description` template, rendered on the
        /// head like `prompt`. A head that predates it gives the task none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        description: Option<String>,
    },
    /// Every saved folder trust. Answers `Trusted`.
    TrustList,
    /// Save `repo` on `machine` as trusted. Answers `Text`.
    TrustAdd {
        machine: String,
        repo: String,
    },
    /// Forget a saved trust. Answers `Text`, or `not_trusted`.
    TrustRemove {
        machine: String,
        repo: String,
    },
    /// `flock describe`, from the flock the head last applied. Answers
    /// `FlockDescription`.
    FlockDescribe {
        name: String,
    },
    /// `machine describe`, from the head's machines. Answers
    /// `MachineDescription`.
    MachineDescribe {
        name: String,
    },
    /// A pull machine's headless serve asks for work: at most `free_slots`
    /// queued tasks to start, those pinned to `machine` first, then, with
    /// `flock_work`, any its flocks would place there now. Each one handed
    /// out is `starting` on `machine` before the reply. Also what keeps the
    /// machine from going lost (`pull_lost_after`). Answers `Tasks`.
    TaskClaim {
        machine: String,
        free_slots: u32,
        #[serde(default)]
        flock_work: bool,
    },
    /// A pull machine's serve says what became of a task it claimed: its
    /// state, its pane, and `detail`, the note the task carries there (why
    /// it failed, the question it asked). The head writes it on the row and
    /// emits the event its own actor would have. Refused `not_on_machine`
    /// for a task placed elsewhere. Answers `Task`.
    TaskReport {
        machine: String,
        id: i64,
        state: TaskState,
        #[serde(default)]
        pane: Option<String>,
        #[serde(default)]
        detail: Option<String>,
    },
}

impl IpcRequest {
    /// Whether the request can start, stop, feed or reshape work: anything
    /// but a read. A reload and a tick, dry or not, count: both apply
    /// pastor.toml and flock.toml first. An agent pastor started is refused
    /// these (`TASK_ENV`).
    pub fn changes_fleet(&self) -> bool {
        match self {
            IpcRequest::Ping
            | IpcRequest::List { .. }
            | IpcRequest::TaskShow { .. }
            | IpcRequest::TaskSummaries { .. }
            | IpcRequest::TaskRead { .. }
            | IpcRequest::FlockList
            | IpcRequest::JobList
            | IpcRequest::EventsSince { .. }
            | IpcRequest::TrustList
            | IpcRequest::FlockDescribe { .. }
            | IpcRequest::MachineDescribe { .. }
            | IpcRequest::Queue { .. } => false,
            IpcRequest::FileGet { .. } | IpcRequest::JobDescribe { .. } => false,
            IpcRequest::Reload
            | IpcRequest::Tick { .. }
            | IpcRequest::Run { .. }
            | IpcRequest::FlockRemove { .. }
            | IpcRequest::FlockAdd { .. }
            | IpcRequest::FlockSetDefault { .. }
            | IpcRequest::MachineAdd { .. }
            | IpcRequest::MachineRemove { .. }
            | IpcRequest::MachineMove { .. }
            | IpcRequest::JobRun { .. }
            | IpcRequest::TaskRetry { .. }
            | IpcRequest::TaskPriority { .. }
            | IpcRequest::QueueMove { .. }
            | IpcRequest::TaskClose { .. }
            | IpcRequest::TaskSend { .. }
            | IpcRequest::TaskDone { .. }
            | IpcRequest::TaskPrune { .. }
            | IpcRequest::FilePut { .. }
            | IpcRequest::JobSetEnabled { .. }
            | IpcRequest::JobSubmit { .. }
            | IpcRequest::JobTask { .. }
            | IpcRequest::TrustAdd { .. }
            | IpcRequest::TrustRemove { .. }
            | IpcRequest::TaskClaim { .. }
            | IpcRequest::TaskReport { .. } => true,
        }
    }

    /// Whether an orchestrator task (`TaskRole::Orchestrator`) may make this
    /// change without `agents_change_fleet`: run, retry, type into and close
    /// tasks, and enable or disable a job.
    /// Making another orchestrator is refused apart from this, whoever asks
    /// from inside a task (`Daemon::handle_from`).
    pub fn orchestrator_may(&self) -> bool {
        matches!(
            self,
            IpcRequest::Run { .. }
                | IpcRequest::TaskRetry { .. }
                | IpcRequest::TaskSend { .. }
                | IpcRequest::TaskClose { .. }
                | IpcRequest::JobSetEnabled { .. }
        )
    }

    /// Whether the request only ends `task`, the task the caller runs in
    /// (`TASK_ENV`): an agent may say it is finished, but not for anyone
    /// else.
    pub fn ends_own_task(&self, task: &str) -> bool {
        matches!(self, IpcRequest::TaskDone { id, .. } if crate::task::parse_task_id(task) == Some(*id))
    }
}

/// The line a request crosses the socket as: the request, plus
/// `FROM_TASK_FIELD` when the caller runs in a task's pane. A head that
/// predates the field skips it, as serde skips any unknown field.
pub fn request_line(req: &IpcRequest, from_task: Option<&str>) -> anyhow::Result<String> {
    let mut v = serde_json::to_value(req)?;
    if let (Some(task), Some(obj)) = (from_task, v.as_object_mut()) {
        obj.insert(FROM_TASK_FIELD.into(), task.into());
    }
    let mut line = serde_json::to_string(&v)?;
    line.push('\n');
    Ok(line)
}

/// The head's side of `request_line`: the request, and the task it says it
/// comes from.
pub fn parse_request_line(line: &str) -> serde_json::Result<(IpcRequest, Option<String>)> {
    let mut v: serde_json::Value = serde_json::from_str(line)?;
    let from_task = v
        .as_object_mut()
        .and_then(|obj| obj.remove(FROM_TASK_FIELD))
        .and_then(|t| t.as_str().map(str::to_string));
    Ok((serde_json::from_value(v)?, from_task))
}

static CALLER_TASK: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// The task `TASK_ENV` names: `None` outside a pane pastor started, or when
/// the variable is empty.
pub fn task_from_env() -> Option<String> {
    std::env::var(TASK_ENV).ok().filter(|t| !t.is_empty())
}

/// Sets the task this process's requests carry. Only the `pastor` binary
/// calls it, from `task_from_env`: the library never reads the variable
/// itself, so tests run from an agent's pane don't send the mark and get
/// their own requests refused.
pub fn set_caller_task(task: Option<String>) {
    let _ = CALLER_TASK.set(task);
}

/// The task this process's requests carry, as `set_caller_task` left it.
pub fn caller_task() -> Option<String> {
    CALLER_TASK.get().cloned().flatten()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
// Adjacently tagged (`content = "data"`), not internally tagged: several variants
// here are newtypes wrapping a `Vec` or a `String` (`Tasks`, `Text`, `Machines`),
// and serde_json cannot serialize those under internal tagging (a sequence or a
// string cannot carry a `kind` field merged into it). Adjacent tagging works
// uniformly across struct and newtype variants alike.
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
// One response crosses the socket at a time (never in a hot loop), so the size
// difference between variants that clippy flags here doesn't matter in practice.
#[allow(clippy::large_enum_variant)]
pub enum IpcResponse {
    Pong {
        version: String,
        /// `IPC_PROTOCOL` of the head; missing from a head older than it.
        #[serde(default)]
        protocol: u32,
        /// `SHEPHERD_ROLE` from a headless serve; absent from a head.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role: Option<String>,
    },
    Task(Task),
    Tasks(Vec<Task>),
    Text(String),
    Machines(Vec<MachineStatus>),
    Error {
        code: String,
        message: String,
    },
    Runs(Vec<JobRunReport>),
    Jobs(Vec<JobStatus>),
    Pruned(crate::store::PruneOutcome),
    Events(crate::events::EventsPage),
    File(FileText),
    Job(crate::describe::JobDescription),
    /// `JobSubmit`'s outcome: the tasks queued, the keys already seen (or
    /// repeated in the request), and each item refused with its reason.
    JobSubmitted {
        tasks: Vec<Task>,
        skipped: Vec<String>,
        refused: Vec<(String, String)>,
    },
    Trusted(Vec<crate::store::TrustedRepo>),
    FlockDescription(crate::describe::FlockDescription),
    MachineDescription(crate::describe::MachineDescription),
    Queue(Vec<crate::queue::QueueEntry>),
    Moved(crate::store::Moved),
    Summaries(Vec<crate::task::TaskSummary>),
}

/// One of the head's config files as `FileGet` found it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileText {
    /// Where it is on the head, for messages and the editor's temp copy.
    pub path: String,
    /// Empty for a missing flock.toml or pastor.toml.
    pub text: String,
    /// `edit::hash` of `text`, to send back as `FilePut::base_hash`.
    pub hash: String,
}

impl IpcResponse {
    pub fn error(code: &str, message: impl std::fmt::Display) -> IpcResponse {
        IpcResponse::Error {
            code: code.into(),
            message: message.to_string(),
        }
    }
}

/// Default bound on a full request/reply round trip. Deliberately longer than
/// `MachineSettings::request_timeout`'s default (60s): a `Run` that dispatches
/// inline (see `Daemon::handle`) must have room to finish before this gives up.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// `daemon_running` only needs to know whether something answers `Ping`; it
/// shouldn't itself hang for a minute against a wedged daemon.
const PING_TIMEOUT: Duration = Duration::from_secs(2);

/// Why a request to the head failed. The CLI tells the user different things
/// for each: nothing listening means `pastor serve` is not running, while a
/// connection that was accepted and then left unanswered means the head is up
/// but busy (a long dispatch pass holds the accept loop).
#[derive(Debug, thiserror::Error)]
pub enum RequestError {
    /// The connect itself failed: no socket, connection refused, permission
    /// denied. Nothing answered.
    #[error("{0}")]
    Connect(std::io::Error),
    /// Something accepted the connection (or kept it in the backlog) but did
    /// not reply within the bound.
    #[error("the head did not answer within {0:?}")]
    Timeout(Duration),
    /// Connected, then the exchange broke: a dropped connection, an empty or
    /// malformed reply.
    #[error(transparent)]
    Exchange(anyhow::Error),
    /// A remote head (`head::RemoteHead`): ssh exited with no reply. The
    /// message carries ssh's stderr.
    #[error("{0}")]
    Unreachable(String),
    /// A remote head: `pastor bridge` answered with an error of its own
    /// (`no_head`) instead of the head's reply.
    #[error("{message}")]
    Refused { code: String, message: String },
}

static REMOTE_HEAD: std::sync::OnceLock<Option<crate::head::RemoteHead>> =
    std::sync::OnceLock::new();

/// Sets the head this process's requests go to over ssh. Only the `pastor`
/// binary calls it, once, from `--head`, `PASTOR_HEAD` or client.toml.
pub fn set_remote_head(head: Option<crate::head::RemoteHead>) {
    let _ = REMOTE_HEAD.set(head);
}

/// The remote head `set_remote_head` left, if any.
pub fn remote_head() -> Option<&'static crate::head::RemoteHead> {
    REMOTE_HEAD.get().and_then(Option::as_ref)
}

/// One request to the head, wherever it is: over ssh to a remote head when
/// one is set, else to this machine's socket. Every CLI request goes through
/// here.
pub async fn request_head(
    paths: &crate::config::Paths,
    req: &IpcRequest,
) -> Result<IpcResponse, RequestError> {
    request_head_with_timeout(paths, req, DEFAULT_REQUEST_TIMEOUT).await
}

/// `request_head` with its own bound on the round trip.
pub async fn request_head_with_timeout(
    paths: &crate::config::Paths,
    req: &IpcRequest,
    timeout: Duration,
) -> Result<IpcResponse, RequestError> {
    match remote_head() {
        Some(head) => {
            let line =
                request_line(req, caller_task().as_deref()).map_err(RequestError::Exchange)?;
            head.request(&line, timeout).await
        }
        None => request_with_timeout(&paths.socket_file(), req, timeout).await,
    }
}

/// One request, one reply, then the connection closes. Bounded by
/// `DEFAULT_REQUEST_TIMEOUT`; use `request_with_timeout` to choose a different bound.
pub async fn request(socket: &Path, req: &IpcRequest) -> Result<IpcResponse, RequestError> {
    request_with_timeout(socket, req, DEFAULT_REQUEST_TIMEOUT).await
}

/// Like `request`, but bounds the whole round trip (connect + write + read) by
/// `timeout` instead of the default. A daemon that accepts the connection but
/// never replies (wedged on a hung herdr request, or otherwise stuck) must not
/// hang the caller forever.
pub async fn request_with_timeout(
    socket: &Path,
    req: &IpcRequest,
    timeout: Duration,
) -> Result<IpcResponse, RequestError> {
    let exchange = async {
        let stream = tokio::net::UnixStream::connect(socket)
            .await
            .map_err(RequestError::Connect)?;
        round_trip(stream, req)
            .await
            .map_err(RequestError::Exchange)
    };
    match tokio::time::timeout(timeout, exchange).await {
        Ok(result) => result,
        Err(_) => Err(RequestError::Timeout(timeout)),
    }
}

/// One request line as a client wrote it, passed to the head unread, and the
/// reply line as the head wrote it, newline included: `pastor bridge`'s round
/// trip. Bytes in, bytes out: a request need not be UTF-8 (the head decides
/// what is a valid request, not this relay), so this reads and writes bytes
/// rather than a `str`. The same bound and failures as `request`.
pub async fn relay_line(socket: &Path, line: &[u8]) -> Result<Vec<u8>, RequestError> {
    let exchange = async {
        let stream = tokio::net::UnixStream::connect(socket)
            .await
            .map_err(RequestError::Connect)?;
        let (r, mut w) = stream.into_split();
        let io = async {
            w.write_all(line).await?;
            if !line.ends_with(b"\n") {
                w.write_all(b"\n").await?;
            }
            w.flush().await?;
            let mut reply = Vec::new();
            BufReader::new(r).read_until(b'\n', &mut reply).await?;
            anyhow::ensure!(
                reply.iter().any(|b| !b.is_ascii_whitespace()),
                "daemon closed the connection without a reply"
            );
            if !reply.ends_with(b"\n") {
                reply.push(b'\n');
            }
            Ok(reply)
        };
        io.await.map_err(RequestError::Exchange)
    };
    match tokio::time::timeout(DEFAULT_REQUEST_TIMEOUT, exchange).await {
        Ok(result) => result,
        Err(_) => Err(RequestError::Timeout(DEFAULT_REQUEST_TIMEOUT)),
    }
}

async fn round_trip(
    stream: tokio::net::UnixStream,
    req: &IpcRequest,
) -> anyhow::Result<IpcResponse> {
    let (r, mut w) = stream.into_split();
    let line = request_line(req, caller_task().as_deref())?;
    w.write_all(line.as_bytes()).await?;
    w.flush().await?;
    let mut reply = String::new();
    BufReader::new(r).read_line(&mut reply).await?;
    anyhow::ensure!(
        !reply.trim().is_empty(),
        "daemon closed the connection without a reply"
    );
    Ok(serde_json::from_str(reply.trim())?)
}

/// What `probe_daemon` found at a socket path. Only `NotRunning` means it's safe to
/// unlink and replace the socket file: a connect refused (or a path that doesn't
/// exist) is the one signal that nothing is on the other end. `Running` and
/// `Unresponsive` both mean something is there — a busy daemon mid-request looks
/// exactly like a wedged one from the outside, so both must be left alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonProbe {
    /// Connected and got back a `Pong` within the probe window.
    Running,
    /// The connect failed the way an absent or abandoned socket does (refused, or
    /// the path doesn't exist).
    NotRunning,
    /// Something accepted the connection but didn't answer `Ping` within
    /// `PING_TIMEOUT`, or the connect failed some other way (e.g. permission
    /// denied). Either way, it is not safe to assume the socket is stale.
    Unresponsive,
}

/// Whether a failed connect to the daemon socket shows that no daemon is there.
/// Only a refused connect or a missing path does; anything else, such as
/// permission denied, may hide a live daemon. `probe_daemon` and the CLI's
/// "not running" advice both use this so they cannot disagree.
pub fn connect_error_means_no_daemon(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
    )
}

/// What one ping at the head's socket found, with the pong's fields when
/// there was one: `probe_daemon`, keeping what the head said about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadPing {
    NotRunning,
    Unresponsive,
    Pong {
        version: String,
        protocol: u32,
        role: Option<String>,
    },
}

pub async fn ping_head(socket: &Path) -> HeadPing {
    let stream = match tokio::net::UnixStream::connect(socket).await {
        Ok(s) => s,
        Err(err) if connect_error_means_no_daemon(&err) => return HeadPing::NotRunning,
        Err(_) => return HeadPing::Unresponsive,
    };
    match tokio::time::timeout(PING_TIMEOUT, round_trip(stream, &IpcRequest::Ping)).await {
        Ok(Ok(IpcResponse::Pong {
            version,
            protocol,
            role,
        })) => HeadPing::Pong {
            version,
            protocol,
            role,
        },
        _ => HeadPing::Unresponsive,
    }
}

/// The head as a CLI command found it with its one `ping_head`, carried
/// through the whole command so no later probe can read it differently. A
/// head that is listening but does not answer is neither: the command stops
/// (`head_unresponsive`) rather than work as if none ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Head {
    /// Nothing listens on the socket; the command works without a head.
    Absent,
    /// The head answered the ping.
    Live,
}

impl Head {
    pub fn is_live(self) -> bool {
        self == Head::Live
    }
}

pub async fn probe_daemon(socket: &Path) -> DaemonProbe {
    match ping_head(socket).await {
        HeadPing::NotRunning => DaemonProbe::NotRunning,
        HeadPing::Unresponsive => DaemonProbe::Unresponsive,
        HeadPing::Pong { .. } => DaemonProbe::Running,
    }
}

pub async fn daemon_running(socket: &Path) -> bool {
    matches!(probe_daemon(socket).await, DaemonProbe::Running)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[tokio::test]
    async fn request_with_timeout_bounds_a_daemon_that_never_replies() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("hung.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            // Accept the connection and then never write a reply.
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            std::future::pending::<()>().await;
            drop(stream);
        });

        let start = Instant::now();
        let err = request_with_timeout(&socket, &IpcRequest::Ping, Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "did not bound the round trip: took {:?}",
            start.elapsed()
        );
        assert!(matches!(err, RequestError::Timeout(_)), "{err:?}");
        assert!(err.to_string().contains("did not answer"), "{err}");
    }

    #[tokio::test]
    async fn request_to_a_missing_socket_is_a_connect_error_not_a_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let err = request_with_timeout(
            &tmp.path().join("absent.sock"),
            &IpcRequest::Ping,
            Duration::from_millis(100),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RequestError::Connect(_)), "{err:?}");
    }

    /// Minimal, directly-constructed `Task`. `task::tests` has its own builder but
    /// it lives in a private `mod tests` that isn't reachable from here, so this
    /// mirrors its shape instead of trying to reuse it.
    fn minimal_task() -> Task {
        let now = chrono::Utc::now();
        Task {
            description: None,
            id: 1,
            job: "run".into(),
            item: serde_json::Value::Null,
            prompt: "hi".into(),
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
            machine: None,
            workspace_id: None,
            pane_id: None,
            agent_name: None,
            state: crate::task::TaskState::Queued,
            error: None,
            last_completion_seq: None,
            prompt_pending: false,
            activity_seen: false,
            ended: false,
            retry_of: None,
            priority: Default::default(),
            priority_from: None,
            queue_pos: 0,
            pause: Default::default(),
            summary: None,
            created_at: now,
            started_at: None,
            finished_at: None,
            updated_at: now,
            flock: None,
            role: Default::default(),
        }
    }

    /// Every `IpcResponse` variant must round-trip through JSON: the newtype
    /// variants wrapping a `Vec` or `String` (`Tasks`, `Text`, `Machines`) broke
    /// under internal tagging (serde_json refuses to merge a `kind` field into a
    /// sequence or a string), which the adjacently-tagged `content = "data"`
    /// representation fixes for every shape uniformly. `Task(Task)` is included
    /// too: it's what `run` and `task describe` actually return over the wire.
    #[test]
    fn every_response_variant_round_trips_through_json() {
        let responses = vec![
            IpcResponse::Pong {
                version: "1".into(),
                protocol: IPC_PROTOCOL,
                role: Some(SHEPHERD_ROLE.into()),
            },
            IpcResponse::Task(minimal_task()),
            IpcResponse::Tasks(vec![minimal_task()]),
            IpcResponse::Text("hello".into()),
            IpcResponse::Machines(vec![]),
            IpcResponse::Runs(vec![]),
            IpcResponse::Jobs(vec![]),
            IpcResponse::Pruned(crate::store::PruneOutcome {
                pruned: 2,
                kept_worktrees: vec![3],
            }),
            IpcResponse::Events(crate::events::EventsPage {
                events: vec![crate::events::EventRecord {
                    summary: None,
                    seq: 812,
                    at: chrono::Utc::now(),
                    kind: "task.done".into(),
                    task: Some(minimal_task()),
                    job: Some("run".into()),
                    machine: None,
                    detail: None,
                    model: None,
                }],
                gap: false,
                oldest: Some(812),
                newest: Some(812),
            }),
            IpcResponse::File(FileText {
                path: "/c/flock.toml".into(),
                text: "a = 1\n".into(),
                hash: "00".into(),
            }),
            IpcResponse::JobSubmitted {
                tasks: vec![minimal_task()],
                skipped: vec!["k1".into()],
                refused: vec![("k2".into(), "max_tasks_per_run".into())],
            },
            IpcResponse::Summaries(vec![crate::task::TaskSummary {
                round: 1,
                outcome: crate::task::Outcome::NothingToDo,
                text: "nothing to do".into(),
                source: crate::task::SummarySource::Agent,
                at: chrono::Utc::now(),
            }]),
            IpcResponse::error("some_code", "some message"),
        ];
        for resp in responses {
            let json = serde_json::to_string(&resp).unwrap();
            let back: IpcResponse = serde_json::from_str(&json).unwrap();
            assert_eq!(format!("{resp:?}"), format!("{back:?}"), "{json}");
        }
    }

    #[test]
    fn scheduler_requests_round_trip() {
        for req in [
            IpcRequest::Tick {
                job: Some("j".into()),
                dry_run: true,
            },
            IpcRequest::Reload,
            IpcRequest::JobList,
            IpcRequest::JobRun { name: "j".into() },
            IpcRequest::FileGet {
                file: "job:j".into(),
            },
            IpcRequest::FilePut {
                file: "config".into(),
                text: "tick = \"5s\"\n".into(),
                base_hash: "ab".into(),
            },
            IpcRequest::JobDescribe { name: "j".into() },
            IpcRequest::JobSetEnabled {
                name: "j".into(),
                enabled: true,
            },
        ] {
            let json = serde_json::to_string(&req).unwrap();
            let back: IpcRequest = serde_json::from_str(&json).unwrap();
            assert_eq!(format!("{req:?}"), format!("{back:?}"), "{json}");
        }
    }

    /// Pin the adjacent-tagging wire shape itself for the two variants that used
    /// to be impossible to serialize at all: `Task`'s payload is a JSON object,
    /// `Tasks`'s is a JSON array, both carried under a `"data"` key alongside
    /// `"kind"`.
    #[test]
    fn task_and_tasks_carry_kind_and_data_with_the_expected_shape() {
        let v: serde_json::Value = serde_json::from_str(
            &serde_json::to_string(&IpcResponse::Task(minimal_task())).unwrap(),
        )
        .unwrap();
        assert_eq!(v["kind"], "task");
        assert!(v["data"].is_object(), "{v}");
        assert_eq!(v["data"]["id"], 1);

        let v: serde_json::Value = serde_json::from_str(
            &serde_json::to_string(&IpcResponse::Tasks(vec![minimal_task()])).unwrap(),
        )
        .unwrap();
        assert_eq!(v["kind"], "tasks");
        assert!(v["data"].is_array(), "{v}");
        assert_eq!(v["data"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn task_lifecycle_requests_round_trip() {
        for req in [
            IpcRequest::TaskRetry { id: 4, place: None },
            IpcRequest::TaskRetry {
                id: 4,
                place: Some(crate::task::Place::Pane("work".into())),
            },
            IpcRequest::TaskClose {
                id: 4,
                remove_worktree: true,
            },
            IpcRequest::TaskPrune {
                states: vec![crate::task::TaskState::Done, crate::task::TaskState::Closed],
                older_than_secs: 3 * 86400,
            },
            IpcRequest::EventsSince {
                after: 7,
                limit: 50,
                task: Some(3),
            },
            IpcRequest::TaskClaim {
                machine: "laptop".into(),
                free_slots: 2,
                flock_work: true,
            },
            IpcRequest::TaskReport {
                machine: "laptop".into(),
                id: 4,
                state: crate::task::TaskState::Blocked,
                pane: Some("p1".into()),
                detail: Some("agent asked: go on?".into()),
            },
        ] {
            let json = serde_json::to_string(&req).unwrap();
            let back: IpcRequest = serde_json::from_str(&json).unwrap();
            assert_eq!(format!("{req:?}"), format!("{back:?}"), "{json}");
        }
        let v = serde_json::to_value(IpcRequest::TaskPrune {
            states: vec![crate::task::TaskState::Failed],
            older_than_secs: 1,
        })
        .unwrap();
        assert_eq!(v["op"], "task_prune");
        assert_eq!(v["states"][0], "failed");
    }

    /// What an agent pastor started may still ask for: reads. Everything
    /// else changes the fleet (`IpcRequest::changes_fleet`), a reload and a
    /// dry tick included, since both apply pastor.toml and flock.toml.
    #[test]
    fn only_reads_leave_the_fleet_alone() {
        let reads = [
            IpcRequest::Ping,
            IpcRequest::List {
                filter: TaskFilter::default(),
            },
            IpcRequest::TaskShow { id: 1 },
            IpcRequest::TaskSummaries { id: 1 },
            IpcRequest::TaskRead { id: 1, lines: 5 },
            IpcRequest::FlockList,
            IpcRequest::JobList,
            IpcRequest::EventsSince {
                after: 0,
                limit: 10,
                task: None,
            },
            IpcRequest::FileGet {
                file: "flock".into(),
            },
            IpcRequest::JobDescribe { name: "j".into() },
            IpcRequest::TrustList,
            IpcRequest::FlockDescribe { name: "f".into() },
            IpcRequest::MachineDescribe { name: "m".into() },
            IpcRequest::Queue {
                flock: Some("work".into()),
                machine: None,
            },
        ];
        for req in reads {
            assert!(!req.changes_fleet(), "{req:?}");
        }
        for req in fleet_changes() {
            assert!(req.changes_fleet(), "{req:?}");
        }
    }

    /// One of every request that changes the fleet.
    fn fleet_changes() -> Vec<IpcRequest> {
        vec![
            IpcRequest::Run {
                preempt: false,
                summary: None,
                description: None,
                prompt: "p".into(),
                spec: minimal_task().spec,
                flock: None,
                agent: None,
                priority: None,
                role: TaskRole::Agent,
            },
            IpcRequest::TaskPriority {
                preempt: false,
                id: 1,
                priority: crate::task::Priority::High,
            },
            IpcRequest::QueueMove {
                id: 1,
                to: crate::queue::QueueSpot::Before(2),
            },
            IpcRequest::TaskSend {
                id: 1,
                input: crate::machine::SendInput::default(),
            },
            IpcRequest::TaskRetry { id: 1, place: None },
            IpcRequest::TaskClose {
                id: 1,
                remove_worktree: false,
            },
            IpcRequest::TaskPrune {
                states: vec![],
                older_than_secs: 0,
            },
            IpcRequest::FlockRemove { name: "f".into() },
            IpcRequest::FlockAdd {
                description: None,
                name: "f".into(),
                default: false,
            },
            IpcRequest::FlockSetDefault { name: "f".into() },
            IpcRequest::MachineAdd {
                machine: crate::config::flock::MachineConfig {
                    pull: false,
                    description: None,
                    name: "m".into(),
                    local: true,
                    ssh: None,
                    command: None,
                    session: "default".into(),
                    max_agents: 1,
                    job_slots: 1,
                    burst: 1,
                    tags: vec![],
                    flock: None,
                    agent: None,
                    agent_args: None,
                    model: None,
                    priority: None,
                    agents: Default::default(),
                    profile: None,
                },
            },
            IpcRequest::MachineRemove { name: "m".into() },
            IpcRequest::MachineMove {
                name: "m".into(),
                flock: "f".into(),
            },
            IpcRequest::Tick {
                job: None,
                dry_run: false,
            },
            IpcRequest::Tick {
                job: None,
                dry_run: true,
            },
            IpcRequest::Reload,
            IpcRequest::JobRun { name: "j".into() },
            IpcRequest::FilePut {
                file: "job:j".into(),
                text: String::new(),
                base_hash: String::new(),
            },
            IpcRequest::JobSetEnabled {
                name: "j".into(),
                enabled: false,
            },
            IpcRequest::JobSetEnabled {
                name: "j".into(),
                enabled: true,
            },
            IpcRequest::JobSubmit {
                job: "j".into(),
                dispatch: serde_json::Value::Null,
                prompt: "p".into(),
                items: vec![],
            },
            IpcRequest::TrustAdd {
                machine: "m".into(),
                repo: "/r".into(),
            },
            IpcRequest::TrustRemove {
                machine: "m".into(),
                repo: "/r".into(),
            },
        ]
    }

    /// An orchestrator may run, retry, type into and close tasks and enable
    /// or disable a job, and nothing else that changes the fleet: not a
    /// prune, not a file or machine edit.
    #[test]
    fn an_orchestrator_may_make_exactly_the_specs_changes() {
        for req in fleet_changes() {
            let allowed = matches!(
                req,
                IpcRequest::Run { .. }
                    | IpcRequest::TaskRetry { .. }
                    | IpcRequest::TaskSend { .. }
                    | IpcRequest::TaskClose { .. }
                    | IpcRequest::JobSetEnabled { .. }
            );
            assert_eq!(req.orchestrator_may(), allowed, "{req:?}");
        }
        for enabled in [true, false] {
            assert!(
                IpcRequest::JobSetEnabled {
                    name: "j".into(),
                    enabled
                }
                .orchestrator_may()
            );
        }
        assert!(
            IpcRequest::TaskClose {
                id: 1,
                remove_worktree: false
            }
            .orchestrator_may()
        );
        assert!(
            !IpcRequest::TaskPrune {
                states: vec![TaskState::Done],
                older_than_secs: 60,
            }
            .orchestrator_may()
        );
    }

    /// A plain agent's `Run` leaves `role` out, so an older head reads it;
    /// an orchestrator's names it, and both read back.
    #[test]
    fn run_names_its_role_only_when_not_a_plain_agent() {
        let run = |role| IpcRequest::Run {
            preempt: false,
            summary: None,
            prompt: "p".into(),
            spec: minimal_task().spec,
            flock: None,
            agent: None,
            priority: None,
            role,
            description: None,
        };
        let v = serde_json::to_value(run(TaskRole::Agent)).unwrap();
        assert!(v.get("role").is_none(), "{v}");
        let v = serde_json::to_value(run(TaskRole::Orchestrator)).unwrap();
        assert_eq!(v["role"], "orchestrator");
        let back: IpcRequest = serde_json::from_value(v).unwrap();
        assert!(matches!(
            back,
            IpcRequest::Run {
                role: TaskRole::Orchestrator,
                ..
            }
        ));
    }

    /// The CLI tells the head which task it runs in, beside the request's
    /// own fields, and says nothing outside one.
    #[test]
    fn a_request_line_carries_the_task_it_comes_from() {
        let line = request_line(&IpcRequest::FlockList, Some("t-4")).unwrap();
        assert!(line.ends_with('\n'));
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["op"], "flock_list");
        assert_eq!(v[FROM_TASK_FIELD], "t-4");
        let (req, from) = parse_request_line(line.trim()).unwrap();
        assert!(matches!(req, IpcRequest::FlockList));
        assert_eq!(from.as_deref(), Some("t-4"));

        let line = request_line(&IpcRequest::FlockList, None).unwrap();
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert!(v.get(FROM_TASK_FIELD).is_none(), "{v}");
        let (_, from) = parse_request_line(line.trim()).unwrap();
        assert_eq!(from, None);
        assert!(parse_request_line("not json").is_err());
    }

    /// A client refuses to send `JobSubmit` to a head older than
    /// `JOB_SUBMIT_PROTOCOL`, which would not read it.
    #[test]
    fn an_older_head_is_too_old_to_submit_to() {
        let err = check_protocol("0.5.0", 3, JOB_SUBMIT_PROTOCOL, "job submit").unwrap_err();
        let err = err.downcast_ref::<crate::cli::CliError>().unwrap();
        assert_eq!(err.code, "head_too_old");
        assert!(err.message.contains("0.5.0"), "{}", err.message);
        assert!(check_protocol("0.6.0", IPC_PROTOCOL, JOB_SUBMIT_PROTOCOL, "job submit").is_ok());
        let v = serde_json::to_value(IpcRequest::JobSubmit {
            job: "j".into(),
            dispatch: serde_json::json!({"repo": "r"}),
            prompt: "p".into(),
            items: vec![serde_json::json!({"key": "k"})],
        })
        .unwrap();
        assert_eq!(v["op"], "job_submit");
        assert_eq!(v["items"][0]["key"], "k");
    }

    #[tokio::test]
    async fn probe_daemon_reports_not_running_for_a_missing_socket() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("nothing-here.sock");
        assert_eq!(probe_daemon(&socket).await, DaemonProbe::NotRunning);
        assert!(!daemon_running(&socket).await);
    }
}
