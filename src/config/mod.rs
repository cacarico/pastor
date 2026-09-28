pub mod flock;
pub mod job;
pub mod opencode;
pub mod profile;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// Where pastor reads config and keeps state. Overridable by env for tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    /// Managed connector checkouts live here (`~/.local/share/pastor`).
    pub data_dir: PathBuf,
}

impl Paths {
    pub fn from_env() -> anyhow::Result<Paths> {
        let config_dir = match std::env::var_os("PASTOR_CONFIG_DIR") {
            Some(v) => PathBuf::from(v),
            None => config_home().context("no config dir")?.join("pastor"),
        };
        let state_dir = match std::env::var_os("PASTOR_STATE_DIR") {
            Some(v) => PathBuf::from(v),
            None => xdg_home("XDG_STATE_HOME", ".local/state")
                .context("no state dir")?
                .join("pastor"),
        };
        let data_dir = match std::env::var_os("PASTOR_DATA_DIR") {
            Some(v) => PathBuf::from(v),
            None => xdg_home("XDG_DATA_HOME", ".local/share")
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
    /// A headless serve's own database: its jobs' state and seen keys, and
    /// how far it has read the head's events. Never `db_file`, which is a
    /// head's task store.
    pub fn shepherd_db_file(&self) -> PathBuf {
        self.state_dir.join("shepherd.db")
    }
    pub fn socket_file(&self) -> PathBuf {
        self.state_dir.join("pastor.sock")
    }

    /// A background `pastor serve`'s log (`serve_cli`), rotated to
    /// `serve.log.1` .. `serve.log.3`. A head in the foreground or under a
    /// service logs to stderr instead.
    pub fn serve_log_file(&self) -> PathBuf {
        self.state_dir.join("serve.log")
    }

    /// What the running `pastor serve` wrote about itself at start: its pid,
    /// the service manager that runs it, its log (`serve_cli::Record`).
    pub fn serve_record_file(&self) -> PathBuf {
        self.state_dir.join("serve.json")
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

    /// One directory per connector: a managed checkout, or a symlink made by
    /// `connector link`.
    pub fn connectors_dir(&self) -> PathBuf {
        self.data_dir.join("connectors")
    }

    /// Secrets and settings for one connector, written by the user.
    pub fn connector_env_file(&self, id: &str) -> PathBuf {
        self.config_dir.join("connectors").join(id).join(".env")
    }

    /// One TOML file per orchestrator, next to `jobs/`. Read by the head on
    /// each tick; `orchestrator enable|disable` rewrites one line of one.
    pub fn orchestrators_dir(&self) -> PathBuf {
        self.config_dir.join("orchestrators")
    }

    /// What the head keeps for one orchestrator: `state.json`, its handover
    /// `note`, the scripts' `scratch/` dir and their run logs in `runs/`.
    pub fn orchestrator_state_dir(&self, name: &str) -> PathBuf {
        self.state_dir.join("orchestrators").join(name)
    }

    /// Per-job scratch a connector may use; pastor owns the directory, the
    /// connector owns what is in it.
    pub fn connector_state_dir(&self, job: &str) -> PathBuf {
        self.state_dir.join("connectors").join(job)
    }

    /// Captured connector and hook output for one job, `<ts>.log` per run.
    pub fn runs_dir(&self, job: &str) -> PathBuf {
        self.state_dir.join("runs").join(job)
    }
}

/// `~/.config`, or `$XDG_CONFIG_HOME`: where pastor's config, herdr's socket
/// and the systemd user units live.
pub fn config_home() -> Option<PathBuf> {
    xdg_home("XDG_CONFIG_HOME", ".config")
}

/// The XDG base directory `var` names, on every platform. On macOS `dirs`
/// answers `~/Library/Application Support` instead, but herdr keeps its socket
/// under `~/.config` there too, and one layout on every machine is simpler to
/// document and to reach over ssh.
fn xdg_home(var: &str, fallback: &str) -> Option<PathBuf> {
    xdg_dir(std::env::var_os(var), dirs::home_dir(), fallback)
}

/// The XDG rule: the variable when it holds an absolute path, else
/// `<home>/<fallback>`. A relative or empty value is ignored, as the spec says.
fn xdg_dir(
    value: Option<std::ffi::OsString>,
    home: Option<PathBuf>,
    fallback: &str,
) -> Option<PathBuf> {
    value
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.map(|h| h.join(fallback)))
}

/// Where pastor kept its config and connector checkouts on macOS before it
/// followed the XDG layout there: `~/Library/Application Support/pastor`.
/// `None` off macOS, and when `PASTOR_CONFIG_DIR` or `XDG_CONFIG_HOME` moves
/// the config dir: then the config is read from where the user said, not from
/// the default the old one would move to. A data or state override alone
/// leaves the config at its default, so the move still happens.
pub fn legacy_macos_dir() -> Option<PathBuf> {
    legacy_dir(
        cfg!(target_os = "macos"),
        std::env::var_os("PASTOR_CONFIG_DIR"),
        std::env::var_os("XDG_CONFIG_HOME"),
        dirs::home_dir(),
    )
}

fn legacy_dir(
    macos: bool,
    config_override: Option<std::ffi::OsString>,
    xdg_config: Option<std::ffi::OsString>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    // The same rule `xdg_dir` applies: only an absolute value moves it.
    let xdg_moves_config = xdg_config.is_some_and(|v| Path::new(&v).is_absolute());
    if !macos || config_override.is_some() || xdg_moves_config {
        return None;
    }
    home.map(|h| h.join("Library/Application Support/pastor"))
}

/// Marks a legacy move that has not finished: written into the legacy dir
/// before it becomes the config dir, removed after the last step. It holds
/// `connectors` when the old `plugins/` checkouts are to move to the data
/// dir.
const MIGRATING: &str = ".pastor-migrating";

/// Move a config left at `legacy` into `paths`, once. The old macOS layout had
/// config and data in one directory, and still used the old name for
/// connectors, so `plugins/` held both the checkouts and each connector's
/// `.env`: the checkouts go to `connectors/` in the data dir and the `.env`
/// files to `<config>/connectors/<id>/`. Nothing happens when there is no
/// legacy directory or the new config dir already exists (never merge two
/// configs), unless that config dir is a move a failed run left unfinished:
/// then this run finishes it. Returns the note to show the user when
/// something moved.
pub fn migrate_legacy_dir(legacy: &Path, paths: &Paths) -> anyhow::Result<Option<String>> {
    let is_real_dir = |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir());
    let marker = paths.config_dir.join(MIGRATING);
    let move_connectors = if std::fs::symlink_metadata(&paths.config_dir).is_ok() {
        match std::fs::read_to_string(&marker) {
            Ok(text) => text.trim() == "connectors",
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(err).with_context(|| format!("read {}", marker.display()));
            }
        }
    } else {
        if !is_real_dir(legacy) {
            return Ok(None);
        }
        // Decided once, before anything moves: a data dir that already has
        // connectors holds someone else's checkouts, never merged with these.
        let move_connectors = is_real_dir(&legacy.join("plugins"))
            && std::fs::symlink_metadata(paths.connectors_dir()).is_err();
        let legacy_marker = legacy.join(MIGRATING);
        std::fs::write(
            &legacy_marker,
            if move_connectors {
                "connectors\n"
            } else {
                "\n"
            },
        )
        .with_context(|| format!("write {}", legacy_marker.display()))?;
        if let Some(parent) = paths.config_dir.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        std::fs::rename(legacy, &paths.config_dir).with_context(|| {
            format!(
                "move {} to {}",
                legacy.display(),
                paths.config_dir.display()
            )
        })?;
        move_connectors
    };
    let mut note = format!(
        "note: moved the pastor config from {} to {}",
        legacy.display(),
        paths.config_dir.display()
    );
    // Every step from here is safe to repeat, so a run that fails part way
    // leaves the marker behind and the next run picks up where it stopped.
    let old_plugins = paths.config_dir.join("plugins");
    let connectors = paths.connectors_dir();
    let connectors_moved = std::fs::symlink_metadata(&connectors).is_ok();
    if move_connectors && (connectors_moved || is_real_dir(&old_plugins)) {
        if !connectors_moved {
            create_private_dir(&paths.data_dir)?;
            std::fs::rename(&old_plugins, &connectors).with_context(|| {
                format!("move {} to {}", old_plugins.display(), connectors.display())
            })?;
        }
        let mut ids: Vec<_> = std::fs::read_dir(&connectors)
            .with_context(|| format!("read {}", connectors.display()))?
            .filter_map(Result::ok)
            // Follows a `connector link` symlink, so a linked connector is
            // seen as the directory it points at.
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name())
            .collect();
        ids.sort();
        let mut linked_envs = Vec::new();
        for id in ids {
            let dir = connectors.join(&id);
            let from = dir.join(".env");
            let to = paths.connector_env_file(&id.to_string_lossy());
            if std::fs::symlink_metadata(&dir).is_ok_and(|m| m.is_symlink()) {
                // The legacy `plugins/<id>/.env` resolved through the link, so
                // a linked connector's `.env` sits in the user's own checkout.
                // That is their file: leave it, and say where it now goes.
                if from.is_file() {
                    let real = std::fs::canonicalize(&from).unwrap_or(from);
                    linked_envs.push(format!("{} to {}", real.display(), to.display()));
                }
                continue;
            }
            if !std::fs::symlink_metadata(&from).is_ok_and(|m| m.is_file()) {
                continue;
            }
            if let Some(dir) = to.parent() {
                create_private_dir(dir)?;
            }
            std::fs::rename(&from, &to)
                .with_context(|| format!("move {} to {}", from.display(), to.display()))?;
        }
        note.push_str(&format!(", and its connectors to {}", connectors.display()));
        if !linked_envs.is_empty() {
            note.push_str(&format!(
                "; linked connectors keep their .env in their own checkout, copy {} if they still need it",
                linked_envs.join(", ")
            ));
        }
    }
    std::fs::remove_file(&marker).with_context(|| format!("remove {}", marker.display()))?;
    Ok(Some(note))
}

/// Create `dir` (and its parents) with mode 0700, or bring an existing one to
/// 0700. The dir must be the user's own: one another uid owns, or reached
/// through a symlink another uid owns, is refused rather than used, since a
/// state dir under a shared path such as /tmp could otherwise be planted by
/// someone else and receive the ssh and IPC sockets. A symlink the user owns
/// (a config dir kept in dotfiles) is followed. A missing dir is made only
/// under ancestors nobody else can swap out (`safe_ancestors`).
pub fn create_private_dir(dir: &Path) -> anyhow::Result<()> {
    create_private_dir_as(dir, unsafe { libc::geteuid() })
}

fn create_private_dir_as(dir: &Path, euid: u32) -> anyhow::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let owned = |md: &std::fs::Metadata, what: &str| {
        if md.uid() == euid {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "{}: {what} is owned by uid {}, not {euid}; refusing to use it",
                dir.display(),
                md.uid()
            ))
        }
    };
    let link = match std::fs::symlink_metadata(dir) {
        Ok(md) => md,
        // Missing, or a parent is in the way: creating says which.
        Err(_) => {
            safe_ancestors(dir, euid)?;
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
            std::fs::symlink_metadata(dir).with_context(|| format!("stat {}", dir.display()))?
        }
    };
    let md = if link.file_type().is_symlink() {
        owned(&link, "the symlink")?;
        std::fs::metadata(dir).with_context(|| format!("follow {}", dir.display()))?
    } else {
        link
    };
    if !md.is_dir() {
        anyhow::bail!("{}: not a directory", dir.display());
    }
    owned(&md, "the directory")?;
    // Called on every ssh connect, so the common case — it is already there and
    // already private — costs the stats above instead of a chmod too.
    if md.permissions().mode() & 0o777 != 0o700 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod {}", dir.display()))?;
    }
    Ok(())
}

/// Check the ancestors of `dir` that exist before `create_dir_all` makes the
/// rest through them. One that someone else could swap for a path of their
/// choosing is refused: a symlink owned by neither the user nor root, or a dir
/// anyone can write to, without the sticky bit, owned by neither. A sticky
/// shared dir such as /tmp, root's dirs and the user's own pass.
fn safe_ancestors(dir: &Path, euid: u32) -> anyhow::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let trusted = |uid: u32| uid == euid || uid == 0;
    for anc in dir.ancestors().skip(1) {
        if anc.as_os_str().is_empty() {
            continue;
        }
        let Ok(link) = std::fs::symlink_metadata(anc) else {
            continue;
        };
        if link.file_type().is_symlink() && !trusted(link.uid()) {
            anyhow::bail!(
                "{}: its parent {} is a symlink owned by uid {}; refusing to create it there",
                dir.display(),
                anc.display(),
                link.uid()
            );
        }
        let Ok(md) = std::fs::metadata(anc) else {
            continue;
        };
        let mode = md.permissions().mode();
        if md.is_dir() && mode & 0o002 != 0 && mode & 0o1000 == 0 && !trusted(md.uid()) {
            anyhow::bail!(
                "{}: its parent {} is writable by anyone and owned by uid {}; refusing to create it there",
                dir.display(),
                anc.display(),
                md.uid()
            );
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Defaults {
    pub agent: String,
    /// Extra argv for the agent (`["--model", "claude-opus-5-5"]`), for tasks
    /// whose run flags, job file and flock give none. See `resolve_agent`.
    pub agent_args: Vec<String>,
    /// Tool patterns every task's agent may use without asking
    /// (`"Bash(git:*)"`); a flock and a job add to them. See `resolve_agent`.
    pub allow: Vec<String>,
    /// Tool patterns every task's agent must never use; wins over `allow`.
    pub deny: Vec<String>,
    /// The `[models]` entry tasks run when their run flags, job, flock and
    /// machine name none. See `resolve_agent`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The level of tasks whose run flags, job, flock and pinned machine
    /// name none; unset, `normal`. See `resolve_priority`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<crate::task::Priority>,
    /// The agent that runs a model of another kind than `agent`'s, by kind
    /// (`{ opencode = "opencode" }`). See `resolve_agent_for`.
    #[serde(skip_serializing_if = "KindAgents::is_empty")]
    pub agents: KindAgents,
    /// The permission profile tasks run under when their run flags, job,
    /// flock and machine name none. See `resolve_agent`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    pub max_tasks_per_run: u32,
    pub timeout: String,
    /// Whether tasks whose run flags, job and flock say nothing are asked
    /// for a summary, or need one (`SummaryMode`); unset, `ask`. See
    /// `resolve_summary`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<crate::task::SummaryMode>,
    /// Where a task's pane goes when its run flags and job say nothing
    /// (`task::Place`).
    #[serde(skip_serializing_if = "crate::task::Place::is_repo")]
    pub place: crate::task::Place,
    /// The label template of the workspace a task makes when its run
    /// flags, job and flock set none; unset, `task::DEFAULT_LABEL`. See
    /// `resolve_label`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// What `pastor task run` flags or a job file's `[dispatch]` say about the
/// agent, before the flock and `[defaults]` fill in the rest. `None` means it
/// said nothing; `Some(vec![])` for `agent_args` means "no args".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentChoice {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_args: Option<Vec<String>>,
    /// Tool patterns added to the flock's and `[defaults]` allow lists.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    /// Tool patterns added to the flock's and `[defaults]` deny lists.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
    /// A `[models]` name, before the flock's, the machine's and `[defaults]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// A permission profile, before the flock's, the machine's and
    /// `[defaults]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// `--timeout` or a job's `[dispatch] timeout`, in seconds, before the
    /// flock's and `[defaults]` (`Defaults::resolve_timeout`). Unset from a
    /// client that predates flock timeouts: the spec's own then stands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// `--place` or a job's `[dispatch] place`, before the flock's and
    /// `[defaults]` (`Defaults::resolve_place`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place: Option<crate::task::Place>,
}

/// Where a task's agent, or its args, came from (`Defaults::resolve_agent_on`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    /// The run flags or the job file.
    Ask,
    /// The `[[machine]]` entry of the machine the task runs on.
    Machine,
    /// The task's `[[flock]]` entry.
    Flock,
    /// `[defaults]`, whose `agent` is the built-in `claude` when unset.
    Defaults,
}

/// The agent a task runs, as `Defaults::resolve_agent` settled it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPick {
    pub agent: String,
    pub agent_args: Vec<String>,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub agent_from: Layer,
    /// `None` when no layer's args were written for the agent: it runs
    /// with none.
    pub args_from: Option<Layer>,
    /// The `[models]` name the task runs, and the layer that named it;
    /// `None` when no layer names one. Its args are not in `agent_args`:
    /// `Models::apply` puts them in front.
    pub model: Option<(String, Layer)>,
    /// The agent is `agent_from`'s `agents` entry for its kind, not its
    /// `agent` (`Defaults::resolve_agent_for`).
    pub by_kind: bool,
    /// The permission profile the task runs under, and the layer that named
    /// it; `None` when no layer names one. `Profiles::apply` adds its lists.
    pub profile: Option<(String, Layer)>,
    /// The kind of the agent the flock names, when the machine runs another
    /// kind and has no agent of this one (`Defaults::resolve_agent_for`):
    /// the task cannot run there.
    pub missing_kind: Option<String>,
}

