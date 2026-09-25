use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

fn default_session() -> String {
    "default".to_string()
}
fn default_max_agents() -> u32 {
    2
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineConfig {
    pub name: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub local: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<String>,
    /// Developer option: argv speaking the herdr protocol on stdio.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(default = "default_session")]
    pub session: String,
    #[serde(default = "default_max_agents")]
    pub max_agents: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// The flock this machine belongs to; `None` is the default flock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flock: Option<String>,
}

/// The flock a file with no `[[flock]]` entry has: every machine is in it.
pub const DEFAULT_FLOCK: &str = "default";

/// One `[[flock]]` entry: a name, and the agent its tasks get when the task
/// or job says nothing. Which machines are in it is each machine's `flock`
/// field, so a machine is in exactly one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlockEntry {
    pub name: String,
    /// Where tasks and jobs that name no flock go. Exactly one entry has it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub default: bool,
    /// The agent for this flock's tasks that name none; `None` falls through
    /// to `[defaults] agent` (see `Defaults::resolve_agent`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Like `agent`, for the agent's args; `[]` means none, not `[defaults]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_args: Option<Vec<String>>,
    /// Tool patterns this flock's agents may use, on top of `[defaults]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    /// Tool patterns this flock's agents must not use, on top of `[defaults]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
}

/// Why a task cannot have the flock it asked for (`Flock::task_flock`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TaskFlockError {
    #[error("flock {0} does not exist")]
    UnknownFlock(String),
    #[error("machine {machine} is in flock {flock}, not {requested}")]
    MachineElsewhere {
        machine: String,
        flock: String,
        requested: String,
    },
}

/// `flock.toml`: the declared flocks and the machines, each in one of them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Flock {
    /// Empty means one implicit flock, `DEFAULT_FLOCK`, holding every machine.
    #[serde(default, rename = "flock", skip_serializing_if = "Vec::is_empty")]
    pub flocks: Vec<FlockEntry>,
    #[serde(default, rename = "machine")]
    pub machines: Vec<MachineConfig>,
}

impl Flock {
    pub fn load(path: &Path) -> anyhow::Result<Flock> {
        if !path.exists() {
            return Ok(Flock::default());
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let flock: Flock =
            toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        flock
            .validate()
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        Ok(flock)
    }

    /// Like `load`, but a missing file is an error rather than the defaults:
    /// for a reload, where the `exists()` check and the read used to race a
    /// concurrent delete-and-rewrite and could momentarily see no machines.
    /// One read; the error keeps `std::io::ErrorKind::NotFound` at the top so
    /// callers can tell "missing" from "does not parse".
    pub fn load_existing(path: &Path) -> anyhow::Result<Flock> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow::Error::new(e)
            } else {
                anyhow::Error::new(e).context(format!("read {}", path.display()))
            }
        })?;
        let flock: Flock =
            toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        flock
            .validate()
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        Ok(flock)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        self.validate().map_err(|e| anyhow::anyhow!(e))?;
        if let Some(parent) = path.parent() {
            crate::config::create_private_dir(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
        Ok(())
    }

    pub fn validate(&self) -> Result<(), String> {
        let mut names = std::collections::HashSet::new();
        for f in &self.flocks {
            if f.name.is_empty() {
                return Err("flock with empty name".into());
            }
            if !names.insert(&f.name) {
                return Err(format!("flock {} listed twice", f.name));
            }
            crate::config::check_tools(&format!("flock {}: allow", f.name), &f.allow)?;
            crate::config::check_tools(&format!("flock {}: deny", f.name), &f.deny)?;
        }
        if !self.flocks.is_empty() {
            let defaults: Vec<&str> = self
                .flocks
                .iter()
                .filter(|f| f.default)
                .map(|f| f.name.as_str())
                .collect();
            match defaults[..] {
                [_] => {}
                [] => return Err("no flock has default = true; exactly one must".into()),
                [a, b, ..] => {
                    return Err(format!(
                        "flocks {a} and {b} are both default; exactly one may be"
                    ));
                }
            }
        }
        let mut seen = std::collections::HashSet::new();
        for m in &self.machines {
            if m.name.is_empty() {
                return Err("machine with empty name".into());
            }
            if !seen.insert(&m.name) {
                return Err(format!("machine {} listed twice", m.name));
            }
            let ways = m.local as u8 + m.ssh.is_some() as u8 + m.command.is_some() as u8;
            if ways != 1 {
                return Err(format!(
                    "machine {}: set exactly one of local, ssh, command",
                    m.name
                ));
            }
            if m.command.as_ref().is_some_and(|c| c.is_empty()) {
                return Err(format!("machine {}: command is empty", m.name));
            }
            if m.max_agents == 0 {
                return Err(format!("machine {}: max_agents must be at least 1", m.name));
            }
            if let Some(f) = &m.flock
                && !self.has_flock(f)
            {
                return Err(format!("machine {}: flock {f} is not declared", m.name));
            }
        }
        Ok(())
    }

    /// The flock tasks and jobs that name none go to.
    pub fn default_flock(&self) -> &str {
        self.flocks
            .iter()
            .find(|f| f.default)
            .map_or(DEFAULT_FLOCK, |f| f.name.as_str())
    }

    /// Every flock in file order; the implicit one when none is declared.
    pub fn flock_names(&self) -> Vec<&str> {
        if self.flocks.is_empty() {
            vec![DEFAULT_FLOCK]
        } else {
            self.flocks.iter().map(|f| f.name.as_str()).collect()
        }
    }

    /// The `[[flock]]` entry named `name`; `None` for the implicit flock of
    /// a file that declares none, which has no settings.
    pub fn entry(&self, name: &str) -> Option<&FlockEntry> {
        self.flocks.iter().find(|f| f.name == name)
    }

    pub fn has_flock(&self, name: &str) -> bool {
        self.flock_names().contains(&name)
    }

    /// The flock `m` belongs to.
    pub fn flock_of<'a>(&'a self, m: &'a MachineConfig) -> &'a str {
        m.flock.as_deref().unwrap_or_else(|| self.default_flock())
    }

    /// The flock of the machine named `name`, `None` when there is no such
    /// machine.
    pub fn machine_flock(&self, name: &str) -> Option<&str> {
        self.get(name).map(|m| self.flock_of(m))
    }

    /// The flock a new task targets: the one it names, else the flock of the
    /// machine it is pinned to, else the default. A named flock must exist,
    /// and a pinned machine must be in it. A pin to a machine this file does
    /// not have settles nothing: the caller refuses that machine itself.
    pub fn task_flock(
        &self,
        requested: Option<&str>,
        pinned: Option<&str>,
    ) -> Result<String, TaskFlockError> {
        if let Some(f) = requested
            && !self.has_flock(f)
        {
            return Err(TaskFlockError::UnknownFlock(f.to_string()));
        }
        let pinned = pinned.and_then(|m| Some((m, self.machine_flock(m)?)));
        match (requested, pinned) {
            (Some(want), Some((machine, flock))) if want != flock => {
                Err(TaskFlockError::MachineElsewhere {
                    machine: machine.to_string(),
                    flock: flock.to_string(),
                    requested: want.to_string(),
                })
            }
            (Some(want), _) => Ok(want.to_string()),
            (None, Some((_, flock))) => Ok(flock.to_string()),
            (None, None) => Ok(self.default_flock().to_string()),
        }
    }

    pub fn get(&self, name: &str) -> Option<&MachineConfig> {
        self.machines.iter().find(|m| m.name == name)
    }

    pub fn add(&mut self, m: MachineConfig) -> Result<(), String> {
        if self.get(&m.name).is_some() {
            return Err(format!("machine {} already exists", m.name));
        }
        self.machines.push(m);
        if let Err(e) = self.validate() {
            self.machines.pop();
            return Err(e);
        }
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.machines.len();
        self.machines.retain(|m| m.name != name);
        self.machines.len() != before
    }
}

