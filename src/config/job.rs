//! One job per TOML file in `~/.config/pastor/jobs/`. `JobFile` is the file's
//! shape and nothing else; `Job` is what survives validation and is what the
//! scheduler runs. Validation happens here, at load, so `job list` can show a
//! broken file as `invalid` with its reason instead of a run failing later.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;
use serde_json::Value;

use crate::config::{AgentChoice, Defaults, check_tools, parse_duration};
use crate::connector::Catalog;
use crate::schedule::Schedule;
use crate::task::{DispatchSpec, Place, Priority};
use crate::template;

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobFile {
    pub name: Option<String>,
    /// One line on what the job does, for `job list --wide` and `describe`.
    pub description: Option<String>,
    pub every: Option<String>,
    pub cron: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub connector: ConnectorTable,
    pub dispatch: DispatchTable,
}

/// `use` names the connector; every other key is passed to it as config.
#[derive(Debug, Deserialize)]
pub struct ConnectorTable {
    #[serde(rename = "use")]
    pub use_: String,
    #[serde(flatten)]
    pub config: toml::Table,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DispatchTable {
    pub agent: Option<String>,
    /// `None` (no key) takes `[defaults] agent_args`; `[]` means none.
    pub agent_args: Option<Vec<String>>,
    /// Added to the flock's and `[defaults]` allow list.
    pub allow: Vec<String>,
    /// Added to the flock's and `[defaults]` deny list.
    pub deny: Vec<String>,
    /// A `[models]` name, or a template of one (`{{ item.model }}`) rendered
    /// per item; rendered empty, the flock's, machine's or `[defaults]` model.
    pub model: Option<String>,
    /// `[models]` names the job's tasks may fall back to, each a name or a
    /// template of one rendered per item; entries that render empty are
    /// dropped. Before the machine's, the flock's and `[defaults]`; `[]`
    /// means none.
    pub fallback: Option<Vec<String>>,
    /// A level (`low`, `normal`, `high`, `critical`), or a template of one
    /// (`{{ item.priority }}`) rendered per item; rendered empty, the flock's,
    /// pinned machine's or `[defaults]` level.
    pub priority: Option<String>,
    /// Let the job's critical tasks pause a `low` Claude task on a full
    /// machine to start (see `Task::pause`); tasks it queues below critical
    /// ignore it.
    pub preempt: bool,
    /// Whether the job's tasks are asked for a summary, or need one
    /// (`SummaryMode`), before the flock's and `[defaults]`.
    pub summary: Option<crate::task::SummaryMode>,
    /// A permission profile, built in or in `[profiles]`, before the
    /// machine's, the flock's and `[defaults]`.
    pub profile: Option<String>,
    pub repo: Option<String>,
    pub worktree: bool,
    pub branch: Option<String>,
    pub tags: Vec<String>,
    pub machine: Option<String>,
    /// Where the job's tasks go; `None`: the flock of `machine`, else the
    /// default flock.
    pub flock: Option<String>,
    pub timeout: Option<String>,
    /// Where the job's tasks' panes go; `None` takes `[defaults] place`.
    pub place: Option<Place>,
    /// The label template of each task's workspace; `None` takes the
    /// flock's, else `[defaults] label` (`Defaults::resolve_label`).
    pub label: Option<String>,
    pub max_tasks_per_run: Option<u32>,
    pub backfill: Option<String>,
    /// The template of each task's description; `None` is
    /// `DEFAULT_TASK_DESCRIPTION`.
    pub description: Option<String>,
    pub prompt: String,
}

/// A job task's description when `[dispatch]` names none: its item's title,
/// so a board card's task reads as the card.
pub const DEFAULT_TASK_DESCRIPTION: &str = "{{ item.title }}";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub name: String,
    /// The file's `description`, trimmed; `None` when it has none.
    pub description: Option<String>,
    /// `[dispatch] description`, unrendered: `task_description_for` renders
    /// it for one item.
    pub task_description: Option<String>,
    pub schedule: Schedule,
    pub enabled: bool,
    pub connector: String,
    /// The `[connector]` table minus `use`, as JSON for the connector's stdin.
    pub connector_config: Value,
    /// Unrendered; `{{ item.* }}`, `{{ job.name }}`, `{{ task.id }}` allowed.
    pub prompt: String,
    pub max_tasks_per_run: u32,
    pub backfill: Duration,
    /// `repo` and `branch` are unrendered templates too; the scheduler renders
    /// a copy per task. Its agent is the job's own, else `[defaults]`: the
    /// flock's is only known when a task is queued, which re-resolves it
    /// from `agent` (`Fleet::queue_job_task`).
    pub spec: DispatchSpec,
    /// What `[dispatch]` itself says about the agent. Its `model` and
    /// `fallback` are still templates: `model_for` and `fallback_for` render
    /// them for one item.
    pub agent: AgentChoice,
    /// `dispatch.flock`, checked against flock.toml at each run (see
    /// `Flock::task_flock`): the job file does not know the flocks.
    pub flock: Option<String>,
    /// `dispatch.priority`, still a template: `priority_for` renders it for
    /// one item.
    pub priority: Option<String>,
    /// `dispatch.preempt`: its tasks that settle at critical get `preempt`.
    pub preempt: bool,
    /// `dispatch.summary`: before the flock's and `[defaults]`
    /// (`Defaults::resolve_summary`).
    pub summary: Option<crate::task::SummaryMode>,
    /// The `[dispatch]` table as written, as JSON and without `prompt`: what
    /// a headless serve sends the head with its items (`IpcRequest::
    /// JobSubmit`), so the head applies its own `[defaults]` to what the
    /// file leaves out. `Null` for a job the head built from a `JobTask`.
    pub dispatch: Value,
}

impl Job {
    /// Parse and validate one file's text. `stem` is the file name without
    /// `.toml`: it is the job's name, and a `name` key must agree with it.
    /// `catalog` decides whether `connector.use` exists and its config suits it.
    pub fn parse(
        text: &str,
        stem: &str,
        defaults: &Defaults,
        catalog: &dyn Catalog,
    ) -> Result<Job, String> {
        let file: JobFile = toml::from_str(text).map_err(|e| e.to_string())?;
        let raw: toml::Table = toml::from_str(text).map_err(|e| e.to_string())?;
        let dispatch = raw
            .get("dispatch")
            .map(serde_json::to_value)
            .transpose()
            .map_err(|e| e.to_string())?
            .unwrap_or(Value::Null);
        let name = file.name.clone().unwrap_or_else(|| stem.to_string());
        if name != stem {
            return Err(format!(
                "name {name:?} does not match the file name {stem:?}"
            ));
        }
        check_name(&name)?;
        let schedule = Schedule::from_fields(file.every.as_deref(), file.cron.as_deref())?;
        if file.connector.use_.is_empty() {
            return Err("connector.use is required".into());
        }
        let connector_config =
            serde_json::to_value(&file.connector.config).map_err(|e| e.to_string())?;
        catalog.check(&file.connector.use_, &connector_config)?;
        Job::from_dispatch(
            name,
            schedule,
            file.enabled,
            file.connector.use_,
            connector_config,
            file.dispatch,
            defaults,
        )
        .map(|job| Job {
            dispatch: Value::Object(table_without_prompt(&dispatch)),
            description: crate::config::clean_description(file.description.as_deref()),
            ..job
        })
    }

