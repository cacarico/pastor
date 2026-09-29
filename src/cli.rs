use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::config::Paths;
use crate::config::flock::{DEFAULT_FLOCK, Flock, FlockNumber};
use crate::ipc::{IpcRequest, IpcResponse, RequestError, connect_error_means_no_daemon};
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

/// The fewest characters of a description `--wide` keeps on a narrow
/// terminal; the table wraps rather than show less.
const MIN_DESCRIPTION: usize = 20;

/// `table` with a DESCRIPTION column last, one per row (`-` for none), each
/// on one escaped line. With `width` (a terminal's), a description longer
/// than what the other columns leave is cut to fit, ending in `…`; it keeps
/// `MIN_DESCRIPTION` characters however narrow. Without it nothing is cut.
pub fn wide_table(
    header: &[&str],
    rows: &[Vec<String>],
    descriptions: &[Option<String>],
    width: Option<usize>,
) -> String {
    let before: usize = (0..header.len())
        .map(|i| {
            rows.iter()
                .filter_map(|r| r.get(i))
                .map(|c| c.chars().count())
                .chain([header[i].len()])
                .max()
                .unwrap_or(0)
                + 2
        })
        .sum();
    let room = width.map(|w| w.saturating_sub(before).max(MIN_DESCRIPTION));
    let mut full_header = header.to_vec();
    full_header.push("DESCRIPTION");
    let full_rows: Vec<Vec<String>> = rows
        .iter()
        .zip(descriptions.iter().chain(std::iter::repeat(&None)))
        .map(|(row, d)| {
            let mut text = d.as_deref().map_or_else(|| "-".to_string(), one_line);
            if let Some(room) = room
                && text.chars().count() > room
            {
                text = text.chars().take(room - 1).chain(['…']).collect();
            }
            let mut row = row.clone();
            row.push(text);
            row
        })
        .collect();
    table(&full_header, &full_rows)
}

/// A list command's table: `table`, or with `--wide` the `wide_table`
/// cut to this terminal (`terminal_width`).
pub fn list_table(
    header: &[&str],
    rows: &[Vec<String>],
    wide: bool,
    descriptions: &[Option<String>],
) -> String {
    if wide {
        wide_table(header, rows, descriptions, terminal_width())
    } else {
        table(header, rows)
    }
}

/// The width of the terminal stdout is, from the terminal itself, else
/// `$COLUMNS`. `None` when stdout is not a terminal: piped or saved, a
/// list is not cut.
pub fn terminal_width() -> Option<usize> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        return None;
    }
    // SAFETY: TIOCGWINSZ only writes a `winsize` into the one we pass.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0 && ws.ws_col > 0
    {
        return Some(ws.ws_col as usize);
    }
    std::env::var("COLUMNS")
        .ok()?
        .trim()
        .parse()
        .ok()
        .filter(|&n| n > 0)
}

pub fn task_rows(tasks: &[Task]) -> Vec<Vec<String>> {
    tasks
        .iter()
        .map(|t| {
            let note = task_note(t);
            // A done task whose pane pastor left open on purpose: someone
            // may still be talking to its agent.
            let state = if t.state == crate::task::TaskState::Done
                && t.spec.keeps_pane()
                && t.pane_id.is_some()
            {
                format!("{} (kept)", t.state)
            } else if t.state == crate::task::TaskState::Waiting
                && let Some(until) = t.waiting_until
            {
                // When its usage limit resets and it goes on.
                format!(
                    "{} {}",
                    t.state,
                    crate::limit::local_time(until, chrono::Utc::now())
                )
            } else {
                t.state.to_string()
            };
            vec![
                t.display_id(),
                state,
                t.priority.to_string(),
                t.machine.clone().unwrap_or_else(|| "-".into()),
                t.flock.clone().unwrap_or_else(|| "-".into()),
                t.spec.agent.clone(),
                t.model().unwrap_or("-").to_string(),
                t.job.clone(),
                age(t.created_at),
                note,
            ]
        })
        .collect()
}

/// `task list`'s order: the waiting tasks first, the soonest reset first
/// (`waiting_until`), then the rest as they came, newest first.
pub fn waiting_first(mut tasks: Vec<Task>) -> Vec<Task> {
    tasks.sort_by_key(|t| match t.state {
        crate::task::TaskState::Waiting => (0, t.waiting_until),
        _ => (1, None),
    });
    tasks
}

/// The NOTE column of `task list`, one escaped line: the error, else the
/// item's title, else the prompt's first line. Shell completion shows it too.
pub fn task_note(t: &Task) -> String {
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
    let mut note = one_line(&note);
    if t.spec.now {
        note = format!("now: {note}");
    }
    match t.retry_of {
        Some(of) => format!("retry of t-{of}: {note}"),
        None => note,
    }
}

/// Errors carry raw stderr, newlines included; a human line (an events
/// record, a `task describe` field) must stay one line. The escapes keep what was
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

/// The `label` line of `task describe`: the workspace's name once dispatch
/// gave it one, else the template, then where it came from. A task that
/// joined a workspace says so instead, since no template named it.
fn workspace_label(l: &crate::task::WorkspaceLabel) -> String {
    let from = match &l.from {
        Some(from) => format!("from {from}"),
        None => "built-in".to_string(),
    };
    let why = match l.note.as_deref() {
        Some(note @ crate::task::JOINED_WORKSPACE) => note.to_string(),
        Some(note) => format!("{from}; {note}"),
        None => from,
    };
    let shown = l
        .name
        .as_deref()
        .or(l.template.as_deref())
        .unwrap_or(crate::task::DEFAULT_LABEL);
    format!("{shown} ({why})")
}

/// `pastor task describe`: every field a human asks about one task, one per line,
/// then the prompt. The agent args are shell-quoted, so the line reads as the
/// command herdr runs; each is followed by where it came from, when the task
/// knows (`DispatchSpec::agent_source`).
pub fn task_detail(t: &Task) -> String {
    task_detail_with(t, t.summary.as_slice())
}

