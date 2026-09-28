//! `pastor queue`: the queued tasks in the order dispatch takes them
//! (`Store::queued_tasks`), with how long each has waited and why it has
//! not started yet, and `pastor queue move` to put one elsewhere in it
//! (`Store::move_queued`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cli::age;
use crate::dispatch::{MachineView, pick_machine_where};
use crate::task::Task;

/// Where `pastor queue move` puts a task. Positions count from 1 over the
/// whole queue, as `pastor queue` numbers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueSpot {
    /// First in the queue; only ever lifts the task's level.
    Top,
    /// Just ahead of this task.
    Before(i64),
    /// Just behind this task.
    After(i64),
    /// At this position, or last when the queue is shorter.
    To(usize),
}

/// One queued task as `pastor queue` shows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueEntry {
    /// Its place in the whole queue, from 1, whatever the filter shows.
    pub pos: usize,
    /// The flock it waits in: its own, or the default one.
    pub flock: String,
    /// Why it has not started yet (`waiting_reasons`).
    pub why: String,
    pub task: Task,
}

pub const QUEUE_HEADER: [&str; 7] = [
    "POS",
    "TASK",
    "LEVEL",
    "WHERE",
    "FROM",
    "WAITED",
    "WHY NOT YET",
];

/// How a queued task's error starts while no machine in its flock runs an
/// agent its model suits; cleared once one takes it.
pub const WAITING_FOR_MODEL: &str = "waiting for a machine";

impl QueueEntry {
    /// The machine it is pinned to, else its flock.
    pub fn place(&self) -> String {
        match &self.task.spec.machine {
            Some(m) => format!("machine {m}"),
            None => format!("flock {}", self.flock),
        }
    }

    /// Who queued it: `task run` or `job <name>`.
    pub fn from(&self) -> String {
        asked_by(&self.task)
    }

    /// Whether `--flock` and `--machine` keep it: its flock, and the
    /// machine it is pinned to.
    pub fn matches(&self, flock: Option<&str>, machine: Option<&str>) -> bool {
        flock.is_none_or(|f| f == self.flock)
            && machine.is_none_or(|m| self.task.spec.machine.as_deref() == Some(m))
    }

    pub fn to_json(&self) -> Value {
        let waited = (chrono::Utc::now() - self.task.created_at)
            .num_seconds()
            .max(0);
        serde_json::json!({
            "pos": self.pos,
            "id": self.task.display_id(),
            "priority": self.task.priority,
            "where": self.place(),
            "flock": self.flock,
            "machine": self.task.spec.machine,
            "from": self.from(),
            "waited_secs": waited,
            "why": self.why,
            "task": self.task.to_json(),
        })
    }
}

/// Who asked for `task`, as `AgentSource` and `pastor queue` label it.
pub fn asked_by(task: &Task) -> String {
    if !task.from_job() {
        "task run".to_string()
    } else {
        format!("job {}", task.job)
    }
}

/// `queue` (in dispatch order) as entries, each with why it waits on
/// `machines`: a dispatch pass is played through, in order, each task that
/// fits taking its slot, so a task behind them reads its flock as full.
/// `accepts` is `dispatch_queued`'s model/agent compatibility check: a
/// machine whose agent cannot run the task's model does not take it.
pub fn entries(
    queue: Vec<Task>,
    machines: &[MachineView],
    default_flock: &str,
    accepts: &dyn Fn(&Task, &str) -> bool,
) -> Vec<QueueEntry> {
    let mut views = machines.to_vec();
    queue
        .into_iter()
        .enumerate()
        .map(|(i, task)| {
            let flock = task
                .flock
                .clone()
                .unwrap_or_else(|| default_flock.to_string());
            let why = why_waiting(&task, &flock, &mut views, accepts);
            QueueEntry {
                pos: i + 1,
                flock,
                why,
                task,
            }
        })
        .collect()
}

/// Why `task` waits in `flock` on `views`; a task that fits takes its slot
/// in `views`, as the pass would.
fn why_waiting(
    task: &Task,
    flock: &str,
    views: &mut [MachineView],
    accepts: &dyn Fn(&Task, &str) -> bool,
) -> String {
    if let Some(note) = &task.error
        && note.starts_with(WAITING_FOR_MODEL)
    {
        return note.clone();
    }
    if let Some(m) = pick_machine_where(views, flock, &task.spec, &|m| accepts(task, m)) {
        let v = views.iter_mut().find(|v| v.name == m).expect("picked");
        v.live += 1;
        return format!("next pass: {m} has room");
    }
    let tags = &task.spec.tags;
    let has_tags = |v: &MachineView| tags.iter().all(|t| v.tags.contains(t));
    if let Some(pinned) = &task.spec.machine {
        return match views.iter().find(|v| &v.name == pinned) {
            None => format!("machine {pinned} is not in the flock"),
            Some(v) if v.flock != flock => {
                format!("machine {pinned} is in flock {}, not {flock}", v.flock)
            }
            Some(v) if !v.healthy => format!("machine {pinned} is not connected"),
            Some(v) if !has_tags(v) => format!("machine {pinned} lacks tags {}", tags.join(", ")),
            Some(v) => format!("machine {pinned} is full ({}/{})", v.live, v.max_agents),
        };
    }
    let mine: Vec<&MachineView> = views.iter().filter(|v| v.flock == flock).collect();
    if mine.is_empty() {
        format!("flock {flock} has no machines")
    } else if !mine.iter().any(|v| v.healthy) {
        format!("no machine in flock {flock} is connected")
    } else if !mine.iter().any(|v| v.healthy && has_tags(v)) {
        format!("no machine in flock {flock} has tags {}", tags.join(", "))
    } else {
        format!("flock {flock} is full")
    }
}