/// `agents = { <kind> = "<agent>" }` on a machine, a flock or `[defaults]`:
/// the agent that layer runs a model of that kind on when its own `agent`
/// is of another kind.
pub type KindAgents = std::collections::BTreeMap<String, String>;

/// Refuse a layer's `agents` entry whose agent is not of the kind it is
/// filed under, or one for the kind of the layer's own agent (`own`),
/// which that agent already runs. `agents` is `[agents]` from pastor.toml.
pub fn check_kind_agents(
    own: Option<&str>,
    by_kind: &KindAgents,
    agents: &Agents,
) -> Result<(), String> {
    for (kind, agent) in by_kind {
        let is = agents.kind(agent);
        if is != kind {
            return Err(format!("agents.{kind}: agent {agent} is {is}, not {kind}"));
        }
        if let Some(own) = own
            && agents.kind(own) == kind
        {
            return Err(format!(
                "agents.{kind}: its own agent {own} is already {kind}"
            ));
        }
    }
    Ok(())
}

/// Refuse an `agents` key or agent that is empty: what `check_kind_agents`
/// can check without `[agents]`.
pub fn check_kind_agent_names(by_kind: &KindAgents) -> Result<(), String> {
    for (kind, agent) in by_kind {
        if kind.trim().is_empty() {
            return Err("agents: a kind must not be empty".into());
        }
        if agent.trim().is_empty() {
            return Err(format!("agents.{kind} must not be empty"));
        }
    }
    Ok(())
}

impl AgentPick {
    pub fn apply_to(&self, spec: &mut crate::task::DispatchSpec) {
        spec.agent = self.agent.clone();
        spec.agent_args = self.agent_args.clone();
        spec.allow = self.allow.clone();
        spec.deny = self.deny.clone();
    }
}

/// Refuse a tool pattern the agent's command line would misread: an empty
/// one, or one that starts with `-` and reads as a flag. `key` names where
/// it came from for the error.
pub fn check_tools(key: &str, patterns: &[String]) -> Result<(), String> {
    for p in patterns {
        if p.trim().is_empty() {
            return Err(format!("{key}: a tool pattern must not be empty"));
        }
        if p.starts_with('-') {
            return Err(format!(
                "{key}: tool pattern {p:?} starts with '-'; agent flags go in agent_args"
            ));
        }
    }
    Ok(())
}

impl Defaults {
    /// The agent a task gets: from the first of `ask` (run flags, a job
    /// file), `flock` (its `[[flock]]` entry) and these defaults that names
    /// one. Its args come from the first of the three that sets
    /// `agent_args` and was written for that agent: one that names no agent,
    /// or names the same one. So a flock's `--model` for codex never reaches
    /// a task that asked for claude.
    ///
    /// `allow` and `deny` add up instead, `[defaults]` then flock then ask,
    /// so a narrower layer can never lift a broader one's deny: a pattern
    /// in any `deny` is dropped from `allow`, and also passed as a deny.
    /// The one place these rules live, so run and jobs cannot drift apart.
    pub fn resolve_agent(&self, ask: &AgentChoice, flock: Option<&flock::FlockEntry>) -> AgentPick {
        self.resolve_agent_on(ask, None, flock)
    }

    /// `resolve_agent` for a task on `machine`: its `agent` and
    /// `agent_args` come between the ask and the flock's, so machines of
    /// one flock can run different agents, each the one installed and
    /// logged in there. The `model` and `profile` come from the flock
    /// before the machine: a project's flock sets them on a shared machine.
    /// Tool lists stay per flock.
    pub fn resolve_agent_on(
        &self,
        ask: &AgentChoice,
        machine: Option<&flock::MachineConfig>,
        flock: Option<&flock::FlockEntry>,
    ) -> AgentPick {
        let lists =
            |pick: fn(&flock::FlockEntry) -> &Vec<String>, own: &Vec<String>, ask: &Vec<String>| {
                let mut out: Vec<String> = Vec::new();
                for p in own
                    .iter()
                    .chain(flock.map(pick).into_iter().flatten())
                    .chain(ask)
                {
                    if !out.contains(p) {
                        out.push(p.clone());
                    }
                }
                out
            };
        let deny = lists(|f| &f.deny, &self.deny, &ask.deny);
        let mut allow = lists(|f| &f.allow, &self.allow, &ask.allow);
        allow.retain(|p| !deny.contains(p));
        let layers = [
            (Layer::Ask, ask.agent.as_deref(), ask.agent_args.as_ref()),
            (
                Layer::Machine,
                machine.and_then(|m| m.agent.as_deref()),
                machine.and_then(|m| m.agent_args.as_ref()),
            ),
            (
                Layer::Flock,
                flock.and_then(|f| f.agent.as_deref()),
                flock.and_then(|f| f.agent_args.as_ref()),
            ),
            (
                Layer::Defaults,
                Some(self.agent.as_str()),
                Some(&self.agent_args),
            ),
        ];
        let (agent_from, agent) = layers
            .iter()
            .find_map(|&(layer, agent, _)| Some((layer, agent?.to_string())))
            .expect("[defaults] always names an agent");
        let (args_from, agent_args) = layers
            .iter()
            .find_map(|&(layer, for_agent, args)| {
                args.filter(|_| for_agent.is_none_or(|a| a == agent))
                    .map(|a| (Some(layer), a.clone()))
            })
            .unwrap_or_default();
        let model = [
            (Layer::Ask, ask.model.as_ref()),
            (Layer::Flock, flock.and_then(|f| f.model.as_ref())),
            (Layer::Machine, machine.and_then(|m| m.model.as_ref())),
            (Layer::Defaults, self.model.as_ref()),
        ]
        .into_iter()
        .find_map(|(layer, name)| Some((name?.clone(), layer)));
        let profile = [
            (Layer::Ask, ask.profile.as_ref()),
            (Layer::Flock, flock.and_then(|f| f.profile.as_ref())),
            (Layer::Machine, machine.and_then(|m| m.profile.as_ref())),
            (Layer::Defaults, self.profile.as_ref()),
        ]
        .into_iter()
        .find_map(|(layer, name)| Some((name?.clone(), layer)));
        AgentPick {
            agent,
            agent_args,
            allow,
            deny,
            agent_from,
            args_from,
            model,
            by_kind: false,
            profile,
            missing_kind: None,
        }
    }

    /// `resolve_agent_on`, then, for a task with no model, the machine's
    /// agent of the kind its flock's agent is (`flock_kind_on`). When the
    /// task's model runs on another kind than that agent's, the model's
    /// kind decides instead, and the agent is the one for it: at the machine,
    /// the flock and these defaults in turn, the layer's `agent` if it is
    /// of that kind, else its `agents` entry for it. An agent the task or
    /// job named itself is kept, as is the first agent when no layer has
    /// one of the kind, for `Models::apply` to refuse. An agent found this
    /// way takes `agent_args` only from layers whose `agent` is that same
    /// one: a layer's args with no `agent` are for its default agent.
    pub fn resolve_agent_for(
        &self,
        ask: &AgentChoice,
        machine: Option<&flock::MachineConfig>,
        flock: Option<&flock::FlockEntry>,
        models: &Models,
        agents: &Agents,
    ) -> AgentPick {
        let pick = self.resolve_agent_on(ask, machine, flock);
        let Some(kind) = pick
            .model
            .as_ref()
            .and_then(|(name, _)| models.0.get(name))
            .map(|def| def.kind.as_str())
        else {
            return self.flock_kind_on(pick, ask, machine, flock, agents);
        };
        if pick.agent_from == Layer::Ask || agents.kind(&pick.agent) == kind {
            return pick;
        }
        let layers = [
            (
                Layer::Machine,
                machine.and_then(|m| m.agent.as_deref()),
                machine.map(|m| &m.agents),
                machine.and_then(|m| m.agent_args.as_ref()),
            ),
            (
                Layer::Flock,
                flock.and_then(|f| f.agent.as_deref()),
                flock.map(|f| &f.agents),
                flock.and_then(|f| f.agent_args.as_ref()),
            ),
            (
                Layer::Defaults,
                Some(self.agent.as_str()),
                Some(&self.agents),
                Some(&self.agent_args),
            ),
        ];
        let found = layers
            .iter()
            .find_map(|&(layer, own, by_kind, _)| match own {
                Some(own) if agents.kind(own) == kind => Some((layer, own.to_string(), false)),
                _ => Some((layer, by_kind?.get(kind)?.clone(), true)),
            });
        let Some((agent_from, agent, by_kind)) = found else {
            return pick;
        };
        let (args_from, agent_args) = layers
            .iter()
            .find_map(|&(layer, own, _, args)| {
                args.filter(|_| own == Some(agent.as_str()))
                    .map(|a| (Some(layer), a.clone()))
            })
            .unwrap_or_default();
        AgentPick {
            agent,
            agent_args,
            agent_from,
            args_from,
            by_kind,
            ..pick
        }
    }

    /// The agent the flock names, on `machine`: the machine keeps choosing
    /// it, but of the flock agent's kind. Its `agent` when that is of the
    /// kind, else its `agents` entry for the kind; a machine that names no
    /// agent runs the flock's. A machine whose `agent` is of another kind
    /// and has no entry for this one cannot run the task (`missing_kind`).
    /// An agent the task or job named is kept.
    fn flock_kind_on(
        &self,
        pick: AgentPick,
        ask: &AgentChoice,
        machine: Option<&flock::MachineConfig>,
        flock: Option<&flock::FlockEntry>,
        agents: &Agents,
    ) -> AgentPick {
        let (Some(m), Some(wanted)) = (machine, flock.and_then(|f| f.agent.as_deref())) else {
            return pick;
        };
        if ask.agent.is_some() {
            return pick;
        }
        let kind = agents.kind(wanted);
        if agents.kind(&pick.agent) == kind {
            return pick;
        }
        let Some(agent) = m.agents.get(kind) else {
            if m.agent.is_some() {
                return AgentPick {
                    missing_kind: Some(kind.to_string()),
                    ..pick
                };
            }
            return pick;
        };
        // Args only from a layer written for this very agent, as for a
        // model's kind: the machine's and the flock's are for their own.
        let (args_from, agent_args) = [
            (Layer::Ask, None, ask.agent_args.as_ref()),
            (Layer::Machine, m.agent.as_deref(), m.agent_args.as_ref()),
            (
                Layer::Flock,
                Some(wanted),
                flock.and_then(|f| f.agent_args.as_ref()),
            ),
            (
                Layer::Defaults,
                Some(self.agent.as_str()),
                Some(&self.agent_args),
            ),
        ]
        .into_iter()
        .find_map(|(layer, own, args)| {
            args.filter(|_| own.is_none_or(|a| a == agent))
                .map(|a| (Some(layer), a.clone()))
        })
        .unwrap_or_default();
        AgentPick {
            agent: agent.clone(),
            agent_args,
            agent_from: Layer::Machine,
            args_from,
            by_kind: true,
            ..pick
        }
    }
}

impl Defaults {
    /// A task's level: from the first of `ask` (`--priority`, a job's
    /// `priority`), its flock, the machine it is pinned to and these
    /// defaults that sets one, and the layer that did; `normal` from none.
    /// Only a pinned task has a machine here: an unpinned one is queued
    /// before any machine is picked, and its level is settled then.
    pub fn resolve_priority(
        &self,
        ask: Option<crate::task::Priority>,
        pinned: Option<&flock::MachineConfig>,
        flock: Option<&flock::FlockEntry>,
    ) -> (crate::task::Priority, Option<Layer>) {
        [
            (Layer::Ask, ask),
            (Layer::Flock, flock.and_then(|f| f.priority)),
            (Layer::Machine, pinned.and_then(|m| m.priority)),
            (Layer::Defaults, self.priority),
        ]
        .into_iter()
        .find_map(|(layer, p)| Some((p?, Some(layer))))
        .unwrap_or_default()
    }

    /// A task's timeout in seconds: from the first of `ask` (`--timeout`, a
    /// job's `[dispatch] timeout`), its flock and these defaults, and the
    /// layer that set it. No machine layer: a machine has no `timeout`.
    /// Both files are checked on load; one that no longer parses is passed
    /// over.
    pub fn resolve_timeout(
        &self,
        ask: Option<u64>,
        flock: Option<&flock::FlockEntry>,
    ) -> (u64, Layer) {
        let secs = |t: &str| parse_duration(t).ok().map(|d| d.as_secs());
        [
            (Layer::Ask, ask),
            (
                Layer::Flock,
                flock.and_then(|f| f.timeout.as_deref()).and_then(secs),
            ),
        ]
        .into_iter()
        .find_map(|(layer, t)| Some((t?, layer)))
        .unwrap_or_else(|| {
            let t = duration_or_default(&self.timeout, &Defaults::default().timeout);
            (t.as_secs(), Layer::Defaults)
        })
    }

