//! `pastor-bundle.toml`: what a bundle carries into the harness homes of a
//! flock's machines. Skills, files, an instruction block, settings
//! fragments in each harness's own shape, MCP servers, harness plugins and
//! requirements a machine must meet. Validation reads the bundle's
//! directory too: every `src` must be there, of the right type, and every
//! settings fragment must parse, so a broken bundle is refused at `bundle
//! add`, not on a machine.
//!
//! A bundle with `target = "workdir"` goes into the working directory of
//! each task whose flock or job names it, as a workdir set
//! (`workdir_stored`), so its keys are a set's: a `[[file]]` takes a `path`
//! in the directory instead of a slot, and it carries no MCP servers,
//! plugins or requirements, which belong to a home.
//!
//! Nothing here renders or places anything; that is the apply engine's.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

use crate::connector::manifest::{
    Version, check_command, check_env_name, check_id, pastor_version,
};
use crate::provision::AGENT_KINDS;

pub const MANIFEST_FILE: &str = "pastor-bundle.toml";

/// The values pastor gives every bundle per machine and harness home; a
/// bundle may use them in its templates and may not declare them.
pub const BUILTIN_VARS: [&str; 9] = [
    "home",
    "config_dir",
    "bundle_dir",
    "bin",
    "machine",
    "flock",
    "agent",
    "kind",
    "account",
];

/// Top-level settings keys a bundle may not set, per kind: the permission
/// rules there come from pastor's profiles, which would overwrite them.
const PROFILE_KEYS: [(&str, &[&str]); 3] = [
    ("claude", &["permissions"]),
    (
        "agy",
        &[
            "permissions",
            "toolPermission",
            "agentMode",
            "allowNonWorkspaceAccess",
        ],
    ),
    ("opencode", &["permission"]),
];

/// Where a bundle goes: the harness homes of a flock's machines, or the
/// working directory of each task that names it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    #[default]
    Home,
    Workdir,
}

impl Target {
    pub fn as_str(self) -> &'static str {
        match self {
            Target::Home => "home",
            Target::Workdir => "workdir",
        }
    }
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The kinds whose working directory has a settings file a workdir
/// bundle's `[settings.<kind>]` merges into.
const WORKDIR_SETTINGS_KINDS: [&str; 2] = ["claude", "agy"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub id: String,
    /// The author's version, any word; pastor tells versions apart by the
    /// content hash.
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_pastor_version: Option<String>,
    /// Where it goes; unset is `home`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<Target>,
    /// Values the bundle needs per flock, given in flock.toml's
    /// `bundle_vars`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub vars: BTreeMap<String, Var>,
    #[serde(default, rename = "skill", skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<Skill>,
    #[serde(default, rename = "file", skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<File>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<Instructions>,
    /// A fragment per harness kind, merged into that harness's settings.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub settings: BTreeMap<String, Settings>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp: Vec<Mcp>,
    #[serde(default, rename = "plugin", skip_serializing_if = "Vec::is_empty")]
    pub plugins: Vec<Plugin>,
    #[serde(default, rename = "require", skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<Require>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Var {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A skill folder, placed under its own name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Skill {
    pub src: String,
    /// Unset: every kind that has a skills slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kinds: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Slot {
    Hooks,
    Bin,
    Mods,
    InstructionsInclude,
}

/// Any other file: in a slot of each harness home, or for a workdir
/// bundle at a path in the working directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct File {
    pub src: String,
    /// A home bundle's: which slot. Required there, refused in a workdir
    /// bundle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<Slot>,
    /// A workdir bundle's: where in the directory, by a set's path rules.
    /// Required there, refused in a home bundle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// A workdir bundle's: the kinds of agent it is for; unset is every
    /// kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kinds: Option<Vec<String>>,
    /// Octal, such as `"0755"`; unset keeps the file's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// A workdir bundle's: fill the vars in the file, as a set's `render`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render: Option<bool>,
}

/// A marked block in each harness's instruction file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Instructions {
    pub src: String,
    /// A path with `{machine}` in it; the file for a machine is appended
    /// when the bundle has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_machine: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kinds: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// JSON for claude, agy and opencode; TOML for codex.
    pub src: String,
}

/// One MCP server: a command, or a URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mcp {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Names of the variables the server needs, whose values the machine's
    /// env supplies. Never values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_from: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kinds: Option<Vec<String>>,
}

/// A harness plugin, installed by the harness's own CLI. Only Claude's are
/// installed today.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plugin {
    pub kind: String,
    pub name: String,
    pub marketplace: String,
    pub source: String,
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
}

