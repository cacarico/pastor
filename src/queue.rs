//! `pastor queue`: the queued tasks in the order dispatch takes them
//! (`Store::queued_tasks`), with how long each has waited and why it has
//! not started yet, and `pastor queue move` to put one elsewhere in it
//! (`Store::move_queued`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cli::age;
use crate::dispatch::{
    Claim, MachineView, flock_held, mark_waiting_under_share, pick_machine_where,
};
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
    /// The machine it is pinned to (or paused on), else its flock.
    pub fn place(&self) -> String {
        match self.task.pinned_machine() {
            Some(m) => format!("machine {m}"),
            None => format!("flock {}", self.flock),
        }
    }

    /// Its level, and the one it had before it aged: `high (was low)`.
    pub fn level(&self) -> String {
        match self.task.aged_from {
            Some(was) => format!("{} (was {was})", self.task.priority),
            None => self.task.priority.to_string(),
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
            && machine.is_none_or(|m| self.task.pinned_machine() == Some(m))
    }

    pub fn to_json(&self) -> Value {
        let waited = (chrono::Utc::now() - self.task.created_at)
            .num_seconds()
            .max(0);
        serde_json::json!({
            "pos": self.pos,
            "id": self.task.display_id(),
            "priority": self.task.priority,
            "aged_from": self.task.aged_from,
            "where": self.place(),
            "flock": self.flock,
            "machine": self.task.pinned_machine(),
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
    (0..queue.len())
        .map(|i| {
            let task = &queue[i];
            let flock = task
                .flock
                .clone()
                .unwrap_or_else(|| default_flock.to_string());
            let later = &queue[i + 1..];
            mark_waiting_under_share(&mut views, &flock, later, default_flock, accepts);
            let why = why_waiting(task, &flock, &mut views, accepts);
            QueueEntry {
                pos: i + 1,
                flock,
                why,
                task: task.clone(),
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
    let claim = Claim::of(task);
    // A paused task resumes only on its machine, whatever its flock.
    if task.state == crate::task::TaskState::Paused {
        let m = task.machine.as_deref().unwrap_or("-");
        let by = task
            .pause
            .paused_for
            .map(|id| format!("paused for {}; ", Task::agent_name_for(id)))
            .unwrap_or_default();
        return match views.iter_mut().find(|v| v.name == m) {
            None => format!("{by}machine {m} is not in the flock"),
            Some(v) if !v.healthy => format!("{by}machine {m} is not connected"),
            Some(v) if v.has_room(claim) => {
                v.live += 1;
                v.live_jobs += usize::from(task.from_job());
                format!("next pass: resumes on {m}")
            }
            Some(v) => format!("{by}machine {m} is full ({}/{})", v.live, v.max_agents),
        };
    }
    if let Some(m) = pick_machine_where(views, flock, &task.spec, claim, &|m| accepts(task, m)) {
        let v = views.iter_mut().find(|v| v.name == m).expect("picked");
        v.take(flock, claim);
        return format!("next pass: {m} has room");
    }
    let tags = &task.spec.tags;
    let has_tags = |v: &MachineView| tags.iter().all(|t| v.tags.contains(t));
    // The flock at its number on `v`, or past its share while another
    // waits, as `dispatch_queued` notes it.
    let at_number = |v: &MachineView| flock_held(v, flock);
    if let Some(pinned) = &task.spec.machine {
        return match views.iter().find(|v| &v.name == pinned) {
            None => format!("machine {pinned} is not in the flock"),
            Some(v) if !v.in_flock(flock) => {
                let names: Vec<&str> = v.flocks.iter().map(|f| f.name.as_str()).collect();
                let noun = if names.len() == 1 { "flock" } else { "flocks" };
                format!(
                    "machine {pinned} is in {noun} {}, not {flock}",
                    names.join(", ")
                )
            }
            Some(v) if !v.healthy => format!("machine {pinned} is not connected"),
            Some(v) if !has_tags(v) => format!("machine {pinned} lacks tags {}", tags.join(", ")),
            Some(v) if v.has_room(claim) && at_number(v).is_some() => {
                at_number(v).expect("checked")
            }
            Some(v) => format!("machine {pinned} is full ({}/{})", v.live, v.max_agents),
        };
    }
    let mine: Vec<&MachineView> = views.iter().filter(|v| v.in_flock(flock)).collect();
    if mine.is_empty() {
        format!("flock {flock} has no machines")
    } else if !mine.iter().any(|v| v.healthy) {
        format!("no machine in flock {flock} is connected")
    } else if !mine.iter().any(|v| v.healthy && has_tags(v)) {
        format!("no machine in flock {flock} has tags {}", tags.join(", "))
    } else if let Some(why) = mine
        .iter()
        .filter(|v| v.healthy && has_tags(v) && v.has_room(claim))
        .find_map(|v| at_number(v))
    {
        why
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
                e.level(),
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
    use crate::task::{DispatchSpec, Priority, TaskRole, TaskState};
    use chrono::Utc;

    fn task(id: i64, flock: Option<&str>, machine: Option<&str>) -> Task {
        let now = Utc::now() - chrono::Duration::seconds(90);
        Task {
            id,
            job: "run".into(),
            item: serde_json::json!({}),
            prompt: "p".into(),
            description: None,
            spec: DispatchSpec {
                machine: machine.map(str::to_string),
                ..serde_json::from_value(serde_json::json!({"agent": "claude"})).unwrap()
            },
            state: TaskState::Queued,
            role: TaskRole::Agent,
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
            aged_from: None,
            aged_at: None,
            pause: Default::default(),
            summary: None,
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
            job_slots: 0,
            burst: 0,
            tags: vec![],
            live,
            live_jobs: 0,
            healthy,
            flocks: vec![crate::dispatch::FlockSeat {
                name: flock.into(),
                share: None,
                max: None,
                live,
            }],
            waiting_under_share: vec![],
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

    /// A paused job task that the simulation resumes takes the machine's
    /// one job slot, so a job task queued behind it reads that slot as
    /// taken instead of still free.
    #[test]
    fn a_resumed_paused_job_task_takes_the_job_slot_in_the_simulation() {
        let mut paused = task(3, Some("default"), None);
        paused.job = "board".into();
        paused.state = TaskState::Paused;
        paused.machine = Some("b".into());
        let mut behind = task(4, Some("default"), None);
        behind.job = "board".into();
        let mut b = view("b", "default", 0, 0, true);
        b.job_slots = 1;
        assert_eq!(
            whys(vec![paused, behind], &[b]),
            ["next pass: resumes on b", "flock default is full",],
            "the job slot the paused task takes is not free twice"
        );
    }

    /// A paused task waits on the machine it was paused on, whatever its
    /// flock or pin, and says for whom it was paused.
    #[test]
    fn a_paused_task_waits_on_its_own_machine() {
        let mut paused = task(3, Some("default"), None);
        paused.state = TaskState::Paused;
        paused.machine = Some("b".into());
        paused.pause.paused_for = Some(9);
        let full = [
            view("a", "default", 0, 2, true),
            view("b", "work", 1, 1, true),
        ];
        let e = &entries(vec![paused.clone()], &full, "default", &|_, _| true)[0];
        assert_eq!(e.why, "paused for t-9; machine b is full (1/1)");
        assert_eq!(e.place(), "machine b");
        assert!(e.matches(None, Some("b")));
        let free = [view("b", "work", 0, 1, true)];
        assert_eq!(whys(vec![paused], &free), ["next pass: resumes on b"]);
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

    /// A flock at its number on a machine with room waits with that as the
    /// reason, and the task behind it in another flock goes.
    #[test]
    fn a_flock_at_its_number_says_so_and_the_next_task_goes() {
        let seat = |name: &str, max: u32, live: usize| crate::dispatch::FlockSeat {
            name: name.into(),
            share: None,
            max: Some(max),
            live,
        };
        let views = [MachineView {
            flocks: vec![seat("default", 3, 0), seat("work", 2, 1)],
            ..view("desk", "default", 1, 4, true)
        }];
        assert_eq!(
            whys(
                vec![
                    task(1, Some("work"), None),
                    task(2, Some("work"), Some("desk")),
                    task(3, Some("work"), None),
                    task(4, None, None),
                ],
                &views
            ),
            [
                "next pass: desk has room",
                "flock work is at 2 of 2 on desk",
                "flock work is at 2 of 2 on desk",
                "next pass: desk has room",
            ]
        );
    }

    /// A flock past its share waits while a flock under its share has a
    /// task behind it; once that one has its slot, the next reads the
    /// machine as idle enough. At its max it reads so.
    #[test]
    fn a_flock_past_its_share_waits_for_one_under_it() {
        use crate::config::flock::FlockNumber;
        let views = [MachineView {
            flocks: vec![
                crate::dispatch::FlockSeat::new("work", Some(FlockNumber::split(1, 3)), 1),
                crate::dispatch::FlockSeat::new("home", Some(FlockNumber::plain(2)), 1),
            ],
            ..view("desk", "default", 2, 4, true)
        }];
        assert_eq!(
            whys(
                vec![
                    task(1, Some("work"), None),
                    task(2, Some("home"), None),
                    task(3, Some("work"), None),
                    task(4, Some("work"), None),
                ],
                &views
            ),
            [
                "flock work is past its share on desk, at 1 of 1/3, while flock home waits under its share",
                "next pass: desk has room",
                "next pass: desk has room",
                "flock work is full",
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
        assert_eq!(json["aged_from"], serde_json::Value::Null);
    }

    /// An aged task shows its level with the one it had before it aged.
    #[test]
    fn an_aged_task_shows_its_own_level() {
        let mut t = task(1, None, None);
        t.priority = Priority::High;
        t.aged_from = Some(Priority::Low);
        let e = entries(vec![t], &[], "default", &|_, _| true);
        assert_eq!(rows(&e)[0][2], "high (was low)");
        let json = e[0].to_json();
        assert_eq!(json["priority"], "high");
        assert_eq!(json["aged_from"], "low");
        assert_eq!(json["task"]["aged_from"], "low");
    }
}