    /// Where a task's pane goes: from the first of `ask` (`--place`, a
    /// job's `[dispatch] place`), its flock and these defaults, and the
    /// layer that set it. No machine layer, as for `resolve_timeout`.
    pub fn resolve_place(
        &self,
        ask: Option<&crate::task::Place>,
        flock: Option<&flock::FlockEntry>,
    ) -> (crate::task::Place, Layer) {
        match (ask, flock.and_then(|f| f.place.as_ref())) {
            (Some(p), _) => (p.clone(), Layer::Ask),
            (None, Some(p)) => (p.clone(), Layer::Flock),
            (None, None) => (self.place.clone(), Layer::Defaults),
        }
    }

    /// The machine's own profile, which decides whether a task may ask for
    /// `unrestricted` on it (`Profiles::apply`): its `profile`, else its
    /// flock's, else these defaults'. The machine comes first here, unlike
    /// the profile a task runs under, so a flock's never lifts it.
    pub fn own_profile(
        &self,
        machine: Option<&flock::MachineConfig>,
        flock: Option<&flock::FlockEntry>,
    ) -> Option<String> {
        machine
            .and_then(|m| m.profile.clone())
            .or_else(|| flock.and_then(|f| f.profile.clone()))
            .or_else(|| self.profile.clone())
    }

    /// A task's `summary` setting: from the first of `ask` (`task run
    /// --summary`, a job's `[dispatch] summary`), its flock and these
    /// defaults that sets one; `ask` from none.
    pub fn resolve_summary(
        &self,
        ask: Option<crate::task::SummaryMode>,
        flock: Option<&flock::FlockEntry>,
    ) -> crate::task::SummaryMode {
        ask.or_else(|| flock.and_then(|f| f.summary))
            .or(self.summary)
            .unwrap_or_default()
    }
}

impl Defaults {
    /// A task's workspace label template: from the first of `ask`
    /// (`--label`, a job's `label`), `flock` and these defaults that sets
    /// one, and the layer that did; `None` leaves `task::DEFAULT_LABEL`.
    /// No machine layer: the label is settled when the task is queued,
    /// before a machine is picked, and `{{ machine }}` covers that need.
    pub fn resolve_label(
        &self,
        ask: Option<&str>,
        flock: Option<&flock::FlockEntry>,
    ) -> Option<(String, Layer)> {
        [
            (Layer::Ask, ask),
            (Layer::Flock, flock.and_then(|f| f.label.as_deref())),
            (Layer::Defaults, self.label.as_deref()),
        ]
        .into_iter()
        .find_map(|(layer, label)| Some((label?.to_string(), layer)))
    }
}

impl Default for Defaults {
    fn default() -> Self {
        Defaults {
            agent: "claude".into(),
            agent_args: vec![],
            allow: vec![],
            deny: vec![],
            model: None,
            priority: None,
            agents: KindAgents::new(),
            profile: None,
            max_tasks_per_run: 5,
            timeout: "2h".into(),
            summary: None,
            place: crate::task::Place::Repo,
            label: None,
        }
    }
}

/// One agent's definition under `[agents.<name>]` in `pastor.toml`. The
/// name is what tasks, jobs and flocks call it; `kind` is what herdr starts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentDef {
    /// The herdr agent kind to start (`claude`, `codex`); unset, the name
    /// itself. Built-in trust keys and tool flags follow it, so
    /// `[agents.claude-personal] kind = "claude"` gets Claude's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Environment for the task's pane, set when pastor creates it. A value
    /// that starts with `~/` (or is `~`) is expanded against the home of the
    /// machine the task runs on.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
    /// The keys that accept the agent's folder-trust prompt, for `pastor
    /// task send --trust` and saved trust. Unset keeps the built-in keys;
    /// an empty list means the agent has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trust_keys: Option<Vec<String>>,
    /// Text the agent's folder-trust prompt shows, and no other dialog does.
    /// Saved trust presses the trust keys only while the pane shows it.
    /// Unset keeps the built-in (Claude's); empty means none, and then saved
    /// trust presses the keys without reading the pane.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trust_marker: Option<String>,
    /// The flag that hands the agent one pattern of an `allow` list, put
    /// before each pattern. Unset keeps the built-in (`--allowedTools` for
    /// claude); an agent with none refuses tasks that carry an allow list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_flag: Option<String>,
    /// Like `allow_flag`, for `deny` (`--disallowedTools` for claude).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deny_flag: Option<String>,
}

/// `[agents.<name>]`, by agent name (`claude`, `codex`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Agents(pub std::collections::BTreeMap<String, AgentDef>);

/// How dispatch starts a task's agent (`Agents::launch`): the herdr kind,
/// its argv, and the env its pane is created with (`~` not yet expanded).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Launch {
    pub kind: String,
    pub args: Vec<String>,
    pub env: std::collections::BTreeMap<String, String>,
}

/// Claude's folder-trust dialog opens on "No, exit"; Down moves to "Yes, I
/// trust this folder" and Enter takes it.
const CLAUDE_TRUST_KEYS: [&str; 2] = ["Down", "Enter"];

/// Text only Claude's folder-trust dialog shows. Its bypass-permissions
/// warning also opens on "No, exit" with a yes below, so the same keys would
/// accept that; saved trust presses them only while this is on screen.
const CLAUDE_TRUST_MARKER: &str = "Yes, I trust this folder";

/// Does the prompt at the bottom of `screen` (a pane's text, scrollback
/// included) show `marker`? Only `bottom_prompt` is searched, so a trust
/// prompt answered earlier and still in scrollback does not count for the
/// dialog below it. Case and whitespace are ignored, since a narrow pane
/// wraps the line and a redraw may space it differently.
pub fn shows_trust_marker(screen: &str, marker: &str) -> bool {
    let squash = |s: &str| {
        s.chars()
            .filter(|c| !c.is_whitespace())
            .flat_map(char::to_lowercase)
            .collect::<String>()
    };
    let marker = squash(marker);
    !marker.is_empty() && squash(&bottom_prompt(screen)).contains(&marker)
}

/// The trailing lines of `screen` that make up the prompt at its bottom:
/// those after the last output (a `●` or `⏺` message) and, when there is a
/// menu, after the last option of any menu above the last one (a menu starts
/// at an option numbered 1). The whole screen when neither is there.
fn bottom_prompt(screen: &str) -> String {
    let lines: Vec<&str> = screen.lines().collect();
    let option = |line: &str| -> Option<u32> {
        let t = line
            .trim_start()
            .trim_start_matches(['\u{276f}', '\u{203a}', '>'])
            .trim_start();
        let n = t.bytes().take_while(u8::is_ascii_digit).count();
        match t[n..].chars().next() {
            Some('.' | ')') if n > 0 => t[..n].parse().ok(),
            _ => None,
        }
    };
    let output = |line: &str| line.trim_start().starts_with(['\u{25cf}', '\u{23fa}']);
    let mut start = lines.iter().rposition(|l| output(l)).map_or(0, |i| i + 1);
    if let Some(menu) = lines.iter().rposition(|l| option(l) == Some(1))
        && let Some(prev) = lines[..menu].iter().rposition(|l| option(l).is_some())
    {
        start = start.max(prev + 1);
    }
    lines[start.min(lines.len())..].join("\n")
}

/// What a Claude agent under a permission profile starts with: it denies
/// whatever its allow list and settings do not already allow, instead of
/// asking, so the task never stops at a permission prompt.
const CLAUDE_NO_ASK: [&str; 2] = ["--permission-mode", "dontAsk"];

/// The code of a task whose profile meets agent args that pick a permission
/// mode of their own.
pub const PROFILE_ARGS_CONFLICT: &str = "profile_args_conflict";

/// Does `arg` pick Claude's permission mode, or turn its permissions off?
fn is_permission_arg(arg: &str) -> bool {
    arg == "--permission-mode"
        || arg.starts_with("--permission-mode=")
        || arg == "--dangerously-skip-permissions"
        || arg == "--allow-dangerously-skip-permissions"
}

/// Claude Code's own names for the allow and deny lists (`claude --help`).
/// Both take several patterns and may repeat, so one flag per pattern works.
const CLAUDE_TOOL_FLAGS: (&str, &str) = ("--allowedTools", "--disallowedTools");

impl Agents {
    /// The herdr kind `agent` starts: its definition's `kind`, else its name.
    pub fn kind<'a>(&'a self, agent: &'a str) -> &'a str {
        self.0
            .get(agent)
            .and_then(|d| d.kind.as_deref())
            .unwrap_or(agent)
    }

    /// The keys that accept `agent`'s folder-trust prompt: its own
    /// `trust_keys`, else the built-in ones of its kind (only `claude` has
    /// any). `None` when it has none.
    pub fn trust_keys(&self, agent: &str) -> Option<Vec<String>> {
        let keys = match self.0.get(agent).and_then(|d| d.trust_keys.clone()) {
            Some(keys) => keys,
            None if self.kind(agent) == "claude" => CLAUDE_TRUST_KEYS.map(str::to_string).to_vec(),
            None => return None,
        };
        (!keys.is_empty()).then_some(keys)
    }

    /// What the pane must show before saved trust presses `agent`'s trust
    /// keys: its own `trust_marker`, else the built-in of its kind (only
    /// `claude` has one). `None` when it has none.
    pub fn trust_marker(&self, agent: &str) -> Option<String> {
        let marker = match self.0.get(agent).and_then(|d| d.trust_marker.clone()) {
            Some(marker) => marker,
            None if self.kind(agent) == "claude" => CLAUDE_TRUST_MARKER.to_string(),
            None => return None,
        };
        (!marker.trim().is_empty()).then_some(marker)
    }

    /// The flags that carry `agent`'s allow and deny lists: its own, else the
    /// built-in ones of its kind (only `claude` has any).
    fn tool_flags(&self, agent: &str) -> (Option<String>, Option<String>) {
        let def = self.0.get(agent);
        let builtin = (self.kind(agent) == "claude").then_some(CLAUDE_TOOL_FLAGS);
        (
            def.and_then(|d| d.allow_flag.clone())
                .or(builtin.map(|b| b.0.to_string())),
            def.and_then(|d| d.deny_flag.clone())
                .or(builtin.map(|b| b.1.to_string())),
        )
    }

    /// How to start `spec`'s agent: its kind, `launch_args` and the env of
    /// its definition. Refused as `launch_args` is.
    /// An opencode agent under a profile gets its lists in the env instead
    /// (`opencode::permission_json`), over its definition's, with the
    /// variables that would load another config emptied.
    pub fn launch(&self, spec: &crate::task::DispatchSpec) -> Result<Launch, AgentRefusal> {
        let mut env = self
            .0
            .get(&spec.agent)
            .map(|d| d.env.clone())
            .unwrap_or_default();
        if self.opencode_profile(spec) {
            for key in opencode::CONFIG_ENV {
                env.insert(key.into(), String::new());
            }
            env.insert(
                opencode::PERMISSION_ENV.into(),
                opencode::permission_json(
                    &spec.allow,
                    &spec.deny,
                    spec.profile() == Some(profile::UNRESTRICTED),
                ),
            );
        }
        Ok(Launch {
            kind: self.kind(&spec.agent).to_string(),
            args: self.launch_args(spec)?,
            env,
        })
    }

    /// Does `spec` run an opencode agent under a permission profile? Its
    /// lists then go in `OPENCODE_PERMISSION`, not in flags.
    pub fn opencode_profile(&self, spec: &crate::task::DispatchSpec) -> bool {
        spec.profile().is_some() && self.kind(&spec.agent) == opencode::KIND
    }

    /// The argv after the agent's name for `spec`: its `agent_args`, then,
    /// under a permission profile, the args that stop a Claude agent from
    /// asking (`--permission-mode dontAsk`), then the flag and pattern of
    /// each `allow`, then of each `deny`; an opencode agent under a profile
    /// gets no tool flags, its lists going in `launch`'s env. Refused when a list is not empty
    /// and the agent has no flag for it (`agent_tools_unsupported`):
    /// dropping a deny list without a word would be worse than not
    /// starting. Refused too when a profile applies and the args already
    /// pick a permission mode (`profile_args_conflict`): the agent would
    /// take the last one, and either the profile or the args would be
    /// silently lost.
    pub fn launch_args(
        &self,
        spec: &crate::task::DispatchSpec,
    ) -> Result<Vec<String>, AgentRefusal> {
        let (allow_flag, deny_flag) = self.tool_flags(&spec.agent);
        let mut args = spec.agent_args.clone();
        if let Some(profile) = spec.profile()
            && self.kind(&spec.agent) == "claude"
        {
            if let Some(arg) = args.iter().find(|a| is_permission_arg(a)) {
                return Err(AgentRefusal {
                    code: PROFILE_ARGS_CONFLICT,
                    message: format!(
                        "task runs under profile {profile}, and agent {}'s args set {arg}; \
                         drop it from agent_args or the model's args, or run without a profile",
                        spec.agent
                    ),
                });
            }
            args.extend(CLAUDE_NO_ASK.map(str::to_string));
        }
        if self.opencode_profile(spec) {
            return Ok(args);
        }
        for (list, flag, key) in [
            (&spec.allow, allow_flag, "allow_flag"),
            (&spec.deny, deny_flag, "deny_flag"),
        ] {
            if list.is_empty() {
                continue;
            }
            let Some(flag) = flag else {
                return Err(AgentRefusal {
                    code: "agent_tools_unsupported",
                    message: format!(
                        "agent {} has no {key} to pass its tool list; set [agents.{}] {key} in pastor.toml",
                        spec.agent, spec.agent
                    ),
                });
            };
            for p in list {
                args.push(flag.clone());
                args.push(p.clone());
            }
        }
        Ok(args)
    }
}

/// One model under `[models.<name>]` in `pastor.toml`: the herdr agent kind
/// it runs on and the args that pick it. A task names it with `model`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelDef {
    /// The herdr agent kind (`claude`, `codex`) whose agents can run it.
    pub kind: String,
    /// Put before the task's `agent_args` (`["--model", "claude-sonnet-5"]`).
    pub args: Vec<String>,
}

/// `[models.<name>]`, by model name. No model is built in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Models(pub std::collections::BTreeMap<String, ModelDef>);

/// A description as pastor keeps one: trimmed, and `None` when that leaves
/// nothing. Jobs, flocks, machines and tasks all take theirs through it.
pub fn clean_description(text: Option<&str>) -> Option<String> {
    text.map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

/// Model names go in the store, in events and on the command line, so they
/// keep to the job names' alphabet: `[a-z0-9][a-z0-9_.-]{0,63}`. That also
/// keeps a raw agent arg such as `--model` from passing for one.
pub fn check_model_name(name: &str) -> Result<(), String> {
    let first_ok = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let rest_ok = name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_.-".contains(c));
    if first_ok && rest_ok && name.len() <= 64 {
        Ok(())
    } else {
        Err(format!(
            "model name {name:?} must match [a-z0-9][a-z0-9_.-]{{0,63}}"
        ))
    }
}

/// Profile names go where model names go, so they keep to the same
/// alphabet; a raw arg such as `--permission-mode` never passes for one.
pub fn check_profile_name(name: &str) -> Result<(), String> {
    check_model_name(name).map_err(|e| e.replacen("model name", "profile name", 1))
}

/// The code of a model whose kind is not the task's agent's.
pub const MODEL_KIND_MISMATCH: &str = "model_kind_mismatch";

/// The code of a task whose flock names an agent of a kind the machine has
/// no agent of (`AgentPick::missing_kind`): an unpinned task skips that
/// machine, as for `MODEL_KIND_MISMATCH`.
pub const AGENT_KIND_MISSING: &str = "agent_kind_missing";

/// Why a task's agent cannot run as resolved: a stable code for the CLI's
/// error and a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRefusal {
    /// `unknown_model`, `model_kind_mismatch`, `agent_tools_unsupported`,
    /// `unknown_profile`, `profile_not_allowed` or `profile_args_conflict`.
    pub code: &'static str,
    pub message: String,
}

