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
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Flock {
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
            if m.max_agents == 0 {
                return Err(format!("machine {}: max_agents must be at least 1", m.name));
            }
        }
        Ok(())
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
        }
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
