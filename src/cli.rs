use chrono::Utc;

use crate::machine::MachineStatus;
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
}