impl std::fmt::Display for AgentRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl Models {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The names, sorted, for errors and shell completion.
    pub fn names(&self) -> Vec<&str> {
        self.0.keys().map(String::as_str).collect()
    }

    /// `name`'s definition, or `unknown_model`.
    pub fn get(&self, name: &str) -> Result<&ModelDef, AgentRefusal> {
        self.0.get(name).ok_or_else(|| AgentRefusal {
            code: "unknown_model",
            message: if self.0.is_empty() {
                format!("model {name} is not in [models] in pastor.toml, which names none")
            } else {
                format!(
                    "model {name} is not in [models] in pastor.toml; it has {}",
                    self.names().join(", ")
                )
            },
        })
    }

    /// `get`, for a check that only needs to know it is there.
    pub fn check(&self, name: &str) -> Result<(), String> {
        self.get(name).map(drop).map_err(|e| e.message)
    }

    /// Settle `pick`'s model into `spec`: the model's args before the
    /// agent's own. Refused when the model is not defined or its kind is not
    /// the one of the agent the task runs (`agents.kind`).
    pub fn apply(
        &self,
        pick: &AgentPick,
        agents: &Agents,
        spec: &mut crate::task::DispatchSpec,
    ) -> Result<(), AgentRefusal> {
        let Some((name, _)) = &pick.model else {
            return Ok(());
        };
        let def = self.get(name)?;
        let kind = agents.kind(&pick.agent);
        if def.kind != kind {
            // Not the task's own agent: no layer had one of the kind.
            let none = if pick.agent_from == Layer::Ask {
                String::new()
            } else {
                format!(", and no layer's agents.{} names one", def.kind)
            };
            return Err(AgentRefusal {
                code: MODEL_KIND_MISMATCH,
                message: format!(
                    "model {name} runs on {} agents, and agent {} is {kind}{none}",
                    def.kind, pick.agent
                ),
            });
        }
        spec.agent_args = def.args.iter().chain(&pick.agent_args).cloned().collect();
        Ok(())
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
    /// How long a `done` task keeps its pane before pastor closes it, so
    /// `pastor task attach` can still show the agent's last screen. `never`
    /// turns auto-close off.
    pub close_done_after: String,
    /// How long a pull machine (`pull = true` in flock.toml) may go without
    /// a `TaskClaim` or `TaskReport` before the head counts it lost and its
    /// starting and running tasks go stale.
    pub pull_lost_after: String,
    /// Whether an agent pastor started (`ipc::TASK_ENV` in its pane) may
    /// change the fleet: run, send to, retry, close or prune tasks, run jobs,
    /// and edit machines, flocks and jobs. Off by default, so the head
    /// refuses it. A guard against an agent acting on its own; the agent runs
    /// as the same user, so it is not a security boundary.
    pub agents_change_fleet: bool,
    /// How many orchestrator agents (`role = "orchestrator"`) the head runs
    /// at once, of both kinds, outside `max_agents` and job slots. A
    /// scheduled orchestrator's run past it starts no agent
    /// (`orchestrator.held`).
    pub max_orchestrators: u32,
    /// The ssh destination other machines reach the head by. Agents on
    /// machines other than the head's own get it as `ipc::HEAD_ENV`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_address: Option<String>,
    pub defaults: Defaults,
    #[serde(skip_serializing_if = "is_empty_agents")]
    pub agents: Agents,
    #[serde(skip_serializing_if = "Models::is_empty")]
    pub models: Models,
    /// `[profiles.<name>]`: permission profiles beside the built-in ones.
    #[serde(skip_serializing_if = "profile::Profiles::is_empty")]
    pub profiles: profile::Profiles,
    /// What `pastor watch` runs besides the head's events.
    #[serde(skip_serializing_if = "WatchConfig::is_empty")]
    pub watch: WatchConfig,
    /// `[shepherd]`: how this machine takes tasks when its `pastor serve`
    /// runs headless and the head has it as a pull machine.
    #[serde(skip_serializing_if = "ShepherdConfig::is_empty")]
    pub shepherd: ShepherdConfig,
}

/// The variable that makes a headless serve take flock work, as
/// `[shepherd] takes_flock_work = true` does: `1` or `true`.
pub const SHEPHERD_FLOCK_WORK_ENV: &str = "PASTOR_SHEPHERD_FLOCK_WORK";

/// `[shepherd]` in pastor.toml, read by a headless `pastor serve`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShepherdConfig {
    /// The name this machine has in the head's flock.toml, where it is
    /// `pull = true`; the hostname when unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    /// Take any task the head would place on this machine, not only those
    /// pinned to it (`task run --shepherd`, `--machine <this one>`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub takes_flock_work: bool,
    /// Developer option: argv speaking the herdr protocol on stdio, in
    /// place of this machine's own herdr (as `command` in flock.toml).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
}

impl ShepherdConfig {
    pub fn is_empty(&self) -> bool {
        self == &ShepherdConfig::default()
    }

    /// The pull machine this machine is: `machine`, else the hostname.
    pub fn machine_name(&self) -> String {
        self.machine
            .clone()
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(hostname)
    }

    /// Whether it takes flock work: `takes_flock_work`, or
    /// `SHEPHERD_FLOCK_WORK_ENV` set to `1` or `true`.
    pub fn flock_work(&self) -> bool {
        self.takes_flock_work
            || std::env::var(SHEPHERD_FLOCK_WORK_ENV)
                .is_ok_and(|v| matches!(v.trim(), "1" | "true"))
    }
}