/// What a machine must have for the bundle to work, as a check row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Require {
    pub id: String,
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix_hint: Option<String>,
}

impl Manifest {
    /// Where the bundle goes.
    pub fn target(&self) -> Target {
        self.target.unwrap_or_default()
    }

    /// Read and validate `dir/pastor-bundle.toml` against the files in
    /// `dir`.
    pub fn load(dir: &Path) -> Result<Manifest, String> {
        let path = dir.join(MANIFEST_FILE);
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let manifest = Manifest::parse(&text)?;
        manifest.check_files(dir)?;
        Ok(manifest)
    }

    /// Parse and validate what the manifest says on its own.
    pub fn parse(text: &str) -> Result<Manifest, String> {
        Manifest::parse_for(text, &pastor_version())
    }

    pub fn parse_for(text: &str, pastor: &Version) -> Result<Manifest, String> {
        // As for a connector: a manifest for a newer pastor may carry keys
        // this one lacks, so say that instead of naming the key.
        if let Ok(loose) = text.parse::<toml::Table>()
            && let Some(min) = loose.get("min_pastor_version").and_then(|v| v.as_str())
            && let Ok(min) = Version::parse(min)
            && &min > pastor
        {
            return Err(format!("needs pastor {min} or later; this is {pastor}"));
        }
        let m: Manifest = toml::from_str(text).map_err(|e| e.to_string())?;
        m.validate()?;
        Ok(m)
    }