/// `task_detail` with `summaries` in place of the last round's: `task
/// describe --all-summaries` passes every round's.
pub fn task_detail_with(t: &Task, summaries: &[crate::task::TaskSummary]) -> String {
    let opt = |v: &Option<String>| v.clone().unwrap_or_else(|| "-".into());
    let when = |v: Option<chrono::DateTime<Utc>>| {
        v.map(|at| format!("{} ({} ago)", at.format("%Y-%m-%d %H:%M:%S UTC"), age(at)))
            .unwrap_or_else(|| "-".into())
    };
    let source = t.spec.agent_source.as_deref();
    let from = |label: Option<&String>| label.map(|l| format!(" (from {l})")).unwrap_or_default();
    let agent = format!("{}{}", t.spec.agent, from(source.map(|s| &s.agent)));
    let model = match (t.model(), source.and_then(|s| s.fallback_use.as_ref())) {
        (Some(m), Some(used)) => format!("{m} ({used})"),
        (Some(m), None) => format!("{m}{}", from(source.and_then(|s| s.model_from.as_ref()))),
        (None, _) => "-".to_string(),
    };
    // `-` with where it came from: a layer's `[]` gave the task none.
    let fallback = match t.fallback() {
        [] => "-".to_string(),
        names => names.join(", "),
    } + &from(source.and_then(|s| s.fallback_from.as_ref()));
    let mut priority = match t.aged_from {
        Some(was) => format!(
            "{}, aged from {was}{}",
            t.priority,
            from(t.priority_from.as_ref())
        ),
        None => format!("{}{}", t.priority, from(t.priority_from.as_ref())),
    };
    if t.pause.preempt {
        priority.push_str(", preempt: pauses a low task on a full machine");
    }
    if t.spec.now {
        priority.push_str(", now: started at once, past its machine's limits");
    }
    let profile = match t.profile() {
        Some(p) => format!("{p}{}", from(source.and_then(|s| s.profile_from.as_ref()))),
        None => "-".to_string(),
    };
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
    // A list the agent never gets (`AgentSource::lists_unapplied`) says so.
    let unapplied = |shown: String| match source.and_then(|s| s.lists_unapplied.as_deref()) {
        Some(why) if shown != "-" => format!("{shown} ({why})"),
        _ => shown,
    };
    let mut repo = opt(&t.spec.repo);
    if t.spec.worktree {
        repo.push_str(" (worktree");
        if let Some(b) = &t.spec.branch {
            repo.push_str(&format!(", branch {b}"));
        }
        repo.push(')');
    }
    let label = workspace_label(&t.spec.label);
    let tags = if t.spec.tags.is_empty() {
        "-".to_string()
    } else {
        t.spec.tags.join(",")
    };
    let mut fields = vec![
        ("id", t.display_id()),
        ("state", t.state.to_string()),
        ("priority", priority),
        ("job", t.job.clone()),
        ("role", t.role.to_string()),
        ("summary", t.spec.summary.describe()),
        ("flock", opt(&t.flock)),
        ("machine", opt(&t.machine)),
        ("agent", agent),
        ("model", model),
        ("fallback", fallback),
        ("profile", profile),
        ("agent args", args),
        ("allow", unapplied(list(&t.spec.allow))),
        ("deny", unapplied(list(&t.spec.deny))),
        ("repo", repo),
        (
            "place",
            format!(
                "{}{}",
                t.spec.place,
                from(source.and_then(|s| s.place_from.as_ref()))
            ),
        ),
        ("label", label),
        (
            "keep pane",
            format!(
                "{}{}",
                if t.spec.keeps_pane() { "yes" } else { "no" },
                from(t.spec.keep_pane_from.as_ref())
            ),
        ),
        ("tags", tags),
        (
            "timeout",
            format!(
                "{}s{}",
                t.spec.timeout_secs,
                from(source.and_then(|s| s.timeout_from.as_ref()))
            ),
        ),
        ("pane", opt(&t.pane_id)),
        ("session", opt(&t.spec.session_id)),
        ("created", when(Some(t.created_at))),
        ("started", when(t.started_at)),
        ("finished", when(t.finished_at)),
    ];
    if let Some(at) = t.pause.paused_at {
        let by = t
            .pause
            .paused_for
            .map(|id| format!(" for {}", Task::agent_name_for(id)))
            .unwrap_or_default();
        fields.push(("paused", format!("{}{by}", when(Some(at)))));
    }
    if t.pause.resumed_at.is_some() {
        fields.push(("resumed", when(t.pause.resumed_at)));
    }
    // A waiting task's error is the limit it waits on: which account,
    // until when, what ran out and the task that saw it.
    let waiting = t.state == crate::task::TaskState::Waiting;
    if waiting && let Some(until) = t.waiting_until {
        fields.push((
            "waiting",
            format!(
                "until {} ({})",
                crate::limit::local_time(until, Utc::now()),
                until.format("%Y-%m-%d %H:%M:%S UTC")
            ),
        ));
    }
    if let Some(e) = &t.error {
        fields.push((if waiting { "limit" } else { "error" }, e.clone()));
    }
    // What the task's Claude session used, as read when its last round
    // ended: its model is the one the machine gave it when `model` is `-`.
    if let Some(u) = &t.usage {
        let calls = format!("{} API calls", grouped(u.api_calls));
        fields.push((
            "used",
            match &u.model {
                Some(m) => format!("{m}, {calls}"),
                None => calls,
            },
        ));
        fields.push((
            "tokens",
            format!(
                "input {}, cache write {}, cache read {}, output {}",
                grouped(u.input),
                grouped(u.cache_write),
                grouped(u.cache_read),
                grouped(u.output)
            ),
        ));
    }
    // Branch, repo and the prompt can come from an item; every field is
    // escaped the same way so none of them reaches the terminal raw.
    let mut out: Vec<String> = fields
        .into_iter()
        .map(|(k, v)| format!("{:<12}{}", format!("{k}:"), one_line(&v)))
        .collect();
    // As written, bar control characters: `describe` is where a long or
    // multi-line description is read whole.
    out.insert(
        1,
        format!(
            "description: {} (from {})",
            printable(&t.description_text()),
            t.description_from()
        ),
    );
    for s in summaries {
        out.push(summary_heading(s));
        out.extend(
            printable(&s.text)
                .lines()
                .map(|l| format!("  {l}").trim_end().to_string()),
        );
    }
    out.push("prompt:".into());
    out.extend(
        printable(&t.prompt)
            .lines()
            .map(|l| format!("  {l}").trim_end().to_string()),
    );
    out.join("\n")
}

/// `n` with a comma between each group of three digits: `3,400,500`.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A summary's first line in `task describe`: its outcome, round, who wrote
/// it and when.
fn summary_heading(s: &crate::task::TaskSummary) -> String {
    let by = match s.source {
        crate::task::SummarySource::Agent => "from the agent",
        crate::task::SummarySource::Pane => "the pane's last lines",
    };
    format!(
        "{:<12}{} (round {}, {by}, {} ago)",
        "summary:",
        s.outcome,
        s.round,
        age(s.at)
    )
}

/// The RESULT column of `task list --wide`: the outcome of the task's last
/// round, `-` while it has none.
pub fn task_result(t: &Task) -> String {
    t.summary
        .as_ref()
        .map_or_else(|| "-".into(), |s| s.outcome.to_string())
}