    /// A job another machine runs and submits items for (`IpcRequest::
    /// JobSubmit`): its `[dispatch]` table as JSON, with `prompt` beside it
    /// (it wins over a `prompt` inside the table). Checked exactly as a job
    /// file's `[dispatch]` is. It has no schedule or connector on the head:
    /// those stay on the submitter, and nothing here reads them.
    pub fn submitted(
        name: &str,
        dispatch: &Value,
        prompt: &str,
        defaults: &Defaults,
    ) -> Result<Job, String> {
        check_name(name)?;
        let mut table = match dispatch {
            Value::Object(m) => m.clone(),
            Value::Null => serde_json::Map::new(),
            _ => return Err("dispatch must be a table".into()),
        };
        table.insert("prompt".into(), Value::String(prompt.to_string()));
        let d: DispatchTable =
            serde_json::from_value(Value::Object(table)).map_err(|e| e.to_string())?;
        Job::from_dispatch(
            name.to_string(),
            Schedule::Every(Duration::MAX),
            true,
            String::new(),
            Value::Null,
            d,
            defaults,
        )
        .map(|job| Job {
            dispatch: Value::Object(table_without_prompt(dispatch)),
            ..job
        })
    }

    /// The `[dispatch]` checks and defaults, shared by a job file and a
    /// submitted job.
    fn from_dispatch(
        name: String,
        schedule: Schedule,
        enabled: bool,
        connector: String,
        connector_config: Value,
        d: DispatchTable,
        defaults: &Defaults,
    ) -> Result<Job, String> {
        if d.prompt.trim().is_empty() {
            return Err("dispatch.prompt is required".into());
        }
        if d.worktree && d.repo.is_none() {
            return Err("dispatch.worktree = true needs dispatch.repo".into());
        }
        if let Some(text) = &d.description {
            for path in
                template::placeholders(text).map_err(|e| format!("dispatch.description: {e}"))?
            {
                if !(path.starts_with("item.") || path == "job.name") {
                    return Err(format!(
                        "dispatch.description: unknown placeholder {{{{ {path} }}}}; use item.* or job.name"
                    ));
                }
            }
        }
        if let Some(model) = &d.model {
            for path in template::placeholders(model).map_err(|e| format!("dispatch.model: {e}"))? {
                if !(path.starts_with("item.") || path == "job.name") {
                    return Err(format!(
                        "dispatch.model: unknown placeholder {{{{ {path} }}}}; use item.* or job.name"
                    ));
                }
            }
            if !model.contains("{{") {
                crate::config::check_model_name(model)
                    .map_err(|e| format!("dispatch.model: {e}"))?;
            }
        }
        for entry in d.fallback.iter().flatten() {
            for path in
                template::placeholders(entry).map_err(|e| format!("dispatch.fallback: {e}"))?
            {
                if !(path.starts_with("item.") || path == "job.name") {
                    return Err(format!(
                        "dispatch.fallback: unknown placeholder {{{{ {path} }}}}; use item.* or job.name"
                    ));
                }
            }
            if !entry.contains("{{") {
                crate::config::check_model_name(entry)
                    .map_err(|e| format!("dispatch.fallback: {e}"))?;
            }
        }
        if let Some(priority) = &d.priority {
            for path in
                template::placeholders(priority).map_err(|e| format!("dispatch.priority: {e}"))?
            {
                if !(path.starts_with("item.") || path == "job.name") {
                    return Err(format!(
                        "dispatch.priority: unknown placeholder {{{{ {path} }}}}; use item.* or job.name"
                    ));
                }
            }
            if !priority.contains("{{") && !priority.trim().is_empty() {
                let level = priority
                    .trim()
                    .parse::<Priority>()
                    .map_err(|e| format!("dispatch.priority: {e}"))?;
                if d.preempt && level != Priority::Critical {
                    return Err(format!(
                        "dispatch.preempt: only a critical task may pause another, and priority is {level} (preempt_needs_critical)"
                    ));
                }
            }
        }
        if let Some(profile) = &d.profile {
            crate::config::check_profile_name(profile)
                .map_err(|e| format!("dispatch.profile: {e}"))?;
        }
        if let Some(label) = &d.label {
            crate::task::check_label(label).map_err(|e| format!("dispatch.{e}"))?;
        }
        for (field, text) in [
            ("prompt", Some(d.prompt.as_str())),
            ("branch", d.branch.as_deref()),
            ("repo", d.repo.as_deref()),
        ] {
            let Some(text) = text else { continue };
            for path in
                template::placeholders(text).map_err(|e| format!("dispatch.{field}: {e}"))?
            {
                let known = path.starts_with("item.") || path == "job.name" || path == "task.id";
                if !known {
                    return Err(format!(
                        "dispatch.{field}: unknown placeholder {{{{ {path} }}}}; use item.*, job.name or task.id"
                    ));
                }
            }
        }
        // An item must not name an existing branch such as `main` and have the
        // agent commit to it. The job fixes the first component of the branch
        // (`pastor/...`), so items only choose names inside that namespace.
        if let Some(branch) = d.branch.as_deref() {
            let first = branch.split('/').next().unwrap_or_default();
            let from_item = template::placeholders(first)
                .map_err(|e| format!("dispatch.branch: {e}"))?
                .iter()
                .any(|p| p.starts_with("item.") || p == "item");
            if from_item {
                return Err(format!(
                    "dispatch.branch: {branch:?} puts an item value in its first component; \
                     start it with a fixed prefix, like \"pastor/{{{{ item.key }}}}\""
                ));
            }
        }
        let timeout = match d.timeout.as_deref() {
            Some(t) => parse_duration(t).map_err(|e| format!("dispatch.timeout: {e}"))?,
            None => {
                parse_duration(&defaults.timeout).map_err(|e| format!("defaults.timeout: {e}"))?
            }
        };
        let backfill = match d.backfill.as_deref() {
            Some(b) => parse_duration(b).map_err(|e| format!("dispatch.backfill: {e}"))?,
            None => Duration::ZERO,
        };
        let max_tasks_per_run = d.max_tasks_per_run.unwrap_or(defaults.max_tasks_per_run);
        if max_tasks_per_run == 0 {
            return Err("dispatch.max_tasks_per_run must be at least 1".into());
        }
        check_tools("dispatch.allow", &d.allow)?;
        check_tools("dispatch.deny", &d.deny)?;
        let agent = AgentChoice {
            agent: d.agent,
            agent_args: d.agent_args,
            allow: d.allow,
            deny: d.deny,
            model: d.model,
            fallback: d.fallback,
            profile: d.profile,
            timeout_secs: d.timeout.is_some().then_some(timeout.as_secs()),
            place: d.place.clone(),
        };
        let pick = defaults.resolve_agent(&agent, None);
        Ok(Job {
            name,
            description: None,
            task_description: d.description,
            schedule,
            enabled,
            connector,
            connector_config,
            prompt: d.prompt,
            max_tasks_per_run,
            backfill,
            flock: d.flock,
            priority: d.priority,
            preempt: d.preempt,
            summary: d.summary,
            agent,
            dispatch: Value::Null,
            spec: DispatchSpec {
                agent: pick.agent,
                agent_args: pick.agent_args,
                allow: pick.allow,
                deny: pick.deny,
                repo: d.repo,
                worktree: d.worktree,
                branch: d.branch,
                machine: d.machine,
                tags: d.tags,
                timeout_secs: timeout.as_secs(),
                checkout: None,
                reopen: None,
                agent_source: None,
                place: d.place.unwrap_or_else(|| defaults.place.clone()),
                session_id: None,
                label: crate::task::WorkspaceLabel {
                    template: d.label,
                    ..Default::default()
                },
                summary: Default::default(),
                cwd: None,
            },
        })
    }

