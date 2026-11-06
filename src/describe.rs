//! `pastor job|machine|flock|connector describe`: one thing in full, for a human, or
//! as JSON with `--json`. A running head builds a machine's or a flock's
//! description itself (`IpcRequest::MachineDescribe`, `FlockDescribe`); with
//! none, the CLI gathers the parts from the files and the store. This module
//! holds their shape, the parts both sides share, and how they read.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::cli::CliError;
use crate::cli::{
    JOB_HEADER, MachineRow, TASK_HEADER, age, in_, job_rows, one_line, table, task_rows,
};
use crate::config::Paths;
use crate::config::flock::Flock;
use crate::connector::install::Origin;
use crate::edit::ConfigFile;
use crate::events::EventRecord;
use crate::herdr::shell_quote;
use crate::machine::MachineStatus;
use crate::scheduler::JobStatus;
use crate::store::{Store, TaskFilter};
use crate::task::{LIVE_STATES, PANE_OWNING_STATES, Task};

/// How many recent tasks and events a description lists.
pub const RECENT: usize = 10;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobDescription {
    pub name: String,
    /// The file's `description`.
    #[serde(default)]
    pub description: Option<String>,
    pub file: String,
    pub schedule: Option<String>,
    pub enabled: bool,
    /// The file's current problem, if it has one.
    pub error: Option<String>,
    pub running: bool,
    pub flock: Option<String>,
    /// The `[connector]` table as written, `use` included.
    pub connector: Option<serde_json::Value>,
    /// The `[dispatch]` table as written.
    pub dispatch: Option<serde_json::Value>,
    pub next_due: Option<DateTime<Utc>>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_ok_at: Option<DateTime<Utc>>,
    pub last_result: Option<String>,
    pub last_error: Option<String>,
    pub failures: u32,
    pub backoff_until: Option<DateTime<Utc>>,
    /// Its most recent tasks, newest first.
    pub tasks: Vec<Task>,
    /// Its most recent `job.*` events, oldest first.
    pub events: Vec<EventRecord>,
}