/// The CLI error for a request that got no reply, the one mapper every
/// command uses. Only a connect that was refused or found no socket means
/// nothing is listening (`daemon_not_running`); one denied for permissions
/// may hide a live head, as `probe_daemon` also assumes. A head that took the
/// connection and then sat on it is running but busy. Telling the user to
/// start a head in either case would send them the wrong way. The head
/// handles each request in a detached task, so a timed-out `run`,
/// `task retry`, `tick` or `job run` may still land; sending it again blindly
/// can queue a duplicate.
pub fn request_error(err: &RequestError) -> CliError {
    let (code, message) = match err {
        RequestError::Connect(e) if connect_error_means_no_daemon(e) => (
            "daemon_not_running",
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
        RequestError::Unreachable(message) => ("head_unreachable", message.clone()),
        RequestError::Refused { code, message } => {
            return CliError {
                code: code.clone(),
                message: message.clone(),
            };
        }
    };
    CliError {
        code: code.into(),
        message,
    }
}

/// What `request_head` got, as the CLI takes it: a reply, or a `CliError`
/// for an error reply or for no reply at all (`request_error`).
pub fn reply(got: Result<IpcResponse, RequestError>) -> Result<IpcResponse, CliError> {
    match got {
        Ok(IpcResponse::Error { code, message }) => Err(CliError { code, message }),
        Ok(resp) => Ok(resp),
        Err(err) => Err(request_error(&err)),
    }
}

/// One request to the head, the local serve or the remote head, whichever
/// `request_head` reaches. The error is for the caller to return, print or
/// act on (an invalid edit reopens the editor); nothing here exits.
pub async fn ask(paths: &Paths, req: IpcRequest) -> Result<IpcResponse, CliError> {
    reply(crate::ipc::request_head(paths, &req).await)
}

/// A reply of a variant the command does not expect, as from a head of
/// another version: an error with code `internal`, never a panic.
pub fn unexpected(resp: IpcResponse) -> anyhow::Error {
    CliError::err("internal", format!("unexpected daemon reply: {resp:?}"))
}

/// One task, as `task show` prints it: JSON, or a one-row table.
pub fn print_task(t: &Task, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&t.to_json())?);
    } else {
        println!(
            "{}",
            table(&TASK_HEADER, &task_rows(std::slice::from_ref(t)))
        );
    }
    Ok(())
}

/// Tasks still holding a pane on a machine the flock no longer has. Nothing
/// reconciles them, so their state is the last one seen; the MACHINE column
/// says so instead of looking live. Rows and tasks are in the same order
/// (`task_rows`).
pub fn mark_removed(rows: &mut [Vec<String>], tasks: &[Task], known: impl Fn(&str) -> bool) {
    for (row, t) in rows.iter_mut().zip(tasks) {
        if let Some(m) = &t.machine
            && t.state.occupies_pane()
            && !known(m)
        {
            row[3] = format!("{m} (removed)");
        }
    }
}

pub const TASK_HEADER: [&str; 10] = [
    "ID", "STATE", "PRIORITY", "MACHINE", "FLOCK", "AGENT", "MODEL", "JOB", "AGE", "NOTE",
];

/// One flock in `pastor flock list`. `agents` is the live agents on its
/// machines, known only from a running head; `queued` counts the tasks
/// waiting for one of them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FlockRow {
    pub name: String,
    pub default: bool,
    pub machines: Vec<String>,
    /// Each of `machines` with the flock's number there and its live tasks
    /// of the flock, known only from a running head.
    pub members: Vec<FlockMember>,
    pub agents: Option<usize>,
    pub queued: usize,
    /// Its `[[flock]]` entry's `description`.
    pub description: Option<String>,
}

/// A machine in `FlockRow::members`. `share` and `max` are the flock's
/// number there (equal for a plain number), or for a machine with none
/// written (the old `flock` key, or the default flock of a machine no flock
/// lists) its `max_agents`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FlockMember {
    pub name: String,
    pub share: u32,
    pub max: u32,
    pub live: Option<usize>,
}