    fn validate(&self) -> Result<(), String> {
        check_id(&self.id)?;
        if self.id == super::plan::MCP_PLUGIN {
            return Err(format!(
                "id: {} is the name of the plugin pastor makes for Claude's MCP servers; pick another",
                self.id
            ));
        }
        if self.version.is_empty()
            || self
                .version
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(format!("version {:?} must be one word", self.version));
        }
        if let Some(min) = &self.min_pastor_version {
            Version::parse(min).map_err(|e| format!("min_pastor_version: {e}"))?;
        }
        for name in self.vars.keys() {
            check_var_name(name).map_err(|e| format!("vars: {e}"))?;
            if BUILTIN_VARS.contains(&name.as_str()) {
                return Err(format!(
                    "vars: {name} is pastor's own; use {{{name}}} without declaring it"
                ));
            }
        }
        if self.skills.is_empty()
            && self.files.is_empty()
            && self.instructions.is_none()
            && self.settings.is_empty()
            && self.mcp.is_empty()
            && self.plugins.is_empty()
            && self.requires.is_empty()
        {
            return Err("carries nothing: add a skill, file, instructions, settings, mcp, plugin or require".into());
        }
        let mut names = BTreeSet::new();
        for (i, s) in self.skills.iter().enumerate() {
            let at = format!("skill[{i}]");
            let name = check_src(&format!("{at}.src"), &s.src)?;
            if !names.insert(name.clone()) {
                return Err(format!("{at}.src: a second skill named {name:?}"));
            }
            check_kinds(&format!("{at}.kinds"), s.kinds.as_deref())?;
        }
        let workdir = self.target() == Target::Workdir;
        let mut placed = BTreeSet::new();
        for (i, f) in self.files.iter().enumerate() {
            let at = format!("file[{i}]");
            let name = check_src(&format!("{at}.src"), &f.src)?;
            let at_place = if workdir {
                if f.slot.is_some() {
                    return Err(format!(
                        "{at}.slot: a workdir bundle places a file at a path in the directory: give `path`, not `slot`"
                    ));
                }
                let Some(path) = &f.path else {
                    return Err(format!(
                        "{at}: a workdir bundle's file needs `path`, where it goes in the directory"
                    ));
                };
                check_kinds(&format!("{at}.kinds"), f.kinds.as_deref())?;
                path.clone()
            } else {
                let Some(slot) = f.slot else {
                    return Err(format!(
                        "{at}: needs `slot` (hooks, bin, mods or instructions-include)"
                    ));
                };
                for (key, set) in [
                    ("path", f.path.is_some()),
                    ("kinds", f.kinds.is_some()),
                    ("render", f.render.is_some()),
                ] {
                    if set {
                        return Err(format!(
                            "{at}.{key}: only a workdir bundle's file takes {key} (target = \"workdir\")"
                        ));
                    }
                }
                format!("{slot:?}/{name}")
            };
            if !placed.insert(at_place.clone()) {
                return Err(if workdir {
                    format!("{at}.path: a second file at {at_place:?}")
                } else {
                    format!("{at}.src: a second file named {name:?} in its slot")
                });
            }
            if let Some(mode) = &f.mode {
                match u32::from_str_radix(mode, 8) {
                    Ok(m) if m <= 0o777 && !mode.is_empty() => {}
                    _ => {
                        return Err(format!(
                            "{at}.mode: {mode:?} is not an octal mode such as \"0755\""
                        ));
                    }
                }
            }
        }
        if let Some(ins) = &self.instructions {
            check_src("instructions.src", &ins.src)?;
            if let Some(p) = &ins.per_machine {
                if !p.contains("{machine}") {
                    return Err(format!(
                        "instructions.per_machine: {p:?} must contain {{machine}}"
                    ));
                }
                if let Some(other) = placeholders(p).into_iter().find(|v| v != "machine") {
                    return Err(format!(
                        "instructions.per_machine: only {{machine}} may vary, not {{{other}}}"
                    ));
                }
                check_src("instructions.per_machine", &p.replace("{machine}", "m"))?;
            }
            check_kinds("instructions.kinds", ins.kinds.as_deref())?;
        }
        for (kind, s) in &self.settings {
            check_kind(&format!("settings.{kind}"), kind)?;
            check_src(&format!("settings.{kind}.src"), &s.src)?;
            if workdir && !WORKDIR_SETTINGS_KINDS.contains(&kind.as_str()) {
                return Err(format!(
                    "settings.{kind}: a {kind} agent's working directory has no settings file pastor merges into; only {} do",
                    WORKDIR_SETTINGS_KINDS.join(" and ")
                ));
            }
        }
        if workdir {
            if !self.mcp.is_empty() {
                return Err(
                    "mcp: a working directory has no file of its own for MCP servers; carry them in a home bundle".into(),
                );
            }
            if !self.plugins.is_empty() {
                return Err(
                    "plugin: a harness plugin is installed in a home; carry it in a home bundle"
                        .into(),
                );
            }
            if !self.requires.is_empty() {
                return Err(
                    "require: what a machine must have is a home bundle's; carry it in one".into(),
                );
            }
        }
        let mut servers = BTreeSet::new();
        for (i, m) in self.mcp.iter().enumerate() {
            let at = format!("mcp[{i}]");
            let ok = !m.name.is_empty()
                && m.name.len() <= 64
                && m.name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_-".contains(c));
            if !ok {
                return Err(format!(
                    "{at}.name: {:?} must match [A-Za-z0-9_-]{{1,64}}",
                    m.name
                ));
            }
            if !servers.insert(m.name.as_str()) {
                return Err(format!("{at}.name: a second server named {:?}", m.name));
            }
            match (&m.command, &m.url) {
                (Some(c), None) if !c.is_empty() => {}
                (None, Some(u)) if !u.is_empty() => {
                    if !m.args.is_empty() {
                        return Err(format!("{at}.args: a server with a url takes no args"));
                    }
                }
                _ => return Err(format!("{at}: needs a command or a url, not both")),
            }
            for name in &m.env_from {
                check_env_name(name).map_err(|e| format!("{at}.env_from: {name:?} {e}"))?;
            }
            let templates = m.command.iter().chain(&m.args).chain(&m.url);
            for word in templates {
                for v in placeholders(word) {
                    if !BUILTIN_VARS.contains(&v.as_str()) && !self.vars.contains_key(&v) {
                        return Err(format!(
                            "{at}: {{{v}}} is no var of the bundle's and not one of pastor's"
                        ));
                    }
                }
            }
            check_kinds(&format!("{at}.kinds"), m.kinds.as_deref())?;
        }
        for (i, p) in self.plugins.iter().enumerate() {
            let at = format!("plugin[{i}]");
            if p.kind != "claude" {
                return Err(format!(
                    "{at}.kind: {:?}: pastor installs only claude plugins",
                    p.kind
                ));
            }
            for (key, v) in [
                ("name", &p.name),
                ("marketplace", &p.marketplace),
                ("source", &p.source),
            ] {
                if v.is_empty() || v.chars().any(|c| c.is_whitespace() || c.is_control()) {
                    return Err(format!("{at}.{key}: {v:?} must be one word"));
                }
            }
            if let Some(r) = &p.git_ref
                && (r.is_empty() || r.starts_with('-'))
            {
                return Err(format!("{at}.ref: {r:?} is not a branch, tag or commit"));
            }
        }
        let mut ids = BTreeSet::new();
        for (i, r) in self.requires.iter().enumerate() {
            let at = format!("require[{i}]");
            crate::provision::check_check_id(&r.id).map_err(|e| format!("{at}.id: {e}"))?;
            if !ids.insert(r.id.as_str()) {
                return Err(format!("{at}.id: {:?} is used twice", r.id));
            }
            check_command(&format!("{at}.command"), &r.command)?;
        }
        Ok(())
    }

    /// What `validate` cannot see from the text: each `src` is in `dir` and
    /// of its type, and each settings fragment parses and leaves the
    /// profile's keys alone.
    fn check_files(&self, dir: &Path) -> Result<(), String> {
        let is_dir = |src: &str| dir.join(src).is_dir();
        let is_file = |src: &str| dir.join(src).is_file();
        for (i, s) in self.skills.iter().enumerate() {
            if !is_dir(&s.src) {
                return Err(format!(
                    "skill[{i}].src: {} is not a directory in the bundle",
                    s.src
                ));
            }
            if !dir.join(&s.src).join("SKILL.md").is_file() {
                return Err(format!("skill[{i}].src: {} has no SKILL.md", s.src));
            }
        }
        for (i, f) in self.files.iter().enumerate() {
            if !is_file(&f.src) {
                return Err(format!(
                    "file[{i}].src: {} is not a file in the bundle",
                    f.src
                ));
            }
        }
        if let Some(ins) = &self.instructions
            && !is_file(&ins.src)
        {
            return Err(format!(
                "instructions.src: {} is not a file in the bundle",
                ins.src
            ));
        }
        for (kind, s) in &self.settings {
            let at = format!("settings.{kind}.src");
            let path = dir.join(&s.src);
            let text =
                std::fs::read_to_string(&path).map_err(|e| format!("{at}: {}: {e}", s.src))?;
            let keys: Vec<String> = if kind == "codex" {
                let table: toml::Table = text
                    .parse()
                    .map_err(|e| format!("{at}: {} is not TOML: {e}", s.src))?;
                table.keys().cloned().collect()
            } else {
                let value: serde_json::Value = serde_json::from_str(&text)
                    .map_err(|e| format!("{at}: {} is not JSON: {e}", s.src))?;
                let Some(object) = value.as_object() else {
                    return Err(format!("{at}: {} must hold a JSON object", s.src));
                };
                object.keys().cloned().collect()
            };
            // A workdir bundle's settings are a set's, whose own check
            // names the keys a profile writes (`workdir_stored::check`).
            let owned = PROFILE_KEYS
                .iter()
                .find(|(k, _)| k == kind && self.target() == Target::Home)
                .map_or(&[][..], |(_, keys)| *keys);
            if let Some(key) = keys.iter().find(|k| owned.contains(&k.as_str())) {
                return Err(format!(
                    "{at}: {} sets {key}, which pastor's permission profiles own; put the rules in a profile",
                    s.src
                ));
            }
        }
        if self.target() == Target::Workdir {
            super::workdir_stored::check(self, dir)?;
        }
        Ok(())
    }

    /// The vars this bundle needs a value for in flock.toml.
    pub fn var_names(&self) -> impl Iterator<Item = &str> {
        self.vars.keys().map(String::as_str)
    }
}