/// `pastor queue`'s table rows, `QUEUE_HEADER`'s columns.
pub fn rows(entries: &[QueueEntry]) -> Vec<Vec<String>> {
    entries
        .iter()
        .map(|e| {
            vec![
                e.pos.to_string(),
                e.task.display_id(),
                e.task.priority.to_string(),
                e.place(),
                e.from(),
                age(e.task.created_at),
                e.why.clone(),
            ]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{DispatchSpec, Priority, TaskState};
    use chrono::Utc;

    fn task(id: i64, flock: Option<&str>, machine: Option<&str>) -> Task {
        let now = Utc::now() - chrono::Duration::seconds(90);
        Task {
            id,
            job: "run".into(),
            item: serde_json::json!({}),
            prompt: "p".into(),
            spec: DispatchSpec {
                machine: machine.map(str::to_string),
                ..serde_json::from_value(serde_json::json!({"agent": "claude"})).unwrap()
            },
            state: TaskState::Queued,
            machine: None,
            workspace_id: None,
            pane_id: None,
            agent_name: None,
            last_completion_seq: None,
            error: None,
            prompt_pending: false,
            activity_seen: false,
            ended: false,
            retry_of: None,
            flock: flock.map(str::to_string),
            priority: Priority::Normal,
            priority_from: None,
            queue_pos: id,
            created_at: now,
            started_at: None,
            finished_at: None,
            updated_at: now,
        }
    }

    fn view(name: &str, flock: &str, live: usize, max: u32, healthy: bool) -> MachineView {
        MachineView {
            name: name.into(),
            max_agents: max,
            tags: vec![],
            live,
            healthy,
            flock: flock.into(),
        }
    }

    fn whys(queue: Vec<Task>, views: &[MachineView]) -> Vec<String> {
        entries(queue, views, "default", &|_, _| true)
            .into_iter()
            .map(|e| e.why)
            .collect()
    }

    /// The pass is played through: the first task that fits takes the last
    /// free slot, and the one behind it reads its flock as full.
    #[test]
    fn a_task_behind_the_last_free_slot_reads_its_flock_full() {
        let views = [view("a", "default", 1, 2, true)];
        assert_eq!(
            whys(vec![task(1, None, None), task(2, None, None)], &views),
            ["next pass: a has room", "flock default is full"]
        );
    }

    /// Each reason a flock or a pinned machine takes nothing is named.
    #[test]
    fn why_names_what_keeps_a_task_waiting() {
        let views = [
            view("a", "default", 2, 2, true),
            view("b", "work", 0, 2, false),
            view("c", "lab", 0, 2, true),
        ];
        let mut tagged = task(6, Some("lab"), None);
        tagged.spec.tags = vec!["gpu".into()];
        let mut model = task(7, None, None);
        model.error = Some(format!("{WAITING_FOR_MODEL}: no agent runs gpt"));
        assert_eq!(
            whys(
                vec![
                    task(1, Some("play"), None),
                    task(2, Some("work"), None),
                    task(3, None, Some("a")),
                    task(4, None, Some("c")),
                    task(5, None, Some("z")),
                    tagged,
                    model,
                    task(8, Some("work"), Some("b")),
                ],
                &views
            ),
            [
                "flock play has no machines",
                "no machine in flock work is connected",
                "machine a is full (2/2)",
                "machine c is in flock lab, not default",
                "machine z is not in the flock",
                "no machine in flock lab has tags gpu",
                "waiting for a machine: no agent runs gpt",
                "machine b is not connected",
            ]
        );
    }

    /// The rows follow the header; WHERE is the pinned machine or the
    /// flock, FROM who queued it, and the filters keep a flock's tasks or a
    /// machine's pinned ones, numbered in the whole queue.
    #[test]
    fn rows_and_filters() {
        let mut job = task(2, Some("work"), Some("pi-1"));
        job.job = "nightly".into();
        job.priority = Priority::High;
        let e = entries(vec![task(1, None, None), job], &[], "default", &|_, _| true);
        let rows = rows(&e);
        assert_eq!(QUEUE_HEADER.len(), rows[0].len());
        assert_eq!(
            rows[0][..5],
            ["1", "t-1", "normal", "flock default", "task run"]
        );
        assert_eq!(
            rows[1][..5],
            ["2", "t-2", "high", "machine pi-1", "job nightly"]
        );
        assert_eq!(rows[1][5], "1m");
        let kept: Vec<usize> = e
            .iter()
            .filter(|e| e.matches(Some("work"), None))
            .map(|e| e.pos)
            .collect();
        assert_eq!(kept, [2]);
        assert!(e[1].matches(None, Some("pi-1")));
        assert!(!e[0].matches(None, Some("pi-1")));
        assert!(e[0].matches(Some("default"), None));
        let json = e[1].to_json();
        assert_eq!(json["pos"], 2);
        assert_eq!(json["id"], "t-2");
        assert_eq!(json["priority"], "high");
        assert_eq!(json["where"], "machine pi-1");
        assert_eq!(json["from"], "job nightly");
        assert!(json["waited_secs"].as_i64().unwrap() >= 90);
        assert_eq!(json["task"]["id"], 2);
    }
}
