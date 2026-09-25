//! One job per TOML file in `~/.config/pastor/jobs/`. `JobFile` is the file's
//! shape and nothing else; `Job` is what survives validation and is what the
//! scheduler runs. Validation happens here, at load, so `job list` can show a
//! broken file as `invalid` with its reason instead of a run failing later.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;
use serde_json::Value;

use crate::config::{Defaults, parse_duration};
use crate::connector::Catalog;
use crate::schedule::Schedule;
use crate::task::DispatchSpec;
use crate::template;

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobFile {
    pub name: Option<String>,
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
    pub repo: Option<String>,
    pub worktree: bool,
    pub branch: Option<String>,
    pub tags: Vec<String>,
    pub machine: Option<String>,
    /// Where the job's tasks go; `None`: the flock of `machine`, else the
    /// default flock.
    pub flock: Option<String>,
    pub timeout: Option<String>,
    pub max_tasks_per_run: Option<u32>,
    pub backfill: Option<String>,
    pub prompt: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub name: String,
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
    /// a copy per task.
    pub spec: DispatchSpec,
    /// `dispatch.flock`, checked against flock.toml at each run (see
    /// `Flock::task_flock`): the job file does not know the flocks.
    pub flock: Option<String>,
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
        let d = file.dispatch;
        if d.prompt.trim().is_empty() {
            return Err("dispatch.prompt is required".into());
        }
        if d.worktree && d.repo.is_none() {
            return Err("dispatch.worktree = true needs dispatch.repo".into());
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
        Ok(Job {
            name,
            schedule,
            enabled: file.enabled,
            connector: file.connector.use_,
            connector_config,
            prompt: d.prompt,
            max_tasks_per_run,
            backfill,
            flock: d.flock,
            spec: DispatchSpec {
                agent: d.agent.unwrap_or_else(|| defaults.agent.clone()),
                agent_args: defaults.agent_args_or(d.agent_args),
                repo: d.repo,
                worktree: d.worktree,
                branch: d.branch,
                machine: d.machine,
                tags: d.tags,
                timeout_secs: timeout.as_secs(),
            },
        })
    }
}

/// Job names appear in `tasks.job`, in `{{ job.name }}` and, from plan 3, as a
/// directory under the state dir, so they are kept to a safe alphabet. `run`
/// is what one-off tasks carry in `tasks.job`. Public so `job_path` callers
/// outside a full `Job::parse` (the CLI's `enable`/`disable`) can reject a
/// name before joining it under the jobs directory: an unvalidated name like
/// `"../pastor"` resolves outside it entirely.
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
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
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
    // Resolve the real file first: `path` may be a symlink (e.g. into a
    // dotfiles repo), and writing the temp file next to `path` then renaming
    // over it would replace the link with a plain file. Writing beside, and
    // renaming onto, the canonical target keeps the link and edits what it
    // points to.
    let target =
        std::fs::canonicalize(path).with_context(|| format!("canonicalize {}", path.display()))?;
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

        let empty = Job::parse(SPEC_EXAMPLE, "support-slack", &d, &Builtins).unwrap();
        assert!(empty.spec.agent_args.is_empty(), "explicit [] opts out");

        let own = SPEC_EXAMPLE.replace("agent_args = []", "agent_args = [\"--model\", \"x\"]");
        let job = Job::parse(&own, "support-slack", &d, &Builtins).unwrap();
        assert_eq!(job.spec.agent_args, vec!["--model", "x"]);
    }

    #[test]
    fn a_connector_the_catalog_lacks_is_invalid() {
        let text = SPEC_EXAMPLE.replace("use = \"clock\"", "use = \"slack\"");
        let err = Job::parse(&text, "support-slack", &defaults(), &Builtins).unwrap_err();
        assert!(err.contains("slack"), "{err}");
        assert!(err.contains("not available"), "{err}");
    }

    /// A catalog's reason (a plugin that is missing, or a config key its
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
            max_tasks_per_run: 2,
            timeout: "30m".into(),
        };
        let job = Job::parse(text, "hourly", &d, &Builtins).unwrap();
        assert_eq!(job.spec.agent, "codex");
        assert_eq!(job.max_tasks_per_run, 2);
        assert_eq!(job.spec.timeout_secs, 1800);
        assert!(job.enabled, "enabled defaults to true");
        assert!(!job.spec.worktree);
        assert_eq!(job.connector_config, serde_json::json!({}));
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
