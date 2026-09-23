pub mod flock;

use std::path::{Path, PathBuf};

use anyhow::Context;

/// Where pastor reads config and keeps state. Overridable by env for tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
}

impl Paths {
    pub fn from_env() -> anyhow::Result<Paths> {
        let config_dir = match std::env::var_os("PASTOR_CONFIG_DIR") {
            Some(v) => PathBuf::from(v),
            None => dirs::config_dir().context("no config dir")?.join("pastor"),
        };
        let state_dir = match std::env::var_os("PASTOR_STATE_DIR") {
            Some(v) => PathBuf::from(v),
            None => dirs::state_dir()
                .or_else(|| dirs::home_dir().map(|h| h.join(".local/state")))
                .context("no state dir")?
                .join("pastor"),
        };
        Ok(Paths {
            config_dir,
            state_dir,
        })
    }

    pub fn new(config_dir: impl Into<PathBuf>, state_dir: impl Into<PathBuf>) -> Paths {
        Paths {
            config_dir: config_dir.into(),
            state_dir: state_dir.into(),
        }
    }

    /// Create both directories with mode 0700. Idempotent.
    pub fn ensure(&self) -> anyhow::Result<()> {
        for dir in [&self.config_dir, &self.state_dir] {
            create_private_dir(dir)?;
        }
        Ok(())
    }

    pub fn flock_file(&self) -> PathBuf {
        self.config_dir.join("flock.toml")
    }
    pub fn db_file(&self) -> PathBuf {
        self.state_dir.join("pastor.db")
    }
    pub fn socket_file(&self) -> PathBuf {
        self.state_dir.join("pastor.sock")
    }
}

pub fn create_private_dir(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod {}", dir.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn from_env_uses_overrides() {
        let tmp = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("PASTOR_CONFIG_DIR", tmp.path().join("c"));
            std::env::set_var("PASTOR_STATE_DIR", tmp.path().join("s"));
        }
        let p = Paths::from_env().unwrap();
        assert_eq!(p.config_dir, tmp.path().join("c"));
        assert_eq!(p.state_dir, tmp.path().join("s"));
        unsafe {
            std::env::remove_var("PASTOR_CONFIG_DIR");
            std::env::remove_var("PASTOR_STATE_DIR");
        }
    }

    #[test]
    fn ensure_creates_private_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        p.ensure().unwrap();
        p.ensure().unwrap();
        for d in [&p.config_dir, &p.state_dir] {
            let mode = std::fs::metadata(d).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{}", d.display());
        }
        assert_eq!(p.flock_file(), tmp.path().join("c/flock.toml"));
        assert_eq!(p.db_file(), tmp.path().join("s/pastor.db"));
    }
}
