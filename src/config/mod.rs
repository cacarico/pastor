pub mod flock;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize};

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

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("pastor.toml")
    }

    /// Directory for the ssh `ControlMaster` sockets, one per machine. Created
    /// with mode 0700 by the transport when it first connects.
    pub fn ssh_dir(&self) -> PathBuf {
        self.state_dir.join("ssh")
    }

    /// ControlPath template for `machine`'s ssh master: the machine name plus
    /// ssh's own `%C`, which hashes the destination, user and port.
    ///
    /// The name alone would be wrong: retarget a machine (edit `ssh =`, or
    /// remove and re-add it against another host) and the next connection would
    /// reuse a master still attached to the *old* host for up to
    /// `ControlPersist` seconds. `%C` changes with the destination, so a
    /// retargeted machine simply gets a new master. It also separates two names
    /// that sanitise to the same thing (`a/b` and `a_b`).
    ///
    /// The name is still in there, sanitised to `[A-Za-z0-9._-]`, so the socket
    /// is recognisable and can never walk out of the directory; any `%` in the
    /// state dir is escaped as `%%`, or ssh would expand it.
    pub fn ssh_control_path(&self, machine: &str) -> PathBuf {
        let safe: String = machine
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "._-".contains(c) {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let dir = self.ssh_dir().to_string_lossy().replace('%', "%%");
        PathBuf::from(format!("{dir}/{safe}-%C"))
    }
}

pub fn create_private_dir(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // Called on every ssh connect, so the common case — it is already there and
    // already private — costs one stat instead of a create plus a chmod.
    if let Ok(md) = std::fs::metadata(dir)
        && md.is_dir()
        && md.permissions().mode() & 0o777 == 0o700
    {
        return Ok(());
    }
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod {}", dir.display()))?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Defaults {
    pub agent: String,
    pub max_tasks_per_run: u32,
    pub timeout: String,
}

