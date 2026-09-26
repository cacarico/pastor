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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSource {
    pub ask: crate::config::AgentChoice,
    pub agent: String,
    /// `None` when the agent runs with no args because no layer set any
    /// for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_args: Option<String>,
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

fn default_timeout() -> u64 {
    2 * 60 * 60
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
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

impl Task {
    pub fn display_id(&self) -> String {
        format!("t-{}", self.id)
    }
    pub fn agent_name_for(id: i64) -> String {
        format!("t-{id}")
    }
}

pub fn parse_task_id(s: &str) -> Option<i64> {
    s.strip_prefix("t-").unwrap_or(s).parse().ok()
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
const MESSAGE_MARKERS: [char; 2] = ['●', '⏺'];

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
                    // Running/Blocked).
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
mod tests {
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

    pub fn task(state: TaskState, last_completion_seq: Option<u64>) -> Task {
        let now = Utc::now();
        Task {
            id: 1,
            job: "run".into(),
            item: Value::Null,
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
                timeout_secs: 10,
                checkout: None,
                reopen: None,
                agent_source: None,
                place: Default::default(),
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
            created_at: now,
            started_at: Some(now),
            finished_at: None,
            updated_at: now,
            flock: None,
        }
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

    #[test]
    fn ids() {
        assert_eq!(parse_task_id("t-12"), Some(12));
        assert_eq!(parse_task_id("12"), Some(12));
        assert_eq!(parse_task_id("x"), None);
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
}