/// A `bundle_vars` key or a declared var: `[a-z][a-z0-9_]{0,63}`.
pub fn check_var_name(name: &str) -> Result<(), String> {
    let ok = name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if ok {
        Ok(())
    } else {
        Err(format!("{name:?} must match [a-z][a-z0-9_]{{0,63}}"))
    }
}

/// The `{name}` placeholders in `s`, each a var name.
fn placeholders(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        match after.find('}') {
            Some(close) => {
                let name = &after[..close];
                if check_var_name(name).is_ok() {
                    out.push(name.to_string());
                }
                rest = &after[close + 1..];
            }
            None => break,
        }
    }
    out
}

/// A path inside the bundle: relative, with no `..`. Gives its last part,
/// the name it is placed under.
fn check_src(field: &str, src: &str) -> Result<String, String> {
    let path = Path::new(src);
    let mut last = None;
    for c in path.components() {
        match c {
            Component::Normal(part) => last = Some(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => {
                return Err(format!(
                    "{field}: {src:?} must be a path inside the bundle, with no .."
                ));
            }
        }
    }
    last.ok_or_else(|| format!("{field}: {src:?} names no file"))
}

fn check_kind(field: &str, kind: &str) -> Result<(), String> {
    if AGENT_KINDS.contains(&kind) {
        Ok(())
    } else {
        Err(format!(
            "{field}: {kind:?} is not an agent kind ({})",
            AGENT_KINDS.join(", ")
        ))
    }
}

fn check_kinds(field: &str, kinds: Option<&[String]>) -> Result<(), String> {
    let Some(kinds) = kinds else { return Ok(()) };
    if kinds.is_empty() {
        return Err(format!(
            "{field}: leave it out for every kind, rather than empty"
        ));
    }
    for (i, k) in kinds.iter().enumerate() {
        check_kind(field, k)?;
        if kinds[..i].contains(k) {
            return Err(format!("{field}: {k} is listed twice"));
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The design's example bundle, laid out on disk.
    pub(crate) fn write_example(dir: &Path) {
        let put = |rel: &str, text: &str| {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        };
        put(
            "skills/reviewing-a-pr/SKILL.md",
            "---\nname: reviewing-a-pr\n---\nRead the diff.\n",
        );
        put("hooks/notify.sh", "#!/bin/sh\necho done\n");
        put("instructions/team.md", "Open tickets at {ticket_url}.\n");
        put(
            "instructions/machines/build-01.md",
            "This one has the GPUs.\n",
        );
        put("settings/claude.json", "{\"env\": {\"TEAM\": \"ops\"}}\n");
        put("settings/codex.toml", "model = \"o4\"\n");
        put(MANIFEST_FILE, EXAMPLE);
    }

    pub(crate) const EXAMPLE: &str = r#"
id = "team-rules"
version = "2026.10.27.1"
description = "the team's hooks, skills and agent instructions"
author = "platform team"

[vars]
ticket_url = { description = "the team's ticket tracker URL" }

[[skill]]
src = "skills/reviewing-a-pr"
kinds = ["claude", "codex"]

[[file]]
src = "hooks/notify.sh"
slot = "hooks"
mode = "0755"

[instructions]
src = "instructions/team.md"
per_machine = "instructions/machines/{machine}.md"
kinds = ["claude", "codex", "opencode"]

[settings.claude]
src = "settings/claude.json"
[settings.codex]
src = "settings/codex.toml"

[[mcp]]
name = "tickets"
command = "npx"
args = ["-y", "@example/tickets-mcp", "--url", "{ticket_url}", "--home", "{home}"]
env_from = ["TICKETS_TOKEN"]
kinds = ["claude", "agy", "codex", "opencode"]

[[plugin]]
kind = "claude"
name = "code-review"
marketplace = "example-marketplace"
source = "github:example/claude-plugins"
ref = "v1.4.0"

[[require]]
id = "gh"
command = ["sh", "-c", "command -v gh"]
fix_hint = "install the GitHub CLI on this machine"
"#;

    #[test]
    fn the_example_loads() {
        let tmp = tempfile::tempdir().unwrap();
        write_example(tmp.path());
        let m = Manifest::load(tmp.path()).unwrap();
        assert_eq!(m.id, "team-rules");
        assert_eq!(m.var_names().collect::<Vec<_>>(), ["ticket_url"]);
        assert_eq!(m.skills[0].kinds.as_deref().unwrap(), ["claude", "codex"]);
        assert_eq!(m.files[0].slot, Some(Slot::Hooks));
        assert_eq!(m.mcp[0].env_from, ["TICKETS_TOKEN"]);
        assert_eq!(m.plugins[0].git_ref.as_deref(), Some("v1.4.0"));
        assert_eq!(m.requires[0].id, "gh");
        assert_eq!(m.settings.keys().collect::<Vec<_>>(), ["claude", "codex"]);
    }

    /// Each text-only mistake, with what the error says.
    #[test]
    fn a_bad_manifest_says_what_is_wrong() {
        let base = "id = \"b\"\nversion = \"1\"\n";
        let item = "[[require]]\nid = \"r\"\ncommand = [\"true\"]\n";
        let cases: &[(&str, &str)] = &[
            ("id = \"B\"\nversion = \"1\"\n", "must match"),
            (
                &format!("id = \"pastor-mcp\"\nversion = \"1\"\n{item}"),
                "the plugin pastor makes",
            ),
            ("id = \"b\"\nversion = \"one two\"\n", "one word"),
            (base, "carries nothing"),
            (&format!("{base}surprise = 1\n{item}"), "unknown field"),
            (
                &format!("{base}[vars]\nhome = {{}}\n{item}"),
                "pastor's own",
            ),
            (
                &format!("{base}[vars]\nTicket = {{}}\n{item}"),
                "must match",
            ),
            (&format!("{base}[[skill]]\nsrc = \"../x\"\n"), "with no .."),
            (
                &format!("{base}[[skill]]\nsrc = \"/etc/x\"\n"),
                "with no ..",
            ),
            (
                &format!("{base}[[skill]]\nsrc = \"a/s\"\n[[skill]]\nsrc = \"b/s\"\n"),
                "a second skill",
            ),
            (
                &format!("{base}[[skill]]\nsrc = \"s\"\nkinds = []\n"),
                "rather than empty",
            ),
            (
                &format!("{base}[[skill]]\nsrc = \"s\"\nkinds = [\"vim\"]\n"),
                "not an agent kind",
            ),
            (
                &format!("{base}[[skill]]\nsrc = \"s\"\nkinds = [\"codex\", \"codex\"]\n"),
                "listed twice",
            ),
            (
                &format!("{base}[[file]]\nsrc = \"f\"\nslot = \"etc\"\n"),
                "unknown variant",
            ),
            (
                &format!("{base}[[file]]\nsrc = \"f\"\nslot = \"bin\"\nmode = \"rwx\"\n"),
                "octal mode",
            ),
            (
                &format!("{base}[[file]]\nsrc = \"f\"\nslot = \"bin\"\nmode = \"1777\"\n"),
                "octal mode",
            ),
            (
                &format!(
                    "{base}[[file]]\nsrc = \"a/f\"\nslot = \"bin\"\n[[file]]\nsrc = \"b/f\"\nslot = \"bin\"\n"
                ),
                "a second file",
            ),
            (
                &format!("{base}[instructions]\nsrc = \"i.md\"\nper_machine = \"m.md\"\n"),
                "must contain {machine}",
            ),
            (
                &format!(
                    "{base}[instructions]\nsrc = \"i.md\"\nper_machine = \"{{machine}}/{{flock}}.md\"\n"
                ),
                "only {machine}",
            ),
            (
                &format!("{base}[settings.vim]\nsrc = \"s.json\"\n"),
                "not an agent kind",
            ),
            (
                &format!("{base}[[mcp]]\nname = \"a b\"\ncommand = \"x\"\n"),
                "must match",
            ),
            (
                &format!("{base}[[mcp]]\nname = \"a\"\n"),
                "a command or a url",
            ),
            (
                &format!("{base}[[mcp]]\nname = \"a\"\ncommand = \"x\"\nurl = \"https://x\"\n"),
                "not both",
            ),
            (
                &format!("{base}[[mcp]]\nname = \"a\"\nurl = \"https://x\"\nargs = [\"y\"]\n"),
                "takes no args",
            ),
            (
                &format!(
                    "{base}[[mcp]]\nname = \"a\"\ncommand = \"x\"\n[[mcp]]\nname = \"a\"\ncommand = \"y\"\n"
                ),
                "a second server",
            ),
            (
                &format!("{base}[[mcp]]\nname = \"a\"\ncommand = \"x\"\nenv_from = [\"token\"]\n"),
                "environment variable name",
            ),
            (
                &format!(
                    "{base}[[mcp]]\nname = \"a\"\ncommand = \"x\"\nargs = [\"{{tikcet_url}}\"]\n"
                ),
                "{tikcet_url} is no var",
            ),
            (
                &format!(
                    "{base}[[plugin]]\nkind = \"codex\"\nname = \"p\"\nmarketplace = \"m\"\nsource = \"s\"\n"
                ),
                "only claude plugins",
            ),
            (
                &format!(
                    "{base}[[plugin]]\nkind = \"claude\"\nname = \"p q\"\nmarketplace = \"m\"\nsource = \"s\"\n"
                ),
                "one word",
            ),
            (
                &format!(
                    "{base}[[plugin]]\nkind = \"claude\"\nname = \"p\"\nmarketplace = \"m\"\nsource = \"s\"\nref = \"-x\"\n"
                ),
                "not a branch",
            ),
            (&format!("{base}{item}{item}"), "used twice"),
            (
                &format!("{base}[[require]]\nid = \"r\"\ncommand = []\n"),
                "needs a program",
            ),
            (
                "min_pastor_version = \"99.0.0\"\nid = \"b\"\nversion = \"1\"\nnew_key = 1\n",
                "needs pastor 99.0.0",
            ),
        ];
        for (text, want) in cases {
            let err = Manifest::parse(text).unwrap_err();
            assert!(err.contains(want), "{text}\n=> {err}\nwanted {want:?}");
        }
    }

    /// What only the files can tell: a missing `src`, a skill without
    /// SKILL.md, a fragment that does not parse or sets the profile's keys.
    #[test]
    fn the_files_must_be_there_and_parse() {
        // (the manifest's items, the files beside it, what the error says)
        type Case<'a> = (&'a str, &'a [(&'a str, &'a str)], &'a str);
        let base = "id = \"b\"\nversion = \"1\"\n";
        let cases: &[Case] = &[
            ("[[skill]]\nsrc = \"s\"\n", &[], "not a directory"),
            (
                "[[skill]]\nsrc = \"s\"\n",
                &[("s/README.md", "x")],
                "no SKILL.md",
            ),
            ("[[file]]\nsrc = \"f\"\nslot = \"bin\"\n", &[], "not a file"),
            ("[instructions]\nsrc = \"i.md\"\n", &[], "not a file"),
            (
                "[settings.claude]\nsrc = \"c.json\"\n",
                &[("c.json", "{")],
                "not JSON",
            ),
            (
                "[settings.claude]\nsrc = \"c.json\"\n",
                &[("c.json", "[1]")],
                "a JSON object",
            ),
            (
                "[settings.codex]\nsrc = \"c.toml\"\n",
                &[("c.toml", "= x")],
                "not TOML",
            ),
            (
                "[settings.claude]\nsrc = \"c.json\"\n",
                &[("c.json", "{\"permissions\": {}}")],
                "sets permissions",
            ),
            (
                "[settings.agy]\nsrc = \"a.json\"\n",
                &[("a.json", "{\"toolPermission\": \"x\"}")],
                "sets toolPermission",
            ),
            (
                "[settings.opencode]\nsrc = \"o.json\"\n",
                &[("o.json", "{\"permission\": {}}")],
                "sets permission",
            ),
        ];
        for (items, files, want) in cases {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join(MANIFEST_FILE), format!("{base}{items}")).unwrap();
            for (rel, text) in *files {
                let p = tmp.path().join(rel);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(p, text).unwrap();
            }
            let err = Manifest::load(tmp.path()).unwrap_err();
            assert!(err.contains(want), "{items}\n=> {err}\nwanted {want:?}");
        }
    }

    /// A workdir bundle laid out on disk: a skill, a hook script, an agent
    /// file only for claude, instructions, and settings for both kinds.
    pub(crate) fn write_workdir_example(dir: &Path) {
        let put = |rel: &str, text: &str| {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        };
        put(
            "skills/reviewing-a-pr/SKILL.md",
            "---\nname: reviewing-a-pr\n---\nRead the diff.\n",
        );
        put("skills/reviewing-a-pr/notes/style.md", "Short.\n");
        put("agents/checker.md", "Check {ticket_url}.\n");
        put("instructions/repo.md", "Tickets live at {ticket_url}.\n");
        put(
            "instructions/machines/build-01.md",
            "This one has the GPUs.\n",
        );
        put(
            "settings/claude.json",
            "{\"env\": {\"TICKETS\": \"{ticket_url}\"}, \"permissions\": {\"additionalDirectories\": [\"{home}/notes\"]}}",
        );
        put(MANIFEST_FILE, WORKDIR_EXAMPLE);
    }

    pub(crate) const WORKDIR_EXAMPLE: &str = r#"
id = "repo-notes"
version = "1"
target = "workdir"

[vars]
ticket_url = { description = "the team's ticket tracker URL" }

[[skill]]
src = "skills/reviewing-a-pr"

[[file]]
src = "agents/checker.md"
path = ".claude/agents/checker.md"
kinds = ["claude"]
render = true

[instructions]
src = "instructions/repo.md"
per_machine = "instructions/machines/{machine}.md"

[settings.claude]
src = "settings/claude.json"
"#;

    /// `target = "workdir"` loads with a set's keys: a file's `path`,
    /// `kinds` and `render`; `permissions.additionalDirectories` in
    /// Claude's settings, which a set may carry. A home bundle is unchanged.
    #[test]
    fn a_workdir_bundle_loads() {
        let tmp = tempfile::tempdir().unwrap();
        write_workdir_example(tmp.path());
        let m = Manifest::load(tmp.path()).unwrap();
        assert_eq!(m.target(), Target::Workdir);
        assert_eq!(
            m.files[0].path.as_deref(),
            Some(".claude/agents/checker.md")
        );
        assert_eq!(m.files[0].render, Some(true));
        assert!(m.files[0].slot.is_none());

        let home = tempfile::tempdir().unwrap();
        write_example(home.path());
        assert_eq!(Manifest::load(home.path()).unwrap().target(), Target::Home);
    }

    /// What a workdir bundle may not carry, and what a home bundle's file
    /// may not take.
    #[test]
    fn a_workdir_bundle_takes_a_sets_keys_only() {
        let base = "id = \"b\"\nversion = \"1\"\ntarget = \"workdir\"\n";
        let home = "id = \"b\"\nversion = \"1\"\n";
        let cases: &[(String, &str)] = &[
            (
                format!("{base}[[file]]\nsrc = \"f\"\nslot = \"hooks\"\n"),
                "give `path`, not `slot`",
            ),
            (format!("{base}[[file]]\nsrc = \"f\"\n"), "needs `path`"),
            (
                format!(
                    "{base}[[file]]\nsrc = \"a\"\npath = \"x\"\n[[file]]\nsrc = \"b\"\npath = \"x\"\n"
                ),
                "a second file at \"x\"",
            ),
            (
                format!("{base}[[file]]\nsrc = \"f\"\npath = \"x\"\nkinds = []\n"),
                "rather than empty",
            ),
            (
                format!("{base}[[mcp]]\nname = \"a\"\ncommand = \"x\"\n"),
                "no file of its own for MCP servers",
            ),
            (
                format!(
                    "{base}[[plugin]]\nkind = \"claude\"\nname = \"p\"\nmarketplace = \"m\"\nsource = \"s\"\n"
                ),
                "carry it in a home bundle",
            ),
            (
                format!("{base}[[require]]\nid = \"r\"\ncommand = [\"true\"]\n"),
                "carry it in one",
            ),
            (
                format!("{base}[settings.codex]\nsrc = \"c.toml\"\n"),
                "no settings file pastor merges into",
            ),
            (format!("{home}[[file]]\nsrc = \"f\"\n"), "needs `slot`"),
            (
                format!("{home}[[file]]\nsrc = \"f\"\nslot = \"bin\"\npath = \"x\"\n"),
                "only a workdir bundle's file takes path",
            ),
            (
                format!("{home}[[file]]\nsrc = \"f\"\nslot = \"bin\"\nrender = true\n"),
                "only a workdir bundle's file takes render",
            ),
            (format!("{home}target = \"elsewhere\"\n"), "unknown variant"),
        ];
        for (text, want) in cases {
            let err = Manifest::parse(text).unwrap_err();
            assert!(err.contains(want), "{text}\n=> {err}\nwanted {want:?}");
        }
    }

    /// What a workdir bundle's files must be: each set it gives is one a
    /// set's own checks pass, so a bad path, a harness file, the profile's
    /// keys or a secret is refused when it is added, not when a task starts.
    #[test]
    fn a_workdir_bundle_s_sets_are_checked() {
        type Case<'a> = (&'a str, &'a [(&'a str, &'a str)], &'a str);
        let base = "id = \"b\"\nversion = \"1\"\ntarget = \"workdir\"\n";
        let cases: &[Case] = &[
            (
                "[[file]]\nsrc = \"f\"\npath = \"../out\"\n",
                &[("f", "x")],
                "as a claude set",
            ),
            (
                "[[file]]\nsrc = \"f\"\npath = \".git/hooks/pre-commit\"\n",
                &[("f", "x")],
                ".git",
            ),
            (
                "[[file]]\nsrc = \"f\"\npath = \"AGENTS.md\"\n",
                &[("f", "x")],
                "as a agy set",
            ),
            (
                "[settings.claude]\nsrc = \"c.json\"\n",
                &[("c.json", "{\"permissions\": {\"allow\": [\"Bash\"]}}")],
                "workdir_permissions_owned",
            ),
            (
                "[[file]]\nsrc = \"f\"\npath = \"x\"\nkinds = [\"codex\"]\n",
                &[("f", "x")],
                "carries nothing for claude or agy",
            ),
            (
                "[[file]]\nsrc = \"f\"\npath = \"x\"\n",
                &[("f", "\u{fffe}")],
                "",
            ),
        ];
        for (items, files, want) in cases {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(tmp.path().join(MANIFEST_FILE), format!("{base}{items}")).unwrap();
            for (rel, text) in *files {
                let p = tmp.path().join(rel);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(p, text).unwrap();
            }
            let got = Manifest::load(tmp.path());
            if want.is_empty() {
                assert!(got.is_ok(), "{items}: {got:?}");
                continue;
            }
            let err = got.unwrap_err();
            assert!(err.contains(want), "{items}\n=> {err}\nwanted {want:?}");
        }
        // A file that is not text cannot be a set's.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(MANIFEST_FILE),
            format!("{base}[[file]]\nsrc = \"f\"\npath = \"x\"\n"),
        )
        .unwrap();
        std::fs::write(tmp.path().join("f"), [0xff, 0xfe, 0x00]).unwrap();
        let err = Manifest::load(tmp.path()).unwrap_err();
        assert!(err.contains("are text"), "{err}");
    }

    #[test]
    fn placeholders_are_var_names_in_braces() {
        assert_eq!(
            placeholders("{home}/x/{ticket_url}"),
            ["home", "ticket_url"]
        );
        assert!(placeholders("{\"json\": 1} {A} {}").is_empty());
    }
}