impl FlockMember {
    /// `desk 1/2`, or `desk -/2` with no head to count; with a share and a
    /// max, `desk 1/2/4`.
    pub fn label(&self) -> String {
        let live = self.live.map_or_else(|| "-".into(), |n| n.to_string());
        if self.share == self.max {
            format!("{} {live}/{}", self.name, self.max)
        } else {
            format!("{} {live}/{}/{}", self.name, self.share, self.max)
        }
    }
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
            let machines: Vec<String> = flock.members(name).into_iter().map(String::from).collect();
            // The flock's own tasks on a machine; a head from before many
            // flocks reports only the machine's.
            let live_on = |s: &MachineStatus| match s.flocks.iter().find(|f| f.name == name) {
                Some(seat) => seat.live,
                // A head that reports only its single `flock` (or none, before
                // flocks): count its live agents under that one flock, not
                // under every local flock the machine is configured into.
                None if s.flocks.is_empty()
                    && name == s.flock.as_deref().unwrap_or(flock.default_flock()) =>
                {
                    s.live
                }
                None => 0,
            };
            let members: Vec<FlockMember> = machines
                .iter()
                .map(|m| {
                    let number = flock
                        .machine_flocks(m)
                        .unwrap_or_default()
                        .into_iter()
                        .find(|(f, _)| *f == name)
                        .and_then(|(_, n)| n)
                        .or_else(|| flock.get(m).map(|c| FlockNumber::plain(c.max_agents)))
                        .unwrap_or(FlockNumber::plain(0));
                    FlockMember {
                        name: m.clone(),
                        share: number.share(),
                        max: number.max(),
                        live: live.map(|ms| ms.iter().find(|s| &s.name == m).map_or(0, live_on)),
                    }
                })
                .collect();
            let agents = live.map(|_| members.iter().filter_map(|m| m.live).sum());
            let queued = queued
                .iter()
                .filter(|t| t.flock.as_deref().unwrap_or(flock.default_flock()) == name)
                .count();
            FlockRow {
                name: name.to_string(),
                default: name == flock.default_flock(),
                machines,
                members,
                agents,
                queued,
                description: flock
                    .entry(name)
                    .and_then(|e| crate::config::clean_description(e.description.as_deref())),
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
                if f.members.is_empty() {
                    "-".into()
                } else {
                    f.members
                        .iter()
                        .map(FlockMember::label)
                        .collect::<Vec<_>>()
                        .join(", ")
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
        .filter(|m| flock.is_none_or(|f| m.in_flock(f)))
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineRow {
    pub name: String,
    pub host: String,
    pub endpoint: String,
    /// The flock that stands for the machine (`Flock::primary_flock`).
    pub flock: String,
    /// Every flock it is in, with its number and live tasks there. Empty
    /// from a head that predates it: then `flock` is the only one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flocks: Vec<crate::machine::FlockSeat>,
    pub channel: String,
    pub herdr_version: Option<String>,
    pub pastor_version: Option<String>,
    pub protocol: Option<u32>,
    pub error: Option<String>,
    pub live: Option<usize>,
    pub max_agents: u32,
    /// `MachineConfig::job_slots` and `burst`, shown after `max_agents` as
    /// `2+1j+1b` when either is set. Defaulted so a CLI can still read a
    /// head that predates these fields.
    #[serde(default)]
    pub job_slots: u32,
    #[serde(default)]
    pub burst: u32,
    pub tags: Vec<String>,
    /// `MachineStatus::orphans`; a probe works them out itself from
    /// `agent.list` and the store, and leaves them empty when it cannot.
    pub orphans: Vec<String>,
    /// `MachineStatus::now`; a probe leaves it empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub now: Vec<String>,
    /// `MachineStatus::profile`; a probe settles it from this machine's
    /// pastor.toml and flock.toml.
    #[serde(default)]
    pub profile: Option<String>,
    /// Its `description` in flock.toml.
    #[serde(default)]
    pub description: Option<String>,
}

impl MachineRow {
    pub fn in_flock(&self, flock: &str) -> bool {
        if self.flocks.is_empty() {
            self.flock == flock
        } else {
            self.flocks.iter().any(|f| f.name == flock)
        }
    }

    /// The FLOCKS column: every flock, with its number where it has one
    /// (`home,work:2`, `work:2/4` for a share and a max).
    pub fn flock_label(&self) -> String {
        if self.flocks.is_empty() {
            return self.flock.clone();
        }
        let names: Vec<String> = self
            .flocks
            .iter()
            .map(|f| match f.number_label() {
                Some(n) => format!("{}:{n}", f.name),
                None => f.name.clone(),
            })
            .collect();
        names.join(",")
    }
}

impl From<&MachineStatus> for MachineRow {
    fn from(m: &MachineStatus) -> MachineRow {
        MachineRow {
            name: m.name.clone(),
            host: m.host.clone(),
            endpoint: m.endpoint.clone(),
            // A head from before flocks has only the one.
            flock: m.flock.clone().unwrap_or_else(|| DEFAULT_FLOCK.into()),
            flocks: m.flocks.clone(),
            channel: m.channel.to_string(),
            herdr_version: m.herdr_version.clone(),
            pastor_version: m.pastor_version.clone(),
            protocol: m.protocol,
            error: m.error.clone(),
            live: Some(m.live),
            max_agents: m.max_agents,
            job_slots: m.job_slots,
            burst: m.burst,
            tags: m.tags.clone(),
            orphans: m.orphans.clone(),
            now: m.now.clone(),
            profile: m.profile.clone(),
            description: m.description.clone(),
        }
    }
}

/// AGENTS counts orphans too; ORPHANS names them (see `MachineStatus::orphans`).
pub const MACHINE_HEADER: [&str; 11] = [
    "NAME", "HOST", "FLOCKS", "PROFILE", "CHANNEL", "HERDR", "PASTOR", "AGENTS", "ORPHANS", "TAGS",
    "ERROR",
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

/// A machine's room as `machine list` shows it: `max_agents`, then
/// `+<n>j` for job slots and `+<n>b` for burst when they are set.
pub fn capacity(max_agents: u32, job_slots: u32, burst: u32) -> String {
    let mut out = max_agents.to_string();
    if job_slots > 0 {
        out.push_str(&format!("+{job_slots}j"));
    }
    if burst > 0 {
        out.push_str(&format!("+{burst}b"));
    }
    out
}

/// The AGENTS column: live over capacity, then the `--now` tasks that may
/// have taken it past (`4/3 now:t-9`).
fn agents_cell(m: &MachineRow) -> String {
    let mut out = format!(
        "{}/{}",
        m.live.map_or_else(|| "-".to_string(), |n| n.to_string()),
        capacity(m.max_agents, m.job_slots, m.burst)
    );
    if !m.now.is_empty() {
        out.push_str(&format!(" now:{}", m.now.join(",")));
    }
    out
}

/// One row per machine, in the order given.
pub fn machine_rows(ms: &[MachineRow]) -> Vec<Vec<String>> {
    let dash = || "-".to_string();
    ms.iter()
        .map(|m| {
            vec![
                m.name.clone(),
                m.host.clone(),
                m.flock_label(),
                m.profile.clone().unwrap_or_else(dash),
                m.channel.clone(),
                m.herdr_version.clone().unwrap_or_else(dash),
                m.pastor_version.clone().unwrap_or_else(dash),
                agents_cell(m),
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

pub const ORCHESTRATOR_HEADER: [&str; 8] = [
    "NAME",
    "KIND",
    "STATE",
    "SCHEDULE",
    "LAST RUN",
    "NEXT RUN",
    "AGENT",
    "LAST RESULT",
];

/// `orchestrator list`'s rows, one per file.
pub fn orchestrator_rows(list: &[crate::orchestrator::OrchestratorStatus]) -> Vec<Vec<String>> {
    list.iter()
        .map(|o| {
            let result = match &o.error {
                Some(e) => format!("invalid: {e}"),
                None => o.last_result.clone().unwrap_or_default(),
            };
            vec![
                o.name.clone(),
                o.kind.map_or("-".into(), |k| k.to_string()),
                o.state.clone(),
                o.schedule.clone().unwrap_or_else(|| "-".into()),
                o.last_run_at
                    .map(|t| format!("{} ago", age(t)))
                    .unwrap_or_else(|| "never".into()),
                o.next_run.map(in_).unwrap_or_else(|| "-".into()),
                o.task.map_or("-".into(), |t| format!("t-{t}")),
                one_line(result.trim()),
            ]
        })
        .collect()
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

/// A job in `job list --json` on a shepherd: its status, and `where` it
/// lives, so a script need not read the two tables.
#[derive(Debug, Serialize)]
pub struct PlacedJob<'a> {
    #[serde(flatten)]
    pub job: &'a JobStatus,
    /// `head` or `shepherd`.
    #[serde(rename = "where")]
    pub place: &'static str,
}

/// The head's jobs, then this machine's, each tagged with where it lives.
pub fn placed_jobs<'a>(head: &'a [JobStatus], here: &'a [JobStatus]) -> Vec<PlacedJob<'a>> {
    let tag = |jobs: &'a [JobStatus], place| jobs.iter().map(move |job| PlacedJob { job, place });
    tag(head, "head").chain(tag(here, "shepherd")).collect()
}

/// `job list` on a shepherd: the head's jobs under `head: <head>`, then this
/// machine's under `shepherd: <host> (this machine)`, a blank line between.
/// A side with no jobs prints its header and `no jobs`.
pub fn job_sections(head: &str, head_jobs: &[JobStatus], host: &str, here: &[JobStatus]) -> String {
    let section = |header: String, jobs: &[JobStatus]| {
        let body = if jobs.is_empty() {
            "no jobs".to_string()
        } else {
            table(&JOB_HEADER, &job_rows(jobs))
        };
        format!("{header}\n{}", body.trim_end())
    };
    format!(
        "{}\n\n{}",
        section(format!("head: {head}"), head_jobs),
        section(format!("shepherd: {host} (this machine)"), here)
    )
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
            description: None,
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
            ended: false,
            retry_of: None,
            priority: Default::default(),
            priority_from: None,
            queue_pos: 0,
            aged_from: None,
            aged_at: None,
            pause: Default::default(),
            waiting_until: None,
            usage: None,
            summary: None,
            created_at: now,
            started_at: Some(now),
            finished_at: None,
            updated_at: now,
            flock: None,
            role: Default::default(),
        }
    }

    fn status(name: &str, host: &str) -> MachineStatus {
        MachineStatus {
            now: Vec::new(),
            description: None,
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
            live_jobs: 0,
            job_slots: 0,
            burst: 0,
            tags: vec!["fast".into(), "arm".into()],
            orphans: vec![],
            flock: None,
            flocks: vec![],
            live_by_flock: vec![],
            shutting_down: false,
            profile: None,
        }
    }

    fn head() -> HeadRow {
        HeadRow::new("desk".into(), Some("0.9.1".into()))
    }

    fn row(name: &str, host: &str) -> MachineRow {
        MachineRow::from(&status(name, host))
    }

    /// The head's own machine (the `local` one) comes first; the others
    /// keep flock order. FLOCKS follows HOST, and PROFILE follows FLOCKS.
    #[test]
    fn machine_table_puts_the_heads_machine_first_with_its_flock() {
        let mut rows = vec![
            MachineRow {
                flock: "work".into(),
                profile: Some("develop".into()),
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
                "NAME", "HOST", "FLOCKS", "PROFILE", "CHANNEL", "HERDR", "PASTOR", "AGENTS",
                "ORPHANS", "TAGS", "ERROR"
            ]
        );
        assert_eq!(cells(1)[..3], ["here", "local", "default"]);
        assert_eq!(
            cells(2),
            [
                "pi-3",
                "user@pi-3",
                "work",
                "develop",
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
                "-",
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

    /// Job slots and burst show after max_agents only when set; the JSON
    /// always has both.
    #[test]
    fn machine_list_shows_job_slots_and_burst() {
        assert_eq!(capacity(2, 0, 0), "2");
        assert_eq!(capacity(2, 1, 1), "2+1j+1b");
        assert_eq!(capacity(3, 2, 0), "3+2j");
        assert_eq!(capacity(3, 0, 1), "3+1b");
        let m = MachineStatus {
            job_slots: 1,
            burst: 1,
            max_agents: 2,
            ..status("pi-3", "fleet@pi-3")
        };
        let row = MachineRow::from(&m);
        assert_eq!(machine_rows(std::slice::from_ref(&row))[0][7], "1/2+1j+1b");
        let v = serde_json::to_value(&row).unwrap();
        assert_eq!(v["job_slots"], 1);
        assert_eq!(v["burst"], 1);
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
            now: false,
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
            cwd: None,
            keep_pane: None,
            keep_pane_from: None,
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
                pull: false,
                description: None,
                name: "pi-1".into(),
                local: true,
                ssh: None,
                command: None,
                session: "default".into(),
                max_agents: 2,
                job_slots: 1,
                burst: 1,
                tags: vec![],
                flock: None,
                agent: None,
                agent_args: None,
                model: None,
                fallback: None,
                priority: None,
                agents: Default::default(),
                profile: None,
            }],
        };
        let mut rows = task_rows(&tasks);
        mark_removed(&mut rows, &tasks, |m| flock.get(m).is_some());
        assert_eq!(rows[0][3], "pi-3 (removed)");
        assert_eq!(
            rows[1][3], "pi-3",
            "a closed task is history, not a live row"
        );
        assert_eq!(rows[2][3], "pi-1");
    }

    #[test]
    fn task_detail_prints_the_agent_args_shell_quoted() {
        let spec = crate::task::DispatchSpec {
            now: false,
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
            session_id: None,
            label: Default::default(),
            summary: Default::default(),
            cwd: None,
            keep_pane: None,
            keep_pane_from: None,
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

    /// `task describe` says where the agent and its args came from, when the
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
                model: None,
                model_from: None,
                fallback: vec![],
                fallback_from: None,
                fallback_use: None,
                profile: None,
                profile_from: None,
                timeout_from: None,
                place_from: None,
                lists_unapplied: None,
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
                model: None,
                model_from: None,
                fallback: vec![],
                fallback_from: None,
                fallback_use: None,
                profile: None,
                profile_from: None,
                timeout_from: None,
                place_from: None,
                lists_unapplied: None,
            })),
            ..spec
        }));
        assert!(
            bare.contains("agent:      claude-personal (from defaults)\n"),
            "{bare}"
        );
        assert!(bare.contains("agent args: -\n"), "{bare}");
    }

    /// `task describe` shows the fallback list and where it came from; a
    /// layer's `[]` reads as none from that layer.
    #[test]
    fn task_detail_prints_the_fallback_and_where_it_came_from() {
        let spec = |fallback: &[&str], from: Option<&str>| crate::task::DispatchSpec {
            agent_source: Some(Box::new(crate::task::AgentSource {
                agent: "defaults".into(),
                fallback: fallback.iter().map(|s| s.to_string()).collect(),
                fallback_from: from.map(Into::into),
                ..Default::default()
            })),
            ..serde_json::from_str(r#"{"agent": "claude"}"#).unwrap()
        };
        let out = task_detail(&task_with(spec(&["sonnet", "gpt"], Some("flock personal"))));
        assert!(
            out.contains("fallback:   sonnet, gpt (from flock personal)\n"),
            "{out}"
        );
        let out = task_detail(&task_with(spec(&[], Some("machine m"))));
        assert!(out.contains("fallback:   - (from machine m)\n"), "{out}");
        let out = task_detail(&task_with(spec(&[], None)));
        assert!(out.contains("fallback:   -\n"), "{out}");
    }

    /// `task describe` says when the lists never reach the agent (a
    /// profiled Codex task), and says nothing on an empty list.
    #[test]
    fn task_detail_says_when_the_lists_are_not_applied() {
        let why = "not applied: codex has no per-command allow or deny flag; it runs in its workspace-write sandbox";
        let spec = crate::task::DispatchSpec {
            allow: vec!["Edit".into()],
            agent_source: Some(Box::new(crate::task::AgentSource {
                agent: "defaults".into(),
                profile: Some("develop".into()),
                lists_unapplied: Some(why.into()),
                ..Default::default()
            })),
            ..serde_json::from_str(r#"{"agent": "codex"}"#).unwrap()
        };
        let out = task_detail(&task_with(spec));
        assert!(
            out.contains(&format!("allow:      Edit ({why})\n")),
            "{out}"
        );
        assert!(out.contains("deny:       -\n"), "{out}");
    }

    /// `task describe` shows what a finished Claude task's session used,
    /// and nothing for a task pastor has read no usage for.
    #[test]
    fn a_task_with_usage_shows_its_model_and_tokens() {
        let mut t = task_with(serde_json::from_str(r#"{"agent": "claude"}"#).unwrap());
        assert!(!task_detail(&t).contains("tokens:"));
        t.usage = Some(crate::usage::TaskUsage {
            model: Some("claude-opus-4-5".into()),
            api_calls: 42,
            input: 1_234,
            cache_write: 56_000,
            cache_read: 3_400_500,
            output: 12,
            read_at: Utc::now(),
        });
        let out = task_detail(&t);
        assert!(
            out.contains("\nused:       claude-opus-4-5, 42 API calls\n"),
            "{out}"
        );
        assert!(
            out.contains(
                "\ntokens:     input 1,234, cache write 56,000, cache read 3,400,500, output 12\n"
            ),
            "{out}"
        );
        assert_eq!(
            t.to_json()["usage"]["cache_read"],
            3_400_500,
            "task list --json"
        );
        t.usage.as_mut().unwrap().model = None;
        assert!(task_detail(&t).contains("\nused:       42 API calls\n"));
    }

    /// `task describe` says when a paused task was paused and for which
    /// task, and marks a task that may pause one.
    #[test]
    fn a_paused_task_says_when_and_for_whom() {
        let mut t = task_with(serde_json::from_str(r#"{"agent": "claude"}"#).unwrap());
        t.priority = crate::task::Priority::Low;
        t.state = crate::task::TaskState::Paused;
        t.pause.paused_at = Some(Utc::now());
        t.pause.paused_for = Some(9);
        let out = task_detail(&t);
        assert!(out.contains("state:      paused\n"), "{out}");
        assert!(out.contains(" for t-9\n"), "{out}");
        assert!(!out.contains("resumed:"), "{out}");
        t.pause.preempt = true;
        t.priority = crate::task::Priority::Critical;
        assert!(task_detail(&t).contains("critical, preempt: "));
    }

    /// A `task run --now` task says it ran past its machine's limits, in
    /// `task describe` and in `task list`'s NOTE.
    #[test]
    fn a_now_task_says_it_skipped_the_queue() {
        let mut t = task_with(serde_json::from_str(r#"{"agent": "claude"}"#).unwrap());
        assert!(!task_detail(&t).contains("now:"));
        assert!(!task_note(&t).starts_with("now"));
        t.spec.now = true;
        assert!(
            task_detail(&t).contains("normal, now: started at once, past its machine's limits"),
            "{}",
            task_detail(&t)
        );
        assert!(task_note(&t).starts_with("now: "), "{}", task_note(&t));
    }

    /// `machine list` AGENTS names the `--now` tasks that run past the
    /// machine's limits beside the count, which may pass them.
    #[test]
    fn machine_rows_name_now_tasks() {
        let m = MachineStatus {
            live: 4,
            max_agents: 3,
            now: vec!["t-9".into()],
            ..status("pi", "local")
        };
        let rows = machine_rows(&[MachineRow::from(&m)]);
        assert_eq!(rows[0][7], "4/3 now:t-9");
        let plain = MachineStatus {
            now: vec![],
            ..m.clone()
        };
        assert_eq!(machine_rows(&[MachineRow::from(&plain)])[0][7], "4/3");
    }

    /// `task describe` gives the level and the layer that set it, and
    /// `task list` the level, beside the state.
    #[test]
    fn a_tasks_priority_shows_with_where_it_came_from() {
        let mut t = task_with(serde_json::from_str(r#"{"agent": "claude"}"#).unwrap());
        assert!(
            task_detail(&t).contains("priority:   normal\n"),
            "{}",
            task_detail(&t)
        );
        t.priority = crate::task::Priority::High;
        t.priority_from = Some("flock work".into());
        let out = task_detail(&t);
        assert!(
            out.contains("priority:   high (from flock work)\n"),
            "{out}"
        );
        let rows = task_rows(std::slice::from_ref(&t));
        assert_eq!(TASK_HEADER[2], "PRIORITY");
        assert_eq!(rows[0][2], "high");
        let json = t.to_json();
        assert_eq!(json["priority"], "high");
        assert_eq!(json["priority_from"], "flock work");
        t.priority = crate::task::Priority::High;
        t.aged_from = Some(crate::task::Priority::Low);
        let out = task_detail(&t);
        assert!(
            out.contains("priority:   high, aged from low (from flock work)\n"),
            "{out}"
        );
    }

    /// `task describe` says whether the task keeps its pane and where that
    /// came from, `--json` carries both, and `task list` marks a done task
    /// whose pane was kept.
    #[test]
    fn keep_pane_shows_with_where_it_came_from_and_marks_the_list() {
        use crate::task::TaskState;
        let mut t = task_with(serde_json::from_str(r#"{"agent": "claude"}"#).unwrap());
        t.state = TaskState::Done;
        t.pane_id = Some("p1".into());
        let out = task_detail(&t);
        assert!(out.contains("keep pane:  no\n"), "{out}");
        assert_eq!(task_rows(std::slice::from_ref(&t))[0][1], "done");
        t.spec.keep_pane = Some(true);
        t.spec.keep_pane_from = Some("task run".into());
        let out = task_detail(&t);
        assert!(out.contains("keep pane:  yes (from task run)\n"), "{out}");
        assert_eq!(task_rows(std::slice::from_ref(&t))[0][1], "done (kept)");
        assert_eq!(t.to_json()["spec"]["keep_pane"], true);
        assert_eq!(t.to_json()["spec"]["keep_pane_from"], "task run");
        t.state = TaskState::Running;
        assert_eq!(task_rows(std::slice::from_ref(&t))[0][1], "running");
    }

    /// A waiting task reads `waiting 03:00` in `task list`, and `task
    /// describe` says until when and on which limit.
    #[test]
    fn a_waiting_task_shows_its_reset_and_its_limit() {
        use crate::task::TaskState;
        let mut t = task_with(serde_json::from_str(r#"{"agent": "claude"}"#).unwrap());
        let until = chrono::Utc::now() + chrono::Duration::minutes(30);
        t.state = TaskState::Waiting;
        t.waiting_until = Some(until);
        t.error = Some("me exhausted until 03:00 (5-hour limit, seen by t-4)".into());
        let shown = crate::limit::local_time(until, chrono::Utc::now());
        assert_eq!(
            task_rows(std::slice::from_ref(&t))[0][1],
            format!("waiting {shown}")
        );
        let out = task_detail(&t);
        assert!(out.contains("\nwaiting:    until "), "{out}");
        assert!(
            out.contains("\nlimit:      me exhausted until 03:00 (5-hour limit, seen by t-4)\n"),
            "{out}"
        );
        assert!(!out.contains("\nerror:"), "{out}");
        // Listed first, the soonest reset first; the rest as they came.
        let at = |id: i64, state: TaskState, mins: i64| {
            let mut t = t.clone();
            t.id = id;
            t.state = state;
            t.waiting_until = Some(until + chrono::Duration::minutes(mins));
            t
        };
        let listed = waiting_first(vec![
            at(5, TaskState::Running, 0),
            at(4, TaskState::Waiting, 60),
            at(3, TaskState::Queued, 0),
            at(2, TaskState::Waiting, 5),
        ]);
        let ids: Vec<i64> = listed.iter().map(|t| t.id).collect();
        assert_eq!(ids, [2, 4, 5, 3]);
        assert_eq!(t.to_json()["waiting_until"], serde_json::json!(until));
    }

    /// `task describe` gives the workspace label and where it came from:
    /// the template until dispatch names the workspace, then that name,
    /// with a note when it joined a workspace or fell back to `t-N`.
    #[test]
    fn a_tasks_label_shows_with_where_it_came_from() {
        let mut t = task_with(serde_json::from_str(r#"{"agent": "claude"}"#).unwrap());
        let out = task_detail(&t);
        assert!(
            out.contains("label:      {{ flock }}/{{ task.id }} (built-in)\n"),
            "{out}"
        );
        t.spec.label.template = Some("{{ machine }}/{{ task.id }}".into());
        t.spec.label.from = Some("flock work".into());
        let out = task_detail(&t);
        assert!(
            out.contains("label:      {{ machine }}/{{ task.id }} (from flock work)\n"),
            "{out}"
        );
        t.spec.label.name = Some("pi-1/t-1".into());
        let out = task_detail(&t);
        assert!(
            out.contains("label:      pi-1/t-1 (from flock work)\n"),
            "{out}"
        );
        t.spec.label.name = Some("t-1".into());
        t.spec.label.note = Some("fell back to t-1: it renders empty".into());
        let out = task_detail(&t);
        assert!(
            out.contains("label:      t-1 (from flock work; fell back to t-1: it renders empty)\n"),
            "{out}"
        );
        t.spec.label.name = Some("pastor".into());
        t.spec.label.note = Some("joined workspace".into());
        let out = task_detail(&t);
        assert!(
            out.contains("label:      pastor (joined workspace)\n"),
            "{out}"
        );
    }

    /// `task describe` says whether pastor asks the task for a summary.
    #[test]
    fn a_task_shows_its_summary_setting() {
        let mut t = task_with(serde_json::from_str(r#"{"agent": "claude"}"#).unwrap());
        let out = task_detail(&t);
        assert!(
            out.contains("summary:    ask (line added to the prompt)\n"),
            "{out}"
        );
        t.spec.summary = crate::task::SummaryMode::Require;
        assert!(
            task_detail(&t)
                .contains("summary:    require (line added to the prompt; fails without one)\n")
        );
        t.spec.summary = crate::task::SummaryMode::Off;
        assert!(task_detail(&t).contains("summary:    off (nothing added to the prompt)\n"));
    }

    /// `task describe` names the task's role, and `task list --json`'s
    /// record carries it, plain agents included.
    #[test]
    fn a_task_shows_its_role() {
        let mut t = task_with(serde_json::from_str(r#"{"agent": "claude"}"#).unwrap());
        assert!(task_detail(&t).contains("role:       agent\n"));
        assert_eq!(t.to_json()["role"], "agent");
        t.role = crate::task::TaskRole::Orchestrator;
        assert!(task_detail(&t).contains("role:       orchestrator\n"));
        assert_eq!(t.to_json()["role"], "orchestrator");
    }

    /// `task describe` says what the task is about and where that came
    /// from, near the top.
    #[test]
    fn task_detail_shows_the_description_and_its_source() {
        let mut t = task_with(crate::task::DispatchSpec {
            now: false,
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
            cwd: None,
            keep_pane: None,
            keep_pane_from: None,
        });
        let out = task_detail(&t);
        assert!(
            out.starts_with("id:         t-3\ndescription: fix it (from the prompt)\n"),
            "{out}"
        );
        t.description = Some("Fix the flaky test".into());
        let out = task_detail(&t);
        assert!(
            out.contains("\ndescription: Fix the flaky test (from --description)\n"),
            "{out}"
        );
    }

    /// An error can be raw multi-line stderr; it must stay one field on one
    /// line, escaped the way the events log's human lines escape it.
    #[test]
    fn task_detail_keeps_a_multiline_error_on_its_line() {
        let mut t = task_with(crate::task::DispatchSpec {
            now: false,
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
            cwd: None,
            keep_pane: None,
            keep_pane_from: None,
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
            now: false,
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
            cwd: None,
            keep_pane: None,
            keep_pane_from: None,
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
            now: false,
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
            session_id: None,
            label: Default::default(),
            summary: Default::default(),
            cwd: None,
            keep_pane: None,
            keep_pane_from: None,
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

    fn job_status(name: &str) -> JobStatus {
        JobStatus {
            name: name.into(),
            schedule: Some("every 1h".into()),
            enabled: true,
            connector: Some("clock".into()),
            error: None,
            last_run_at: None,
            last_result: None,
            next_due: None,
            running: false,
            flock: None,
            description: None,
        }
    }

    /// Two tables, the head's first; a side with none says `no jobs`.
    #[test]
    fn job_sections_show_the_head_then_this_machine() {
        let text = job_sections("user@pi-1", &[job_status("a")], "laptop", &[]);
        let (head, here) = text.split_once("\n\n").unwrap();
        assert_eq!(
            head,
            format!(
                "head: user@pi-1\n{}",
                table(&JOB_HEADER, &job_rows(&[job_status("a")])).trim_end()
            )
        );
        assert_eq!(here, "shepherd: laptop (this machine)\nno jobs");
        let text = job_sections("user@pi-1", &[], "laptop", &[job_status("b")]);
        assert!(
            text.starts_with("head: user@pi-1\nno jobs\n\nshepherd: laptop (this machine)\nNAME"),
            "{text}"
        );
    }

    /// One flat array in `--json`, each job saying where it lives.
    #[test]
    fn placed_jobs_say_where_each_job_lives() {
        let head = [job_status("a")];
        let here = [job_status("b"), job_status("c")];
        let v = serde_json::to_value(placed_jobs(&head, &here)).unwrap();
        let got: Vec<(&str, &str)> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|j| (j["name"].as_str().unwrap(), j["where"].as_str().unwrap()))
            .collect();
        assert_eq!(got, [("a", "head"), ("b", "shepherd"), ("c", "shepherd")]);
        assert_eq!(v[0]["schedule"], "every 1h");
    }

    /// `--wide` adds DESCRIPTION last. On a terminal it is cut to what the
    /// other columns leave, 20 characters at least; off one it is whole.
    #[test]
    fn wide_tables_add_a_description_cut_to_the_width() {
        let rows = vec![
            vec!["a".to_string(), "x".to_string()],
            vec!["bb".to_string(), "".to_string()],
        ];
        let long = "Carry out the answers on the Pastor board's Answered list";
        let descriptions = vec![Some(long.to_string()), None];
        let whole = wide_table(&["A", "BB"], &rows, &descriptions, None);
        assert_eq!(
            whole,
            format!("A   BB  DESCRIPTION\na   x   {long}\nbb      -")
        );
        // "a   x   " is 8 wide, so 40 columns leave 32.
        let cut = wide_table(&["A", "BB"], &rows, &descriptions, Some(40));
        let first = cut.lines().nth(1).unwrap();
        assert_eq!(first.chars().count(), 40, "{first}");
        assert!(first.ends_with('…'), "{first}");
        assert!(
            first.starts_with("a   x   Carry out the answers"),
            "{first}"
        );
        let narrow = wide_table(&["A", "BB"], &rows, &descriptions, Some(10));
        let first = narrow.lines().nth(1).unwrap();
        assert_eq!(first.chars().count(), 8 + 20, "at least 20 kept: {first}");
        // A description that fits is left alone; a newline shows escaped.
        let short = vec![Some("two\nlines".to_string()), None];
        let out = wide_table(&["A", "BB"], &rows, &short, Some(40));
        assert!(out.contains("two\\nlines"), "{out}");
        assert!(!out.contains('…'), "{out}");
    }

    #[test]
    fn job_rows_show_errors_over_results_and_relative_next() {
        use crate::scheduler::JobStatus;
        let now = chrono::Utc::now();
        let ok = JobStatus {
            description: None,
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
            description: None,
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
        assert_eq!(rows[0][7], "3/4");
        assert_eq!(rows[0][8], "t-4,t-9");
        assert_eq!(rows[1][8], "-");
        assert_eq!(rows[0].len(), MACHINE_HEADER.len());
    }

    /// `flock list` gives each machine of a flock with the flock's number
    /// there and its live tasks of the flock: `desk 1/2`; without a head,
    /// `desk -/2`. A machine with no number written shows its max_agents.
    #[test]
    fn a_flock_row_shows_each_machine_with_its_number_and_live_count() {
        let f: Flock = toml::from_str(
            "[[flock]]\nname = \"home\"\ndefault = true\n\n[[flock]]\nname = \"work\"\nmachines = { desk = 2 }\n\n\
             [[machine]]\nname = \"desk\"\nlocal = true\nmax_agents = 4\n\n\
             [[machine]]\nname = \"lab\"\nssh = \"user@lab\"\nflock = \"work\"\n",
        )
        .unwrap();
        let seat = |name: &str, max: Option<u32>, live: usize| crate::machine::FlockSeat {
            name: name.into(),
            share: None,
            max,
            live,
        };
        let desk = MachineStatus {
            flocks: vec![seat("work", Some(2), 1)],
            ..status("desk", "local")
        };
        let rows = flock_list(&f, Some(&[desk]), &[]);
        assert_eq!(rows[1].machines, ["desk", "lab"]);
        assert_eq!(flock_rows(&rows)[1][2], "desk 1/2, lab 0/2");
        assert_eq!(rows[1].agents, Some(1));
        assert_eq!(flock_rows(&rows)[0][2], "-");
        let rows = flock_list(&f, None, &[]);
        assert_eq!(flock_rows(&rows)[1][2], "desk -/2, lab -/2");
        let json = serde_json::to_value(&rows[1]).unwrap();
        assert_eq!(
            json["members"],
            serde_json::json!([
                {"name": "desk", "share": 2, "max": 2, "live": null},
                {"name": "lab", "share": 2, "max": 2, "live": null}
            ])
        );
    }

    /// A share and a max read `2/4`: `desk 1/2/4` in `flock list` (live,
    /// share, max), `work:2/4` in `machine list`, and both numbers in JSON.
    #[test]
    fn a_share_and_a_max_show_as_two_numbers() {
        let f: Flock = toml::from_str(
            "[[flock]]\nname = \"work\"\ndefault = true\nmachines = { desk = { share = 2, max = 4 } }\n\n\
             [[machine]]\nname = \"desk\"\nlocal = true\nmax_agents = 4\n",
        )
        .unwrap();
        let seat = crate::machine::FlockSeat::new(
            "work",
            Some(crate::config::flock::FlockNumber::split(2, 4)),
            1,
        );
        let desk = MachineStatus {
            flock: Some("work".into()),
            flocks: vec![seat.clone()],
            ..status("desk", "local")
        };
        let rows = flock_list(&f, Some(std::slice::from_ref(&desk)), &[]);
        assert_eq!(flock_rows(&rows)[0][2], "desk 1/2/4");
        assert_eq!(
            serde_json::to_value(&rows[0]).unwrap()["members"],
            serde_json::json!([{"name": "desk", "share": 2, "max": 4, "live": 1}])
        );
        let row = MachineRow::from(&desk);
        assert_eq!(row.flock_label(), "work:2/4");
        let json = serde_json::to_value(&row).unwrap();
        assert_eq!(
            json["flocks"],
            serde_json::json!([{"name": "work", "share": 2, "max": 4, "live": 1}])
        );
        // A head from before shares sends only `max`: its number was a
        // plain ceiling.
        let old: crate::machine::FlockSeat =
            serde_json::from_value(serde_json::json!({"name": "work", "max": 2, "live": 2}))
                .unwrap();
        assert_eq!(old.number_label().as_deref(), Some("2"));
        assert!(!old.has_room() && !old.under_share());
    }

    /// A status carries every flock of the machine; one from a head before
    /// many flocks has only `flock` and reads as that one flock.
    #[test]
    fn a_machine_row_carries_its_flocks_and_an_old_head_reads_as_one() {
        let seat = |name: &str, max: Option<u32>, live: usize| crate::machine::FlockSeat {
            name: name.into(),
            share: None,
            max,
            live,
        };
        let m = MachineStatus {
            flock: Some("home".into()),
            flocks: vec![seat("home", None, 1), seat("work", Some(2), 0)],
            ..status("desk", "local")
        };
        let row = MachineRow::from(&m);
        assert_eq!(row.flock_label(), "home,work:2");
        assert!(row.in_flock("work") && row.in_flock("home") && !row.in_flock("play"));
        assert_eq!(
            orphan_lines(
                &[MachineStatus {
                    orphans: vec!["t-1".into()],
                    ..m.clone()
                }],
                None,
                Some("work")
            )
            .len(),
            1
        );
        let back: MachineStatus =
            serde_json::from_value(serde_json::to_value(&m).unwrap()).unwrap();
        assert_eq!(back.flocks, m.flocks);

        let mut old = serde_json::to_value(&m).unwrap();
        old.as_object_mut().unwrap().remove("flocks");
        old["flock"] = "work".into();
        let old: MachineStatus = serde_json::from_value(old).unwrap();
        assert_eq!(old.flock_names(), ["work"]);
        let row = MachineRow::from(&old);
        assert_eq!(row.flock_label(), "work");
        assert!(row.in_flock("work") && !row.in_flock("home"));
    }
}
