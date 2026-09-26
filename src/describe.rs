//! `pastor job|machine|flock|connector describe`: one thing in full, for a human, or
//! as JSON with `--json`. The CLI gathers the parts (from the head when one
//! runs, else from the files and the store); this module holds their shape
//! and how they read.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::cli::{
    JOB_HEADER, MachineRow, TASK_HEADER, age, in_, job_rows, one_line, table, task_rows,
};
use crate::connector::install::Origin;
use crate::events::EventRecord;
use crate::herdr::shell_quote;
use crate::scheduler::JobStatus;
use crate::task::Task;

/// How many recent tasks and events a description lists.
pub const RECENT: usize = 10;

#[derive(Debug, Clone, Serialize)]
pub struct JobDescription {
    pub name: String,
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

#[derive(Debug, Clone, Serialize)]
pub struct MachineDescription {
    #[serde(flatten)]
    pub row: MachineRow,
    pub session: String,
    /// The tasks whose agent holds a pane on it, newest first.
    pub tasks: Vec<Task>,
    /// Its recent `machine.*` events that carried an error, and its failed
    /// tasks, oldest first.
    pub recent_errors: Vec<EventRecord>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FlockDescription {
    pub name: String,
    pub default: bool,
    /// The flock's own agent settings; `None` falls through to `[defaults]`.
    pub agent: Option<String>,
    pub agent_args: Option<Vec<String>>,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
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

/// `key: value` lines with the values in one column.
fn fields(rows: &[(&str, String)]) -> Vec<String> {
    let width = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0) + 2;
    rows.iter()
        .map(|(k, v)| format!("{:<width$}{v}", format!("{k}:")))
        .collect()
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

pub fn machine_text(m: &MachineDescription) -> String {
    let r = &m.row;
    let agents = match r.live {
        Some(n) => format!("{n} of {}", r.max_agents),
        None => format!("- of {}", r.max_agents),
    };
    let mut out = fields(&[
        ("name", r.name.clone()),
        ("host", r.host.clone()),
        ("endpoint", r.endpoint.clone()),
        ("flock", r.flock.clone()),
        ("session", m.session.clone()),
        ("channel", r.channel.clone()),
        ("herdr", dash(r.herdr_version.clone())),
        ("protocol", dash(r.protocol.map(|p| p.to_string()))),
        ("pastor", dash(r.pastor_version.clone())),
        ("agents", agents),
        ("orphans", dash(Some(r.orphans.join(",")))),
        ("tags", dash(Some(r.tags.join(",")))),
        ("error", dash(r.error.clone().map(|e| one_line(&e)))),
    ]);
    section(&mut out, "tasks", task_table(&m.tasks));
    section(
        &mut out,
        "recent errors",
        m.recent_errors.iter().map(EventRecord::line).collect(),
    );
    out.join("\n")
}

pub fn flock_text(f: &FlockDescription) -> String {
    let mut out = fields(&[
        ("name", f.name.clone()),
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
        ("description", text(&c.description)),
        ("authors", dash(Some(one_line(&c.authors.join(", "))))),
        ("homepage", text(&c.homepage)),
        ("repository", text(&c.repository)),
        ("license", text(&c.license)),
        ("status", status),
        ("dir", c.dir.clone()),
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
            rows.push(("linked to", format!("{}{gone}", path.display())));
        }
    }
    rows.push(("env file", c.env_file.clone()));
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
            name: "work".into(),
            default: false,
            agent: None,
            agent_args: Some(vec!["--model".into(), "a b".into()]),
            allow: vec![],
            deny: vec!["Bash(rm:*)".into()],
            machines: vec!["pi-1".into(), "pi-2".into()],
            agents: None,
            tasks: vec![],
        };
        let text = flock_text(&f);
        assert!(text.contains("agent:      - (from [defaults])"), "{text}");
        assert!(text.contains("--model 'a b'"), "{text}");
        assert!(text.contains("pi-1,pi-2"), "{text}");
        assert!(text.ends_with("tasks: none"), "{text}");
    }

    #[test]
    fn recent_events_keeps_the_last_ones_in_order() {
        let ev = |kind: &str| EventRecord {
            at: Utc::now(),
            kind: kind.into(),
            task: None,
            job: None,
            machine: None,
            detail: None,
        };
        let all: Vec<EventRecord> = (0..15).map(|i| ev(&format!("job.{i}"))).collect();
        let kept = recent_events(all, |e| e.kind != "job.14");
        assert_eq!(kept.len(), RECENT);
        assert_eq!(kept[0].kind, "job.4");
        assert_eq!(kept[RECENT - 1].kind, "job.13");
    }
}
