use chrono::Utc;
use serde::Serialize;

use crate::machine::MachineStatus;
use crate::scheduler::{JobRunReport, JobStatus};
use crate::task::Task;

/// A runtime CLI error with a stable code. A library command handler returns
/// it (inside `anyhow::Error`) and `main` prints it as the usual JSON on
/// stderr with that code, instead of the generic `runtime_error`.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct CliError {
    pub code: String,
    pub message: String,
}

impl CliError {
    pub fn err(code: &str, message: impl std::fmt::Display) -> anyhow::Error {
        CliError {
            code: code.into(),
            message: message.to_string(),
        }
        .into()
    }
}

pub fn age(from: chrono::DateTime<Utc>) -> String {
    let secs = (Utc::now() - from).num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

pub fn table(header: &[&str], rows: &[Vec<String>]) -> String {
    let cols = header.len();
    let mut widths: Vec<usize> = header.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(cols) {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let fmt = |cells: &[String]| -> String {
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if i + 1 == cols {
                    c.clone()
                } else {
                    format!("{:<w$}", c, w = widths[i])
                }
            })
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_string()
    };
    let mut out = fmt(&header.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    for row in rows {
        out.push('\n');
        out.push_str(&fmt(row));
    }
    out
}

pub fn task_rows(tasks: &[Task]) -> Vec<Vec<String>> {
    tasks
        .iter()
        .map(|t| {
            let note = t
                .error
                .clone()
                .or_else(|| {
                    t.item
                        .get("title")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| {
                    t.prompt
                        .lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(60)
                        .collect()
                });
            let note = match t.retry_of {
                Some(of) => format!("retry of t-{of}: {note}"),
                None => note,
            };
            vec![
                t.display_id(),
                t.state.to_string(),
                t.machine.clone().unwrap_or_else(|| "-".into()),
                t.spec.agent.clone(),
                t.job.clone(),
                age(t.created_at),
                note,
            ]
        })
        .collect()
}

/// Errors carry raw stderr, newlines included; a human line (an events
/// record, a `task show` field) must stay one line. The escapes keep what was
/// there visible, as JSON does, rather than folding it into spaces that read
/// like the original text.
pub fn one_line(s: &str) -> String {
    s.replace('\r', "\\r").replace('\n', "\\n")
}

/// `pastor task show`: every field a human asks about one task, one per line,
/// then the prompt. The agent args are shell-quoted, so the line reads as the
/// command herdr runs.
pub fn task_detail(t: &Task) -> String {
    let opt = |v: &Option<String>| v.clone().unwrap_or_else(|| "-".into());
    let when = |v: Option<chrono::DateTime<Utc>>| {
        v.map(|at| format!("{} ({} ago)", at.format("%Y-%m-%d %H:%M:%S UTC"), age(at)))
            .unwrap_or_else(|| "-".into())
    };
    let args = if t.spec.agent_args.is_empty() {
        "-".to_string()
    } else {
        t.spec
            .agent_args
            .iter()
            .map(|a| crate::herdr::shell_quote(a))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut repo = opt(&t.spec.repo);
    if t.spec.worktree {
        repo.push_str(" (worktree");
        if let Some(b) = &t.spec.branch {
            repo.push_str(&format!(", branch {b}"));
        }
        repo.push(')');
    }
    let tags = if t.spec.tags.is_empty() {
        "-".to_string()
    } else {
        t.spec.tags.join(",")
    };
    let mut fields = vec![
        ("id", t.display_id()),
        ("state", t.state.to_string()),
        ("job", t.job.clone()),
        ("machine", opt(&t.machine)),
        ("agent", t.spec.agent.clone()),
        ("agent args", args),
        ("repo", repo),
        ("tags", tags),
        ("timeout", format!("{}s", t.spec.timeout_secs)),
        ("pane", opt(&t.pane_id)),
        ("created", when(Some(t.created_at))),
        ("started", when(t.started_at)),
        ("finished", when(t.finished_at)),
    ];
    if let Some(e) = &t.error {
        fields.push(("error", one_line(e)));
    }
    let mut out: Vec<String> = fields
        .into_iter()
        .map(|(k, v)| format!("{:<12}{v}", format!("{k}:")))
        .collect();
    out.push("prompt:".into());
    out.extend(
        t.prompt
            .lines()
            .map(|l| format!("  {l}").trim_end().to_string()),
    );
    out.join("\n")
}

pub const TASK_HEADER: [&str; 7] = ["ID", "STATE", "MACHINE", "AGENT", "JOB", "AGE", "NOTE"];

/// One line per orphaned agent, for under the `pastor list` table. With
/// `machine`, only that machine's, as `task list --machine` shows only its
/// tasks.
pub fn orphan_lines(ms: &[MachineStatus], machine: Option<&str>) -> Vec<String> {
    ms.iter()
        .filter(|m| machine.is_none_or(|name| m.name == name))
        .flat_map(|m| {
            m.orphans.iter().map(move |o| {
                format!(
                    "orphan {o} on {}: an agent no open task owns; `pastor task close {o}` closes it",
                    m.name
                )
            })
        })
        .collect()
}

/// The first row of `machine list`: the head itself. It runs no tasks, so it
/// is not a machine and has no agents, tags or error of its own. Its
/// `pastor_version` is this binary's, which is always known.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HeadRow {
    pub name: String,
    pub host: String,
    pub channel: String,
    pub herdr_version: Option<String>,
    pub pastor_version: String,
}

impl HeadRow {
    pub fn new(host: String, herdr_version: Option<String>) -> HeadRow {
        HeadRow {
            name: "pastor".into(),
            host,
            channel: "head".into(),
            herdr_version,
            pastor_version: env!("CARGO_PKG_VERSION").into(),
        }
    }
}

/// The version in `herdr --version` output (`herdr 0.9.1`).
pub fn herdr_version_from(output: &str) -> Option<String> {
    output
        .lines()
        .next()?
        .split_whitespace()
        .last()
        .map(str::to_string)
}

/// One machine in `machine list`, from the head's live view or from a direct
/// probe when no head runs. The fields and names are `MachineStatus`'s, so the
/// JSON matches what the events log carries; `channel` is a plain string
/// because a probe reports one of `probed`, `server down`, `unreachable` or
/// `error`, none of which is a live channel state, and `live` is absent when
/// a probe could not count the agents. `pastor_version` is the pastor
/// installed on the machine, `null` when there is none or it cannot be known
/// (always for a `command` machine).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MachineRow {
    pub name: String,
    pub host: String,
    pub endpoint: String,
    pub channel: String,
    pub herdr_version: Option<String>,
    pub pastor_version: Option<String>,
    pub protocol: Option<u32>,
    pub error: Option<String>,
    pub live: Option<usize>,
    pub max_agents: u32,
    pub tags: Vec<String>,
    /// `MachineStatus::orphans`; a probe works them out itself from
    /// `agent.list` and the store, and leaves them empty when it cannot.
    pub orphans: Vec<String>,
}

impl From<&MachineStatus> for MachineRow {
    fn from(m: &MachineStatus) -> MachineRow {
        MachineRow {
            name: m.name.clone(),
            host: m.host.clone(),
            endpoint: m.endpoint.clone(),
            channel: m.channel.to_string(),
            herdr_version: m.herdr_version.clone(),
            pastor_version: m.pastor_version.clone(),
            protocol: m.protocol,
            error: m.error.clone(),
            live: Some(m.live),
            max_agents: m.max_agents,
            tags: m.tags.clone(),
            orphans: m.orphans.clone(),
        }
    }
}

/// AGENTS counts orphans too; ORPHANS names them (see `MachineStatus::orphans`).
pub const MACHINE_HEADER: [&str; 9] = [
    "NAME", "HOST", "CHANNEL", "HERDR", "PASTOR", "AGENTS", "ORPHANS", "TAGS", "ERROR",
];

/// The head's row, then one per machine.
pub fn machine_rows(head: &HeadRow, ms: &[MachineRow]) -> Vec<Vec<String>> {
    let dash = || "-".to_string();
    let head_row = vec![
        head.name.clone(),
        head.host.clone(),
        head.channel.clone(),
        head.herdr_version.clone().unwrap_or_else(dash),
        head.pastor_version.clone(),
        dash(),
        dash(),
        dash(),
        String::new(),
    ];
    std::iter::once(head_row)
        .chain(ms.iter().map(|m| {
            vec![
                m.name.clone(),
                m.host.clone(),
                m.channel.clone(),
                m.herdr_version.clone().unwrap_or_else(dash),
                m.pastor_version.clone().unwrap_or_else(dash),
                format!(
                    "{}/{}",
                    m.live.map_or_else(dash, |n| n.to_string()),
                    m.max_agents
                ),
                if m.orphans.is_empty() {
                    dash()
                } else {
                    m.orphans.join(",")
                },
                if m.tags.is_empty() {
                    dash()
                } else {
                    m.tags.join(",")
                },
                m.error.clone().unwrap_or_default(),
            ]
        }))
        .collect()
}

/// `machine list --json`: the head under its own key so `machines` holds
/// machines only. A struct rather than `json!` so fields keep their order.
#[derive(Debug, Serialize)]
pub struct MachineList<'a> {
    pub head: &'a HeadRow,
    pub machines: &'a [MachineRow],
}

