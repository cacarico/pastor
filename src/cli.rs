use chrono::Utc;

use crate::machine::MachineStatus;
use crate::scheduler::{JobRunReport, JobStatus};
use crate::task::Task;

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

pub fn machine_rows(ms: &[MachineStatus]) -> Vec<Vec<String>> {
    ms.iter()
        .map(|m| {
            vec![
                m.name.clone(),
                m.channel.to_string(),
                m.herdr_version.clone().unwrap_or_else(|| "-".into()),
                format!("{}/{}", m.live, m.max_agents),
                m.tags.join(","),
                m.error.clone().unwrap_or_default(),
            ]
        })
        .collect()
}

pub const MACHINE_HEADER: [&str; 6] = ["NAME", "CHANNEL", "HERDR", "AGENTS", "TAGS", "ERROR"];

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
            created_at: now,
            started_at: Some(now),
            finished_at: None,
            updated_at: now,
        }
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
}