    /// The job's `priority` rendered for `item`: `None` when the job sets
    /// none, or its template renders empty, so the flock's, pinned machine's
    /// or `[defaults]` level applies. A rendered value that is not a level
    /// is refused (`unknown_priority`), and the item with it.
    pub fn priority_for(&self, item: &Value) -> Result<Option<Priority>, String> {
        let Some(priority) = &self.priority else {
            return Ok(None);
        };
        let ctx = serde_json::json!({"item": item, "job": {"name": self.name}});
        let text = template::render(priority, &ctx)
            .map_err(|e| format!("dispatch.priority: {e}"))?
            .text;
        let text = text.trim();
        if text.is_empty() {
            return Ok(None);
        }
        text.parse()
            .map(Some)
            .map_err(|e| format!("dispatch.priority: {} ({e})", crate::task::UNKNOWN_PRIORITY))
    }

    /// The description of this job's task for `item`: `[dispatch]
    /// description` rendered, else its item's title, trimmed. `None` when
    /// that is empty, or a path the item lacks leaves it so: the task then
    /// reads as its prompt's first line.
    pub fn task_description_for(&self, item: &Value) -> Option<String> {
        let text = self
            .task_description
            .as_deref()
            .unwrap_or(DEFAULT_TASK_DESCRIPTION);
        let ctx = serde_json::json!({"item": item, "job": {"name": self.name}});
        let rendered = template::render(text, &ctx).ok()?.text;
        crate::config::clean_description(Some(&rendered))
    }

    /// The job's `model` rendered for `item`: `None` when the job names
    /// none, or its template renders empty, so the flock's, machine's or
    /// `[defaults]` model applies. A rendered value that is not a model name
    /// is refused; whether `[models]` has it is checked when the task is
    /// queued.
    pub fn model_for(&self, item: &Value) -> Result<Option<String>, String> {
        let Some(model) = &self.agent.model else {
            return Ok(None);
        };
        let ctx = serde_json::json!({"item": item, "job": {"name": self.name}});
        let text = template::render(model, &ctx)
            .map_err(|e| format!("dispatch.model: {e}"))?
            .text;
        let text = text.trim();
        if text.is_empty() {
            return Ok(None);
        }
        crate::config::check_model_name(text).map_err(|e| format!("dispatch.model: {e}"))?;
        Ok(Some(text.to_string()))
    }

    /// The job's `fallback` rendered for `item`: `None` when the job sets
    /// none, so the machine's, flock's or `[defaults]` list applies. Entries
    /// that render empty are dropped, and the list stays the job's even when
    /// that leaves it empty: the task then has none. A rendered entry that
    /// is not a model name is refused; whether `[models]` has it is checked
    /// when the task is queued.
    pub fn fallback_for(&self, item: &Value) -> Result<Option<Vec<String>>, String> {
        let Some(entries) = &self.agent.fallback else {
            return Ok(None);
        };
        let ctx = serde_json::json!({"item": item, "job": {"name": self.name}});
        let mut out = Vec::new();
        for entry in entries {
            let text = template::render(entry, &ctx)
                .map_err(|e| format!("dispatch.fallback: {e}"))?
                .text;
            let text = text.trim();
            if text.is_empty() {
                continue;
            }
            crate::config::check_model_name(text).map_err(|e| format!("dispatch.fallback: {e}"))?;
            out.push(text.to_string());
        }
        Ok(Some(out))
    }
}

/// Job names appear in `tasks.job`, in `{{ job.name }}` and, for a connector
/// connector, as a directory under the state dir, so they are kept to a safe alphabet. `run`
/// is what one-off tasks carry in `tasks.job`. Public so `job_path` callers
/// outside a full `Job::parse` (the CLI's `enable`/`disable`) can reject a
/// name before joining it under the jobs directory: an unvalidated name like
/// `"../pastor"` resolves outside it entirely.
/// A `[dispatch]` table with `prompt` removed, since it travels beside it.
fn table_without_prompt(table: &Value) -> serde_json::Map<String, Value> {
    let mut map = table.as_object().cloned().unwrap_or_default();
    map.remove("prompt");
    map
}

pub fn check_name(name: &str) -> Result<(), String> {
    let first_ok = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let rest_ok = name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_.-".contains(c));
    if !(first_ok && rest_ok && name.len() <= 64) {
        return Err(format!(
            "job name {name:?} must match [a-z0-9][a-z0-9_.-]{{0,63}}"
        ));
    }
    if name == "run" {
        return Err("job name \"run\" is reserved for one-off tasks".into());
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub enum Loaded {
    // Boxed: `Invalid`'s two `String`s would otherwise force every `Loaded`
    // (including the common `Invalid` case) to be sized for the much larger
    // `Job`.
    Valid(Box<Job>),
    Invalid { name: String, error: String },
}

impl Loaded {
    pub fn name(&self) -> &str {
        match self {
            Loaded::Valid(j) => &j.name,
            Loaded::Invalid { name, .. } => name,
        }
    }
}

pub fn job_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.toml"))
}

/// Every `*.toml` in `dir`, sorted by name, each valid or invalid with its
/// reason. A missing directory is simply no jobs.
pub fn load_dir(
    dir: &Path,
    defaults: &Defaults,
    catalog: &dyn Catalog,
) -> anyhow::Result<Vec<Loaded>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("read {}", dir.display())),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        out.push(load_file(&path, stem, defaults, catalog));
    }
    out.sort_by(|a, b| a.name().cmp(b.name()));
    Ok(out)
}

