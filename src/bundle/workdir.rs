//! Workdir sets: files pastor places in an agent's working directory, as
//! bundles place theirs in a harness home. Whoever starts an agent hands
//! pastor the set before the start (`pastor workdir place`); pastor writes
//! it, keeps a ledger of what it wrote, and takes out what a later, smaller
//! set no longer has.
//!
//! - A set is one document, JSON or TOML, or a directory whose
//!   `pastor-workdir.toml` points at files beside it (`from`). It has an
//!   `id`, unique in the directory, and the harness `kind` of the agent that
//!   starts there.
//! - Its skills and files are whole files, placed by the apply engine
//!   (`apply::place_workdir`), with the same rules as a home: pastor changes
//!   or removes only what it wrote and still finds as it wrote it, never
//!   writes through a symlink, and backs a file up before it changes it.
//! - Its `settings` merge into the harness's local settings file key by
//!   key, and its `instructions` are a marked block in the local
//!   instruction file, as a home bundle's are in a home; the rest of either
//!   file is not pastor's.
//! - Sets share a directory: each item is keyed by the set's id, and a set
//!   that wants a file or a key another set holds is refused, naming both.
//! - The ledger is never inside the directory, where the agent works:
//!   `<state>/workdirs/<key>/ledger.json`, `<key>` from the directory's real
//!   path, which the ledger names.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::apply::{self, Action, Row};
use super::plan::{Item, What};
use super::secrets;
use super::workdir_git as git;
use crate::cli::CliError;
use crate::config::Paths;
use crate::profile_render::Render;

/// The error code when a set, or the directory it is for, is wrong.
pub const WORKDIR_INVALID: &str = "workdir_invalid";
/// The error code for a set that names MCP servers: a working directory
/// has no file of its own for them (a home bundle carries them).
pub const WORKDIR_MCP_UNSUPPORTED: &str = "workdir_mcp_unsupported";
/// The error code for what a set may name but pastor does not place in a
/// working directory: a kind other than claude or agy.
pub const WORKDIR_UNSUPPORTED: &str = "workdir_unsupported";
/// The error code for a set whose settings name the permission keys a
/// profile writes: `allow`, `deny`, `ask` and `defaultMode`.
pub const WORKDIR_PERMISSIONS_OWNED: &str = "workdir_permissions_owned";
/// The error code for a set that wants a file or a settings key another
/// set in the directory holds.
pub const WORKDIR_CONFLICT: &str = "workdir_conflict";

/// The manifest of a set given as a directory.
pub const SET_FILE: &str = "pastor-workdir.toml";
/// The most a set may carry, its files and vars together.
pub const MAX_SET: usize = 4 << 20;
/// Where the ledgers are, under the state dir.
pub const LEDGERS: &str = "workdirs";

/// The vars pastor fills itself; a set may not name its own so.
const OWN_VARS: [&str; 6] = ["home", "workdir", "machine", "agent", "kind", "account"];

/// The harness kinds a set may name.
pub(crate) const KINDS: [&str; 4] = ["claude", "agy", "codex", "opencode"];
/// The kinds pastor places sets for.
pub(crate) const PLACED_KINDS: [&str; 2] = ["claude", "agy"];

/// Where a set of one kind goes in a working directory.
struct Slots {
    /// Its skills, a directory each.
    skills: &'static str,
    /// The file its `settings` merge into.
    settings: &'static str,
    /// Paths a set never places whole: the harness's own files, the
    /// repository's (usually tracked) and the ones pastor merges into.
    never: &'static [&'static str],
    /// Directories a set never places a file in: the harness's own state,
    /// or what it no longer reads.
    never_under: &'static [&'static str],
}

/// Claude: `.claude/settings.local.json` and `CLAUDE.local.md` are its
/// local files, for one person and usually not tracked.
const CLAUDE_SLOTS: Slots = Slots {
    skills: ".claude/skills",
    settings: ".claude/settings.local.json",
    never: &[
        ".claude/settings.json",
        ".claude/settings.local.json",
        ".mcp.json",
        CLAUDE_INSTRUCTIONS,
    ],
    never_under: &[],
};

/// agy: named hooks in `.agents/hooks.json`, its instructions in
/// `AGENTS.md`. `.agents/teamwork/` is its own working state,
/// `.agents/workflows/` it no longer reads (skills took their place), and
/// `.agents/skills.json` and `.agents/mcp_config.json` point it at skills
/// and MCP servers elsewhere, which a set does not carry.
const AGY_SLOTS: Slots = Slots {
    skills: ".agents/skills",
    settings: ".agents/hooks.json",
    never: &[
        ".agents/hooks.json",
        AGY_INSTRUCTIONS,
        ".agents/skills.json",
        ".agents/mcp_config.json",
    ],
    never_under: &[".agents/teamwork", ".agents/workflows"],
};

fn slots(kind: &str) -> &'static Slots {
    match kind {
        "agy" => &AGY_SLOTS,
        _ => &CLAUDE_SLOTS,
    }
}

/// Claude's instruction file in a working directory, which holds the
/// `instructions` block.
const CLAUDE_INSTRUCTIONS: &str = "CLAUDE.local.md";
/// Where Claude's block goes when the repository tracks
/// `CLAUDE_INSTRUCTIONS`.
const CLAUDE_INSTRUCTIONS_FALLBACK: &str = ".claude/CLAUDE.md";
/// agy's instruction file in a working directory.
const AGY_INSTRUCTIONS: &str = "AGENTS.md";
/// Files a set merges into that people commit with the project, unlike
/// the harness-local ones: a line in the repository's exclude file would
/// hide the user's own copy from `git status` in every worktree. So one is
/// kept out of git only when pastor made it in this directory
/// (`workdir_git::exclude`).
pub const SHARED_FILES: [&str; 3] = [
    AGY_INSTRUCTIONS,
    AGY_SLOTS.settings,
    CLAUDE_INSTRUCTIONS_FALLBACK,
];

/// What starts a rule file of agy's that it always reads: it skips one
/// without it.
const AGY_RULE_HEAD: &str = "---\ntrigger: always_on\n---\n\n";

/// Where a set's instructions go in a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Instructions {
    /// A marked block in this file.
    Block(&'static str),
    /// A file of the set's own: agy's rule, when the repository tracks
    /// `AGENTS.md`.
    Rule(String),
}

impl Instructions {
    fn path(&self) -> &str {
        match self {
            Instructions::Block(p) => p,
            Instructions::Rule(p) => p,
        }
    }
}
/// The permission keys a profile writes, never a set.
const PROFILE_KEYS: [&str; 4] = ["allow", "deny", "ask", "defaultMode"];