/// Why an edit of flock.toml was refused. Each has a stable CLI code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EditError {
    #[error("machine {0} already exists")]
    MachineExists(String),
    #[error("machine {0} not found")]
    UnknownMachine(String),
    #[error("flock {0} already exists")]
    FlockExists(String),
    #[error("flock {0} does not exist")]
    UnknownFlock(String),
    #[error("flock {flock} still has machines: {}; move them first", machines.join(", "))]
    FlockHasMachines {
        flock: String,
        machines: Vec<String>,
    },
    #[error("flock {flock} still has queued tasks: {}; close them first", tasks.join(", "))]
    FlockHasTasks { flock: String, tasks: Vec<String> },
    #[error("flock {0} is the default; make another flock the default first")]
    RemovingDefault(String),
    #[error("{0}")]
    Invalid(String),
}

impl EditError {
    pub fn code(&self) -> &'static str {
        match self {
            EditError::MachineExists(_) => "machine_exists",
            EditError::UnknownMachine(_) => "unknown_machine",
            EditError::FlockExists(_) => "flock_exists",
            EditError::UnknownFlock(_) => "unknown_flock",
            EditError::FlockHasMachines { .. } => "flock_not_empty",
            EditError::FlockHasTasks { .. } => "flock_has_tasks",
            EditError::RemovingDefault(_) => "flock_is_default",
            EditError::Invalid(_) => "config_error",
        }
    }
}

/// What `flock add` did with the machines that name no flock of their own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlockAdded {
    /// The flock they are in after the edit.
    pub flock: String,
    pub machines: Vec<String>,
    /// They followed the new flock, rather than staying where they were.
    pub moved: bool,
}

/// flock.toml opened for an edit that keeps everything it does not touch:
/// comments, key order, blank lines. `machine add|remove|move` and the
/// `flock` commands go through it; `Flock::save` would rewrite the file from
/// scratch. Each edit checks what it needs against the file as it stands,
/// and `save` checks the result loads before it replaces the file.
pub struct FlockDoc {
    doc: toml_edit::DocumentMut,
}