pub fn load_file(path: &Path, stem: &str, defaults: &Defaults, catalog: &dyn Catalog) -> Loaded {
    let parsed = std::fs::read_to_string(path)
        .map_err(|e| e.to_string())
        .and_then(|text| Job::parse(&text, stem, defaults, catalog));
    match parsed {
        Ok(job) => Loaded::Valid(Box::new(job)),
        Err(error) => Loaded::Invalid {
            name: stem.to_string(),
            error,
        },
    }
}

/// `pastor job enable|disable`: rewrite the top-level `enabled` line (or insert
/// one before the first table) and nothing else, so comments and layout the
/// user wrote survive. The result must still parse or the file is left alone.
pub fn set_enabled(path: &Path, enabled: bool) -> anyhow::Result<()> {
    // Resolve the real file first: `path` may be a symlink (e.g. into a
    // dotfiles repo), and writing the temp file next to `path` then renaming
    // over it would replace the link with a plain file. Writing beside, and
    // renaming onto, the canonical target keeps the link and edits what it
    // points to. The edit lock is held from the read to the rename so a
    // `put` or another toggle cannot be overwritten with stale text.
    let target =
        std::fs::canonicalize(path).with_context(|| format!("canonicalize {}", path.display()))?;
    let _lock = crate::edit::lock_file(&target)?;
    let text =
        std::fs::read_to_string(&target).with_context(|| format!("read {}", target.display()))?;
    let line = format!("enabled = {enabled}");
    let mut out: Vec<String> = Vec::new();
    let mut replaced = false;
    let mut top_level = true;
    for l in text.lines() {
        let t = l.trim_start();
        if t.starts_with('[') {
            top_level = false;
        }
        let is_enabled_key = t
            .strip_prefix("enabled")
            .is_some_and(|rest| rest.trim_start().starts_with('='));
        if top_level && !replaced && is_enabled_key {
            out.push(line.clone());
            replaced = true;
        } else {
            out.push(l.to_string());
        }
    }
    if !replaced {
        // Insert before whatever ends the top-level block first: a table
        // header, or the blank line conventionally left before one. Inserting
        // only before the header would land the new key after that blank
        // line, inside what reads as the table's own paragraph.
        let at = out
            .iter()
            .position(|l| l.trim().is_empty() || l.trim_start().starts_with('['))
            .unwrap_or(out.len());
        out.insert(at, line);
    }
    let mut new_text = out.join("\n");
    if text.ends_with('\n') || !text.is_empty() {
        new_text.push('\n');
    }
    // A syntax check only: full `JobFile` validation would reject unrelated
    // fields the user has every right to keep (e.g. a `dispatch` key pastor
    // does not know yet), which is not what "leave a broken edit alone" means.
    toml::from_str::<toml::Value>(&new_text)
        .with_context(|| format!("{} would not parse after the edit", path.display()))?;
    let tmp = target.with_extension("toml.tmp");
    std::fs::write(&tmp, &new_text).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &target).with_context(|| format!("rename to {}", target.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::Builtins;

    const SPEC_EXAMPLE: &str = r#"
name = "support-slack"
every = "5m"
enabled = true

[connector]
use = "clock"
channel = "C0123ABC"

[dispatch]
agent = "claude"
agent_args = []
repo = "~/work/support"
worktree = true
branch = "pastor/{{ item.key }}"
tags = ["fast"]
flock = "work"
timeout = "2h"
max_tasks_per_run = 5
backfill = "0s"
prompt = """
New message in #support from {{ item.author }}:

{{ item.text }}

Investigate, fix if it is a bug, and write your answer to REPLY.md.
"""
"#;

    fn defaults() -> Defaults {
        Defaults::default()
    }

    #[test]
    fn parses_the_spec_example() {
        let job = Job::parse(SPEC_EXAMPLE, "support-slack", &defaults(), &Builtins).unwrap();
        assert_eq!(job.name, "support-slack");
        assert_eq!(job.schedule, Schedule::Every(Duration::from_secs(300)));
        assert!(job.enabled);
        assert_eq!(job.connector, "clock");
        assert_eq!(job.connector_config["channel"], "C0123ABC");
        assert!(
            job.connector_config.get("use").is_none(),
            "use is not config"
        );
        assert_eq!(job.spec.agent, "claude");
        assert_eq!(job.spec.repo.as_deref(), Some("~/work/support"));
        assert!(job.spec.worktree);
        assert_eq!(job.spec.branch.as_deref(), Some("pastor/{{ item.key }}"));
        assert_eq!(job.spec.tags, vec!["fast"]);
        assert_eq!(job.flock.as_deref(), Some("work"));
        assert_eq!(job.spec.timeout_secs, 7200);
        assert_eq!(job.max_tasks_per_run, 5);
        assert_eq!(job.backfill, Duration::ZERO);
        assert!(job.prompt.contains("{{ item.author }}"));
    }

    /// A job's own `description` is plain text, trimmed; none is fine. Its
    /// `[dispatch] description` is rendered per item, `{{ item.title }}`
    /// when left out, and one that renders empty gives the task none.
    #[test]
    fn a_job_and_its_tasks_have_descriptions() {
        let old = Job::parse(SPEC_EXAMPLE, "support-slack", &defaults(), &Builtins).unwrap();
        assert_eq!(old.description, None);
        let item = serde_json::json!({"key": "k", "title": " Fix the login page \n", "n": 7});
        assert_eq!(
            old.task_description_for(&item).as_deref(),
            Some("Fix the login page")
        );
        assert_eq!(
            old.task_description_for(&serde_json::json!({"key": "k"})),
            None
        );

        let job = |top: &str, dispatch: &str| {
            Job::parse(
                &format!(
                    "{top}every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\n{dispatch}prompt = \"p\"\n"
                ),
                "j",
                &defaults(),
                &Builtins,
            )
        };
        let j = job(
            "description = \"  Carry out the answers  \"\n",
            "description = \"#{{ item.n }} for {{ job.name }}\"\n",
        )
        .unwrap();
        assert_eq!(j.description.as_deref(), Some("Carry out the answers"));
        assert_eq!(j.task_description_for(&item).as_deref(), Some("#7 for j"));
        let empty = job(
            "description = \"\"\n",
            "description = \"{{ item.nope }}\"\n",
        )
        .unwrap();
        assert_eq!(empty.description, None);
        assert_eq!(empty.task_description_for(&item), None);
        let err = job("", "description = \"{{ task.id }}\"\n").unwrap_err();
        assert!(err.contains("dispatch.description"), "{err}");
    }

    /// A job's `fallback` entries are templates rendered per item: those
    /// that render empty are dropped, the list stays the job's even when
    /// empty, and each rendered entry must be a model name.
    #[test]
    fn a_jobs_fallback_renders_per_item() {
        let job = |fallback: &str| {
            Job::parse(
                &format!(
                    "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nfallback = {fallback}\nprompt = \"p\"\n"
                ),
                "j",
                &defaults(),
                &Builtins,
            )
        };
        let j = job(r#"["{{ item.fallback }}", "gpt"]"#).unwrap();
        let item = |v: serde_json::Value| j.fallback_for(&v).unwrap();
        assert_eq!(
            item(serde_json::json!({"fallback": "sonnet"})),
            Some(vec!["sonnet".to_string(), "gpt".to_string()])
        );
        assert_eq!(item(serde_json::json!({})), Some(vec!["gpt".to_string()]));
        let only = job(r#"["{{ item.fallback }}"]"#).unwrap();
        assert_eq!(
            only.fallback_for(&serde_json::json!({})).unwrap(),
            Some(vec![])
        );
        let err = only
            .fallback_for(&serde_json::json!({"fallback": "--model x"}))
            .unwrap_err();
        assert!(err.contains("dispatch.fallback"), "{err}");
        let none = job("[]").unwrap();
        assert_eq!(
            none.fallback_for(&serde_json::json!({})).unwrap(),
            Some(vec![])
        );
        let plain = job(r#"["sonnet"]"#).unwrap();
        assert_eq!(
            plain.fallback_for(&serde_json::json!({})).unwrap(),
            Some(vec!["sonnet".to_string()])
        );
        let unset = Job::parse(
            "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"p\"\n",
            "j",
            &defaults(),
            &Builtins,
        )
        .unwrap();
        assert_eq!(unset.fallback_for(&serde_json::json!({})).unwrap(), None);
        assert!(
            job(r#"["{{ task.id }}"]"#)
                .unwrap_err()
                .contains("dispatch.fallback")
        );
        assert!(
            job(r#"["Sonnet"]"#)
                .unwrap_err()
                .contains("dispatch.fallback")
        );
    }

    /// A job's `model` is a template rendered per item: empty means the job
    /// names none, and what it renders must be a model name.
    #[test]
    fn a_jobs_model_renders_per_item() {
        let job = |model: &str| {
            Job::parse(
                &format!(
                    "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nmodel = {model:?}\nprompt = \"p\"\n"
                ),
                "j",
                &defaults(),
                &Builtins,
            )
        };
        let j = job("{{ item.model }}").unwrap();
        let item = |v: serde_json::Value| j.model_for(&v);
        assert_eq!(
            item(serde_json::json!({"model": "opus"}))
                .unwrap()
                .as_deref(),
            Some("opus")
        );
        assert_eq!(item(serde_json::json!({})).unwrap(), None);
        assert_eq!(item(serde_json::json!({"model": " "})).unwrap(), None);
        let err = item(serde_json::json!({"model": "--model x"})).unwrap_err();
        assert!(err.contains("dispatch.model"), "{err}");
        assert_eq!(
            job("sonnet")
                .unwrap()
                .model_for(&serde_json::json!({}))
                .unwrap()
                .as_deref(),
            Some("sonnet")
        );
        assert!(job("{{ task.id }}").unwrap_err().contains("dispatch.model"));
        assert!(job("Sonnet").unwrap_err().contains("dispatch.model"));
    }

    /// `summary` is read from `[dispatch]`; a job without it leaves it to
    /// the flock and `[defaults]`, and another word fails the file.
    #[test]
    fn a_jobs_summary_is_read_from_dispatch() {
        use crate::task::SummaryMode;
        let job = |extra: &str| {
            Job::parse(
                &format!(
                    "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\n{extra}\nprompt = \"p\"\n"
                ),
                "j",
                &Defaults::default(),
                &Builtins,
            )
        };
        assert_eq!(job("").unwrap().summary, None);
        assert_eq!(
            job("summary = \"require\"").unwrap().summary,
            Some(SummaryMode::Require)
        );
        assert_eq!(
            job("summary = \"off\"").unwrap().summary,
            Some(SummaryMode::Off)
        );
        assert!(job("summary = \"never\"").is_err());
    }

    /// `preempt` is read from `[dispatch]`, and refused beside a priority
    /// written below critical; a template decides per item.
    #[test]
    fn a_jobs_preempt_needs_critical() {
        let job = |extra: &str| {
            Job::parse(
                &format!(
                    "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\n{extra}\nprompt = \"p\"\n"
                ),
                "j",
                &Defaults::default(),
                &Builtins,
            )
        };
        assert!(!job("").unwrap().preempt);
        assert!(
            job("preempt = true\npriority = \"critical\"")
                .unwrap()
                .preempt
        );
        assert!(
            job("preempt = true\npriority = \"{{ item.level }}\"")
                .unwrap()
                .preempt
        );
        assert!(job("preempt = true").unwrap().preempt);
        let err = job("preempt = true\npriority = \"high\"").unwrap_err();
        assert!(err.contains("preempt_needs_critical"), "{err}");
    }

    /// A job's `priority` is a template rendered per item: empty falls
    /// through to the next layer, and what it renders must be a level.
    #[test]
    fn a_jobs_priority_renders_per_item() {
        let job = |priority: &str| {
            Job::parse(
                &format!(
                    "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\npriority = {priority:?}\nprompt = \"p\"\n"
                ),
                "j",
                &defaults(),
                &Builtins,
            )
        };
        let j = job("{{ item.priority }}").unwrap();
        let item = |v: serde_json::Value| j.priority_for(&v);
        assert_eq!(
            item(serde_json::json!({"priority": "critical"})).unwrap(),
            Some(Priority::Critical)
        );
        assert_eq!(item(serde_json::json!({})).unwrap(), None);
        assert_eq!(item(serde_json::json!({"priority": " "})).unwrap(), None);
        let err = item(serde_json::json!({"priority": "urgent"})).unwrap_err();
        assert!(err.contains("unknown_priority"), "{err}");
        assert!(err.contains("urgent"), "{err}");
        assert_eq!(
            job("high")
                .unwrap()
                .priority_for(&serde_json::json!({}))
                .unwrap(),
            Some(Priority::High)
        );
        assert_eq!(
            job("")
                .unwrap()
                .priority_for(&serde_json::json!({}))
                .unwrap(),
            None
        );
        assert!(
            job("{{ task.id }}")
                .unwrap_err()
                .contains("dispatch.priority")
        );
        assert!(job("urgent").unwrap_err().contains("dispatch.priority"));
    }

    /// A job's `profile` is a plain profile name, kept in its ask; whether
    /// pastor.toml has it is checked when a task is queued.
    #[test]
    fn a_jobs_profile_is_a_name() {
        let job = |profile: &str| {
            Job::parse(
                &format!(
                    "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nprofile = {profile:?}\nprompt = \"p\"\n"
                ),
                "j",
                &defaults(),
                &Builtins,
            )
        };
        assert_eq!(job("ci").unwrap().agent.profile.as_deref(), Some("ci"));
        for bad in ["Ci", "--permission-mode", "{{ item.profile }}"] {
            assert!(job(bad).unwrap_err().contains("dispatch.profile"), "{bad}");
        }
    }

    /// `[defaults] agent_args` fills in for a job file that has no
    /// `agent_args` key; a key that is there, even `[]`, is the job's choice.
    #[test]
    fn agent_args_fall_back_to_defaults_only_when_the_key_is_absent() {
        let d = Defaults {
            agent_args: vec!["--model".into(), "claude-opus-5-5".into()],
            ..Defaults::default()
        };
        let absent = SPEC_EXAMPLE.replace("agent_args = []\n", "");
        let job = Job::parse(&absent, "support-slack", &d, &Builtins).unwrap();
        assert_eq!(job.spec.agent_args, vec!["--model", "claude-opus-5-5"]);
        // The job's own word is kept apart, for the flock to fill in later.
        assert_eq!(job.agent.agent_args, None);

        let empty = Job::parse(SPEC_EXAMPLE, "support-slack", &d, &Builtins).unwrap();
        assert!(empty.spec.agent_args.is_empty(), "explicit [] opts out");
        assert_eq!(empty.agent.agent_args, Some(vec![]));

        let own = SPEC_EXAMPLE.replace("agent_args = []", "agent_args = [\"--model\", \"x\"]");
        let job = Job::parse(&own, "support-slack", &d, &Builtins).unwrap();
        assert_eq!(job.spec.agent_args, vec!["--model", "x"]);
    }

    #[test]
    fn a_job_can_add_tool_lists_and_bad_patterns_are_invalid() {
        let d = Defaults::default();
        let text = SPEC_EXAMPLE.replace(
            "agent_args = []\n",
            "agent_args = []\nallow = [\"Edit\"]\ndeny = [\"WebFetch\"]\n",
        );
        let job = Job::parse(&text, "support-slack", &d, &Builtins).unwrap();
        assert_eq!(job.agent.allow, vec!["Edit"]);
        assert_eq!(job.agent.deny, vec!["WebFetch"]);
        assert_eq!(job.spec.deny, vec!["WebFetch"]);
        let bad = SPEC_EXAMPLE.replace("agent_args = []\n", "agent_args = []\nallow = [\"\"]\n");
        let err = Job::parse(&bad, "support-slack", &d, &Builtins).unwrap_err();
        assert!(err.contains("dispatch.allow"), "{err}");
    }

    #[test]
    fn a_connector_the_catalog_lacks_is_invalid() {
        let text = SPEC_EXAMPLE.replace("use = \"clock\"", "use = \"slack\"");
        let err = Job::parse(&text, "support-slack", &defaults(), &Builtins).unwrap_err();
        assert!(err.contains("slack"), "{err}");
        assert!(err.contains("not available"), "{err}");
    }

    /// A catalog's reason (a connector that is missing, or a config key its
    /// manifest requires) is the job's `invalid` reason, and the config it
    /// checks is the table minus `use`.
    #[test]
    fn the_catalog_checks_the_connector_config() {
        struct NeedsChannel;
        impl Catalog for NeedsChannel {
            fn source(&self, _: &str) -> Option<std::sync::Arc<dyn crate::connector::ItemSource>> {
                None
            }
            fn check(&self, id: &str, config: &Value) -> Result<(), String> {
                assert!(config.get("use").is_none());
                match config.get("channel") {
                    Some(_) => Ok(()),
                    None => Err(format!("{id}: connector.channel is required")),
                }
            }
        }
        let text = SPEC_EXAMPLE.replace("use = \"clock\"", "use = \"slack\"");
        assert!(Job::parse(&text, "support-slack", &defaults(), &NeedsChannel).is_ok());
        let text = text.replace("channel = \"C0123ABC\"\n", "");
        let err = Job::parse(&text, "support-slack", &defaults(), &NeedsChannel).unwrap_err();
        assert_eq!(err, "slack: connector.channel is required");
    }

    #[test]
    fn exactly_one_schedule_and_name_must_match_stem() {
        let both = SPEC_EXAMPLE.replace("every = \"5m\"", "every = \"5m\"\ncron = \"* * * * *\"");
        assert!(
            Job::parse(&both, "support-slack", &defaults(), &Builtins)
                .unwrap_err()
                .contains("not both")
        );
        let neither = SPEC_EXAMPLE.replace("every = \"5m\"\n", "");
        assert!(
            Job::parse(&neither, "support-slack", &defaults(), &Builtins)
                .unwrap_err()
                .contains("every or cron")
        );
        let err = Job::parse(SPEC_EXAMPLE, "other", &defaults(), &Builtins).unwrap_err();
        assert!(err.contains("does not match the file name"), "{err}");
        // No name: the stem is the name.
        let unnamed = SPEC_EXAMPLE.replace("name = \"support-slack\"\n", "");
        assert_eq!(
            Job::parse(&unnamed, "anything-9", &defaults(), &Builtins)
                .unwrap()
                .name,
            "anything-9"
        );
    }

    /// An item must not be able to pick an existing branch such as `main`
    /// and have the agent commit to it. The rule: an item value may not
    /// appear in the first component of `branch`, so the job fixes the
    /// namespace (`pastor/...`) and items only name branches inside it.
    #[test]
    fn a_branch_must_start_with_a_component_the_job_fixes() {
        for branch in [
            "{{ item.branch }}",
            "{{ item.branch }}/x",
            "fix-{{ item.key }}",
            "{{ item.a }}{{ job.name }}/x",
        ] {
            let text = SPEC_EXAMPLE.replace("pastor/{{ item.key }}", branch);
            let err = Job::parse(&text, "support-slack", &defaults(), &Builtins).unwrap_err();
            assert!(
                err.starts_with("dispatch.branch:") && err.contains("first component"),
                "{branch}: {err}"
            );
        }
        for branch in [
            "pastor/{{ item.key }}",
            "{{ job.name }}/{{ item.key }}",
            "{{ task.id }}",
            "pastor/{{ item.a }}/{{ item.b }}",
            "main",
        ] {
            let text = SPEC_EXAMPLE.replace("pastor/{{ item.key }}", branch);
            Job::parse(&text, "support-slack", &defaults(), &Builtins)
                .unwrap_or_else(|e| panic!("{branch}: {e}"));
        }
    }

    #[test]
    fn defaults_fill_agent_timeout_and_max_tasks() {
        let text = r#"
every = "1h"
[connector]
use = "clock"
[dispatch]
prompt = "tick {{ item.key }} for {{ job.name }} as {{ task.id }}"
"#;
        let d = Defaults {
            agent: "codex".into(),
            agent_args: vec![],
            allow: vec![],
            deny: vec![],
            max_tasks_per_run: 2,
            timeout: "30m".into(),
            place: Default::default(),
            model: None,
            fallback: None,
            priority: None,
            agents: Default::default(),
            profile: None,
            label: None,
            summary: None,
        };
        let job = Job::parse(text, "hourly", &d, &Builtins).unwrap();
        assert_eq!(job.spec.agent, "codex");
        assert_eq!(job.max_tasks_per_run, 2);
        assert_eq!(job.spec.timeout_secs, 1800);
        assert!(job.enabled, "enabled defaults to true");
        assert!(!job.spec.worktree);
        assert_eq!(job.connector_config, serde_json::json!({}));
        assert_eq!(job.spec.place, Place::Repo);
    }

    /// `[dispatch] place` wins over `[defaults] place`, which fills in for a
    /// job that says nothing; a place that is none of the four is refused.
    #[test]
    fn place_comes_from_the_job_then_the_defaults() {
        let text = |extra: &str| {
            format!(
                "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"p\"\n{extra}"
            )
        };
        let d = Defaults {
            place: Place::Pastor,
            ..defaults()
        };
        let job = Job::parse(&text(""), "j", &d, &Builtins).unwrap();
        assert_eq!(job.spec.place, Place::Pastor);
        let job = Job::parse(&text("place = \"pane:work\"\n"), "j", &d, &Builtins).unwrap();
        assert_eq!(job.spec.place, Place::Pane("work".into()));
        let job = Job::parse(&text("place = \"own\"\n"), "j", &defaults(), &Builtins).unwrap();
        assert_eq!(job.spec.place, Place::Own);
        let err = Job::parse(&text("place = \"elsewhere\"\n"), "j", &d, &Builtins).unwrap_err();
        assert!(err.contains("unknown place elsewhere"), "{err}");
    }

    /// `[dispatch] label` is the job's label template, kept unrendered for
    /// dispatch; a job without one leaves it to the flock and `[defaults]`,
    /// settled when each task is queued.
    #[test]
    fn label_is_the_job_s_own_template() {
        let text = |extra: &str| {
            format!(
                "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"p\"\n{extra}"
            )
        };
        let d = Defaults {
            label: Some("{{ machine }}".into()),
            ..defaults()
        };
        let job = Job::parse(&text(""), "j", &d, &Builtins).unwrap();
        assert_eq!(job.spec.label.template, None);
        let job = Job::parse(
            &text("label = \"{{ job }}/{{ item.key }}\"\n"),
            "j",
            &d,
            &Builtins,
        )
        .unwrap();
        assert_eq!(
            job.spec.label.template.as_deref(),
            Some("{{ job }}/{{ item.key }}")
        );
        let err =
            Job::parse(&text("label = \"{{ job.name }}\"\n"), "j", &d, &Builtins).unwrap_err();
        assert!(err.contains("dispatch.label: unknown placeholder"), "{err}");
    }

    #[test]
    fn rejects_bad_names_templates_and_shapes() {
        let base = |name: &str, extra: &str| {
            format!(
                "name = \"{name}\"\nevery = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"p\"\n{extra}"
            )
        };
        for (name, needle) in [
            ("run", "reserved"),
            ("Upper", "must match"),
            ("-x", "must match"),
            ("a b", "must match"),
        ] {
            let err = Job::parse(&base(name, ""), name, &defaults(), &Builtins).unwrap_err();
            assert!(err.contains(needle), "{name}: {err}");
        }
        let err = Job::parse(
            &base("ok", "branch = \"pastor/{{ job.nope }}\"\n"),
            "ok",
            &defaults(),
            &Builtins,
        )
        .unwrap_err();
        assert!(
            err.contains("dispatch.branch") && err.contains("job.nope"),
            "{err}"
        );
        let err = Job::parse(
            &base("ok", "repo = \"{{ item.repo \"\n"),
            "ok",
            &defaults(),
            &Builtins,
        )
        .unwrap_err();
        assert!(
            err.contains("dispatch.repo") && err.contains("unterminated"),
            "{err}"
        );
        let err = Job::parse(
            &base("ok", "worktree = true\n"),
            "ok",
            &defaults(),
            &Builtins,
        )
        .unwrap_err();
        assert!(err.contains("needs dispatch.repo"), "{err}");
        let err = Job::parse(
            &base("ok", "max_tasks_per_run = 0\n"),
            "ok",
            &defaults(),
            &Builtins,
        )
        .unwrap_err();
        assert!(err.contains("max_tasks_per_run"), "{err}");
        let err = Job::parse(
            &base("ok", "colour = \"blue\"\n"),
            "ok",
            &defaults(),
            &Builtins,
        )
        .unwrap_err();
        assert!(
            err.contains("colour"),
            "unknown keys must be reported: {err}"
        );
        let err = Job::parse(
            "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"  \"\n",
            "ok",
            &defaults(),
            &Builtins,
        )
        .unwrap_err();
        assert!(err.contains("prompt is required"), "{err}");
    }

    /// A job keeps its `[dispatch]` table as written, less the prompt, for
    /// a headless serve to submit: the head builds the same job from it.
    #[test]
    fn a_job_keeps_its_dispatch_table_for_the_head() {
        let file = Job::parse(SPEC_EXAMPLE, "support-slack", &defaults(), &Builtins).unwrap();
        assert!(file.dispatch.get("prompt").is_none(), "{:?}", file.dispatch);
        assert_eq!(file.dispatch["repo"], "~/work/support");
        assert_eq!(file.dispatch["timeout"], "2h");
        assert!(file.dispatch.get("place").is_none(), "the head's default");
        let sub =
            Job::submitted("support-slack", &file.dispatch, &file.prompt, &defaults()).unwrap();
        assert_eq!(sub.spec, file.spec);
        assert_eq!(sub.agent, file.agent);
        assert_eq!(sub.flock, file.flock);
        assert_eq!(sub.prompt, file.prompt);
        assert_eq!(sub.dispatch, file.dispatch);
    }

    /// A submitted job's `[dispatch]` is the job file's, checked the same
    /// way: the same job, and the same error for the same mistake.
    #[test]
    fn a_submitted_dispatch_is_checked_like_a_job_file() {
        let file = Job::parse(SPEC_EXAMPLE, "support-slack", &defaults(), &Builtins).unwrap();
        let text: toml::Table = toml::from_str(SPEC_EXAMPLE).unwrap();
        let mut dispatch = serde_json::to_value(&text["dispatch"]).unwrap();
        let prompt = dispatch["prompt"].as_str().unwrap().to_string();
        dispatch.as_object_mut().unwrap().remove("prompt");
        let sub = Job::submitted("support-slack", &dispatch, &prompt, &defaults()).unwrap();
        assert_eq!(sub.spec, file.spec);
        assert_eq!(sub.agent, file.agent);
        assert_eq!(sub.flock, file.flock);
        assert_eq!(sub.prompt, file.prompt);
        assert_eq!(sub.max_tasks_per_run, file.max_tasks_per_run);

        let bad = |extra: &str| {
            let file = format!(
                "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"p\"\n{extra}"
            );
            let file_err = Job::parse(&file, "ok", &defaults(), &Builtins).unwrap_err();
            let table: toml::Table = toml::from_str(extra).unwrap();
            let json = serde_json::to_value(&table).unwrap();
            let err = Job::submitted("ok", &json, "p", &defaults()).unwrap_err();
            (file_err, err)
        };
        for extra in [
            "worktree = true\n",
            "max_tasks_per_run = 0\n",
            "branch = \"{{ item.key }}/x\"\n",
            "timeout = \"soon\"\n",
        ] {
            let (file_err, err) = bad(extra);
            assert_eq!(err, file_err, "{extra}");
        }
        let (file_err, err) = bad("colour = \"blue\"\n");
        assert!(file_err.contains("unknown field `colour`"), "{file_err}");
        assert!(err.contains("unknown field `colour`"), "{err}");

        let err = Job::submitted("run", &Value::Null, "p", &defaults()).unwrap_err();
        assert!(err.contains("reserved"), "{err}");
        let err = Job::submitted("ok", &Value::Null, " ", &defaults()).unwrap_err();
        assert!(err.contains("prompt is required"), "{err}");
        let err = Job::submitted("ok", &serde_json::json!([1]), "p", &defaults()).unwrap_err();
        assert!(err.contains("must be a table"), "{err}");

        // A shepherd sends `description` inside `[dispatch]` like any key.
        let sub = Job::submitted(
            "ok",
            &serde_json::json!({"description": "#{{ item.key }}"}),
            "p",
            &defaults(),
        )
        .unwrap();
        assert_eq!(
            sub.task_description_for(&serde_json::json!({"key": "k1"}))
                .as_deref(),
            Some("#k1")
        );
    }

    /// `[connector]` passes every key but `use` to the connector, so the
    /// only key pastor can call misspelt there is `use` itself: the load
    /// fails and names the file (the job) and the key.
    #[test]
    fn a_typo_of_connector_use_is_a_load_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = job_path(tmp.path(), "nightly");
        std::fs::write(
            &path,
            "every = \"1h\"\n[connector]\nuse_ = \"clock\"\n[dispatch]\nprompt = \"p\"\n",
        )
        .unwrap();
        let Loaded::Invalid { name, error } = load_file(&path, "nightly", &defaults(), &Builtins)
        else {
            panic!("a misspelt use must not load")
        };
        assert_eq!(name, "nightly");
        assert!(error.contains("`use`"), "{error}");
        // A typo beside `use` is the connector's config, not pastor's.
        std::fs::write(
            &path,
            "every = \"1h\"\n[connector]\nuse = \"clock\"\nchanel = \"C1\"\n[dispatch]\nprompt = \"p\"\n",
        )
        .unwrap();
        assert!(matches!(
            load_file(&path, "nightly", &defaults(), &Builtins),
            Loaded::Valid(_)
        ));
    }

    #[test]
    fn load_dir_sorts_and_reports_invalid_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("jobs");
        assert!(
            load_dir(&dir, &defaults(), &Builtins).unwrap().is_empty(),
            "missing dir is no jobs"
        );
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            job_path(&dir, "zeta"),
            "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"z\"\n",
        )
        .unwrap();
        std::fs::write(
            job_path(&dir, "alpha"),
            "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"a\"\n",
        )
        .unwrap();
        std::fs::write(job_path(&dir, "broken"), "every = \"1h\"\n[connector\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "ignored").unwrap();
        let loaded = load_dir(&dir, &defaults(), &Builtins).unwrap();
        assert_eq!(
            loaded.iter().map(Loaded::name).collect::<Vec<_>>(),
            vec!["alpha", "broken", "zeta"]
        );
        let Loaded::Invalid { error, .. } = &loaded[1] else {
            panic!("broken must be invalid")
        };
        assert!(!error.is_empty());
        assert!(matches!(&loaded[0], Loaded::Valid(j) if j.prompt == "a"));
    }

    #[test]
    fn set_enabled_rewrites_one_line_and_keeps_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("j.toml");
        // `enabled_looking` is a decoy: a sub-table key that merely starts
        // with "enabled", to prove the top-level-only line match isn't fooled
        // by a prefix. It sits under [connector], which accepts arbitrary
        // connector-specific keys; [dispatch] has a closed, known field set
        // and would reject it as unknown, which is not what this test is
        // about.
        let original = "# my job\nname = \"j\"\nevery = \"1h\"   # hourly\nenabled = true\n\n[connector]\nuse = \"clock\"\nenabled_looking = 1\n\n[dispatch]\nprompt = \"p\"\n";
        std::fs::write(&path, original).unwrap();
        set_enabled(&path, false).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            original.replace("enabled = true", "enabled = false"),
            "only the top-level enabled line changes"
        );
        assert!(Job::parse(&text, "j", &defaults(), &Builtins).is_ok());
        assert!(
            !Job::parse(&text, "j", &defaults(), &Builtins)
                .unwrap()
                .enabled
        );

        // Absent: inserted before the first table so it stays top-level.
        let without = "name = \"k\"\nevery = \"1h\"\n\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"p\"\n";
        std::fs::write(&path, without).unwrap();
        set_enabled(&path, false).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "name = \"k\"\nevery = \"1h\"\nenabled = false\n\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"p\"\n"
        );
        set_enabled(&path, true).unwrap();
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("enabled = true\n")
        );

        // A file that would not parse after the edit is left untouched.
        std::fs::write(&path, "every = \"1h\"\n[connector\n").unwrap();
        assert!(set_enabled(&path, true).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "every = \"1h\"\n[connector\n"
        );
    }

    #[test]
    fn set_enabled_on_a_symlink_edits_the_target_and_keeps_the_link() {
        let tmp = tempfile::tempdir().unwrap();
        // The job file lives elsewhere (e.g. a dotfiles repo); the jobs dir
        // only holds a symlink to it.
        let real_dir = tmp.path().join("dotfiles");
        std::fs::create_dir_all(&real_dir).unwrap();
        let target = real_dir.join("j.toml");
        let original = "name = \"j\"\nevery = \"1h\"\nenabled = true\n\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"p\"\n";
        std::fs::write(&target, original).unwrap();
        let link = tmp.path().join("jobs").join("j.toml");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        set_enabled(&link, false).unwrap();

        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "enable/disable must not replace the symlink with a regular file"
        );
        let via_target = std::fs::read_to_string(&target).unwrap();
        assert_eq!(
            via_target,
            original.replace("enabled = true", "enabled = false")
        );
        let via_link = std::fs::read_to_string(&link).unwrap();
        assert_eq!(via_link, via_target, "the link still resolves to the edit");
    }
}