/// This machine's hostname, read from the kernel and files rather than a
/// new dependency for `gethostname`; `-` when none says.
pub fn hostname() -> String {
    ["/proc/sys/kernel/hostname", "/etc/hostname"]
        .iter()
        .find_map(|p| {
            std::fs::read_to_string(p)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .or_else(|| std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "-".into())
}

/// `[watch]` in pastor.toml.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WatchConfig {
    /// `[[watch.connector]]`: the connectors whose `[watch]` command `pastor
    /// watch` runs each interval, unless `--connector` names others.
    pub connector: Vec<WatchConnector>,
}

impl WatchConfig {
    pub fn is_empty(&self) -> bool {
        self.connector.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchConnector {
    /// The connector's id, as `connector list` shows it.
    pub name: String,
}

fn is_empty_agents(a: &Agents) -> bool {
    a.0.is_empty()
}

impl Default for PastorConfig {
    fn default() -> Self {
        PastorConfig {
            tick: "10s".into(),
            settle: "10s".into(),
            reconcile_every: "60s".into(),
            request_timeout: "60s".into(),
            agent_ready_timeout: "30s".into(),
            close_done_after: "5s".into(),
            pull_lost_after: "10m".into(),
            agents_change_fleet: false,
            max_orchestrators: 1,
            head_address: None,
            defaults: Defaults::default(),
            agents: Agents::default(),
            models: Models::default(),
            profiles: profile::Profiles::default(),
            watch: WatchConfig::default(),
            shepherd: ShepherdConfig::default(),
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

    /// `text` as the file at `path` would load, with errors that name
    /// `path`. `pastor config edit` checks an edit with it.
    pub fn parse(path: &Path, text: &str) -> anyhow::Result<PastorConfig> {
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
            ("close_done_after", &cfg.close_done_after, false),
            ("pull_lost_after", &cfg.pull_lost_after, false),
        ] {
            if name == "close_done_after" && v == CLOSE_NEVER {
                continue;
            }
            let d = parse_duration(v)
                .map_err(|e| anyhow::anyhow!("{}: {name}: {e}", path.display()))?;
            if !zero_ok && d.is_zero() {
                anyhow::bail!("{}: {name}: must not be zero", path.display());
            }
        }
        for (key, list) in [
            ("defaults.allow", &cfg.defaults.allow),
            ("defaults.deny", &cfg.defaults.deny),
        ] {
            check_tools(key, list).map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        }
        // It ends up as an ssh destination in an agent's pane, the same way a
        // machine's own `ssh` does; reject what that validation rejects.
        if let Some(head) = &cfg.head_address
            && let Some(problem) = flock::ssh_target_problem(head)
        {
            anyhow::bail!(
                "{}: head_address must be an ssh destination: {problem}",
                path.display()
            );
        }
        for (name, def) in &cfg.agents.0 {
            if def.kind.as_deref().is_some_and(|k| k.trim().is_empty()) {
                anyhow::bail!("{}: agents.{name}.kind must not be empty", path.display());
            }
            for key in def.env.keys() {
                let ok = key
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                    && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                if !ok {
                    anyhow::bail!(
                        "{}: agents.{name}.env: {key:?} is not a variable name (letters, digits and _, not starting with a digit)",
                        path.display()
                    );
                }
            }
            for (key, flag) in [
                ("allow_flag", &def.allow_flag),
                ("deny_flag", &def.deny_flag),
            ] {
                if flag.as_deref().is_some_and(|f| f.trim().is_empty()) {
                    anyhow::bail!("{}: agents.{name}.{key} must not be empty", path.display());
                }
            }
            if def.trust_keys.iter().flatten().any(|k| k.trim().is_empty()) {
                anyhow::bail!(
                    "{}: agents.{name}.trust_keys: a key name must not be empty",
                    path.display()
                );
            }
        }
        for (name, def) in &cfg.models.0 {
            check_model_name(name)
                .map_err(|e| anyhow::anyhow!("{}: models: {e}", path.display()))?;
            if def.kind.trim().is_empty() {
                anyhow::bail!("{}: models.{name}.kind must not be empty", path.display());
            }
        }
        check_kind_agent_names(&cfg.defaults.agents)
            .and_then(|()| {
                check_kind_agents(Some(&cfg.defaults.agent), &cfg.defaults.agents, &cfg.agents)
            })
            .map_err(|e| anyhow::anyhow!("{}: defaults.{e}", path.display()))?;
        if let Some(label) = &cfg.defaults.label {
            crate::task::check_label(label)
                .map_err(|e| anyhow::anyhow!("{}: defaults.{e}", path.display()))?;
        }
        if let Some(m) = &cfg.defaults.model {
            cfg.models
                .check(m)
                .map_err(|e| anyhow::anyhow!("{}: defaults.model: {e}", path.display()))?;
        }
        cfg.profiles
            .validate()
            .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
        if let Some(p) = &cfg.defaults.profile {
            cfg.profiles
                .resolve(p)
                .map_err(|e| anyhow::anyhow!("{}: defaults.profile: {e}", path.display()))?;
        }
        if cfg.shepherd.command.as_ref().is_some_and(|c| c.is_empty()) {
            anyhow::bail!("{}: shepherd.command is empty", path.display());
        }
        if let Some(m) = &cfg.shepherd.machine
            && m.trim().is_empty()
        {
            anyhow::bail!("{}: shepherd.machine must not be empty", path.display());
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
    /// `None` when auto-close is off (`never`).
    pub fn close_done_after_duration(&self) -> Option<Duration> {
        if self.close_done_after == CLOSE_NEVER {
            return None;
        }
        Some(duration_or_default(
            &self.close_done_after,
            &PastorConfig::default().close_done_after,
        ))
    }
    pub fn pull_lost_after_duration(&self) -> Duration {
        duration_or_default(
            &self.pull_lost_after,
            &PastorConfig::default().pull_lost_after,
        )
    }
    pub fn agent_ready_timeout_duration(&self) -> Duration {
        duration_or_default(
            &self.agent_ready_timeout,
            &PastorConfig::default().agent_ready_timeout,
        )
    }
}

/// The `close_done_after` value that turns auto-close off.
const CLOSE_NEVER: &str = "never";

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
    /// Agents pastor started may not change the fleet unless pastor.toml
    /// says so.
    #[test]
    fn agents_change_fleet_is_off_unless_set() {
        assert!(!PastorConfig::default().agents_change_fleet);
        let cfg: PastorConfig = toml::from_str("agents_change_fleet = true").unwrap();
        assert!(cfg.agents_change_fleet);
    }

    /// `head_address` is an ssh destination: optional, and when set a
    /// non-empty word.
    #[test]
    fn head_address_is_an_ssh_destination() {
        let path = Path::new("pastor.toml");
        assert_eq!(PastorConfig::default().head_address, None);
        let cfg = PastorConfig::parse(path, "head_address = \"user@head.example\"").unwrap();
        assert_eq!(cfg.head_address.as_deref(), Some("user@head.example"));
        for bad in [
            "\"\"",
            "\"  \"",
            "\"user@head example\"",
            "\"head\\n\"",
            "\"-oProxyCommand=evil\"",
        ] {
            let err = PastorConfig::parse(path, &format!("head_address = {bad}")).unwrap_err();
            assert!(err.to_string().contains("head_address"), "{bad}: {err}");
        }
    }

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

    /// A private dir someone else owns, or reaches through a link someone else
    /// owns, is refused and left as it was: in a shared place such as /tmp
    /// another user can plant either before pastor first runs.
    #[test]
    fn create_private_dir_refuses_a_dir_or_link_another_user_owns() {
        use std::os::unix::fs::MetadataExt;
        let tmp = tempfile::tempdir().unwrap();
        let theirs = tmp.path().join("theirs");
        std::fs::create_dir(&theirs).unwrap();
        std::fs::set_permissions(&theirs, std::fs::Permissions::from_mode(0o755)).unwrap();
        let other = std::fs::metadata(&theirs).unwrap().uid() + 1;
        let err = create_private_dir_as(&theirs, other).unwrap_err();
        assert!(err.to_string().contains("owned by uid"), "{err:#}");
        let mode = std::fs::metadata(&theirs).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "a refused dir keeps its mode");
        // The fast path too: an already private dir is still checked.
        std::fs::set_permissions(&theirs, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(create_private_dir_as(&theirs, other).is_err());

        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&theirs, &link).unwrap();
        assert!(create_private_dir_as(&link, other).is_err());
    }

    /// A symlinked dir (a config dir kept in dotfiles, say) still works when
    /// the link and what it points at are the user's own.
    #[test]
    fn create_private_dir_accepts_an_own_symlinked_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        create_private_dir(&link).unwrap();
        let mode = std::fs::metadata(&real).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    /// A missing dir is created through its existing ancestors, so each one
    /// is checked first: a symlink another user owns, or a dir anyone can
    /// write to without the sticky bit that is neither root's nor the user's,
    /// would let someone else swap in a parent of their choosing. Nothing is
    /// created under a refused ancestor.
    #[test]
    fn create_private_dir_refuses_an_unsafe_ancestor() {
        use std::os::unix::fs::MetadataExt;
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let other = std::fs::metadata(&real).unwrap().uid() + 1;

        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let err = create_private_dir_as(&link.join("state/ssh"), other).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err:#}");
        assert!(!real.join("state").exists(), "nothing is made under it");

        let open = tmp.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        let err = create_private_dir_as(&open.join("state/ssh"), other).unwrap_err();
        assert!(err.to_string().contains("writable"), "{err:#}");
        assert!(!open.join("state").exists(), "nothing is made under it");
    }

    /// The usual ancestors pass: the user's own dirs, a link of their own, one
    /// they own that others can write to, and a sticky shared dir like /tmp.
    #[test]
    fn create_private_dir_accepts_safe_ancestors() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        create_private_dir(&link.join("a/b")).unwrap();
        assert!(real.join("a/b").is_dir());

        let open = tmp.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        create_private_dir(&open.join("a")).unwrap();

        let sticky = tmp.path().join("sticky");
        std::fs::create_dir(&sticky).unwrap();
        std::fs::set_permissions(&sticky, std::fs::Permissions::from_mode(0o1777)).unwrap();
        create_private_dir(&sticky.join("a")).unwrap();
    }

    #[test]
    fn create_private_dir_refuses_a_file_or_a_dangling_link() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("file");
        std::fs::write(&file, "").unwrap();
        let err = create_private_dir(&file).unwrap_err();
        assert!(err.to_string().contains("not a directory"), "{err:#}");
        let dangling = tmp.path().join("dangling");
        std::os::unix::fs::symlink(tmp.path().join("nowhere"), &dangling).unwrap();
        assert!(create_private_dir(&dangling).is_err());
        assert!(
            !tmp.path().join("nowhere").exists(),
            "a link is never followed to create its target"
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

    /// A label comes from the first of the ask, the flock and `[defaults]`
    /// that sets one; none leaves the built-in.
    #[test]
    fn a_label_comes_from_its_layers() {
        let work = flock::FlockEntry {
            name: "work".into(),
            label: Some("w/{{ task.id }}".into()),
            ..Default::default()
        };
        let bare = flock::FlockEntry {
            name: "bare".into(),
            ..Default::default()
        };
        let d = Defaults {
            label: Some("d/{{ task.id }}".into()),
            ..Default::default()
        };
        let got =
            |d: &Defaults, ask: Option<&str>, f: &flock::FlockEntry| d.resolve_label(ask, Some(f));
        assert_eq!(
            got(&d, Some("a"), &work),
            Some(("a".to_string(), Layer::Ask))
        );
        assert_eq!(
            got(&d, None, &work),
            Some(("w/{{ task.id }}".to_string(), Layer::Flock))
        );
        assert_eq!(
            got(&d, None, &bare),
            Some(("d/{{ task.id }}".to_string(), Layer::Defaults))
        );
        assert_eq!(got(&Defaults::default(), None, &bare), None);
    }

    /// `[defaults] label` is checked on load like any other template.
    #[test]
    fn a_defaults_label_is_checked_on_load() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.toml");
        std::fs::write(
            &path,
            "[defaults]\nlabel = \"{{ machine }}/{{ task.id }}\"\n",
        )
        .unwrap();
        let cfg = PastorConfig::load(&path).unwrap();
        assert_eq!(
            cfg.defaults.label.as_deref(),
            Some("{{ machine }}/{{ task.id }}")
        );
        std::fs::write(&path, "[defaults]\nlabel = \"{{ nope }}\"\n").unwrap();
        let err = PastorConfig::load(&path).unwrap_err().to_string();
        assert!(err.contains("defaults.label: unknown placeholder"), "{err}");
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

    /// Allow and deny lists add up from `[defaults]` through the flock to
    /// the task, in that order and without repeats; a deny anywhere drops the
    /// same pattern from allow, so no layer can lift another's deny.
    #[test]
    fn tool_lists_add_up_and_deny_wins() {
        let d = Defaults {
            allow: vec!["Read".into(), "Bash(git:*)".into()],
            deny: vec!["Bash(rm:*)".into()],
            ..Default::default()
        };
        let work = flock::FlockEntry {
            name: "work".into(),
            allow: vec!["Edit".into(), "Read".into()],
            deny: vec!["Bash(git:*)".into()],
            ..Default::default()
        };
        let ask = AgentChoice {
            allow: vec!["Bash(rm:*)".into(), "Write".into()],
            ..Default::default()
        };
        let p = d.resolve_agent(&ask, Some(&work));
        assert_eq!(p.allow, vec!["Read", "Edit", "Write"]);
        assert_eq!(p.deny, vec!["Bash(rm:*)", "Bash(git:*)"]);
        let p = d.resolve_agent(&AgentChoice::default(), None);
        assert_eq!(p.allow, vec!["Read", "Bash(git:*)"]);
        assert_eq!(p.deny, vec!["Bash(rm:*)"]);
    }

    /// `[models.<name>]` needs a kind and args, a name in the job names'
    /// alphabet, and nothing else; `[defaults] model` must name one.
    #[test]
    fn models_load_and_bad_ones_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.toml");
        std::fs::write(
            &path,
            "[defaults]\nmodel = \"sonnet\"\n[models.sonnet]\nkind = \"claude\"\nargs = [\"--model\", \"claude-sonnet-5\"]\n[models.plain]\nkind = \"codex\"\nargs = []\n",
        )
        .unwrap();
        let cfg = PastorConfig::load(&path).unwrap();
        assert_eq!(cfg.defaults.model.as_deref(), Some("sonnet"));
        assert_eq!(
            cfg.models.0["sonnet"].args,
            vec!["--model", "claude-sonnet-5"]
        );
        assert!(cfg.models.0["plain"].args.is_empty());
        for (text, says) in [
            ("[models.sonnet]\nkind = \"claude\"\n", "args"),
            ("[models.sonnet]\nargs = []\n", "kind"),
            (
                "[models.sonnet]\nkind = \" \"\nargs = []\n",
                "models.sonnet.kind",
            ),
            (
                "[models.Sonnet]\nkind = \"claude\"\nargs = []\n",
                "must match",
            ),
            (
                "[models.\"--model\"]\nkind = \"claude\"\nargs = []\n",
                "must match",
            ),
            (
                "[models.sonnet]\nkind = \"claude\"\nargs = []\nenv = {}\n",
                "env",
            ),
            ("[defaults]\nmodel = \"haiku\"\n", "defaults.model"),
        ] {
            std::fs::write(&path, text).unwrap();
            let err = format!("{:#}", PastorConfig::load(&path).unwrap_err());
            assert!(err.contains(says), "{text}: {err}");
        }
    }

    /// `[profiles]` loads, and a bad profile fails the file's load.
    #[test]
    fn profiles_load_and_bad_ones_fail_the_load() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.toml");
        std::fs::write(
            &path,
            "[profiles.ci]\nextends = \"develop\"\nallow = [\"Bash(docker:*)\"]\n",
        )
        .unwrap();
        let cfg = PastorConfig::load(&path).unwrap();
        let ci = cfg.profiles.resolve("ci").unwrap();
        assert_eq!(ci.chain, vec!["ci", "develop"]);
        std::fs::write(&path, "[profiles.ci]\nextends = \"nope\"\n").unwrap();
        let err = format!("{:#}", PastorConfig::load(&path).unwrap_err());
        assert!(err.contains("profiles.ci") && err.contains("nope"), "{err}");
    }

    /// `summary` comes from the first of the ask (`task run`, the job), the
    /// flock and `[defaults]` that sets one; with none it is `ask`.
    #[test]
    fn the_summary_setting_comes_from_the_first_layer_that_sets_one() {
        use crate::task::SummaryMode;
        let d = Defaults {
            summary: Some(SummaryMode::Off),
            ..Default::default()
        };
        let flock = flock::FlockEntry {
            name: "p".into(),
            summary: Some(SummaryMode::Require),
            ..Default::default()
        };
        assert_eq!(
            d.resolve_summary(Some(SummaryMode::Ask), Some(&flock)),
            SummaryMode::Ask
        );
        assert_eq!(d.resolve_summary(None, Some(&flock)), SummaryMode::Require);
        assert_eq!(
            d.resolve_summary(None, Some(&flock::FlockEntry::default())),
            SummaryMode::Off
        );
        assert_eq!(
            Defaults::default().resolve_summary(None, None),
            SummaryMode::Ask
        );
    }

    /// `[defaults] summary` loads as one of its words; a file without it
    /// loads as before, and asks; another word fails the load.
    #[test]
    fn defaults_summary_loads_and_is_ask_when_absent() {
        use crate::task::SummaryMode;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.toml");
        std::fs::write(&path, "[defaults]\nagent = \"claude\"\n").unwrap();
        let cfg = PastorConfig::load(&path).unwrap();
        assert_eq!(cfg.defaults.summary, None);
        assert_eq!(cfg.defaults.resolve_summary(None, None), SummaryMode::Ask);
        std::fs::write(&path, "[defaults]\nsummary = \"require\"\n").unwrap();
        let cfg = PastorConfig::load(&path).unwrap();
        assert_eq!(cfg.defaults.summary, Some(SummaryMode::Require));
        std::fs::write(&path, "[defaults]\nsummary = \"always\"\n").unwrap();
        assert!(PastorConfig::load(&path).is_err());
    }

    /// The level comes from the first of the ask, the pinned machine, the
    /// flock and `[defaults]` that sets one; with none it is `normal`.
    #[test]
    fn the_priority_comes_from_the_first_layer_that_sets_one() {
        use crate::task::Priority;
        let d = Defaults {
            priority: Some(Priority::Low),
            ..Default::default()
        };
        let flock = flock::FlockEntry {
            name: "p".into(),
            priority: Some(Priority::High),
            ..Default::default()
        };
        let machine: flock::MachineConfig =
            toml::from_str("name = \"m\"\nlocal = true\npriority = \"critical\"\n").unwrap();
        assert_eq!(
            d.resolve_priority(Some(Priority::Normal), Some(&machine), Some(&flock)),
            (Priority::Normal, Some(Layer::Ask))
        );
        // The flock before the machine: a project's flock sets the level
        // on a shared machine.
        assert_eq!(
            d.resolve_priority(None, Some(&machine), Some(&flock)),
            (Priority::High, Some(Layer::Flock))
        );
        assert_eq!(
            d.resolve_priority(None, Some(&machine), None),
            (Priority::Critical, Some(Layer::Machine))
        );
        assert_eq!(
            d.resolve_priority(None, None, None),
            (Priority::Low, Some(Layer::Defaults))
        );
        assert_eq!(
            Defaults::default().resolve_priority(None, None, None),
            (Priority::Normal, None)
        );
    }

    /// `[defaults] priority`, a flock's and a machine's take only the four
    /// levels.
    #[test]
    fn a_priority_in_a_config_file_must_be_a_level() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.toml");
        std::fs::write(&path, "[defaults]\npriority = \"high\"\n").unwrap();
        assert_eq!(
            PastorConfig::load(&path).unwrap().defaults.priority,
            Some(crate::task::Priority::High)
        );
        std::fs::write(&path, "[defaults]\npriority = \"urgent\"\n").unwrap();
        let err = format!("{:#}", PastorConfig::load(&path).unwrap_err());
        assert!(err.contains("urgent"), "{err}");
        for text in [
            "[[flock]]\nname = \"p\"\ndefault = true\npriority = \"asap\"\n",
            "[[machine]]\nname = \"m\"\nlocal = true\npriority = \"asap\"\n",
        ] {
            let err = format!(
                "{:#}",
                flock::Flock::parse(std::path::Path::new("flock.toml"), text).unwrap_err()
            );
            assert!(err.contains("asap"), "{text}: {err}");
        }
    }

    /// The model comes from the first of the ask, the machine, the flock
    /// and `[defaults]` that names one, and its args go before the agent's.
    #[test]
    fn the_model_comes_from_the_first_layer_that_names_one() {
        let d = Defaults {
            model: Some("opus".into()),
            agent_args: vec!["-v".into()],
            ..Default::default()
        };
        let flock = flock::FlockEntry {
            name: "p".into(),
            model: Some("sonnet".into()),
            ..Default::default()
        };
        let machine: flock::MachineConfig =
            toml::from_str("name = \"m\"\nlocal = true\nmodel = \"haiku\"\n").unwrap();
        let ask = |model: Option<&str>| AgentChoice {
            model: model.map(Into::into),
            ..Default::default()
        };
        let model = |p: AgentPick| p.model.unwrap();
        assert_eq!(
            model(d.resolve_agent_on(&ask(Some("gpt")), Some(&machine), Some(&flock))),
            ("gpt".into(), Layer::Ask)
        );
        assert_eq!(
            model(d.resolve_agent_on(&ask(None), Some(&machine), Some(&flock))),
            ("sonnet".into(), Layer::Flock)
        );
        assert_eq!(
            model(d.resolve_agent_on(&ask(None), Some(&machine), None)),
            ("haiku".into(), Layer::Machine)
        );
        assert_eq!(
            model(d.resolve_agent(&ask(None), None)),
            ("opus".into(), Layer::Defaults)
        );
        assert_eq!(
            Defaults::default().resolve_agent(&ask(None), None).model,
            None
        );

        let models: Models = toml::from_str(
            "[opus]\nkind = \"claude\"\nargs = [\"--model\", \"claude-opus-5-5\"]\n",
        )
        .unwrap();
        let agents = Agents::default();
        let pick = d.resolve_agent(&ask(None), None);
        let mut spec = spec_with("claude", &[], &[]);
        pick.apply_to(&mut spec);
        models.apply(&pick, &agents, &mut spec).unwrap();
        assert_eq!(spec.agent_args, vec!["--model", "claude-opus-5-5", "-v"]);

        let codex = AgentChoice {
            agent: Some("codex".into()),
            ..Default::default()
        };
        let err = models
            .apply(&d.resolve_agent(&codex, None), &agents, &mut spec)
            .unwrap_err();
        assert_eq!(err.code, MODEL_KIND_MISMATCH);
        let err = models
            .apply(
                &d.resolve_agent(&ask(Some("gpt")), None),
                &agents,
                &mut spec,
            )
            .unwrap_err();
        assert_eq!(err.code, "unknown_model");
    }

    /// A model of another kind than the default agent's runs on the first
    /// layer's agent of that kind: its own `agent`, else its `agents` entry.
    /// That agent takes args only from layers that name it; claude's never
    /// reach it. An agent the task named is kept, and so is the default one
    /// when no layer has the kind.
    #[test]
    fn a_model_of_another_kind_takes_the_agent_named_for_its_kind() {
        let models: Models = toml::from_str(
            "[gpt]\nkind = \"opencode\"\nargs = [\"--model\", \"openai/gpt-5.5\"]\n",
        )
        .unwrap();
        let agents: Agents = toml::from_str(
            "[claude-personal]\nkind = \"claude\"\n[oc-work]\nkind = \"opencode\"\n",
        )
        .unwrap();
        let d = Defaults {
            agent_args: vec!["--permission-mode".into(), "auto".into()],
            ..Default::default()
        };
        let flock: flock::FlockEntry =
            toml::from_str("name = \"p\"\nagent = \"claude-personal\"\nagent_args = [\"-v\"]\n")
                .unwrap();
        let machine = |extra: &str| -> flock::MachineConfig {
            toml::from_str(&format!("name = \"m\"\nlocal = true\n{extra}")).unwrap()
        };
        let gpt = AgentChoice {
            model: Some("gpt".into()),
            ..Default::default()
        };
        let pick = |ask: &AgentChoice, m: &flock::MachineConfig| {
            d.resolve_agent_for(ask, Some(m), Some(&flock), &models, &agents)
        };

        let p = pick(&gpt, &machine("agents = { opencode = \"opencode\" }\n"));
        assert_eq!(
            (p.agent.as_str(), p.agent_from, p.by_kind),
            ("opencode", Layer::Machine, true)
        );
        assert!(p.agent_args.is_empty(), "{:?}", p.agent_args);
        assert_eq!(p.args_from, None);
        let mut spec = spec_with("claude", &[], &[]);
        p.apply_to(&mut spec);
        models.apply(&p, &agents, &mut spec).unwrap();
        assert_eq!(spec.agent_args, vec!["--model", "openai/gpt-5.5"]);

        // The machine's own agent of the kind wins, with its own args; a
        // layer's args with no agent stay with the default agent.
        let m = machine("agent = \"oc-work\"\nagent_args = [\"--x\"]\n");
        let p = pick(&gpt, &m);
        assert_eq!((p.agent.as_str(), p.by_kind), ("oc-work", false));
        assert_eq!(p.agent_args, vec!["--x"]);
        let m = machine("agent_args = [\"--claude-only\"]\nagents = { opencode = \"oc-work\" }\n");
        let p = pick(&gpt, &m);
        assert_eq!(p.agent, "oc-work");
        assert!(p.agent_args.is_empty(), "{:?}", p.agent_args);

        // A flock's agents entry when the machine has none.
        let flock_oc: flock::FlockEntry = toml::from_str(
            "name = \"p\"\nagent = \"claude-personal\"\nagents = { opencode = \"oc-work\" }\n",
        )
        .unwrap();
        let p = d.resolve_agent_for(&gpt, Some(&machine("")), Some(&flock_oc), &models, &agents);
        assert_eq!(
            (p.agent.as_str(), p.agent_from, p.by_kind),
            ("oc-work", Layer::Flock, true)
        );

        // No layer has one: the default agent, for `apply` to refuse.
        let p = pick(&gpt, &machine(""));
        assert_eq!(p.agent, "claude-personal");
        let err = models.apply(&p, &agents, &mut spec).unwrap_err();
        assert_eq!(err.code, MODEL_KIND_MISMATCH);
        assert!(err.message.contains("no layer's agents.opencode"), "{err}");

        // The task's own agent is kept.
        let asked = AgentChoice {
            agent: Some("claude".into()),
            ..gpt.clone()
        };
        let p = pick(&asked, &machine("agents = { opencode = \"opencode\" }\n"));
        assert_eq!(p.agent, "claude");
        let err = models.apply(&p, &agents, &mut spec).unwrap_err();
        assert!(!err.message.contains("agents."), "{err}");
    }

    /// A flock's timeout and place come after the task's or job's and
    /// before `[defaults]`; a flock's timeout is checked on load.
    #[test]
    fn a_flocks_timeout_and_place_come_before_the_defaults() {
        use crate::task::Place;
        let d = Defaults::default();
        let flock: flock::FlockEntry =
            toml::from_str("name = \"p\"\ntimeout = \"30m\"\nplace = \"pastor\"\n").unwrap();
        assert_eq!(d.resolve_timeout(Some(60), Some(&flock)), (60, Layer::Ask));
        assert_eq!(d.resolve_timeout(None, Some(&flock)), (1800, Layer::Flock));
        assert_eq!(d.resolve_timeout(None, None), (7200, Layer::Defaults));
        let own = Place::Own;
        assert_eq!(
            d.resolve_place(Some(&own), Some(&flock)),
            (Place::Own, Layer::Ask)
        );
        assert_eq!(
            d.resolve_place(None, Some(&flock)),
            (Place::Pastor, Layer::Flock)
        );
        assert_eq!(d.resolve_place(None, None), (Place::Repo, Layer::Defaults));

        let f: flock::Flock = toml::from_str(
            "[[machine]]\nname = \"m\"\nlocal = true\n[[flock]]\nname = \"p\"\ndefault = true\ntimeout = \"soon\"\n",
        )
        .unwrap();
        let err = f.validate().unwrap_err();
        assert!(err.contains("flock p: timeout"), "{err}");
        assert!(
            toml::from_str::<flock::FlockEntry>("name = \"p\"\nplace = \"nowhere\"\n").is_err()
        );
    }

    /// A flock that names an agent sets its kind; the machine picks which
    /// agent of that kind runs: its `agent` if of the kind, else its
    /// `agents` entry. A machine whose agent is of another kind and has no
    /// entry for this one cannot run the task. A machine that names no
    /// agent runs the flock's, and the task's own agent is kept.
    #[test]
    fn a_flocks_agent_kind_takes_the_machines_agent_of_that_kind() {
        let models = Models::default();
        let agents: Agents = toml::from_str(
            "[claude-personal]\nkind = \"claude\"\n[oc-work]\nkind = \"opencode\"\n",
        )
        .unwrap();
        let d = Defaults::default();
        let flock: flock::FlockEntry =
            toml::from_str("name = \"p\"\nagent = \"opencode\"\nagent_args = [\"-q\"]\n").unwrap();
        let machine = |extra: &str| -> flock::MachineConfig {
            toml::from_str(&format!("name = \"m\"\nlocal = true\n{extra}")).unwrap()
        };
        let pick = |ask: &AgentChoice, m: &flock::MachineConfig| {
            d.resolve_agent_for(ask, Some(m), Some(&flock), &models, &agents)
        };
        let none = AgentChoice::default();

        // The machine's own agent, of the flock's kind.
        let p = pick(
            &none,
            &machine("agent = \"oc-work\"\nagent_args = [\"--x\"]\n"),
        );
        assert_eq!(
            (p.agent.as_str(), p.agent_from),
            ("oc-work", Layer::Machine)
        );
        assert_eq!(p.agent_args, vec!["--x"]);
        assert_eq!(p.missing_kind, None);

        // Its agent is claude: its agents entry for opencode, with no args
        // written for another agent.
        let p = pick(
            &none,
            &machine(
                "agent = \"claude-personal\"\nagent_args = [\"-v\"]\nagents = { opencode = \"oc-work\" }\n",
            ),
        );
        assert_eq!(
            (p.agent.as_str(), p.agent_from, p.by_kind),
            ("oc-work", Layer::Machine, true)
        );
        assert!(p.agent_args.is_empty(), "{:?}", p.agent_args);
        assert_eq!(p.missing_kind, None);

        // No opencode agent there: the machine cannot run the task.
        let p = pick(&none, &machine("agent = \"claude-personal\"\n"));
        assert_eq!(p.missing_kind.as_deref(), Some("opencode"));

        // A machine that names no agent runs the flock's, with its args.
        let p = pick(&none, &machine(""));
        assert_eq!((p.agent.as_str(), p.agent_from), ("opencode", Layer::Flock));
        assert_eq!(p.agent_args, vec!["-q"]);
        assert_eq!(p.missing_kind, None);

        // The task's own agent is kept.
        let asked = AgentChoice {
            agent: Some("claude".into()),
            ..Default::default()
        };
        let p = pick(&asked, &machine("agent = \"claude-personal\"\n"));
        assert_eq!((p.agent.as_str(), p.missing_kind), ("claude", None));

        // A flock with no agent leaves the machine's alone.
        let plain = flock::FlockEntry {
            name: "p".into(),
            ..Default::default()
        };
        let p = d.resolve_agent_for(
            &none,
            Some(&machine("agent = \"claude-personal\"\n")),
            Some(&plain),
            &models,
            &agents,
        );
        assert_eq!(
            (p.agent.as_str(), p.missing_kind),
            ("claude-personal", None)
        );
    }

    /// An `agents` entry whose agent is of another kind than its key, and
    /// one for the kind of the layer's own agent, fail the load: in
    /// `[defaults]`, and on a flock or machine against `[agents]`.
    #[test]
    fn a_bad_agents_entry_fails_the_load() {
        let path = Path::new("pastor.toml");
        let agents = "[agents.claude-personal]\nkind = \"claude\"\n";
        let cfg = PastorConfig::parse(
            path,
            &format!("[defaults]\nagents = {{ opencode = \"opencode\" }}\n{agents}"),
        )
        .unwrap();
        assert_eq!(cfg.defaults.agents["opencode"], "opencode");
        for (defaults, says) in [
            (
                "agents = { opencode = \"claude-personal\" }",
                "defaults.agents.opencode: agent claude-personal is claude, not opencode",
            ),
            (
                "agents = { claude = \"claude-personal\" }",
                "defaults.agents.claude: its own agent claude is already claude",
            ),
            (
                "agents = { opencode = \"\" }",
                "defaults.agents.opencode must not be empty",
            ),
        ] {
            let err = format!(
                "{:#}",
                PastorConfig::parse(path, &format!("[defaults]\n{defaults}\n{agents}"))
                    .unwrap_err()
            );
            assert!(err.contains(says), "{defaults}: {err}");
        }

        let config = PastorConfig::parse(path, agents).unwrap();
        let flock = |text: &str| {
            flock::Flock::parse(Path::new("flock.toml"), text)
                .unwrap()
                .check_config(&config.models, &config.agents, &config.profiles)
        };
        flock(
            "[[flock]]\nname = \"p\"\ndefault = true\nagent = \"claude-personal\"\nagents = { opencode = \"opencode\" }\n",
        )
        .unwrap();
        for (text, says) in [
            (
                "[[machine]]\nname = \"m\"\nlocal = true\nagents = { opencode = \"claude-personal\" }\n",
                "machine m: agents.opencode: agent claude-personal is claude, not opencode",
            ),
            (
                "[[flock]]\nname = \"p\"\ndefault = true\nagent = \"claude-personal\"\nagents = { claude = \"claude\" }\n",
                "flock p: agents.claude: its own agent claude-personal is already claude",
            ),
        ] {
            let err = flock(text).unwrap_err().to_string();
            assert!(err.contains(says), "{text}: {err}");
        }
    }

    fn spec_with(agent: &str, allow: &[&str], deny: &[&str]) -> crate::task::DispatchSpec {
        crate::task::DispatchSpec {
            agent: agent.into(),
            agent_args: vec!["--model".into(), "m".into()],
            allow: allow.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
            repo: None,
            worktree: false,
            branch: None,
            machine: None,
            tags: vec![],
            timeout_secs: 60,
            checkout: None,
            reopen: None,
            agent_source: None,
            place: Default::default(),
            session_id: None,
            label: Default::default(),
            summary: Default::default(),
        }
    }

    /// Claude gets its own flags, one per pattern after its args; an agent
    /// with no flag for a list it was given is refused, not started without
    /// it; one that sets its own flags gets those.
    #[test]
    fn launch_args_turn_tool_lists_into_the_agents_flags() {
        let agents = Agents::default();
        let spec = spec_with("claude", &["Bash(git log:*)", "Edit"], &["Bash(rm:*)"]);
        assert_eq!(
            agents.launch_args(&spec).unwrap(),
            vec![
                "--model",
                "m",
                "--allowedTools",
                "Bash(git log:*)",
                "--allowedTools",
                "Edit",
                "--disallowedTools",
                "Bash(rm:*)",
            ]
        );
        assert_eq!(
            agents.launch_args(&spec_with("codex", &[], &[])).unwrap(),
            vec!["--model", "m"]
        );
        let err = agents
            .launch_args(&spec_with("codex", &[], &["Bash(rm:*)"]))
            .unwrap_err();
        assert!(err.message.contains("[agents.codex] deny_flag"), "{err}");

        let mut own = Agents::default();
        own.0.insert(
            "codex".into(),
            AgentDef {
                deny_flag: Some("--deny".into()),
                ..Default::default()
            },
        );
        assert_eq!(
            own.launch_args(&spec_with("codex", &[], &["x"])).unwrap(),
            vec!["--model", "m", "--deny", "x"]
        );
        assert!(own.launch_args(&spec_with("codex", &["y"], &[])).is_err());
    }

    /// Under a profile a Claude agent starts with `--permission-mode
    /// dontAsk` after its args and before its tool flags; args that pick a
    /// permission mode of their own are refused. An agent of another kind
    /// gets the lists only, through its own flags.
    #[test]
    fn a_profile_starts_claude_without_asking() {
        let with_profile = |agent: &str, args: &[&str]| {
            let mut spec = spec_with(agent, &["Edit"], &["Bash(sudo:*)"]);
            spec.agent_args = args.iter().map(|s| s.to_string()).collect();
            spec.agent_source = Some(Box::new(crate::task::AgentSource {
                ask: AgentChoice::default(),
                agent: "defaults".into(),
                agent_args: None,
                model: None,
                model_from: None,
                profile: Some("develop".into()),
                profile_from: Some("defaults".into()),
                timeout_from: None,
                place_from: None,
            }));
            spec
        };
        let agents = Agents::default();
        assert_eq!(
            agents
                .launch_args(&with_profile("claude", &["--model", "m"]))
                .unwrap(),
            vec![
                "--model",
                "m",
                "--permission-mode",
                "dontAsk",
                "--allowedTools",
                "Edit",
                "--disallowedTools",
                "Bash(sudo:*)"
            ]
        );
        for arg in [
            &["--permission-mode", "bypassPermissions"][..],
            &["--permission-mode=acceptEdits"],
            &["--dangerously-skip-permissions"],
        ] {
            let err = agents
                .launch_args(&with_profile("claude", arg))
                .unwrap_err();
            assert_eq!(err.code, PROFILE_ARGS_CONFLICT, "{arg:?}");
            assert!(err.message.contains("profile develop"), "{err}");
        }
        // Without a profile the same args pass as written.
        let mut plain = with_profile("claude", &["--dangerously-skip-permissions"]);
        plain.agent_source = None;
        assert_eq!(
            agents.launch_args(&plain).unwrap()[..1],
            ["--dangerously-skip-permissions"]
        );
        // A definition of kind claude is claude.
        let personal: Agents = toml::from_str("[claude-personal]\nkind = \"claude\"\n").unwrap();
        assert!(
            personal
                .launch_args(&with_profile("claude-personal", &[]))
                .unwrap()
                .contains(&"dontAsk".to_string())
        );
        let codex: Agents =
            toml::from_str("[codex]\nallow_flag = \"--allow\"\ndeny_flag = \"--deny\"\n").unwrap();
        assert_eq!(
            codex
                .launch_args(&with_profile("codex", &["--permission-mode", "x"]))
                .unwrap(),
            vec![
                "--permission-mode",
                "x",
                "--allow",
                "Edit",
                "--deny",
                "Bash(sudo:*)"
            ]
        );
    }

    /// Under a profile an opencode agent gets its lists as
    /// `OPENCODE_PERMISSION`, not as flags, whatever flags it defines, and
    /// the variables that point it at another config emptied, over its own
    /// env. Without a profile its lists still need flags.
    #[test]
    fn a_profile_reaches_opencode_through_its_env() {
        let mut spec = spec_with("opencode", &["Edit"], &["Bash(sudo:*)"]);
        spec.agent_source = Some(Box::new(crate::task::AgentSource {
            ask: AgentChoice::default(),
            agent: "defaults".into(),
            agent_args: None,
            model: None,
            model_from: None,
            profile: Some("develop".into()),
            profile_from: Some("defaults".into()),
            timeout_from: None,
            place_from: None,
        }));
        let agents: Agents = toml::from_str(
            "[opencode]\nallow_flag = \"--allow\"\n\
             env = { OPENCODE_CONFIG = \"~/x.json\", OPENCODE_PERMISSION = \"{}\", KEEP = \"1\" }\n",
        )
        .unwrap();
        let launch = agents.launch(&spec).unwrap();
        assert_eq!(launch.kind, "opencode");
        assert_eq!(launch.args, vec!["--model", "m"]);
        assert_eq!(
            launch.env["OPENCODE_PERMISSION"],
            opencode::permission_json(&spec.allow, &spec.deny, false)
        );
        for key in opencode::CONFIG_ENV {
            assert_eq!(launch.env[key], "", "{key}");
        }
        assert_eq!(launch.env["KEEP"], "1");

        // A definition of kind opencode is opencode.
        let mine: Agents = toml::from_str("[oc]\nkind = \"opencode\"\n").unwrap();
        spec.agent = "oc".into();
        assert!(
            mine.launch(&spec)
                .unwrap()
                .env
                .contains_key("OPENCODE_PERMISSION")
        );

        spec.agent = "opencode".into();
        spec.agent_source = None;
        let err = Agents::default().launch(&spec).unwrap_err();
        assert_eq!(err.code, "agent_tools_unsupported");
        spec.allow.clear();
        spec.deny.clear();
        assert!(Agents::default().launch(&spec).unwrap().env.is_empty());
    }

    /// A task's profile comes from the first layer that names one, like its
    /// model; `[defaults] profile` must be one there is.
    #[test]
    fn the_profile_comes_from_the_first_layer_that_names_one() {
        let d = Defaults {
            profile: Some("review".into()),
            ..Defaults::default()
        };
        let flock = flock::FlockEntry {
            name: "f".into(),
            profile: Some("develop".into()),
            ..Default::default()
        };
        let machine: flock::MachineConfig =
            toml::from_str("name = \"m\"\nlocal = true\nprofile = \"ci\"\n").unwrap();
        let ask = |profile: Option<&str>| AgentChoice {
            profile: profile.map(Into::into),
            ..Default::default()
        };
        let profile = |p: AgentPick| p.profile.unwrap();
        assert_eq!(
            profile(d.resolve_agent_on(&ask(Some("x")), Some(&machine), Some(&flock))),
            ("x".into(), Layer::Ask)
        );
        assert_eq!(
            profile(d.resolve_agent_on(&ask(None), Some(&machine), Some(&flock))),
            ("develop".into(), Layer::Flock)
        );
        assert_eq!(
            profile(d.resolve_agent_on(&ask(None), Some(&machine), None)),
            ("ci".into(), Layer::Machine)
        );
        // The machine's own profile, which decides `unrestricted`, is
        // still its own before the flock's.
        assert_eq!(
            d.own_profile(Some(&machine), Some(&flock)).as_deref(),
            Some("ci")
        );
        assert_eq!(
            d.own_profile(None, Some(&flock)).as_deref(),
            Some("develop")
        );
        assert_eq!(d.own_profile(None, None).as_deref(), Some("review"));
        assert_eq!(
            profile(d.resolve_agent(&ask(None), None)),
            ("review".into(), Layer::Defaults)
        );
        assert_eq!(
            Defaults::default().resolve_agent(&ask(None), None).profile,
            None
        );

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.toml");
        std::fs::write(&path, "[defaults]\nprofile = \"ci\"\n[profiles.ci]\n").unwrap();
        assert_eq!(
            PastorConfig::load(&path)
                .unwrap()
                .defaults
                .profile
                .as_deref(),
            Some("ci")
        );
        std::fs::write(&path, "[defaults]\nprofile = \"nope\"\n").unwrap();
        let err = format!("{:#}", PastorConfig::load(&path).unwrap_err());
        assert!(err.contains("defaults.profile: profile nope"), "{err}");
    }

    #[test]
    fn tool_lists_load_from_defaults_and_bad_ones_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.toml");
        std::fs::write(
            &path,
            "[defaults]\nallow = [\"Bash(git:*)\"]\ndeny = [\"WebFetch\"]\n[agents.codex]\nallow_flag = \"--allow\"\n",
        )
        .unwrap();
        let cfg = PastorConfig::load(&path).unwrap();
        assert_eq!(cfg.defaults.allow, vec!["Bash(git:*)"]);
        assert_eq!(cfg.defaults.deny, vec!["WebFetch"]);
        assert_eq!(cfg.agents.0["codex"].allow_flag.as_deref(), Some("--allow"));

        for (text, want) in [
            (
                "[defaults]\ndeny = [\"\"]\n",
                "defaults.deny: a tool pattern must not be empty",
            ),
            (
                "[defaults]\nallow = [\"--dangerously-skip-permissions\"]\n",
                "agent flags go in agent_args",
            ),
            (
                "[agents.codex]\ndeny_flag = \" \"\n",
                "agents.codex.deny_flag",
            ),
        ] {
            std::fs::write(&path, text).unwrap();
            let err = format!("{:#}", PastorConfig::load(&path).unwrap_err());
            assert!(err.contains(want), "{text}: {err}");
        }
    }

    /// An agent definition names a herdr kind and an env; Claude's built-in
    /// trust keys and tool flags follow the kind, not the name.
    #[test]
    fn agent_definitions_carry_a_kind_and_an_env() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.toml");
        std::fs::write(
            &path,
            "[agents.claude-personal]\nkind = \"claude\"\nenv = { CLAUDE_CONFIG_DIR = \"~/.claude-personal\" }\n",
        )
        .unwrap();
        let agents = PastorConfig::load(&path).unwrap().agents;
        assert_eq!(agents.kind("claude-personal"), "claude");
        assert_eq!(agents.kind("codex"), "codex", "no definition: the name");
        assert_eq!(
            agents.trust_keys("claude-personal"),
            Some(vec!["Down".into(), "Enter".into()])
        );
        let launch = agents
            .launch(&spec_with("claude-personal", &["Edit"], &[]))
            .unwrap();
        assert_eq!(launch.kind, "claude");
        assert_eq!(launch.args, vec!["--model", "m", "--allowedTools", "Edit"]);
        assert_eq!(launch.env["CLAUDE_CONFIG_DIR"], "~/.claude-personal");
        assert!(
            agents
                .launch(&spec_with("codex", &[], &[]))
                .unwrap()
                .env
                .is_empty()
        );

        for (text, want) in [
            ("[agents.x]\nkind = \"\"\n", "agents.x.kind"),
            ("[agents.x]\nenv = { \"1A\" = \"v\" }\n", "agents.x.env"),
            (
                "[agents.x]\nenv = { \"A-B\" = \"v\" }\n",
                "not a variable name",
            ),
        ] {
            std::fs::write(&path, text).unwrap();
            let err = format!("{:#}", PastorConfig::load(&path).unwrap_err());
            assert!(err.contains(want), "{text}: {err}");
        }
    }

    #[test]
    fn claude_has_built_in_trust_keys_and_agents_can_set_their_own() {
        let cfg = PastorConfig::default();
        assert_eq!(
            cfg.agents.trust_keys("claude"),
            Some(vec!["Down".to_string(), "Enter".to_string()])
        );
        assert_eq!(cfg.agents.trust_keys("codex"), None);

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.toml");
        std::fs::write(
            &path,
            "[agents.codex]\ntrust_keys = [\"Enter\"]\n[agents.claude]\ntrust_keys = []\n",
        )
        .unwrap();
        let cfg = PastorConfig::load(&path).unwrap();
        assert_eq!(cfg.agents.trust_keys("codex"), Some(vec!["Enter".into()]));
        // An empty list turns the built-in keys off.
        assert_eq!(cfg.agents.trust_keys("claude"), None);

        // A definition that does not mention trust_keys keeps the built-in.
        std::fs::write(&path, "[agents.claude]\n").unwrap();
        let cfg = PastorConfig::load(&path).unwrap();
        assert_eq!(cfg.agents.trust_keys("claude").map(|k| k.len()), Some(2));

        std::fs::write(&path, "[agents.claude]\ntrust_keys = [\"Down\", \"\"]\n").unwrap();
        let err = format!("{:#}", PastorConfig::load(&path).unwrap_err());
        assert!(err.contains("agents.claude.trust_keys"), "{err}");
    }

    /// Saved trust presses the keys only when the pane shows the trust
    /// dialog: Claude's is known by its "Yes, I trust this folder" option,
    /// another agent's by the `trust_marker` it sets.
    #[test]
    fn claude_has_a_built_in_trust_marker_and_agents_can_set_their_own() {
        let cfg = PastorConfig::default();
        assert_eq!(
            cfg.agents.trust_marker("claude").as_deref(),
            Some("Yes, I trust this folder")
        );
        assert_eq!(cfg.agents.trust_marker("codex"), None);
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.toml");
        std::fs::write(
            &path,
            "[agents.codex]\ntrust_keys = [\"Enter\"]\ntrust_marker = \"Trust this directory?\"\n[agents.claude]\ntrust_marker = \"\"\n",
        )
        .unwrap();
        let cfg = PastorConfig::load(&path).unwrap();
        assert_eq!(
            cfg.agents.trust_marker("codex").as_deref(),
            Some("Trust this directory?")
        );
        assert_eq!(
            cfg.agents.trust_marker("claude"),
            None,
            "empty turns it off"
        );
    }

    #[test]
    fn a_trust_marker_matches_across_spacing_and_case() {
        let screen =
            "Quick safety check: ...\n \u{276f} 1. No, exit\n   2.  Yes, I trust\n this folder\n";
        assert!(shows_trust_marker(screen, "Yes, I trust this folder"));
        assert!(shows_trust_marker(
            "YES, I TRUST THIS FOLDER",
            "Yes, I trust this folder"
        ));
        let bypass = "WARNING: Claude Code running in Bypass Permissions mode\n \u{276f} 1. No, exit\n   2. Yes, I accept\n";
        assert!(!shows_trust_marker(bypass, "Yes, I trust this folder"));
    }

    /// The read is scrollback, so a trust prompt already answered can still
    /// be in it. Only the prompt at the bottom counts: the lines after the
    /// last output, from the end of any menu before the last one.
    #[test]
    fn a_trust_marker_counts_only_in_the_prompt_at_the_bottom() {
        let marker = "Yes, I trust this folder";
        let trust = "Quick safety check: Is this a project you trust?\n\n\u{276f} 1. No, exit\n  2. Yes, I trust this folder\n\nEnter to confirm \u{b7} Esc to cancel\n";
        let bypass = "WARNING: Claude Code running in Bypass Permissions mode\n\n\u{276f} 1. No, exit\n  2. Yes, I accept\n\nEnter to confirm \u{b7} Esc to cancel\n";
        assert!(shows_trust_marker(trust, marker));
        assert!(shows_trust_marker(
            &format!("fake output\n\n{trust}"),
            marker
        ));
        assert!(!shows_trust_marker(&format!("{trust}\n{bypass}"), marker));
        assert!(!shows_trust_marker(
            &format!("{trust}\n\u{25cf} Reading the repo\n"),
            marker
        ));
        // A marker above its own menu, such as a question, still counts.
        let codex = "Trust this directory?\n\n> 1. Yes\n  2. No\n";
        assert!(shows_trust_marker(codex, "Trust this directory?"));
        assert!(!shows_trust_marker(
            &format!("{codex}\n{bypass}"),
            "Trust this directory?"
        ));
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
        let args = |given: Option<Vec<String>>| {
            let ask = AgentChoice {
                agent_args: given,
                ..Default::default()
            };
            d.resolve_agent(&ask, None).agent_args
        };
        assert_eq!(args(None), vec!["--model", "claude-opus-5-5"]);
        assert_eq!(args(Some(vec!["-v".into()])), vec!["-v"]);
        assert!(args(Some(vec![])).is_empty());
    }

    /// The task or job wins, then its flock, then `[defaults]`; each key on
    /// its own, and `[]` from a flock is a choice, not a gap.
    #[test]
    fn the_agent_comes_from_the_ask_then_the_flock_then_the_defaults() {
        let d = Defaults {
            agent: "claude".into(),
            agent_args: vec!["--model".into(), "claude-sonnet-5".into()],
            ..Default::default()
        };
        let work = flock::FlockEntry {
            name: "work".into(),
            agent: Some("codex".into()),
            agent_args: Some(vec!["--model".into(), "gpt-x".into()]),
            ..Default::default()
        };
        let bare = flock::FlockEntry {
            name: "home".into(),
            ..Default::default()
        };
        let none = AgentChoice::default();
        let pick = |ask: &AgentChoice, f: Option<&flock::FlockEntry>| {
            let p = d.resolve_agent(ask, f);
            (p.agent, p.agent_args.join(" "))
        };

        assert_eq!(
            pick(&none, None),
            ("claude".into(), "--model claude-sonnet-5".into())
        );
        assert_eq!(pick(&none, Some(&bare)), pick(&none, None));
        assert_eq!(
            pick(&none, Some(&work)),
            ("codex".into(), "--model gpt-x".into())
        );

        let own = AgentChoice {
            agent: Some("aider".into()),
            agent_args: Some(vec!["-v".into()]),
            allow: vec![],
            deny: vec![],
            model: None,
            profile: None,
            ..Default::default()
        };
        assert_eq!(pick(&own, Some(&work)), ("aider".into(), "-v".into()));
        // Args follow the agent they were written for: the flock's are for
        // codex, so a task that asks for claude gets `[defaults]`' instead.
        let claude = AgentChoice {
            agent: Some("claude".into()),
            agent_args: None,
            allow: vec![],
            deny: vec![],
            model: None,
            profile: None,
            ..Default::default()
        };
        assert_eq!(
            pick(&claude, Some(&work)),
            ("claude".into(), "--model claude-sonnet-5".into())
        );
        let codex = AgentChoice {
            agent: Some("codex".into()),
            agent_args: None,
            allow: vec![],
            deny: vec![],
            model: None,
            profile: None,
            ..Default::default()
        };
        assert_eq!(
            pick(&codex, Some(&work)),
            ("codex".into(), "--model gpt-x".into())
        );
        // ...and `[defaults]`' are for claude, so codex outside the flock gets none.
        assert_eq!(pick(&codex, Some(&bare)), ("codex".into(), String::new()));
        // A flock that sets only args lends them to whatever agent runs.
        let args_only = flock::FlockEntry {
            agent_args: Some(vec!["--fast".into()]),
            ..bare.clone()
        };
        assert_eq!(
            pick(&codex, Some(&args_only)),
            ("codex".into(), "--fast".into())
        );

        let no_args = flock::FlockEntry {
            agent_args: Some(vec![]),
            ..bare.clone()
        };
        assert_eq!(
            pick(&none, Some(&no_args)),
            ("claude".into(), String::new())
        );
        // Nothing set anywhere: the built-in.
        assert_eq!(
            Defaults::default().resolve_agent(&none, None).agent,
            "claude"
        );
    }

    /// A machine's agent comes before its flock's and `[defaults]`, and the
    /// ask before all three; each pick says which layer it came from.
    #[test]
    fn a_machines_agent_comes_before_its_flocks_and_the_defaults() {
        let d = Defaults {
            agent_args: vec!["--model".into(), "claude-sonnet-5".into()],
            ..Default::default()
        };
        let flock = flock::FlockEntry {
            name: "personal".into(),
            agent: Some("codex".into()),
            ..Default::default()
        };
        let machine = |extra: &str| -> flock::MachineConfig {
            toml::from_str(&format!("name = \"m\"\nlocal = true\n{extra}")).unwrap()
        };
        let own = machine("agent = \"claude-personal\"\nagent_args = [\"-v\"]");
        let plain = machine("");
        let none = AgentChoice::default();
        let pick = |ask: &AgentChoice, m: Option<&flock::MachineConfig>, f| {
            let p = d.resolve_agent_on(ask, m, f);
            (p.agent, p.agent_args.join(" "), p.agent_from, p.args_from)
        };
        assert_eq!(
            pick(&none, Some(&own), Some(&flock)),
            (
                "claude-personal".into(),
                "-v".into(),
                Layer::Machine,
                Some(Layer::Machine)
            )
        );
        assert_eq!(
            pick(&none, Some(&plain), Some(&flock)),
            ("codex".into(), String::new(), Layer::Flock, None)
        );
        assert_eq!(
            pick(&none, Some(&plain), None),
            (
                "claude".into(),
                "--model claude-sonnet-5".into(),
                Layer::Defaults,
                Some(Layer::Defaults)
            )
        );
        let asked = AgentChoice {
            agent: Some("aider".into()),
            ..Default::default()
        };
        assert_eq!(
            pick(&asked, Some(&own), Some(&flock)),
            ("aider".into(), String::new(), Layer::Ask, None)
        );
        let args = AgentChoice {
            agent_args: Some(vec!["--fast".into()]),
            ..Default::default()
        };
        assert_eq!(
            pick(&args, Some(&own), Some(&flock)),
            (
                "claude-personal".into(),
                "--fast".into(),
                Layer::Machine,
                Some(Layer::Ask)
            )
        );
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
    fn xdg_dirs_take_an_absolute_variable_or_fall_back_to_home() {
        let home = Some(PathBuf::from("/h"));
        assert_eq!(
            xdg_dir(Some("/x/cfg".into()), home.clone(), ".config"),
            Some(PathBuf::from("/x/cfg"))
        );
        // Relative or empty values are ignored, per the XDG spec.
        for bad in ["", "rel/cfg"] {
            assert_eq!(
                xdg_dir(Some(bad.into()), home.clone(), ".config"),
                Some(PathBuf::from("/h/.config"))
            );
        }
        assert_eq!(
            xdg_dir(None, home, ".local/state"),
            Some(PathBuf::from("/h/.local/state"))
        );
        assert_eq!(xdg_dir(None, None, ".config"), None);
    }

    #[test]
    fn default_paths_follow_xdg_not_application_support() {
        // What `from_env` builds with no overrides: on macOS this used to be
        // `~/Library/Application Support/pastor` for config and data.
        let home = dirs::home_dir().unwrap();
        let config = config_home().unwrap();
        assert!(!config.to_string_lossy().contains("Library"));
        if std::env::var_os("XDG_CONFIG_HOME").is_none() {
            assert_eq!(config, home.join(".config"));
        }
        if !cfg!(target_os = "macos") {
            assert_eq!(legacy_macos_dir(), None);
        }
    }

    #[test]
    fn only_a_config_override_skips_the_legacy_move() {
        let home = || Some(PathBuf::from("/Users/u"));
        let legacy = Some(PathBuf::from("/Users/u/Library/Application Support/pastor"));
        assert_eq!(legacy_dir(true, None, None, home()), legacy);
        // A data or state override does not move the config, so it is not
        // passed here and the move still happens.
        assert_eq!(legacy_dir(false, None, None, home()), None);
        assert_eq!(legacy_dir(true, Some("/c".into()), None, home()), None);
        assert_eq!(legacy_dir(true, None, Some("/xdg".into()), home()), None);
        // A relative XDG_CONFIG_HOME is ignored, so the config stays put.
        assert_eq!(legacy_dir(true, None, Some("rel".into()), home()), legacy);
        assert_eq!(legacy_dir(true, None, None, None), None);
    }

    fn legacy_layout(tmp: &Path) -> (PathBuf, Paths) {
        let legacy = tmp.join("Library/Application Support/pastor");
        std::fs::create_dir_all(legacy.join("jobs")).unwrap();
        std::fs::write(legacy.join("flock.toml"), "# flock\n").unwrap();
        std::fs::write(legacy.join("jobs/nightly.toml"), "# job\n").unwrap();
        std::fs::create_dir_all(legacy.join("plugins/github")).unwrap();
        std::fs::write(legacy.join("plugins/github/pastor-connector.toml"), "# m\n").unwrap();
        std::fs::write(legacy.join("plugins/github/.env"), "TOKEN=x\n").unwrap();
        let paths = Paths::new(tmp.join(".config/pastor"), tmp.join(".local/state/pastor"))
            .with_data_dir(tmp.join(".local/share/pastor"));
        (legacy, paths)
    }

    #[test]
    fn a_legacy_macos_config_moves_once_with_a_note() {
        let tmp = tempfile::tempdir().unwrap();
        let (legacy, paths) = legacy_layout(tmp.path());

        let note = migrate_legacy_dir(&legacy, &paths).unwrap().unwrap();
        assert!(note.contains(&legacy.display().to_string()), "{note}");
        assert!(
            note.contains(&paths.config_dir.display().to_string()),
            "{note}"
        );
        assert!(!legacy.exists());
        assert!(paths.flock_file().is_file());
        assert!(paths.jobs_dir().join("nightly.toml").is_file());
        // Checkouts go to the data dir, secrets stay with the config.
        assert!(
            paths
                .connectors_dir()
                .join("github/pastor-connector.toml")
                .is_file()
        );
        assert!(!paths.connectors_dir().join("github/.env").exists());
        assert_eq!(
            std::fs::read_to_string(paths.connector_env_file("github")).unwrap(),
            "TOKEN=x\n"
        );
        assert!(
            !paths
                .config_dir
                .join("plugins/github/pastor-connector.toml")
                .exists()
        );

        // A second run has nothing to move.
        assert_eq!(migrate_legacy_dir(&legacy, &paths).unwrap(), None);
    }

    /// A `connector link` symlink kept its `.env` inside the user's own
    /// checkout, since the legacy config and data dirs were one directory and
    /// `plugins/<id>/.env` resolved through the link. That file is left where
    /// it is, and the note says where pastor now reads it from.
    #[test]
    fn a_linked_connector_keeps_its_env_and_the_note_names_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (legacy, paths) = legacy_layout(tmp.path());
        let checkout = tmp.path().join("src/linked");
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::write(checkout.join(".env"), "K=v\n").unwrap();
        std::os::unix::fs::symlink(&checkout, legacy.join("plugins/linked")).unwrap();

        let note = migrate_legacy_dir(&legacy, &paths).unwrap().unwrap();
        let link = paths.connectors_dir().join("linked");
        assert!(link.symlink_metadata().unwrap().is_symlink());
        assert_eq!(
            std::fs::read_to_string(checkout.join(".env")).unwrap(),
            "K=v\n"
        );
        assert!(!paths.connector_env_file("linked").exists());
        assert!(
            note.contains(
                &std::fs::canonicalize(checkout.join(".env"))
                    .unwrap()
                    .display()
                    .to_string()
            ),
            "{note}"
        );
        assert!(
            note.contains(&paths.connector_env_file("linked").display().to_string()),
            "{note}"
        );
        // The managed one still moves as before.
        assert!(paths.connector_env_file("github").is_file());
    }

    /// A step that fails after the config has moved leaves the migration
    /// marked unfinished, and the next run completes it instead of taking the
    /// moved config as a finished one.
    #[test]
    fn a_failed_legacy_move_finishes_on_the_next_run() {
        let tmp = tempfile::tempdir().unwrap();
        let (legacy, paths) = legacy_layout(tmp.path());
        // The data dir cannot be created while a file sits where its parent
        // should be, so moving the checkouts fails after the config moved.
        let blocker = tmp.path().join(".local/share");
        std::fs::create_dir_all(blocker.parent().unwrap()).unwrap();
        std::fs::write(&blocker, "").unwrap();

        let err = migrate_legacy_dir(&legacy, &paths).unwrap_err();
        assert!(format!("{err:#}").contains("create"), "{err:#}");
        assert!(!legacy.exists());
        assert!(paths.flock_file().is_file());
        assert!(!paths.connectors_dir().exists());

        std::fs::remove_file(&blocker).unwrap();
        let note = migrate_legacy_dir(&legacy, &paths).unwrap().unwrap();
        assert!(note.contains("connectors"), "{note}");
        assert!(
            paths
                .connectors_dir()
                .join("github/pastor-connector.toml")
                .is_file()
        );
        assert!(!paths.connectors_dir().join("github/.env").exists());
        assert_eq!(
            std::fs::read_to_string(paths.connector_env_file("github")).unwrap(),
            "TOKEN=x\n"
        );
        assert!(
            !paths
                .config_dir
                .join("plugins/github/pastor-connector.toml")
                .exists()
        );
        assert!(!paths.config_dir.join(MIGRATING).exists());

        // Finished now: a third run has nothing to do.
        assert_eq!(migrate_legacy_dir(&legacy, &paths).unwrap(), None);
    }

    /// Checkouts already in the data dir are not the legacy ones, so the move
    /// leaves them, and the legacy `plugins/` stays with the config.
    #[test]
    fn existing_data_connectors_are_not_touched_by_the_move() {
        let tmp = tempfile::tempdir().unwrap();
        let (legacy, paths) = legacy_layout(tmp.path());
        std::fs::create_dir_all(paths.connectors_dir().join("other")).unwrap();
        std::fs::write(paths.connectors_dir().join("other/.env"), "K=v\n").unwrap();

        migrate_legacy_dir(&legacy, &paths).unwrap().unwrap();
        assert!(paths.connectors_dir().join("other/.env").is_file());
        assert!(
            paths
                .config_dir
                .join("plugins/github/pastor-connector.toml")
                .is_file()
        );
        assert!(!paths.config_dir.join(MIGRATING).exists());
    }

    #[test]
    fn a_legacy_config_is_left_alone_when_the_new_one_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let (legacy, paths) = legacy_layout(tmp.path());
        std::fs::create_dir_all(&paths.config_dir).unwrap();

        assert_eq!(migrate_legacy_dir(&legacy, &paths).unwrap(), None);
        assert!(legacy.join("flock.toml").is_file());
        assert!(!paths.flock_file().exists());
    }

    #[test]
    fn no_legacy_dir_means_no_move() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        assert_eq!(
            migrate_legacy_dir(&tmp.path().join("missing"), &paths).unwrap(),
            None
        );
        assert!(!paths.config_dir.exists());
    }

    #[test]
    fn connector_paths() {
        let p = Paths::new("/tmp/c", "/tmp/s");
        assert_eq!(p.data_dir(), PathBuf::from("/tmp/s/data"));
        assert_eq!(p.connectors_dir(), PathBuf::from("/tmp/s/data/connectors"));
        let p = p.with_data_dir("/tmp/d");
        assert_eq!(p.connectors_dir(), PathBuf::from("/tmp/d/connectors"));
        assert_eq!(
            p.connector_env_file("slack"),
            PathBuf::from("/tmp/c/connectors/slack/.env")
        );
        assert_eq!(
            p.connector_state_dir("support"),
            PathBuf::from("/tmp/s/connectors/support")
        );
        assert_eq!(p.runs_dir("support"), PathBuf::from("/tmp/s/runs/support"));
    }

    #[test]
    fn close_done_after_defaults_to_five_seconds_and_never_disables() {
        let d = PastorConfig::default();
        assert_eq!(d.close_done_after, "5s");
        assert_eq!(d.close_done_after_duration(), Some(Duration::from_secs(5)));
        let c = PastorConfig {
            close_done_after: "never".into(),
            ..Default::default()
        };
        assert_eq!(c.close_done_after_duration(), None);
    }

    /// `[shepherd]` names the pull machine this one is, the hostname when
    /// it does not; an empty name or command and an unknown key are refused.
    #[test]
    fn shepherd_names_the_pull_machine_this_one_is() {
        let path = Path::new("pastor.toml");
        let d = PastorConfig::default();
        assert!(d.shepherd.is_empty());
        assert_eq!(d.shepherd.machine_name(), hostname());
        let cfg = PastorConfig::parse(
            path,
            "[shepherd]\nmachine = \"laptop\"\ntakes_flock_work = true\n",
        )
        .unwrap();
        assert_eq!(cfg.shepherd.machine_name(), "laptop");
        assert!(cfg.shepherd.flock_work());
        for (bad, key) in [
            ("[shepherd]\nmachine = \" \"\n", "shepherd.machine"),
            ("[shepherd]\ncommand = []\n", "shepherd.command"),
            ("[shepherd]\nmachines = \"x\"\n", "machines"),
        ] {
            let err = format!("{:#}", PastorConfig::parse(path, bad).unwrap_err());
            assert!(err.contains(key), "{bad}: {err}");
        }
    }

    /// A pull machine is lost after ten silent minutes unless pastor.toml
    /// says otherwise; zero is refused like any other timing.
    #[test]
    fn pull_lost_after_defaults_to_ten_minutes() {
        assert_eq!(
            PastorConfig::default().pull_lost_after_duration(),
            Duration::from_secs(600)
        );
        let path = Path::new("pastor.toml");
        let cfg = PastorConfig::parse(path, "pull_lost_after = \"90s\"").unwrap();
        assert_eq!(cfg.pull_lost_after_duration(), Duration::from_secs(90));
        let err = PastorConfig::parse(path, "pull_lost_after = \"0s\"")
            .unwrap_err()
            .to_string();
        assert!(err.contains("pull_lost_after"), "{err}");
    }

    #[test]
    fn close_done_after_parses_never_and_rejects_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.toml");
        std::fs::write(&path, "close_done_after = \"never\"\n").unwrap();
        let cfg = PastorConfig::load(&path).unwrap();
        assert_eq!(cfg.close_done_after_duration(), None);
        std::fs::write(&path, "close_done_after = \"2h\"\n").unwrap();
        let cfg = PastorConfig::load(&path).unwrap();
        assert_eq!(
            cfg.close_done_after_duration(),
            Some(Duration::from_secs(7200))
        );
        std::fs::write(&path, "close_done_after = \"0s\"\n").unwrap();
        let err = PastorConfig::load(&path).unwrap_err().to_string();
        assert!(err.contains("close_done_after"), "{err}");
        assert!(err.contains("must not be zero"), "{err}");
        std::fs::write(&path, "close_done_after = \"later\"\n").unwrap();
        let err = PastorConfig::load(&path).unwrap_err().to_string();
        assert!(err.contains("close_done_after"), "{err}");
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