impl FlockDoc {
    /// A missing file is an empty one, as for `Flock::load`. A file that
    /// does not load is refused: an edit must not paper over it.
    pub fn open(path: &Path) -> anyhow::Result<FlockDoc> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        let doc: toml_edit::DocumentMut = text
            .parse()
            .with_context(|| format!("parse {}", path.display()))?;
        let d = FlockDoc { doc };
        d.flock()
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        Ok(d)
    }

    pub fn parse(text: &str) -> anyhow::Result<FlockDoc> {
        let d = FlockDoc { doc: text.parse()? };
        d.flock().map_err(anyhow::Error::msg)?;
        Ok(d)
    }

    /// The file as it now reads, validated.
    pub fn flock(&self) -> Result<Flock, String> {
        let f: Flock = toml::from_str(&self.doc.to_string()).map_err(|e| e.to_string())?;
        f.validate()?;
        Ok(f)
    }

    /// Write the edited file in place of `path` (a temp file, then a rename,
    /// so a reader never sees half of it). Refused if it would not load.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        self.flock().map_err(anyhow::Error::msg)?;
        if let Some(parent) = path.parent() {
            crate::config::create_private_dir(parent)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, self.to_string())
            .with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
        Ok(())
    }

    fn current(&self) -> Result<Flock, EditError> {
        self.flock().map_err(EditError::Invalid)
    }

    /// Every table in the document with its position, for placing new ones.
    fn positions(&self) -> Vec<(bool, isize)> {
        let mut out = Vec::new();
        for (key, item) in self.doc.as_table().iter() {
            let is_flock = key == "flock";
            match item {
                toml_edit::Item::ArrayOfTables(a) => {
                    out.extend(a.iter().filter_map(|t| Some((is_flock, t.position()?))))
                }
                toml_edit::Item::Table(t) => out.extend(t.position().map(|p| (is_flock, p))),
                _ => {}
            }
        }
        out
    }

    fn array(&mut self, key: &str) -> &mut toml_edit::ArrayOfTables {
        let item = self
            .doc
            .as_table_mut()
            .entry(key)
            .or_insert_with(|| toml_edit::Item::ArrayOfTables(Default::default()));
        item.as_array_of_tables_mut()
            .expect("checked by flock(): the key holds an array of tables")
    }

    /// A new `[[<key>]]` table: flocks after the last flock (or before
    /// everything when there is none, so they head the file), machines at
    /// the end. Separated from what comes before by a blank line.
    fn push(&mut self, key: &str, mut table: toml_edit::Table) {
        let pos = self.positions();
        let at = if key == "flock" {
            pos.iter()
                .filter(|(f, _)| *f)
                .map(|(_, p)| *p)
                .max()
                .unwrap_or_else(|| pos.iter().map(|(_, p)| *p).min().unwrap_or(0) - 1)
        } else {
            pos.iter().map(|(_, p)| *p).max().unwrap_or(0) + 1
        };
        table.set_position(Some(at));
        let first = pos.iter().all(|(_, p)| *p > at);
        let blank = self.doc.to_string().trim().is_empty();
        let prefix = match self.first_table_mut() {
            // The new table heads the file, so the file's header comment
            // (the old first table's prefix, up to its last blank line)
            // moves up with it; what follows the blank line stays with the
            // table it was written above.
            Some(old) if first => {
                let p = old.decor().prefix().and_then(|p| p.as_str()).unwrap_or("");
                let (header, rest) = match p.rfind("\n\n") {
                    Some(i) => (format!("{}\n", &p[..=i]), p[i + 2..].to_string()),
                    None => (String::new(), p.to_string()),
                };
                old.decor_mut().set_prefix(format!("\n{rest}"));
                header
            }
            Some(_) => "\n".to_string(),
            None if blank => String::new(),
            None => "\n".to_string(),
        };
        table.decor_mut().set_prefix(prefix);
        self.array(key).push(table);
    }

    /// The table that comes first in the file.
    fn first_table_mut(&mut self) -> Option<&mut toml_edit::Table> {
        let mut tables: Vec<&mut toml_edit::Table> = Vec::new();
        for (_, item) in self.doc.as_table_mut().iter_mut() {
            match item {
                toml_edit::Item::ArrayOfTables(a) => tables.extend(a.iter_mut()),
                toml_edit::Item::Table(t) => tables.push(t),
                _ => {}
            }
        }
        tables
            .into_iter()
            .filter(|t| t.position().is_some())
            .min_by_key(|t| t.position())
    }

    fn tables_mut<'a>(
        &'a mut self,
        key: &str,
    ) -> impl Iterator<Item = &'a mut toml_edit::Table> + 'a {
        self.doc
            .get_mut(key)
            .and_then(|i| i.as_array_of_tables_mut())
            .into_iter()
            .flat_map(|a| a.iter_mut())
    }

    fn machine_mut(&mut self, name: &str) -> Option<&mut toml_edit::Table> {
        self.tables_mut("machine")
            .find(|t| t.get("name").and_then(|v| v.as_str()) == Some(name))
    }

    fn remove_named(&mut self, key: &str, name: &str) {
        if let Some(a) = self
            .doc
            .get_mut(key)
            .and_then(|i| i.as_array_of_tables_mut())
        {
            a.retain(|t| t.get("name").and_then(|v| v.as_str()) != Some(name));
        }
    }

    /// With no `[[flock]]` entry the file has one implicit flock; naming a
    /// second one needs the first on paper, as the default.
    fn declare_implicit(&mut self, f: &Flock) {
        if f.flocks.is_empty() {
            let mut t = toml_edit::Table::new();
            t.insert("name", toml_edit::value(DEFAULT_FLOCK));
            t.insert("default", toml_edit::value(true));
            self.push("flock", t);
        }
    }

    pub fn add_machine(&mut self, m: &MachineConfig) -> Result<(), EditError> {
        let f = self.current()?;
        if f.get(&m.name).is_some() {
            return Err(EditError::MachineExists(m.name.clone()));
        }
        if let Some(name) = &m.flock
            && !f.has_flock(name)
        {
            return Err(EditError::UnknownFlock(name.clone()));
        }
        let mut probe = f.clone();
        probe.machines.push(m.clone());
        probe.validate().map_err(EditError::Invalid)?;
        let text = toml::to_string(m).map_err(|e| EditError::Invalid(e.to_string()))?;
        let table: toml_edit::DocumentMut = text
            .parse()
            .map_err(|e: toml_edit::TomlError| EditError::Invalid(e.to_string()))?;
        self.push("machine", table.as_table().clone());
        Ok(())
    }

    pub fn remove_machine(&mut self, name: &str) -> Result<(), EditError> {
        if self.current()?.get(name).is_none() {
            return Err(EditError::UnknownMachine(name.into()));
        }
        self.remove_named("machine", name);
        Ok(())
    }

    /// `machine move`: put `name` in `flock`, by name, so it stays there
    /// whichever flock is the default later.
    pub fn move_machine(&mut self, name: &str, flock: &str) -> Result<(), EditError> {
        let f = self.current()?;
        if f.get(name).is_none() {
            return Err(EditError::UnknownMachine(name.into()));
        }
        if !f.has_flock(flock) {
            return Err(EditError::UnknownFlock(flock.into()));
        }
        let t = self.machine_mut(name).expect("checked above");
        t.insert("flock", toml_edit::value(flock));
        Ok(())
    }

    /// `flock add`. A file with only the implicit flock gets it declared
    /// first, so its machines keep their flock; unless the new flock is to
    /// be the default, which then takes the implicit one's place and its
    /// machines. The implicit flock is still declared when a machine names
    /// it, and that machine stays in it.
    pub fn add_flock(&mut self, name: &str, default: bool) -> Result<FlockAdded, EditError> {
        let f = self.current()?;
        if f.has_flock(name) {
            return Err(EditError::FlockExists(name.into()));
        }
        if name.is_empty() {
            return Err(EditError::Invalid("flock with empty name".into()));
        }
        let machines: Vec<String> = f
            .machines
            .iter()
            .filter(|m| m.flock.is_none())
            .map(|m| m.name.clone())
            .collect();
        if default && f.flocks.is_empty() {
            if f.machines.iter().any(|m| m.flock.is_some()) {
                let mut t = toml_edit::Table::new();
                t.insert("name", toml_edit::value(DEFAULT_FLOCK));
                self.push("flock", t);
            }
            let mut t = toml_edit::Table::new();
            t.insert("name", toml_edit::value(name));
            t.insert("default", toml_edit::value(true));
            self.push("flock", t);
            return Ok(FlockAdded {
                flock: name.into(),
                machines,
                moved: true,
            });
        }
        self.declare_implicit(&f);
        let mut t = toml_edit::Table::new();
        t.insert("name", toml_edit::value(name));
        self.push("flock", t);
        if default {
            self.set_default(name)?;
        }
        Ok(FlockAdded {
            flock: f.default_flock().into(),
            machines,
            moved: false,
        })
    }

    /// `flock remove`: refused while machines are in it, while `queued`
    /// (the ids of the queued tasks that name it) is not empty, or while it
    /// is the default. Dispatch only looks in declared flocks, so a queued
    /// task left naming a removed one would wait forever.
    pub fn remove_flock(&mut self, name: &str, queued: &[String]) -> Result<(), EditError> {
        let f = self.current()?;
        // `has_flock` counts the implicit `default` of a file with no
        // `[[flock]]`, so removing it reads as removing the default.
        if !f.has_flock(name) {
            return Err(EditError::UnknownFlock(name.into()));
        }
        if f.default_flock() == name {
            return Err(EditError::RemovingDefault(name.into()));
        }
        let machines: Vec<String> = f
            .machines
            .iter()
            .filter(|m| f.flock_of(m) == name)
            .map(|m| m.name.clone())
            .collect();
        if !machines.is_empty() {
            return Err(EditError::FlockHasMachines {
                flock: name.into(),
                machines,
            });
        }
        if !queued.is_empty() {
            return Err(EditError::FlockHasTasks {
                flock: name.into(),
                tasks: queued.to_vec(),
            });
        }
        self.remove_named("flock", name);
        Ok(())
    }

    /// `flock default`: tasks and jobs that name no flock go to `name` from
    /// now on. A machine with no `flock` of its own is in the default flock,
    /// so each one is first written into the flock it is in now: changing
    /// where new work goes must not move machines.
    pub fn set_default(&mut self, name: &str) -> Result<(), EditError> {
        let f = self.current()?;
        if !f.has_flock(name) {
            return Err(EditError::UnknownFlock(name.into()));
        }
        let old = f.default_flock().to_string();
        if old == name {
            return Ok(());
        }
        self.declare_implicit(&f);
        for m in f.machines.iter().filter(|m| m.flock.is_none()) {
            let t = self.machine_mut(&m.name).expect("listed by current()");
            t.insert("flock", toml_edit::value(old.as_str()));
        }
        for t in self.tables_mut("flock") {
            if t.get("name").and_then(|v| v.as_str()) == Some(name) {
                t.insert("default", toml_edit::value(true));
            } else {
                t.remove("default");
            }
        }
        Ok(())
    }
}

