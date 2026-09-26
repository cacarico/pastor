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

    /// One directory per connector: a managed checkout, or a symlink made by
    /// `connector link`.
    pub fn connectors_dir(&self) -> PathBuf {
        self.data_dir.join("connectors")
    }

    /// Secrets and settings for one connector, written by the user.
    pub fn connector_env_file(&self, id: &str) -> PathBuf {
        self.config_dir.join("connectors").join(id).join(".env")
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
/// `None` off macOS, and when an override says where the files are, because
/// then nothing was ever read from the old place.
pub fn legacy_macos_dir() -> Option<PathBuf> {
    if !cfg!(target_os = "macos")
        || std::env::var_os("PASTOR_CONFIG_DIR").is_some()
        || std::env::var_os("PASTOR_DATA_DIR").is_some()
    {
        return None;
    }
    dirs::home_dir().map(|h| h.join("Library/Application Support/pastor"))
}

/// Move a config left at `legacy` into `paths`, once. The old macOS layout had
/// config and data in one directory, and still used the old name for
/// connectors, so `plugins/` held both the checkouts and each connector's
/// `.env`: the checkouts go to `connectors/` in the data dir and the `.env`
/// files to `<config>/connectors/<id>/`. Nothing happens when there is no
/// legacy directory or the new config dir already exists (never merge two
/// configs). Returns the note to show the user when something moved.
pub fn migrate_legacy_dir(legacy: &Path, paths: &Paths) -> anyhow::Result<Option<String>> {
    let is_dir = std::fs::symlink_metadata(legacy).is_ok_and(|m| m.is_dir());
    if !is_dir || std::fs::symlink_metadata(&paths.config_dir).is_ok() {
        return Ok(None);
    }
    if let Some(parent) = paths.config_dir.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::rename(legacy, &paths.config_dir).with_context(|| {
        format!(
            "move {} to {}",
            legacy.display(),
            paths.config_dir.display()
        )
    })?;
    let mut note = format!(
        "note: moved the pastor config from {} to {}",
        legacy.display(),
        paths.config_dir.display()
    );
    let old_plugins = paths.config_dir.join("plugins");
    let connectors = paths.connectors_dir();
    let is_real_dir = |p: &Path| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir());
    if is_real_dir(&old_plugins) && std::fs::symlink_metadata(&connectors).is_err() {
        create_private_dir(&paths.data_dir)?;
        std::fs::rename(&old_plugins, &connectors).with_context(|| {
            format!(
                "move {} to {}",
                old_plugins.display(),
                connectors.display()
            )
        })?;
        let mut ids: Vec<_> = std::fs::read_dir(&connectors)
            .with_context(|| format!("read {}", connectors.display()))?
            .filter_map(Result::ok)
            // A `connector link` symlink points into the user's own checkout,
            // whose `.env` is theirs to move.
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| e.file_name())
            .collect();
        ids.sort();
        for id in ids {
            let from = connectors.join(&id).join(".env");
            if !std::fs::symlink_metadata(&from).is_ok_and(|m| m.is_file()) {
                continue;
            }
            let to = paths.connector_env_file(&id.to_string_lossy());
            if let Some(dir) = to.parent() {
                create_private_dir(dir)?;
            }
            std::fs::rename(&from, &to)
                .with_context(|| format!("move {} to {}", from.display(), to.display()))?;
        }
        note.push_str(&format!(", and its connectors to {}", connectors.display()));
    }
    Ok(Some(note))
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
    /// whose run flags, job file and flock give none. See `resolve_agent`.
    pub agent_args: Vec<String>,
    /// Tool patterns every task's agent may use without asking
    /// (`"Bash(git:*)"`); a flock and a job add to them. See `resolve_agent`.
    pub allow: Vec<String>,
    /// Tool patterns every task's agent must never use; wins over `allow`.
    pub deny: Vec<String>,
    pub max_tasks_per_run: u32,
    pub timeout: String,
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
    /// one flock can run different agents. Tool lists stay per flock.
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
        AgentPick {
            agent,
            agent_args,
            allow,
            deny,
            agent_from,
            args_from,
        }
    }
}

impl Default for Defaults {
    fn default() -> Self {
        Defaults {
            agent: "claude".into(),
            agent_args: vec![],
            allow: vec![],
            deny: vec![],
            max_tasks_per_run: 5,
            timeout: "2h".into(),
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

/// Claude's folder-trust dialog opens on "No, exit"; Down moves to "Yes,
/// proceed" and Enter takes it.
const CLAUDE_TRUST_KEYS: [&str; 2] = ["Down", "Enter"];

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
    pub fn launch(&self, spec: &crate::task::DispatchSpec) -> Result<Launch, String> {
        Ok(Launch {
            kind: self.kind(&spec.agent).to_string(),
            args: self.launch_args(spec)?,
            env: self
                .0
                .get(&spec.agent)
                .map(|d| d.env.clone())
                .unwrap_or_default(),
        })
    }

    /// The argv after the agent's name for `spec`: its `agent_args`, then
    /// the flag and pattern of each `allow`, then of each `deny`. Refused
    /// when a list is not empty and the agent has no flag for it: dropping
    /// a deny list without a word would be worse than not starting.
    pub fn launch_args(&self, spec: &crate::task::DispatchSpec) -> Result<Vec<String>, String> {
        let (allow_flag, deny_flag) = self.tool_flags(&spec.agent);
        let mut args = spec.agent_args.clone();
        for (list, flag, key) in [
            (&spec.allow, allow_flag, "allow_flag"),
            (&spec.deny, deny_flag, "deny_flag"),
        ] {
            if list.is_empty() {
                continue;
            }
            let Some(flag) = flag else {
                return Err(format!(
                    "agent {} has no {key} to pass its tool list; set [agents.{}] {key} in pastor.toml",
                    spec.agent, spec.agent
                ));
            };
            for p in list {
                args.push(flag.clone());
                args.push(p.clone());
            }
        }
        Ok(args)
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
    /// Whether an agent pastor started (`ipc::TASK_ENV` in its pane) may
    /// change the fleet: run, send to, retry, close or prune tasks, run jobs,
    /// and edit machines, flocks and jobs. Off by default, so the head
    /// refuses it. A guard against an agent acting on its own; the agent runs
    /// as the same user, so it is not a security boundary.
    pub agents_change_fleet: bool,
    pub defaults: Defaults,
    #[serde(skip_serializing_if = "is_empty_agents")]
    pub agents: Agents,
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
            close_done_after: "15m".into(),
            agents_change_fleet: false,
            defaults: Defaults::default(),
            agents: Agents::default(),
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
        assert!(err.contains("[agents.codex] deny_flag"), "{err}");

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
        };
        assert_eq!(pick(&own, Some(&work)), ("aider".into(), "-v".into()));
        // Args follow the agent they were written for: the flock's are for
        // codex, so a task that asks for claude gets `[defaults]`' instead.
        let claude = AgentChoice {
            agent: Some("claude".into()),
            agent_args: None,
            allow: vec![],
            deny: vec![],
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
    fn close_done_after_defaults_to_fifteen_minutes_and_never_disables() {
        let d = PastorConfig::default();
        assert_eq!(d.close_done_after, "15m");
        assert_eq!(
            d.close_done_after_duration(),
            Some(Duration::from_secs(15 * 60))
        );
        let c = PastorConfig {
            close_done_after: "never".into(),
            ..Default::default()
        };
        assert_eq!(c.close_done_after_duration(), None);
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
