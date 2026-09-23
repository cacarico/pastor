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
    pub last_completion_seq: Option<u64>,
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
        completion_seq: Option<u64>,
    },
    PaneClosed,
    PaneExited,
    /// Dispatch is about to make its first herdr call. Not something herdr reports;
    /// the actor emits it itself so the one legal source transition (only a queued
    /// task may start) lives with the rest of the state machine instead of being
    /// hard-coded where dispatch happens.
    DispatchStarting,
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
        Observed::PaneExited => {
            if task.state == Done {
                Closed
            } else {
                Failed
            }
        }
        Observed::Status {
            status,
            completion_seq,
        } => match status {
            AgentStatus::Working => Running,
            AgentStatus::Blocked => Blocked,
            AgentStatus::Unknown => return None,
            AgentStatus::Idle | AgentStatus::Done => {
                let advanced = match (completion_seq, task.last_completion_seq) {
                    (Some(new), Some(old)) => *new > old,
                    (Some(_), None) => true,
                    (None, _) => false,
                };
                if advanced {
                    Done
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
                repo: None,
                worktree: false,
                branch: None,
                machine: None,
                tags: vec![],
                timeout_secs: 10,
            },
            machine: Some("pi-1".into()),
            workspace_id: Some("w1".into()),
            pane_id: Some("w1:p1".into()),
            agent_name: Some("t-1".into()),
            state,
            error: None,
            last_completion_seq,
            created_at: now,
            started_at: Some(now),
            finished_at: None,
            updated_at: now,
        }
    }

    fn status(s: AgentStatus, seq: Option<u64>) -> Observed {
        Observed::Status {
            status: s,
            completion_seq: seq,
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
    fn idle_is_done_only_when_completion_seq_advances() {
        assert_eq!(
            next_state(
                &task(TaskState::Running, None),
                &status(AgentStatus::Idle, Some(1))
            ),
            Some(TaskState::Done)
        );
        assert_eq!(
            next_state(
                &task(TaskState::Running, Some(1)),
                &status(AgentStatus::Done, Some(2))
            ),
            Some(TaskState::Done)
        );
        assert_eq!(
            next_state(
                &task(TaskState::Running, Some(2)),
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

    /// A `Starting` task adopted after a crash (see `Actor::reconcile`) can be
    /// found idle before it ever produces completed work: dispatch reached at
    /// least `agent.start`, so that counts as reaching `Running`, the same as a
    /// successful dispatch would have recorded, not as reaching `Done` (that
    /// still requires completion_seq to advance) or being left stuck.
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
        // A real completion_seq advance still wins: an adopted agent that has
        // already finished its work goes straight to Done, same as any other
        // state (this is `next_state`'s call; `Actor::reconcile` never lets an
        // adopted agent's real completion_seq reach here directly — it passes
        // `None` and routes the real value through the settle window instead).
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
            next_state(&task(TaskState::Closed, None), &Observed::PaneExited),
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
            next_state(&task(TaskState::Running, None), &Observed::PaneExited),
            Some(TaskState::Failed)
        );
        assert_eq!(
            next_state(&task(TaskState::Done, Some(1)), &Observed::PaneExited),
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
}