impl Default for Defaults {
    fn default() -> Self {
        Defaults {
            agent: "claude".into(),
            max_tasks_per_run: 5,
            timeout: "2h".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PastorConfig {
    pub tick: String,
    pub settle: String,
    pub reconcile_every: String,
    pub defaults: Defaults,
}

impl Default for PastorConfig {
    fn default() -> Self {
        PastorConfig {
            tick: "10s".into(),
            settle: "10s".into(),
            reconcile_every: "60s".into(),
            defaults: Defaults::default(),
        }
    }
}

impl PastorConfig {
    pub fn load(path: &Path) -> anyhow::Result<PastorConfig> {
        if !path.exists() {
            return Ok(PastorConfig::default());
        }
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let cfg: PastorConfig =
            toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        // tick, settle and reconcile_every all drive `tokio::time::interval`,
        // which panics on a zero period. Reject zero here so a bad config
        // fails to load instead of crashing the daemon at startup.
        for (name, v, zero_ok) in [
            ("tick", &cfg.tick, false),
            ("settle", &cfg.settle, false),
            ("reconcile_every", &cfg.reconcile_every, false),
            ("defaults.timeout", &cfg.defaults.timeout, true),
        ] {
            let d = parse_duration(v)
                .map_err(|e| anyhow::anyhow!("{}: {name}: {e}", path.display()))?;
            if !zero_ok && d.is_zero() {
                anyhow::bail!("{}: {name}: must not be zero", path.display());
            }
        }
        Ok(cfg)
    }
    pub fn tick_duration(&self) -> Duration {
        duration_or_default(&self.tick, &PastorConfig::default().tick)
    }
    pub fn settle_duration(&self) -> Duration {
        duration_or_default(&self.settle, &PastorConfig::default().settle)
    }
    pub fn reconcile_duration(&self) -> Duration {
        duration_or_default(
            &self.reconcile_every,
            &PastorConfig::default().reconcile_every,
        )
    }
    pub fn timeout_duration(&self) -> Duration {
        duration_or_default(
            &self.defaults.timeout,
            &PastorConfig::default().defaults.timeout,
        )
    }
}

/// Parse `value`, falling back to `default` (assumed valid) if `value` is bad.
fn duration_or_default(value: &str, default: &str) -> Duration {
    parse_duration(value)
        .or_else(|_| parse_duration(default))
        .expect("default durations are valid")
}

/// "30s", "5m", "2h", "1d". No spaces, one unit.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let (num, unit) = s.split_at(
        s.find(|c: char| !c.is_ascii_digit())
            .ok_or_else(|| format!("{s:?}: missing unit"))?,
    );
    let n: u64 = num.parse().map_err(|_| format!("{s:?}: bad number"))?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => return Err(format!("{s:?}: unit must be s, m, h or d")),
    };
    let secs = n
        .checked_mul(mult)
        .ok_or_else(|| format!("{s:?}: duration too large"))?;
    Ok(Duration::from_secs(secs))
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
        assert_eq!(p.ssh_control_path("pi-3"), tmp.path().join("s/ssh/pi-3-%C"));
        assert_eq!(
            p.ssh_control_path("../../etc/x"),
            tmp.path().join("s/ssh/.._.._etc_x-%C"),
            "a machine name must not escape the ssh directory"
        );
    }

    #[test]
    fn control_path_escapes_percent_in_the_state_dir() {
        // ssh expands `%` in a ControlPath, so a state dir that contains one has
        // to arrive escaped or the socket lands somewhere unintended.
        let p = Paths::new("/tmp/c", "/tmp/100%/state");
        assert_eq!(
            p.ssh_control_path("pi-3"),
            PathBuf::from("/tmp/100%%/state/ssh/pi-3-%C")
        );
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("1d").unwrap(), Duration::from_secs(86400));
        assert!(parse_duration("5").is_err());
        assert!(parse_duration("5 m").is_err());
        assert!(parse_duration("x").is_err());
        assert!(parse_duration("300000000000000d").is_err());
    }

    #[test]
    fn accessors_fall_back_to_defaults() {
        let cfg = PastorConfig {
            tick: "bogus".into(),
            ..Default::default()
        };
        assert_eq!(cfg.tick_duration(), PastorConfig::default().tick_duration());
    }

    #[test]
    fn config_defaults_and_partial_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.toml");
        assert_eq!(PastorConfig::load(&path).unwrap(), PastorConfig::default());
        std::fs::write(&path, "tick = \"3s\"\n[defaults]\nagent = \"codex\"\n").unwrap();
        let cfg = PastorConfig::load(&path).unwrap();
        assert_eq!(cfg.tick_duration(), Duration::from_secs(3));
        assert_eq!(cfg.defaults.agent, "codex");
        assert_eq!(cfg.defaults.max_tasks_per_run, 5);
        std::fs::write(&path, "settle = \"soon\"\n").unwrap();
        assert!(
            PastorConfig::load(&path)
                .unwrap_err()
                .to_string()
                .contains("settle")
        );
    }

    #[test]
    fn zero_intervals_are_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.toml");

        std::fs::write(&path, "tick = \"0s\"\n").unwrap();
        let err = PastorConfig::load(&path).unwrap_err().to_string();
        assert!(err.contains("tick"), "{err}");
        assert!(err.contains("must not be zero"), "{err}");

        std::fs::write(&path, "reconcile_every = \"0s\"\n").unwrap();
        let err = PastorConfig::load(&path).unwrap_err().to_string();
        assert!(err.contains("reconcile_every"), "{err}");
        assert!(err.contains("must not be zero"), "{err}");

        std::fs::write(&path, "settle = \"0s\"\n").unwrap();
        let err = PastorConfig::load(&path).unwrap_err().to_string();
        assert!(err.contains("settle"), "{err}");
        assert!(err.contains("must not be zero"), "{err}");
    }
}