impl std::fmt::Display for FlockDoc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.doc.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pi(name: &str) -> MachineConfig {
        MachineConfig {
            name: name.into(),
            local: false,
            ssh: Some(format!("fleet@{name}")),
            command: None,
            session: "default".into(),
            max_agents: 2,
            tags: vec![],
            flock: None,
        }
    }

    fn flocks(text: &str) -> Result<Flock, String> {
        let f: Flock = toml::from_str(text).map_err(|e| e.to_string())?;
        f.validate()?;
        Ok(f)
    }

    #[test]
    fn parses_the_flocks_of_the_spec_example() {
        let f = flocks(
            r#"
[[flock]]
name = "personal"
default = true

[[flock]]
name = "work"

[[machine]]
name = "pi-1"
local = true

[[machine]]
name = "pi-3"
ssh = "user@pi-3"
flock = "work"
"#,
        )
        .unwrap();
        assert_eq!(f.default_flock(), "personal");
        assert_eq!(f.flock_names(), ["personal", "work"]);
        assert_eq!(f.machine_flock("pi-1"), Some("personal"));
        assert_eq!(f.machine_flock("pi-3"), Some("work"));
        assert_eq!(f.machine_flock("pi-9"), None);
        assert!(f.has_flock("work"));
        assert!(!f.has_flock("default"));
    }

    /// Flock files from before flocks keep working: one flock, `default`.
    #[test]
    fn no_flock_entry_is_one_flock_named_default() {
        let f = flocks("[[machine]]\nname = \"a\"\nlocal = true\n").unwrap();
        assert_eq!(f.default_flock(), DEFAULT_FLOCK);
        assert_eq!(f.flock_names(), [DEFAULT_FLOCK]);
        assert_eq!(f.machine_flock("a"), Some(DEFAULT_FLOCK));
        // Naming the implicit flock is allowed; any other name is not.
        flocks("[[machine]]\nname = \"a\"\nlocal = true\nflock = \"default\"\n").unwrap();
        let err =
            flocks("[[machine]]\nname = \"a\"\nlocal = true\nflock = \"work\"\n").unwrap_err();
        assert_eq!(err, "machine a: flock work is not declared");
        assert_eq!(Flock::default().default_flock(), DEFAULT_FLOCK);
    }

    #[test]
    fn flock_errors_are_config_errors() {
        let two =
            "[[flock]]\nname = \"a\"\ndefault = true\n[[flock]]\nname = \"b\"\ndefault = true\n";
        assert_eq!(
            flocks(two).unwrap_err(),
            "flocks a and b are both default; exactly one may be"
        );
        let none = "[[flock]]\nname = \"a\"\n[[flock]]\nname = \"b\"\n";
        assert_eq!(
            flocks(none).unwrap_err(),
            "no flock has default = true; exactly one must"
        );
        let dup = "[[flock]]\nname = \"a\"\ndefault = true\n[[flock]]\nname = \"a\"\n";
        assert_eq!(flocks(dup).unwrap_err(), "flock a listed twice");
        let empty = "[[flock]]\nname = \"\"\ndefault = true\n";
        assert_eq!(flocks(empty).unwrap_err(), "flock with empty name");
        let unknown = "[[flock]]\nname = \"a\"\ndefault = true\n[[machine]]\nname = \"m\"\nlocal = true\nflock = \"b\"\n";
        assert_eq!(
            flocks(unknown).unwrap_err(),
            "machine m: flock b is not declared"
        );
        // A typo in a key is a load error too, not a machine in the default flock.
        assert!(flocks("[[flock]]\nname = \"a\"\ndefualt = true\n").is_err());
    }

    #[test]
    fn a_bad_flock_fails_the_load_with_the_path() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("flock.toml");
        std::fs::write(&path, "[[flock]]\nname = \"a\"\n").unwrap();
        let err = format!("{:#}", Flock::load(&path).unwrap_err());
        assert!(err.contains("flock.toml"), "{err}");
        assert!(err.contains("no flock has default = true"), "{err}");
        assert!(Flock::load_existing(&path).is_err());
    }

    #[test]
    fn flocks_survive_a_save() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("flock.toml");
        let f = flocks(
            "[[flock]]\nname = \"home\"\ndefault = true\n[[flock]]\nname = \"work\"\n\n[[machine]]\nname = \"a\"\nlocal = true\nflock = \"work\"\n",
        )
        .unwrap();
        f.save(&path).unwrap();
        assert_eq!(Flock::load(&path).unwrap(), f);
    }

    fn home_and_work() -> Flock {
        flocks(
            "[[flock]]\nname = \"home\"\ndefault = true\n[[flock]]\nname = \"work\"\n\n[[machine]]\nname = \"h\"\nlocal = true\n\n[[machine]]\nname = \"w\"\nssh = \"w\"\nflock = \"work\"\n",
        )
        .unwrap()
    }

    /// Naming a flock picks it; a pinned machine settles it; neither is the
    /// default flock. A machine outside the flock asked for is refused.
    #[test]
    fn task_flock_follows_the_request_then_the_pin_then_the_default() {
        let f = home_and_work();
        assert_eq!(f.task_flock(None, None).unwrap(), "home");
        assert_eq!(f.task_flock(Some("work"), None).unwrap(), "work");
        assert_eq!(f.task_flock(None, Some("w")).unwrap(), "work");
        assert_eq!(f.task_flock(Some("work"), Some("w")).unwrap(), "work");
        assert_eq!(
            f.task_flock(Some("home"), Some("w")).unwrap_err(),
            TaskFlockError::MachineElsewhere {
                machine: "w".into(),
                flock: "work".into(),
                requested: "home".into(),
            }
        );
        assert_eq!(
            f.task_flock(Some("play"), None).unwrap_err(),
            TaskFlockError::UnknownFlock("play".into())
        );
        assert_eq!(
            f.task_flock(Some("home"), Some("w"))
                .unwrap_err()
                .to_string(),
            "machine w is in flock work, not home"
        );
        // A pin to a machine the file does not have settles nothing; the
        // caller refuses the machine on its own terms.
        assert_eq!(f.task_flock(None, Some("gone")).unwrap(), "home");
    }

    #[test]
    fn a_flock_entry_can_carry_an_agent_and_its_args() {
        let f = flocks(
            "[[flock]]\nname = \"home\"\ndefault = true\n[[flock]]\nname = \"work\"\nagent = \"codex\"\nagent_args = [\"--model\", \"gpt-x\"]\n",
        )
        .unwrap();
        let work = f.entry("work").unwrap();
        assert_eq!(work.agent.as_deref(), Some("codex"));
        assert_eq!(
            work.agent_args.as_deref(),
            Some(&["--model".to_string(), "gpt-x".to_string()][..])
        );
        let home = f.entry("home").unwrap();
        assert_eq!(
            (home.agent.as_ref(), home.agent_args.as_ref()),
            (None, None)
        );
        assert!(f.entry("play").is_none());
        // The implicit flock of a file with no `[[flock]]` has no entry.
        assert!(Flock::default().entry(DEFAULT_FLOCK).is_none());
    }

    #[test]
    fn a_flock_entry_can_carry_tool_lists_and_bad_patterns_fail_the_load() {
        let f = flocks(
            "[[flock]]\nname = \"home\"\ndefault = true\nallow = [\"Edit\"]\ndeny = [\"Bash(rm:*)\"]\n",
        )
        .unwrap();
        assert_eq!(f.entry("home").unwrap().allow, vec!["Edit"]);
        assert_eq!(f.entry("home").unwrap().deny, vec!["Bash(rm:*)"]);
        let err =
            flocks("[[flock]]\nname = \"home\"\ndefault = true\ndeny = [\"-x\"]\n").unwrap_err();
        assert!(err.contains("flock home: deny"), "{err}");
    }

    const COMMENTED: &str = "# my fleet\n\n[[machine]]\nname = \"pi-1\"   # the desk one\nlocal = true\n\n# spare\n[[machine]]\nname = \"pi-3\"\nssh = \"user@pi-3\"\n";

    #[test]
    fn edits_keep_comments_and_layout() {
        let mut d = FlockDoc::parse(COMMENTED).unwrap();
        d.add_machine(&pi("pi-4")).unwrap();
        let text = d.to_string();
        assert!(text.starts_with(COMMENTED), "{text}");
        assert!(
            text.contains("\n\n[[machine]]\nname = \"pi-4\"\n"),
            "{text}"
        );
        d.remove_machine("pi-4").unwrap();
        assert_eq!(d.to_string(), COMMENTED);
        assert_eq!(
            d.remove_machine("pi-9").unwrap_err(),
            EditError::UnknownMachine("pi-9".into())
        );
        assert_eq!(
            d.add_machine(&pi("pi-3")).unwrap_err(),
            EditError::MachineExists("pi-3".into())
        );
    }

    /// The first named flock puts the implicit one on paper, as the default,
    /// at the head of the file; the machines stay where they were.
    #[test]
    fn adding_a_flock_declares_the_implicit_default_first() {
        let mut d = FlockDoc::parse(COMMENTED).unwrap();
        d.add_flock("work", false).unwrap();
        let text = d.to_string();
        assert!(
            text.starts_with("# my fleet\n\n[[flock]]\nname = \"default\"\ndefault = true\n\n[[flock]]\nname = \"work\"\n\n[[machine]]\nname = \"pi-1\"   # the desk one\n"),
            "{text}"
        );
        let f = d.flock().unwrap();
        assert_eq!(f.flock_names(), ["default", "work"]);
        assert_eq!(f.machine_flock("pi-1"), Some("default"));
        d.add_flock("play", false).unwrap();
        assert_eq!(
            d.flock().unwrap().flock_names(),
            ["default", "work", "play"]
        );
        assert!(
            d.to_string()
                .contains("name = \"work\"\n\n[[flock]]\nname = \"play\"\n\n[[machine]]"),
            "{d}"
        );
        assert_eq!(
            d.add_flock("work", false).unwrap_err(),
            EditError::FlockExists("work".into())
        );
    }

    /// Without `--default` the first named flock leaves the machines in the
    /// implicit one, and says so.
    #[test]
    fn a_first_flock_reports_the_machines_stay_in_default() {
        let mut d = FlockDoc::parse(COMMENTED).unwrap();
        assert_eq!(
            d.add_flock("work", false).unwrap(),
            FlockAdded {
                flock: "default".into(),
                machines: vec!["pi-1".into(), "pi-3".into()],
                moved: false,
            }
        );
        assert_eq!(d.flock().unwrap().machine_flock("pi-1"), Some("default"));
    }

    /// The first named flock added as the default replaces the implicit one:
    /// the machines that name no flock follow it, and no `default` flock is
    /// written down to sit empty.
    #[test]
    fn a_first_default_flock_takes_the_machines_along() {
        let mut d = FlockDoc::parse(COMMENTED).unwrap();
        assert_eq!(
            d.add_flock("personal", true).unwrap(),
            FlockAdded {
                flock: "personal".into(),
                machines: vec!["pi-1".into(), "pi-3".into()],
                moved: true,
            }
        );
        let f = d.flock().unwrap();
        assert_eq!(f.flock_names(), ["personal"]);
        assert_eq!(f.default_flock(), "personal");
        assert_eq!(f.machine_flock("pi-1"), Some("personal"));
        assert_eq!(f.machine_flock("pi-3"), Some("personal"));
        let text = d.to_string();
        assert!(
            text.starts_with("# my fleet\n\n[[flock]]\nname = \"personal\"\ndefault = true\n\n[[machine]]\nname = \"pi-1\"   # the desk one\n"),
            "{text}"
        );
        assert!(!text.contains("flock = "), "{text}");

        // A machine that names the implicit flock stays in it, so that one
        // is declared, just not as the default.
        let named = format!(
            "{COMMENTED}\n[[machine]]\nname = \"pi-5\"\nlocal = true\nflock = \"default\"\n"
        );
        let mut d = FlockDoc::parse(&named).unwrap();
        let added = d.add_flock("personal", true).unwrap();
        assert_eq!(added.machines, ["pi-1", "pi-3"]);
        assert!(added.moved);
        let f = d.flock().unwrap();
        assert_eq!(f.flock_names(), ["default", "personal"]);
        assert_eq!(f.default_flock(), "personal");
        assert_eq!(f.machine_flock("pi-1"), Some("personal"));
        assert_eq!(f.machine_flock("pi-5"), Some("default"));

        // With no machines there is nothing to move.
        let mut d = FlockDoc::parse("").unwrap();
        assert_eq!(
            d.add_flock("personal", true).unwrap().machines,
            Vec::<String>::new()
        );
        assert_eq!(d.flock().unwrap().flock_names(), ["personal"]);
    }

    #[test]
    fn moving_a_machine_names_its_flock() {
        let mut d = FlockDoc::parse(COMMENTED).unwrap();
        assert_eq!(
            d.move_machine("pi-3", "work").unwrap_err(),
            EditError::UnknownFlock("work".into())
        );
        d.add_flock("work", false).unwrap();
        d.move_machine("pi-3", "work").unwrap();
        assert_eq!(d.flock().unwrap().machine_flock("pi-3"), Some("work"));
        assert!(d.to_string().contains("# spare\n"), "{d}");
        assert_eq!(
            d.move_machine("pi-9", "work").unwrap_err(),
            EditError::UnknownMachine("pi-9".into())
        );
        let mut m = pi("pi-5");
        m.flock = Some("nope".into());
        assert_eq!(
            d.add_machine(&m).unwrap_err(),
            EditError::UnknownFlock("nope".into())
        );
        m.flock = Some("work".into());
        d.add_machine(&m).unwrap();
        assert_eq!(d.flock().unwrap().machine_flock("pi-5"), Some("work"));
    }

    /// Changing the default changes where new work goes, not where the
    /// machines are: those with no flock of their own get the old one.
    #[test]
    fn a_new_default_keeps_the_machines_in_their_flocks() {
        let mut d = FlockDoc::parse(COMMENTED).unwrap();
        d.add_flock("play", false).unwrap();
        assert_eq!(
            d.add_flock("work", true).unwrap(),
            FlockAdded {
                flock: "default".into(),
                machines: vec!["pi-1".into(), "pi-3".into()],
                moved: false,
            }
        );
        let f = d.flock().unwrap();
        assert_eq!(f.default_flock(), "work");
        assert_eq!(f.machine_flock("pi-1"), Some("default"));
        assert_eq!(f.machine_flock("pi-3"), Some("default"));
        d.set_default("default").unwrap();
        assert_eq!(d.flock().unwrap().default_flock(), "default");
        d.set_default("default").unwrap();
        assert_eq!(
            d.set_default("nope").unwrap_err(),
            EditError::UnknownFlock("nope".into())
        );
    }

    #[test]
    fn a_flock_is_removed_only_when_empty_idle_and_not_the_default() {
        let mut d = FlockDoc::parse(COMMENTED).unwrap();
        d.add_flock("work", false).unwrap();
        d.move_machine("pi-3", "work").unwrap();
        assert_eq!(
            d.remove_flock("work", &[]).unwrap_err(),
            EditError::FlockHasMachines {
                flock: "work".into(),
                machines: vec!["pi-3".into()],
            }
        );
        assert_eq!(
            d.remove_flock("default", &[]).unwrap_err(),
            EditError::RemovingDefault("default".into())
        );
        assert_eq!(
            d.remove_flock("nope", &[]).unwrap_err(),
            EditError::UnknownFlock("nope".into())
        );
        d.move_machine("pi-3", "default").unwrap();
        assert_eq!(
            d.remove_flock("work", &["t-7".into()]).unwrap_err(),
            EditError::FlockHasTasks {
                flock: "work".into(),
                tasks: vec!["t-7".into()],
            }
        );
        d.remove_flock("work", &[]).unwrap();
        assert_eq!(d.flock().unwrap().flock_names(), ["default"]);
    }

    #[test]
    fn the_implicit_default_flock_is_refused_as_the_default() {
        let mut d = FlockDoc::parse(COMMENTED).unwrap();
        assert_eq!(
            d.remove_flock("default", &[]).unwrap_err(),
            EditError::RemovingDefault("default".into())
        );
    }

    #[test]
    fn a_doc_saves_atomically_and_refuses_a_bad_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("flock.toml");
        let mut d = FlockDoc::open(&path).unwrap();
        d.add_machine(&pi("a")).unwrap();
        d.save(&path).unwrap();
        assert_eq!(Flock::load(&path).unwrap().machines, vec![pi("a")]);
        assert!(!path.with_extension("toml.tmp").exists());
        std::fs::write(&path, "[[flock]]\nname = \"x\"\n").unwrap();
        let err = format!("{:#}", FlockDoc::open(&path).err().unwrap());
        assert!(err.contains("no flock has default"), "{err}");
    }

    #[test]
    fn an_empty_command_is_refused() {
        let f = Flock {
            flocks: vec![],
            machines: vec![MachineConfig {
                ssh: None,
                command: Some(vec![]),
                ..pi("x")
            }],
        };
        assert_eq!(f.validate().unwrap_err(), "machine x: command is empty");
    }

    #[test]
    fn parses_spec_example() {
        let text = r#"
[[machine]]
name = "pi-1"
local = true
max_agents = 2

[[machine]]
name = "pi-3"
ssh = "fleet@pi-3"
session = "default"
max_agents = 3
tags = ["fast"]
"#;
        let f: Flock = toml::from_str(text).unwrap();
        assert_eq!(f.machines.len(), 2);
        assert!(f.machines[0].local);
        assert_eq!(f.machines[1].ssh.as_deref(), Some("fleet@pi-3"));
        assert_eq!(f.machines[1].tags, vec!["fast"]);
        f.validate().unwrap();
    }

    #[test]
    fn missing_file_is_empty_flock() {
        let tmp = tempfile::tempdir().unwrap();
        let f = Flock::load(&tmp.path().join("flock.toml")).unwrap();
        assert!(f.machines.is_empty());
    }

    /// `load_existing` is for a reload, where a missing file must not be
    /// mistaken for an intentionally emptied flock (Copilot 4103271200).
    #[test]
    fn load_existing_errors_on_a_missing_path_and_reads_an_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("flock.toml");
        let err = Flock::load_existing(&path).unwrap_err();
        assert_eq!(
            err.downcast_ref::<std::io::Error>().map(|e| e.kind()),
            Some(std::io::ErrorKind::NotFound)
        );

        std::fs::write(&path, "").unwrap();
        let f = Flock::load_existing(&path).unwrap();
        assert!(f.machines.is_empty());
    }

    #[test]
    fn save_then_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("flock.toml");
        let mut f = Flock::default();
        f.add(pi("pi-3")).unwrap();
        f.save(&path).unwrap();
        assert_eq!(Flock::load(&path).unwrap(), f);
    }

    #[test]
    fn validate_rejects_bad_machines() {
        let mut f = Flock::default();
        f.machines.push(MachineConfig {
            local: true,
            ..pi("both")
        });
        assert!(f.validate().unwrap_err().contains("both"));
        let mut f = Flock::default();
        f.machines.push(MachineConfig {
            ssh: None,
            ..pi("none")
        });
        assert!(f.validate().unwrap_err().contains("none"));
        let mut f = Flock::default();
        f.machines.push(pi("dup"));
        f.machines.push(pi("dup"));
        assert!(f.validate().unwrap_err().contains("dup"));
        let mut f = Flock::default();
        f.machines.push(MachineConfig {
            max_agents: 0,
            ..pi("zero")
        });
        assert!(f.validate().unwrap_err().contains("zero"));
    }

    #[test]
    fn add_rejects_duplicate_and_remove_reports() {
        let mut f = Flock::default();
        f.add(pi("a")).unwrap();
        assert!(f.add(pi("a")).is_err());
        assert!(f.remove("a"));
        assert!(!f.remove("a"));
    }

    #[test]
    fn add_rejects_invalid_machine_without_mutating() {
        let mut f = Flock::default();
        assert!(
            f.add(MachineConfig {
                local: true,
                ..pi("both")
            })
            .is_err()
        );
        assert!(f.machines.is_empty());

        let mut f = Flock::default();
        assert!(
            f.add(MachineConfig {
                max_agents: 0,
                ..pi("zero")
            })
            .is_err()
        );
        assert!(f.machines.is_empty());
    }
}
