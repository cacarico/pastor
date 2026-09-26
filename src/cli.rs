use chrono::Utc;
use serde::Serialize;

use crate::config::flock::{DEFAULT_FLOCK, Flock};
use crate::ipc::{RequestError, connect_error_means_no_daemon};
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
                        .map(|title| one_line(title).chars().take(60).collect())
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
            let note = one_line(&note);
            let note = match t.retry_of {
                Some(of) => format!("retry of t-{of}: {note}"),
                None => note,
            };
            vec![
                t.display_id(),
                t.state.to_string(),
                t.machine.clone().unwrap_or_else(|| "-".into()),
                t.flock.clone().unwrap_or_else(|| "-".into()),
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
///
/// Every other control character is escaped too, as `printable` does.
pub fn one_line(s: &str) -> String {
    escape_controls(s, false)
}

/// Text that came from outside pastor (an item, a pane, a connector
/// manifest), made safe to print on the user's terminal: newlines and tabs
/// stay, `\r` shows as `\r`, and every other C0 or C1 control character
/// and DEL shows as `\xNN` or `\u{NN}`. Printed raw, an ESC could set the
/// clipboard (OSC 52), draw a fake link, retitle the terminal or erase the
/// line before it. `--json` output stays raw; JSON escapes these itself.
pub fn printable(s: &str) -> String {
    escape_controls(s, true)
}

fn escape_controls(s: &str, keep_lines: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' | '\t' if keep_lines => out.push(c),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x80 && c.is_control() => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// `pastor task show`: every field a human asks about one task, one per line,
/// then the prompt. The agent args are shell-quoted, so the line reads as the
/// command herdr runs; each is followed by where it came from, when the task
/// knows (`DispatchSpec::agent_source`).
pub fn task_detail(t: &Task) -> String {
    let opt = |v: &Option<String>| v.clone().unwrap_or_else(|| "-".into());
    let when = |v: Option<chrono::DateTime<Utc>>| {
        v.map(|at| format!("{} ({} ago)", at.format("%Y-%m-%d %H:%M:%S UTC"), age(at)))
            .unwrap_or_else(|| "-".into())
    };
    let source = t.spec.agent_source.as_deref();
    let from = |label: Option<&String>| label.map(|l| format!(" (from {l})")).unwrap_or_default();
    let agent = format!("{}{}", t.spec.agent, from(source.map(|s| &s.agent)));
    let args = if t.spec.agent_args.is_empty() {
        "-".to_string()
    } else {
        let args = t
            .spec
            .agent_args
            .iter()
            .map(|a| crate::herdr::shell_quote(a))
            .collect::<Vec<_>>()
            .join(" ");
        format!("{args}{}", from(source.and_then(|s| s.agent_args.as_ref())))
    };
    let list = |v: &[String]| {
        if v.is_empty() {
            "-".to_string()
        } else {
            v.iter()
                .map(|p| crate::herdr::shell_quote(p))
                .collect::<Vec<_>>()
                .join(" ")
        }
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
        ("flock", opt(&t.flock)),
        ("machine", opt(&t.machine)),
        ("agent", agent),
        ("agent args", args),
        ("allow", list(&t.spec.allow)),
        ("deny", list(&t.spec.deny)),
        ("repo", repo),
        ("place", t.spec.place.to_string()),
        ("tags", tags),
        ("timeout", format!("{}s", t.spec.timeout_secs)),
        ("pane", opt(&t.pane_id)),
        ("created", when(Some(t.created_at))),
        ("started", when(t.started_at)),
        ("finished", when(t.finished_at)),
    ];
    if let Some(e) = &t.error {
        fields.push(("error", e.clone()));
    }
    // Branch, repo and the prompt can come from an item; every field is
    // escaped the same way so none of them reaches the terminal raw.
    let mut out: Vec<String> = fields
        .into_iter()
        .map(|(k, v)| format!("{:<12}{}", format!("{k}:"), one_line(&v)))
        .collect();
    out.push("prompt:".into());
    out.extend(
        printable(&t.prompt)
            .lines()
            .map(|l| format!("  {l}").trim_end().to_string()),
    );
    out.join("\n")
}

/// The stable code and message for a request that got no reply. Only a connect
/// that was refused or found no socket means nothing is listening; one denied
/// for permissions may hide a live head, as `probe_daemon` also assumes. A head
/// that took the connection and then sat on it is running but busy. Telling
/// the user to start a head in either case would send them the wrong way.
/// The head handles each request in a detached task, so a timed-out `run`,
/// `task retry`, `tick` or `job run` may still land; sending it again blindly
/// can queue a duplicate.
pub fn request_failure(err: &RequestError) -> (&'static str, String) {
    match err {
        RequestError::Connect(e) if connect_error_means_no_daemon(e) => (
            "runtime_error",
            format!("pastor serve is not running ({e}); start it with `pastor serve`"),
        ),
        RequestError::Connect(e) => (
            "runtime_error",
            format!("could not connect to pastor serve: {e}"),
        ),
        RequestError::Timeout(bound) => (
            "timeout",
            format!(
                "pastor serve did not answer within {}s; the request may still complete, so check `pastor task list` (or `pastor job list` for a tick or job run) before sending it again",
                bound.as_secs()
            ),
        ),
        RequestError::Exchange(e) => (
            "runtime_error",
            format!("pastor serve dropped the request: {e:#}"),
        ),
    }
}

/// Tasks still holding a pane on a machine the flock no longer has. Nothing
/// reconciles them, so their state is the last one seen; the MACHINE column
/// says so instead of looking live. Rows and tasks are in the same order
/// (`task_rows`).
pub fn mark_removed(rows: &mut [Vec<String>], tasks: &[Task], flock: &Flock) {
    for (row, t) in rows.iter_mut().zip(tasks) {
        if let Some(m) = &t.machine
            && t.state.occupies_pane()
            && flock.get(m).is_none()
        {
            row[2] = format!("{m} (removed)");
        }
    }
}

pub const TASK_HEADER: [&str; 8] = [
    "ID", "STATE", "MACHINE", "FLOCK", "AGENT", "JOB", "AGE", "NOTE",
];

/// One flock in `pastor flock list`. `agents` is the live agents on its
/// machines, known only from a running head; `queued` counts the tasks
/// waiting for one of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FlockRow {
    pub name: String,
    pub default: bool,
    pub machines: Vec<String>,
    pub agents: Option<usize>,
    pub queued: usize,
}

pub const FLOCK_HEADER: [&str; 5] = ["NAME", "DEFAULT", "MACHINES", "AGENTS", "QUEUED"];

/// `flock`'s flocks in file order. `live` is each machine's live agents from
/// the head, `None` without one; `queued` the queued tasks, whose flock
/// (`None`: a row from before flocks) reads as the default.
pub fn flock_list(flock: &Flock, live: Option<&[MachineStatus]>, queued: &[Task]) -> Vec<FlockRow> {
    flock
        .flock_names()
        .into_iter()
        .map(|name| {
            let machines: Vec<String> = flock
                .machines
                .iter()
                .filter(|m| flock.flock_of(m) == name)
                .map(|m| m.name.clone())
                .collect();
            let agents = live.map(|ms| {
                ms.iter()
                    .filter(|s| machines.contains(&s.name))
                    .map(|s| s.live)
                    .sum()
            });
            let queued = queued
                .iter()
                .filter(|t| t.flock.as_deref().unwrap_or(flock.default_flock()) == name)
                .count();
            FlockRow {
                name: name.to_string(),
                default: name == flock.default_flock(),
                machines,
                agents,
                queued,
            }
        })
        .collect()
}

pub fn flock_rows(rows: &[FlockRow]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|f| {
            vec![
                f.name.clone(),
                if f.default { "yes" } else { "no" }.into(),
                if f.machines.is_empty() {
                    "-".into()
                } else {
                    f.machines.join(",")
                },
                f.agents.map_or_else(|| "-".into(), |n| n.to_string()),
                f.queued.to_string(),
            ]
        })
        .collect()
}