pub fn machine_list_json<'a>(head: &'a HeadRow, ms: &'a [MachineRow]) -> MachineList<'a> {
    MachineList { head, machines: ms }
}

/// "in 4m" for the future, "12s ago" for the past, "now" within a second.
pub fn in_(at: chrono::DateTime<Utc>) -> String {
    let delta = at - Utc::now();
    if delta.num_seconds().abs() < 1 {
        return "now".into();
    }
    if delta > chrono::Duration::zero() {
        format!("in {}", age(Utc::now() - delta))
    } else {
        format!("{} ago", age(at))
    }
}

pub const JOB_HEADER: [&str; 7] = [
    "NAME",
    "SCHEDULE",
    "ENABLED",
    "CONNECTOR",
    "LAST RUN",
    "NEXT",
    "RESULT",
];

pub fn job_rows(jobs: &[JobStatus]) -> Vec<Vec<String>> {
    jobs.iter()
        .map(|j| {
            let mut result = match &j.error {
                Some(e) => format!("invalid: {e}"),
                None => j.last_result.clone().unwrap_or_default(),
            };
            if j.running {
                result.push_str(" (running)");
            }
            vec![
                j.name.clone(),
                j.schedule.clone().unwrap_or_else(|| "-".into()),
                if j.enabled { "yes" } else { "no" }.into(),
                j.connector.clone().unwrap_or_else(|| "-".into()),
                j.last_run_at
                    .map(|t| format!("{} ago", age(t)))
                    .unwrap_or_else(|| "never".into()),
                j.next_due.map(in_).unwrap_or_else(|| "-".into()),
                result.trim().to_string(),
            ]
        })
        .collect()
}