/// `job describe` for `name`: how `statuses` (the head's, or a standalone
/// scheduler's) report it, its tables as written in its file under `paths`,
/// and its state, recent tasks and events from `store` and the events log.
/// The head (`JobDescribe`) and the CLI with no head both build it here.
pub fn job(
    paths: &Paths,
    name: &str,
    statuses: Vec<JobStatus>,
    store: &Store,
) -> anyhow::Result<JobDescription> {
    let path = ConfigFile::Job(name.to_string()).path(paths)?;
    let Some(status) = statuses.into_iter().find(|j| j.name == name) else {
        return Err(CliError::err(
            "job_not_found",
            format!("the head has no job {name}"),
        ));
    };
    // The tables as written, so a file that does not parse still shows what
    // it says as far as TOML goes.
    let raw: Option<toml::Table> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| toml::from_str(&t).ok());
    let table = |key: &str| {
        raw.as_ref()
            .and_then(|r| r.get(key))
            .and_then(|v| serde_json::to_value(v).ok())
    };
    let state = store.job_state(name)?.unwrap_or_default();
    let tasks = store
        .list_tasks(&TaskFilter {
            job: Some(name.to_string()),
            ..Default::default()
        })?
        .into_iter()
        .take(RECENT)
        .collect();
    let log = crate::events::read(&paths.events_file(), None).unwrap_or_default();
    let events = recent_events(log, |e| {
        e.kind.starts_with("job.") && e.job.as_deref() == Some(name)
    });
    Ok(JobDescription {
        name: name.to_string(),
        description: status.description.clone(),
        file: path.display().to_string(),
        schedule: status.schedule,
        enabled: status.enabled,
        error: status.error,
        running: status.running,
        flock: status.flock,
        connector: table("connector"),
        dispatch: table("dispatch"),
        next_due: status.next_due,
        last_run_at: status.last_run_at.or(state.last_run_at),
        last_ok_at: state.last_ok_at,
        last_result: status.last_result.or(state.last_result),
        last_error: state.last_error,
        failures: state.failures,
        backoff_until: state.backoff_until,
        tasks,
        events,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineDescription {
    #[serde(flatten)]
    pub row: MachineRow,
    pub session: String,
    /// The machine's own `model`, after its task's flock's; `None` falls
    /// through to `[defaults]`.
    pub model: Option<String>,
    /// The machine's own `fallback`, before its flock's; `None` falls
    /// through to the flock's, then `[defaults]`. Missing from an older head.
    #[serde(default)]
    pub fallback: Option<Vec<String>>,
    /// The machine's own `agents`, the agent per kind for a model of
    /// another kind than its agent's. Missing from an older head.
    #[serde(default)]
    pub agents_by_kind: crate::config::KindAgents,
    /// The tasks whose agent holds a pane on it, newest first.
    pub tasks: Vec<Task>,
    /// Its recent `machine.*` events that carried an error, and its failed
    /// tasks, oldest first.
    pub recent_errors: Vec<EventRecord>,
    /// Its bundles: what it is meant to have, how it takes a change, the
    /// last apply and each harness home. Missing when no bundle is meant for
    /// it and none was applied, and from an older head.
    #[serde(default)]
    pub config: Option<crate::bundle::keep::ConfigDescription>,
    /// The workdir sets the head placed on it, as last read: `None` when it
    /// placed none. Missing from an older head.
    #[serde(default)]
    pub workdirs: Option<crate::bundle::workdir_keep::WorkdirsDescription>,
    /// The agents found signed out on it (`signed_out_on`). Missing from an
    /// older head.
    #[serde(default)]
    pub signed_out: Vec<crate::limit::AccountLimit>,
    /// How its bootstrap stands, if it has a state: `None` for a machine
    /// that never had one, and from an older head.
    #[serde(default)]
    pub bootstrap: Option<crate::store::MachineState>,
}

/// A cordon in one line: `since 2026-10-31 10:00: kernel update`.
pub fn cordon_line(c: &crate::store::Cordon) -> String {
    let since = c.since.format("%Y-%m-%d %H:%M");
    match &c.reason {
        Some(reason) => format!("since {since}: {reason}"),
        None => format!("since {since}"),
    }
}

/// A machine's bootstrap in one line: `ok (user mode; isolation: none)`,
/// `pending`, or `failed at <step>: <why>`, the why already carrying the
/// step's fix as `; fix: <fix>`.
pub fn bootstrap_line(s: &crate::store::MachineState) -> String {
    use crate::store::BootstrapState;
    match s.bootstrap {
        BootstrapState::Ok => format!(
            "ok (user mode; isolation: {})",
            s.isolation.as_deref().unwrap_or("none")
        ),
        BootstrapState::Pending => "pending".to_string(),
        BootstrapState::Failed => format!(
            "failed at {}: {}",
            s.failed_step.as_deref().unwrap_or("bootstrap"),
            s.why.as_deref().unwrap_or("no reason given")
        ),
    }
}

/// Of `limits`, the signed-out rows about machine `name`: seen there, or
/// kept under one of its agents (`<name>/<agent>`).
pub fn signed_out_on(
    limits: Vec<crate::limit::AccountLimit>,
    name: &str,
) -> Vec<crate::limit::AccountLimit> {
    let own = format!("{name}/");
    limits
        .into_iter()
        .filter(|l| {
            l.signed_out && (l.machine.as_deref() == Some(name) || l.account.starts_with(&own))
        })
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlockDescription {
    pub name: String,
    /// Its `[[flock]]` entry's `description`.
    #[serde(default)]
    pub description: Option<String>,
    pub default: bool,
    /// The flock's own agent settings; `None` falls through to `[defaults]`.
    pub agent: Option<String>,
    pub agent_args: Option<Vec<String>>,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    /// The flock's own `model`, before the machine's; `None` falls through
    /// to the machine's, then `[defaults]`.
    pub model: Option<String>,
    /// The flock's own `fallback`, after the machine's; `None` falls
    /// through to `[defaults]`. Missing from an older head.
    #[serde(default)]
    pub fallback: Option<Vec<String>>,
    /// The flock's own `agents`, as the machine's. Missing from an older
    /// head.
    #[serde(default)]
    pub agents_by_kind: crate::config::KindAgents,
    /// The flock's own `profile`, before the machine's, as `model`.
    #[serde(default)]
    pub profile: Option<String>,
    /// The flock's own `timeout`; `None` falls through to `[defaults]`.
    /// Missing from an older head.
    #[serde(default)]
    pub timeout: Option<String>,
    /// The flock's own `place`, as `timeout`.
    #[serde(default)]
    pub place: Option<crate::task::Place>,
    pub machines: Vec<String>,
    /// Live agents on its machines; known only from a running head.
    pub agents: Option<usize>,
    /// Its queued, starting, running and blocked tasks, newest first.
    pub tasks: Vec<Task>,
}

/// One connector. Everything from its manifest is `None` or empty when the
/// manifest does not load; `error` then says why.
#[derive(Debug, Clone, Serialize)]
pub struct ConnectorDescription {
    pub id: String,
    /// `ok`, `missing_secrets`, or `invalid` (see `error`).
    pub status: String,
    pub error: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub version: Option<String>,
    pub min_pastor_version: Option<String>,
    pub authors: Vec<String>,
    pub homepage: Option<String>,
    pub repository: Option<String>,
    pub license: Option<String>,
    /// Where commands run: the checkout, or a link's target.
    pub dir: String,
    pub origin: Origin,
    pub connector: Option<ConnectorCommand>,
    pub hooks: Vec<ConnectorHook>,
    /// Run when a task of one of its jobs is done or failed.
    pub finish: Option<ConnectorFinish>,
    /// Run by `pastor watch` each interval.
    pub watch: Option<ConnectorWatch>,
    pub env_file: String,
    /// Declared secrets and whether the `.env` sets them; never their values.
    pub secrets: Vec<ConnectorSecret>,
    pub missing_secrets: Vec<String>,
    /// The jobs whose `[connector] use` names it, as `job list` has them.
    pub jobs: Vec<JobStatus>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConnectorCommand {
    pub mode: String,
    pub command: Vec<String>,
    pub timeout_secs: u64,
    pub config: Vec<ConfigKey>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigKey {
    pub key: String,
    pub required: bool,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConnectorHook {
    pub on: Vec<String>,
    pub only_own: bool,
    pub command: Vec<String>,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConnectorFinish {
    pub command: Vec<String>,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConnectorWatch {
    pub command: Vec<String>,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConnectorSecret {
    pub name: String,
    pub description: Option<String>,
    pub set: bool,
}

/// An argv as one line a shell would read back: each word quoted, control
/// characters escaped. A manifest is its author's text, and an escape
/// sequence in it could otherwise draw a harmless command over the real one.
pub fn argv(cmd: &[String]) -> String {
    one_line(
        &cmd.iter()
            .map(|a| shell_quote(a))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// The last `RECENT` of `events` that `keep` selects, in log order.
pub fn recent_events(
    events: Vec<EventRecord>,
    keep: impl Fn(&EventRecord) -> bool,
) -> Vec<EventRecord> {
    let mut kept: Vec<EventRecord> = events.into_iter().filter(|e| keep(e)).collect();
    let skip = kept.len().saturating_sub(RECENT);
    kept.drain(..skip);
    kept
}

/// The tasks `machine describe` lists: those whose agent holds a pane on
/// `name`.
pub fn machine_tasks(name: &str) -> TaskFilter {
    TaskFilter {
        machine: Some(name.to_string()),
        states: Some(PANE_OWNING_STATES.to_vec()),
        ..Default::default()
    }
}

/// The tasks `flock describe` lists: the live ones in `name`.
pub fn flock_tasks(name: &str) -> TaskFilter {
    TaskFilter {
        flock: Some(name.to_string()),
        states: Some(LIVE_STATES.to_vec()),
        ..Default::default()
    }
}

/// `name`'s recent errors: its `machine.*` events that carried one, and
/// the tasks that failed on it.
pub fn machine_errors(events: Vec<EventRecord>, name: &str) -> Vec<EventRecord> {
    recent_events(events, |e| {
        let machine_error = e.kind.starts_with("machine.")
            && e.machine
                .as_ref()
                .is_some_and(|s| s.name == name && s.error.is_some());
        let failed_here = e.kind == "task.failed"
            && e.task.as_ref().and_then(|t| t.machine.as_deref()) == Some(name);
        machine_error || failed_here
    })
}

/// Flock `name` of `flock`, with the live agents of `live` when a head
/// knows them. `None` when `flock` has no such flock.
pub fn flock_description(
    flock: &Flock,
    name: &str,
    live: Option<&[MachineStatus]>,
    tasks: Vec<Task>,
) -> Option<FlockDescription> {
    let row = crate::cli::flock_list(flock, live, &[])
        .into_iter()
        .find(|r| r.name == name)?;
    let entry = flock.entry(name).cloned().unwrap_or_default();
    Some(FlockDescription {
        name: row.name,
        description: row.description,
        default: row.default,
        agent: entry.agent,
        agent_args: entry.agent_args,
        allow: entry.allow,
        deny: entry.deny,
        model: entry.model,
        fallback: entry.fallback,
        agents_by_kind: entry.agents,
        profile: entry.profile,
        timeout: entry.timeout,
        place: entry.place,
        machines: row.machines,
        agents: row.agents,
        tasks,
    })
}

/// A flock's or machine's own `fallback`: its names, `none` for `[]`, or
/// `unset` when it sets none.
fn fallback(list: &Option<Vec<String>>, unset: &str) -> String {
    match list.as_deref() {
        None => unset.to_string(),
        Some([]) => "none".to_string(),
        Some(names) => names.join(", "),
    }
}

/// `key: value` lines with the values in one column.
fn fields(rows: &[(&str, String)]) -> Vec<String> {
    let width = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0) + 2;
    rows.iter()
        .map(|(k, v)| format!("{:<width$}{v}", format!("{k}:")))
        .collect()
}

/// A description as `describe` shows it: whole, newlines kept, other
/// control characters escaped; `-` for none.
fn description(v: &Option<String>) -> String {
    dash(v.as_deref().map(crate::cli::printable))
}

fn dash(v: Option<String>) -> String {
    v.filter(|s| !s.is_empty()).unwrap_or_else(|| "-".into())
}

fn ago(v: Option<DateTime<Utc>>) -> String {
    dash(v.map(|at| format!("{} ({} ago)", at.format("%Y-%m-%d %H:%M:%S UTC"), age(at))))
}

fn yes(b: bool) -> String {
    if b { "yes" } else { "no" }.into()
}

fn words(v: &[String]) -> String {
    if v.is_empty() {
        "-".into()
    } else {
        v.iter()
            .map(|w| shell_quote(w))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// A titled section: the title, then `body` indented, or `none` after the
/// title when there is nothing in it.
fn section(out: &mut Vec<String>, title: &str, body: Vec<String>) {
    if body.is_empty() {
        out.push(format!("{title}: none"));
    } else {
        out.push(format!("{title}:"));
        out.extend(
            body.into_iter()
                .map(|l| format!("  {l}").trim_end().to_string()),
        );
    }
}

fn task_table(tasks: &[Task]) -> Vec<String> {
    if tasks.is_empty() {
        return vec![];
    }
    table(&TASK_HEADER, &task_rows(tasks))
        .lines()
        .map(str::to_string)
        .collect()
}

/// A TOML-ish table's keys, one `key = value` line each; a nested table is
/// its own dotted key.
fn toml_lines(v: &serde_json::Value, prefix: &str, out: &mut Vec<String>) {
    let Some(map) = v.as_object() else { return };
    for (k, v) in map {
        let key = if prefix.is_empty() {
            k.clone()
        } else {
            format!("{prefix}.{k}")
        };
        if v.is_object() {
            toml_lines(v, &key, out);
        } else {
            out.push(format!("{key} = {}", one_line(&v.to_string())));
        }
    }
}

pub fn job_text(j: &JobDescription) -> String {
    let next = if j.running {
        "running now".to_string()
    } else {
        dash(
            j.next_due
                .filter(|_| j.enabled)
                .map(|at| format!("{} ({})", at.format("%Y-%m-%d %H:%M:%S UTC"), in_(at))),
        )
    };
    let connector = j
        .connector
        .as_ref()
        .and_then(|c| c.get("use"))
        .and_then(|u| u.as_str())
        .map(str::to_string);
    let mut out = fields(&[
        ("name", j.name.clone()),
        ("description", description(&j.description)),
        ("file", j.file.clone()),
        ("schedule", dash(j.schedule.clone())),
        ("enabled", yes(j.enabled)),
        ("flock", dash(j.flock.clone())),
        ("connector", dash(connector)),
        ("next run", next),
        ("last run", ago(j.last_run_at)),
        ("last ok", ago(j.last_ok_at)),
        (
            "last result",
            dash(j.last_result.clone().map(|s| one_line(&s))),
        ),
        (
            "last error",
            dash(j.last_error.clone().map(|s| one_line(&s))),
        ),
        ("failures", j.failures.to_string()),
        ("backoff until", ago(j.backoff_until)),
    ]);
    if let Some(e) = &j.error {
        out.push(format!("file error: {}", one_line(e)));
    }
    let lines = |v: &Option<serde_json::Value>| {
        let mut l = Vec::new();
        if let Some(v) = v {
            toml_lines(v, "", &mut l);
        }
        l
    };
    let (connector, dispatch) = (lines(&j.connector), lines(&j.dispatch));
    section(&mut out, "connector config", connector);
    section(&mut out, "dispatch", dispatch);
    section(&mut out, "recent tasks", task_table(&j.tasks));
    section(
        &mut out,
        "recent events",
        j.events.iter().map(EventRecord::line).collect(),
    );
    out.join("\n")
}

/// `orchestrator describe`: the file's settings, its state, the note, the
/// last runs with the lines each pre script printed, and recent events.
pub fn orchestrator_text(d: &crate::orchestrator::OrchestratorDescription) -> String {
    let s = &d.status;
    let next = dash(
        s.next_run
            .map(|at| format!("{} ({})", at.format("%Y-%m-%d %H:%M:%S UTC"), in_(at))),
    );
    let mut rows = vec![
        ("name", s.name.clone()),
        ("description", description(&s.description)),
        ("file", d.file.display().to_string()),
        ("kind", dash(s.kind.map(|k| k.to_string()))),
        ("state", s.state.clone()),
        ("enabled", yes(s.enabled)),
        ("schedule", dash(s.schedule.clone())),
    ];
    if s.kind == Some(crate::orchestrator::Kind::Scheduled) {
        rows.push(("pre", words(&d.pre)));
        rows.push(("post", dash(d.post.as_deref().map(words))));
        rows.push(("timeout", dash(d.timeout.clone())));
    }
    if s.kind == Some(crate::orchestrator::Kind::Session) {
        rows.push(("stop grace", dash(d.stop_grace.clone())));
        if let Some(run) = &d.session {
            rows.push((
                "session",
                format!(
                    "started {} by {}, until {} ({}), {} restart{} in the last hour",
                    ago(Some(run.started_at)),
                    run.started_by,
                    run.until.format("%Y-%m-%d %H:%M:%S UTC"),
                    in_(run.until),
                    run.restarts.len(),
                    if run.restarts.len() == 1 { "" } else { "s" }
                ),
            ));
        }
    }
    rows.extend([
        ("model", dash(d.model.clone())),
        ("skill", dash(d.skill.clone())),
        ("repo", dash(d.repo.clone())),
        ("next run", next),
        ("last run", ago(s.last_run_at)),
        (
            "last result",
            dash(s.last_result.clone().map(|r| one_line(&r))),
        ),
        ("agent", dash(s.task.map(|t| format!("t-{t}")))),
        ("quota until", ago(s.quota_until)),
    ]);
    let mut out = fields(&rows);
    if let Some(e) = &s.error {
        out.push(format!("file error: {}", one_line(e)));
    }
    section(
        &mut out,
        "prompt",
        d.prompt
            .as_deref()
            .map(|p| p.lines().map(crate::cli::printable).collect())
            .unwrap_or_default(),
    );
    section(
        &mut out,
        "note",
        d.note
            .as_deref()
            .map(|n| n.lines().map(crate::cli::printable).collect())
            .unwrap_or_default(),
    );
    let mut runs = Vec::new();
    for r in d.runs.iter().rev() {
        let mut head = format!("{} {}", r.at.format("%Y-%m-%d %H:%M:%S UTC"), r.outcome);
        if let Some(t) = r.task {
            head.push_str(&format!(" t-{t}"));
        }
        if let Some(detail) = &r.detail {
            head.push_str(&format!(": {}", one_line(detail)));
        }
        runs.push(head);
        runs.extend(r.lines.iter().map(|l| format!("  {}", one_line(l))));
    }
    section(&mut out, "last runs, newest first", runs);
    section(
        &mut out,
        "recent events",
        d.events.iter().map(EventRecord::line).collect(),
    );
    out.join("\n")
}

/// An `agents` table as `kind=agent` words, or where the layer's lookup
/// goes on to when it has none.
fn by_kind(agents: &crate::config::KindAgents, next: &str) -> String {
    if agents.is_empty() {
        return format!("- (from {next})");
    }
    agents
        .iter()
        .map(|(kind, agent)| format!("{kind}={}", one_line(agent)))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn machine_text(m: &MachineDescription) -> String {
    let r = &m.row;
    let room = crate::cli::capacity(r.max_agents, r.job_slots, r.burst);
    let agents = match r.live {
        Some(n) => format!("{n} of {room}"),
        None => format!("- of {room}"),
    };
    let agents = if r.reserved > 0 {
        format!("{agents}, {} reserved for other panes", r.reserved)
    } else {
        agents
    };
    let mut rows = vec![
        ("name", r.name.clone()),
        ("description", description(&r.description)),
        ("host", r.host.clone()),
        ("endpoint", r.endpoint.clone()),
        ("flock", r.flock_label()),
        ("session", m.session.clone()),
        (
            "model",
            m.model
                .clone()
                .unwrap_or_else(|| "- (from [defaults])".into()),
        ),
        (
            "fallback",
            fallback(&m.fallback, "- (from its flock or [defaults])"),
        ),
        ("by kind", by_kind(&m.agents_by_kind, "its flock")),
        ("profile", dash(r.profile.clone())),
        ("channel", r.channel.clone()),
        ("herdr", dash(r.herdr_version.clone())),
        ("protocol", dash(r.protocol.map(|p| p.to_string()))),
        ("pastor", dash(r.pastor_version.clone())),
        ("version read", ago(r.pastor_read_at)),
        ("agents", agents),
        ("orphans", dash(Some(r.orphans.join(",")))),
        ("tags", dash(Some(r.tags.join(",")))),
        ("error", dash(r.problems().map(|e| one_line(&e)))),
    ];
    // Only for a machine that has been bootstrapped or is being: the rest
    // never had a state.
    if let Some(b) = &m.bootstrap {
        rows.insert(rows.len() - 1, ("bootstrap", bootstrap_line(b)));
    }
    // Likewise only for a machine an operator cordoned.
    if let Some(c) = &r.cordon {
        rows.insert(rows.len() - 1, ("cordoned", cordon_line(c)));
    }
    let mut out = fields(&rows);
    if let Some(c) = &m.config {
        section(&mut out, "bundles", config_lines(c));
    }
    if let Some(w) = &m.workdirs {
        section(
            &mut out,
            "workdirs",
            crate::bundle::workdir_keep::describe_lines(w),
        );
    }
    section(&mut out, "tasks", task_table(&m.tasks));
    // Only when there is one: most machines never have any.
    if !m.signed_out.is_empty() {
        let now = chrono::Utc::now();
        section(
            &mut out,
            "signed out",
            m.signed_out
                .iter()
                .map(|l| {
                    let agent = l.agent.as_deref().unwrap_or("-");
                    format!("{agent}: {}", l.note(now))
                })
                .collect(),
        );
    }
    section(
        &mut out,
        "recent errors",
        m.recent_errors.iter().map(EventRecord::line).collect(),
    );
    out.join("\n")
}

/// The `bundles` section of a machine's text: how it takes a plan, the last
/// apply, the bundles and each home.
fn config_lines(c: &crate::bundle::keep::ConfigDescription) -> Vec<String> {
    let mode = match c.apply_mode {
        crate::bundle::Apply::Auto => "auto",
        crate::bundle::Apply::Manual => "manual",
    };
    let last = match &c.last_apply {
        None => "never applied".to_string(),
        Some(a) if a.ok => format!("applied {}", ago(Some(a.at))),
        Some(a) => format!(
            "failed {}: {}",
            ago(Some(a.at)),
            one_line(a.error.as_deref().unwrap_or("-"))
        ),
    };
    let mut lines = vec![format!(
        "apply: {mode}; {}; {last}",
        if c.stale { "stale" } else { "up to date" }
    )];
    for b in &c.bundles {
        let was = b
            .applied_hash
            .as_deref()
            .map(|h| format!(" (applied {})", &h[..h.len().min(12)]))
            .unwrap_or_default();
        lines.push(format!(
            "bundle {} {} {}{was}",
            b.id,
            b.version,
            &b.hash[..b.hash.len().min(12)]
        ));
    }
    for h in &c.homes {
        let state = h.state.map_or("unread", |s| s.word());
        lines.push(format!(
            "home {} ({}): {state}",
            h.path,
            h.agents.join(", ")
        ));
    }
    lines
}

pub fn flock_text(f: &FlockDescription) -> String {
    let mut out = fields(&[
        ("name", f.name.clone()),
        ("description", description(&f.description)),
        ("default", yes(f.default)),
        (
            "agent",
            f.agent
                .clone()
                .unwrap_or_else(|| "- (from [defaults])".into()),
        ),
        (
            "agent args",
            f.agent_args
                .as_deref()
                .map_or_else(|| "- (from [defaults])".into(), words),
        ),
        ("allow", words(&f.allow)),
        ("deny", words(&f.deny)),
        (
            "model",
            f.model
                .clone()
                .unwrap_or_else(|| "- (from the machine or [defaults])".into()),
        ),
        ("fallback", fallback(&f.fallback, "- (from [defaults])")),
        ("by kind", by_kind(&f.agents_by_kind, "[defaults]")),
        (
            "profile",
            f.profile
                .clone()
                .unwrap_or_else(|| "- (from the machine or [defaults])".into()),
        ),
        (
            "timeout",
            f.timeout
                .clone()
                .unwrap_or_else(|| "- (from [defaults])".into()),
        ),
        (
            "place",
            f.place
                .as_ref()
                .map_or_else(|| "- (from [defaults])".into(), |p| p.to_string()),
        ),
        ("machines", dash(Some(f.machines.join(",")))),
        ("agents", dash(f.agents.map(|n| n.to_string()))),
    ]);
    section(&mut out, "tasks", task_table(&f.tasks));
    out.join("\n")
}

pub fn connector_text(c: &ConnectorDescription) -> String {
    let text = |v: &Option<String>| dash(v.as_deref().map(one_line));
    let status = match c.status.as_str() {
        "missing_secrets" => format!("missing secrets: {}", c.missing_secrets.join(", ")),
        "invalid" => format!("invalid: {}", one_line(c.error.as_deref().unwrap_or(""))),
        s => s.to_string(),
    };
    let mut rows = vec![
        ("id", c.id.clone()),
        ("name", text(&c.name)),
        ("version", dash(c.version.clone())),
        ("min pastor", dash(c.min_pastor_version.clone())),
        ("description", description(&c.description)),
        ("authors", dash(Some(one_line(&c.authors.join(", "))))),
        ("homepage", text(&c.homepage)),
        ("repository", text(&c.repository)),
        ("license", text(&c.license)),
        ("status", status),
        ("dir", one_line(&c.dir)),
    ];
    let unknown = || "unknown".to_string();
    match &c.origin {
        Origin::Installed {
            source,
            url,
            git_ref,
            commit,
            installed_at,
        } => {
            rows.push((
                "installed from",
                source
                    .clone()
                    .unwrap_or_else(|| "unknown (installed before pastor recorded it)".into()),
            ));
            rows.push(("git url", url.clone().unwrap_or_else(unknown)));
            let asked = match (git_ref, source) {
                (Some(r), _) => one_line(r),
                (None, Some(_)) => "- (the default branch)".into(),
                (None, None) => unknown(),
            };
            rows.push(("ref", asked));
            rows.push(("commit", commit.clone().unwrap_or_else(unknown)));
            rows.push((
                "installed at",
                installed_at.map_or_else(unknown, |at| ago(Some(at))),
            ));
        }
        Origin::Linked { path, exists } => {
            let gone = if *exists { "" } else { " (missing)" };
            rows.push((
                "linked to",
                format!("{}{gone}", one_line(&path.display().to_string())),
            ));
        }
    }
    rows.push(("env file", one_line(&c.env_file)));
    let mut out = fields(&rows);
    let command = c
        .connector
        .as_ref()
        .map(|k| {
            vec![
                format!("mode: {}", k.mode),
                format!("command: {}", argv(&k.command)),
                format!("timeout: {}s", k.timeout_secs),
            ]
        })
        .unwrap_or_default();
    section(&mut out, "connector", command);
    let config = c
        .connector
        .iter()
        .flat_map(|k| &k.config)
        .map(|k| {
            let req = if k.required { " (required)" } else { "" };
            format!("{}{req}: {}", one_line(&k.key), text(&k.description))
        })
        .collect();
    section(&mut out, "config", config);
    let hooks = c
        .hooks
        .iter()
        .map(|h| {
            let whose = if h.only_own {
                "own jobs only"
            } else {
                "every job's tasks"
            };
            format!(
                "on {} ({whose}): {} [timeout {}s]",
                one_line(&h.on.join(", ")),
                argv(&h.command),
                h.timeout_secs
            )
        })
        .collect();
    section(&mut out, "hooks", hooks);
    let finish = c
        .finish
        .iter()
        .map(|f| format!("{} [timeout {}s]", argv(&f.command), f.timeout_secs))
        .collect();
    section(&mut out, "finish", finish);
    let watch = c
        .watch
        .iter()
        .map(|w| format!("{} [timeout {}s]", argv(&w.command), w.timeout_secs))
        .collect();
    section(&mut out, "watch", watch);
    let secrets = c
        .secrets
        .iter()
        .map(|s| {
            let set = if s.set { "set" } else { "missing" };
            format!("{} ({set}): {}", s.name, text(&s.description))
        })
        .collect();
    section(&mut out, "secrets", secrets);
    let jobs = if c.jobs.is_empty() {
        vec![]
    } else {
        table(&JOB_HEADER, &job_rows(&c.jobs))
            .lines()
            .map(str::to_string)
            .collect()
    };
    section(&mut out, "jobs", jobs);
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_line_up_and_sections_say_none() {
        let mut out = fields(&[("a", "1".into()), ("longer", "2".into())]);
        assert_eq!(out, ["a:      1", "longer: 2"]);
        section(&mut out, "tasks", vec![]);
        section(&mut out, "events", vec!["x".into()]);
        assert_eq!(out[2..], ["tasks: none", "events:", "  x"]);
    }

    /// `machine describe` of an `ssh` machine whose agents cannot reach
    /// the head says so on its error line.
    #[test]
    fn a_machine_cut_off_from_the_head_says_so() {
        let row: crate::cli::MachineRow = serde_json::from_value(serde_json::json!({
            "name": "pi", "host": "user@pi", "endpoint": "ssh user@pi",
            "flock": "default", "channel": "connected", "herdr_version": null,
            "pastor_version": null, "protocol": null, "error": null, "live": 0,
            "max_agents": 1, "tags": [], "orphans": [], "no_head_address": true,
        }))
        .unwrap();
        let d = MachineDescription {
            row,
            session: "default".into(),
            model: None,
            fallback: None,
            agents_by_kind: Default::default(),
            tasks: vec![],
            recent_errors: vec![],
            config: None,
            workdirs: None,
            signed_out: vec![],
            bootstrap: None,
        };
        let text = machine_text(&d);
        let error = text.lines().find(|l| l.starts_with("error:")).unwrap();
        assert!(error.ends_with(crate::cli::CUT_OFF), "{text}");
    }

    #[test]
    fn toml_lines_flatten_nested_tables() {
        let v = serde_json::json!({"use": "gh", "query": {"label": "bug", "n": 3}});
        let mut out = vec![];
        toml_lines(&v, "", &mut out);
        assert_eq!(
            out,
            ["query.label = \"bug\"", "query.n = 3", "use = \"gh\""]
        );
    }

    #[test]
    fn a_flock_without_its_own_agent_says_where_it_comes_from() {
        let f = FlockDescription {
            description: None,
            name: "work".into(),
            default: false,
            agent: None,
            agent_args: Some(vec!["--model".into(), "a b".into()]),
            allow: vec![],
            deny: vec!["Bash(rm:*)".into()],
            model: Some("sonnet".into()),
            fallback: None,
            agents_by_kind: Default::default(),
            profile: Some("develop".into()),
            timeout: None,
            place: Some(crate::task::Place::Pastor),
            machines: vec!["pi-1".into(), "pi-2".into()],
            agents: None,
            tasks: vec![],
        };
        let text = flock_text(&f);
        assert!(text.contains("agent:       - (from [defaults])"), "{text}");
        assert!(text.contains("timeout:     - (from [defaults])"), "{text}");
        assert!(text.contains("place:       pastor\n"), "{text}");
        assert!(text.contains("by kind:     - (from [defaults])"), "{text}");
        assert!(
            text.contains("fallback:    - (from [defaults])\n"),
            "{text}"
        );
        let text = flock_text(&FlockDescription {
            fallback: Some(vec!["sonnet".into(), "gpt".into()]),
            ..f.clone()
        });
        assert!(text.contains("fallback:    sonnet, gpt\n"), "{text}");
        let text = flock_text(&FlockDescription {
            fallback: Some(vec![]),
            ..f.clone()
        });
        assert!(text.contains("fallback:    none\n"), "{text}");
        let with = FlockDescription {
            agents_by_kind: [("opencode".to_string(), "opencode".to_string())].into(),
            ..f.clone()
        };
        let text = flock_text(&with);
        assert!(text.contains("by kind:     opencode=opencode\n"), "{text}");
        assert!(text.contains("--model 'a b'"), "{text}");
        assert!(text.contains("model:       sonnet"), "{text}");
        assert!(text.contains("profile:     develop"), "{text}");
        assert!(text.contains("pi-1,pi-2"), "{text}");
        assert!(text.ends_with("tasks: none"), "{text}");
        assert!(text.contains("\ndescription: -\n"), "{text}");
        let described = flock_text(&FlockDescription {
            description: Some("Paid work".into()),
            ..f
        });
        assert!(
            described.starts_with("name:        work\ndescription: Paid work\n"),
            "{described}"
        );
    }

    #[test]
    fn recent_events_keeps_the_last_ones_in_order() {
        let ev = |kind: &str| EventRecord {
            summary: None,
            seq: 0,
            at: Utc::now(),
            kind: kind.into(),
            task: None,
            job: None,
            machine: None,
            detail: None,
            model: None,
        };
        let all: Vec<EventRecord> = (0..15).map(|i| ev(&format!("job.{i}"))).collect();
        let kept = recent_events(all, |e| e.kind != "job.14");
        assert_eq!(kept.len(), RECENT);
        assert_eq!(kept[0].kind, "job.4");
        assert_eq!(kept[RECENT - 1].kind, "job.13");
    }

    /// `machine describe` has a `bootstrap` line for a machine with a state:
    /// ok says its mode and isolation, a failure its step, why and fix; a
    /// machine with none has no such line.
    #[test]
    fn describe_shows_bootstrap_line() {
        use crate::store::{BootstrapState, MachineState};
        let state = |bootstrap, step: Option<&str>, why: Option<&str>| MachineState {
            machine: "pi-3".into(),
            bootstrap,
            isolation: Some("none".into()),
            failed_step: step.map(str::to_string),
            why: why.map(str::to_string),
            host_key: None,
            herdr_version: None,
            pastor_version: None,
            generation: 1,
            updated_at: chrono::Utc::now(),
        };
        assert_eq!(
            bootstrap_line(&state(BootstrapState::Ok, None, None)),
            "ok (user mode; isolation: none)"
        );
        assert_eq!(
            bootstrap_line(&state(BootstrapState::Pending, None, None)),
            "pending"
        );
        let failed = state(
            BootstrapState::Failed,
            Some("herdr"),
            Some("herdr is not on the PATH; fix: pastor machine install pi-3 --kind herdr"),
        );
        assert_eq!(
            bootstrap_line(&failed),
            "failed at herdr: herdr is not on the PATH; fix: pastor machine install pi-3 --kind herdr"
        );

        let row: crate::cli::MachineRow = serde_json::from_value(serde_json::json!({
            "name": "pi-3", "host": "user@pi-3", "endpoint": "ssh user@pi-3",
            "flock": "default", "channel": "connected", "herdr_version": null,
            "pastor_version": null, "protocol": null, "error": null, "live": 0,
            "max_agents": 1, "tags": [], "orphans": [],
        }))
        .unwrap();
        let describe = |bootstrap| MachineDescription {
            row: row.clone(),
            session: "default".into(),
            model: None,
            fallback: None,
            agents_by_kind: Default::default(),
            tasks: vec![],
            recent_errors: vec![],
            config: None,
            workdirs: None,
            signed_out: vec![],
            bootstrap,
        };
        let text = machine_text(&describe(Some(failed)));
        assert!(
            text.lines().any(|l| l.starts_with("bootstrap:")
                && l.ends_with("failed at herdr: herdr is not on the PATH; fix: pastor machine install pi-3 --kind herdr")),
            "{text}"
        );
        assert!(!machine_text(&describe(None)).contains("bootstrap:"));
        // An older head's JSON has no bootstrap.
        let mut json = serde_json::to_value(describe(None)).unwrap();
        json.as_object_mut().unwrap().remove("bootstrap");
        let back: MachineDescription = serde_json::from_value(json).unwrap();
        assert!(back.bootstrap.is_none());
    }

    /// A cordoned machine says since when and why; any other has no line.
    #[test]
    fn describe_shows_a_cordon() {
        let row = |cordon: serde_json::Value| -> crate::cli::MachineRow {
            serde_json::from_value(serde_json::json!({
                "name": "pi", "host": "user@pi", "endpoint": "ssh user@pi",
                "flock": "default", "channel": "connected", "herdr_version": null,
                "pastor_version": null, "protocol": null, "error": null, "live": 1,
                "max_agents": 3, "tags": [], "orphans": [], "cordon": cordon,
            }))
            .unwrap()
        };
        let text = |cordon| {
            machine_text(&MachineDescription {
                row: row(cordon),
                session: "default".into(),
                model: None,
                fallback: None,
                agents_by_kind: Default::default(),
                tasks: vec![],
                recent_errors: vec![],
                config: None,
                workdirs: None,
                signed_out: vec![],
                bootstrap: None,
            })
        };
        let cordoned = text(serde_json::json!({
            "machine": "pi", "reason": "kernel update", "since": "2026-10-31T10:00:00Z",
        }));
        assert!(
            cordoned.lines().any(|l| l.starts_with("cordoned:")
                && l.ends_with("since 2026-10-31 10:00: kernel update")),
            "{cordoned}"
        );
        let bare = text(serde_json::json!({
            "machine": "pi", "reason": null, "since": "2026-10-31T10:00:00Z",
        }));
        assert!(bare.contains("since 2026-10-31 10:00\n"), "{bare}");
        assert!(!text(serde_json::Value::Null).contains("cordoned"));
    }

    /// A machine with slots reserved says so on its agents line.
    #[test]
    fn describe_says_how_many_slots_are_reserved() {
        let row = |reserved: u32| -> crate::cli::MachineRow {
            serde_json::from_value(serde_json::json!({
                "name": "pi", "host": "user@pi", "endpoint": "ssh user@pi",
                "flock": "default", "channel": "connected", "herdr_version": null,
                "pastor_version": null, "protocol": null, "error": null, "live": 1,
                "max_agents": 3, "reserved": reserved, "tags": [], "orphans": [],
            }))
            .unwrap()
        };
        let text = |reserved| {
            machine_text(&MachineDescription {
                row: row(reserved),
                session: "default".into(),
                model: None,
                fallback: None,
                agents_by_kind: Default::default(),
                tasks: vec![],
                recent_errors: vec![],
                config: None,
                workdirs: None,
                signed_out: vec![],
                bootstrap: None,
            })
        };
        assert!(
            text(1)
                .lines()
                .any(|l| l.starts_with("agents:")
                    && l.ends_with("1 of 3, 1 reserved for other panes")),
            "{}",
            text(1)
        );
        assert!(text(0).contains("agents:       1 of 3\n"), "{}", text(0));
    }
}