/// One line per orphaned agent, for under the `pastor list` table. With
/// `machine`, only that machine's, as `task list --machine` shows only its
/// tasks; with `flock`, only its machines' (a machine with no flock, from a
/// head before flocks, is in the default one, as in `MachineRow`).
pub fn orphan_lines(
    ms: &[MachineStatus],
    machine: Option<&str>,
    flock: Option<&str>,
) -> Vec<String> {
    ms.iter()
        .filter(|m| machine.is_none_or(|name| m.name == name))
        .filter(|m| flock.is_none_or(|f| m.flock.as_deref().unwrap_or(DEFAULT_FLOCK) == f))
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

/// The head itself, for the line `machine list` opens with and the `head`
/// key of its JSON. The head runs no tasks of its own; when it is also a
/// machine (a `local` one), that machine has its own row. Its
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
    /// The flock the machine is in.
    pub flock: String,
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
            // A head from before flocks has only the one.
            flock: m.flock.clone().unwrap_or_else(|| DEFAULT_FLOCK.into()),
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
pub const MACHINE_HEADER: [&str; 10] = [
    "NAME", "HOST", "FLOCK", "CHANNEL", "HERDR", "PASTOR", "AGENTS", "ORPHANS", "TAGS", "ERROR",
];

/// The machine that is the head itself: the first `local` one, whose herdr
/// runs on this host.
fn is_head_machine(m: &MachineRow) -> bool {
    m.host == "local"
}

/// Move the head's own machine, if it is one, to the front; the rest keep
/// flock order.
pub fn head_machine_first(rows: &mut [MachineRow]) {
    if let Some(i) = rows.iter().position(is_head_machine) {
        rows[..=i].rotate_right(1);
    }
}

/// The line `machine list` opens with when a head runs: its version, its
/// host and the herdr there, how many machines follow, and, when the head
/// is itself one of them, which.
pub fn head_line(head: &HeadRow, rows: &[MachineRow]) -> String {
    let n = rows.len();
    let mut line = format!(
        "pastor {} on {} (herdr {}), {n} machine{}",
        head.pastor_version,
        head.host,
        head.herdr_version.as_deref().unwrap_or("-"),
        if n == 1 { "" } else { "s" }
    );
    if let Some(m) = rows.iter().find(|m| is_head_machine(m)) {
        line.push_str(&format!(", {} is the head of the flock", m.name));
    }
    line
}

/// One row per machine, in the order given.
pub fn machine_rows(ms: &[MachineRow]) -> Vec<Vec<String>> {
    let dash = || "-".to_string();
    ms.iter()
        .map(|m| {
            vec![
                m.name.clone(),
                m.host.clone(),
                m.flock.clone(),
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
        })
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

pub const JOB_HEADER: [&str; 8] = [
    "NAME",
    "SCHEDULE",
    "ENABLED",
    "FLOCK",
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
                j.flock.clone().unwrap_or_else(|| "-".into()),
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
            flock: None,
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
            flock: None,
        }
    }

    fn head() -> HeadRow {
        HeadRow::new("desk".into(), Some("0.9.1".into()))
    }

    fn row(name: &str, host: &str) -> MachineRow {
        MachineRow::from(&status(name, host))
    }

    /// The head's own machine (the `local` one) comes first; the others
    /// keep flock order. FLOCK follows HOST.
    #[test]
    fn machine_table_puts_the_heads_machine_first_with_its_flock() {
        let mut rows = vec![
            MachineRow {
                flock: "work".into(),
                ..row("pi-3", "user@pi-3")
            },
            row("here", "local"),
            row("fake", "fake-herdr"),
        ];
        head_machine_first(&mut rows);
        let out = table(&MACHINE_HEADER, &machine_rows(&rows));
        let lines: Vec<&str> = out.lines().collect();
        let cells = |i: usize| lines[i].split_whitespace().collect::<Vec<_>>();
        assert_eq!(
            cells(0),
            [
                "NAME", "HOST", "FLOCK", "CHANNEL", "HERDR", "PASTOR", "AGENTS", "ORPHANS", "TAGS",
                "ERROR"
            ]
        );
        assert_eq!(cells(1)[..3], ["here", "local", "default"]);
        assert_eq!(
            cells(2),
            [
                "pi-3",
                "user@pi-3",
                "work",
                "connected",
                "0.9.1",
                "0.2.0",
                "1/3",
                "-",
                "fast,arm"
            ]
        );
        assert_eq!(cells(3)[1], "fake-herdr");
    }

    #[test]
    fn the_head_line_names_the_head_when_it_is_a_machine() {
        let v = env!("CARGO_PKG_VERSION");
        let mut rows = vec![row("pi-3", "user@pi-3"), row("desk", "local")];
        assert_eq!(
            head_line(&head(), &rows),
            format!("pastor {v} on desk (herdr 0.9.1), 2 machines, desk is the head of the flock")
        );
        rows.remove(1);
        let bare = HeadRow::new("desk".into(), None);
        assert_eq!(
            head_line(&bare, &rows),
            format!("pastor {v} on desk (herdr -), 1 machine"),
            "a head that runs no agents: no ending, and no herdr is a dash"
        );
        assert_eq!(
            head_line(&head(), &[]),
            format!("pastor {v} on desk (herdr 0.9.1), 0 machines")
        );
    }

    #[test]
    fn a_probed_machine_without_a_count_shows_a_dash_and_its_error() {
        let r = MachineRow {
            channel: "unreachable".into(),
            herdr_version: None,
            pastor_version: None,
            live: None,
            error: Some("no route to host".into()),
            tags: vec![],
            ..row("pi-3", "user@pi-3")
        };
        let rows = machine_rows(&[r]);
        assert_eq!(
            rows[0],
            [
                "pi-3",
                "user@pi-3",
                "default",
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
        assert_eq!(v["head"]["host"], "desk");
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
        assert_eq!(ms[0]["flock"], "default");
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
    fn mark_removed_names_machines_no_longer_in_the_flock() {
        use crate::config::flock::{Flock, MachineConfig};
        use crate::task::TaskState;
        let spec = crate::task::DispatchSpec {
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
        };
        let running_gone = task_with(spec.clone()); // on pi-3, running
        let closed_gone = Task {
            state: TaskState::Closed,
            ..task_with(spec.clone())
        };
        let here = Task {
            machine: Some("pi-1".into()),
            ..task_with(spec)
        };
        let tasks = vec![running_gone, closed_gone, here];
        let flock = Flock {
            flocks: vec![],
            machines: vec![MachineConfig {
                name: "pi-1".into(),
                local: true,
                ssh: None,
                command: None,
                session: "default".into(),
                max_agents: 2,
                tags: vec![],
                flock: None,
                agent: None,
                agent_args: None,
            }],
        };
        let mut rows = task_rows(&tasks);
        mark_removed(&mut rows, &tasks, &flock);
        assert_eq!(rows[0][2], "pi-3 (removed)");
        assert_eq!(
            rows[1][2], "pi-3",
            "a closed task is history, not a live row"
        );
        assert_eq!(rows[2][2], "pi-1");
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
            allow: vec!["Bash(git log:*)".into(), "Edit".into()],
            deny: vec!["Bash(rm:*)".into()],
            repo: Some("~/work/api".into()),
            worktree: true,
            branch: Some("pastor/t-3".into()),
            machine: None,
            tags: vec!["fast".into()],
            timeout_secs: 7200,
            checkout: None,
            reopen: None,
            agent_source: None,
            place: Default::default(),
        };
        let out = task_detail(&task_with(spec.clone()));
        assert!(
            out.contains("agent args: --model claude-opus-5-5 --append-system-prompt 'be brief'"),
            "{out}"
        );
        assert!(out.contains("agent:      claude"), "{out}");
        assert!(
            out.contains("allow:      'Bash(git log:*)' Edit\n"),
            "{out}"
        );
        assert!(out.contains("deny:       'Bash(rm:*)'\n"), "{out}");
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

    /// `task show` says where the agent and its args came from, when the
    /// task knows; a task with no args from any layer says so too.
    #[test]
    fn task_detail_prints_where_the_agent_came_from() {
        let spec = crate::task::DispatchSpec {
            agent: "claude-personal".into(),
            agent_args: vec!["--model".into(), "claude-opus-5-5".into()],
            agent_source: Some(Box::new(crate::task::AgentSource {
                ask: Default::default(),
                agent: "machine own".into(),
                agent_args: Some("flock personal".into()),
            })),
            ..serde_json::from_str(r#"{"agent": "claude"}"#).unwrap()
        };
        let out = task_detail(&task_with(spec.clone()));
        assert!(
            out.contains("agent:      claude-personal (from machine own)\n"),
            "{out}"
        );
        assert!(
            out.contains("agent args: --model claude-opus-5-5 (from flock personal)\n"),
            "{out}"
        );
        let bare = task_detail(&task_with(crate::task::DispatchSpec {
            agent_args: vec![],
            agent_source: Some(Box::new(crate::task::AgentSource {
                ask: Default::default(),
                agent: "defaults".into(),
                agent_args: None,
            })),
            ..spec
        }));
        assert!(
            bare.contains("agent:      claude-personal (from defaults)\n"),
            "{bare}"
        );
        assert!(bare.contains("agent args: -\n"), "{bare}");
    }

    /// An error can be raw multi-line stderr; it must stay one field on one
    /// line, escaped the way the events log's human lines escape it.
    #[test]
    fn task_detail_keeps_a_multiline_error_on_its_line() {
        let mut t = task_with(crate::task::DispatchSpec {
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
        });
        t.error = Some("ssh failed:\nPermission denied\r\nbye".into());
        let out = task_detail(&t);
        assert!(
            out.contains("\nerror:      ssh failed:\\nPermission denied\\r\\nbye\nprompt:"),
            "{out}"
        );
        assert!(!out.contains('\r'), "{out:?}");
    }

    /// Text from items, panes and manifests reaches the user's terminal, so
    /// no control character goes through raw: an ESC could set the
    /// clipboard (OSC 52), draw a fake link or erase what came before.
    #[test]
    fn printable_and_one_line_escape_every_control_character() {
        let s = "a\x1b]52;c;aGk=\x07b\u{9b}2Jc\x7fd\te\r\nf";
        assert_eq!(
            printable(s),
            "a\\x1b]52;c;aGk=\\x07b\\u{9b}2Jc\\x7fd\te\\r\nf"
        );
        assert_eq!(
            one_line(s),
            "a\\x1b]52;c;aGk=\\x07b\\u{9b}2Jc\\x7fd\\te\\r\\nf"
        );
        assert_eq!(printable("plain é ✓\n"), "plain é ✓\n");
    }

    #[test]
    fn task_rows_escape_and_cap_an_item_title() {
        let mut t = task_with(crate::task::DispatchSpec {
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
        });
        t.item = serde_json::json!({"key": "k", "title": format!("x\n t-9  done\x1b[2K{}", "y".repeat(80))});
        let note = task_rows(std::slice::from_ref(&t))[0]
            .last()
            .unwrap()
            .clone();
        assert!(note.starts_with("x\\n t-9  done\\x1b[2Kyy"), "{note}");
        assert!(!note.chars().any(char::is_control), "{note:?}");
        assert_eq!(note.chars().count(), 60, "{note}");
    }

    #[test]
    fn task_detail_escapes_item_text_in_the_prompt_and_fields() {
        let mut t = task_with(crate::task::DispatchSpec {
            agent: "claude".into(),
            agent_args: vec!["--x\x1b[2J".into()],
            allow: vec![],
            deny: vec![],
            repo: Some("~/work".into()),
            worktree: true,
            branch: Some("pastor/a\x1bb".into()),
            machine: None,
            tags: vec![],
            timeout_secs: 60,
            checkout: None,
            reopen: None,
            agent_source: None,
            place: Default::default(),
        });
        t.prompt = "look at\x1b]8;;http://x\x07this\r\nand stop".into();
        let out = task_detail(&t);
        assert!(!out.chars().any(|c| c.is_control() && c != '\n'), "{out:?}");
        assert!(out.contains("branch pastor/a\\x1bb"), "{out}");
        assert!(
            out.ends_with("prompt:\n  look at\\x1b]8;;http://x\\x07this\\r\n  and stop"),
            "{out}"
        );
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
            flock: Some("work".into()),
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
            flock: None,
        };
        let rows = job_rows(&[ok, broken]);
        assert_eq!(
            rows[0],
            vec![
                "a",
                "every 5m",
                "yes",
                "work",
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
        let lines = orphan_lines(&[m.clone(), none.clone()], None, None);
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
        assert_eq!(orphan_lines(&both, None, None).len(), 3);
        let lines = orphan_lines(&both, Some("pi-2"), None);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("orphan t-5 on pi-2:"), "{}", lines[0]);
        assert!(orphan_lines(&both, Some("nope"), None).is_empty());
        // `task list --flock work` likewise: only the orphans of work's
        // machines. A machine with no flock (a head from before flocks) is
        // in the default flock, as its row in `machine list` says.
        let work = MachineStatus {
            name: "pi-3".into(),
            orphans: vec!["t-6".into()],
            flock: Some("work".into()),
            ..m.clone()
        };
        let three = [m.clone(), both[1].clone(), work];
        let lines = orphan_lines(&three, None, Some("work"));
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("orphan t-6 on pi-3:"), "{}", lines[0]);
        assert_eq!(orphan_lines(&three, None, Some(DEFAULT_FLOCK)).len(), 3);
        assert!(orphan_lines(&three, Some("pi-3"), Some(DEFAULT_FLOCK)).is_empty());
        let rows = machine_rows(&[MachineRow::from(&m), MachineRow::from(&none)]);
        assert_eq!(rows[0][6], "3/4");
        assert_eq!(rows[0][7], "t-4,t-9");
        assert_eq!(rows[1][7], "-");
        assert_eq!(rows[0].len(), MACHINE_HEADER.len());
    }
}
