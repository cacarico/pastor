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