/// A set as it is written: what `place` reads.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Doc {
    id: String,
    kind: String,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    settings: Option<serde_json::Value>,
    #[serde(default)]
    instructions: Option<String>,
    #[serde(default)]
    mcp: Option<serde_json::Value>,
    #[serde(default)]
    skills: Vec<SkillDoc>,
    #[serde(default)]
    files: Vec<FileDoc>,
    #[serde(default)]
    vars: BTreeMap<String, String>,
    #[serde(default)]
    git_exclude: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillDoc {
    name: String,
    /// Its files by path inside the skill: `SKILL.md` and any beside it.
    #[serde(default)]
    files: BTreeMap<String, String>,
    /// In a directory set: the skill's directory, relative to the set.
    #[serde(default)]
    from: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileDoc {
    path: String,
    #[serde(default)]
    content: Option<String>,
    /// In a directory set: the file, relative to the set.
    #[serde(default)]
    from: Option<String>,
    /// Octal, as a string: `"0755"`.
    #[serde(default)]
    mode: Option<String>,
    /// Fill pastor's vars and the set's in it; off by default.
    #[serde(default)]
    render: bool,
}

/// A set read and checked, every file's content in hand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Set {
    pub id: String,
    pub kind: String,
    /// The permission profile whose rules go in the settings file: rendered
    /// by pastor when the set is placed (`place_with`), never carried by
    /// the set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Merged into the settings file, in the harness's own shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<serde_json::Map<String, serde_json::Value>>,
    /// The text of the set's block in the instruction file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    pub skills: Vec<Skill>,
    pub files: Vec<File>,
    pub vars: BTreeMap<String, String>,
    pub git_exclude: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Skill {
    pub name: String,
    pub files: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct File {
    pub path: String,
    pub content: String,
    /// The mode the set asks for; unset, a new file gets `0644` and one
    /// there keeps its own.
    pub mode: Option<u32>,
    pub render: bool,
}

fn invalid(message: impl std::fmt::Display) -> anyhow::Error {
    CliError::err(WORKDIR_INVALID, message)
}

impl Set {
    /// Read a set from `src`: `-` for stdin, a JSON or TOML file, or a
    /// directory with a `pastor-workdir.toml`.
    pub fn load(src: &str) -> anyhow::Result<Set> {
        if src == "-" {
            use std::io::Read as _;
            // One byte past the cap is enough to refuse a set too big.
            let mut text = String::new();
            std::io::stdin()
                .take(MAX_SET as u64 + 1)
                .read_to_string(&mut text)
                .map_err(|e| invalid(format!("stdin: {e}")))?;
            return Set::parse(&text, None);
        }
        let path = Path::new(src);
        if path.is_dir() {
            let manifest = path.join(SET_FILE);
            let mut left = MAX_SET;
            let text = read_plain(&manifest, &mut left)?;
            let doc: Doc = toml::from_str(&text)
                .map_err(|e| invalid(format!("{}: {}", manifest.display(), e.message())))?;
            return Set::check(doc, Some(path));
        }
        let text = read_capped(path)?;
        Set::parse(&text, path.extension().and_then(|e| e.to_str()))
    }

    /// A set from its text: JSON, or TOML; `ext` decides when given.
    pub fn parse(text: &str, ext: Option<&str>) -> anyhow::Result<Set> {
        if text.len() > MAX_SET {
            return Err(invalid(format!("the set is over {} MiB", MAX_SET >> 20)));
        }
        let json = match ext {
            Some("json") => true,
            Some("toml") => false,
            _ => text.trim_start().starts_with('{'),
        };
        let doc: Doc = if json {
            serde_json::from_str(text).map_err(|e| invalid(format!("the set: {e}")))?
        } else {
            toml::from_str(text).map_err(|e| invalid(format!("the set: {}", e.message())))?
        };
        Set::check(doc, None)
    }

    /// A set from its document, as `to_doc` writes it and the head receives
    /// it. It is measured by what it places (`check_size`), not by its text,
    /// which escaping makes longer.
    pub fn from_doc(doc: &serde_json::Value) -> anyhow::Result<Set> {
        let doc = Doc::deserialize(doc).map_err(|e| invalid(format!("the set: {e}")))?;
        Set::check(doc, None)
    }

    /// Check `doc` and read what it points at in `dir`, for a set given as
    /// a directory.
    fn check(doc: Doc, dir: Option<&Path>) -> anyhow::Result<Set> {
        // What files a directory set points at may still read, in all.
        let mut left = MAX_SET;
        if !is_name(&doc.id) {
            return Err(invalid(format!(
                "id {:?}: lower-case letters, digits, '.', '_' and '-', at most 64, starting with a letter or digit",
                doc.id
            )));
        }
        if !KINDS.contains(&doc.kind.as_str()) {
            return Err(invalid(format!(
                "kind {:?}: one of {}",
                doc.kind,
                KINDS.join(", ")
            )));
        }
        let mcp_in_settings = doc.settings.as_ref().is_some_and(|s| {
            s.get("mcpServers").is_some() || s.get("enabledMcpjsonServers").is_some()
        });
        if doc.mcp.is_some() || mcp_in_settings {
            return Err(CliError::err(
                WORKDIR_MCP_UNSUPPORTED,
                "a working directory has no file of its own for MCP servers; carry them in a home bundle",
            ));
        }
        if !PLACED_KINDS.contains(&doc.kind.as_str()) {
            return Err(CliError::err(
                WORKDIR_UNSUPPORTED,
                format!(
                    "kind {}: pastor places workdir sets for {} only",
                    doc.kind,
                    PLACED_KINDS.join(" and ")
                ),
            ));
        }
        if let Some(p) = &doc.profile
            && (p.trim().is_empty() || p.chars().any(char::is_control))
        {
            return Err(invalid("profile: the name of a permission profile"));
        }
        let settings = match doc.settings {
            None => None,
            Some(serde_json::Value::Object(map)) if doc.kind == "agy" => Some(check_hooks(map)?),
            Some(serde_json::Value::Object(map)) => Some(check_settings(map)?),
            Some(_) => return Err(invalid("settings: an object, in the harness's own shape")),
        };
        // No text is no block: a launcher may hand an empty one.
        let instructions = doc.instructions.filter(|t| !t.trim().is_empty());
        for name in doc.vars.keys() {
            if OWN_VARS.contains(&name.as_str()) {
                return Err(invalid(format!("vars.{name}: pastor fills it itself")));
            }
            if !name.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
                || !name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            {
                return Err(invalid(format!(
                    "vars.{name}: lower-case letters, digits and '_'"
                )));
            }
        }
        // As it will be written, its vars filled: a marker in a var counts.
        if let Some(text) = &instructions
            && let Some(line) = super::plan::fill(text, &doc.vars).lines().find(|l| {
                let l = l.trim_start();
                l.starts_with("<!-- pastor:") || l.starts_with("<!-- /pastor:")
            })
        {
            return Err(invalid(format!(
                "instructions: {line:?} is how pastor marks its blocks; it may not be in one"
            )));
        }
        let mut skills = Vec::new();
        for s in doc.skills {
            if !is_name(&s.name) {
                return Err(invalid(format!("skill {:?}: not a skill name", s.name)));
            }
            let files = match (s.from, dir) {
                (Some(_), None) => {
                    return Err(invalid(format!(
                        "skill {}: from is only for a set given as a directory",
                        s.name
                    )));
                }
                (Some(from), Some(dir)) => {
                    if !s.files.is_empty() {
                        return Err(invalid(format!(
                            "skill {}: from or files, not both",
                            s.name
                        )));
                    }
                    read_tree(dir, &from, &mut left)?
                }
                (None, _) => s.files,
            };
            for rel in files.keys() {
                relative(rel).map_err(|why| invalid(format!("skill {}: {rel}: {why}", s.name)))?;
            }
            if !files.contains_key("SKILL.md") {
                return Err(invalid(format!("skill {}: it has no SKILL.md", s.name)));
            }
            skills.push(Skill {
                name: s.name,
                files,
            });
        }
        let mut files = Vec::new();
        for f in doc.files {
            relative(&f.path).map_err(|why| invalid(format!("files: {}: {why}", f.path)))?;
            let slots = slots(&doc.kind);
            if let Some(slot) = slots
                .never
                .iter()
                .find(|n| Path::new(&f.path) == Path::new(n))
            {
                return Err(invalid(format!(
                    "files: {slot} is the harness's own file or a slot pastor merges into, never a whole file of a set"
                )));
            }
            if let Some(under) = slots
                .never_under
                .iter()
                .find(|n| Path::new(&f.path).starts_with(n))
            {
                return Err(invalid(format!(
                    "files: {}: {under} is the harness's own, never a set's",
                    f.path
                )));
            }
            let content = match (f.content, f.from, dir) {
                (Some(c), None, _) => c,
                (None, Some(from), Some(dir)) => read_plain(&inside(dir, &from)?, &mut left)?,
                (None, Some(_), None) => {
                    return Err(invalid(format!(
                        "files: {}: from is only for a set given as a directory",
                        f.path
                    )));
                }
                _ => {
                    return Err(invalid(format!(
                        "files: {}: content or from, one of them",
                        f.path
                    )));
                }
            };
            let mode = match f.mode {
                None => None,
                Some(m) => Some(
                    parse_mode(&m)
                        .ok_or_else(|| invalid(format!("files: {}: mode {m:?}", f.path)))?,
                ),
            };
            files.push(File {
                path: f.path,
                content,
                mode,
                render: f.render,
            });
        }
        let set = Set {
            id: doc.id,
            kind: doc.kind,
            profile: doc.profile,
            settings,
            instructions,
            skills,
            files,
            vars: doc.vars,
            git_exclude: doc.git_exclude.unwrap_or(true),
        };
        set.check_paths()?;
        set.check_size()?;
        set.check_secrets()?;
        Ok(set)
    }

    /// Every file of the set by where it goes in the directory, relative.
    fn placed(&self) -> Vec<(String, &str, Option<u32>, bool)> {
        let mut out = Vec::new();
        for s in &self.skills {
            for (rel, text) in &s.files {
                out.push((
                    format!("{}/{}/{rel}", slots(&self.kind).skills, s.name),
                    text.as_str(),
                    None,
                    false,
                ));
            }
        }
        for f in &self.files {
            out.push((f.path.clone(), f.content.as_str(), f.mode, f.render));
        }
        out
    }

    fn check_paths(&self) -> anyhow::Result<()> {
        let placed = self.placed();
        let mut seen: Vec<&str> = Vec::new();
        for (path, ..) in &placed {
            if seen.contains(&path.as_str()) {
                return Err(invalid(format!("{path}: the set places it twice")));
            }
            seen.push(path);
        }
        // A file where another goes under it: `a` and `a/b`.
        for a in &seen {
            if let Some(b) = seen.iter().find(|b| Path::new(b).starts_with(a) && b != &a) {
                return Err(invalid(format!("{a}: a file, and {b} is inside it")));
            }
        }
        Ok(())
    }

    fn check_size(&self) -> anyhow::Result<()> {
        let size: usize = self
            .placed()
            .iter()
            .map(|(_, t, ..)| t.len())
            .sum::<usize>()
            + self.vars.values().map(String::len).sum::<usize>()
            + self.settings.as_ref().map_or(0, |s| {
                serde_json::Value::Object(s.clone()).to_string().len()
            })
            + self.instructions.as_ref().map_or(0, String::len);
        if size > MAX_SET {
            return Err(invalid(format!("the set is over {} MiB", MAX_SET >> 20)));
        }
        Ok(())
    }

    /// Refuse a set with what looks like a secret: it is written in plain
    /// text into a directory an agent works in, often a repository.
    fn check_secrets(&self) -> anyhow::Result<()> {
        let mut found = Vec::new();
        for (path, text, ..) in self.placed() {
            found.extend(secrets::scan_text(&path, text));
        }
        // A rendered file holds what its vars put in it: a token split
        // between the file and a var is whole only there.
        for (path, text, _, render) in self.placed() {
            if render {
                found.extend(secrets::scan_text(
                    &format!("{path} (rendered)"),
                    &super::plan::fill(text, &self.vars),
                ));
            }
        }
        // Settings and instructions are always rendered: each string as it
        // will be written, the set's vars filled.
        if let Some(text) = &self.instructions {
            found.extend(secrets::scan_lines(
                "instructions",
                &super::plan::fill(text, &self.vars),
            ));
        }
        if let Some(settings) = &self.settings {
            // A string under a key is checked as an env variable of that
            // name would be (`env` entries, a hook's headers, any
            // `client_secret`), and every string for a token's shape.
            for (at, key, text) in strings("settings", settings) {
                let text = super::plan::fill(text, &self.vars);
                let many = text.lines().count() > 1;
                for (i, line) in text.lines().enumerate() {
                    let what = secrets::scan_lines(&at, line)
                        .into_iter()
                        .map(|f| f.what)
                        .next()
                        .or_else(|| key.and_then(|k| secrets::env_finding(k, line)));
                    if let Some(what) = what {
                        found.push(secrets::Finding {
                            path: at.clone(),
                            line: many.then_some(i + 1),
                            what,
                        });
                    }
                }
            }
        }
        // A var is a name and a value, as an env variable is; its name is
        // no file's.
        for (name, value) in &self.vars {
            for (i, line) in value.lines().enumerate() {
                if let Some(what) = secrets::env_finding(name, line) {
                    found.push(secrets::Finding {
                        path: format!("vars.{name}"),
                        line: (value.lines().count() > 1).then_some(i + 1),
                        what,
                    });
                }
            }
        }
        found.dedup();
        if found.is_empty() {
            return Ok(());
        }
        let list: Vec<String> = found.iter().map(ToString::to_string).collect();
        Err(CliError::err(
            super::BUNDLE_SECRET,
            format!(
                "the set looks like it holds a secret: {}; have the agent read it from its env instead",
                list.join("; ")
            ),
        ))
    }

    /// The set as a document `parse` reads back to the same set: what the
    /// head sends a machine, and keeps to check it by. A directory set's
    /// files are inline in it.
    pub fn to_doc(&self) -> serde_json::Value {
        let mut doc = serde_json::json!({
            "id": self.id,
            "kind": self.kind,
            "skills": self.skills.iter().map(|s| serde_json::json!({
                "name": s.name,
                "files": s.files,
            })).collect::<Vec<_>>(),
            "files": self.files.iter().map(|f| {
                let mut file = serde_json::json!({
                    "path": f.path,
                    "content": f.content,
                    "render": f.render,
                });
                if let Some(mode) = f.mode {
                    file["mode"] = format!("{mode:04o}").into();
                }
                file
            }).collect::<Vec<_>>(),
            "vars": self.vars,
            "git_exclude": self.git_exclude,
        });
        if let Some(p) = &self.profile {
            doc["profile"] = p.clone().into();
        }
        if let Some(settings) = &self.settings {
            doc["settings"] = settings.clone().into();
        }
        if let Some(text) = &self.instructions {
            doc["instructions"] = text.clone().into();
        }
        doc
    }

    /// The SHA-256 of the set as it was checked.
    pub fn hash(&self) -> String {
        hex(&Sha256::digest(
            serde_json::to_string(self)
                .expect("a set serializes")
                .as_bytes(),
        ))
    }

    /// The set's items in `dir`, vars filled in the files that ask for it
    /// and in every string of its settings and instructions. The
    /// instructions go where `instructions` says; none skips them.
    fn items(
        &self,
        dir: &Path,
        home: &Path,
        instructions: Option<&Instructions>,
        render: Option<&Render>,
    ) -> Vec<Item> {
        let mut vars = self.vars.clone();
        vars.insert("home".into(), home.display().to_string());
        vars.insert("workdir".into(), dir.display().to_string());
        vars.insert("kind".into(), self.kind.clone());
        let item = |rel: &str, what: What| Item {
            bundle: self.id.clone(),
            home: None,
            path: dir.join(rel).display().to_string(),
            what,
        };
        let mut items: Vec<Item> = self
            .placed()
            .into_iter()
            .map(|(rel, text, mode, render)| {
                let text = if render {
                    super::plan::fill(text, &vars)
                } else {
                    text.to_string()
                };
                item(&rel, What::Text { text, mode })
            })
            .collect();
        if let Some(settings) = &self.settings {
            let whats = match self.kind.as_str() {
                // Each named hook whole.
                "agy" => super::plan::settings_items("agy", settings.clone()),
                _ => settings_items(settings.clone()),
            };
            for what in whats {
                let what = match what {
                    What::Key { key, value } => What::Key {
                        key,
                        value: super::plan::fill_value(value, &vars),
                    },
                    What::Entry { key, value } => What::Entry {
                        key,
                        value: super::plan::fill_value(value, &vars),
                    },
                    other => other,
                };
                items.push(item(slots(&self.kind).settings, what));
            }
        }
        // The profile's rules, key by key, which pastor owns whole.
        if let Some(permissions) = render
            .filter(|r| r.kind == "claude")
            .map(|r| &r.settings["permissions"])
            .and_then(serde_json::Value::as_object)
        {
            for k in PROFILE_KEYS {
                // A list the profile has none of is empty, not left to the
                // agent: a rule it asks about would shadow an allowed one.
                let value = match (permissions.get(k), k) {
                    (Some(value), _) => value.clone(),
                    (None, "defaultMode") => continue,
                    (None, _) => serde_json::json!([]),
                };
                items.push(item(
                    slots("claude").settings,
                    What::Key {
                        key: vec!["permissions".into(), k.into()],
                        value,
                    },
                ));
            }
        }
        if let (Some(text), Some(at)) = (&self.instructions, instructions) {
            let text = super::plan::fill(text, &vars);
            items.push(match at {
                Instructions::Block(file) => item(file, What::Block { text }),
                Instructions::Rule(file) => item(
                    file,
                    What::Text {
                        text: format!("{AGY_RULE_HEAD}{text}"),
                        mode: None,
                    },
                ),
            });
        }
        items
    }
}

/// A set's settings, checked: `permissions` is an object without the keys
/// a profile writes, and its `additionalDirectories` a list of strings.
fn check_settings(
    settings: serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
    let Some(permissions) = settings.get("permissions") else {
        return Ok(settings);
    };
    let Some(permissions) = permissions.as_object() else {
        return Err(invalid("settings.permissions: an object"));
    };
    if let Some(key) = PROFILE_KEYS.iter().find(|k| permissions.contains_key(**k)) {
        return Err(CliError::err(
            WORKDIR_PERMISSIONS_OWNED,
            format!(
                "settings.permissions.{key}: what an agent may run comes from a profile, never a set; leave it out"
            ),
        ));
    }
    if let Some(dirs) = permissions.get("additionalDirectories")
        && !dirs
            .as_array()
            .is_some_and(|d| d.iter().all(serde_json::Value::is_string))
    {
        return Err(invalid(
            "settings.permissions.additionalDirectories: a list of directories",
        ));
    }
    Ok(settings)
}

/// An agy set's settings, checked: named hooks, each an object of its
/// events, as `.agents/hooks.json` holds them.
fn check_hooks(
    hooks: serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
    if let Some((name, _)) = hooks.iter().find(|(_, v)| !v.is_object()) {
        return Err(invalid(format!(
            "settings.{name}: a named hook, an object of its events, as .agents/hooks.json holds it"
        )));
    }
    Ok(hooks)
}

/// A set's settings as items, by the home bundle's rules
/// (`plan::settings_items`), with `permissions` key by key and each of
/// its `additionalDirectories` an entry: the agent's own rules there, and
/// another set's directories, are not this set's.
fn settings_items(mut settings: serde_json::Map<String, serde_json::Value>) -> Vec<What> {
    let permissions = settings.remove("permissions");
    let mut out = super::plan::settings_items("claude", settings);
    if let Some(serde_json::Value::Object(permissions)) = permissions {
        for (k, v) in permissions {
            let key = vec!["permissions".to_string(), k.clone()];
            match v {
                serde_json::Value::Array(dirs) if k == "additionalDirectories" => {
                    out.extend(dirs.into_iter().map(|value| What::Entry {
                        key: key.clone(),
                        value,
                    }));
                }
                value => out.push(What::Key { key, value }),
            }
        }
    }
    out
}

/// Every string in `value`, by where it is, dotted from `at`.
/// The key of the object a string sits in, if it sits under one.
type Strings<'a> = Vec<(String, Option<&'a str>, &'a str)>;

/// Every string in `value`, by where it is, dotted from `at`, with the key
/// it is the value of (none for a list's item).
fn strings<'a>(at: &str, value: &'a serde_json::Map<String, serde_json::Value>) -> Strings<'a> {
    fn walk<'a>(at: String, key: Option<&'a str>, v: &'a serde_json::Value, out: &mut Strings<'a>) {
        match v {
            serde_json::Value::String(s) => out.push((at, key, s)),
            serde_json::Value::Array(a) => {
                for (i, v) in a.iter().enumerate() {
                    walk(format!("{at}.{i}"), None, v, out);
                }
            }
            serde_json::Value::Object(o) => {
                for (k, v) in o {
                    walk(format!("{at}.{k}"), Some(k), v, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for (k, v) in value {
        walk(format!("{at}.{k}"), Some(k), v, &mut out);
    }
    out
}

/// Where `set`'s instructions go in `dir`. Claude: a block in
/// `CLAUDE.local.md`, or, when the repository tracks it, in
/// `.claude/CLAUDE.md`; `Err` says why neither: the repository tracks both.
/// agy: a block in `AGENTS.md`, or, when the repository tracks it, a rule
/// of the set's own. With no repository, or no `git`, nothing is tracked.
fn instruction_file(dir: &Path, set: &Set) -> Result<Instructions, String> {
    let tracked = |rel: &str| git::tracked(GIT, dir, Path::new(rel));
    if set.kind == "agy" {
        if !tracked(AGY_INSTRUCTIONS) {
            return Ok(Instructions::Block(AGY_INSTRUCTIONS));
        }
        return Ok(Instructions::Rule(format!(
            ".agents/rules/pastor-{}.md",
            set.id
        )));
    }
    if !tracked(CLAUDE_INSTRUCTIONS) {
        return Ok(Instructions::Block(CLAUDE_INSTRUCTIONS));
    }
    if !tracked(CLAUDE_INSTRUCTIONS_FALLBACK) {
        return Ok(Instructions::Block(CLAUDE_INSTRUCTIONS_FALLBACK));
    }
    Err(format!(
        "the repository tracks {CLAUDE_INSTRUCTIONS} and {CLAUDE_INSTRUCTIONS_FALLBACK}; pastor writes its block in neither"
    ))
}

/// Whether agy, with its home at `home`, trusts one of `folders` itself:
/// it loads a folder's hooks in an interactive session only then, and a
/// parent it trusts does not count. A settings file it cannot read trusts
/// nothing.
fn agy_trusts(home: &Path, folders: &[&Path]) -> bool {
    let rel = crate::provision::agy::SETTINGS_FILE.trim_start_matches("~/");
    let Ok(text) = std::fs::read_to_string(home.join(rel)) else {
        return false;
    };
    folders.iter().any(|f| {
        matches!(
            crate::trust_write::agy_edit(&text, &f.display().to_string()),
            Ok(crate::trust_write::Edit::Already)
        )
    })
}

/// A set's own file, given by its path: a regular file, at most
/// `MAX_SET`.
fn read_capped(path: &Path) -> anyhow::Result<String> {
    let meta = path
        .metadata()
        .map_err(|e| invalid(format!("{}: {e}", path.display())))?;
    let mut left = MAX_SET;
    read_file(path, &meta, &mut left)
}

/// A file inside a set's directory: a plain file, never a symlink, and
/// what is `left` of the set's budget at most, which it takes from.
fn read_plain(path: &Path, left: &mut usize) -> anyhow::Result<String> {
    let meta = path
        .symlink_metadata()
        .map_err(|e| invalid(format!("{}: {e}", path.display())))?;
    if meta.file_type().is_symlink() {
        return Err(invalid(format!(
            "{}: a symlink; a set's files are plain files",
            path.display()
        )));
    }
    read_file(path, &meta, left)
}

fn read_file(path: &Path, meta: &std::fs::Metadata, left: &mut usize) -> anyhow::Result<String> {
    // A FIFO or a device would block, or never end.
    if !meta.is_file() {
        return Err(invalid(format!("{}: not a regular file", path.display())));
    }
    if meta.len() > *left as u64 {
        return Err(invalid(format!(
            "{}: the set is over {} MiB",
            path.display(),
            MAX_SET >> 20
        )));
    }
    let text =
        std::fs::read_to_string(path).map_err(|e| invalid(format!("{}: {e}", path.display())))?;
    *left = left.saturating_sub(text.len());
    Ok(text)
}

/// `rel` inside the set's directory `dir`, with no symlink on its way: it
/// cannot lead out of it.
fn inside(dir: &Path, rel: &str) -> anyhow::Result<PathBuf> {
    relative(rel).map_err(|why| invalid(format!("from {rel}: {why}")))?;
    let mut at = dir
        .canonicalize()
        .map_err(|e| invalid(format!("{}: {e}", dir.display())))?;
    for part in Path::new(rel).components() {
        at.push(part);
        let meta = at
            .symlink_metadata()
            .map_err(|e| invalid(format!("from {rel}: {e}")))?;
        if meta.file_type().is_symlink() {
            return Err(invalid(format!(
                "from {rel}: {} is a symlink; a set's files are plain files",
                at.display()
            )));
        }
    }
    Ok(at)
}

/// The files under `rel` in the set's directory, by path inside it.
fn read_tree(dir: &Path, rel: &str, left: &mut usize) -> anyhow::Result<BTreeMap<String, String>> {
    let root = inside(dir, rel)?;
    if !root.is_dir() {
        return Err(invalid(format!("from {rel}: not a directory")));
    }
    let mut out = BTreeMap::new();
    let mut todo = vec![root.clone()];
    while let Some(d) = todo.pop() {
        let entries =
            std::fs::read_dir(&d).map_err(|e| invalid(format!("{}: {e}", d.display())))?;
        for e in entries {
            let e = e.map_err(|e| invalid(format!("{}: {e}", d.display())))?;
            let path = e.path();
            let kind = e
                .file_type()
                .map_err(|e| invalid(format!("{}: {e}", path.display())))?;
            if kind.is_dir() {
                todo.push(path);
                continue;
            }
            let name = path
                .strip_prefix(&root)
                .expect("under the skill's dir")
                .to_string_lossy()
                .into_owned();
            out.insert(name, read_plain(&path, left)?);
        }
    }
    Ok(out)
}

/// Whether `rel` is a plain relative path that stays inside the directory
/// and out of `.git`; `Err` says why not.
fn relative(rel: &str) -> Result<(), &'static str> {
    let path = Path::new(rel);
    if rel.is_empty() || rel.ends_with('/') {
        return Err("not a file path");
    }
    if !path.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err("a path inside the directory, with no '..', '.' or leading '/'");
    }
    if path.components().any(|c| c.as_os_str() == ".git") {
        return Err("inside .git, which pastor never writes");
    }
    if rel.chars().any(char::is_control) {
        return Err("a name with no control characters");
    }
    Ok(())
}

pub(crate) fn is_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

/// An octal file mode pastor can read back: at most `0777`, readable by
/// its owner.
fn parse_mode(m: &str) -> Option<u32> {
    let mode = u32::from_str_radix(m, 8).ok()?;
    (mode <= 0o777 && mode & 0o400 != 0).then_some(mode)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The ledger dir of the directory at `real`, its resolved path.
pub fn ledger_dir(paths: &Paths, real: &Path) -> PathBuf {
    let key = hex(&Sha256::digest(real.as_os_str().as_encoded_bytes()));
    paths.state_dir.join(LEDGERS).join(&key[..16])
}

/// The directory a set goes in: absolute, its symlinks above it resolved,
/// never a symlink itself nor the home.
fn workdir(dir: &Path, home: &Path) -> anyhow::Result<PathBuf> {
    let meta = dir
        .symlink_metadata()
        .map_err(|e| invalid(format!("{}: {e}", dir.display())))?;
    if meta.file_type().is_symlink() {
        return Err(invalid(format!(
            "{} is a symlink; pastor does not write through it: give the directory it points to",
            dir.display()
        )));
    }
    if !meta.is_dir() {
        return Err(invalid(format!("{} is not a directory", dir.display())));
    }
    let real = dir
        .canonicalize()
        .map_err(|e| invalid(format!("{}: {e}", dir.display())))?;
    if home.canonicalize().is_ok_and(|h| h == real) {
        return Err(invalid(
            "the home directory is where harness homes are, not a working directory; bundles place files there",
        ));
    }
    Ok(real)
}

/// What `place` and `remove` answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placed {
    /// Always `workdir`.
    pub target: String,
    pub dir: String,
    /// The set placed, or removed; none for every set in the directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub dry_run: bool,
    pub ok: bool,
    pub rows: Vec<Row>,
    /// The machine the directory is on, when the head placed it there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    /// The profile the set names, as rendered: what the launch adds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<ProfileOut>,
    /// How the set stands, when this is a check's answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<super::workdir_keep::State>,
    /// The first few items that are not ok, with `state`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}

/// A rendered profile as `place --json` echoes it: the args and env the
/// launcher adds to the agent's start, so one answer gives the files and
/// the command line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileOut {
    pub name: String,
    pub flags: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Patterns of the profile that do not reach the agent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unsupported: Vec<String>,
}

impl Placed {
    pub fn new(dir: &Path, id: Option<&str>, dry_run: bool, rows: Vec<Row>) -> Placed {
        Placed {
            target: "workdir".into(),
            dir: dir.display().to_string(),
            id: id.map(str::to_string),
            dry_run,
            ok: !rows.iter().any(|r| r.action.is_problem()),
            rows,
            machine: None,
            profile: None,
            state: None,
            items: Vec::new(),
        }
    }
}

/// The keys of the settings file a profile's rules go in, which pastor owns
/// whole.
fn permission_keys() -> Vec<Vec<String>> {
    PROFILE_KEYS
        .iter()
        .map(|k| vec!["permissions".to_string(), (*k).to_string()])
        .collect()
}

/// Place `set` in `dir` on this machine, whose home is `home`, and take out
/// what an earlier version of the set placed that this one no longer has.
/// Other sets in the directory are left as they are.
pub fn place(
    paths: &Paths,
    home: &Path,
    dir: &Path,
    set: &Set,
    dry_run: bool,
) -> anyhow::Result<Placed> {
    place_with(paths, home, dir, set, None, dry_run)
}

/// `place`, with the rendering of the profile the set names (`render`): its
/// permission rules go in the settings file, which pastor owns whole (a rule
/// the agent added since is replaced, and counted in the row's detail as
/// `agent_rules_dropped`), and its flags and env are answered.
pub fn place_with(
    paths: &Paths,
    home: &Path,
    dir: &Path,
    set: &Set,
    render: Option<&Render>,
    dry_run: bool,
) -> anyhow::Result<Placed> {
    match (&set.profile, render) {
        (Some(want), Some(r)) if *want == r.profile && r.kind == set.kind => {}
        (None, None) => {}
        (Some(want), _) => {
            return Err(invalid(format!(
                "profile {want}: it is rendered when the set is placed, with `pastor workdir place`"
            )));
        }
        (None, Some(_)) => return Err(invalid("a profile was rendered for a set that names none")),
    }
    let real = workdir(dir, home)?;
    let (file, skipped) = match set
        .instructions
        .as_ref()
        .map(|_| instruction_file(&real, set))
    {
        None => (None, None),
        Some(Ok(file)) => (Some(file), None),
        Some(Err(why)) => (None, Some(why)),
    };
    // The files the set merges into here, and what they are for: a whole
    // file of the set may not be one of them, nor a file on their way, nor
    // under one.
    let merged = [
        (
            (set.settings.is_some()
                || render.is_some_and(|r| r.kind == "claude" && !r.settings.is_null()))
            .then_some(slots(&set.kind).settings),
            "settings",
        ),
        (file.as_ref().map(Instructions::path), "instructions"),
    ];
    for (merged, what) in merged
        .iter()
        .filter_map(|(f, w)| Some((Path::new((*f)?), w)))
    {
        if let Some((path, ..)) = set.placed().into_iter().find(|(p, ..)| {
            let p = Path::new(p);
            p.starts_with(merged) || merged.starts_with(p)
        }) {
            return Err(invalid(format!(
                "files: {path} and {}, where the set's {what} go here, are in each other's way; place one of them",
                merged.display()
            )));
        }
    }
    let mut items = set.items(&real, home, file.as_ref(), render);
    let ledger = ledger_dir(paths, &real);
    // A path git tracks is not written over: a conflict, not an item.
    let repo = git::find(GIT, &real);
    // The settings file holds an item per key: one path.
    let mut wanted: Vec<PathBuf> = items.iter().map(|i| PathBuf::from(&i.path)).collect();
    wanted.dedup();
    let tracked = git::tracked_new(GIT, &repo, &real, &ledger, &set.id, &wanted);
    items.retain(|i| !tracked.contains(&PathBuf::from(&i.path)));
    let mut rows = apply::place_workdir(
        &ledger,
        &real,
        apply::Scope::Set {
            id: set.id.clone(),
            hash: Some(set.hash()),
            profile: render.map(|r| apply::Profiled {
                name: r.profile.clone(),
                hash: rendered_hash(r),
            }),
        },
        &items,
        !tracked.is_empty(),
        &permission_keys(),
        dry_run,
    )?;
    if let Some(why) = skipped {
        rows.push(Row {
            bundle: set.id.clone(),
            home: None,
            path: "instructions".into(),
            key: None,
            action: Action::Skipped,
            detail: Some(why),
            old: None,
            backup: None,
        });
    }
    if set.kind == "agy" && set.settings.is_some() && !agy_trusts(home, &[dir, &real]) {
        rows.push(Row {
            bundle: set.id.clone(),
            home: None,
            path: "trust".into(),
            key: None,
            action: Action::Skipped,
            detail: Some(format!(
                "hooks need a trusted folder: agy loads .agents/hooks.json in an interactive session only once its trustedWorkspaces lists {} itself",
                real.display()
            )),
            old: None,
            backup: None,
        });
    }
    for p in &tracked {
        let rel = p.strip_prefix(&real).unwrap_or(p);
        rows.push(Row {
            bundle: set.id.clone(),
            home: None,
            path: rel.display().to_string(),
            key: None,
            action: Action::Conflict,
            detail: Some(tracked_detail(&set.kind, rel)),
            old: None,
            backup: None,
        });
    }
    // What a dry run says it would list: what was not refused.
    let refused: Vec<&str> = rows
        .iter()
        .filter(|r| r.action.is_problem())
        .map(|r| r.path.as_str())
        .collect();
    // A shared file that is there already is not pastor's to keep out of
    // git; one pastor makes now is, as the ledger will say.
    let placing: Vec<PathBuf> = wanted
        .into_iter()
        .filter(|p| !tracked.contains(p))
        .filter(|p| !(shared(&real, p) && p.exists()))
        .filter(|p| {
            let rel = p.strip_prefix(&real).unwrap_or(p).display().to_string();
            !refused.contains(&rel.as_str())
        })
        .collect();
    if repo == git::Repo::NoGit && set.git_exclude {
        rows.push(git::not_excluded(&set.id, &real, "git not found"));
    } else {
        rows.extend(excluded(
            paths,
            &ledger,
            &real,
            &repo,
            Some((&set.id, set.git_exclude)),
            &placing,
            dry_run,
        ));
    }
    if !dry_run {
        super::workdir_keep::cap(paths, home, super::workdir_keep::CAP);
    }
    let mut placed = Placed::new(&real, Some(&set.id), dry_run, rows);
    placed.profile = render.map(|r| ProfileOut {
        name: r.profile.clone(),
        flags: r.flags.clone(),
        env: r.env.clone(),
        unsupported: r.unsupported.iter().map(|u| u.pattern.clone()).collect(),
    });
    Ok(placed)
}

/// The SHA-256 the ledger keeps for a profile as rendered.
fn rendered_hash(r: &Render) -> String {
    hex(&Sha256::digest(
        serde_json::to_string(r).unwrap_or_default().as_bytes(),
    ))
}

/// Take out what pastor placed in `dir` for the set `id`, or for every set
/// with none, as long as it still holds what pastor wrote. The directory
/// may be gone already.
pub fn remove(
    paths: &Paths,
    home: &Path,
    dir: &Path,
    id: Option<&str>,
    dry_run: bool,
) -> anyhow::Result<Placed> {
    let real = match dir.symlink_metadata() {
        Ok(_) => workdir(dir, home)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && dir.is_absolute() => gone(dir),
        Err(e) => return Err(invalid(format!("{}: {e}", dir.display()))),
    };
    // Nothing placed there: nothing to write, not even a ledger.
    let ledger = ledger_dir(paths, &real);
    if !ledger.join("ledger.json").exists() {
        return Ok(Placed::new(&real, id, dry_run, Vec::new()));
    }
    let scope = match id {
        Some(id) => apply::Scope::Set {
            id: id.to_string(),
            hash: None,
            profile: None,
        },
        None => apply::Scope::All,
    };
    let mut rows = apply::place_workdir(
        &ledger,
        &real,
        scope,
        &[],
        false,
        &permission_keys(),
        dry_run,
    )?;
    if !dry_run {
        let repo = git::find(GIT, &real);
        rows.extend(excluded(paths, &ledger, &real, &repo, None, &[], false));
    }
    Ok(Placed::new(&real, id, dry_run, rows))
}

/// The `git` the exclusion runs.
const GIT: &str = "git";

/// Keep the placed files out of git (`workdir_git::exclude`). A failure is
/// a row, not an error: the files are placed already.
/// Whether `path` is one of `SHARED_FILES` of `dir`.
fn shared(dir: &Path, path: &Path) -> bool {
    path.strip_prefix(dir)
        .is_ok_and(|rel| SHARED_FILES.iter().any(|f| rel == Path::new(f)))
}

/// Why a path git tracks takes nothing from a set. agy reads its hooks
/// from `.agents/hooks.json` alone, so a repository that commits that file
/// has no local one to use instead.
fn tracked_detail(kind: &str, rel: &Path) -> String {
    if kind == "agy" && rel == Path::new(AGY_SLOTS.settings) {
        return format!(
            "tracked by git: pastor does not write over it, and agy has no local hooks file; put the set's hooks in the committed {} or take them out of the set",
            AGY_SLOTS.settings
        );
    }
    "tracked by git: pastor does not write over it; use the local file the harness reads"
        .to_string()
}

fn excluded(
    paths: &Paths,
    ledger: &Path,
    dir: &Path,
    repo: &git::Repo,
    set: Option<(&str, bool)>,
    placing: &[PathBuf],
    dry_run: bool,
) -> Vec<Row> {
    git::exclude(GIT, paths, ledger, dir, repo, set, placing, dry_run).unwrap_or_else(|e| {
        vec![Row {
            bundle: set.map(|(id, _)| id.to_string()).unwrap_or_default(),
            home: None,
            path: ".git/info/exclude".to_string(),
            key: None,
            action: Action::Failed,
            detail: Some(format!("could not keep the files out of git: {e:#}")),
            old: None,
            backup: None,
        }]
    })
}

/// The real path a directory that is gone had, as `workdir` made it when
/// it was there: `.`, `..` and a trailing `/` taken out, and its nearest
/// ancestor still there resolved, so a link above it leads to the same
/// ledger.
pub fn gone(dir: &Path) -> PathBuf {
    let mut plain = PathBuf::new();
    for c in dir.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                plain.pop();
            }
            other => plain.push(other),
        }
    }
    let mut tail = Vec::new();
    let mut at = plain.as_path();
    loop {
        if let Ok(real) = at.canonicalize() {
            return tail.iter().rev().fold(real, |p, name| p.join(name));
        }
        match (at.parent(), at.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                at = parent;
            }
            _ => return plain,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::apply::Action;
    use std::os::unix::fs::PermissionsExt;

    struct Machine {
        _tmp: tempfile::TempDir,
        paths: Paths,
        home: PathBuf,
        dir: PathBuf,
    }

    impl Machine {
        fn new() -> Machine {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().canonicalize().unwrap();
            let home = root.join("home");
            let dir = home.join("src/app");
            std::fs::create_dir_all(&dir).unwrap();
            let paths = Paths::new(
                home.join(".config/pastor"),
                home.join(".local/state/pastor"),
            );
            Machine {
                _tmp: tmp,
                paths,
                home,
                dir,
            }
        }

        fn place(&self, set: &str) -> Placed {
            self.try_place(set, false).unwrap()
        }

        fn try_place(&self, set: &str, dry_run: bool) -> anyhow::Result<Placed> {
            let set = Set::parse(set, Some("json"))?;
            place(&self.paths, &self.home, &self.dir, &set, dry_run)
        }

        fn remove(&self, id: Option<&str>) -> Placed {
            remove(&self.paths, &self.home, &self.dir, id, false).unwrap()
        }

        fn at(&self, rel: &str) -> PathBuf {
            self.dir.join(rel)
        }

        fn read(&self, rel: &str) -> Option<String> {
            std::fs::read_to_string(self.at(rel)).ok()
        }

        fn ledger(&self) -> serde_json::Value {
            let file = ledger_dir(&self.paths, &self.dir).join("ledger.json");
            serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap()
        }
    }

    fn actions(p: &Placed) -> Vec<(String, Action)> {
        p.rows.iter().map(|r| (r.path.clone(), r.action)).collect()
    }

    fn code(e: &anyhow::Error) -> String {
        e.downcast_ref::<CliError>()
            .map(|c| c.code.clone())
            .unwrap_or_else(|| format!("not a CliError: {e:#}"))
    }

    const REVIEWER: &str = r##"{
        "id": "reviewer",
        "kind": "claude",
        "skills": [{"name": "reviewing-a-pr", "files": {"SKILL.md": "---\nname: reviewing-a-pr\n---\nRead the diff.\n"}}],
        "files": [
            {"path": ".claude/hooks/start.sh", "content": "#!/bin/sh\necho {workdir} {ticket_url}\n", "mode": "0755", "render": true},
            {"path": ".claude/agents/checker.md", "content": "Check {workdir}.\n"}
        ],
        "vars": {"ticket_url": "https://tickets.example.com"}
    }"##;

    /// The first place writes every file; the second finds them all in
    /// place and writes nothing.
    #[test]
    fn place_then_place_again_changes_nothing() {
        let m = Machine::new();
        let first = m.place(REVIEWER);
        assert!(first.ok, "{first:?}");
        assert_eq!(
            actions(&first),
            vec![
                (
                    ".claude/skills/reviewing-a-pr/SKILL.md".to_string(),
                    Action::Added
                ),
                (".claude/hooks/start.sh".to_string(), Action::Added),
                (".claude/agents/checker.md".to_string(), Action::Added),
            ]
        );
        assert_eq!(first.target, "workdir");
        assert_eq!(first.dir, m.dir.display().to_string());
        assert_eq!(first.id.as_deref(), Some("reviewer"));
        assert_eq!(
            m.read(".claude/skills/reviewing-a-pr/SKILL.md").unwrap(),
            "---\nname: reviewing-a-pr\n---\nRead the diff.\n"
        );
        let ledger_before = m.ledger();
        let again = m.place(REVIEWER);
        assert!(
            again.rows.iter().all(|r| r.action == Action::Kept),
            "{again:?}"
        );
        assert_eq!(m.ledger(), ledger_before);
    }

    /// `render` fills pastor's vars and the set's; a file without it is
    /// placed as written. Its mode is the set's.
    #[test]
    fn vars_and_modes() {
        let m = Machine::new();
        m.place(REVIEWER);
        assert_eq!(
            m.read(".claude/hooks/start.sh").unwrap(),
            format!(
                "#!/bin/sh\necho {} https://tickets.example.com\n",
                m.dir.display()
            )
        );
        assert_eq!(
            m.read(".claude/agents/checker.md").unwrap(),
            "Check {workdir}.\n"
        );
        let mode = |rel: &str| m.at(rel).metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(".claude/hooks/start.sh"), 0o755);
        assert_eq!(mode(".claude/agents/checker.md"), 0o644);
    }

    /// A dry run says what it would do and writes nothing, not even a
    /// ledger.
    #[test]
    fn a_dry_run_writes_nothing() {
        let m = Machine::new();
        let p = m.try_place(REVIEWER, true).unwrap();
        assert!(p.dry_run);
        assert!(p.rows.iter().all(|r| r.action == Action::Added), "{p:?}");
        assert!(!m.at(".claude").exists());
        assert!(!ledger_dir(&m.paths, &m.dir).exists());
    }

    /// The ledger is outside the directory, keyed by its real path, and
    /// names it.
    #[test]
    fn the_ledger_is_outside_the_directory_and_names_it() {
        let m = Machine::new();
        m.place(REVIEWER);
        let ledger = m.ledger();
        assert_eq!(ledger["dir"], m.dir.display().to_string());
        assert!(ledger["sets"]["reviewer"]["hash"].is_string(), "{ledger}");
        assert!(ledger_dir(&m.paths, &m.dir).starts_with(m.paths.state_dir.join("workdirs")));
        let names: Vec<String> = std::fs::read_dir(&m.dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![".claude".to_string()]);
    }

    /// A smaller set takes out what it no longer has, and the dirs it made
    /// for it once they are empty; a new content updates a file.
    #[test]
    fn a_smaller_set_takes_out_what_it_dropped() {
        let m = Machine::new();
        m.place(REVIEWER);
        let smaller = r#"{"id": "reviewer", "kind": "claude",
            "files": [{"path": ".claude/agents/checker.md", "content": "Check again.\n"}]}"#;
        let p = m.place(smaller);
        assert!(p.ok, "{p:?}");
        let mut got = actions(&p);
        got.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            got,
            vec![
                (".claude/agents/checker.md".to_string(), Action::Updated),
                (".claude/hooks/start.sh".to_string(), Action::Removed),
                (
                    ".claude/skills/reviewing-a-pr/SKILL.md".to_string(),
                    Action::Removed
                ),
            ]
        );
        assert!(!m.at(".claude/skills").exists());
        assert!(!m.at(".claude/hooks").exists());
        assert_eq!(
            m.read(".claude/agents/checker.md").unwrap(),
            "Check again.\n"
        );
    }

    /// A file changed since pastor wrote it is the user's: a new version
    /// leaves it, and so does a removal.
    #[test]
    fn a_file_changed_by_hand_is_left_alone() {
        let m = Machine::new();
        m.place(REVIEWER);
        std::fs::write(m.at(".claude/agents/checker.md"), "Mine now.\n").unwrap();
        let newer = REVIEWER.replace("Check {workdir}.", "Check twice.");
        let p = m.place(&newer);
        let row = p
            .rows
            .iter()
            .find(|r| r.path == ".claude/agents/checker.md")
            .unwrap();
        assert_eq!(row.action, Action::ChangedByHand, "{p:?}");
        assert!(!p.ok);
        let gone = m.remove(Some("reviewer"));
        assert!(
            gone.rows
                .iter()
                .any(|r| r.path == ".claude/agents/checker.md" && r.action == Action::ChangedByHand),
            "{gone:?}"
        );
        assert_eq!(m.read(".claude/agents/checker.md").unwrap(), "Mine now.\n");
        assert!(!m.at(".claude/hooks/start.sh").exists());
    }

    /// Something pastor did not write where a file goes is a conflict, and
    /// stays.
    #[test]
    fn something_else_in_a_files_place_is_a_conflict() {
        let m = Machine::new();
        std::fs::create_dir_all(m.at(".claude/agents")).unwrap();
        std::fs::write(m.at(".claude/agents/checker.md"), "The repo's.\n").unwrap();
        let p = m.place(REVIEWER);
        assert!(!p.ok);
        assert_eq!(
            p.rows
                .iter()
                .find(|r| r.path == ".claude/agents/checker.md")
                .unwrap()
                .action,
            Action::Conflict
        );
        assert_eq!(
            m.read(".claude/agents/checker.md").unwrap(),
            "The repo's.\n"
        );
    }

    /// A file already there as the set has it is not pastor's: it is kept,
    /// and stays when the set drops it.
    #[test]
    fn a_file_already_there_is_not_taken() {
        let m = Machine::new();
        std::fs::create_dir_all(m.at(".claude/agents")).unwrap();
        std::fs::write(m.at(".claude/agents/checker.md"), "Check {workdir}.\n").unwrap();
        let p = m.place(REVIEWER);
        assert!(p.ok, "{p:?}");
        let gone = m.remove(None);
        assert!(
            gone.rows
                .iter()
                .all(|r| r.path != ".claude/agents/checker.md"),
            "{gone:?}"
        );
        assert_eq!(
            m.read(".claude/agents/checker.md").unwrap(),
            "Check {workdir}.\n"
        );
    }

    /// The same file there already with another mode than the set's is a
    /// conflict: pastor does not change it, and as it is a hook may not run.
    #[test]
    fn a_file_already_there_with_another_mode_is_a_conflict() {
        let m = Machine::new();
        std::fs::create_dir_all(m.at(".claude/hooks")).unwrap();
        let hook = m.at(".claude/hooks/start.sh");
        std::fs::write(
            &hook,
            format!(
                "#!/bin/sh\necho {} https://tickets.example.com\n",
                m.dir.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o644)).unwrap();
        let p = m.place(REVIEWER);
        let row = p
            .rows
            .iter()
            .find(|r| r.path == ".claude/hooks/start.sh")
            .unwrap();
        assert_eq!(row.action, Action::Conflict, "{p:?}");
        assert!(row.detail.as_deref().unwrap().contains("0644"), "{row:?}");
        assert_eq!(hook.metadata().unwrap().permissions().mode() & 0o777, 0o644);
    }

    /// Pastor never writes through a symlink: a dir on a file's way, or
    /// the file itself.
    #[test]
    fn a_symlink_is_not_written_through() {
        let m = Machine::new();
        let elsewhere = m.home.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::create_dir_all(m.at(".claude")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, m.at(".claude/hooks")).unwrap();
        std::fs::create_dir_all(m.at(".claude/agents")).unwrap();
        std::fs::write(elsewhere.join("target.md"), "theirs\n").unwrap();
        std::os::unix::fs::symlink(
            elsewhere.join("target.md"),
            m.at(".claude/agents/checker.md"),
        )
        .unwrap();
        let p = m.place(REVIEWER);
        let linked: Vec<&str> = p
            .rows
            .iter()
            .filter(|r| r.action == Action::Linked)
            .map(|r| r.path.as_str())
            .collect();
        assert_eq!(
            linked,
            vec![".claude/hooks", ".claude/agents/checker.md"],
            "{p:?}"
        );
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 1);
        assert_eq!(
            std::fs::read_to_string(elsewhere.join("target.md")).unwrap(),
            "theirs\n"
        );
    }

    /// The directory itself may not be a symlink, nor the home; a dir
    /// above it may be a link, and the ledger names the real path.
    #[test]
    fn the_directory_is_checked() {
        let m = Machine::new();
        let link = m.home.join("app-link");
        std::os::unix::fs::symlink(&m.dir, &link).unwrap();
        let set = Set::parse(REVIEWER, None).unwrap();
        let e = place(&m.paths, &m.home, &link, &set, false).unwrap_err();
        assert_eq!(code(&e), WORKDIR_INVALID);
        let e = place(&m.paths, &m.home, &m.home, &set, false).unwrap_err();
        assert_eq!(code(&e), WORKDIR_INVALID);
        let e = place(&m.paths, &m.home, &m.home.join("nope"), &set, false).unwrap_err();
        assert_eq!(code(&e), WORKDIR_INVALID);
        let src = m.home.join("src-link");
        std::os::unix::fs::symlink(m.home.join("src"), &src).unwrap();
        let p = place(&m.paths, &m.home, &src.join("app"), &set, false).unwrap();
        assert_eq!(p.dir, m.dir.display().to_string());
        assert_eq!(m.ledger()["dir"], m.dir.display().to_string());
    }

    /// The ledger records a set while it holds something of it: not one
    /// whose only file was refused, and still one whose file a removal
    /// had to leave.
    #[test]
    fn a_set_is_recorded_while_the_ledger_holds_it() {
        let m = Machine::new();
        std::fs::write(m.at("x.md"), "theirs\n").unwrap();
        let p = m.place(
            r#"{"id": "solo", "kind": "claude", "files": [{"path": "x.md", "content": "ours\n"}]}"#,
        );
        assert!(!p.ok);
        assert!(m.ledger()["sets"].get("solo").is_none(), "{}", m.ledger());
        m.place(REVIEWER);
        let elsewhere = m.home.join("elsewhere");
        std::fs::rename(m.at(".claude/hooks"), &elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, m.at(".claude/hooks")).unwrap();
        let gone = m.remove(Some("reviewer"));
        assert!(
            gone.rows.iter().any(|r| r.action == Action::Linked),
            "{gone:?}"
        );
        assert!(m.ledger()["sets"]["reviewer"].is_object(), "{}", m.ledger());
    }

    /// Two sets share a directory: each is placed and removed alone.
    #[test]
    fn two_sets_in_one_directory_are_kept_apart() {
        let m = Machine::new();
        m.place(REVIEWER);
        let other = r#"{"id": "notes", "kind": "claude",
            "files": [{"path": ".claude/commands/note.md", "content": "Take a note.\n"}]}"#;
        let p = m.place(other);
        assert_eq!(
            actions(&p),
            vec![(".claude/commands/note.md".to_string(), Action::Added)]
        );
        let again = m.place(REVIEWER);
        assert!(
            again.rows.iter().all(|r| r.action == Action::Kept),
            "{again:?}"
        );
        let gone = m.remove(Some("reviewer"));
        assert!(
            gone.rows.iter().all(|r| r.action == Action::Removed),
            "{gone:?}"
        );
        assert_eq!(
            m.read(".claude/commands/note.md").unwrap(),
            "Take a note.\n"
        );
        let ledger = m.ledger();
        assert!(ledger["sets"].get("reviewer").is_none(), "{ledger}");
        assert!(ledger["sets"]["notes"].is_object(), "{ledger}");
        let all = m.remove(None);
        assert_eq!(
            actions(&all),
            vec![(".claude/commands/note.md".to_string(), Action::Removed)]
        );
        assert!(!m.at(".claude").exists());
    }

    /// A directory that is gone is found by the path it was placed by, or
    /// one through a link above it, with `.` or a trailing `/`.
    #[test]
    fn a_gone_directory_is_found_through_a_link_above_it() {
        let m = Machine::new();
        m.place(REVIEWER);
        std::fs::remove_dir_all(&m.dir).unwrap();
        let link = m.home.join("src-link");
        std::os::unix::fs::symlink(m.home.join("src"), &link).unwrap();
        let by = PathBuf::from(format!("{}/./x/../app/", link.display()));
        let p = remove(&m.paths, &m.home, &by, None, false).unwrap();
        assert_eq!(p.dir, m.dir.display().to_string());
        assert!(
            !p.rows.is_empty() && p.rows.iter().all(|r| r.action == Action::Removed),
            "{p:?}"
        );
    }

    /// A removal where pastor placed nothing writes nothing: no ledger, no
    /// lock.
    #[test]
    fn a_removal_where_nothing_was_placed_writes_nothing() {
        let m = Machine::new();
        let p = m.remove(None);
        assert!(p.ok && p.rows.is_empty(), "{p:?}");
        let typo = m.home.join("src/nope");
        let p = remove(&m.paths, &m.home, &typo, None, false).unwrap();
        assert!(p.rows.is_empty(), "{p:?}");
        assert!(!m.paths.state_dir.join(LEDGERS).exists());
    }

    /// A removal for a directory that is gone drops its ledger entries.
    #[test]
    fn a_gone_directory_can_be_removed() {
        let m = Machine::new();
        m.place(REVIEWER);
        std::fs::remove_dir_all(&m.dir).unwrap();
        let p = m.remove(None);
        assert!(p.ok, "{p:?}");
        assert!(p.rows.iter().all(|r| r.action == Action::Removed));
        assert_eq!(m.ledger()["entries"], serde_json::json!([]));
    }

    const SETTINGS: &str = ".claude/settings.local.json";

    const WITH_SETTINGS: &str = r##"{
        "id": "reviewer",
        "kind": "claude",
        "settings": {
            "hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "{workdir}/.claude/hooks/start.sh"}]}]},
            "env": {"TICKET_URL": "{ticket_url}", "TOOLS": "{bin}/tools"},
            "permissions": {"additionalDirectories": ["{home}/notes"]},
            "model": "opus"
        },
        "instructions": "You review pull requests in {workdir}.\n",
        "vars": {"ticket_url": "https://tickets.example.com"}
    }"##;

    fn json_at(m: &Machine, rel: &str) -> serde_json::Value {
        serde_json::from_str(&m.read(rel).unwrap()).unwrap()
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    /// `settings` merge into the local settings file by the home rules: a
    /// hook group is an entry in its event's list, `env` and `permissions`
    /// go key by key, a directory is an entry of `additionalDirectories`,
    /// and any other key whole. The user's keys stay, and pastor's own
    /// vars and the set's are filled, none of the home's.
    #[test]
    fn settings_merge_into_the_local_settings_file() {
        let m = Machine::new();
        std::fs::create_dir_all(m.at(".claude")).unwrap();
        let mine = serde_json::json!({
            "permissions": {"allow": ["Bash(ls:*)"], "additionalDirectories": ["/srv/data"]},
            "env": {"MINE": "1"}
        });
        std::fs::write(m.at(SETTINGS), format!("{mine:#}\n")).unwrap();
        let p = m.place(WITH_SETTINGS);
        assert!(p.ok, "{p:?}");
        let mut keys: Vec<(String, String, Action)> = p
            .rows
            .iter()
            .filter(|r| r.path == SETTINGS)
            .map(|r| (r.path.clone(), r.key.clone().unwrap(), r.action))
            .collect();
        keys.sort_by(|a, b| a.1.cmp(&b.1));
        let want = |k: &str| (SETTINGS.to_string(), k.to_string(), Action::Added);
        assert_eq!(
            keys,
            vec![
                want("env.TICKET_URL"),
                want("env.TOOLS"),
                want("hooks.SessionStart"),
                want("model"),
                want("permissions.additionalDirectories"),
            ]
        );
        let got = json_at(&m, SETTINGS);
        let dir = m.dir.display().to_string();
        assert_eq!(
            got,
            serde_json::json!({
                "permissions": {
                    "allow": ["Bash(ls:*)"],
                    "additionalDirectories": ["/srv/data", format!("{}/notes", m.home.display())]
                },
                "env": {"MINE": "1", "TICKET_URL": "https://tickets.example.com", "TOOLS": "{bin}/tools"},
                "hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": format!("{dir}/.claude/hooks/start.sh")}]}]},
                "model": "opus"
            })
        );
        let again = m.place(WITH_SETTINGS);
        assert!(
            again.rows.iter().all(|r| r.action == Action::Kept),
            "{again:?}"
        );
        let gone = m.remove(Some("reviewer"));
        assert!(gone.ok, "{gone:?}");
        assert_eq!(json_at(&m, SETTINGS), mine);
    }

    /// What Claude writes in the file itself, such as a rule the user
    /// allowed for good, is its own: the next place keeps it and is all
    /// `kept`.
    #[test]
    fn what_claude_adds_to_the_settings_stays() {
        let m = Machine::new();
        m.place(WITH_SETTINGS);
        let mut now = json_at(&m, SETTINGS);
        now["permissions"]["allow"] = serde_json::json!(["Bash(make check)"]);
        now["permissions"]["additionalDirectories"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!("/srv/more"));
        std::fs::write(m.at(SETTINGS), format!("{now:#}\n")).unwrap();
        let again = m.place(WITH_SETTINGS);
        assert!(
            again.rows.iter().all(|r| r.action == Action::Kept),
            "{again:?}"
        );
        assert_eq!(json_at(&m, SETTINGS), now);
        m.remove(None);
        assert_eq!(
            json_at(&m, SETTINGS),
            serde_json::json!({"permissions": {"allow": ["Bash(make check)"], "additionalDirectories": ["/srv/more"]}})
        );
    }

    /// A settings file pastor made goes once its last key is out.
    #[test]
    fn a_settings_file_pastor_made_goes_with_the_set() {
        let m = Machine::new();
        m.place(WITH_SETTINGS);
        let p = m.remove(Some("reviewer"));
        assert!(p.ok, "{p:?}");
        assert!(!m.at(SETTINGS).exists());
        assert!(!m.at("CLAUDE.local.md").exists());
        assert!(!m.at(".claude").exists());
    }

    /// `instructions` are a marked block in `CLAUDE.local.md`, after the
    /// text already there, which is never touched; vars are filled.
    #[test]
    fn instructions_are_a_block_in_claude_local_md() {
        let m = Machine::new();
        std::fs::write(m.at("CLAUDE.local.md"), "My own notes.\n").unwrap();
        let p = m.place(WITH_SETTINGS);
        assert!(
            p.rows
                .iter()
                .any(|r| r.path == "CLAUDE.local.md" && r.action == Action::Added),
            "{p:?}"
        );
        assert_eq!(
            m.read("CLAUDE.local.md").unwrap(),
            format!(
                "My own notes.\n\n<!-- pastor:workdir:reviewer -->\nYou review pull requests in {}.\n<!-- /pastor:workdir:reviewer -->\n",
                m.dir.display()
            )
        );
        let newer = WITH_SETTINGS.replace("You review", "You read");
        let p = m.place(&newer);
        assert!(
            p.rows
                .iter()
                .any(|r| r.path == "CLAUDE.local.md" && r.action == Action::Updated),
            "{p:?}"
        );
        let without = r#"{"id": "reviewer", "kind": "claude"}"#;
        let p = m.place(without);
        assert!(
            p.rows
                .iter()
                .any(|r| r.path == "CLAUDE.local.md" && r.action == Action::Removed),
            "{p:?}"
        );
        assert_eq!(m.read("CLAUDE.local.md").unwrap(), "My own notes.\n");
    }

    /// A block someone edited is theirs: a new version leaves it.
    #[test]
    fn an_edited_block_is_left() {
        let m = Machine::new();
        m.place(WITH_SETTINGS);
        let text = m.read("CLAUDE.local.md").unwrap();
        std::fs::write(
            m.at("CLAUDE.local.md"),
            text.replace("You review", "You approve"),
        )
        .unwrap();
        let p = m.place(&WITH_SETTINGS.replace("You review", "You read"));
        let row = p.rows.iter().find(|r| r.path == "CLAUDE.local.md").unwrap();
        assert_eq!(row.action, Action::ChangedByHand, "{p:?}");
    }

    /// A set's document reads back to the same set, every part kept: what
    /// the head sends a machine and keeps to check by.
    #[test]
    fn a_set_reads_back_from_its_document() {
        let text = r##"{
            "id": "reviewer",
            "kind": "claude",
            "profile": "review",
            "settings": {"env": {"TICKET_URL": "{ticket_url}"}},
            "instructions": "Review {workdir}.\n",
            "skills": [{"name": "reviewing-a-pr", "files": {"SKILL.md": "Read.\n", "notes.md": "More.\n"}}],
            "files": [
                {"path": ".claude/hooks/start.sh", "content": "#!/bin/sh\n", "mode": "0755", "render": true},
                {"path": ".claude/agents/checker.md", "content": "Check.\n"}
            ],
            "vars": {"ticket_url": "https://tickets.example.com"},
            "git_exclude": false
        }"##;
        let set = Set::parse(text, Some("json")).unwrap();
        let again = Set::parse(&set.to_doc().to_string(), Some("json")).unwrap();
        assert_eq!(again, set);
        assert_eq!(again.hash(), set.hash());
        assert_eq!(set.to_doc()["files"][0]["mode"], "0755");
        let bare = Set::parse(r#"{"id": "a", "kind": "claude"}"#, None).unwrap();
        assert_eq!(Set::parse(&bare.to_doc().to_string(), None).unwrap(), bare);
    }

    /// When the repository tracks `CLAUDE.local.md`, the block goes in
    /// `.claude/CLAUDE.md`; when it tracks both, the block is skipped. A
    /// set may not place `.claude/CLAUDE.md` whole and its block in it.
    #[test]
    fn instructions_fall_back_when_the_repo_tracks_claude_local_md() {
        let m = Machine::new();
        git(&m.dir, &["init", "-q"]);
        std::fs::write(m.at("CLAUDE.local.md"), "Shared.\n").unwrap();
        git(&m.dir, &["add", "-f", "CLAUDE.local.md"]);
        let p = m.place(WITH_SETTINGS);
        assert!(p.ok, "{p:?}");
        assert!(
            p.rows
                .iter()
                .any(|r| r.path == ".claude/CLAUDE.md" && r.action == Action::Added),
            "{p:?}"
        );
        assert_eq!(m.read("CLAUDE.local.md").unwrap(), "Shared.\n");
        assert!(
            m.read(".claude/CLAUDE.md")
                .unwrap()
                .starts_with("<!-- pastor:workdir:reviewer -->\n")
        );
        let whole = WITH_SETTINGS.replace(
            r#""vars""#,
            r#""files": [{"path": ".claude/CLAUDE.md", "content": "x"}], "vars""#,
        );
        let e = m.try_place(&whole, false).unwrap_err();
        assert_eq!(code(&e), WORKDIR_INVALID, "{e:#}");
        // A file where the settings file's directory goes.
        let in_the_way = WITH_SETTINGS.replace(
            r#""vars""#,
            r#""files": [{"path": ".claude", "content": "x"}], "vars""#,
        );
        let e = m.try_place(&in_the_way, false).unwrap_err();
        assert_eq!(code(&e), WORKDIR_INVALID, "{e:#}");
        assert!(format!("{e:#}").contains("settings"), "{e:#}");
        // The repository takes the file over: pastor takes its block out
        // and skips it, and its own text stays.
        std::fs::remove_file(m.at(".claude/CLAUDE.md")).unwrap();
        std::fs::write(m.at(".claude/CLAUDE.md"), "Team rules.\n").unwrap();
        git(&m.dir, &["add", "-f", ".claude/CLAUDE.md"]);
        let p = m.place(WITH_SETTINGS);
        assert!(p.ok, "{p:?}");
        let row = p
            .rows
            .iter()
            .find(|r| r.action == Action::Skipped)
            .expect("a skipped row");
        assert_eq!(row.path, "instructions", "{p:?}");
        assert!(row.detail.as_deref().unwrap().contains("tracks"), "{row:?}");
        assert_eq!(m.read(".claude/CLAUDE.md").unwrap(), "Team rules.\n");
    }

    /// Two sets share a directory: each has its block, and keys of its
    /// own in the settings; a hook group or a directory both add stays
    /// until the last of them goes.
    #[test]
    fn two_sets_share_the_settings_and_instruction_files() {
        let m = Machine::new();
        m.place(WITH_SETTINGS);
        let notes = r#"{"id": "notes", "kind": "claude",
            "settings": {"env": {"NOTES": "1"},
                         "permissions": {"additionalDirectories": ["{home}/notes"]}},
            "instructions": "Take notes."}"#;
        let p = m.place(notes);
        assert!(p.ok, "{p:?}");
        let text = m.read("CLAUDE.local.md").unwrap();
        assert!(
            text.contains("<!-- pastor:workdir:reviewer -->")
                && text.contains("<!-- pastor:workdir:notes -->\nTake notes.\n"),
            "{text}"
        );
        m.remove(Some("reviewer"));
        let got = json_at(&m, SETTINGS);
        assert_eq!(
            got,
            serde_json::json!({
                "env": {"NOTES": "1"},
                "permissions": {"additionalDirectories": [format!("{}/notes", m.home.display())]}
            })
        );
        assert_eq!(
            m.read("CLAUDE.local.md").unwrap(),
            "\n<!-- pastor:workdir:notes -->\nTake notes.\n<!-- /pastor:workdir:notes -->\n"
        );
        m.remove(Some("notes"));
        assert!(!m.at(SETTINGS).exists());
        assert!(!m.at("CLAUDE.local.md").exists());
    }

    /// Removing every set at once takes out the files pastor made for
    /// them, though the set that made each leaves first.
    #[test]
    fn removing_every_set_takes_out_the_files_pastor_made() {
        let m = Machine::new();
        m.place(WITH_SETTINGS);
        m.place(
            r#"{"id": "notes", "kind": "claude", "settings": {"env": {"NOTES": "1"}},
                "instructions": "Take notes."}"#,
        );
        let p = m.remove(None);
        assert!(p.ok, "{p:?}");
        assert!(!m.at("CLAUDE.local.md").exists(), "{p:?}");
        assert!(!m.at(".claude").exists(), "{p:?}");
    }

    /// Two sets that set one key, or place one file, clash: the second is
    /// refused, naming both, and writes nothing.
    #[test]
    fn two_sets_that_want_one_place_clash() {
        let m = Machine::new();
        m.place(WITH_SETTINGS);
        let before = m.read(SETTINGS).unwrap();
        for (other, names) in [
            (
                r#"{"id": "notes", "kind": "claude", "settings": {"env": {"TICKET_URL": "x"}}}"#,
                "env.TICKET_URL",
            ),
            (
                r#"{"id": "notes", "kind": "claude", "settings": {"model": "opus"}}"#,
                "model",
            ),
            (
                r#"{"id": "notes", "kind": "claude", "settings": {"env": "x"}}"#,
                "env",
            ),
        ] {
            let e = m.try_place(other, false).unwrap_err();
            assert_eq!(code(&e), WORKDIR_CONFLICT, "{other}: {e:#}");
            let msg = format!("{e:#}");
            assert!(
                msg.contains("reviewer") && msg.contains("notes") && msg.contains(names),
                "{msg}"
            );
            let e = m.try_place(other, true).unwrap_err();
            assert_eq!(code(&e), WORKDIR_CONFLICT, "dry run: {e:#}");
        }
        assert_eq!(m.read(SETTINGS).unwrap(), before);
        m.place(REVIEWER.replace("reviewer\"", "hooks\"").as_str());
        let e = m
            .try_place(
                r#"{"id": "notes", "kind": "claude", "files": [{"path": ".claude/agents/checker.md", "content": "Check {workdir}.\n"}]}"#,
                false,
            )
            .unwrap_err();
        assert_eq!(code(&e), WORKDIR_CONFLICT, "{e:#}");
        assert!(format!("{e:#}").contains("hooks"), "{e:#}");
        // A file where another set's file is a directory, and the other way.
        for path in [".claude/agents", ".claude/agents/checker.md/x"] {
            let e = m
                .try_place(
                    &format!(
                        r#"{{"id": "notes", "kind": "claude", "files": [{{"path": "{path}", "content": "x"}}]}}"#
                    ),
                    false,
                )
                .unwrap_err();
            assert_eq!(code(&e), WORKDIR_CONFLICT, "{path}: {e:#}");
            assert!(
                !m.at(".claude/agents/checker.md/x").exists(),
                "nothing written"
            );
        }
    }

    /// The forms a set comes in: JSON, TOML, and a directory whose
    /// manifest points at files beside it.
    #[test]
    fn a_set_from_json_toml_or_a_directory() {
        let json = Set::parse(REVIEWER, None).unwrap();
        let toml = Set::parse(
            r##"
id = "reviewer"
kind = "claude"
[vars]
ticket_url = "https://tickets.example.com"
[[skills]]
name = "reviewing-a-pr"
files = { "SKILL.md" = "---\nname: reviewing-a-pr\n---\nRead the diff.\n" }
[[files]]
path = ".claude/hooks/start.sh"
content = "#!/bin/sh\necho {workdir} {ticket_url}\n"
mode = "0755"
render = true
[[files]]
path = ".claude/agents/checker.md"
content = "Check {workdir}.\n"
"##,
            None,
        )
        .unwrap();
        assert_eq!(toml, json);
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        std::fs::create_dir_all(d.join("skills/reviewing-a-pr")).unwrap();
        std::fs::write(
            d.join("skills/reviewing-a-pr/SKILL.md"),
            "---\nname: reviewing-a-pr\n---\nRead the diff.\n",
        )
        .unwrap();
        std::fs::write(
            d.join("start.sh"),
            "#!/bin/sh\necho {workdir} {ticket_url}\n",
        )
        .unwrap();
        std::fs::write(d.join("checker.md"), "Check {workdir}.\n").unwrap();
        std::fs::write(
            d.join(SET_FILE),
            r#"
id = "reviewer"
kind = "claude"
vars = { ticket_url = "https://tickets.example.com" }
skills = [{ name = "reviewing-a-pr", from = "skills/reviewing-a-pr" }]
files = [
  { path = ".claude/hooks/start.sh", from = "start.sh", mode = "0755", render = true },
  { path = ".claude/agents/checker.md", from = "checker.md" },
]
"#,
        )
        .unwrap();
        let dir = Set::load(&d.display().to_string()).unwrap();
        assert_eq!(dir, json);
        assert_eq!(dir.hash(), json.hash());
        std::fs::write(d.join("set.json"), REVIEWER).unwrap();
        assert_eq!(
            Set::load(&d.join("set.json").display().to_string()).unwrap(),
            json
        );
    }

    /// `from` stays inside the set's directory.
    #[test]
    fn from_cannot_leave_the_set() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("set");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(tmp.path().join("outside.md"), "x\n").unwrap();
        std::os::unix::fs::symlink(tmp.path().join("outside.md"), d.join("in.md")).unwrap();
        std::fs::create_dir_all(d.join("real")).unwrap();
        std::fs::write(d.join("real/x.md"), "x\n").unwrap();
        std::os::unix::fs::symlink(d.join("real"), d.join("via")).unwrap();
        for from in ["../outside.md", "in.md", "via/x.md"] {
            std::fs::write(
                d.join(SET_FILE),
                format!(
                    "id = \"a\"\nkind = \"claude\"\nfiles = [{{ path = \"x.md\", from = \"{from}\" }}]\n"
                ),
            )
            .unwrap();
            let e = Set::load(&d.display().to_string()).unwrap_err();
            assert_eq!(code(&e), WORKDIR_INVALID, "{from}: {e:#}");
        }
        let e = Set::parse(
            r#"{"id": "a", "kind": "claude", "files": [{"path": "x.md", "from": "y.md"}]}"#,
            None,
        )
        .unwrap_err();
        assert_eq!(code(&e), WORKDIR_INVALID);
    }

    /// A set's manifest is a plain file in its directory, and what it
    /// points at is a regular file, within the set's 4 MiB in all.
    #[test]
    fn a_directory_set_reads_only_plain_files_within_its_size() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("set");
        std::fs::create_dir_all(&d).unwrap();
        let load = || Set::load(&d.display().to_string());
        std::fs::write(
            tmp.path().join("elsewhere.toml"),
            "id = \"a\"\nkind = \"claude\"\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(tmp.path().join("elsewhere.toml"), d.join(SET_FILE)).unwrap();
        assert_eq!(code(&load().unwrap_err()), WORKDIR_INVALID);
        std::fs::remove_file(d.join(SET_FILE)).unwrap();
        let fifo = std::ffi::CString::new(d.join("pipe").display().to_string()).unwrap();
        // SAFETY: a plain mkfifo of a path this test owns.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        std::fs::write(
            d.join(SET_FILE),
            "id = \"a\"\nkind = \"claude\"\nfiles = [{ path = \"x\", from = \"pipe\" }]\n",
        )
        .unwrap();
        let e = load().unwrap_err();
        assert!(format!("{e:#}").contains("not a regular file"), "{e:#}");
        let half = "x".repeat(MAX_SET / 2 + 1);
        std::fs::write(d.join("a.md"), &half).unwrap();
        std::fs::write(d.join("b.md"), &half).unwrap();
        std::fs::write(
            d.join(SET_FILE),
            "id = \"a\"\nkind = \"claude\"\nfiles = [{ path = \"a\", from = \"a.md\" }, { path = \"b\", from = \"b.md\" }]\n",
        )
        .unwrap();
        let e = load().unwrap_err();
        assert!(
            format!("{e:#}").contains("b.md: the set is over 4 MiB"),
            "{e:#}"
        );
    }

    /// What a set is refused for, with its code.
    #[test]
    fn a_set_is_checked() {
        let file = |path: &str| {
            format!(
                r#"{{"id": "a", "kind": "claude", "files": [{{"path": "{path}", "content": "x"}}]}}"#
            )
        };
        let cases: Vec<(String, &str)> = vec![
            (r#"{"id": "A b", "kind": "claude"}"#.into(), WORKDIR_INVALID),
            (r#"{"id": "a", "kind": "vim"}"#.into(), WORKDIR_INVALID),
            (r#"{"id": "a", "kind": "claude", "extra": 1}"#.into(), WORKDIR_INVALID),
            (r#"{"id": "a", "kind": "codex"}"#.into(), WORKDIR_UNSUPPORTED),
            (r#"{"id": "a", "kind": "opencode"}"#.into(), WORKDIR_UNSUPPORTED),
            (r#"{"id": "a", "kind": "claude", "profile": " "}"#.into(), WORKDIR_INVALID),
            (r#"{"id": "a", "kind": "claude", "settings": ["x"]}"#.into(), WORKDIR_INVALID),
            (r#"{"id": "a", "kind": "claude", "settings": {"permissions": "x"}}"#.into(), WORKDIR_INVALID),
            (r#"{"id": "a", "kind": "claude", "settings": {"permissions": {"additionalDirectories": "x"}}}"#.into(), WORKDIR_INVALID),
            (r#"{"id": "a", "kind": "claude", "settings": {"permissions": {"allow": ["Bash"]}}}"#.into(), WORKDIR_PERMISSIONS_OWNED),
            (r#"{"id": "a", "kind": "claude", "settings": {"permissions": {"deny": []}}}"#.into(), WORKDIR_PERMISSIONS_OWNED),
            (r#"{"id": "a", "kind": "claude", "settings": {"permissions": {"ask": []}}}"#.into(), WORKDIR_PERMISSIONS_OWNED),
            (r#"{"id": "a", "kind": "claude", "settings": {"permissions": {"defaultMode": "dontAsk"}}}"#.into(), WORKDIR_PERMISSIONS_OWNED),
            (r#"{"id": "a", "kind": "claude", "instructions": "one\n<!-- /pastor:workdir:a -->\ntwo"}"#.into(), WORKDIR_INVALID),
            (r#"{"id": "a", "kind": "claude", "instructions": "<!-- pastor:bundle:b -->"}"#.into(), WORKDIR_INVALID),
            (r#"{"id": "a", "kind": "claude", "vars": {"m": "<!-- /pastor:workdir:a -->"}, "instructions": "one\n{m}\ntwo"}"#.into(), WORKDIR_INVALID),
            (
                r#"{"id": "a", "kind": "claude", "settings": {"env": {"API_TOKEN": "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6a7b8"}}}"#.into(),
                super::super::BUNDLE_SECRET,
            ),
            (
                r#"{"id": "a", "kind": "claude", "instructions": "use AKIAABCDEFGHIJKLMNOP"}"#.into(),
                super::super::BUNDLE_SECRET,
            ),
            (r#"{"id": "a", "kind": "claude", "mcp": [{"name": "x"}]}"#.into(), WORKDIR_MCP_UNSUPPORTED),
            (r#"{"id": "a", "kind": "claude", "settings": {"mcpServers": {}}}"#.into(), WORKDIR_MCP_UNSUPPORTED),
            (r#"{"id": "a", "kind": "claude", "vars": {"workdir": "x"}}"#.into(), WORKDIR_INVALID),
            (r#"{"id": "a", "kind": "claude", "vars": {"Big": "x"}}"#.into(), WORKDIR_INVALID),
            (r#"{"id": "a", "kind": "claude", "skills": [{"name": "s", "files": {"other.md": "x"}}]}"#.into(), WORKDIR_INVALID),
            (r#"{"id": "a", "kind": "claude", "skills": [{"name": "s", "files": {"../SKILL.md": "x"}}]}"#.into(), WORKDIR_INVALID),
            (r#"{"id": "a", "kind": "claude", "files": [{"path": "x", "content": "x", "mode": "4755"}]}"#.into(), WORKDIR_INVALID),
            (r#"{"id": "a", "kind": "claude", "files": [{"path": "x", "content": "x", "mode": "0200"}]}"#.into(), WORKDIR_INVALID),
            (r#"{"id": "a", "kind": "claude", "files": [{"path": "x"}]}"#.into(), WORKDIR_INVALID),
            (r#"{"id": "a", "kind": "claude", "files": [{"path": "x", "content": "1"}, {"path": "x", "content": "2"}]}"#.into(), WORKDIR_INVALID),
            (r#"{"id": "a", "kind": "claude", "files": [{"path": "x", "content": "1"}, {"path": "x/y", "content": "2"}]}"#.into(), WORKDIR_INVALID),
            (file("../x"), WORKDIR_INVALID),
            (file("/etc/x"), WORKDIR_INVALID),
            (file("./x"), WORKDIR_INVALID),
            (file("a/../x"), WORKDIR_INVALID),
            (file(".git/hooks/pre-commit"), WORKDIR_INVALID),
            (file("sub/.git/config"), WORKDIR_INVALID),
            (file(".claude/settings.json"), WORKDIR_INVALID),
            (file(".claude/settings.local.json"), WORKDIR_INVALID),
            (file(".mcp.json"), WORKDIR_INVALID),
            (file("CLAUDE.local.md"), WORKDIR_INVALID),
            (
                r#"{"id": "a", "kind": "claude", "files": [{"path": "x", "content": "key: AKIAABCDEFGHIJKLMNOP"}]}"#.into(),
                super::super::BUNDLE_SECRET,
            ),
        ];
        for (set, want) in cases {
            let e = Set::parse(&set, None).unwrap_err();
            assert_eq!(code(&e), want, "{set}: {e:#}");
        }
        let big = format!(
            r#"{{"id": "a", "kind": "claude", "files": [{{"path": "x", "content": "{}"}}]}}"#,
            "x".repeat(MAX_SET)
        );
        assert_eq!(code(&Set::parse(&big, None).unwrap_err()), WORKDIR_INVALID);
    }

    /// A set read as a document is measured by what it places, as a
    /// directory set is, not by its text: escaping makes a set under the
    /// cap longer than it as JSON.
    #[test]
    fn a_document_is_measured_by_its_content() {
        let doc = serde_json::json!({
            "id": "a",
            "kind": "claude",
            "files": [{"path": "x", "content": "\n".repeat(MAX_SET - 64)}],
        });
        assert!(doc.to_string().len() > MAX_SET, "the text is over the cap");
        let set = Set::from_doc(&doc).unwrap();
        assert_eq!(Set::from_doc(&set.to_doc()).unwrap(), set);
        let over = serde_json::json!({
            "id": "a",
            "kind": "claude",
            "files": [{"path": "x", "content": "x".repeat(MAX_SET + 1)}],
        });
        assert_eq!(code(&Set::from_doc(&over).unwrap_err()), WORKDIR_INVALID);
        let bad = serde_json::json!({"id": "a", "kind": "claude", "nope": 1});
        assert_eq!(code(&Set::from_doc(&bad).unwrap_err()), WORKDIR_INVALID);
    }

    /// A var is checked as a name and a value, and a rendered file as it
    /// will be written; a var's name is not taken for a file's.
    #[test]
    fn secrets_in_vars_and_rendered_files_are_refused() {
        let with = |vars: &str, content: &str| {
            Set::parse(
                &format!(
                    r#"{{"id": "a", "kind": "claude", "vars": {vars},
                        "files": [{{"path": "x.sh", "content": "{content}", "render": true}}]}}"#
                ),
                None,
            )
        };
        let e = with(
            r#"{"api_token": "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6a7b8"}"#,
            "token is {api_token}",
        )
        .unwrap_err();
        assert_eq!(code(&e), super::super::BUNDLE_SECRET, "{e:#}");
        let msg = format!("{e:#}");
        assert!(msg.contains("vars.api_token"), "{msg}");
        assert!(
            !msg.contains("a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6a7b8"),
            "{msg}"
        );
        let e = with(
            r#"{"rest": "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5"}"#,
            "Authorization: ghp_{rest}",
        )
        .unwrap_err();
        assert_eq!(code(&e), super::super::BUNDLE_SECRET, "{e:#}");
        assert!(format!("{e:#}").contains("x.sh (rendered)"), "{e:#}");
        with(
            r#"{"oauth_token_url": "https://example.com/oauth/token"}"#,
            "get {oauth_token_url}",
        )
        .unwrap();
    }

    /// A set's settings and instructions are checked with its vars filled:
    /// a secret is named by where it is, never shown.
    #[test]
    fn secrets_in_settings_and_instructions_are_named() {
        let e = Set::parse(
            r#"{"id": "a", "kind": "claude", "vars": {"rest": "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5"},
                "settings": {"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "curl -H 'Authorization: ghp_{rest}'"}]}]}}}"#,
            None,
        )
        .unwrap_err();
        let msg = format!("{e:#}");
        assert_eq!(code(&e), super::super::BUNDLE_SECRET, "{msg}");
        assert!(msg.contains("settings.hooks.Stop"), "{msg}");
        assert!(!msg.contains("a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5"), "{msg}");
        let e = Set::parse(
            r#"{"id": "a", "kind": "claude", "instructions": "one\nkey AKIAABCDEFGHIJKLMNOP\n"}"#,
            None,
        )
        .unwrap_err();
        let msg = format!("{e:#}");
        assert!(msg.contains("instructions:2"), "{msg}");
        assert!(!msg.contains("AKIAABCDEFGHIJKLMNOP"), "{msg}");
        // A long value under a secret's name anywhere in the settings, as
        // written or through a var with a plain name.
        let token = "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6a7b8";
        for (settings, vars, at) in [
            (
                r#"{"client_secret": "@SECRET@"}"#,
                "{}",
                "settings.client_secret",
            ),
            (
                r#"{"hooks": {"Stop": [{"hooks": [{"type": "http", "url": "https://hooks.example.com", "headers": {"X-Api-Token": "{v}"}}]}]}}"#,
                r#"{"v": "@SECRET@"}"#,
                "settings.hooks.Stop.0.hooks.0.headers.X-Api-Token",
            ),
            (
                r#"{"env": {"API_TOKEN": "@SECRET@"}}"#,
                "{}",
                "settings.env.API_TOKEN",
            ),
        ] {
            let set = format!(
                r#"{{"id": "a", "kind": "claude", "settings": {settings}, "vars": {vars}}}"#
            )
            .replace("@SECRET@", token);
            let e = Set::parse(&set, None).unwrap_err();
            let msg = format!("{e:#}");
            assert_eq!(code(&e), super::super::BUNDLE_SECRET, "{set}: {msg}");
            assert!(msg.contains(at), "{msg}");
            assert!(!msg.contains(token), "{msg}");
        }
        // A path under a key's name is no secret.
        Set::parse(
            r#"{"id": "a", "kind": "claude", "settings": {"apiKeyHelper": "{workdir}/.claude/hooks/key-helper-for-the-review-agent.sh"}}"#,
            None,
        )
        .unwrap();
    }

    /// A secret is reported by file and line, never by its value.
    #[test]
    fn a_secret_is_named_but_not_shown() {
        let e = Set::parse(
            r#"{"id": "a", "kind": "claude", "files": [{"path": "notes.md", "content": "one\naws AKIAABCDEFGHIJKLMNOP\n"}]}"#,
            None,
        )
        .unwrap_err();
        let msg = format!("{e:#}");
        assert!(msg.contains("notes.md:2"), "{msg}");
        assert!(!msg.contains("AKIAABCDEFGHIJKLMNOP"), "{msg}");
    }

    // Keeping the placed files out of git (`workdir_git`).

    fn run_git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    /// A repo at `dir` with one commit, made without signing or hooks.
    fn init_repo(dir: &Path) {
        run_git(dir, &["init", "-q"]);
        let tree = run_git(dir, &["write-tree"]);
        let commit = run_git(dir, &["commit-tree", tree.trim(), "-m", "start"]);
        run_git(dir, &["update-ref", "HEAD", commit.trim()]);
    }

    const ONE: &str = r#"{"id": "reviewer", "kind": "claude", "files": [{"path": ".claude/agents/checker.md", "content": "Check.\n"}]}"#;
    const BLOCK: &str = "# pastor:workdir\n/.claude/agents/checker.md\n# /pastor:workdir\n";

    fn exclude_of(repo: &Path) -> String {
        std::fs::read_to_string(repo.join(".git/info/exclude")).unwrap_or_default()
    }

    /// A placed file in a checkout is listed in the repo's exclude file, so
    /// `git status` does not show it, and placing again leaves the file and
    /// the rows as they were.
    #[test]
    fn placed_files_are_excluded_from_git() {
        let m = Machine::new();
        init_repo(&m.dir);
        let first = m.place(ONE);
        assert!(first.ok, "{first:?}");
        assert_eq!(
            exclude_of(&m.dir).trim_end().lines().last(),
            Some("# /pastor:workdir")
        );
        assert!(
            exclude_of(&m.dir).ends_with(BLOCK),
            "{}",
            exclude_of(&m.dir)
        );
        assert_eq!(run_git(&m.dir, &["status", "--porcelain"]), "");
        let row = first
            .rows
            .iter()
            .find(|r| r.path.ends_with("info/exclude"))
            .unwrap();
        assert_eq!(row.action, Action::Updated);
        assert!(
            row.detail
                .as_deref()
                .unwrap()
                .contains("/.claude/agents/checker.md")
        );

        let before = exclude_of(&m.dir);
        let again = m.place(ONE);
        assert!(
            again.rows.iter().all(|r| r.action == Action::Kept),
            "{again:?}"
        );
        assert_eq!(exclude_of(&m.dir), before);
    }

    /// A directory below the top of the repo gets lines anchored at the top.
    #[test]
    fn a_directory_below_the_repo_top_is_anchored_at_the_top() {
        let m = Machine::new();
        init_repo(&m.home);
        m.place(ONE);
        assert!(
            exclude_of(&m.home).contains("/src/app/.claude/agents/checker.md\n"),
            "{}",
            exclude_of(&m.home)
        );
        assert_eq!(
            run_git(&m.home, &["status", "--porcelain", "-uall"])
                .matches("checker")
                .count(),
            0
        );
    }

    /// A path the repo ignores already needs no line.
    #[test]
    fn a_path_the_repo_ignores_gets_no_line() {
        let m = Machine::new();
        init_repo(&m.dir);
        std::fs::write(m.at(".gitignore"), ".claude/\n").unwrap();
        let p = m.place(ONE);
        assert!(p.ok);
        assert!(
            !exclude_of(&m.dir).contains("pastor:workdir"),
            "{}",
            exclude_of(&m.dir)
        );
        assert!(
            p.rows.iter().all(|r| !r.path.ends_with("info/exclude")),
            "{p:?}"
        );
    }

    /// A path git tracks is never written over: a conflict, and the file is
    /// as it was. One pastor placed earlier, then committed, goes on being
    /// managed.
    #[test]
    fn a_tracked_path_is_a_conflict() {
        let m = Machine::new();
        init_repo(&m.dir);
        std::fs::create_dir_all(m.at(".claude/agents")).unwrap();
        std::fs::write(m.at(".claude/agents/checker.md"), "Theirs.\n").unwrap();
        run_git(&m.dir, &["add", ".claude/agents/checker.md"]);
        let p = m.place(ONE);
        assert!(!p.ok, "{p:?}");
        let row = p
            .rows
            .iter()
            .find(|r| r.action == Action::Conflict)
            .unwrap();
        assert_eq!(row.path, ".claude/agents/checker.md");
        assert!(row.detail.as_deref().unwrap().contains("tracked by git"));
        assert_eq!(m.read(".claude/agents/checker.md").unwrap(), "Theirs.\n");
        assert!(!exclude_of(&m.dir).contains("pastor:workdir"));
        // A set with nothing placed is not recorded as placed.
        let ledger = ledger_dir(&m.paths, &m.dir).join("ledger.json");
        let recorded = std::fs::read_to_string(ledger).unwrap_or_default();
        assert!(!recorded.contains("reviewer"), "{recorded}");

        // Placed first, committed after: still pastor's.
        let m = Machine::new();
        init_repo(&m.dir);
        m.place(ONE);
        run_git(&m.dir, &["add", "-f", ".claude/agents/checker.md"]);
        let again = m.place(ONE);
        assert!(again.ok, "{again:?}");
        assert!(
            again.rows.iter().all(|r| r.action != Action::Conflict),
            "{again:?}"
        );
    }

    /// `git_exclude: false` lists nothing; the tracked check stays.
    #[test]
    fn git_exclude_false_lists_nothing() {
        let m = Machine::new();
        init_repo(&m.dir);
        let set = ONE.replace(
            r#""kind": "claude","#,
            r#""kind": "claude", "git_exclude": false,"#,
        );
        let p = m.place(&set);
        assert!(p.ok, "{p:?}");
        assert!(!exclude_of(&m.dir).contains("pastor:workdir"));
        assert_eq!(
            run_git(&m.dir, &["status", "--porcelain", "-uall"])
                .matches("checker")
                .count(),
            1
        );

        let m = Machine::new();
        init_repo(&m.dir);
        std::fs::create_dir_all(m.at(".claude/agents")).unwrap();
        std::fs::write(m.at(".claude/agents/checker.md"), "Theirs.\n").unwrap();
        run_git(&m.dir, &["add", ".claude/agents/checker.md"]);
        assert!(!m.place(&set).ok);
    }

    /// Taking a set out takes its lines out, the block with the last of them,
    /// and leaves the rest of the file alone.
    #[test]
    fn removing_a_set_takes_its_lines_out() {
        let m = Machine::new();
        init_repo(&m.dir);
        let file = m.dir.join(".git/info/exclude");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "# mine\n*.swp\n").unwrap();
        m.place(ONE);
        m.place(
            &ONE.replace("reviewer", "other")
                .replace("checker", "second"),
        );
        let both = exclude_of(&m.dir);
        assert!(
            both.starts_with("# mine\n*.swp\n# pastor:workdir\n"),
            "{both}"
        );
        assert!(
            both.contains("/.claude/agents/checker.md\n")
                && both.contains("/.claude/agents/second.md\n")
        );

        let removed = m.remove(Some("reviewer"));
        assert!(
            removed
                .rows
                .iter()
                .any(|r| r.path.ends_with("info/exclude") && r.action == Action::Removed),
            "{removed:?}"
        );
        let left = exclude_of(&m.dir);
        assert!(
            !left.contains("checker") && left.contains("second"),
            "{left}"
        );
        m.remove(None);
        assert_eq!(exclude_of(&m.dir), "# mine\n*.swp\n");
    }

    /// Git reads one exclude file for a repo and its worktrees, so a line
    /// stays while the clone or any worktree still has the path.
    #[test]
    fn a_line_stays_while_another_worktree_has_the_path() {
        let m = Machine::new();
        init_repo(&m.dir);
        let wt = m.home.join("src/wt");
        run_git(
            &m.dir,
            &["worktree", "add", "-q", "--detach", wt.to_str().unwrap()],
        );
        let set = Set::parse(ONE, Some("json")).unwrap();
        place(&m.paths, &m.home, &m.dir, &set, false).unwrap();
        let p = place(&m.paths, &m.home, &wt, &set, false).unwrap();
        assert!(p.ok, "{p:?}");
        assert!(
            exclude_of(&m.dir).ends_with(BLOCK),
            "{}",
            exclude_of(&m.dir)
        );
        assert_eq!(
            exclude_of(&m.dir).matches("checker").count(),
            1,
            "one shared file, one line"
        );
        assert_eq!(run_git(&wt, &["status", "--porcelain"]), "");

        remove(&m.paths, &m.home, &m.dir, None, false).unwrap();
        assert!(
            exclude_of(&m.dir).ends_with(BLOCK),
            "the worktree still needs it"
        );
        remove(&m.paths, &m.home, &wt, None, false).unwrap();
        assert!(
            !exclude_of(&m.dir).contains("pastor:workdir"),
            "{}",
            exclude_of(&m.dir)
        );
    }

    /// A dry run says what it would exclude and writes nothing.
    #[test]
    fn a_dry_run_names_the_lines_and_writes_nothing() {
        let m = Machine::new();
        init_repo(&m.dir);
        let p = m.try_place(ONE, true).unwrap();
        let row = p
            .rows
            .iter()
            .find(|r| r.path.ends_with("info/exclude"))
            .unwrap();
        assert!(
            row.detail
                .as_deref()
                .unwrap()
                .contains("/.claude/agents/checker.md")
        );
        assert!(!exclude_of(&m.dir).contains("pastor:workdir"));
        assert!(m.read(".claude/agents/checker.md").is_none());
    }

    /// Without `git` the files are placed and a row says they are not
    /// excluded; no repo, nothing to say.
    #[test]
    fn without_git_or_a_repo_there_is_nothing_to_exclude() {
        let m = Machine::new();
        assert_eq!(
            git::find("git-that-is-not-installed", &m.dir),
            git::Repo::None
        );
        std::fs::create_dir(m.at(".git")).unwrap();
        assert_eq!(
            git::find("git-that-is-not-installed", &m.dir),
            git::Repo::NoGit
        );
        let row = git::not_excluded("reviewer", &m.dir, "git not found");
        assert_eq!(row.action, Action::Skipped);
        assert!(
            row.detail
                .as_deref()
                .unwrap()
                .starts_with("not excluded: git not found")
        );
    }

    /// A dry run lists what a real run would, no more: not a path the repo
    /// ignores, nor one the engine refused.
    #[test]
    fn a_dry_run_lists_what_a_real_run_would() {
        let m = Machine::new();
        init_repo(&m.dir);
        std::fs::write(m.at(".gitignore"), ".claude/hooks/\n").unwrap();
        std::fs::create_dir_all(m.at(".claude/agents")).unwrap();
        std::fs::write(m.at(".claude/agents/theirs.md"), "Theirs.\n").unwrap();
        let set = r#"{"id": "reviewer", "kind": "claude", "files": [
            {"path": ".claude/hooks/start.sh", "content": "x\n"},
            {"path": ".claude/agents/theirs.md", "content": "Mine.\n"},
            {"path": ".claude/agents/checker.md", "content": "Check.\n"}]}"#;
        let dry = m.try_place(set, true).unwrap();
        let said = |p: &Placed| {
            p.rows
                .iter()
                .find(|r| r.path.ends_with("info/exclude"))
                .and_then(|r| r.detail.clone())
                .unwrap_or_default()
        };
        assert_eq!(said(&dry), "excluded /.claude/agents/checker.md");
        let real = m.place(set);
        assert_eq!(said(&real), said(&dry));
    }

    /// Pastor never writes through a symlink to the exclude file: a failed
    /// row, the files placed, the target untouched.
    #[test]
    fn an_exclude_file_that_is_a_symlink_is_not_written_through() {
        let m = Machine::new();
        init_repo(&m.dir);
        let target = m.home.join("elsewhere");
        std::fs::write(&target, "mine\n").unwrap();
        std::fs::create_dir_all(m.dir.join(".git/info")).unwrap();
        let _ = std::fs::remove_file(m.dir.join(".git/info/exclude"));
        std::os::unix::fs::symlink(&target, m.dir.join(".git/info/exclude")).unwrap();
        let p = m.place(ONE);
        assert!(!p.ok, "{p:?}");
        let row = p.rows.iter().find(|r| r.action == Action::Failed).unwrap();
        assert!(
            row.detail.as_deref().unwrap().contains("symlink"),
            "{row:?}"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "mine\n");
        assert!(m.read(".claude/agents/checker.md").is_some());
    }

    /// A clone and a worktree reached through a symlinked parent path are
    /// one repo, one exclude file, one record of it.
    #[test]
    fn a_symlinked_path_to_the_repo_is_the_same_exclude_file() {
        let m = Machine::new();
        init_repo(&m.dir);
        let link = m.home.join("srclink");
        std::os::unix::fs::symlink(m.home.join("src"), &link).unwrap();
        let wt = link.join("wt");
        run_git(
            &link.join("app"),
            &["worktree", "add", "-q", "--detach", wt.to_str().unwrap()],
        );
        let set = Set::parse(ONE, Some("json")).unwrap();
        place(&m.paths, &m.home, &link.join("app"), &set, false).unwrap();
        let p = place(&m.paths, &m.home, &wt, &set, false).unwrap();
        assert!(p.ok, "{p:?}");
        let file_of = |dir: &Path| {
            let real = dir.canonicalize().unwrap();
            let text =
                std::fs::read_to_string(ledger_dir(&m.paths, &real).join("exclude.json")).unwrap();
            serde_json::from_str::<serde_json::Value>(&text).unwrap()["file"].clone()
        };
        assert_eq!(file_of(&link.join("app")), file_of(&wt));
        assert!(
            exclude_of(&m.dir).ends_with(BLOCK),
            "{}",
            exclude_of(&m.dir)
        );
        remove(&m.paths, &m.home, &wt, None, false).unwrap();
        assert!(
            exclude_of(&m.dir).ends_with(BLOCK),
            "the clone still needs it"
        );
    }

    /// A name with a newline could close pastor's block early: refused.
    #[test]
    fn a_path_with_a_control_character_is_refused() {
        let e = Set::parse(
            r##"{"id": "a", "kind": "claude", "files": [{"path": "x\n# /pastor:workdir\n*", "content": "y"}]}"##,
            None,
        )
        .unwrap_err();
        assert_eq!(code(&e), WORKDIR_INVALID, "{e:#}");
    }

    const AGY: &str = r##"{
        "id": "reviewer",
        "kind": "agy",
        "skills": [{"name": "reviewing-a-pr", "files": {"SKILL.md": "---\nname: reviewing-a-pr\ndescription: Use to review.\n---\nRead the diff.\n"}}],
        "files": [
            {"path": ".agents/rules/style.md", "content": "---\ntrigger: always_on\n---\nKeep it short.\n"},
            {"path": ".agents/agents/checker/agent.md", "content": "---\nname: checker\ndescription: Checks.\n---\n# Checker\n"},
            {"path": ".agents/hooks/start.sh", "content": "#!/bin/sh\necho '{}'\n", "mode": "0755"}
        ],
        "settings": {
            "reviewer-start": {"SessionStart": [{"type": "command", "command": "{workdir}/.agents/hooks/start.sh {ticket_url}"}]}
        },
        "instructions": "You review pull requests in {workdir}.\n",
        "vars": {"ticket_url": "https://tickets.example.com"}
    }"##;

    const HOOKS: &str = ".agents/hooks.json";

    /// The agy home's settings, trusting `folders`.
    fn trust(m: &Machine, folders: &[&Path]) {
        let file = m.home.join(".gemini/antigravity-cli/settings.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            file,
            serde_json::json!({"trustedWorkspaces": folders}).to_string(),
        )
        .unwrap();
    }

    /// An agy set's skills go under `.agents/skills/`, and its rules and
    /// custom agents are plain files where agy looks for them.
    #[test]
    fn an_agy_set_places_its_skills_rules_and_agents() {
        let m = Machine::new();
        trust(&m, &[&m.dir]);
        let p = m.place(AGY);
        assert!(p.ok, "{p:?}");
        assert!(
            m.read(".agents/skills/reviewing-a-pr/SKILL.md")
                .unwrap()
                .contains("Read the diff.")
        );
        assert!(m.read(".agents/rules/style.md").is_some());
        assert!(m.read(".agents/agents/checker/agent.md").is_some());
        assert!(!m.at(".claude").exists());
        let again = m.place(AGY);
        assert!(
            again.rows.iter().all(|r| r.action == Action::Kept),
            "{again:?}"
        );
        let p = m.remove(Some("reviewer"));
        assert!(p.ok, "{p:?}");
        assert!(!m.at(".agents").exists());
        assert!(!m.at("AGENTS.md").exists());
    }

    /// `settings` of an agy set are named hooks, each the set's whole, in
    /// `.agents/hooks.json`; a hook of the same name that is not pastor's
    /// is a conflict, and what else the file holds stays.
    #[test]
    fn agy_hooks_merge_into_hooks_json_by_name() {
        let m = Machine::new();
        trust(&m, &[&m.dir]);
        std::fs::create_dir_all(m.at(".agents")).unwrap();
        let mine = serde_json::json!({"mine": {"Stop": [{"command": "true"}]}});
        std::fs::write(m.at(HOOKS), mine.to_string()).unwrap();
        let p = m.place(AGY);
        assert!(p.ok, "{p:?}");
        let dir = m.dir.display().to_string();
        let hooks = json_at(&m, HOOKS);
        assert_eq!(hooks["mine"], mine["mine"]);
        assert_eq!(
            hooks["reviewer-start"]["SessionStart"][0]["command"],
            format!("{dir}/.agents/hooks/start.sh https://tickets.example.com")
        );
        let p = m.remove(Some("reviewer"));
        assert!(p.ok, "{p:?}");
        assert_eq!(json_at(&m, HOOKS), mine);
        // A hook of that name already there, not pastor's: a conflict.
        let theirs = serde_json::json!({"reviewer-start": {"Stop": [{"command": "true"}]}});
        std::fs::write(m.at(HOOKS), theirs.to_string()).unwrap();
        let p = m.place(AGY);
        assert!(!p.ok, "{p:?}");
        assert!(
            p.rows
                .iter()
                .any(|r| r.path == HOOKS && r.action == Action::Conflict),
            "{p:?}"
        );
        assert_eq!(json_at(&m, HOOKS), theirs);
    }

    /// agy loads a folder's hooks in an interactive session only once that
    /// folder itself is trusted: a set with hooks says so, and is placed.
    #[test]
    fn agy_hooks_in_a_folder_agy_does_not_trust_get_a_row() {
        let m = Machine::new();
        let row = |p: &Placed| p.rows.iter().find(|r| r.path == "trust").cloned();
        let p = m.place(AGY);
        assert!(p.ok, "{p:?}");
        let r = row(&p).expect("a trust row");
        assert_eq!(r.action, Action::Skipped);
        assert!(
            r.detail.as_deref().unwrap().contains("trusted folder"),
            "{r:?}"
        );
        assert!(m.read(HOOKS).is_some());
        // A parent trusted is not enough; the folder is.
        trust(&m, &[&m.home]);
        assert!(row(&m.place(AGY)).is_some());
        trust(&m, &[&m.dir]);
        assert_eq!(row(&m.place(AGY)), None);
        // No hooks, nothing to say.
        let plain =
            r#"{"id": "plain", "kind": "agy", "files": [{"path": "x.md", "content": "x\n"}]}"#;
        trust(&m, &[]);
        assert_eq!(row(&m.place(plain)), None);
    }

    /// An agy set's instructions are a block in `AGENTS.md`, after the
    /// text there.
    #[test]
    fn agy_instructions_are_a_block_in_agents_md() {
        let m = Machine::new();
        trust(&m, &[&m.dir]);
        std::fs::write(m.at("AGENTS.md"), "The repo's notes.\n").unwrap();
        let p = m.place(AGY);
        assert!(p.ok, "{p:?}");
        let dir = m.dir.display().to_string();
        assert_eq!(
            m.read("AGENTS.md").unwrap(),
            format!(
                "The repo's notes.\n\n<!-- pastor:workdir:reviewer -->\nYou review pull requests in {dir}.\n<!-- /pastor:workdir:reviewer -->\n"
            )
        );
        m.remove(Some("reviewer"));
        assert_eq!(m.read("AGENTS.md").unwrap(), "The repo's notes.\n");
    }

    /// `AGENTS.md` and `.agents/hooks.json` are files people commit: one
    /// that was there before the set, tracked or not, gets no exclude line,
    /// so `git status` still lists it, in a dry run too, and after the set
    /// is gone. The set's own files are excluded as ever.
    #[test]
    fn a_shared_file_there_before_the_set_stays_in_git_status() {
        let m = Machine::new();
        trust(&m, &[&m.dir]);
        init_repo(&m.dir);
        std::fs::write(m.at("AGENTS.md"), "Mine, not yet committed.\n").unwrap();
        std::fs::create_dir_all(m.at(".agents")).unwrap();
        std::fs::write(m.at(HOOKS), r#"{"mine": {"Stop": [{"command": "true"}]}}"#).unwrap();
        let listed = || run_git(&m.dir, &["status", "--porcelain", "-uall"]);
        let dry = m.try_place(AGY, true).unwrap();
        let said: Vec<&str> = dry
            .rows
            .iter()
            .filter_map(|r| r.detail.as_deref())
            .collect();
        assert!(
            said.iter()
                .all(|d| !d.contains("/AGENTS.md") && !d.contains(HOOKS)),
            "{said:?}"
        );
        let p = m.place(AGY);
        assert!(p.ok, "{p:?}");
        let exclude = exclude_of(&m.dir);
        assert!(exclude.contains("/.agents/rules/style.md\n"), "{exclude}");
        assert!(!exclude.contains("/AGENTS.md"), "{exclude}");
        assert!(!exclude.contains(&format!("/{HOOKS}")), "{exclude}");
        assert!(listed().contains("?? AGENTS.md\n"), "{}", listed());
        assert!(listed().contains(&format!("?? {HOOKS}\n")), "{}", listed());
        assert!(!listed().contains("style.md"), "{}", listed());
        m.remove(Some("reviewer"));
        assert!(listed().contains("?? AGENTS.md\n"), "{}", listed());
    }

    /// A shared file pastor made in this directory is its own, and is kept
    /// out of git like the set's other files.
    #[test]
    fn a_shared_file_pastor_made_is_excluded() {
        let m = Machine::new();
        trust(&m, &[&m.dir]);
        init_repo(&m.dir);
        let p = m.place(AGY);
        assert!(p.ok, "{p:?}");
        let exclude = exclude_of(&m.dir);
        assert!(exclude.contains("/AGENTS.md\n"), "{exclude}");
        assert!(exclude.contains(&format!("/{HOOKS}\n")), "{exclude}");
        assert_eq!(run_git(&m.dir, &["status", "--porcelain", "-uall"]), "");
    }

    /// When the repository tracks `CLAUDE.local.md`, the block goes in
    /// `.claude/CLAUDE.md`, which people commit too: one that was there
    /// before the set stays in `git status`, and one pastor made is excluded.
    #[test]
    fn claudes_fallback_file_is_excluded_only_when_pastor_made_it() {
        let m = Machine::new();
        init_repo(&m.dir);
        std::fs::write(m.at("CLAUDE.local.md"), "Shared.\n").unwrap();
        git(&m.dir, &["add", "-f", "CLAUDE.local.md"]);
        let listed = || run_git(&m.dir, &["status", "--porcelain", "-uall"]);
        std::fs::create_dir_all(m.at(".claude")).unwrap();
        std::fs::write(m.at(".claude/CLAUDE.md"), "Mine, not yet committed.\n").unwrap();
        let p = m.place(WITH_SETTINGS);
        assert!(p.ok, "{p:?}");
        let exclude = exclude_of(&m.dir);
        assert!(!exclude.contains("/.claude/CLAUDE.md"), "{exclude}");
        assert!(listed().contains("?? .claude/CLAUDE.md\n"), "{}", listed());
        m.remove(Some("reviewer"));
        assert!(listed().contains("?? .claude/CLAUDE.md\n"), "{}", listed());

        // A file pastor made is its own.
        std::fs::remove_file(m.at(".claude/CLAUDE.md")).unwrap();
        let p = m.place(WITH_SETTINGS);
        assert!(p.ok, "{p:?}");
        assert!(
            exclude_of(&m.dir).contains("/.claude/CLAUDE.md\n"),
            "{}",
            exclude_of(&m.dir)
        );
        assert!(!listed().contains("CLAUDE.md"), "{}", listed());
    }

    /// A shared file pastor is about to create gets no exclude line while
    /// another worktree of the repo has an untracked copy of it, since the
    /// line would hide that copy there: a `skipped` row names the worktree.
    /// Each place looks again, so the line comes when the copy goes and
    /// goes when a copy appears; a tracked copy is no concern.
    #[test]
    fn a_shared_file_is_not_excluded_over_an_untracked_copy_in_another_worktree() {
        let m = Machine::new();
        trust(&m, &[&m.dir]);
        init_repo(&m.dir);
        let other = m.dir.parent().unwrap().join("other-worktree");
        run_git(&m.dir, &["worktree", "add", "-q", other.to_str().unwrap()]);
        let status = || run_git(&other, &["status", "--porcelain", "-uall"]);
        let skipped = |p: &Placed| {
            p.rows
                .iter()
                .filter(|r| r.action == Action::Skipped && r.path == "AGENTS.md")
                .count()
        };

        std::fs::write(other.join("AGENTS.md"), "Mine, untracked there.\n").unwrap();
        let dry = m.try_place(AGY, true).unwrap();
        assert_eq!(skipped(&dry), 1, "{dry:?}");
        let p = m.place(AGY);
        assert!(p.ok, "{p:?}");
        assert_eq!(skipped(&p), 1, "{p:?}");
        let row = p
            .rows
            .iter()
            .find(|r| r.action == Action::Skipped && r.path == "AGENTS.md");
        let said = row.and_then(|r| r.detail.as_deref()).unwrap_or_default();
        assert!(said.contains(other.to_str().unwrap()), "{said}");
        let exclude = exclude_of(&m.dir);
        assert!(!exclude.contains("/AGENTS.md"), "{exclude}");
        assert!(
            exclude.contains(&format!("/{HOOKS}\n")),
            "the other shared file has none: {exclude}"
        );
        assert!(status().contains("?? AGENTS.md\n"), "{}", status());
        assert_eq!(skipped(&m.place(AGY)), 1, "again: the same");

        // The copy goes: the next place excludes the file pastor made.
        std::fs::remove_file(other.join("AGENTS.md")).unwrap();
        let p = m.place(AGY);
        assert_eq!(skipped(&p), 0, "{p:?}");
        assert!(
            exclude_of(&m.dir).contains("/AGENTS.md\n"),
            "{}",
            exclude_of(&m.dir)
        );

        // A copy appears: the next place takes the line out.
        std::fs::write(other.join("AGENTS.md"), "Mine again.\n").unwrap();
        let p = m.place(AGY);
        assert_eq!(skipped(&p), 1, "{p:?}");
        assert!(
            !exclude_of(&m.dir).contains("/AGENTS.md"),
            "{}",
            exclude_of(&m.dir)
        );
        assert!(status().contains("?? AGENTS.md\n"), "{}", status());

        // A copy the other worktree tracks hides nothing there.
        run_git(&other, &["add", "AGENTS.md"]);
        let p = m.place(AGY);
        assert_eq!(skipped(&p), 0, "{p:?}");
        assert!(
            exclude_of(&m.dir).contains("/AGENTS.md\n"),
            "{}",
            exclude_of(&m.dir)
        );
    }

    /// Pastor's own layout: a worktree per task and a set in each. Each
    /// worktree's copy of a shared file is pastor's, so neither is a copy to
    /// protect, and the one line covers both however often either places.
    #[test]
    fn two_worktrees_with_the_same_set_keep_one_line_for_both() {
        let m = Machine::new();
        init_repo(&m.dir);
        let other = m.dir.parent().unwrap().join("other-worktree");
        run_git(&m.dir, &["worktree", "add", "-q", other.to_str().unwrap()]);
        trust(&m, &[&m.dir, &other]);
        let set = Set::parse(AGY, Some("json")).unwrap();
        let place_in = |dir: &Path| {
            let p = place(&m.paths, &m.home, dir, &set, false).unwrap();
            assert!(p.ok, "{p:?}");
            assert!(
                p.rows
                    .iter()
                    .all(|r| r.action != Action::Skipped || r.path == "trust"),
                "{p:?}"
            );
        };
        place_in(&m.dir);
        place_in(&other);
        place_in(&m.dir);
        place_in(&other);
        let exclude = exclude_of(&m.dir);
        assert!(exclude.contains("/AGENTS.md\n"), "{exclude}");
        assert!(exclude.contains(&format!("/{HOOKS}\n")), "{exclude}");
        for wt in [&m.dir, &other] {
            assert_eq!(
                run_git(wt, &["status", "--porcelain", "-uall"]),
                "",
                "{wt:?}"
            );
        }
    }

    /// A harness-local file is excluded whatever the other worktrees hold:
    /// only the shared files are looked at.
    #[test]
    fn a_harness_local_file_is_excluded_with_a_copy_in_another_worktree() {
        let m = Machine::new();
        init_repo(&m.dir);
        let other = m.dir.parent().unwrap().join("other-worktree");
        run_git(&m.dir, &["worktree", "add", "-q", other.to_str().unwrap()]);
        std::fs::create_dir_all(other.join(".claude/agents")).unwrap();
        std::fs::write(other.join(".claude/agents/checker.md"), "Mine.\n").unwrap();
        let p = m.place(ONE);
        assert!(p.ok, "{p:?}");
        assert!(p.rows.iter().all(|r| r.action != Action::Skipped), "{p:?}");
        assert!(
            exclude_of(&m.dir).contains("/.claude/agents/checker.md\n"),
            "{}",
            exclude_of(&m.dir)
        );
    }

    /// A repository that commits `.agents/hooks.json` takes no hooks from a
    /// set, and the row says agy has no local file to use instead.
    #[test]
    fn tracked_agy_hooks_say_there_is_no_local_file() {
        let m = Machine::new();
        trust(&m, &[&m.dir]);
        init_repo(&m.dir);
        std::fs::create_dir_all(m.at(".agents")).unwrap();
        std::fs::write(m.at(HOOKS), "{}").unwrap();
        run_git(&m.dir, &["add", HOOKS]);
        let p = m.place(AGY);
        let row = p.rows.iter().find(|r| r.path == HOOKS).unwrap();
        assert_eq!(row.action, Action::Conflict);
        assert!(
            row.detail
                .as_deref()
                .unwrap()
                .contains("agy has no local hooks file"),
            "{row:?}"
        );
    }

    /// When the repository tracks `AGENTS.md`, the instructions are a rule
    /// of the set's own instead: agy reads a rule file only with its
    /// frontmatter, so the file is the set's whole.
    #[test]
    fn agy_instructions_are_a_rule_when_the_repo_tracks_agents_md() {
        let m = Machine::new();
        trust(&m, &[&m.dir]);
        git(&m.dir, &["init", "-q"]);
        std::fs::write(m.at("AGENTS.md"), "Shared.\n").unwrap();
        git(&m.dir, &["add", "AGENTS.md"]);
        let p = m.place(AGY);
        assert!(p.ok, "{p:?}");
        let rule = ".agents/rules/pastor-reviewer.md";
        assert!(
            p.rows
                .iter()
                .any(|r| r.path == rule && r.action == Action::Added),
            "{p:?}"
        );
        let dir = m.dir.display().to_string();
        assert_eq!(
            m.read(rule).unwrap(),
            format!("---\ntrigger: always_on\n---\n\nYou review pull requests in {dir}.\n")
        );
        assert_eq!(m.read("AGENTS.md").unwrap(), "Shared.\n");
        // A file of the set in the rule's place.
        let whole = AGY.replace(
            r#""vars""#,
            r#""files": [{"path": ".agents/rules/pastor-reviewer.md", "content": "x"}], "vars""#,
        );
        let e = m.try_place(&whole, false).unwrap_err();
        assert_eq!(code(&e), WORKDIR_INVALID, "{e:#}");
    }

    /// What an agy set may not place whole: the files pastor merges into,
    /// agy's own state and the MCP and skills manifests.
    #[test]
    fn an_agy_set_is_checked() {
        let file = |path: &str| {
            format!(
                r#"{{"id": "a", "kind": "agy", "files": [{{"path": "{path}", "content": "x"}}]}}"#
            )
        };
        let cases: Vec<(String, &str)> = vec![
            (file(".agents/hooks.json"), WORKDIR_INVALID),
            (file("AGENTS.md"), WORKDIR_INVALID),
            (file(".agents/skills.json"), WORKDIR_INVALID),
            (file(".agents/mcp_config.json"), WORKDIR_INVALID),
            (file(".agents/teamwork/state.json"), WORKDIR_INVALID),
            (file(".agents/workflows/ship.md"), WORKDIR_INVALID),
            (file(".git/config"), WORKDIR_INVALID),
            (
                r#"{"id": "a", "kind": "agy", "settings": {"start": "x"}}"#.into(),
                WORKDIR_INVALID,
            ),
            (
                r#"{"id": "a", "kind": "agy", "settings": {"mcpServers": {}}}"#.into(),
                WORKDIR_MCP_UNSUPPORTED,
            ),
        ];
        for (set, want) in cases {
            let e = Set::parse(&set, None).expect_err(&set);
            assert_eq!(code(&e), want, "{set}: {e:#}");
        }
        // Claude's own files are not agy's: an agy set may place them.
        Set::parse(&file("CLAUDE.local.md"), None).unwrap();
    }

    // A profile in a set (`place_with`).

    fn render_of(name: &str) -> Render {
        render_for(name, "claude")
    }

    fn render_for(name: &str, kind: &str) -> Render {
        let config = crate::config::PastorConfig::default();
        let profile = config.profiles.resolve(name).unwrap();
        crate::profile_render::render(&config, &profile, kind, None).unwrap()
    }

    const PROFILED: &str = r#"{"id": "reviewer", "kind": "claude", "profile": "review",
        "settings": {"env": {"TICKET": "T-1"}, "permissions": {"additionalDirectories": ["/srv/notes"]}}}"#;

    fn place_profiled(m: &Machine, set: &str, profile: &str, dry: bool) -> anyhow::Result<Placed> {
        let set = Set::parse(set, Some("json"))?;
        place_with(
            &m.paths,
            &m.home,
            &m.dir,
            &set,
            Some(&render_of(profile)),
            dry,
        )
    }

    fn settings_of(m: &Machine) -> serde_json::Value {
        serde_json::from_str(&m.read(".claude/settings.local.json").unwrap()).unwrap()
    }

    /// The profile's rules land in the settings file beside the set's own
    /// keys, and the answer carries the flags and env the launch adds.
    #[test]
    fn a_profile_writes_its_rules_and_answers_the_launch_args() {
        let m = Machine::new();
        let render = render_of("review");
        let p = place_profiled(&m, PROFILED, "review", false).unwrap();
        assert!(p.ok, "{p:?}");
        let s = settings_of(&m);
        assert_eq!(
            s["permissions"]["allow"],
            render.settings["permissions"]["allow"]
        );
        assert_eq!(
            s["permissions"]["deny"],
            render.settings["permissions"]["deny"]
        );
        assert_eq!(s["permissions"]["defaultMode"], "dontAsk");
        assert_eq!(
            s["permissions"]["additionalDirectories"],
            serde_json::json!(["/srv/notes"])
        );
        assert_eq!(s["env"]["TICKET"], "T-1");
        let out = p.profile.unwrap();
        assert_eq!(out.name, "review");
        assert_eq!(out.flags, render.flags);
        assert_eq!(out.env, render.env);

        // Again: nothing changes.
        let again = place_profiled(&m, PROFILED, "review", false).unwrap();
        assert!(
            again.rows.iter().all(|r| r.action == Action::Kept),
            "{again:?}"
        );
    }

    /// The rules are pastor's whole: one the agent added since is replaced,
    /// and the row says how many went; the rest of the file is the agent's.
    #[test]
    fn a_rule_the_agent_added_is_dropped_and_counted() {
        let m = Machine::new();
        place_profiled(&m, PROFILED, "review", false).unwrap();
        let mut s = settings_of(&m);
        s["permissions"]["allow"]
            .as_array_mut()
            .unwrap()
            .push("Bash(rm -rf:*)".into());
        s["permissions"]["allow"]
            .as_array_mut()
            .unwrap()
            .push("Bash(curl:*)".into());
        s["theme"] = "dark".into();
        std::fs::write(
            m.at(".claude/settings.local.json"),
            serde_json::to_string_pretty(&s).unwrap(),
        )
        .unwrap();

        let p = place_profiled(&m, PROFILED, "review", false).unwrap();
        assert!(p.ok, "{p:?}");
        let row = p
            .rows
            .iter()
            .find(|r| r.key.as_deref() == Some("permissions.allow"))
            .unwrap();
        assert_eq!(row.action, Action::Updated);
        assert_eq!(row.detail.as_deref(), Some("agent_rules_dropped: 2"));
        let now = settings_of(&m);
        assert_eq!(
            now["permissions"]["allow"],
            render_of("review").settings["permissions"]["allow"]
        );
        assert_eq!(now["theme"], "dark", "what pastor does not own stays");
        // A dry run says the same and writes nothing.
        s["permissions"]["allow"]
            .as_array_mut()
            .unwrap()
            .push("Bash(x:*)".into());
        std::fs::write(
            m.at(".claude/settings.local.json"),
            serde_json::to_string_pretty(&s).unwrap(),
        )
        .unwrap();
        let dry = place_profiled(&m, PROFILED, "review", true).unwrap();
        let row = dry
            .rows
            .iter()
            .find(|r| r.key.as_deref() == Some("permissions.allow"))
            .unwrap();
        assert!(
            row.detail
                .as_deref()
                .unwrap()
                .starts_with("agent_rules_dropped"),
            "{row:?}"
        );
        assert!(
            m.read(".claude/settings.local.json")
                .unwrap()
                .contains("Bash(x:*)")
        );
    }

    /// Rules in a file pastor did not write are not pastor's to replace.
    #[test]
    fn rules_pastor_did_not_write_are_a_conflict() {
        let m = Machine::new();
        std::fs::create_dir_all(m.at(".claude")).unwrap();
        std::fs::write(
            m.at(".claude/settings.local.json"),
            r#"{"permissions": {"allow": ["Bash(ls:*)"]}}"#,
        )
        .unwrap();
        let p = place_profiled(&m, PROFILED, "review", false).unwrap();
        assert!(!p.ok, "{p:?}");
        assert!(
            p.rows
                .iter()
                .any(|r| r.key.as_deref() == Some("permissions.allow")
                    && r.action == Action::Conflict)
        );
        assert!(settings_of(&m)["permissions"]["allow"] == serde_json::json!(["Bash(ls:*)"]));
    }

    /// A profile change is a new version of the set; a set that drops its
    /// profile takes the rules out again.
    #[test]
    fn another_profile_replaces_the_rules_and_none_takes_them_out() {
        let m = Machine::new();
        place_profiled(&m, PROFILED, "review", false).unwrap();
        let before = m.ledger()["sets"]["reviewer"]["hash"].clone();
        let p = place_profiled(
            &m,
            PROFILED
                .replace(r#""profile": "review""#, r#""profile": "develop""#)
                .as_str(),
            "develop",
            false,
        )
        .unwrap();
        assert!(p.ok, "{p:?}");
        assert_eq!(
            settings_of(&m)["permissions"]["allow"],
            render_of("develop").settings["permissions"]["allow"]
        );
        assert_ne!(m.ledger()["sets"]["reviewer"]["hash"], before);

        let bare =
            r#"{"id": "reviewer", "kind": "claude", "settings": {"env": {"TICKET": "T-1"}}}"#;
        let p = m.place(bare);
        assert!(p.ok, "{p:?}");
        let s = settings_of(&m);
        assert!(
            s["permissions"].is_null() || s["permissions"].get("allow").is_none(),
            "{s}"
        );
        assert_eq!(s["env"]["TICKET"], "T-1");
    }

    /// Push `rule` onto a list of the permissions in the settings file, as
    /// an agent's "always allow" does.
    fn agent_adds(m: &Machine, list: &str, rule: &str) {
        let mut s = settings_of(m);
        s["permissions"][list]
            .as_array_mut()
            .unwrap()
            .push(rule.into());
        std::fs::write(
            m.at(".claude/settings.local.json"),
            serde_json::to_string_pretty(&s).unwrap(),
        )
        .unwrap();
    }

    const BARE: &str =
        r#"{"id": "reviewer", "kind": "claude", "settings": {"env": {"TICKET": "T-1"}}}"#;

    /// A rule the agent added is no reason to keep the profile's rules in
    /// when the set drops its profile or is removed: they go, and the
    /// ledger and the file agree again.
    #[test]
    fn the_rules_go_with_the_profile_after_the_agent_added_one() {
        let m = Machine::new();
        place_profiled(&m, PROFILED, "review", false).unwrap();
        agent_adds(&m, "allow", "Bash(rm -rf:*)");
        let p = m.place(BARE);
        assert!(p.ok, "{p:?}");
        let s = settings_of(&m);
        assert!(s["permissions"].get("allow").is_none(), "{s}");
        assert_eq!(s["env"]["TICKET"], "T-1");
        // The set can take a profile again.
        let p = place_profiled(&m, PROFILED, "review", false).unwrap();
        assert!(p.ok, "{p:?}");

        agent_adds(&m, "allow", "Bash(curl:*)");
        agent_adds(&m, "deny", "Bash(x:*)");
        let p = m.remove(None);
        assert!(p.ok, "{p:?}");
        assert!(!m.at(".claude/settings.local.json").exists());
        assert!(m.ledger()["sets"].get("reviewer").is_none());
    }

    /// `ask` is a rule list too: pastor writes it empty, and a rule the
    /// agent added to it is dropped like one of `allow`; one in a file
    /// pastor did not write is a conflict.
    #[test]
    fn ask_is_pastors_whole_like_the_other_rules() {
        let m = Machine::new();
        place_profiled(&m, PROFILED, "review", false).unwrap();
        assert_eq!(settings_of(&m)["permissions"]["ask"], serde_json::json!([]));
        agent_adds(&m, "ask", "Bash(git push:*)");
        let p = place_profiled(&m, PROFILED, "review", false).unwrap();
        let row = p
            .rows
            .iter()
            .find(|r| r.key.as_deref() == Some("permissions.ask"))
            .unwrap();
        assert_eq!(row.action, Action::Updated);
        assert_eq!(row.detail.as_deref(), Some("agent_rules_dropped: 1"));
        assert_eq!(settings_of(&m)["permissions"]["ask"], serde_json::json!([]));

        let other = Machine::new();
        std::fs::create_dir_all(other.at(".claude")).unwrap();
        std::fs::write(
            other.at(".claude/settings.local.json"),
            r#"{"permissions": {"ask": ["Bash(ls:*)"]}}"#,
        )
        .unwrap();
        let p = place_profiled(&other, PROFILED, "review", false).unwrap();
        assert!(
            p.rows.iter().any(
                |r| r.key.as_deref() == Some("permissions.ask") && r.action == Action::Conflict
            ),
            "{p:?}"
        );
    }

    /// The ledger records the profile a set names and its rendering beside
    /// the set's own hash, which a profile change leaves as it is.
    #[test]
    fn the_ledger_records_the_profile_and_its_rendering() {
        let m = Machine::new();
        place_profiled(&m, PROFILED, "review", false).unwrap();
        let record = m.ledger()["sets"]["reviewer"].clone();
        assert_eq!(
            record["hash"],
            Set::parse(PROFILED, Some("json")).unwrap().hash()
        );
        assert_eq!(record["profile"]["name"], "review");
        let rendered = record["profile"]["hash"].clone();
        assert!(rendered.as_str().is_some_and(|h| h.len() == 64), "{record}");

        // Another profile: the record names it and its rendering.
        let develop = PROFILED.replace(r#""profile": "review""#, r#""profile": "develop""#);
        place_profiled(&m, &develop, "develop", false).unwrap();
        let record = m.ledger()["sets"]["reviewer"].clone();
        assert_eq!(record["profile"]["name"], "develop");
        assert_ne!(record["profile"]["hash"], rendered);
    }

    /// An agy set may name a profile: its rules are the machine's own, in
    /// its home, so nothing permission-shaped is written in the directory,
    /// and the answer carries the flags the launch adds.
    #[test]
    fn an_agy_set_may_name_a_profile_and_gets_nothing_written_for_it() {
        let m = Machine::new();
        let bare = r#"{"id": "reviewer", "kind": "agy", "profile": "review"}"#;
        let set = Set::parse(bare, Some("json")).unwrap();
        assert_eq!(set.profile.as_deref(), Some("review"));
        let render = render_for("review", "agy");
        let p = place_with(&m.paths, &m.home, &m.dir, &set, Some(&render), false).unwrap();
        assert!(p.ok, "{p:?}");
        assert!(
            p.rows
                .iter()
                .all(|r| r.path != ".claude/settings.local.json")
        );
        assert!(!m.at(".claude").exists());
        assert!(!m.at(".agents").exists());
        let out = p.profile.unwrap();
        assert_eq!(out.name, "review");
        assert_eq!(out.flags, render.flags);
        assert!(!out.flags.is_empty());

        // Hooks beside it are merged as ever, with no permissions in them.
        let with_hooks = AGY.replacen(
            r#""kind": "agy","#,
            r#""kind": "agy", "profile": "review","#,
            1,
        );
        let trusted = Set::parse(&with_hooks, Some("json")).unwrap();
        let p = place_with(&m.paths, &m.home, &m.dir, &trusted, Some(&render), false).unwrap();
        assert!(p.ok, "{p:?}");
        let hooks = m.read(".agents/hooks.json").unwrap();
        assert!(
            !hooks.contains("permissions") && !hooks.contains("allow"),
            "{hooks}"
        );
        // A rendering for another kind is not this set's.
        let e = place_with(
            &m.paths,
            &m.home,
            &m.dir,
            &set,
            Some(&render_of("review")),
            true,
        )
        .unwrap_err();
        assert_eq!(code(&e), WORKDIR_INVALID, "{e:#}");
    }

    /// A profile is placed only with its rendering, and a set that names
    /// none takes none.
    #[test]
    fn a_profile_needs_its_rendering_and_a_set_without_one_takes_none() {
        let m = Machine::new();
        let e = m.try_place(PROFILED, false).unwrap_err();
        assert_eq!(code(&e), WORKDIR_INVALID, "{e:#}");
        let set = Set::parse(ONE, Some("json")).unwrap();
        let e = place_with(
            &m.paths,
            &m.home,
            &m.dir,
            &set,
            Some(&render_of("review")),
            false,
        )
        .unwrap_err();
        assert_eq!(code(&e), WORKDIR_INVALID, "{e:#}");
        let other = Set::parse(PROFILED, Some("json")).unwrap();
        let e = place_with(
            &m.paths,
            &m.home,
            &m.dir,
            &other,
            Some(&render_of("develop")),
            false,
        )
        .unwrap_err();
        assert_eq!(code(&e), WORKDIR_INVALID, "{e:#}");
    }
}
