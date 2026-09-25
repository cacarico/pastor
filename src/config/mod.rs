pub mod flock;
pub mod job;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// Where pastor reads config and keeps state. Overridable by env for tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    /// Managed plugin checkouts live here (`~/.local/share/pastor`).
    pub data_dir: PathBuf,
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
        let data_dir = match std::env::var_os("PASTOR_DATA_DIR") {
            Some(v) => PathBuf::from(v),
            None => dirs::data_dir()
                .or_else(|| dirs::home_dir().map(|h| h.join(".local/share")))
                .context("no data dir")?
                .join("pastor"),
        };
        Ok(Paths {
            config_dir,
            state_dir,
            data_dir,
        })
    }

    /// The data dir defaults to `<state>/data`, so the many tests that build
    /// `Paths` from two temp dirs never reach into the real data dir.
    pub fn new(config_dir: impl Into<PathBuf>, state_dir: impl Into<PathBuf>) -> Paths {
        let state_dir = state_dir.into();
        Paths {
            config_dir: config_dir.into(),
            data_dir: state_dir.join("data"),
            state_dir,
        }
    }

    pub fn with_data_dir(mut self, data_dir: impl Into<PathBuf>) -> Paths {
        self.data_dir = data_dir.into();
        self
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

    /// The events log, one JSON `EventRecord` per line. Rotated by size to
    /// `events.jsonl.1`; read by `pastor events` straight from disk.
    pub fn events_file(&self) -> PathBuf {
        self.state_dir.join("events.jsonl")
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("pastor.toml")
    }

    /// One TOML file per job. Created by the user; pastor only reads and, for
    /// `job enable|disable`, rewrites one line of it.
    pub fn jobs_dir(&self) -> PathBuf {
        self.config_dir.join("jobs")
    }

    /// Directory for the ssh `ControlMaster` sockets, one per machine. Created
    /// with mode 0700 by the transport before any ssh that carries a ControlPath.
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

    pub fn data_dir(&self) -> PathBuf {
        self.data_dir.clone()
    }

    /// One directory per plugin: a managed checkout, or a symlink made by
    /// `plugin link`.
    pub fn plugins_dir(&self) -> PathBuf {
        self.data_dir.join("plugins")
    }

    /// Secrets and settings for one plugin, written by the user.
    pub fn plugin_env_file(&self, id: &str) -> PathBuf {
        self.config_dir.join("plugins").join(id).join(".env")
    }

    /// Per-job scratch a connector may use; pastor owns the directory, the
    /// plugin owns what is in it.
    pub fn plugin_state_dir(&self, job: &str) -> PathBuf {
        self.state_dir.join("plugins").join(job)
    }

    /// Captured connector and hook output for one job, `<ts>.log` per run.
    pub fn runs_dir(&self, job: &str) -> PathBuf {
        self.state_dir.join("runs").join(job)
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
    /// Extra argv for the agent (`["--model", "claude-opus-5-5"]`), for tasks
    /// whose run flags or job file give none. See `agent_args_or`.
    pub agent_args: Vec<String>,
    pub max_tasks_per_run: u32,
    pub timeout: String,
}

impl Defaults {
    /// The agent args a task gets. `given` is what `pastor run --agent-arg` or
    /// a job file's `agent_args` said, `None` when they said nothing; only
    /// then do `[defaults] agent_args` apply. The one place that rule lives,
    /// so run and jobs cannot drift apart.
    pub fn agent_args_or(&self, given: Option<Vec<String>>) -> Vec<String> {
        given.unwrap_or_else(|| self.agent_args.clone())
    }
}

impl Default for Defaults {
    fn default() -> Self {
        Defaults {
            agent: "claude".into(),
            agent_args: vec![],
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
    /// Bound on one herdr request, connect included. A machine that does not
    /// answer within it is treated as lost.
    pub request_timeout: String,
    /// How long dispatch waits between `agent.start` and a prompt herdr
    /// accepts. Runs inside `request_timeout`, so it must be shorter.
    pub agent_ready_timeout: String,
    pub defaults: Defaults,
}

impl Default for PastorConfig {
    fn default() -> Self {
        PastorConfig {
            tick: "10s".into(),
            settle: "10s".into(),
            reconcile_every: "60s".into(),
            request_timeout: "60s".into(),
            agent_ready_timeout: "30s".into(),
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
        Self::parse(path, &text)
    }

    /// Like `load`, but a missing file is an error rather than the defaults:
    /// for a reload, where the `exists()` check and the read used to race a
    /// concurrent delete-and-rewrite. One read; the error keeps
    /// `std::io::ErrorKind::NotFound` at the top so callers can tell
    /// "missing" from "does not parse".
    pub fn load_existing(path: &Path) -> anyhow::Result<PastorConfig> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow::Error::new(e)
            } else {
                anyhow::Error::new(e).context(format!("read {}", path.display()))
            }
        })?;
        Self::parse(path, &text)
    }

    fn parse(path: &Path, text: &str) -> anyhow::Result<PastorConfig> {
        let cfg: PastorConfig =
            toml::from_str(text).with_context(|| format!("parse {}", path.display()))?;
        // tick, settle and reconcile_every all drive `tokio::time::interval`,
        // which panics on a zero period. Reject zero here so a bad config
        // fails to load instead of crashing the daemon at startup.
        for (name, v, zero_ok) in [
            ("tick", &cfg.tick, false),
            ("settle", &cfg.settle, false),
            ("reconcile_every", &cfg.reconcile_every, false),
            ("request_timeout", &cfg.request_timeout, false),
            ("agent_ready_timeout", &cfg.agent_ready_timeout, false),
            ("defaults.timeout", &cfg.defaults.timeout, true),
        ] {
            let d = parse_duration(v)
                .map_err(|e| anyhow::anyhow!("{}: {name}: {e}", path.display()))?;
            if !zero_ok && d.is_zero() {
                anyhow::bail!("{}: {name}: must not be zero", path.display());
            }
        }
        if cfg.agent_ready_timeout_duration() >= cfg.request_timeout_duration() {
            anyhow::bail!(
                "{}: agent_ready_timeout must be shorter than request_timeout",
                path.display()
            );
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
    pub fn request_timeout_duration(&self) -> Duration {
        duration_or_default(
            &self.request_timeout,
            &PastorConfig::default().request_timeout,
        )
    }
    pub fn agent_ready_timeout_duration(&self) -> Duration {
        duration_or_default(
            &self.agent_ready_timeout,
            &PastorConfig::default().agent_ready_timeout,
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
            std::env::set_var("PASTOR_DATA_DIR", tmp.path().join("d"));
        }
        let p = Paths::from_env().unwrap();
        assert_eq!(p.config_dir, tmp.path().join("c"));
        assert_eq!(p.state_dir, tmp.path().join("s"));
        assert_eq!(p.data_dir(), tmp.path().join("d"));
        unsafe {
            std::env::remove_var("PASTOR_CONFIG_DIR");
            std::env::remove_var("PASTOR_STATE_DIR");
            std::env::remove_var("PASTOR_DATA_DIR");
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

    /// `load_existing` is for a reload, where a missing file must not be
    /// mistaken for an intentionally emptied config (Copilot 4103271200).
    #[test]
    fn load_existing_errors_on_a_missing_path_and_reads_an_empty_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.toml");
        let err = PastorConfig::load_existing(&path).unwrap_err();
        assert_eq!(
            err.downcast_ref::<std::io::Error>().map(|e| e.kind()),
            Some(std::io::ErrorKind::NotFound)
        );

        std::fs::write(&path, "").unwrap();
        assert_eq!(
            PastorConfig::load_existing(&path).unwrap(),
            PastorConfig::default()
        );
    }

    #[test]
    fn defaults_agent_args_parse_and_apply_only_when_none_are_given() {
        assert!(Defaults::default().agent_args.is_empty());
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.toml");
        std::fs::write(
            &path,
            "[defaults]\nagent_args = [\"--model\", \"claude-opus-5-5\"]\n",
        )
        .unwrap();
        let d = PastorConfig::load(&path).unwrap().defaults;
        assert_eq!(d.agent_args, vec!["--model", "claude-opus-5-5"]);
        assert_eq!(d.agent_args_or(None), vec!["--model", "claude-opus-5-5"]);
        assert_eq!(d.agent_args_or(Some(vec!["-v".into()])), vec!["-v"]);
        assert!(d.agent_args_or(Some(vec![])).is_empty());
    }

    #[test]
    fn timeout_keys_default_and_parse() {
        let cfg = PastorConfig::default();
        assert_eq!(cfg.request_timeout_duration(), Duration::from_secs(60));
        assert_eq!(cfg.agent_ready_timeout_duration(), Duration::from_secs(30));
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.toml");
        std::fs::write(
            &path,
            "request_timeout = \"90s\"\nagent_ready_timeout = \"45s\"\n",
        )
        .unwrap();
        let cfg = PastorConfig::load(&path).unwrap();
        assert_eq!(cfg.request_timeout_duration(), Duration::from_secs(90));
        assert_eq!(cfg.agent_ready_timeout_duration(), Duration::from_secs(45));
    }

    /// The readiness wait runs inside the request timeout that bounds a whole
    /// dispatch; a config that inverts them would report every slow agent as
    /// a wedged machine (see `MachineSettings::agent_ready_timeout`).
    #[test]
    fn ready_timeout_must_be_shorter_than_request_timeout_and_nonzero() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.toml");
        std::fs::write(
            &path,
            "request_timeout = \"30s\"\nagent_ready_timeout = \"30s\"\n",
        )
        .unwrap();
        let err = PastorConfig::load(&path).unwrap_err().to_string();
        assert!(err.contains("agent_ready_timeout"), "{err}");
        assert!(err.contains("shorter than request_timeout"), "{err}");
        std::fs::write(&path, "agent_ready_timeout = \"0s\"\n").unwrap();
        let err = PastorConfig::load(&path).unwrap_err().to_string();
        assert!(err.contains("must not be zero"), "{err}");
        std::fs::write(&path, "request_timeout = \"0s\"\n").unwrap();
        let err = PastorConfig::load(&path).unwrap_err().to_string();
        assert!(err.contains("request_timeout"), "{err}");
    }

    #[test]
    fn plugin_paths() {
        let p = Paths::new("/tmp/c", "/tmp/s");
        assert_eq!(p.data_dir(), PathBuf::from("/tmp/s/data"));
        assert_eq!(p.plugins_dir(), PathBuf::from("/tmp/s/data/plugins"));
        let p = p.with_data_dir("/tmp/d");
        assert_eq!(p.plugins_dir(), PathBuf::from("/tmp/d/plugins"));
        assert_eq!(
            p.plugin_env_file("slack"),
            PathBuf::from("/tmp/c/plugins/slack/.env")
        );
        assert_eq!(
            p.plugin_state_dir("support"),
            PathBuf::from("/tmp/s/plugins/support")
        );
        assert_eq!(p.runs_dir("support"), PathBuf::from("/tmp/s/runs/support"));
    }

    #[test]
    fn jobs_dir_lives_under_config() {
        let p = Paths::new("/tmp/c", "/tmp/s");
        assert_eq!(p.jobs_dir(), PathBuf::from("/tmp/c/jobs"));
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
