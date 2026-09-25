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

/// One `[[flock]]` entry. A flock is only a name for now; which machines are
/// in it is each machine's `flock` field, so a machine is in exactly one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlockEntry {
    pub name: String,
    /// Where tasks and jobs that name no flock go. Exactly one entry has it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub default: bool,
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