pub const RUN_HEADER: [&str; 7] = [
    "JOB", "OUTCOME", "ITEMS", "CREATED", "SEEN", "DEFERRED", "ERROR",
];

pub fn run_rows(runs: &[JobRunReport]) -> Vec<Vec<String>> {
    runs.iter()
        .map(|r| {
            vec![
                r.job.clone(),
                r.outcome.to_string(),
                r.items.to_string(),
                r.created.join(" "),
                r.skipped_seen.to_string(),
                r.deferred.to_string(),
                r.error.clone().unwrap_or_default(),
            ]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task_with(spec: crate::task::DispatchSpec) -> Task {
        let now = Utc::now();
        Task {
            id: 3,
            job: "run".into(),
            item: serde_json::Value::Null,
            prompt: "fix it\nthen stop".into(),
            spec,
            machine: Some("pi-3".into()),
            workspace_id: Some("w1".into()),
            pane_id: Some("w1:p1".into()),
            agent_name: Some("t-3".into()),
            state: crate::task::TaskState::Running,
            error: None,
            last_completion_seq: None,
            prompt_pending: false,
            activity_seen: false,
            retry_of: None,
            created_at: now,
            started_at: Some(now),
            finished_at: None,
            updated_at: now,
        }
    }

    fn status(name: &str, host: &str) -> MachineStatus {
        MachineStatus {
            name: name.into(),
            host: host.into(),
            endpoint: format!("ssh {host} (session default)"),
            channel: crate::machine::ChannelState::Connected,
            herdr_version: Some("0.9.1".into()),
            pastor_version: Some("0.2.0".into()),
            protocol: Some(22),
            error: None,
            live: 1,
            max_agents: 3,
            tags: vec!["fast".into(), "arm".into()],
            orphans: vec![],
        }
    }

    fn head() -> HeadRow {
        HeadRow::new("darkbeat".into(), Some("0.9.1".into()))
    }

    #[test]
    fn machine_table_puts_the_head_first_then_each_machine_with_its_host() {
        let rows: Vec<MachineRow> = [
            status("pi-3", "fleet@pi-3"),
            status("here", "local"),
            status("fake", "fake-herdr"),
        ]
        .iter()
        .map(MachineRow::from)
        .collect();
        let out = table(&MACHINE_HEADER, &machine_rows(&head(), &rows));
        let lines: Vec<&str> = out.lines().collect();
        let cells = |i: usize| lines[i].split_whitespace().collect::<Vec<_>>();
        assert_eq!(
            cells(0),
            [
                "NAME", "HOST", "CHANNEL", "HERDR", "PASTOR", "AGENTS", "ORPHANS", "TAGS", "ERROR"
            ]
        );
        assert_eq!(
            cells(1),
            [
                "pastor",
                "darkbeat",
                "head",
                "0.9.1",
                env!("CARGO_PKG_VERSION"),
                "-",
                "-",
                "-"
            ]
        );
        assert_eq!(
            cells(2),
            [
                "pi-3",
                "fleet@pi-3",
                "connected",
                "0.9.1",
                "0.2.0",
                "1/3",
                "-",
                "fast,arm"
            ]
        );
        assert_eq!(cells(3)[1], "local");
        assert_eq!(cells(4)[1], "fake-herdr");
    }

    #[test]
    fn a_probed_machine_without_a_count_shows_a_dash_and_its_error() {
        let row = MachineRow {
            channel: "unreachable".into(),
            herdr_version: None,
            pastor_version: None,
            live: None,
            error: Some("no route to host".into()),
            tags: vec![],
            ..MachineRow::from(&status("pi-3", "fleet@pi-3"))
        };
        let head = HeadRow::new("darkbeat".into(), None);
        let rows = machine_rows(&head, &[row]);
        assert_eq!(rows[0][3], "-", "no herdr on the head reads as a dash");
        assert_eq!(
            rows[1],
            [
                "pi-3",
                "fleet@pi-3",
                "unreachable",
                "-",
                "-",
                "-/3",
                "-",
                "-",
                "no route to host"
            ]
        );
    }

    /// The head is not a machine: scripts that walk `machines` must not trip
    /// over it, so it sits under its own key.
    #[test]
    fn machine_list_json_keeps_the_head_out_of_the_machines() {
        let rows = vec![MachineRow::from(&status("pi-3", "fleet@pi-3"))];
        let head = head();
        let v = serde_json::to_value(machine_list_json(&head, &rows)).unwrap();
        assert_eq!(v["head"]["name"], "pastor");
        assert_eq!(v["head"]["host"], "darkbeat");
        assert_eq!(v["head"]["channel"], "head");
        assert_eq!(v["head"]["herdr_version"], "0.9.1");
        assert_eq!(v["head"]["pastor_version"], env!("CARGO_PKG_VERSION"));
        let ms = v["machines"].as_array().unwrap();
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0]["name"], "pi-3");
        assert_eq!(ms[0]["host"], "fleet@pi-3");
        assert_eq!(ms[0]["channel"], "connected");
        assert_eq!(ms[0]["pastor_version"], "0.2.0");
        assert_eq!(ms[0]["live"], 1);
        assert_eq!(ms[0]["max_agents"], 3);
    }

    #[test]
    fn herdr_version_is_the_last_word_of_the_first_line() {
        assert_eq!(
            herdr_version_from("herdr 0.9.1\n").as_deref(),
            Some("0.9.1")
        );
        assert_eq!(herdr_version_from("").as_deref(), None);
    }

    #[test]
    fn task_detail_prints_the_agent_args_shell_quoted() {
        let spec = crate::task::DispatchSpec {
            agent: "claude".into(),
            agent_args: vec![
                "--model".into(),
                "claude-opus-5-5".into(),
                "--append-system-prompt".into(),
                "be brief".into(),
            ],
            repo: Some("~/work/api".into()),
            worktree: true,
            branch: Some("pastor/t-3".into()),
            machine: None,
            tags: vec!["fast".into()],
            timeout_secs: 7200,
        };
        let out = task_detail(&task_with(spec.clone()));
        assert!(
            out.contains("agent args: --model claude-opus-5-5 --append-system-prompt 'be brief'"),
            "{out}"
        );
        assert!(out.contains("agent:      claude"), "{out}");
        assert!(
            out.contains("repo:       ~/work/api (worktree, branch pastor/t-3)"),
            "{out}"
        );
        assert!(out.contains("machine:    pi-3"), "{out}");
        assert!(out.ends_with("prompt:\n  fix it\n  then stop"), "{out}");
        assert!(!out.contains("error:"), "{out}");

        let bare = task_detail(&task_with(crate::task::DispatchSpec {
            agent_args: vec![],
            ..spec
        }));
        assert!(bare.contains("agent args: -"), "{bare}");
    }

    /// An error can be raw multi-line stderr; it must stay one field on one
    /// line, escaped the way the events log's human lines escape it.
    #[test]
    fn task_detail_keeps_a_multiline_error_on_its_line() {
        let mut t = task_with(crate::task::DispatchSpec {
            agent: "claude".into(),
            agent_args: vec![],
            repo: None,
            worktree: false,
            branch: None,
            machine: None,
            tags: vec![],
            timeout_secs: 60,
        });
        t.error = Some("ssh failed:\nPermission denied\r\nbye".into());
        let out = task_detail(&t);
        assert!(
            out.contains("\nerror:      ssh failed:\\nPermission denied\\r\\nbye\nprompt:"),
            "{out}"
        );
        assert!(!out.contains('\r'), "{out:?}");
    }

    #[test]
    fn table_aligns_and_trims() {
        let out = table(
            &["A", "BB"],
            &[vec!["x".into(), "".into()], vec!["long".into(), "y".into()]],
        );
        assert_eq!(out, "A     BB\nx\nlong  y");
    }

    #[test]
    fn job_rows_show_errors_over_results_and_relative_next() {
        use crate::scheduler::JobStatus;
        let now = chrono::Utc::now();
        let ok = JobStatus {
            name: "a".into(),
            schedule: Some("every 5m".into()),
            enabled: true,
            connector: Some("clock".into()),
            error: None,
            last_run_at: Some(now - chrono::Duration::seconds(90)),
            last_result: Some("ok: 1 items, 1 tasks".into()),
            next_due: Some(now + chrono::Duration::seconds(210)),
            running: false,
        };
        let broken = JobStatus {
            name: "b".into(),
            schedule: None,
            enabled: false,
            connector: None,
            error: Some("expected `]`".into()),
            last_run_at: None,
            last_result: None,
            next_due: None,
            running: true,
        };
        let rows = job_rows(&[ok, broken]);
        assert_eq!(
            rows[0],
            vec![
                "a",
                "every 5m",
                "yes",
                "clock",
                "1m ago",
                "in 3m",
                "ok: 1 items, 1 tasks"
            ]
        );
        assert_eq!(
            rows[1],
            vec![
                "b",
                "-",
                "no",
                "-",
                "never",
                "-",
                "invalid: expected `]` (running)"
            ]
        );
    }

    #[test]
    fn run_rows_summarise_a_report() {
        use crate::scheduler::{JobRunReport, RunOutcome};
        let mut r = JobRunReport::new("a", RunOutcome::Ran);
        r.items = 3;
        r.created = vec!["t-1".into(), "t-2".into()];
        r.skipped_seen = 1;
        let rows = run_rows(&[r, JobRunReport::new("b", RunOutcome::NotDue)]);
        assert_eq!(rows[0], vec!["a", "ran", "3", "t-1 t-2", "1", "0", ""]);
        assert_eq!(rows[1], vec!["b", "not_due", "0", "", "0", "0", ""]);
    }

    #[test]
    fn relative_times() {
        let now = chrono::Utc::now();
        assert_eq!(in_(now + chrono::Duration::seconds(250)), "in 4m");
        assert_eq!(in_(now - chrono::Duration::seconds(12)), "12s ago");
        assert_eq!(in_(now), "now");
    }

    #[test]
    fn machine_rows_name_orphans() {
        let m = MachineStatus {
            live: 3,
            max_agents: 4,
            orphans: vec!["t-4".into(), "t-9".into()],
            ..status("pi", "local")
        };
        let none = MachineStatus {
            orphans: vec![],
            ..m.clone()
        };
        let lines = orphan_lines(&[m.clone(), none.clone()], None);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("orphan t-4 on pi:"), "{}", lines[0]);
        assert!(lines[1].contains("pastor task close t-9"));
        // `task list --machine pi-2` shows pi-2's tasks, so only its orphans.
        let other = MachineStatus {
            name: "pi-2".into(),
            orphans: vec!["t-5".into()],
            ..m.clone()
        };
        let both = [m.clone(), other];
        assert_eq!(orphan_lines(&both, None).len(), 3);
        let lines = orphan_lines(&both, Some("pi-2"));
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("orphan t-5 on pi-2:"), "{}", lines[0]);
        assert!(orphan_lines(&both, Some("nope")).is_empty());
        let rows = machine_rows(&head(), &[MachineRow::from(&m), MachineRow::from(&none)]);
        assert_eq!(rows[1][5], "3/4");
        assert_eq!(rows[1][6], "t-4,t-9");
        assert_eq!(rows[2][6], "-");
        assert_eq!(rows[0].len(), MACHINE_HEADER.len());
        assert_eq!(rows[1].len(), MACHINE_HEADER.len());
    }
}
