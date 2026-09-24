# pastor core implementation plan (plan 1 of 4)

> **Executed on 2026-09-23; kept as history.** The code on `feat/core` is
> the result, after review fixes that changed some of what is written here:
> herdr answers one request per connection, so there is no persistent
> request connection; dispatch waits for agent readiness before prompting;
> `agent_not_ready` never marks a task blocked; `pastor list` shows failed
> tasks. Do not copy code from this file. `AGENTS.md` says where to start.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A `pastor serve` daemon that dispatches one-off tasks (`pastor run`) to herdr agents on flock machines over SSH, tracks their state from herdr events, and shows them with `pastor list` and `pastor attach`.

**Architecture:** One Rust binary. The daemon holds a tokio task per machine that owns two connections to that machine's herdr server (one for requests, one for the event subscription), reconnects with backoff, and reconciles against `agent.list`. Tasks live in SQLite. The CLI talks to the daemon over a unix socket with newline-delimited JSON and falls back to reading the database when the daemon is down. A fake herdr speaking the real wire protocol backs the tests.

**Tech Stack:** Rust 2021, tokio, clap (derive), serde/serde_json, toml, rusqlite (bundled), chrono, tracing, dirs, anyhow, thiserror. Dev: tempfile.

**Spec:** `docs/superpowers/specs/2026-09-23-pastor-design.md`

**Later plans (not here):** plan 2 jobs and schedules (`jobs/*.toml`, tick, seen-store, clock connector, reload); plan 3 plugins (connector protocol, event hooks, install/link, events.jsonl, Slack and ntfy plugins); plan 4 systemd setup, `task retry|close|prune`, docs.

## Global Constraints

- herdr protocol: pastor requires `protocol >= 22` from `ping` (herdr 0.9.0 and 0.9.1 ship 22). Constant `MIN_HERDR_PROTOCOL: u32 = 22`.
- herdr agent names must match `[a-z][a-z0-9_-]{0,31}`. Task agent names are `t-<id>`.
- Remote bridge command, exactly: `herdr --session <session> remote-api-bridge`. Local socket: `~/.config/herdr/herdr.sock` for session `default`, else `~/.config/herdr/sessions/<name>/herdr.sock`.
- Wire format: one JSON object per line. Request `{"id":"...","method":"...","params":{...}}`. Success `{"id":"...","result":{"type":"...", ...}}`. Error `{"id":"...","error":{"code":"...","message":"..."}}`. Lifecycle event `{"event":"pane_closed","data":{"type":"pane_closed","pane_id":"w1:p1","workspace_id":"w1"}}`. Subscription event `{"event":"pane.agent_status_changed","data":{"pane_id":"w1:p1","workspace_id":"w1","agent_status":"blocked"}}`.
- A connection that sends `events.subscribe` receives `{"result":{"type":"subscription_started"}}` and then only events until it closes. Requests go on a different connection.
- `pane.agent_status_changed` subscriptions require `pane_id`. `pane.closed` and `pane.exited` take no filter.
- `agent.read` takes `source` as `recent_unwrapped` (underscore on the wire; the CLI spells it with a hyphen) and returns the text at `result.read.text`.
- Config in `~/.config/pastor/`, state in `~/.local/state/pastor/`, both created 0700. Env `PASTOR_CONFIG_DIR` and `PASTOR_STATE_DIR` override (tests use them).
- pastor never closes panes, kills agents or removes worktrees on its own.
- Every runtime CLI error is JSON on stderr `{"code":"...","message":"..."}` with exit 1. Usage errors (clap parsing, missing subcommand, `--help`) keep clap's plain text with exit 2, exactly as herdr does.
- Machine rules: no `sudo`, no package installs. Work on a branch, open a PR, never push `main`. Git hooks add an `Assisted-by:` trailer; do not add co-author lines.
- Commit messages: conventional prefix (`feat:`, `test:`, `chore:`, `docs:`), imperative, short.

## Review Focus

1. A prompt with newlines, quotes and `{{` braces must reach `agent.prompt` verbatim (Task 9 test `dispatch_sends_prompt_verbatim`).
2. A machine at `max_agents` must leave the task `queued` and dispatch it when a task on that machine closes (Task 9 `pick_machine_respects_capacity`, Task 11 `queued_task_dispatches_when_capacity_frees`).
3. A malformed line on the event stream must be skipped, not kill the machine loop (Task 4 `event_stream_skips_bad_lines`).
4. `ssh` failing immediately (auth, host down) must put the machine in `reconnecting` with the stderr text in its status, never panic (Task 6 `process_transport_reports_exit_and_stderr`, Task 10 `machine_reports_reconnecting_with_error`).
5. After a daemon restart, tasks left in `starting` or `running` must be reconciled from `agent.list`: agent present means keep, absent means `failed` (Task 10 `reconcile_marks_missing_agents_failed`).

---

## File structure

```
Cargo.toml
src/lib.rs                 module list, MIN_HERDR_PROTOCOL
src/main.rs                clap CLI, subcommand handlers
src/bin/fake-herdr.rs      fake herdr over stdin/stdout for process-transport tests and manual runs
src/config/mod.rs          Paths (config/state dirs), PastorConfig (pastor.toml)
src/config/flock.rs        MachineConfig, Flock: load, save, validate
src/herdr/mod.rs           HerdrError, re-exports
src/herdr/protocol.rs      serde types for requests, responses, events, AgentInfo, results
src/herdr/client.rs        Connection: call(), typed helpers, subscribe() -> EventStream
src/herdr/transport.rs     Endpoint (Local | Ssh | Command) -> Connection
src/herdr/fake.rs          FakeHerdr in-process server for tests
src/store.rs               SQLite: migrations, task CRUD
src/task.rs                Task, TaskState, DispatchSpec, state transition function
src/dispatch.rs            pick_machine, dispatch steps
src/machine.rs             per-machine actor: connect, subscribe, reconcile, dispatch commands, status
src/ipc.rs                 CLI<->daemon JSON messages
src/daemon.rs              serve(): store, machine actors, queued-task tick, CLI socket
tests/                     integration tests that need the built binaries
```

---

### Task 1: Crate skeleton, paths, CLI stub

**Files:**
- Create: `Cargo.toml`, `src/lib.rs`, `src/main.rs`, `src/config/mod.rs`, `.gitignore`
- Test: `src/config/mod.rs` (unit tests)

**Interfaces:**
- Produces: `pastor::config::Paths { config_dir: PathBuf, state_dir: PathBuf }`, `Paths::from_env() -> anyhow::Result<Paths>`, `Paths::ensure(&self) -> anyhow::Result<()>`, `Paths::flock_file()`, `Paths::db_file()`, `Paths::socket_file()`. `pastor::MIN_HERDR_PROTOCOL: u32`.

- [ ] **Step 1: Create the crate and add dependencies**

```bash
cd /home/cacarico/ghq/github.com/cacarico/pastor
git checkout -b feat/core
cargo init --name pastor
cargo add tokio --features full
cargo add clap --features derive
cargo add serde --features derive
cargo add serde_json toml anyhow thiserror tracing dirs
cargo add tracing-subscriber --features env-filter
cargo add rusqlite --features bundled
cargo add chrono --features serde
cargo add --dev tempfile
printf '/target\n' > .gitignore
```

- [ ] **Step 2: Write the failing test for Paths**

`src/config/mod.rs`:

```rust
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
        todo!()
    }

    pub fn new(config_dir: impl Into<PathBuf>, state_dir: impl Into<PathBuf>) -> Paths {
        Paths { config_dir: config_dir.into(), state_dir: state_dir.into() }
    }

    /// Create both directories with mode 0700. Idempotent.
    pub fn ensure(&self) -> anyhow::Result<()> {
        todo!()
    }

    pub fn flock_file(&self) -> PathBuf { self.config_dir.join("flock.toml") }
    pub fn db_file(&self) -> PathBuf { self.state_dir.join("pastor.db") }
    pub fn socket_file(&self) -> PathBuf { self.state_dir.join("pastor.sock") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn from_env_uses_overrides() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("PASTOR_CONFIG_DIR", tmp.path().join("c"));
        std::env::set_var("PASTOR_STATE_DIR", tmp.path().join("s"));
        let p = Paths::from_env().unwrap();
        assert_eq!(p.config_dir, tmp.path().join("c"));
        assert_eq!(p.state_dir, tmp.path().join("s"));
        std::env::remove_var("PASTOR_CONFIG_DIR");
        std::env::remove_var("PASTOR_STATE_DIR");
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
```

`src/lib.rs`:

```rust
pub mod config;

/// Lowest herdr socket protocol pastor speaks. herdr 0.9.0 and 0.9.1 ship 22.
pub const MIN_HERDR_PROTOCOL: u32 = 22;
```

- [ ] **Step 3: Run the tests to see them fail**

Run: `cargo test config::`
Expected: both tests panic with `not yet implemented`.

- [ ] **Step 4: Implement Paths**

Replace the two `todo!()` bodies in `src/config/mod.rs`:

```rust
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
        Ok(Paths { config_dir, state_dir })
    }

    pub fn ensure(&self) -> anyhow::Result<()> {
        for dir in [&self.config_dir, &self.state_dir] {
            create_private_dir(dir)?;
        }
        Ok(())
    }
```

and add below the impl:

```rust
pub fn create_private_dir(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod {}", dir.display()))?;
    Ok(())
}
```

- [ ] **Step 5: Write the CLI stub**

`src/main.rs`:

```rust
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "pastor", version, about = "run coding agents on machines you own")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon
    Serve,
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Serve => Err(anyhow::anyhow!("not implemented")),
    };
    if let Err(err) = result {
        fail("runtime_error", &format!("{err:#}"));
    }
}

/// Print a JSON error to stderr and exit 1, matching herdr's CLI convention.
fn fail(code: &str, message: &str) -> ! {
    eprintln!("{}", serde_json::json!({"code": code, "message": message}));
    std::process::exit(1)
}
```

- [ ] **Step 6: Run tests and the binary**

Run: `cargo test && cargo run -- --version`
Expected: tests pass, prints `pastor 0.1.0`.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock .gitignore src
git commit -m "chore: crate skeleton with config paths"
```

---

### Task 2: Flock config

**Files:**
- Create: `src/config/flock.rs`
- Modify: `src/config/mod.rs` (add `pub mod flock;`)

**Interfaces:**
- Produces:
  ```rust
  pub struct MachineConfig { pub name: String, pub local: bool, pub ssh: Option<String>, pub command: Option<Vec<String>>, pub session: String, pub max_agents: u32, pub tags: Vec<String> }
  pub struct Flock { pub machines: Vec<MachineConfig> }
  impl Flock { pub fn load(path: &Path) -> anyhow::Result<Flock>; pub fn save(&self, path: &Path) -> anyhow::Result<()>; pub fn validate(&self) -> Result<(), String>; pub fn get(&self, name: &str) -> Option<&MachineConfig>; pub fn add(&mut self, m: MachineConfig) -> Result<(), String>; pub fn remove(&mut self, name: &str) -> bool }
  ```
  `command` is a developer option: an argv that speaks the herdr protocol on stdio (used with `fake-herdr`). Exactly one of `local`, `ssh`, `command` must be set.

- [ ] **Step 1: Write the failing tests**

`src/config/flock.rs`:

```rust
use std::path::Path;

use anyhow::Context;
use serde::{Deserialize, Serialize};

fn default_session() -> String { "default".to_string() }
fn default_max_agents() -> u32 { 2 }

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
    pub fn load(path: &Path) -> anyhow::Result<Flock> { todo!() }
    pub fn save(&self, path: &Path) -> anyhow::Result<()> { todo!() }
    pub fn validate(&self) -> Result<(), String> { todo!() }
    pub fn get(&self, name: &str) -> Option<&MachineConfig> {
        self.machines.iter().find(|m| m.name == name)
    }
    pub fn add(&mut self, m: MachineConfig) -> Result<(), String> { todo!() }
    pub fn remove(&mut self, name: &str) -> bool { todo!() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pi(name: &str) -> MachineConfig {
        MachineConfig { name: name.into(), local: false, ssh: Some(format!("fleet@{name}")), command: None, session: "default".into(), max_agents: 2, tags: vec![] }
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
        f.machines.push(MachineConfig { local: true, ..pi("both") });
        assert!(f.validate().unwrap_err().contains("both"));
        let mut f = Flock::default();
        f.machines.push(MachineConfig { ssh: None, ..pi("none") });
        assert!(f.validate().unwrap_err().contains("none"));
        let mut f = Flock::default();
        f.machines.push(pi("dup"));
        f.machines.push(pi("dup"));
        assert!(f.validate().unwrap_err().contains("dup"));
        let mut f = Flock::default();
        f.machines.push(MachineConfig { max_agents: 0, ..pi("zero") });
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
}
```

Add `pub mod flock;` at the top of `src/config/mod.rs`.

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test config::flock`
Expected: `parses_spec_example` fails in `validate`, the others panic with `not yet implemented`.

- [ ] **Step 3: Implement**

Replace the four `todo!()` bodies:

```rust
    pub fn load(path: &Path) -> anyhow::Result<Flock> {
        if !path.exists() {
            return Ok(Flock::default());
        }
        let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let flock: Flock = toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        flock.validate().map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
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
                return Err(format!("machine {}: set exactly one of local, ssh, command", m.name));
            }
            if m.max_agents == 0 {
                return Err(format!("machine {}: max_agents must be at least 1", m.name));
            }
        }
        Ok(())
    }

    pub fn add(&mut self, m: MachineConfig) -> Result<(), String> {
        if self.get(&m.name).is_some() {
            return Err(format!("machine {} already exists", m.name));
        }
        self.machines.push(m);
        self.validate()
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.machines.len();
        self.machines.retain(|m| m.name != name);
        self.machines.len() != before
    }
```

- [ ] **Step 4: Run tests**

Run: `cargo test config::flock`
Expected: 5 passed.

- [ ] **Step 5: Commit**

```bash
git add src/config
git commit -m "feat: flock config file"
```

---

### Task 3: herdr protocol types

**Files:**
- Create: `src/herdr/mod.rs`, `src/herdr/protocol.rs`
- Modify: `src/lib.rs` (add `pub mod herdr;`)

**Interfaces:**
- Produces (all `serde` types, all `Debug + Clone`):
  ```rust
  pub enum HerdrError { Api { code: String, message: String }, Io(std::io::Error), Protocol(String), Closed }
  impl HerdrError { pub fn code(&self) -> Option<&str> }
  pub struct Request { pub id: String, pub method: String, pub params: Value }
  pub enum Response { Success { id: String, result: Value }, Error { id: String, error: ErrorBody } }
  pub struct ErrorBody { pub code: String, pub message: String }
  pub struct Event { pub event: String, pub data: Value }
  impl Event { pub fn pane_id(&self) -> Option<&str>; pub fn agent_status(&self) -> Option<AgentStatus>; pub fn is_pane_closed(&self) -> bool; pub fn is_pane_exited(&self) -> bool }
  pub enum Incoming { Response(Response), Event(Event) }
  pub enum AgentStatus { Idle, Working, Blocked, Done, Unknown }  // serde lowercase
  pub struct AgentInfo { pub pane_id: String, pub workspace_id: String, pub tab_id: String, pub name: Option<String>, pub agent: Option<String>, pub agent_status: AgentStatus, pub completion_seq: Option<u64>, pub state_change_seq: u64, pub interactive_ready: bool }
  pub struct PaneRef { pub pane_id: String, pub workspace_id: String }
  pub struct WorkspaceRef { pub workspace_id: String, pub label: Option<String> }
  pub struct Pong { pub version: String, pub protocol: u32 }
  pub struct Created { pub workspace: WorkspaceRef, pub root_pane: PaneRef }   // workspace_created and worktree_created
  pub struct AgentResult { pub agent: AgentInfo }                              // agent_started, agent_prompted, agent_info
  pub struct AgentList { pub agents: Vec<AgentInfo> }
  pub struct PaneRead { pub read: ReadBody } pub struct ReadBody { pub text: String }
  pub fn subscription_agent_status(pane_id: &str) -> Value; pub fn subscription_lifecycle(kind: &str) -> Value
  ```

- [ ] **Step 1: Write the failing tests**

`src/herdr/mod.rs`:

```rust
pub mod protocol;

pub use protocol::*;

#[derive(Debug, thiserror::Error)]
pub enum HerdrError {
    #[error("herdr error {code}: {message}")]
    Api { code: String, message: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("connection closed")]
    Closed,
}

impl HerdrError {
    pub fn code(&self) -> Option<&str> {
        match self {
            HerdrError::Api { code, .. } => Some(code),
            _ => None,
        }
    }
}
```

`src/herdr/protocol.rs`:

```rust
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Response {
    Success { id: String, result: Value },
    Error { id: String, error: ErrorBody },
}

impl Response {
    pub fn id(&self) -> &str {
        match self {
            Response::Success { id, .. } | Response::Error { id, .. } => id,
        }
    }
}

/// Lifecycle events (`pane_closed`) and subscription events (`pane.agent_status_changed`)
/// share this envelope; only the `event` spelling differs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub event: String,
    pub data: Value,
}

impl Event {
    pub fn pane_id(&self) -> Option<&str> { self.data.get("pane_id").and_then(Value::as_str) }
    pub fn agent_status(&self) -> Option<AgentStatus> {
        serde_json::from_value(self.data.get("agent_status")?.clone()).ok()
    }
    pub fn is_pane_closed(&self) -> bool { self.event == "pane_closed" || self.event == "pane.closed" }
    pub fn is_pane_exited(&self) -> bool { self.event == "pane_exited" || self.event == "pane.exited" }
    pub fn is_agent_status(&self) -> bool {
        self.event == "pane.agent_status_changed" || self.event == "pane_agent_status_changed"
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Incoming {
    Response(Response),
    Event(Event),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus { Idle, Working, Blocked, Done, Unknown }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentInfo {
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    pub agent_status: AgentStatus,
    #[serde(default)]
    pub completion_seq: Option<u64>,
    #[serde(default)]
    pub state_change_seq: u64,
    #[serde(default)]
    pub interactive_ready: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneRef { pub pane_id: String, pub workspace_id: String }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceRef { pub workspace_id: String, #[serde(default)] pub label: Option<String> }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pong { pub version: String, pub protocol: u32 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Created { pub workspace: WorkspaceRef, pub root_pane: PaneRef }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResult { pub agent: AgentInfo }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentList { pub agents: Vec<AgentInfo> }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReadBody { pub text: String }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneRead { pub read: ReadBody }

pub fn subscription_agent_status(pane_id: &str) -> Value {
    serde_json::json!({"type": "pane.agent_status_changed", "pane_id": pane_id})
}

pub fn subscription_lifecycle(kind: &str) -> Value {
    serde_json::json!({"type": kind})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_success_and_error_responses() {
        let ok: Incoming = serde_json::from_str(r#"{"id":"r1","result":{"type":"pong","version":"0.9.1","protocol":22}}"#).unwrap();
        match ok {
            Incoming::Response(Response::Success { id, result }) => {
                assert_eq!(id, "r1");
                let pong: Pong = serde_json::from_value(result).unwrap();
                assert_eq!(pong.protocol, 22);
            }
            other => panic!("{other:?}"),
        }
        let err: Incoming = serde_json::from_str(r#"{"id":"r2","error":{"code":"agent_blocked","message":"blocked"}}"#).unwrap();
        match err {
            Incoming::Response(Response::Error { error, .. }) => assert_eq!(error.code, "agent_blocked"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parses_lifecycle_and_subscription_events() {
        let closed: Incoming = serde_json::from_str(r#"{"event":"pane_closed","data":{"type":"pane_closed","pane_id":"w1:p1","workspace_id":"w1"}}"#).unwrap();
        let Incoming::Event(e) = closed else { panic!() };
        assert!(e.is_pane_closed());
        assert_eq!(e.pane_id(), Some("w1:p1"));

        let status: Incoming = serde_json::from_str(r#"{"event":"pane.agent_status_changed","data":{"pane_id":"w1:p2","workspace_id":"w1","agent_status":"blocked","agent":"claude"}}"#).unwrap();
        let Incoming::Event(e) = status else { panic!() };
        assert!(e.is_agent_status());
        assert_eq!(e.agent_status(), Some(AgentStatus::Blocked));
    }

    #[test]
    fn parses_agent_list_with_missing_optionals() {
        let v = serde_json::json!({"type":"agent_list","agents":[{"terminal_id":"t","agent_status":"idle","workspace_id":"w1","tab_id":"w1:t1","pane_id":"w1:p1","focused":false,"revision":3}]});
        let list: AgentList = serde_json::from_value(v).unwrap();
        assert_eq!(list.agents[0].completion_seq, None);
        assert_eq!(list.agents[0].state_change_seq, 0);
        assert_eq!(list.agents[0].agent_status, AgentStatus::Idle);
    }

    #[test]
    fn request_serialises_with_method_and_params() {
        let r = Request { id: "x".into(), method: "agent.list".into(), params: serde_json::json!({}) };
        assert_eq!(serde_json::to_string(&r).unwrap(), r#"{"id":"x","method":"agent.list","params":{}}"#);
    }
}
```

Add `pub mod herdr;` to `src/lib.rs`.

- [ ] **Step 2: Run tests**

Run: `cargo test herdr::protocol`
Expected: 4 passed (the types are complete; this task's test-first step doubles as the implementation because the types are pure data).

- [ ] **Step 3: Commit**

```bash
git add src/herdr src/lib.rs
git commit -m "feat: herdr wire protocol types"
```

---

### Task 4: herdr client connection

**Files:**
- Create: `src/herdr/client.rs`
- Modify: `src/herdr/mod.rs` (add `pub mod client; pub use client::*;`)

**Interfaces:**
- Produces:
  ```rust
  pub type BoxRead = Box<dyn tokio::io::AsyncRead + Unpin + Send>;
  pub type BoxWrite = Box<dyn tokio::io::AsyncWrite + Unpin + Send>;
  pub struct Connection { .. }
  impl Connection {
      pub fn new(reader: BoxRead, writer: BoxWrite) -> Connection
      pub fn with_child(self, child: tokio::process::Child) -> Connection   // keeps a bridge process alive
      pub async fn call(&mut self, method: &str, params: Value) -> Result<Value, HerdrError>
      pub async fn call_as<T: DeserializeOwned>(&mut self, method: &str, params: Value) -> Result<T, HerdrError>
      pub async fn ping(&mut self) -> Result<Pong, HerdrError>
      pub async fn agent_list(&mut self) -> Result<Vec<AgentInfo>, HerdrError>
      pub async fn workspace_create(&mut self, cwd: Option<&str>, label: &str) -> Result<Created, HerdrError>
      pub async fn worktree_create(&mut self, cwd: &str, branch: &str, label: &str) -> Result<Created, HerdrError>
      pub async fn agent_start(&mut self, name: &str, kind: &str, pane_id: &str, args: &[String]) -> Result<AgentInfo, HerdrError>
      pub async fn agent_prompt(&mut self, target: &str, text: &str) -> Result<AgentInfo, HerdrError>
      pub async fn agent_read(&mut self, target: &str, lines: u32) -> Result<String, HerdrError>
      pub async fn subscribe(self, subscriptions: Vec<Value>) -> Result<EventStream, HerdrError>
  }
  pub struct EventStream { .. }
  impl EventStream { pub async fn next(&mut self) -> Result<Event, HerdrError> }  // Err(Closed) at EOF, Err(Api{events_lost}) on overrun
  ```

- [ ] **Step 1: Write the failing tests**

`src/herdr/client.rs`:

```rust
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use super::{AgentInfo, AgentList, AgentResult, Created, Event, HerdrError, Incoming, PaneRead, Pong, Request, Response};

pub type BoxRead = Box<dyn AsyncRead + Unpin + Send>;
pub type BoxWrite = Box<dyn AsyncWrite + Unpin + Send>;

/// One herdr socket connection. Requests are sequential: one in flight at a time.
pub struct Connection {
    reader: BufReader<BoxRead>,
    writer: BoxWrite,
    next_id: u64,
    _child: Option<tokio::process::Child>,
}

impl Connection {
    pub fn new(reader: BoxRead, writer: BoxWrite) -> Connection {
        Connection { reader: BufReader::new(reader), writer, next_id: 1, _child: None }
    }

    pub fn with_child(mut self, child: tokio::process::Child) -> Connection {
        self._child = Some(child);
        self
    }

    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value, HerdrError> {
        todo!()
    }

    pub async fn call_as<T: DeserializeOwned>(&mut self, method: &str, params: Value) -> Result<T, HerdrError> {
        let v = self.call(method, params).await?;
        serde_json::from_value(v).map_err(|e| HerdrError::Protocol(format!("{method} result: {e}")))
    }

    pub async fn ping(&mut self) -> Result<Pong, HerdrError> {
        self.call_as("ping", serde_json::json!({})).await
    }

    pub async fn agent_list(&mut self) -> Result<Vec<AgentInfo>, HerdrError> {
        Ok(self.call_as::<AgentList>("agent.list", serde_json::json!({})).await?.agents)
    }

    pub async fn workspace_create(&mut self, cwd: Option<&str>, label: &str) -> Result<Created, HerdrError> {
        self.call_as("workspace.create", serde_json::json!({"cwd": cwd, "label": label, "focus": false})).await
    }

    pub async fn worktree_create(&mut self, cwd: &str, branch: &str, label: &str) -> Result<Created, HerdrError> {
        self.call_as("worktree.create", serde_json::json!({"cwd": cwd, "branch": branch, "label": label, "focus": false})).await
    }

    pub async fn agent_start(&mut self, name: &str, kind: &str, pane_id: &str, args: &[String]) -> Result<AgentInfo, HerdrError> {
        Ok(self.call_as::<AgentResult>("agent.start", serde_json::json!({"name": name, "kind": kind, "pane_id": pane_id, "args": args})).await?.agent)
    }

    pub async fn agent_prompt(&mut self, target: &str, text: &str) -> Result<AgentInfo, HerdrError> {
        Ok(self.call_as::<AgentResult>("agent.prompt", serde_json::json!({"target": target, "text": text})).await?.agent)
    }

    pub async fn agent_read(&mut self, target: &str, lines: u32) -> Result<String, HerdrError> {
        Ok(self.call_as::<PaneRead>("agent.read", serde_json::json!({"target": target, "source": "recent_unwrapped", "lines": lines})).await?.read.text)
    }

    /// Turn this connection into an event stream. herdr dedicates the connection to
    /// events after `events.subscribe`, so no more requests can be sent on it.
    pub async fn subscribe(mut self, subscriptions: Vec<Value>) -> Result<EventStream, HerdrError> {
        todo!()
    }

    async fn write_request(&mut self, method: &str, params: Value) -> Result<String, HerdrError> {
        let id = format!("p{}", self.next_id);
        self.next_id += 1;
        let mut line = serde_json::to_string(&Request { id: id.clone(), method: method.into(), params })
            .map_err(|e| HerdrError::Protocol(e.to_string()))?;
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.flush().await?;
        Ok(id)
    }

    async fn read_line(&mut self) -> Result<Option<String>, HerdrError> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        Ok(Some(line))
    }
}

pub struct EventStream {
    conn: Connection,
}

impl EventStream {
    pub async fn next(&mut self) -> Result<Event, HerdrError> {
        todo!()
    }
}

fn parse_incoming(line: &str) -> Option<Incoming> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    match serde_json::from_str::<Incoming>(line) {
        Ok(v) => Some(v),
        Err(err) => {
            tracing::warn!(%err, line, "skipping unparsable line from herdr");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// Returns a client connection and the server side halves of a duplex pipe.
    fn pipe() -> (Connection, BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>, tokio::io::WriteHalf<tokio::io::DuplexStream>) {
        let (a, b) = duplex(64 * 1024);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        (Connection::new(Box::new(ar), Box::new(aw)), BufReader::new(br), bw)
    }

    #[tokio::test]
    async fn call_returns_result_and_maps_errors() {
        let (mut client, mut sr, mut sw) = pipe();
        let server = tokio::spawn(async move {
            let mut line = String::new();
            sr.read_line(&mut line).await.unwrap();
            let req: Request = serde_json::from_str(&line).unwrap();
            assert_eq!(req.method, "ping");
            sw.write_all(format!("{{\"id\":\"{}\",\"result\":{{\"type\":\"pong\",\"version\":\"0.9.1\",\"protocol\":22}}}}\n", req.id).as_bytes()).await.unwrap();
            let mut line = String::new();
            sr.read_line(&mut line).await.unwrap();
            let req: Request = serde_json::from_str(&line).unwrap();
            sw.write_all(format!("{{\"id\":\"{}\",\"error\":{{\"code\":\"agent_blocked\",\"message\":\"no\"}}}}\n", req.id).as_bytes()).await.unwrap();
        });
        let pong = client.ping().await.unwrap();
        assert_eq!(pong.protocol, 22);
        let err = client.agent_prompt("x", "hi").await.unwrap_err();
        assert_eq!(err.code(), Some("agent_blocked"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn call_reports_closed_on_eof() {
        let (mut client, _sr, sw) = pipe();
        drop(sw);
        let err = client.ping().await.unwrap_err();
        assert!(matches!(err, HerdrError::Closed), "{err:?}");
    }

    #[tokio::test]
    async fn event_stream_skips_bad_lines() {
        let (client, mut sr, mut sw) = pipe();
        let server = tokio::spawn(async move {
            let mut line = String::new();
            sr.read_line(&mut line).await.unwrap();
            let req: Request = serde_json::from_str(&line).unwrap();
            assert_eq!(req.method, "events.subscribe");
            sw.write_all(format!("{{\"id\":\"{}\",\"result\":{{\"type\":\"subscription_started\"}}}}\n", req.id).as_bytes()).await.unwrap();
            sw.write_all(b"this is not json\n\n").await.unwrap();
            sw.write_all(b"{\"event\":\"pane_closed\",\"data\":{\"type\":\"pane_closed\",\"pane_id\":\"w1:p1\",\"workspace_id\":\"w1\"}}\n").await.unwrap();
            sw.write_all(format!("{{\"id\":\"{}\",\"error\":{{\"code\":\"events_lost\",\"message\":\"behind\"}}}}\n", req.id).as_bytes()).await.unwrap();
        });
        let mut stream = client.subscribe(vec![super::super::subscription_lifecycle("pane.closed")]).await.unwrap();
        let ev = stream.next().await.unwrap();
        assert!(ev.is_pane_closed());
        let err = stream.next().await.unwrap_err();
        assert_eq!(err.code(), Some("events_lost"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn subscribe_surfaces_setup_error() {
        let (client, mut sr, mut sw) = pipe();
        tokio::spawn(async move {
            let mut line = String::new();
            sr.read_line(&mut line).await.unwrap();
            let req: Request = serde_json::from_str(&line).unwrap();
            sw.write_all(format!("{{\"id\":\"{}\",\"error\":{{\"code\":\"pane_not_found\",\"message\":\"w9:p9\"}}}}\n", req.id).as_bytes()).await.unwrap();
        });
        let err = client.subscribe(vec![super::super::subscription_agent_status("w9:p9")]).await.unwrap_err();
        assert_eq!(err.code(), Some("pane_not_found"));
    }
}
```

Add to `src/herdr/mod.rs`: `pub mod client;` and `pub use client::*;`.

- [ ] **Step 2: Run tests to see them fail**

Run: `cargo test herdr::client`
Expected: 4 failures with `not yet implemented`.

- [ ] **Step 3: Implement call, subscribe and next**

```rust
    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value, HerdrError> {
        let id = self.write_request(method, params).await?;
        loop {
            let Some(line) = self.read_line().await? else { return Err(HerdrError::Closed) };
            match parse_incoming(&line) {
                Some(Incoming::Response(resp)) if resp.id() == id => {
                    return match resp {
                        Response::Success { result, .. } => Ok(result),
                        Response::Error { error, .. } => Err(HerdrError::Api { code: error.code, message: error.message }),
                    };
                }
                Some(other) => tracing::debug!(?other, "ignoring line while waiting for {id}"),
                None => {}
            }
        }
    }

    pub async fn subscribe(mut self, subscriptions: Vec<Value>) -> Result<EventStream, HerdrError> {
        let id = self.write_request("events.subscribe", serde_json::json!({"subscriptions": subscriptions})).await?;
        loop {
            let Some(line) = self.read_line().await? else { return Err(HerdrError::Closed) };
            match parse_incoming(&line) {
                Some(Incoming::Response(Response::Success { id: rid, .. })) if rid == id => {
                    return Ok(EventStream { conn: self });
                }
                Some(Incoming::Response(Response::Error { id: rid, error })) if rid == id => {
                    return Err(HerdrError::Api { code: error.code, message: error.message });
                }
                _ => {}
            }
        }
    }
```

```rust
impl EventStream {
    pub async fn next(&mut self) -> Result<Event, HerdrError> {
        loop {
            let Some(line) = self.conn.read_line().await? else { return Err(HerdrError::Closed) };
            match parse_incoming(&line) {
                Some(Incoming::Event(ev)) => return Ok(ev),
                Some(Incoming::Response(Response::Error { error, .. })) => {
                    return Err(HerdrError::Api { code: error.code, message: error.message });
                }
                Some(Incoming::Response(Response::Success { .. })) | None => {}
            }
        }
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test herdr::client`
Expected: 4 passed.

- [ ] **Step 5: Commit**

```bash
git add src/herdr
git commit -m "feat: herdr client connection and event stream"
```

---

### Task 5: Fake herdr

**Files:**
- Create: `src/herdr/fake.rs`, `src/bin/fake-herdr.rs`
- Modify: `src/herdr/mod.rs` (add `pub mod fake;`)

**Interfaces:**
- Produces:
  ```rust
  pub enum StartBehaviour { Ready, NotReady, Fail(String) }
  pub struct FakeHerdr { .. }  // Clone, Send
  impl FakeHerdr {
      pub fn new() -> FakeHerdr
      pub fn set_start_behaviour(&self, b: StartBehaviour)
      pub fn set_protocol(&self, p: u32)
      pub async fn serve(&self, reader: BoxRead, writer: BoxWrite)      // one connection until EOF
      pub fn connect(&self) -> Connection                                // in-process duplex, spawns serve
      pub fn set_status(&self, pane_id: &str, status: AgentStatus, completion_seq: Option<u64>)  // broadcasts
      pub fn close_pane(&self, pane_id: &str)                            // removes agent, broadcasts pane_closed
      pub fn exit_pane(&self, pane_id: &str)                             // broadcasts pane_exited
      pub fn agents(&self) -> Vec<AgentInfo>
      pub fn requests(&self) -> Vec<Request>                              // everything received, in order
      pub fn disconnect_all(&self)                                        // drops every live connection
  }
  ```
  Binary `fake-herdr`: serves one connection on stdin/stdout. Env `FAKE_HERDR_AUTO_DONE_MS=<n>`: after `agent.prompt`, flips the agent to `working` then, n ms later, to `idle` with `completion_seq` incremented.

- [ ] **Step 1: Write the failing tests**

`src/herdr/fake.rs`:

```rust
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::broadcast;

use super::{AgentInfo, AgentStatus, BoxRead, BoxWrite, Connection, Event, Request};

#[derive(Debug, Clone)]
pub enum StartBehaviour { Ready, NotReady, Fail(String) }

#[derive(Default)]
struct State {
    next_ws: u32,
    agents: HashMap<String, AgentInfo>,
    requests: Vec<Request>,
    start: Option<StartBehaviour>,
    protocol: u32,
    generation: u64,
}

#[derive(Clone)]
pub struct FakeHerdr {
    state: Arc<Mutex<State>>,
    events: broadcast::Sender<Event>,
    /// Bumped by disconnect_all; serve loops exit when it changes.
    kill: broadcast::Sender<()>,
}

impl Default for FakeHerdr {
    fn default() -> Self { Self::new() }
}

impl FakeHerdr {
    pub fn new() -> FakeHerdr {
        let (events, _) = broadcast::channel(256);
        let (kill, _) = broadcast::channel(16);
        FakeHerdr { state: Arc::new(Mutex::new(State { protocol: 22, ..Default::default() })), events, kill }
    }

    pub fn set_start_behaviour(&self, b: StartBehaviour) { self.state.lock().unwrap().start = Some(b); }
    pub fn set_protocol(&self, p: u32) { self.state.lock().unwrap().protocol = p; }
    pub fn agents(&self) -> Vec<AgentInfo> { self.state.lock().unwrap().agents.values().cloned().collect() }
    pub fn requests(&self) -> Vec<Request> { self.state.lock().unwrap().requests.clone() }

    pub fn connect(&self) -> Connection {
        let (a, b) = tokio::io::duplex(64 * 1024);
        let (ar, aw) = tokio::io::split(a);
        let (br, bw) = tokio::io::split(b);
        let fake = self.clone();
        tokio::spawn(async move { fake.serve(Box::new(br), Box::new(bw)).await });
        Connection::new(Box::new(ar), Box::new(aw))
    }

    pub fn set_status(&self, pane_id: &str, status: AgentStatus, completion_seq: Option<u64>) {
        let mut s = self.state.lock().unwrap();
        if let Some(a) = s.agents.get_mut(pane_id) {
            a.agent_status = status;
            a.state_change_seq += 1;
            if completion_seq.is_some() { a.completion_seq = completion_seq; }
        }
        let ws = pane_id.split(':').next().unwrap_or("w1").to_string();
        let _ = self.events.send(Event { event: "pane.agent_status_changed".into(), data: json!({"pane_id": pane_id, "workspace_id": ws, "agent_status": status}) });
    }

    pub fn close_pane(&self, pane_id: &str) {
        self.state.lock().unwrap().agents.remove(pane_id);
        let ws = pane_id.split(':').next().unwrap_or("w1").to_string();
        let _ = self.events.send(Event { event: "pane_closed".into(), data: json!({"type": "pane_closed", "pane_id": pane_id, "workspace_id": ws}) });
    }

    pub fn exit_pane(&self, pane_id: &str) {
        self.state.lock().unwrap().agents.remove(pane_id);
        let ws = pane_id.split(':').next().unwrap_or("w1").to_string();
        let _ = self.events.send(Event { event: "pane_exited".into(), data: json!({"type": "pane_exited", "pane_id": pane_id, "workspace_id": ws}) });
    }

    pub fn disconnect_all(&self) {
        self.state.lock().unwrap().generation += 1;
        let _ = self.kill.send(());
    }

    pub async fn serve(&self, reader: BoxRead, writer: BoxWrite) {
        todo!()
    }

    fn handle(&self, req: &Request) -> Result<Value, (String, String)> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_start_prompt_list_roundtrip() {
        let fake = FakeHerdr::new();
        let mut c = fake.connect();
        assert_eq!(c.ping().await.unwrap().protocol, 22);
        let created = c.workspace_create(Some("/tmp"), "t-1").await.unwrap();
        assert_eq!(created.root_pane.pane_id, "w1:p1");
        let a = c.agent_start("t-1", "claude", "w1:p1", &[]).await.unwrap();
        assert_eq!(a.name.as_deref(), Some("t-1"));
        assert_eq!(a.agent_status, AgentStatus::Idle);
        let a = c.agent_prompt("t-1", "hello").await.unwrap();
        assert_eq!(a.agent_status, AgentStatus::Working);
        let list = c.agent_list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(fake.requests().iter().map(|r| r.method.as_str()).collect::<Vec<_>>(), ["ping", "workspace.create", "agent.start", "agent.prompt", "agent.list"]);
    }

    #[tokio::test]
    async fn start_behaviours() {
        let fake = FakeHerdr::new();
        fake.set_start_behaviour(StartBehaviour::NotReady);
        let mut c = fake.connect();
        let created = c.workspace_create(None, "x").await.unwrap();
        let err = c.agent_start("t-2", "claude", &created.root_pane.pane_id, &[]).await.unwrap_err();
        assert_eq!(err.code(), Some("agent_not_ready"));
        fake.set_start_behaviour(StartBehaviour::Fail("unsupported_agent_kind".into()));
        let err = c.agent_start("t-3", "nope", &created.root_pane.pane_id, &[]).await.unwrap_err();
        assert_eq!(err.code(), Some("unsupported_agent_kind"));
        let err = c.agent_start("t-4", "claude", "w9:p9", &[]).await.unwrap_err();
        assert_eq!(err.code(), Some("pane_not_found"));
    }

    #[tokio::test]
    async fn prompt_on_blocked_agent_is_rejected() {
        let fake = FakeHerdr::new();
        let mut c = fake.connect();
        let created = c.workspace_create(None, "x").await.unwrap();
        c.agent_start("t-1", "claude", &created.root_pane.pane_id, &[]).await.unwrap();
        fake.set_status(&created.root_pane.pane_id, AgentStatus::Blocked, None);
        let err = c.agent_prompt("t-1", "hi").await.unwrap_err();
        assert_eq!(err.code(), Some("agent_blocked"));
    }

    #[tokio::test]
    async fn subscription_filters_by_pane_and_delivers_lifecycle() {
        let fake = FakeHerdr::new();
        let mut c = fake.connect();
        let a = c.workspace_create(None, "a").await.unwrap();
        let b = c.workspace_create(None, "b").await.unwrap();
        c.agent_start("t-1", "claude", &a.root_pane.pane_id, &[]).await.unwrap();
        c.agent_start("t-2", "claude", &b.root_pane.pane_id, &[]).await.unwrap();
        let mut stream = fake.connect().subscribe(vec![
            super::super::subscription_lifecycle("pane.closed"),
            super::super::subscription_agent_status(&a.root_pane.pane_id),
        ]).await.unwrap();
        fake.set_status(&b.root_pane.pane_id, AgentStatus::Blocked, None); // filtered out
        fake.set_status(&a.root_pane.pane_id, AgentStatus::Working, None);
        fake.close_pane(&b.root_pane.pane_id);
        let e1 = stream.next().await.unwrap();
        assert_eq!(e1.pane_id(), Some(a.root_pane.pane_id.as_str()));
        assert_eq!(e1.agent_status(), Some(AgentStatus::Working));
        let e2 = stream.next().await.unwrap();
        assert!(e2.is_pane_closed());
        assert_eq!(e2.pane_id(), Some(b.root_pane.pane_id.as_str()));
    }

    #[tokio::test]
    async fn disconnect_all_closes_connections() {
        let fake = FakeHerdr::new();
        let mut c = fake.connect();
        c.ping().await.unwrap();
        fake.disconnect_all();
        let err = c.ping().await.unwrap_err();
        assert!(matches!(err, super::super::HerdrError::Closed | super::super::HerdrError::Io(_)), "{err:?}");
    }
}
```

Add `pub mod fake;` to `src/herdr/mod.rs`.

- [ ] **Step 2: Run tests to see them fail**

Run: `cargo test herdr::fake`
Expected: 5 failures with `not yet implemented`.

- [ ] **Step 3: Implement serve and handle**

```rust
    pub async fn serve(&self, reader: BoxRead, mut writer: BoxWrite) {
        let mut reader = BufReader::new(reader);
        let mut kill = self.kill.subscribe();
        loop {
            let mut line = String::new();
            let read = tokio::select! {
                r = reader.read_line(&mut line) => r,
                _ = kill.recv() => return,
            };
            match read {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            let Ok(req) = serde_json::from_str::<Request>(line.trim()) else { continue };
            self.state.lock().unwrap().requests.push(req.clone());
            if req.method == "events.subscribe" {
                let subs: Vec<Value> = req.params.get("subscriptions").and_then(|v| v.as_array()).cloned().unwrap_or_default();
                let mut rx = self.events.subscribe();
                let ack = json!({"id": req.id, "result": {"type": "subscription_started"}});
                if writer.write_all(format!("{ack}\n").as_bytes()).await.is_err() { return; }
                loop {
                    let ev = tokio::select! {
                        ev = rx.recv() => ev,
                        _ = kill.recv() => return,
                    };
                    let ev = match ev {
                        Ok(ev) => ev,
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            let err = json!({"id": req.id, "error": {"code": "events_lost", "message": "fell behind"}});
                            let _ = writer.write_all(format!("{err}\n").as_bytes()).await;
                            return;
                        }
                        Err(_) => return,
                    };
                    if subscription_matches(&subs, &ev) {
                        let line = serde_json::to_string(&ev).unwrap();
                        if writer.write_all(format!("{line}\n").as_bytes()).await.is_err() { return; }
                    }
                }
            }
            let reply = match self.handle(&req) {
                Ok(result) => json!({"id": req.id, "result": result}),
                Err((code, message)) => json!({"id": req.id, "error": {"code": code, "message": message}}),
            };
            if writer.write_all(format!("{reply}\n").as_bytes()).await.is_err() { return; }
        }
    }

    fn handle(&self, req: &Request) -> Result<Value, (String, String)> {
        let mut s = self.state.lock().unwrap();
        let p = &req.params;
        match req.method.as_str() {
            "ping" => Ok(json!({"type": "pong", "version": "fake", "protocol": s.protocol})),
            "workspace.create" | "worktree.create" => {
                s.next_ws += 1;
                let ws = format!("w{}", s.next_ws);
                let pane = format!("{ws}:p1");
                let label = p.get("label").cloned().unwrap_or(Value::Null);
                let kind = if req.method == "worktree.create" { "worktree_created" } else { "workspace_created" };
                let mut result = json!({"type": kind, "workspace": {"workspace_id": ws, "label": label}, "tab": {"tab_id": format!("{ws}:t1")}, "root_pane": {"pane_id": pane, "workspace_id": ws}});
                if kind == "worktree_created" {
                    result["worktree"] = json!({"path": p.get("cwd").cloned().unwrap_or(Value::Null), "branch": p.get("branch").cloned().unwrap_or(Value::Null)});
                }
                Ok(result)
            }
            "agent.start" => {
                let pane_id = p["pane_id"].as_str().unwrap_or("").to_string();
                let ws = pane_id.split(':').next().unwrap_or("").to_string();
                if ws.is_empty() || s.next_ws < ws[1..].parse::<u32>().unwrap_or(u32::MAX) {
                    return Err(("pane_not_found".into(), pane_id));
                }
                match s.start.clone().unwrap_or(StartBehaviour::Ready) {
                    StartBehaviour::Fail(code) => return Err((code, "start failed".into())),
                    StartBehaviour::NotReady => return Err(("agent_not_ready".into(), "blocked during startup".into())),
                    StartBehaviour::Ready => {}
                }
                let info = AgentInfo {
                    pane_id: pane_id.clone(), workspace_id: ws.clone(), tab_id: format!("{ws}:t1"),
                    name: p["name"].as_str().map(str::to_string), agent: p["kind"].as_str().map(str::to_string),
                    agent_status: AgentStatus::Idle, completion_seq: None, state_change_seq: 1, interactive_ready: true,
                };
                s.agents.insert(pane_id, info.clone());
                Ok(json!({"type": "agent_started", "agent": info, "argv": []}))
            }
            "agent.prompt" => {
                let target = p["target"].as_str().unwrap_or("");
                let Some(a) = s.agents.values_mut().find(|a| a.name.as_deref() == Some(target) || a.pane_id == target) else {
                    return Err(("agent_not_found".into(), target.into()));
                };
                if a.agent_status == AgentStatus::Blocked {
                    return Err(("agent_blocked".into(), "agent is blocked".into()));
                }
                a.agent_status = AgentStatus::Working;
                a.state_change_seq += 1;
                let info = a.clone();
                let _ = self.events.send(Event { event: "pane.agent_status_changed".into(), data: json!({"pane_id": info.pane_id, "workspace_id": info.workspace_id, "agent_status": "working"}) });
                Ok(json!({"type": "agent_prompted", "agent": info}))
            }
            "agent.list" => Ok(json!({"type": "agent_list", "agents": s.agents.values().cloned().collect::<Vec<_>>()})),
            "agent.read" => Ok(json!({"type": "pane_read", "read": {"text": "fake output\n"}})),
            other => Err(("unsupported_method".into(), other.into())),
        }
    }
}

fn subscription_matches(subs: &[Value], ev: &Event) -> bool {
    subs.iter().any(|s| {
        let t = s.get("type").and_then(Value::as_str).unwrap_or("");
        match t {
            "pane.agent_status_changed" => ev.is_agent_status() && s.get("pane_id").and_then(Value::as_str) == ev.pane_id(),
            "pane.closed" => ev.is_pane_closed(),
            "pane.exited" => ev.is_pane_exited(),
            _ => false,
        }
    })
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test herdr::fake`
Expected: 5 passed. If `disconnect_all_closes_connections` is flaky because the second `ping` is written before the server task observed the kill, add `tokio::task::yield_now().await;` after `disconnect_all()` in the test.

- [ ] **Step 5: Write the fake-herdr binary**

`src/bin/fake-herdr.rs`:

```rust
//! Speaks the herdr socket protocol on stdin/stdout. Used by transport tests and for
//! running pastor end to end without a real herdr: put
//! `command = ["target/debug/fake-herdr"]` on a flock machine.
use pastor::herdr::{fake::FakeHerdr, AgentStatus};

#[tokio::main]
async fn main() {
    let fake = FakeHerdr::new();
    if let Some(ms) = std::env::var("FAKE_HERDR_AUTO_DONE_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
        let watcher = fake.clone();
        tokio::spawn(async move {
            let mut seen = std::collections::HashSet::new();
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                for a in watcher.agents() {
                    if a.agent_status == AgentStatus::Working && seen.insert((a.pane_id.clone(), a.state_change_seq)) {
                        let w = watcher.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                            w.set_status(&a.pane_id, AgentStatus::Idle, Some(a.completion_seq.unwrap_or(0) + 1));
                        });
                    }
                }
            }
        });
    }
    fake.serve(Box::new(tokio::io::stdin()), Box::new(tokio::io::stdout())).await;
}
```

- [ ] **Step 6: Build and smoke test the binary**

Run:

```bash
cargo build
printf '{"id":"1","method":"ping","params":{}}\n' | ./target/debug/fake-herdr
```

Expected: one line `{"id":"1","result":{"protocol":22,"type":"pong","version":"fake"}}`.

- [ ] **Step 7: Commit**

```bash
git add src/herdr src/bin
git commit -m "feat: fake herdr for tests and local runs"
```

---

### Task 6: Transports

**Files:**
- Create: `src/herdr/transport.rs`
- Modify: `src/herdr/mod.rs` (add `pub mod transport; pub use transport::*;`)

**Interfaces:**
- Produces:
  ```rust
  pub enum Endpoint { Local { session: String }, Ssh { target: String, session: String }, Command { argv: Vec<String> } }
  impl Endpoint { pub fn from_machine(m: &MachineConfig) -> Endpoint; pub fn describe(&self) -> String }
  pub fn local_socket_path(session: &str) -> anyhow::Result<PathBuf>
  pub fn bridge_command(session: &str) -> String   // "herdr --session <s> remote-api-bridge"
  pub async fn connect(ep: &Endpoint) -> Result<Connection, ConnectError>
  pub struct ConnectError { pub message: String }  // includes the child's stderr when it exited early
  ```

- [ ] **Step 1: Write the failing tests**

`src/herdr/transport.rs`:

```rust
use std::path::PathBuf;
use std::process::Stdio;

use tokio::io::AsyncReadExt;

use super::Connection;
use crate::config::flock::MachineConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    Local { session: String },
    Ssh { target: String, session: String },
    Command { argv: Vec<String> },
}

impl Endpoint {
    pub fn from_machine(m: &MachineConfig) -> Endpoint {
        if let Some(argv) = &m.command {
            Endpoint::Command { argv: argv.clone() }
        } else if let Some(target) = &m.ssh {
            Endpoint::Ssh { target: target.clone(), session: m.session.clone() }
        } else {
            Endpoint::Local { session: m.session.clone() }
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Endpoint::Local { session } => format!("local herdr session {session}"),
            Endpoint::Ssh { target, session } => format!("ssh {target} (session {session})"),
            Endpoint::Command { argv } => format!("command {}", argv.join(" ")),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ConnectError { pub message: String }

pub fn local_socket_path(session: &str) -> anyhow::Result<PathBuf> {
    let base = dirs::config_dir().ok_or_else(|| anyhow::anyhow!("no config dir"))?.join("herdr");
    Ok(if session == "default" { base.join("herdr.sock") } else { base.join("sessions").join(session).join("herdr.sock") })
}

/// The exact command herdr's own client runs on the remote host.
pub fn bridge_command(session: &str) -> String {
    format!("herdr --session {} remote-api-bridge", shell_quote(session))
}

fn shell_quote(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) { s.to_string() } else { format!("'{}'", s.replace('\'', "'\\''")) }
}

pub async fn connect(ep: &Endpoint) -> Result<Connection, ConnectError> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_paths_and_bridge_command() {
        let p = local_socket_path("default").unwrap();
        assert!(p.ends_with("herdr/herdr.sock"), "{}", p.display());
        let p = local_socket_path("agents").unwrap();
        assert!(p.ends_with("herdr/sessions/agents/herdr.sock"));
        assert_eq!(bridge_command("default"), "herdr --session default remote-api-bridge");
        assert_eq!(bridge_command("my session"), "herdr --session 'my session' remote-api-bridge");
    }

    #[tokio::test]
    async fn command_transport_talks_to_fake_herdr() {
        let ep = Endpoint::Command { argv: vec![env!("CARGO_BIN_EXE_fake-herdr").to_string()] };
        let mut c = connect(&ep).await.unwrap();
        assert_eq!(c.ping().await.unwrap().protocol, 22);
    }

    #[tokio::test]
    async fn process_transport_reports_exit_and_stderr() {
        let ep = Endpoint::Command { argv: vec!["sh".into(), "-c".into(), "echo permission denied >&2; exit 255".into()] };
        let err = connect(&ep).await.unwrap_err();
        assert!(err.message.contains("permission denied"), "{}", err.message);
        assert!(err.message.contains("255"), "{}", err.message);
    }

    #[tokio::test]
    async fn local_transport_reports_missing_socket() {
        std::env::set_var("XDG_CONFIG_HOME", "/nonexistent-pastor-test");
        let err = connect(&Endpoint::Local { session: "default".into() }).await.unwrap_err();
        std::env::remove_var("XDG_CONFIG_HOME");
        assert!(err.message.contains("herdr.sock"), "{}", err.message);
    }
}
```

Add `pub mod transport;` and `pub use transport::*;` to `src/herdr/mod.rs`.

Note: `env!("CARGO_BIN_EXE_fake-herdr")` is only available in integration tests and... it is also available to unit tests of the same package when the bin target exists. If the compiler rejects it here, move `command_transport_talks_to_fake_herdr` to `tests/transport.rs` with `use pastor::herdr::transport::*;`.

- [ ] **Step 2: Run tests to see them fail**

Run: `cargo test herdr::transport`
Expected: `socket_paths_and_bridge_command` passes, the three async ones panic with `not yet implemented`.

- [ ] **Step 3: Implement connect**

```rust
pub async fn connect(ep: &Endpoint) -> Result<Connection, ConnectError> {
    match ep {
        Endpoint::Local { session } => {
            let path = local_socket_path(session).map_err(|e| ConnectError { message: e.to_string() })?;
            let stream = tokio::net::UnixStream::connect(&path).await.map_err(|e| ConnectError { message: format!("connect {}: {e}", path.display()) })?;
            let (r, w) = stream.into_split();
            Ok(Connection::new(Box::new(r), Box::new(w)))
        }
        Endpoint::Ssh { target, session } => {
            let argv = vec![
                "ssh".to_string(), "-o".into(), "BatchMode=yes".into(), "-o".into(), "ServerAliveInterval=15".into(),
                "-o".into(), "ServerAliveCountMax=3".into(), "-T".into(), target.clone(), bridge_command(session),
            ];
            spawn(&argv).await
        }
        Endpoint::Command { argv } => spawn(argv).await,
    }
}

/// Spawn argv with piped stdio, then prove the bridge is alive with a `ping`.
/// If the process exits before answering, report its exit status and stderr.
async fn spawn(argv: &[String]) -> Result<Connection, ConnectError> {
    let (program, args) = argv.split_first().ok_or_else(|| ConnectError { message: "empty command".into() })?;
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| ConnectError { message: format!("spawn {program}: {e}") })?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let mut conn = Connection::new(Box::new(stdout), Box::new(stdin));
    match tokio::time::timeout(std::time::Duration::from_secs(30), conn.ping()).await {
        Ok(Ok(_)) => Ok(conn.with_child(child)),
        Ok(Err(err)) => {
            drop(conn); // closes the child's stdin so a live process can exit
            let mut err_text = String::new();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), stderr.read_to_string(&mut err_text)).await;
            let status = match tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await {
                Ok(Ok(s)) => s.to_string(),
                Ok(Err(e)) => e.to_string(),
                Err(_) => { let _ = child.kill().await; "still running, killed".to_string() }
            };
            Err(ConnectError { message: format!("{}: {err} ({status}) {}", argv.join(" "), err_text.trim()) })
        }
        Err(_) => Err(ConnectError { message: format!("{}: no ping reply within 30s", argv.join(" ")) }),
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test herdr::transport`
Expected: 4 passed.

- [ ] **Step 5: Commit**

```bash
git add src/herdr
git commit -m "feat: local, ssh and command transports"
```

---

### Task 7: Task model and SQLite store

**Files:**
- Create: `src/task.rs`, `src/store.rs`
- Modify: `src/lib.rs` (add `pub mod task; pub mod store;`)

**Interfaces:**
- Produces:
  ```rust
  // task.rs
  pub enum TaskState { Queued, Starting, Running, Blocked, Done, Stale, Failed, Closed }   // serde + Display lowercase; FromStr
  impl TaskState { pub fn occupies_pane(&self) -> bool /* starting|running|blocked|done|stale */; pub fn is_open(&self) -> bool /* not failed|closed */ }
  pub struct DispatchSpec { pub agent: String, pub agent_args: Vec<String>, pub repo: Option<String>, pub worktree: bool, pub branch: Option<String>, pub machine: Option<String>, pub tags: Vec<String>, pub timeout_secs: u64 }
  pub struct Task { pub id: i64, pub job: String, pub item: Value, pub prompt: String, pub spec: DispatchSpec, pub machine: Option<String>, pub workspace_id: Option<String>, pub pane_id: Option<String>, pub agent_name: Option<String>, pub state: TaskState, pub error: Option<String>, pub last_completion_seq: Option<u64>, pub created_at: DateTime<Utc>, pub started_at: Option<DateTime<Utc>>, pub finished_at: Option<DateTime<Utc>>, pub updated_at: DateTime<Utc> }
  impl Task { pub fn display_id(&self) -> String /* "t-<id>" */; pub fn agent_name_for(id: i64) -> String }
  pub fn parse_task_id(s: &str) -> Option<i64>   // "t-12" or "12"
  pub enum Observed { Status { status: AgentStatus, completion_seq: Option<u64> }, PaneClosed, PaneExited }
  pub fn next_state(task: &Task, observed: &Observed) -> Option<TaskState>   // pure transition
  // store.rs
  pub struct Store { .. }  // Send + Sync
  pub struct NewTask { pub job: String, pub item: Value, pub prompt: String, pub spec: DispatchSpec }
  pub struct TaskFilter { pub job: Option<String>, pub machine: Option<String>, pub states: Option<Vec<TaskState>> }
  impl Store {
      pub fn open(path: &Path) -> anyhow::Result<Store>; pub fn open_in_memory() -> anyhow::Result<Store>
      pub fn insert_task(&self, t: NewTask) -> anyhow::Result<Task>
      pub fn get_task(&self, id: i64) -> anyhow::Result<Option<Task>>
      pub fn update_task(&self, t: &Task) -> anyhow::Result<()>          // sets updated_at
      pub fn list_tasks(&self, f: &TaskFilter) -> anyhow::Result<Vec<Task>>  // newest first
      pub fn tasks_on_machine(&self, machine: &str) -> anyhow::Result<Vec<Task>>  // open tasks with a pane
      pub fn queued_tasks(&self) -> anyhow::Result<Vec<Task>>             // oldest first
      pub fn find_by_pane(&self, machine: &str, pane_id: &str) -> anyhow::Result<Option<Task>>
  }
  ```

- [ ] **Step 1: Write the failing tests for the transition function**

`src/task.rs`:

```rust
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::herdr::AgentStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskState { Queued, Starting, Running, Blocked, Done, Stale, Failed, Closed }

impl TaskState {
    pub fn occupies_pane(&self) -> bool {
        matches!(self, TaskState::Starting | TaskState::Running | TaskState::Blocked | TaskState::Done | TaskState::Stale)
    }
    pub fn is_open(&self) -> bool { !matches!(self, TaskState::Failed | TaskState::Closed) }
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskState::Queued => "queued", TaskState::Starting => "starting", TaskState::Running => "running",
            TaskState::Blocked => "blocked", TaskState::Done => "done", TaskState::Stale => "stale",
            TaskState::Failed => "failed", TaskState::Closed => "closed",
        }
    }
}

impl std::fmt::Display for TaskState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.as_str()) }
}

impl std::str::FromStr for TaskState {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        serde_json::from_value(Value::String(s.to_string())).map_err(|_| format!("unknown state {s}"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchSpec {
    pub agent: String,
    #[serde(default)]
    pub agent_args: Vec<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub worktree: bool,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub machine: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

fn default_timeout() -> u64 { 2 * 60 * 60 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: i64,
    pub job: String,
    pub item: Value,
    pub prompt: String,
    pub spec: DispatchSpec,
    pub machine: Option<String>,
    pub workspace_id: Option<String>,
    pub pane_id: Option<String>,
    pub agent_name: Option<String>,
    pub state: TaskState,
    pub error: Option<String>,
    pub last_completion_seq: Option<u64>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

impl Task {
    pub fn display_id(&self) -> String { format!("t-{}", self.id) }
    pub fn agent_name_for(id: i64) -> String { format!("t-{id}") }
}

pub fn parse_task_id(s: &str) -> Option<i64> {
    s.strip_prefix("t-").unwrap_or(s).parse().ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observed {
    Status { status: AgentStatus, completion_seq: Option<u64> },
    PaneClosed,
    PaneExited,
}

/// Pure transition. `None` means no change. The settle window for `Done` is the
/// caller's job: it should confirm the agent is still idle after the window.
pub fn next_state(task: &Task, observed: &Observed) -> Option<TaskState> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn task(state: TaskState, last_completion_seq: Option<u64>) -> Task {
        let now = Utc::now();
        Task {
            id: 1, job: "run".into(), item: Value::Null, prompt: "p".into(),
            spec: DispatchSpec { agent: "claude".into(), agent_args: vec![], repo: None, worktree: false, branch: None, machine: None, tags: vec![], timeout_secs: 10 },
            machine: Some("pi-1".into()), workspace_id: Some("w1".into()), pane_id: Some("w1:p1".into()), agent_name: Some("t-1".into()),
            state, error: None, last_completion_seq, created_at: now, started_at: Some(now), finished_at: None, updated_at: now,
        }
    }

    fn status(s: AgentStatus, seq: Option<u64>) -> Observed { Observed::Status { status: s, completion_seq: seq } }

    #[test]
    fn working_means_running_and_blocked_means_blocked() {
        assert_eq!(next_state(&task(TaskState::Starting, None), &status(AgentStatus::Working, None)), Some(TaskState::Running));
        assert_eq!(next_state(&task(TaskState::Done, Some(1)), &status(AgentStatus::Working, Some(1))), Some(TaskState::Running));
        assert_eq!(next_state(&task(TaskState::Running, None), &status(AgentStatus::Blocked, None)), Some(TaskState::Blocked));
        assert_eq!(next_state(&task(TaskState::Running, None), &status(AgentStatus::Working, None)), None);
    }

    #[test]
    fn idle_is_done_only_when_completion_seq_advances() {
        assert_eq!(next_state(&task(TaskState::Running, None), &status(AgentStatus::Idle, Some(1))), Some(TaskState::Done));
        assert_eq!(next_state(&task(TaskState::Running, Some(1)), &status(AgentStatus::Done, Some(2))), Some(TaskState::Done));
        assert_eq!(next_state(&task(TaskState::Running, Some(2)), &status(AgentStatus::Idle, Some(2))), None);
        assert_eq!(next_state(&task(TaskState::Starting, None), &status(AgentStatus::Idle, None)), None);
        assert_eq!(next_state(&task(TaskState::Blocked, None), &status(AgentStatus::Idle, None)), Some(TaskState::Running));
    }

    #[test]
    fn unknown_changes_nothing_and_terminal_states_are_sticky() {
        assert_eq!(next_state(&task(TaskState::Running, None), &status(AgentStatus::Unknown, None)), None);
        assert_eq!(next_state(&task(TaskState::Failed, None), &status(AgentStatus::Working, None)), None);
        assert_eq!(next_state(&task(TaskState::Closed, None), &Observed::PaneExited), None);
    }

    #[test]
    fn pane_events() {
        assert_eq!(next_state(&task(TaskState::Running, None), &Observed::PaneClosed), Some(TaskState::Closed));
        assert_eq!(next_state(&task(TaskState::Done, Some(1)), &Observed::PaneClosed), Some(TaskState::Closed));
        assert_eq!(next_state(&task(TaskState::Running, None), &Observed::PaneExited), Some(TaskState::Failed));
        assert_eq!(next_state(&task(TaskState::Done, Some(1)), &Observed::PaneExited), Some(TaskState::Closed));
    }

    #[test]
    fn ids() {
        assert_eq!(parse_task_id("t-12"), Some(12));
        assert_eq!(parse_task_id("12"), Some(12));
        assert_eq!(parse_task_id("x"), None);
        assert_eq!(Task::agent_name_for(7), "t-7");
        assert_eq!("blocked".parse::<TaskState>().unwrap(), TaskState::Blocked);
    }
}
```

- [ ] **Step 2: Run to see failures**

Run: `cargo test task::`
Expected: 4 panics with `not yet implemented`, `ids` passes.

- [ ] **Step 3: Implement next_state**

```rust
pub fn next_state(task: &Task, observed: &Observed) -> Option<TaskState> {
    use TaskState::*;
    if !task.state.is_open() {
        return None;
    }
    let to = match observed {
        Observed::PaneClosed => Closed,
        Observed::PaneExited => if task.state == Done { Closed } else { Failed },
        Observed::Status { status, completion_seq } => match status {
            AgentStatus::Working => Running,
            AgentStatus::Blocked => Blocked,
            AgentStatus::Unknown => return None,
            AgentStatus::Idle | AgentStatus::Done => {
                let advanced = match (completion_seq, task.last_completion_seq) {
                    (Some(new), Some(old)) => *new > old,
                    (Some(_), None) => true,
                    (None, _) => false,
                };
                if advanced {
                    Done
                } else if task.state == Blocked {
                    // The human answered the prompt; the agent is idle again but has not
                    // produced completed work since. Treat it as running until it does.
                    Running
                } else {
                    return None;
                }
            }
        },
    };
    if to == task.state { None } else { Some(to) }
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test task::`
Expected: 5 passed.

- [ ] **Step 5: Write the failing store tests**

`src/store.rs`:

```rust
use std::path::Path;
use std::sync::Mutex;

use anyhow::Context;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde_json::Value;

use crate::task::{DispatchSpec, Task, TaskState};

const SCHEMA_VERSION: i64 = 1;

pub struct Store {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone)]
pub struct NewTask { pub job: String, pub item: Value, pub prompt: String, pub spec: DispatchSpec }

#[derive(Debug, Clone, Default)]
pub struct TaskFilter { pub job: Option<String>, pub machine: Option<String>, pub states: Option<Vec<TaskState>> }

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Store> {
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> anyhow::Result<Store> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> anyhow::Result<Store> { todo!() }

    pub fn insert_task(&self, t: NewTask) -> anyhow::Result<Task> { todo!() }
    pub fn get_task(&self, id: i64) -> anyhow::Result<Option<Task>> { todo!() }
    pub fn update_task(&self, t: &Task) -> anyhow::Result<()> { todo!() }
    pub fn list_tasks(&self, f: &TaskFilter) -> anyhow::Result<Vec<Task>> { todo!() }

    /// Open tasks that hold a pane on this machine (for capacity and reconciliation).
    pub fn tasks_on_machine(&self, machine: &str) -> anyhow::Result<Vec<Task>> {
        let all = self.list_tasks(&TaskFilter { machine: Some(machine.into()), ..Default::default() })?;
        Ok(all.into_iter().filter(|t| t.state.occupies_pane()).collect())
    }

    pub fn queued_tasks(&self) -> anyhow::Result<Vec<Task>> {
        let mut v = self.list_tasks(&TaskFilter { states: Some(vec![TaskState::Queued]), ..Default::default() })?;
        v.reverse();
        Ok(v)
    }

    pub fn find_by_pane(&self, machine: &str, pane_id: &str) -> anyhow::Result<Option<Task>> {
        Ok(self.tasks_on_machine(machine)?.into_iter().find(|t| t.pane_id.as_deref() == Some(pane_id)))
    }
}

fn row_to_task(row: &Row<'_>) -> rusqlite::Result<Task> {
    let parse_dt = |s: String| DateTime::parse_from_rfc3339(&s).map(|d| d.with_timezone(&Utc)).unwrap_or_else(|_| Utc::now());
    let item: String = row.get("item")?;
    let spec: String = row.get("spec")?;
    let state: String = row.get("state")?;
    Ok(Task {
        id: row.get("id")?,
        job: row.get("job")?,
        item: serde_json::from_str(&item).unwrap_or(Value::Null),
        prompt: row.get("prompt")?,
        spec: serde_json::from_str(&spec).map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?,
        machine: row.get("machine")?,
        workspace_id: row.get("workspace_id")?,
        pane_id: row.get("pane_id")?,
        agent_name: row.get("agent_name")?,
        state: state.parse().unwrap_or(TaskState::Failed),
        error: row.get("error")?,
        last_completion_seq: row.get::<_, Option<i64>>("last_completion_seq")?.map(|v| v as u64),
        created_at: parse_dt(row.get("created_at")?),
        started_at: row.get::<_, Option<String>>("started_at")?.map(parse_dt),
        finished_at: row.get::<_, Option<String>>("finished_at")?.map(parse_dt),
        updated_at: parse_dt(row.get("updated_at")?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> DispatchSpec {
        DispatchSpec { agent: "claude".into(), agent_args: vec!["--model".into(), "x".into()], repo: Some("~/w".into()), worktree: true, branch: Some("b".into()), machine: None, tags: vec!["fast".into()], timeout_secs: 60 }
    }

    fn new_task(job: &str) -> NewTask {
        NewTask { job: job.into(), item: serde_json::json!({"key": "k1", "title": "t"}), prompt: "do it\nnow \"quoted\" {{ x }}".into(), spec: spec() }
    }

    #[test]
    fn insert_get_update_roundtrip() {
        let s = Store::open_in_memory().unwrap();
        let t = s.insert_task(new_task("run")).unwrap();
        assert_eq!(t.id, 1);
        assert_eq!(t.state, TaskState::Queued);
        assert_eq!(t.display_id(), "t-1");
        let mut got = s.get_task(1).unwrap().unwrap();
        assert_eq!(got.prompt, "do it\nnow \"quoted\" {{ x }}");
        assert_eq!(got.spec, spec());
        assert_eq!(got.item["key"], "k1");
        got.state = TaskState::Running;
        got.machine = Some("pi-3".into());
        got.pane_id = Some("w2:p1".into());
        got.agent_name = Some("t-1".into());
        got.last_completion_seq = Some(4);
        got.started_at = Some(Utc::now());
        s.update_task(&got).unwrap();
        let again = s.get_task(1).unwrap().unwrap();
        assert_eq!(again.state, TaskState::Running);
        assert_eq!(again.machine.as_deref(), Some("pi-3"));
        assert_eq!(again.last_completion_seq, Some(4));
        assert!(again.updated_at >= got.updated_at);
        assert!(s.get_task(99).unwrap().is_none());
    }

    #[test]
    fn filters_and_helpers() {
        let s = Store::open_in_memory().unwrap();
        let mut a = s.insert_task(new_task("slack")).unwrap();
        let mut b = s.insert_task(new_task("slack")).unwrap();
        let c = s.insert_task(new_task("asana")).unwrap();
        a.state = TaskState::Running; a.machine = Some("pi-3".into()); a.pane_id = Some("w1:p1".into());
        b.state = TaskState::Closed; b.machine = Some("pi-3".into()); b.pane_id = Some("w2:p1".into());
        s.update_task(&a).unwrap();
        s.update_task(&b).unwrap();
        let newest_first = s.list_tasks(&TaskFilter::default()).unwrap();
        assert_eq!(newest_first.iter().map(|t| t.id).collect::<Vec<_>>(), vec![3, 2, 1]);
        assert_eq!(s.list_tasks(&TaskFilter { job: Some("slack".into()), ..Default::default() }).unwrap().len(), 2);
        assert_eq!(s.list_tasks(&TaskFilter { states: Some(vec![TaskState::Running, TaskState::Closed]), ..Default::default() }).unwrap().len(), 2);
        assert_eq!(s.tasks_on_machine("pi-3").unwrap().iter().map(|t| t.id).collect::<Vec<_>>(), vec![1]);
        assert_eq!(s.queued_tasks().unwrap().iter().map(|t| t.id).collect::<Vec<_>>(), vec![c.id]);
        assert_eq!(s.find_by_pane("pi-3", "w1:p1").unwrap().unwrap().id, 1);
        assert!(s.find_by_pane("pi-3", "w2:p1").unwrap().is_none());
    }

    #[test]
    fn open_on_disk_twice_keeps_data() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.db");
        { Store::open(&path).unwrap().insert_task(new_task("run")).unwrap(); }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.list_tasks(&TaskFilter::default()).unwrap().len(), 1);
    }
}
```

Add `pub mod task; pub mod store;` to `src/lib.rs`.

- [ ] **Step 6: Run to see failures**

Run: `cargo test store::`
Expected: 3 panics with `not yet implemented`.

- [ ] **Step 7: Implement the store**

```rust
    fn init(conn: Connection) -> anyhow::Result<Store> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS tasks (
                id INTEGER PRIMARY KEY,
                job TEXT NOT NULL,
                item TEXT NOT NULL,
                prompt TEXT NOT NULL,
                spec TEXT NOT NULL,
                machine TEXT,
                workspace_id TEXT,
                pane_id TEXT,
                agent_name TEXT,
                state TEXT NOT NULL,
                error TEXT,
                last_completion_seq INTEGER,
                created_at TEXT NOT NULL,
                started_at TEXT,
                finished_at TEXT,
                updated_at TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS tasks_state ON tasks(state);
             CREATE INDEX IF NOT EXISTS tasks_machine ON tasks(machine);",
        )?;
        let version: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get(0))
            .optional()?;
        match version.map(|v| v.parse::<i64>().unwrap_or(0)) {
            None => {
                conn.execute("INSERT INTO meta (key, value) VALUES ('schema_version', ?1)", params![SCHEMA_VERSION.to_string()])?;
            }
            Some(v) if v == SCHEMA_VERSION => {}
            Some(v) if v < SCHEMA_VERSION => {
                // Future migrations go here, one `if v < N` block each.
                conn.execute("UPDATE meta SET value = ?1 WHERE key = 'schema_version'", params![SCHEMA_VERSION.to_string()])?;
            }
            Some(v) => anyhow::bail!("database schema {v} is newer than this pastor ({SCHEMA_VERSION}); refusing to touch it"),
        }
        Ok(Store { conn: Mutex::new(conn) })
    }

    pub fn insert_task(&self, t: NewTask) -> anyhow::Result<Task> {
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO tasks (job, item, prompt, spec, state, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, 'queued', ?5, ?5)",
            params![t.job, serde_json::to_string(&t.item)?, t.prompt, serde_json::to_string(&t.spec)?, now],
        )?;
        let id = conn.last_insert_rowid();
        drop(conn);
        self.get_task(id)?.context("task vanished after insert")
    }

    pub fn get_task(&self, id: i64) -> anyhow::Result<Option<Task>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row("SELECT * FROM tasks WHERE id = ?1", params![id], row_to_task).optional()?)
    }

    pub fn update_task(&self, t: &Task) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE tasks SET machine = ?2, workspace_id = ?3, pane_id = ?4, agent_name = ?5, state = ?6, error = ?7,
                last_completion_seq = ?8, started_at = ?9, finished_at = ?10, updated_at = ?11, prompt = ?12, spec = ?13
             WHERE id = ?1",
            params![
                t.id, t.machine, t.workspace_id, t.pane_id, t.agent_name, t.state.as_str(), t.error,
                t.last_completion_seq.map(|v| v as i64), t.started_at.map(|d| d.to_rfc3339()), t.finished_at.map(|d| d.to_rfc3339()),
                Utc::now().to_rfc3339(), t.prompt, serde_json::to_string(&t.spec)?,
            ],
        )?;
        anyhow::ensure!(n == 1, "task {} not found", t.id);
        Ok(())
    }

    pub fn list_tasks(&self, f: &TaskFilter) -> anyhow::Result<Vec<Task>> {
        let mut sql = String::from("SELECT * FROM tasks WHERE 1=1");
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(job) = &f.job {
            args.push(Box::new(job.clone()));
            sql.push_str(&format!(" AND job = ?{}", args.len()));
        }
        if let Some(m) = &f.machine {
            args.push(Box::new(m.clone()));
            sql.push_str(&format!(" AND machine = ?{}", args.len()));
        }
        if let Some(states) = &f.states {
            let placeholders: Vec<String> = states.iter().map(|s| { args.push(Box::new(s.as_str().to_string())); format!("?{}", args.len()) }).collect();
            sql.push_str(&format!(" AND state IN ({})", placeholders.join(",")));
        }
        sql.push_str(" ORDER BY id DESC");
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter().map(|a| a.as_ref())), row_to_task)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
```

- [ ] **Step 8: Run tests**

Run: `cargo test store:: task::`
Expected: 8 passed.

- [ ] **Step 9: Commit**

```bash
git add src/task.rs src/store.rs src/lib.rs
git commit -m "feat: task model, state transitions and sqlite store"
```

---

### Task 8: Pastor config file

**Files:**
- Modify: `src/config/mod.rs`

**Interfaces:**
- Produces:
  ```rust
  pub struct Defaults { pub agent: String, pub max_tasks_per_run: u32, pub timeout: String }
  pub struct PastorConfig { pub tick: String, pub settle: String, pub reconcile_every: String, pub defaults: Defaults }
  impl PastorConfig { pub fn load(path: &Path) -> anyhow::Result<PastorConfig>; pub fn tick_duration(&self) -> Duration; pub fn settle_duration(&self) -> Duration; pub fn reconcile_duration(&self) -> Duration; pub fn timeout_duration(&self) -> Duration }
  pub fn parse_duration(s: &str) -> Result<Duration, String>   // "30s", "5m", "2h", "1d"
  impl Paths { pub fn config_file(&self) -> PathBuf }
  ```

- [ ] **Step 1: Write the failing tests**

Append to `src/config/mod.rs` (above the existing tests module; merge the test modules into one):

```rust
use std::time::Duration;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Defaults {
    pub agent: String,
    pub max_tasks_per_run: u32,
    pub timeout: String,
}

impl Default for Defaults {
    fn default() -> Self { Defaults { agent: "claude".into(), max_tasks_per_run: 5, timeout: "2h".into() } }
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
        PastorConfig { tick: "10s".into(), settle: "10s".into(), reconcile_every: "60s".into(), defaults: Defaults::default() }
    }
}

impl PastorConfig {
    pub fn load(path: &Path) -> anyhow::Result<PastorConfig> {
        if !path.exists() {
            return Ok(PastorConfig::default());
        }
        let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let cfg: PastorConfig = toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        for (name, v) in [("tick", &cfg.tick), ("settle", &cfg.settle), ("reconcile_every", &cfg.reconcile_every), ("defaults.timeout", &cfg.defaults.timeout)] {
            parse_duration(v).map_err(|e| anyhow::anyhow!("{}: {name}: {e}", path.display()))?;
        }
        Ok(cfg)
    }
    pub fn tick_duration(&self) -> Duration { parse_duration(&self.tick).unwrap_or(Duration::from_secs(10)) }
    pub fn settle_duration(&self) -> Duration { parse_duration(&self.settle).unwrap_or(Duration::from_secs(10)) }
    pub fn reconcile_duration(&self) -> Duration { parse_duration(&self.reconcile_every).unwrap_or(Duration::from_secs(60)) }
    pub fn timeout_duration(&self) -> Duration { parse_duration(&self.defaults.timeout).unwrap_or(Duration::from_secs(7200)) }
}

/// "30s", "5m", "2h", "1d". No spaces, one unit.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).ok_or_else(|| format!("{s:?}: missing unit"))?);
    let n: u64 = num.parse().map_err(|_| format!("{s:?}: bad number"))?;
    let mult = match unit { "s" => 1, "m" => 60, "h" => 3600, "d" => 86400, _ => return Err(format!("{s:?}: unit must be s, m, h or d")) };
    Ok(Duration::from_secs(n * mult))
}
```

Add to `impl Paths`: `pub fn config_file(&self) -> PathBuf { self.config_dir.join("pastor.toml") }`.

Add tests inside the existing `mod tests`:

```rust
    #[test]
    fn durations() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("1d").unwrap(), Duration::from_secs(86400));
        assert!(parse_duration("5").is_err());
        assert!(parse_duration("5 m").is_err());
        assert!(parse_duration("x").is_err());
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
        assert!(PastorConfig::load(&path).unwrap_err().to_string().contains("settle"));
    }
```

- [ ] **Step 2: Run tests**

Run: `cargo test config::`
Expected: all pass (this task's code is complete as written; the tests confirm the serde defaults behave).

- [ ] **Step 3: Commit**

```bash
git add src/config
git commit -m "feat: pastor.toml with durations and defaults"
```

---

### Task 9: Dispatch

**Files:**
- Create: `src/dispatch.rs`
- Modify: `src/lib.rs` (add `pub mod dispatch;`)

**Interfaces:**
- Produces:
  ```rust
  pub struct MachineView { pub name: String, pub max_agents: u32, pub tags: Vec<String>, pub live: usize, pub healthy: bool }
  pub fn pick_machine(machines: &[MachineView], spec: &DispatchSpec) -> Option<String>
  pub enum DispatchOutcome { Running, Blocked }
  pub async fn dispatch(conn: &mut Connection, task: &mut Task) -> Result<DispatchOutcome, HerdrError>
  ```
  `dispatch` mutates `task`: sets `agent_name`, `workspace_id`, `pane_id`, `started_at`, and `state` to `Running`, `Blocked` (on `agent_not_ready`) or `Failed` with `error` set. It never touches the store.

- [ ] **Step 1: Write the failing tests**

`src/dispatch.rs`:

```rust
use chrono::Utc;

use crate::herdr::{Connection, HerdrError};
use crate::task::{DispatchSpec, Task, TaskState};

#[derive(Debug, Clone)]
pub struct MachineView {
    pub name: String,
    pub max_agents: u32,
    pub tags: Vec<String>,
    pub live: usize,
    pub healthy: bool,
}

/// Pinned machine wins. Otherwise: healthy, has every required tag, below capacity,
/// fewest live tasks. Ties keep flock order.
pub fn pick_machine(machines: &[MachineView], spec: &DispatchSpec) -> Option<String> {
    todo!()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchOutcome { Running, Blocked }

pub async fn dispatch(conn: &mut Connection, task: &mut Task) -> Result<DispatchOutcome, HerdrError> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::fake::{FakeHerdr, StartBehaviour};
    use crate::herdr::AgentStatus;
    use serde_json::Value;

    fn mv(name: &str, max: u32, live: usize, tags: &[&str], healthy: bool) -> MachineView {
        MachineView { name: name.into(), max_agents: max, tags: tags.iter().map(|s| s.to_string()).collect(), live, healthy }
    }

    fn spec() -> DispatchSpec {
        DispatchSpec { agent: "claude".into(), agent_args: vec!["--model".into(), "opus".into()], repo: Some("/srv/app".into()), worktree: false, branch: None, machine: None, tags: vec![], timeout_secs: 60 }
    }

    fn task(spec: DispatchSpec) -> Task {
        let now = Utc::now();
        Task { id: 7, job: "run".into(), item: Value::Null, prompt: "line one\n\"two\" {{ three }}".into(), spec, machine: Some("pi-1".into()), workspace_id: None, pane_id: None, agent_name: None, state: TaskState::Queued, error: None, last_completion_seq: None, created_at: now, started_at: None, finished_at: None, updated_at: now }
    }

    #[test]
    fn pick_machine_respects_capacity() {
        let ms = vec![mv("a", 1, 1, &[], true), mv("b", 2, 1, &[], true), mv("c", 2, 0, &[], true)];
        assert_eq!(pick_machine(&ms, &spec()).as_deref(), Some("c"));
        let full = vec![mv("a", 1, 1, &[], true)];
        assert_eq!(pick_machine(&full, &spec()), None);
    }

    #[test]
    fn pick_machine_honours_pin_tags_and_health() {
        let ms = vec![mv("a", 2, 0, &["fast"], true), mv("b", 2, 0, &[], false), mv("c", 2, 0, &["fast", "gpu"], true)];
        assert_eq!(pick_machine(&ms, &DispatchSpec { machine: Some("c".into()), ..spec() }).as_deref(), Some("c"));
        assert_eq!(pick_machine(&ms, &DispatchSpec { machine: Some("b".into()), ..spec() }), None, "pinned but unhealthy");
        assert_eq!(pick_machine(&ms, &DispatchSpec { machine: Some("zzz".into()), ..spec() }), None, "pinned but unknown");
        assert_eq!(pick_machine(&ms, &DispatchSpec { tags: vec!["gpu".into()], ..spec() }).as_deref(), Some("c"));
        assert_eq!(pick_machine(&ms, &DispatchSpec { tags: vec!["fast".into()], ..spec() }).as_deref(), Some("a"), "flock order breaks ties");
        assert_eq!(pick_machine(&ms, &DispatchSpec { tags: vec!["nope".into()], ..spec() }), None);
    }

    #[tokio::test]
    async fn dispatch_sends_prompt_verbatim() {
        let fake = FakeHerdr::new();
        let mut c = fake.connect();
        let mut t = task(spec());
        let out = dispatch(&mut c, &mut t).await.unwrap();
        assert_eq!(out, DispatchOutcome::Running);
        assert_eq!(t.state, TaskState::Running);
        assert_eq!(t.agent_name.as_deref(), Some("t-7"));
        assert_eq!(t.pane_id.as_deref(), Some("w1:p1"));
        assert_eq!(t.workspace_id.as_deref(), Some("w1"));
        assert!(t.started_at.is_some());
        let reqs = fake.requests();
        let ws = reqs.iter().find(|r| r.method == "workspace.create").unwrap();
        assert_eq!(ws.params["cwd"], "/srv/app");
        assert_eq!(ws.params["label"], "t-7");
        let start = reqs.iter().find(|r| r.method == "agent.start").unwrap();
        assert_eq!(start.params["kind"], "claude");
        assert_eq!(start.params["args"], serde_json::json!(["--model", "opus"]));
        let prompt = reqs.iter().find(|r| r.method == "agent.prompt").unwrap();
        assert_eq!(prompt.params["target"], "t-7");
        assert_eq!(prompt.params["text"], "line one\n\"two\" {{ three }}");
    }

    #[tokio::test]
    async fn dispatch_uses_worktree_when_asked() {
        let fake = FakeHerdr::new();
        let mut c = fake.connect();
        let mut t = task(DispatchSpec { worktree: true, branch: Some("pastor/k1".into()), ..spec() });
        dispatch(&mut c, &mut t).await.unwrap();
        let wt = fake.requests().into_iter().find(|r| r.method == "worktree.create").unwrap();
        assert_eq!(wt.params["cwd"], "/srv/app");
        assert_eq!(wt.params["branch"], "pastor/k1");
        assert_eq!(wt.params["label"], "t-7");
    }

    #[tokio::test]
    async fn not_ready_is_blocked_and_failures_are_failed() {
        let fake = FakeHerdr::new();
        fake.set_start_behaviour(StartBehaviour::NotReady);
        let mut c = fake.connect();
        let mut t = task(spec());
        assert_eq!(dispatch(&mut c, &mut t).await.unwrap(), DispatchOutcome::Blocked);
        assert_eq!(t.state, TaskState::Blocked);
        assert!(t.pane_id.is_some(), "pane is kept for inspection");
        assert!(!fake.requests().iter().any(|r| r.method == "agent.prompt"));

        fake.set_start_behaviour(StartBehaviour::Fail("unsupported_agent_kind".into()));
        let mut t = task(spec());
        let err = dispatch(&mut c, &mut t).await.unwrap_err();
        assert_eq!(err.code(), Some("unsupported_agent_kind"));
        assert_eq!(t.state, TaskState::Failed);
        assert!(t.error.as_deref().unwrap().contains("unsupported_agent_kind"));
        assert!(t.workspace_id.is_some(), "created workspace is recorded even on failure");
    }

    #[tokio::test]
    async fn worktree_without_repo_fails_before_calling_herdr() {
        let fake = FakeHerdr::new();
        let mut c = fake.connect();
        let mut t = task(DispatchSpec { worktree: true, repo: None, ..spec() });
        let err = dispatch(&mut c, &mut t).await.unwrap_err();
        assert!(matches!(err, HerdrError::Protocol(_)));
        assert_eq!(t.state, TaskState::Failed);
        assert!(fake.requests().is_empty());
        let _ = AgentStatus::Idle;
    }
}
```

Add `pub mod dispatch;` to `src/lib.rs`.

- [ ] **Step 2: Run to see failures**

Run: `cargo test dispatch::`
Expected: 6 panics with `not yet implemented`.

- [ ] **Step 3: Implement**

```rust
pub fn pick_machine(machines: &[MachineView], spec: &DispatchSpec) -> Option<String> {
    let fits = |m: &MachineView| m.healthy && (m.live as u64) < m.max_agents as u64 && spec.tags.iter().all(|t| m.tags.contains(t));
    if let Some(pinned) = &spec.machine {
        return machines.iter().find(|m| &m.name == pinned).filter(|m| fits(m)).map(|m| m.name.clone());
    }
    machines.iter().filter(|m| fits(m)).min_by_key(|m| m.live).map(|m| m.name.clone())
}

pub async fn dispatch(conn: &mut Connection, task: &mut Task) -> Result<DispatchOutcome, HerdrError> {
    let name = Task::agent_name_for(task.id);
    task.agent_name = Some(name.clone());
    task.state = TaskState::Starting;
    task.error = None;

    let result = dispatch_steps(conn, task, &name).await;
    match &result {
        Ok(DispatchOutcome::Running) => {
            task.state = TaskState::Running;
            task.started_at = Some(Utc::now());
        }
        Ok(DispatchOutcome::Blocked) => {
            task.state = TaskState::Blocked;
            task.started_at = Some(Utc::now());
            task.error = Some("agent blocked during startup; answer its prompt".into());
        }
        Err(err) => {
            task.state = TaskState::Failed;
            task.error = Some(err.to_string());
            task.finished_at = Some(Utc::now());
        }
    }
    result
}

async fn dispatch_steps(conn: &mut Connection, task: &mut Task, name: &str) -> Result<DispatchOutcome, HerdrError> {
    let spec = task.spec.clone();
    let created = if spec.worktree {
        let repo = spec.repo.as_deref().ok_or_else(|| HerdrError::Protocol("worktree = true needs repo".into()))?;
        let branch = spec.branch.clone().unwrap_or_else(|| format!("pastor/{name}"));
        conn.worktree_create(repo, &branch, name).await?
    } else {
        conn.workspace_create(spec.repo.as_deref(), name).await?
    };
    task.workspace_id = Some(created.workspace.workspace_id.clone());
    task.pane_id = Some(created.root_pane.pane_id.clone());

    match conn.agent_start(name, &spec.agent, &created.root_pane.pane_id, &spec.agent_args).await {
        Ok(_) => {}
        Err(err) if err.code() == Some("agent_not_ready") => return Ok(DispatchOutcome::Blocked),
        Err(err) => return Err(err),
    }
    conn.agent_prompt(name, &task.prompt).await?;
    Ok(DispatchOutcome::Running)
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test dispatch::`
Expected: 6 passed.

- [ ] **Step 5: Commit**

```bash
git add src/dispatch.rs src/lib.rs
git commit -m "feat: machine selection and dispatch steps"
```

---

### Task 10: Machine actor

**Files:**
- Create: `src/machine.rs`
- Modify: `src/herdr/transport.rs` (add the `Connector` trait), `src/herdr/fake.rs` (impl `Connector`), `src/lib.rs` (add `pub mod machine;`)

**Interfaces:**
- Consumes: `Store`, `Task`, `next_state`, `dispatch`, `Connection`, `EventStream`, `FakeHerdr`.
- Produces:
  ```rust
  // transport.rs
  pub type ConnectFuture<'a> = Pin<Box<dyn Future<Output = Result<Connection, ConnectError>> + Send + 'a>>;
  pub trait Connector: Send + Sync { fn connect(&self) -> ConnectFuture<'_>; fn describe(&self) -> String; }
  impl Connector for Endpoint
  // fake.rs
  impl Connector for FakeHerdr
  // machine.rs
  pub enum ChannelState { Connecting, Connected, Reconnecting, Incompatible }   // serde lowercase, Display
  pub struct MachineStatus { pub name: String, pub endpoint: String, pub channel: ChannelState, pub herdr_version: Option<String>, pub protocol: Option<u32>, pub error: Option<String>, pub live: usize, pub max_agents: u32, pub tags: Vec<String> }
  pub struct MachineSettings { pub settle: Duration, pub reconcile_every: Duration, pub initial_backoff: Duration, pub max_backoff: Duration }
  impl Default for MachineSettings  // 10s, 60s, 1s, 60s
  pub struct PastorEvent { pub kind: String, pub task_id: Option<i64>, pub machine: String }
  pub enum MachineCommand { Dispatch { task_id: i64, reply: oneshot::Sender<anyhow::Result<Task>> }, Read { task_id: i64, lines: u32, reply: oneshot::Sender<anyhow::Result<String>> } }
  pub struct MachineHandle { pub name: String, pub max_agents: u32, pub tags: Vec<String>, pub tx: mpsc::Sender<MachineCommand>, pub status: Arc<RwLock<MachineStatus>> }
  impl MachineHandle { pub fn snapshot(&self) -> MachineStatus; pub async fn dispatch(&self, task_id: i64) -> anyhow::Result<Task>; pub async fn read(&self, task_id: i64, lines: u32) -> anyhow::Result<String> }
  pub fn spawn_machine(name: String, max_agents: u32, tags: Vec<String>, connector: Arc<dyn Connector>, store: Arc<Store>, settings: MachineSettings, events: broadcast::Sender<PastorEvent>) -> MachineHandle
  ```

- [ ] **Step 1: Add the Connector trait**

In `src/herdr/transport.rs` add:

```rust
use std::future::Future;
use std::pin::Pin;

pub type ConnectFuture<'a> = Pin<Box<dyn Future<Output = Result<Connection, ConnectError>> + Send + 'a>>;

/// Anything that can open a fresh herdr connection. Endpoints for real use, FakeHerdr in tests.
pub trait Connector: Send + Sync {
    fn connect(&self) -> ConnectFuture<'_>;
    fn describe(&self) -> String;
}

impl Connector for Endpoint {
    fn connect(&self) -> ConnectFuture<'_> { Box::pin(connect(self)) }
    fn describe(&self) -> String { Endpoint::describe(self) }
}
```

In `src/herdr/fake.rs` add:

```rust
impl super::transport::Connector for FakeHerdr {
    fn connect(&self) -> super::transport::ConnectFuture<'_> {
        Box::pin(async move { Ok(FakeHerdr::connect(self)) })
    }
    fn describe(&self) -> String { "fake herdr".into() }
}
```

Run: `cargo build` — Expected: compiles.

- [ ] **Step 2: Write the failing tests**

`src/machine.rs`:

```rust
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::dispatch::dispatch;
use crate::herdr::{subscription_agent_status, subscription_lifecycle, AgentInfo, Connection, Connector, EventStream};
use crate::store::Store;
use crate::task::{next_state, Observed, Task, TaskState};
use crate::MIN_HERDR_PROTOCOL;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelState { Connecting, Connected, Reconnecting, Incompatible }

impl std::fmt::Display for ChannelState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self { ChannelState::Connecting => "connecting", ChannelState::Connected => "connected", ChannelState::Reconnecting => "reconnecting", ChannelState::Incompatible => "incompatible" })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachineStatus {
    pub name: String,
    pub endpoint: String,
    pub channel: ChannelState,
    pub herdr_version: Option<String>,
    pub protocol: Option<u32>,
    pub error: Option<String>,
    pub live: usize,
    pub max_agents: u32,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct MachineSettings {
    pub settle: Duration,
    pub reconcile_every: Duration,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for MachineSettings {
    fn default() -> Self {
        MachineSettings { settle: Duration::from_secs(10), reconcile_every: Duration::from_secs(60), initial_backoff: Duration::from_secs(1), max_backoff: Duration::from_secs(60) }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PastorEvent {
    pub kind: String,
    pub task_id: Option<i64>,
    pub machine: String,
}

pub enum MachineCommand {
    Dispatch { task_id: i64, reply: oneshot::Sender<anyhow::Result<Task>> },
    Read { task_id: i64, lines: u32, reply: oneshot::Sender<anyhow::Result<String>> },
}

#[derive(Clone)]
pub struct MachineHandle {
    pub name: String,
    pub max_agents: u32,
    pub tags: Vec<String>,
    pub tx: mpsc::Sender<MachineCommand>,
    pub status: Arc<RwLock<MachineStatus>>,
}

impl MachineHandle {
    pub fn snapshot(&self) -> MachineStatus { self.status.read().unwrap().clone() }

    pub async fn dispatch(&self, task_id: i64) -> anyhow::Result<Task> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(MachineCommand::Dispatch { task_id, reply }).await.map_err(|_| anyhow::anyhow!("machine {} is gone", self.name))?;
        rx.await.map_err(|_| anyhow::anyhow!("machine {} dropped the request", self.name))?
    }

    pub async fn read(&self, task_id: i64, lines: u32) -> anyhow::Result<String> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(MachineCommand::Read { task_id, lines, reply }).await.map_err(|_| anyhow::anyhow!("machine {} is gone", self.name))?;
        rx.await.map_err(|_| anyhow::anyhow!("machine {} dropped the request", self.name))?
    }
}

pub fn spawn_machine(
    name: String,
    max_agents: u32,
    tags: Vec<String>,
    connector: Arc<dyn Connector>,
    store: Arc<Store>,
    settings: MachineSettings,
    events: broadcast::Sender<PastorEvent>,
) -> MachineHandle {
    let (tx, rx) = mpsc::channel(32);
    let status = Arc::new(RwLock::new(MachineStatus {
        name: name.clone(), endpoint: connector.describe(), channel: ChannelState::Connecting, herdr_version: None, protocol: None, error: None, live: 0, max_agents, tags: tags.clone(),
    }));
    let actor = Actor { name: name.clone(), connector, store, settings, events, status: status.clone(), rx, pending_done: HashMap::new(), was_connected: false, failures: 0 };
    tokio::spawn(actor.run());
    MachineHandle { name, max_agents, tags, tx, status }
}

struct Actor {
    name: String,
    connector: Arc<dyn Connector>,
    store: Arc<Store>,
    settings: MachineSettings,
    events: broadcast::Sender<PastorEvent>,
    status: Arc<RwLock<MachineStatus>>,
    rx: mpsc::Receiver<MachineCommand>,
    /// task id -> (completion_seq observed, when). Confirmed as Done after `settle`.
    pending_done: HashMap<i64, (Option<u64>, Instant)>,
    was_connected: bool,
    failures: u32,
}

impl Actor {
    async fn run(mut self) { todo!() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::fake::FakeHerdr;
    use crate::herdr::{AgentStatus, ConnectError, ConnectFuture};
    use crate::store::NewTask;
    use crate::task::DispatchSpec;

    fn settings() -> MachineSettings {
        MachineSettings { settle: Duration::from_millis(100), reconcile_every: Duration::from_millis(200), initial_backoff: Duration::from_millis(50), max_backoff: Duration::from_millis(200) }
    }

    fn spec() -> DispatchSpec {
        DispatchSpec { agent: "claude".into(), agent_args: vec![], repo: None, worktree: false, branch: None, machine: None, tags: vec![], timeout_secs: 3600 }
    }

    fn new_task(store: &Store) -> Task {
        store.insert_task(NewTask { job: "run".into(), item: serde_json::Value::Null, prompt: "hi".into(), spec: spec() }).unwrap()
    }

    async fn wait_for<F: Fn() -> bool>(what: &str, f: F) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !f() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn spawn(fake: &FakeHerdr, store: &Arc<Store>) -> (MachineHandle, broadcast::Receiver<PastorEvent>) {
        let (events, rx) = broadcast::channel(64);
        let h = spawn_machine("m".into(), 2, vec![], Arc::new(fake.clone()), store.clone(), settings(), events);
        (h, rx)
    }

    fn state_of(store: &Store, id: i64) -> TaskState { store.get_task(id).unwrap().unwrap().state }

    #[tokio::test]
    async fn dispatch_then_events_drive_state() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = spawn(&fake, &store);
        wait_for("connected", || h.snapshot().channel == ChannelState::Connected).await;
        let t = new_task(&store);
        let t = h.dispatch(t.id).await.unwrap();
        assert_eq!(t.state, TaskState::Running);
        assert_eq!(h.snapshot().live, 1);
        let pane = t.pane_id.clone().unwrap();

        fake.set_status(&pane, AgentStatus::Blocked, None);
        wait_for("blocked", || state_of(&store, t.id) == TaskState::Blocked).await;
        let ev = loop {
            let ev = tokio::time::timeout(Duration::from_secs(2), events.recv()).await.unwrap().unwrap();
            if ev.kind == "task.blocked" { break ev }
            assert_eq!(ev.kind, "task.running", "only the dispatch event may precede task.blocked");
        };
        assert_eq!(ev.task_id, Some(t.id));

        fake.set_status(&pane, AgentStatus::Working, None);
        wait_for("running", || state_of(&store, t.id) == TaskState::Running).await;

        fake.set_status(&pane, AgentStatus::Idle, Some(1));
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Running, "done waits for the settle window");
        wait_for("done", || state_of(&store, t.id) == TaskState::Done).await;
        assert_eq!(store.get_task(t.id).unwrap().unwrap().last_completion_seq, Some(1));

        fake.close_pane(&pane);
        wait_for("closed", || state_of(&store, t.id) == TaskState::Closed).await;
        wait_for("live 0", || h.snapshot().live == 0).await;
        let _ = h.read(t.id, 10).await.unwrap_err();
    }

    #[tokio::test]
    async fn done_is_cancelled_if_agent_resumes_within_settle() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || h.snapshot().channel == ChannelState::Connected).await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        let pane = t.pane_id.clone().unwrap();
        fake.set_status(&pane, AgentStatus::Idle, Some(1));
        tokio::time::sleep(Duration::from_millis(30)).await;
        fake.set_status(&pane, AgentStatus::Working, Some(1));
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(state_of(&store, t.id), TaskState::Running);
    }

    #[tokio::test]
    async fn reconcile_marks_missing_agents_failed() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut t = new_task(&store);
        t.state = TaskState::Running; t.machine = Some("m".into()); t.pane_id = Some("w9:p1".into()); t.agent_name = Some("t-1".into());
        store.update_task(&t).unwrap();
        let (_h, _events) = spawn(&fake, &store);
        wait_for("failed", || state_of(&store, t.id) == TaskState::Failed).await;
        assert!(store.get_task(t.id).unwrap().unwrap().error.unwrap().contains("not found on machine"));
    }

    #[tokio::test]
    async fn reconcile_adopts_live_agent_state() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut c = fake.connect();
        let created = c.workspace_create(None, "t-1").await.unwrap();
        c.agent_start("t-1", "claude", &created.root_pane.pane_id, &[]).await.unwrap();
        fake.set_status(&created.root_pane.pane_id, AgentStatus::Blocked, None);
        let mut t = new_task(&store);
        t.state = TaskState::Starting; t.machine = Some("m".into()); t.pane_id = Some(created.root_pane.pane_id.clone()); t.agent_name = Some("t-1".into());
        store.update_task(&t).unwrap();
        let (_h, _events) = spawn(&fake, &store);
        wait_for("blocked", || state_of(&store, t.id) == TaskState::Blocked).await;
    }

    struct Refusing;
    impl Connector for Refusing {
        fn connect(&self) -> ConnectFuture<'_> { Box::pin(async { Err(ConnectError { message: "ssh: permission denied (255)".into() }) }) }
        fn describe(&self) -> String { "refusing".into() }
    }

    #[tokio::test]
    async fn machine_reports_reconnecting_with_error() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (events, mut rx) = broadcast::channel(64);
        let h = spawn_machine("m".into(), 2, vec![], Arc::new(Refusing), store, settings(), events);
        wait_for("reconnecting", || h.snapshot().channel == ChannelState::Reconnecting).await;
        assert!(h.snapshot().error.unwrap().contains("permission denied"));
        let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.unwrap().unwrap();
        assert_eq!(ev.kind, "machine.lost");
        assert_eq!(h.snapshot().live, 0);
    }

    #[tokio::test]
    async fn disconnect_triggers_reconnect_and_resubscribe() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, mut events) = spawn(&fake, &store);
        wait_for("connected", || h.snapshot().channel == ChannelState::Connected).await;
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        fake.disconnect_all();
        // The reconnecting state is too brief to observe reliably; the event proves the cycle.
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(5), events.recv()).await.expect("machine.connected within 5s").unwrap();
            if ev.kind == "machine.connected" { break }
        }
        assert_eq!(h.snapshot().channel, ChannelState::Connected);
        fake.set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Blocked, None);
        wait_for("blocked after resubscribe", || state_of(&store, t.id) == TaskState::Blocked).await;
    }

    #[tokio::test]
    async fn incompatible_protocol_is_reported_and_not_dispatched_to() {
        let fake = FakeHerdr::new();
        fake.set_protocol(20);
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("incompatible", || h.snapshot().channel == ChannelState::Incompatible).await;
        assert_eq!(h.snapshot().protocol, Some(20));
        let err = h.dispatch(new_task(&store).id).await.unwrap_err();
        assert!(err.to_string().contains("not connected"), "{err}");
    }

    #[tokio::test]
    async fn stale_after_timeout() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || h.snapshot().channel == ChannelState::Connected).await;
        let t = store.insert_task(NewTask { job: "run".into(), item: serde_json::Value::Null, prompt: "hi".into(), spec: DispatchSpec { timeout_secs: 1, ..spec() } }).unwrap();
        let t = h.dispatch(t.id).await.unwrap();
        assert_eq!(t.state, TaskState::Running);
        wait_for("stale", || state_of(&store, t.id) == TaskState::Stale).await;
        assert_eq!(fake.agents().len(), 1, "nothing was killed");
    }
}
```

Add `pub mod machine;` to `src/lib.rs`.

- [ ] **Step 3: Run to see failures**

Run: `cargo test machine::`
Expected: 8 failures (`not yet implemented` panics inside the spawned actor, so the tests fail on timeouts from `wait_for`).

- [ ] **Step 4: Implement the actor**

Replace `impl Actor { async fn run(mut self) { todo!() } }`:

```rust
impl Actor {
    async fn run(mut self) {
        let mut backoff = self.settings.initial_backoff;
        loop {
            self.set_channel(ChannelState::Connecting, None);
            let mut req = match self.connector.connect().await {
                Ok(c) => c,
                Err(err) => { self.connect_failed(err.message, &mut backoff).await; continue; }
            };
            let pong = match req.ping().await {
                Ok(p) => p,
                Err(err) => { self.connect_failed(err.to_string(), &mut backoff).await; continue; }
            };
            {
                let mut s = self.status.write().unwrap();
                s.herdr_version = Some(pong.version.clone());
                s.protocol = Some(pong.protocol);
            }
            if pong.protocol < MIN_HERDR_PROTOCOL {
                self.set_channel(ChannelState::Incompatible, Some(format!("herdr protocol {} is older than {MIN_HERDR_PROTOCOL}; update herdr on this machine", pong.protocol)));
                self.drain_commands_while_down(self.settings.max_backoff).await;
                continue;
            }
            if let Err(err) = self.reconcile(&mut req).await {
                self.connect_failed(format!("reconcile: {err}"), &mut backoff).await;
                continue;
            }
            let mut events = match self.open_events().await {
                Ok(s) => s,
                Err(err) => { self.connect_failed(format!("events: {err}"), &mut backoff).await; continue; }
            };
            backoff = self.settings.initial_backoff;
            self.failures = 0;
            self.set_channel(ChannelState::Connected, None);
            if self.was_connected {
                self.emit("machine.connected", None);
            }
            self.was_connected = true;
            self.refresh_live();

            let mut settle_tick = tokio::time::interval(Duration::from_millis(50).max(self.settings.settle / 4));
            let mut reconcile_tick = tokio::time::interval(self.settings.reconcile_every);
            reconcile_tick.tick().await; // first tick fires immediately; we just reconciled
            loop {
                tokio::select! {
                    cmd = self.rx.recv() => {
                        let Some(cmd) = cmd else { return };
                        let resubscribe = self.handle_command(cmd, &mut req).await;
                        if resubscribe {
                            match self.open_events().await {
                                Ok(s) => events = s,
                                Err(err) => { tracing::warn!(machine = %self.name, %err, "resubscribe failed"); break; }
                            }
                        }
                    }
                    ev = events.next() => match ev {
                        Ok(ev) => self.handle_event(&ev),
                        Err(err) => { tracing::warn!(machine = %self.name, %err, "event stream ended"); break; }
                    },
                    _ = settle_tick.tick() => {
                        if let Err(err) = self.confirm_pending_done(&mut req).await { tracing::warn!(machine = %self.name, %err, "settle check failed"); break; }
                    }
                    _ = reconcile_tick.tick() => {
                        if let Err(err) = self.reconcile(&mut req).await { tracing::warn!(machine = %self.name, %err, "reconcile failed"); break; }
                    }
                }
            }
            self.set_channel(ChannelState::Reconnecting, Some("connection lost".into()));
        }
    }

    async fn connect_failed(&mut self, message: String, backoff: &mut Duration) {
        self.failures += 1;
        tracing::warn!(machine = %self.name, %message, "connect failed");
        self.set_channel(ChannelState::Reconnecting, Some(message));
        if self.was_connected || self.failures == 2 {
            self.emit("machine.lost", None);
            self.was_connected = false;
            self.failures = 0;
        }
        self.drain_commands_while_down(*backoff).await;
        *backoff = (*backoff * 2).min(self.settings.max_backoff);
    }

    /// While down, answer commands with an error instead of leaving callers hanging.
    async fn drain_commands_while_down(&mut self, wait: Duration) {
        let deadline = tokio::time::sleep(wait);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => return,
                cmd = self.rx.recv() => match cmd {
                    None => return,
                    Some(MachineCommand::Dispatch { reply, .. }) => { let _ = reply.send(Err(anyhow::anyhow!("machine {} is not connected", self.name))); }
                    Some(MachineCommand::Read { reply, .. }) => { let _ = reply.send(Err(anyhow::anyhow!("machine {} is not connected", self.name))); }
                },
            }
        }
    }

    fn set_channel(&self, channel: ChannelState, error: Option<String>) {
        let mut s = self.status.write().unwrap();
        s.channel = channel;
        s.error = error;
    }

    fn refresh_live(&self) {
        let live = self.store.tasks_on_machine(&self.name).map(|v| v.len()).unwrap_or(0);
        self.status.write().unwrap().live = live;
    }

    fn emit(&self, kind: &str, task_id: Option<i64>) {
        tracing::info!(machine = %self.name, kind, ?task_id, "event");
        let _ = self.events.send(PastorEvent { kind: kind.into(), task_id, machine: self.name.clone() });
    }

    async fn open_events(&self) -> Result<EventStream, anyhow::Error> {
        let conn = self.connector.connect().await?;
        let mut subs = vec![subscription_lifecycle("pane.closed"), subscription_lifecycle("pane.exited")];
        for t in self.store.tasks_on_machine(&self.name)? {
            if let Some(p) = &t.pane_id {
                subs.push(subscription_agent_status(p));
            }
        }
        Ok(conn.subscribe(subs).await?)
    }

    /// Returns true when the tracked pane set changed and the event subscription must be reopened.
    async fn handle_command(&mut self, cmd: MachineCommand, req: &mut Connection) -> bool {
        match cmd {
            MachineCommand::Dispatch { task_id, reply } => {
                let result = self.run_dispatch(task_id, req).await;
                let changed = result.is_ok();
                let _ = reply.send(result);
                self.refresh_live();
                changed
            }
            MachineCommand::Read { task_id, lines, reply } => {
                let result = match self.store.get_task(task_id) {
                    Ok(Some(t)) if t.machine.as_deref() == Some(&self.name) && t.state.occupies_pane() => {
                        req.agent_read(t.agent_name.as_deref().unwrap_or(""), lines).await.map_err(Into::into)
                    }
                    Ok(_) => Err(anyhow::anyhow!("task {task_id} has no live agent on {}", self.name)),
                    Err(e) => Err(e),
                };
                let _ = reply.send(result);
                false
            }
        }
    }

    async fn run_dispatch(&mut self, task_id: i64, req: &mut Connection) -> anyhow::Result<Task> {
        let mut task = self.store.get_task(task_id)?.ok_or_else(|| anyhow::anyhow!("task {task_id} not found"))?;
        anyhow::ensure!(task.state == TaskState::Queued, "task {} is {}, not queued", task.display_id(), task.state);
        task.machine = Some(self.name.clone());
        let outcome = dispatch(req, &mut task).await;
        self.store.update_task(&task)?;
        match outcome {
            Ok(_) => { self.emit(&format!("task.{}", task.state), Some(task.id)); Ok(task) }
            Err(err) => { self.emit("task.failed", Some(task.id)); Err(anyhow::anyhow!("dispatch {}: {err}", task.display_id())) }
        }
    }

    fn handle_event(&mut self, ev: &crate::herdr::Event) {
        let Some(pane_id) = ev.pane_id() else { return };
        let Ok(Some(task)) = self.store.find_by_pane(&self.name, pane_id) else { return };
        let observed = if ev.is_pane_closed() {
            Observed::PaneClosed
        } else if ev.is_pane_exited() {
            Observed::PaneExited
        } else if let Some(status) = ev.agent_status() {
            // Subscription events carry no completion_seq; treat idle as a candidate and let
            // the settle check read the real sequence from agent.list.
            Observed::Status { status, completion_seq: None }
        } else {
            return;
        };
        match &observed {
            Observed::Status { status: crate::herdr::AgentStatus::Idle | crate::herdr::AgentStatus::Done, .. } => {
                self.pending_done.insert(task.id, (task.last_completion_seq, Instant::now()));
            }
            _ => {
                self.pending_done.remove(&task.id);
                self.apply(task, &observed);
            }
        }
    }

    /// After the settle window, confirm with agent.list that the agent is still idle and
    /// its completion_seq advanced. Only then is the task done.
    async fn confirm_pending_done(&mut self, req: &mut Connection) -> anyhow::Result<()> {
        let due: Vec<i64> = self.pending_done.iter().filter(|(_, (_, at))| at.elapsed() >= self.settings.settle).map(|(id, _)| *id).collect();
        if due.is_empty() {
            return Ok(());
        }
        let agents = req.agent_list().await?;
        for id in due {
            self.pending_done.remove(&id);
            let Ok(Some(task)) = self.store.get_task(id) else { continue };
            let Some(agent) = agents.iter().find(|a| Some(&a.pane_id) == task.pane_id.as_ref()) else { continue };
            let observed = Observed::Status { status: agent.agent_status, completion_seq: agent.completion_seq };
            self.apply(task, &observed);
        }
        Ok(())
    }

    fn apply(&mut self, mut task: Task, observed: &Observed) {
        let Some(to) = next_state(&task, observed) else { return };
        if let Observed::Status { completion_seq: Some(seq), .. } = observed {
            if to == TaskState::Done { task.last_completion_seq = Some(*seq); }
        }
        task.state = to;
        if matches!(to, TaskState::Done | TaskState::Failed | TaskState::Closed) {
            task.finished_at = Some(Utc::now());
        }
        if to == TaskState::Failed && task.error.is_none() {
            task.error = Some("agent process exited".into());
        }
        if let Err(err) = self.store.update_task(&task) {
            tracing::error!(%err, "update task");
            return;
        }
        self.emit(&format!("task.{to}"), Some(task.id));
        self.refresh_live();
    }

    /// Compare open tasks with live agents. Missing agent means the task failed while we
    /// were away; a present agent's status is applied like an event, except idle, which
    /// goes through the settle window. Long-running tasks become stale.
    async fn reconcile(&mut self, req: &mut Connection) -> anyhow::Result<()> {
        let agents: Vec<AgentInfo> = req.agent_list().await?;
        for task in self.store.tasks_on_machine(&self.name)? {
            let Some(pane_id) = task.pane_id.clone() else { continue };
            match agents.iter().find(|a| a.pane_id == pane_id) {
                None => {
                    let mut t = task;
                    t.state = TaskState::Failed;
                    t.error = Some(format!("agent {} not found on machine {}", t.agent_name.clone().unwrap_or_default(), self.name));
                    t.finished_at = Some(Utc::now());
                    self.store.update_task(&t)?;
                    self.emit("task.failed", Some(t.id));
                }
                Some(agent) => {
                    let timed_out = task.started_at.map(|s| (Utc::now() - s).num_seconds() as u64 > task.spec.timeout_secs).unwrap_or(false);
                    if timed_out && matches!(task.state, TaskState::Running | TaskState::Blocked) {
                        let mut t = task;
                        t.state = TaskState::Stale;
                        self.store.update_task(&t)?;
                        self.emit("task.stale", Some(t.id));
                        continue;
                    }
                    if matches!(agent.agent_status, crate::herdr::AgentStatus::Idle | crate::herdr::AgentStatus::Done) {
                        // Never mark Done from a reconcile directly: the settle window applies here too.
                        self.pending_done.entry(task.id).or_insert((task.last_completion_seq, Instant::now()));
                        continue;
                    }
                    let observed = Observed::Status { status: agent.agent_status, completion_seq: agent.completion_seq };
                    self.apply(task, &observed);
                }
            }
        }
        self.refresh_live();
        Ok(())
    }
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test machine::`
Expected: 8 passed. If `dispatch_then_events_drive_state` reports Done too early, check that `handle_event` puts idle into `pending_done` rather than applying it. If `stale_after_timeout` is slow, it depends on `reconcile_every` (200ms in tests) plus `timeout_secs: 1`, so it needs about 1.2s.

- [ ] **Step 6: Commit**

```bash
git add src/machine.rs src/herdr src/lib.rs
git commit -m "feat: per-machine actor with events, reconcile and dispatch"
```

---

### Task 11: IPC and daemon

**Files:**
- Create: `src/ipc.rs`, `src/daemon.rs`
- Modify: `src/store.rs` (derive `Serialize, Deserialize` on `TaskFilter`), `src/lib.rs` (add `pub mod ipc; pub mod daemon;`)

**Interfaces:**
- Produces:
  ```rust
  // ipc.rs
  pub enum IpcRequest { Ping, Run { prompt: String, spec: DispatchSpec }, List { filter: TaskFilter }, TaskShow { id: i64 }, TaskRead { id: i64, lines: u32 }, FlockList }   // serde tag "op", snake_case
  pub enum IpcResponse { Pong { version: String }, Task(Task), Tasks(Vec<Task>), Text(String), Machines(Vec<MachineStatus>), Error { code: String, message: String } }   // serde tag "kind", snake_case
  pub async fn request(socket: &Path, req: &IpcRequest) -> anyhow::Result<IpcResponse>
  pub async fn daemon_running(socket: &Path) -> bool
  // daemon.rs
  pub struct Daemon { .. }
  impl Daemon {
      pub async fn start(paths: Paths, config: PastorConfig, flock: Flock, connectors: Option<Vec<Arc<dyn Connector>>>) -> anyhow::Result<Daemon>   // None: build endpoints from flock
      pub async fn run(self) -> anyhow::Result<()>            // accept loop + tick, until ctrl-c
      pub async fn handle(&self, req: IpcRequest) -> IpcResponse
      pub async fn dispatch_queued(&self)                      // one pass over queued tasks
      pub fn socket_path(&self) -> PathBuf
  }
  pub async fn serve(paths: Paths) -> anyhow::Result<()>     // load config + flock, start, run
  ```

- [ ] **Step 1: Write ipc.rs**

```rust
use std::path::Path;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::machine::MachineStatus;
use crate::store::TaskFilter;
use crate::task::{DispatchSpec, Task};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum IpcRequest {
    Ping,
    Run { prompt: String, spec: DispatchSpec },
    List { filter: TaskFilter },
    TaskShow { id: i64 },
    TaskRead { id: i64, lines: u32 },
    FlockList,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IpcResponse {
    Pong { version: String },
    Task(Task),
    Tasks(Vec<Task>),
    Text(String),
    Machines(Vec<MachineStatus>),
    Error { code: String, message: String },
}

impl IpcResponse {
    pub fn error(code: &str, message: impl std::fmt::Display) -> IpcResponse {
        IpcResponse::Error { code: code.into(), message: message.to_string() }
    }
}

/// One request, one reply, then the connection closes.
pub async fn request(socket: &Path, req: &IpcRequest) -> anyhow::Result<IpcResponse> {
    let stream = tokio::net::UnixStream::connect(socket).await?;
    let (r, mut w) = stream.into_split();
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
    w.flush().await?;
    let mut reply = String::new();
    BufReader::new(r).read_line(&mut reply).await?;
    anyhow::ensure!(!reply.trim().is_empty(), "daemon closed the connection without a reply");
    Ok(serde_json::from_str(reply.trim())?)
}

pub async fn daemon_running(socket: &Path) -> bool {
    matches!(request(socket, &IpcRequest::Ping).await, Ok(IpcResponse::Pong { .. }))
}
```

In `src/store.rs` change `TaskFilter`'s derive to `#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]`.

- [ ] **Step 2: Write the failing daemon tests**

`src/daemon.rs`:

```rust
use std::path::PathBuf;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::broadcast;

use crate::config::flock::Flock;
use crate::config::{Paths, PastorConfig};
use crate::dispatch::{pick_machine, MachineView};
use crate::herdr::{Connector, Endpoint};
use crate::ipc::{IpcRequest, IpcResponse};
use crate::machine::{spawn_machine, ChannelState, MachineHandle, MachineSettings, PastorEvent};
use crate::store::{NewTask, Store};

pub struct Daemon {
    paths: Paths,
    config: PastorConfig,
    store: Arc<Store>,
    machines: Vec<MachineHandle>,
    events: broadcast::Sender<PastorEvent>,
}

impl Daemon {
    pub async fn start(paths: Paths, config: PastorConfig, flock: Flock, connectors: Option<Vec<Arc<dyn Connector>>>) -> anyhow::Result<Daemon> {
        paths.ensure()?;
        let store = Arc::new(Store::open(&paths.db_file())?);
        let (events, _) = broadcast::channel(1024);
        let settings = MachineSettings { settle: config.settle_duration(), reconcile_every: config.reconcile_duration(), ..Default::default() };
        let connectors: Vec<Arc<dyn Connector>> = match connectors {
            Some(c) => c,
            None => flock.machines.iter().map(|m| Arc::new(Endpoint::from_machine(m)) as Arc<dyn Connector>).collect(),
        };
        anyhow::ensure!(connectors.len() == flock.machines.len(), "one connector per machine");
        let machines = flock.machines.iter().zip(connectors).map(|(m, c)| {
            spawn_machine(m.name.clone(), m.max_agents, m.tags.clone(), c, store.clone(), settings.clone(), events.clone())
        }).collect();
        Ok(Daemon { paths, config, store, machines, events })
    }

    pub fn socket_path(&self) -> PathBuf { self.paths.socket_file() }
    pub fn store(&self) -> Arc<Store> { self.store.clone() }
    pub fn subscribe(&self) -> broadcast::Receiver<PastorEvent> { self.events.subscribe() }

    pub async fn run(self) -> anyhow::Result<()> { todo!() }

    pub async fn handle(&self, req: IpcRequest) -> IpcResponse { todo!() }

    /// Try to place every queued task, oldest first. Called on each tick and after `run`.
    pub async fn dispatch_queued(&self) { todo!() }

    fn views(&self) -> Vec<MachineView> {
        self.machines.iter().map(|m| {
            let s = m.snapshot();
            MachineView { name: m.name.clone(), max_agents: m.max_agents, tags: m.tags.clone(), live: s.live, healthy: s.channel == ChannelState::Connected }
        }).collect()
    }
}

pub async fn serve(paths: Paths) -> anyhow::Result<()> {
    let config = PastorConfig::load(&paths.config_file())?;
    let flock = Flock::load(&paths.flock_file())?;
    anyhow::ensure!(!flock.machines.is_empty(), "flock is empty; add a machine with `pastor flock add`");
    let daemon = Daemon::start(paths, config, flock, None).await?;
    tracing::info!(socket = %daemon.socket_path().display(), machines = daemon.machines.len(), "pastor serve");
    daemon.run().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::flock::MachineConfig;
    use crate::herdr::fake::FakeHerdr;
    use crate::store::TaskFilter;
    use crate::task::{DispatchSpec, TaskState};
    use std::time::{Duration, Instant};

    fn machine(name: &str, max: u32) -> MachineConfig {
        MachineConfig { name: name.into(), local: false, ssh: None, command: Some(vec!["fake".into()]), session: "default".into(), max_agents: max, tags: vec![] }
    }

    fn spec() -> DispatchSpec {
        DispatchSpec { agent: "claude".into(), agent_args: vec![], repo: None, worktree: false, branch: None, machine: None, tags: vec![], timeout_secs: 60 }
    }

    async fn daemon(fakes: &[(&str, u32, FakeHerdr)]) -> (Daemon, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        let flock = Flock { machines: fakes.iter().map(|(n, max, _)| machine(n, *max)).collect() };
        let connectors = fakes.iter().map(|(_, _, f)| Arc::new(f.clone()) as Arc<dyn Connector>).collect();
        let mut config = PastorConfig::default();
        config.settle = "1s".into();
        let d = Daemon::start(paths, config, flock, Some(connectors)).await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while d.views().iter().any(|v| !v.healthy) {
            assert!(Instant::now() < deadline, "machines never connected");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        (d, tmp)
    }

    #[tokio::test]
    async fn run_dispatches_immediately() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let resp = d.handle(IpcRequest::Run { prompt: "hi".into(), spec: spec() }).await;
        let IpcResponse::Task(t) = resp else { panic!("{resp:?}") };
        assert_eq!(t.state, TaskState::Running);
        assert_eq!(t.machine.as_deref(), Some("a"));
        let IpcResponse::Tasks(list) = d.handle(IpcRequest::List { filter: TaskFilter::default() }).await else { panic!() };
        assert_eq!(list.len(), 1);
        let IpcResponse::Text(text) = d.handle(IpcRequest::TaskRead { id: t.id, lines: 5 }).await else { panic!() };
        assert!(text.contains("fake output"));
        let IpcResponse::Machines(ms) = d.handle(IpcRequest::FlockList).await else { panic!() };
        assert_eq!(ms[0].live, 1);
    }

    #[tokio::test]
    async fn queued_task_dispatches_when_capacity_frees() {
        let fake = FakeHerdr::new();
        let (d, _tmp) = daemon(&[("a", 1, fake.clone())]).await;
        let IpcResponse::Task(first) = d.handle(IpcRequest::Run { prompt: "1".into(), spec: spec() }).await else { panic!() };
        assert_eq!(first.state, TaskState::Running);
        let IpcResponse::Task(second) = d.handle(IpcRequest::Run { prompt: "2".into(), spec: spec() }).await else { panic!() };
        assert_eq!(second.state, TaskState::Queued);
        d.dispatch_queued().await;
        assert_eq!(d.store.get_task(second.id).unwrap().unwrap().state, TaskState::Queued, "still no room");
        fake.close_pane(first.pane_id.as_deref().unwrap());
        let deadline = Instant::now() + Duration::from_secs(5);
        while d.store.get_task(first.id).unwrap().unwrap().state != TaskState::Closed {
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        d.dispatch_queued().await;
        assert_eq!(d.store.get_task(second.id).unwrap().unwrap().state, TaskState::Running);
    }

    #[tokio::test]
    async fn pinned_unknown_machine_and_bad_ids_are_errors() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let resp = d.handle(IpcRequest::Run { prompt: "x".into(), spec: DispatchSpec { machine: Some("zzz".into()), ..spec() } }).await;
        let IpcResponse::Error { code, .. } = resp else { panic!("{resp:?}") };
        assert_eq!(code, "unknown_machine");
        let IpcResponse::Error { code, .. } = d.handle(IpcRequest::TaskShow { id: 99 }).await else { panic!() };
        assert_eq!(code, "task_not_found");
    }

    #[tokio::test]
    async fn socket_round_trip() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let socket = d.socket_path();
        tokio::spawn(d.run());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !crate::ipc::daemon_running(&socket).await {
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let resp = crate::ipc::request(&socket, &IpcRequest::Run { prompt: "hi".into(), spec: spec() }).await.unwrap();
        assert!(matches!(resp, IpcResponse::Task(_)), "{resp:?}");
        // garbage in, error out, connection survives for the next client
        let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
        let (r, mut w) = stream.into_split();
        w.write_all(b"not json\n").await.unwrap();
        let mut line = String::new();
        BufReader::new(r).read_line(&mut line).await.unwrap();
        assert!(line.contains("invalid_request"), "{line}");
        assert!(crate::ipc::daemon_running(&socket).await);
    }
}
```

Add `pub mod ipc; pub mod daemon;` to `src/lib.rs`.

- [ ] **Step 3: Run to see failures**

Run: `cargo test daemon::`
Expected: 4 failures with `not yet implemented`.

- [ ] **Step 4: Implement run, handle and dispatch_queued**

```rust
    pub async fn run(self) -> anyhow::Result<()> {
        let socket = self.socket_path();
        if socket.exists() {
            if crate::ipc::daemon_running(&socket).await {
                anyhow::bail!("another pastor serve is already listening on {}", socket.display());
            }
            std::fs::remove_file(&socket)?;
        }
        let listener = tokio::net::UnixListener::bind(&socket)?;
        std::fs::set_permissions(&socket, <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600))?;
        let daemon = Arc::new(self);
        let mut tick = tokio::time::interval(daemon.config.tick_duration());
        let mut events = daemon.events.subscribe();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, _) = accepted?;
                    let d = daemon.clone();
                    tokio::spawn(async move {
                        let (r, mut w) = stream.into_split();
                        let mut line = String::new();
                        if BufReader::new(r).read_line(&mut line).await.is_err() { return; }
                        let resp = match serde_json::from_str::<IpcRequest>(line.trim()) {
                            Ok(req) => d.handle(req).await,
                            Err(err) => IpcResponse::error("invalid_request", err),
                        };
                        let mut out = serde_json::to_string(&resp).unwrap_or_else(|e| format!("{{\"kind\":\"error\",\"code\":\"internal\",\"message\":\"{e}\"}}"));
                        out.push('\n');
                        let _ = w.write_all(out.as_bytes()).await;
                    });
                }
                _ = tick.tick() => daemon.dispatch_queued().await,
                ev = events.recv() => match ev {
                    Ok(ev) => tracing::info!(kind = %ev.kind, task = ?ev.task_id, machine = %ev.machine, "pastor event"),
                    Err(broadcast::error::RecvError::Lagged(n)) => tracing::warn!(n, "event log lagged"),
                    Err(_) => {}
                },
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("shutting down; agents keep running");
                    let _ = std::fs::remove_file(&socket);
                    return Ok(());
                }
            }
        }
    }

    pub async fn handle(&self, req: IpcRequest) -> IpcResponse {
        match req {
            IpcRequest::Ping => IpcResponse::Pong { version: env!("CARGO_PKG_VERSION").into() },
            IpcRequest::Run { prompt, spec } => {
                if let Some(m) = &spec.machine {
                    if !self.machines.iter().any(|h| &h.name == m) {
                        return IpcResponse::error("unknown_machine", format!("machine {m} is not in the flock"));
                    }
                }
                let task = match self.store.insert_task(NewTask { job: "run".into(), item: serde_json::Value::Null, prompt, spec }) {
                    Ok(t) => t,
                    Err(err) => return IpcResponse::error("store_error", err),
                };
                self.dispatch_queued().await;
                match self.store.get_task(task.id) {
                    Ok(Some(t)) => IpcResponse::Task(t),
                    Ok(None) => IpcResponse::error("task_not_found", task.id),
                    Err(err) => IpcResponse::error("store_error", err),
                }
            }
            IpcRequest::List { filter } => match self.store.list_tasks(&filter) {
                Ok(ts) => IpcResponse::Tasks(ts),
                Err(err) => IpcResponse::error("store_error", err),
            },
            IpcRequest::TaskShow { id } => match self.store.get_task(id) {
                Ok(Some(t)) => IpcResponse::Task(t),
                Ok(None) => IpcResponse::error("task_not_found", format!("t-{id}")),
                Err(err) => IpcResponse::error("store_error", err),
            },
            IpcRequest::TaskRead { id, lines } => {
                let task = match self.store.get_task(id) {
                    Ok(Some(t)) => t,
                    Ok(None) => return IpcResponse::error("task_not_found", format!("t-{id}")),
                    Err(err) => return IpcResponse::error("store_error", err),
                };
                let Some(handle) = task.machine.as_ref().and_then(|m| self.machines.iter().find(|h| &h.name == m)) else {
                    return IpcResponse::error("no_machine", format!("t-{id} is not on any machine"));
                };
                match handle.read(id, lines).await {
                    Ok(text) => IpcResponse::Text(text),
                    Err(err) => IpcResponse::error("read_failed", err),
                }
            }
            IpcRequest::FlockList => IpcResponse::Machines(self.machines.iter().map(|m| m.snapshot()).collect()),
        }
    }

    pub async fn dispatch_queued(&self) {
        let queued = match self.store.queued_tasks() {
            Ok(q) => q,
            Err(err) => { tracing::error!(%err, "list queued"); return; }
        };
        for task in queued {
            let Some(name) = pick_machine(&self.views(), &task.spec) else { continue };
            let Some(handle) = self.machines.iter().find(|h| h.name == name) else { continue };
            match handle.dispatch(task.id).await {
                Ok(t) => tracing::info!(task = %t.display_id(), machine = %name, state = %t.state, "dispatched"),
                Err(err) => tracing::warn!(task = %task.display_id(), machine = %name, %err, "dispatch failed"),
            }
        }
    }
```

- [ ] **Step 5: Run tests**

Run: `cargo test daemon::`
Expected: 4 passed.

- [ ] **Step 6: Commit**

```bash
git add src/ipc.rs src/daemon.rs src/store.rs src/lib.rs
git commit -m "feat: daemon with unix socket ipc and queued dispatch"
```

---

### Task 12: CLI commands

**Files:**
- Modify: `src/main.rs` (full rewrite)
- Create: `src/cli.rs` (output helpers), `tests/cli.rs`

**Interfaces:**
- Consumes: everything above.
- Produces the user-facing commands:
  ```
  pastor serve
  pastor run <prompt> [--repo P] [--machine M] [--agent K] [--worktree] [--branch B] [--tag T]... [--timeout 2h] [--json]
  pastor list [--job J] [--machine M] [--blocked] [--done] [--all] [--json]
  pastor task show <t-id> [--json]
  pastor task read <t-id> [--lines N]
  pastor flock add <name> [<ssh-target>] [--local] [--command ARGV...] [--session S] [--max-agents N] [--tag T]...
  pastor flock remove <name>
  pastor flock list [--json]
  pastor flock status [name] [--json]
  pastor attach <t-id>
  pastor open <machine>
  ```

- [ ] **Step 1: Write the output helpers**

`src/cli.rs`:

```rust
use chrono::Utc;

use crate::machine::MachineStatus;
use crate::task::Task;

pub fn age(from: chrono::DateTime<Utc>) -> String {
    let secs = (Utc::now() - from).num_seconds().max(0);
    if secs < 60 { format!("{secs}s") } else if secs < 3600 { format!("{}m", secs / 60) } else if secs < 86400 { format!("{}h", secs / 3600) } else { format!("{}d", secs / 86400) }
}

pub fn table(header: &[&str], rows: &[Vec<String>]) -> String {
    let cols = header.len();
    let mut widths: Vec<usize> = header.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(cols) {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let fmt = |cells: &[String]| -> String {
        cells.iter().enumerate().map(|(i, c)| if i + 1 == cols { c.clone() } else { format!("{:<w$}", c, w = widths[i]) }).collect::<Vec<_>>().join("  ").trim_end().to_string()
    };
    let mut out = fmt(&header.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    for row in rows {
        out.push('\n');
        out.push_str(&fmt(row));
    }
    out
}

pub fn task_rows(tasks: &[Task]) -> Vec<Vec<String>> {
    tasks.iter().map(|t| {
        let note = t.error.clone().or_else(|| t.item.get("title").and_then(|v| v.as_str()).map(str::to_string)).unwrap_or_else(|| t.prompt.lines().next().unwrap_or("").chars().take(60).collect());
        vec![t.display_id(), t.state.to_string(), t.machine.clone().unwrap_or_else(|| "-".into()), t.spec.agent.clone(), t.job.clone(), age(t.created_at), note]
    }).collect()
}

pub const TASK_HEADER: [&str; 7] = ["ID", "STATE", "MACHINE", "AGENT", "JOB", "AGE", "NOTE"];

pub fn machine_rows(ms: &[MachineStatus]) -> Vec<Vec<String>> {
    ms.iter().map(|m| {
        vec![m.name.clone(), m.channel.to_string(), m.herdr_version.clone().unwrap_or_else(|| "-".into()), format!("{}/{}", m.live, m.max_agents), m.tags.join(","), m.error.clone().unwrap_or_default()]
    }).collect()
}

pub const MACHINE_HEADER: [&str; 6] = ["NAME", "CHANNEL", "HERDR", "AGENTS", "TAGS", "ERROR"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_aligns_and_trims() {
        let out = table(&["A", "BB"], &[vec!["x".into(), "".into()], vec!["long".into(), "y".into()]]);
        assert_eq!(out, "A     BB\nx\nlong  y");
    }
}
```

Add `pub mod cli;` to `src/lib.rs`.

- [ ] **Step 2: Rewrite main.rs**

```rust
use std::os::unix::process::CommandExt;

use clap::{Args, Parser, Subcommand};
use pastor::config::flock::{Flock, MachineConfig};
use pastor::config::{parse_duration, Paths, PastorConfig};
use pastor::herdr::{Connector, Endpoint};
use pastor::ipc::{daemon_running, request, IpcRequest, IpcResponse};
use pastor::machine::{ChannelState, MachineStatus};
use pastor::store::{Store, TaskFilter};
use pastor::task::{parse_task_id, DispatchSpec, Task, TaskState};

#[derive(Parser)]
#[command(name = "pastor", version, about = "run coding agents on machines you own")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon: scheduler, machine channels, dispatch
    Serve,
    /// Create a one-off task and dispatch it
    Run(RunArgs),
    /// List tasks across the flock
    List(ListArgs),
    /// Inspect a task
    Task { #[command(subcommand)] cmd: TaskCmd },
    /// Manage machines
    Flock { #[command(subcommand)] cmd: FlockCmd },
    /// Attach to a task's agent terminal (ctrl+b q detaches)
    Attach { task: String },
    /// Open the full herdr UI on a machine
    Open { machine: String },
}

#[derive(Args)]
struct RunArgs {
    prompt: String,
    #[arg(long)] repo: Option<String>,
    #[arg(long)] machine: Option<String>,
    #[arg(long)] agent: Option<String>,
    #[arg(long)] worktree: bool,
    #[arg(long)] branch: Option<String>,
    #[arg(long = "tag")] tags: Vec<String>,
    #[arg(long)] timeout: Option<String>,
    #[arg(long)] json: bool,
}

#[derive(Args)]
struct ListArgs {
    #[arg(long)] job: Option<String>,
    #[arg(long)] machine: Option<String>,
    #[arg(long)] blocked: bool,
    #[arg(long)] done: bool,
    /// Include closed and failed tasks
    #[arg(long)] all: bool,
    #[arg(long)] json: bool,
}

#[derive(Subcommand)]
enum TaskCmd {
    Show { task: String, #[arg(long)] json: bool },
    Read { task: String, #[arg(long, default_value_t = 40)] lines: u32 },
}

#[derive(Subcommand)]
enum FlockCmd {
    Add {
        name: String,
        ssh: Option<String>,
        #[arg(long)] local: bool,
        #[arg(long, num_args = 1.., allow_hyphen_values = true)] command: Option<Vec<String>>,
        #[arg(long, default_value = "default")] session: String,
        #[arg(long, default_value_t = 2)] max_agents: u32,
        #[arg(long = "tag")] tags: Vec<String>,
    },
    Remove { name: String },
    List { #[arg(long)] json: bool },
    Status { name: Option<String>, #[arg(long)] json: bool },
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "pastor=info".into()))
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let paths = match Paths::from_env() {
        Ok(p) => p,
        Err(err) => fail("config_error", &err.to_string()),
    };
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let result = rt.block_on(async {
        match cli.command {
            Command::Serve => pastor::daemon::serve(paths).await,
            Command::Run(args) => run(&paths, args).await,
            Command::List(args) => list(&paths, args).await,
            Command::Task { cmd } => task(&paths, cmd).await,
            Command::Flock { cmd } => flock(&paths, cmd).await,
            Command::Attach { task } => attach(&paths, &task).await,
            Command::Open { machine } => open(&paths, &machine).await,
        }
    });
    if let Err(err) = result {
        fail("runtime_error", &format!("{err:#}"));
    }
}

fn fail(code: &str, message: &str) -> ! {
    eprintln!("{}", serde_json::json!({"code": code, "message": message}));
    std::process::exit(1)
}

async fn ask(paths: &Paths, req: IpcRequest) -> anyhow::Result<IpcResponse> {
    let socket = paths.socket_file();
    let resp = request(&socket, &req).await.map_err(|e| anyhow::anyhow!("pastor serve is not running ({e}); start it with `pastor serve`"))?;
    if let IpcResponse::Error { code, message } = &resp {
        fail(code, message);
    }
    Ok(resp)
}

async fn run(paths: &Paths, a: RunArgs) -> anyhow::Result<()> {
    let config = PastorConfig::load(&paths.config_file())?;
    let timeout = a.timeout.as_deref().map(parse_duration).transpose().map_err(|e| anyhow::anyhow!(e))?.unwrap_or(config.timeout_duration());
    let spec = DispatchSpec {
        agent: a.agent.unwrap_or(config.defaults.agent), agent_args: vec![], repo: a.repo, worktree: a.worktree, branch: a.branch,
        machine: a.machine, tags: a.tags, timeout_secs: timeout.as_secs(),
    };
    let IpcResponse::Task(t) = ask(paths, IpcRequest::Run { prompt: a.prompt, spec }).await? else { unreachable!() };
    print_task(&t, a.json);
    Ok(())
}

fn print_task(t: &Task, json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(t).unwrap());
    } else {
        println!("{}", pastor::cli::table(&pastor::cli::TASK_HEADER, &pastor::cli::task_rows(std::slice::from_ref(t))));
    }
}

async fn list(paths: &Paths, a: ListArgs) -> anyhow::Result<()> {
    let states = if a.blocked { Some(vec![TaskState::Blocked]) } else if a.done { Some(vec![TaskState::Done]) } else if a.all { None } else {
        Some(vec![TaskState::Queued, TaskState::Starting, TaskState::Running, TaskState::Blocked, TaskState::Done, TaskState::Stale])
    };
    let filter = TaskFilter { job: a.job, machine: a.machine, states };
    let tasks = if daemon_running(&paths.socket_file()).await {
        let IpcResponse::Tasks(ts) = ask(paths, IpcRequest::List { filter }).await? else { unreachable!() };
        ts
    } else {
        eprintln!("pastor serve is not running; showing the last known state");
        Store::open(&paths.db_file())?.list_tasks(&filter)?
    };
    if a.json {
        println!("{}", serde_json::to_string_pretty(&tasks)?);
    } else if tasks.is_empty() {
        println!("no tasks");
    } else {
        println!("{}", pastor::cli::table(&pastor::cli::TASK_HEADER, &pastor::cli::task_rows(&tasks)));
    }
    Ok(())
}

fn task_id(s: &str) -> i64 {
    parse_task_id(s).unwrap_or_else(|| fail("usage_error", &format!("{s} is not a task id like t-12")))
}

async fn task(paths: &Paths, cmd: TaskCmd) -> anyhow::Result<()> {
    match cmd {
        TaskCmd::Show { task, json } => {
            let id = task_id(&task);
            let t = if daemon_running(&paths.socket_file()).await {
                let IpcResponse::Task(t) = ask(paths, IpcRequest::TaskShow { id }).await? else { unreachable!() };
                t
            } else {
                Store::open(&paths.db_file())?.get_task(id)?.unwrap_or_else(|| fail("task_not_found", &task))
            };
            print_task(&t, json);
        }
        TaskCmd::Read { task, lines } => {
            let IpcResponse::Text(text) = ask(paths, IpcRequest::TaskRead { id: task_id(&task), lines }).await? else { unreachable!() };
            print!("{text}");
        }
    }
    Ok(())
}

async fn flock(paths: &Paths, cmd: FlockCmd) -> anyhow::Result<()> {
    let path = paths.flock_file();
    match cmd {
        FlockCmd::Add { name, ssh, local, command, session, max_agents, tags } => {
            let mut f = Flock::load(&path)?;
            let m = MachineConfig { name: name.clone(), local, ssh, command, session, max_agents, tags };
            f.add(m).map_err(|e| anyhow::anyhow!(e))?;
            f.save(&path)?;
            println!("added {name} to {}", path.display());
            if let Some(target) = f.get(&name).and_then(|m| m.ssh.clone()) {
                println!("to see it in your laptop's herdr sidebar: herdr machine add {target} --label {name}");
            }
            println!("restart pastor serve to pick it up");
        }
        FlockCmd::Remove { name } => {
            let mut f = Flock::load(&path)?;
            anyhow::ensure!(f.remove(&name), "machine {name} not found");
            f.save(&path)?;
            println!("removed {name}; restart pastor serve to apply");
        }
        FlockCmd::List { json } => {
            let statuses: Vec<MachineStatus> = if daemon_running(&paths.socket_file()).await {
                let IpcResponse::Machines(ms) = ask(paths, IpcRequest::FlockList).await? else { unreachable!() };
                ms
            } else {
                eprintln!("pastor serve is not running; showing the flock file only");
                Flock::load(&path)?.machines.iter().map(|m| MachineStatus {
                    name: m.name.clone(), endpoint: Endpoint::from_machine(m).describe(), channel: ChannelState::Connecting, herdr_version: None, protocol: None, error: Some("daemon down".into()), live: 0, max_agents: m.max_agents, tags: m.tags.clone(),
                }).collect()
            };
            if json { println!("{}", serde_json::to_string_pretty(&statuses)?); } else { println!("{}", pastor::cli::table(&pastor::cli::MACHINE_HEADER, &pastor::cli::machine_rows(&statuses))); }
        }
        FlockCmd::Status { name, json } => {
            let f = Flock::load(&path)?;
            let mut rows = Vec::new();
            for m in f.machines.iter().filter(|m| name.as_ref().map_or(true, |n| &m.name == n)) {
                let ep = Endpoint::from_machine(m);
                let (status, version, protocol, agents, error) = match ep.connect().await {
                    Ok(mut c) => match c.ping().await {
                        Ok(p) => {
                            let compatible = p.protocol >= pastor::MIN_HERDR_PROTOCOL;
                            let n = c.agent_list().await.map(|a| a.len()).unwrap_or(0);
                            (if compatible { "reachable" } else { "incompatible" }, Some(p.version), Some(p.protocol), n, if compatible { None } else { Some(format!("protocol {} < {}", p.protocol, pastor::MIN_HERDR_PROTOCOL)) })
                        }
                        Err(e) => ("error", None, None, 0, Some(e.to_string())),
                    },
                    Err(e) => (if e.message.contains("herdr.sock") { "server down" } else { "unreachable" }, None, None, 0, Some(e.message)),
                };
                rows.push(serde_json::json!({"name": m.name, "endpoint": ep.describe(), "status": status, "herdr_version": version, "protocol": protocol, "agents": agents, "error": error}));
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                let table_rows: Vec<Vec<String>> = rows.iter().map(|r| vec![
                    r["name"].as_str().unwrap_or("").into(), r["status"].as_str().unwrap_or("").into(), r["herdr_version"].as_str().unwrap_or("-").into(),
                    r["agents"].to_string(), r["error"].as_str().unwrap_or("").into(),
                ]).collect();
                println!("{}", pastor::cli::table(&["NAME", "STATUS", "HERDR", "AGENTS", "ERROR"], &table_rows));
            }
        }
    }
    Ok(())
}

async fn attach(paths: &Paths, task: &str) -> anyhow::Result<()> {
    let id = task_id(task);
    let t = Store::open(&paths.db_file())?.get_task(id)?.unwrap_or_else(|| fail("task_not_found", task));
    let (Some(machine), Some(agent)) = (t.machine.clone(), t.agent_name.clone()) else { fail("no_agent", &format!("{} has no agent yet", t.display_id())) };
    if !t.state.occupies_pane() { fail("no_agent", &format!("{} is {}; nothing to attach to", t.display_id(), t.state)); }
    let f = Flock::load(&paths.flock_file())?;
    let m = f.get(&machine).unwrap_or_else(|| fail("unknown_machine", &machine));
    let err = if let Some(target) = &m.ssh {
        std::process::Command::new("ssh").arg("-t").arg(target).arg(format!("herdr --session {} agent attach {agent}", m.session)).exec()
    } else if m.local {
        std::process::Command::new("herdr").args(["--session", &m.session, "agent", "attach", &agent]).exec()
    } else {
        fail("no_terminal", "command machines have no terminal to attach to")
    };
    Err(anyhow::anyhow!("exec failed: {err}"))
}

async fn open(paths: &Paths, machine: &str) -> anyhow::Result<()> {
    let f = Flock::load(&paths.flock_file())?;
    let m = f.get(machine).unwrap_or_else(|| fail("unknown_machine", machine));
    let err = if let Some(target) = &m.ssh {
        std::process::Command::new("herdr").args(["--remote", target, "--session", &m.session]).exec()
    } else if m.local {
        std::process::Command::new("herdr").args(["--session", &m.session]).exec()
    } else {
        fail("no_terminal", "command machines have no UI to open")
    };
    Err(anyhow::anyhow!("exec failed: {err}"))
}
```

Run: `cargo build` — Expected: compiles with no warnings about unused imports (fix any).

- [ ] **Step 3: Write the end-to-end test**

`tests/cli.rs`:

```rust
//! Drives the real binaries: pastor serve with a fake-herdr machine, then run/list/task.
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn pastor() -> Command { Command::new(env!("CARGO_BIN_EXE_pastor")) }

struct Env { _tmp: tempfile::TempDir, config: std::path::PathBuf, state: std::path::PathBuf, serve: std::process::Child }

impl Drop for Env {
    fn drop(&mut self) { let _ = self.serve.kill(); let _ = self.serve.wait(); }
}

fn start() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("pastor.toml"), "tick = \"1s\"\nsettle = \"1s\"\n").unwrap();
    std::fs::write(config.join("flock.toml"), format!("[[machine]]\nname = \"fake\"\ncommand = [\"{}\"]\nmax_agents = 2\n", env!("CARGO_BIN_EXE_fake-herdr"))).unwrap();
    let serve = pastor().args(["serve"]).env("PASTOR_CONFIG_DIR", &config).env("PASTOR_STATE_DIR", &state)
        .env("FAKE_HERDR_AUTO_DONE_MS", "300").stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let env = Env { _tmp: tmp, config, state, serve };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let out = env.cmd(&["flock", "list", "--json"]);
        if out.status.success() && String::from_utf8_lossy(&out.stdout).contains("\"connected\"") { break; }
        assert!(Instant::now() < deadline, "daemon never reported the machine connected: {}", String::from_utf8_lossy(&out.stderr));
        std::thread::sleep(Duration::from_millis(100));
    }
    env
}

impl Env {
    fn cmd(&self, args: &[&str]) -> std::process::Output {
        pastor().args(args).env("PASTOR_CONFIG_DIR", &self.config).env("PASTOR_STATE_DIR", &self.state).output().unwrap()
    }
}

#[test]
fn run_list_show_read_end_to_end() {
    let env = start();
    let out = env.cmd(&["run", "say hello\nthen stop", "--repo", "/tmp", "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let task: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(task["state"], "running");
    assert_eq!(task["machine"], "fake");
    assert_eq!(task["agent_name"], "t-1");

    let out = env.cmd(&["task", "read", "t-1"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("fake output"));

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let out = env.cmd(&["task", "show", "t-1", "--json"]);
        let t: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        if t["state"] == "done" { break; }
        assert!(Instant::now() < deadline, "task never became done: {t}");
        std::thread::sleep(Duration::from_millis(100));
    }
    let out = env.cmd(&["list"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("t-1") && text.contains("done"), "{text}");

    let out = env.cmd(&["task", "show", "t-9"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("task_not_found"));
}

#[test]
fn flock_add_and_remove_edit_the_file() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    let run = |args: &[&str]| pastor().args(args).env("PASTOR_CONFIG_DIR", &config).env("PASTOR_STATE_DIR", &state).output().unwrap();
    let out = run(&["flock", "add", "pi-3", "fleet@pi-3", "--max-agents", "3", "--tag", "fast"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = std::fs::read_to_string(config.join("flock.toml")).unwrap();
    assert!(text.contains("name = \"pi-3\"") && text.contains("ssh = \"fleet@pi-3\"") && text.contains("max_agents = 3"), "{text}");
    let out = run(&["flock", "add", "pi-3", "fleet@pi-3"]);
    assert!(!out.status.success());
    let out = run(&["flock", "list"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("pi-3"));
    assert!(run(&["flock", "remove", "pi-3"]).status.success());
    assert!(!run(&["flock", "remove", "pi-3"]).status.success());
}

#[test]
fn list_without_daemon_reads_the_database() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    let out = pastor().args(["list"]).env("PASTOR_CONFIG_DIR", &config).env("PASTOR_STATE_DIR", &state).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("no tasks"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not running"));
}
```

Note `list_without_daemon_reads_the_database` needs `Store::open` to create the state dir. In `main.rs` `list`, call `paths.ensure()?` before `Store::open` in the fallback branch. Same in `task show` fallback and `attach`.

- [ ] **Step 4: Run all tests**

Run: `cargo test`
Expected: everything passes, including the three in `tests/cli.rs`. The end-to-end test takes a few seconds.

- [ ] **Step 5: Manual smoke against the real local herdr (optional, needs herdr 0.9+ running)**

```bash
PASTOR_CONFIG_DIR=/tmp/pastor-c PASTOR_STATE_DIR=/tmp/pastor-s cargo run -- flock add here --local --max-agents 1
PASTOR_CONFIG_DIR=/tmp/pastor-c PASTOR_STATE_DIR=/tmp/pastor-s cargo run -- flock status
```

Expected: `here  reachable  0.9.x  <n>` if herdr is 0.9 or newer, `incompatible` with a protocol note on 0.8.2.

- [ ] **Step 6: Commit**

```bash
git add src/main.rs src/cli.rs src/lib.rs tests/cli.rs
git commit -m "feat: run, list, task, flock, attach and open commands"
```

---

### Task 13: README and branch wrap-up

**Files:**
- Create: `README.md`
- Modify: none

- [ ] **Step 1: Write the README**

`README.md`:

```markdown
# pastor

pastor runs coding agents on always-on machines you own, so they pick up
tasks while your laptop is closed. It sits on top of [herdr](https://herdr.dev):
herdr owns the terminals and the agents, pastor owns the fleet and the
bookkeeping. When you open the laptop you attach to the panes through herdr.

Status: core only. One-off tasks work end to end. Scheduled jobs, connector
plugins and systemd setup are the next milestones; see
`docs/superpowers/specs/2026-09-23-pastor-design.md`.

## How it works

`pastor serve` runs on one machine, the head. For every machine in the flock it
keeps an SSH connection running `herdr --session <s> remote-api-bridge`, which
pipes herdr's socket protocol over stdio. Through it pastor creates a workspace,
starts an agent named after the task, sends the prompt, and subscribes to agent
status events. Task state lives in SQLite under `~/.local/state/pastor/`.

Each machine needs herdr 0.9 or newer with its server running, and SSH access
from the head without a passphrase prompt (a key in ssh-agent won't be there for
a service; use a dedicated key or Tailscale SSH).

## Try it

```bash
cargo build --release
pastor flock add pi-3 fleet@pi-3 --max-agents 2
pastor flock add here --local
pastor flock status                  # ssh, herdr version, protocol
pastor serve &                       # or run it under systemd later
pastor run "Fix the flaky test in ci.yml" --repo ~/work/api --machine pi-3
pastor list
pastor attach t-1                    # lands in the agent's pane; ctrl+b q detaches
```

Without a real herdr, a fake one speaks the same protocol:

```bash
pastor flock add fake --command target/release/fake-herdr
FAKE_HERDR_AUTO_DONE_MS=500 pastor serve
```

## Files

```
~/.config/pastor/pastor.toml      tick, settle, defaults (all optional)
~/.config/pastor/flock.toml       machines
~/.local/state/pastor/pastor.db   tasks
~/.local/state/pastor/pastor.sock daemon socket
```

`PASTOR_CONFIG_DIR` and `PASTOR_STATE_DIR` override the locations.

## Development

```bash
cargo test            # unit tests plus an end-to-end run against fake-herdr
cargo run -- --help
```
```

- [ ] **Step 2: Run the full suite one last time and check formatting**

Run: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: clean. Fix anything clippy raises before moving on.

- [ ] **Step 3: Commit and open the PR**

```bash
git add README.md
git commit -m "docs: readme for the core milestone"
git push -u origin feat/core
gh pr create --title "pastor core: daemon, herdr client, run/list/attach" --body "Implements plan 1 of docs/superpowers/plans/2026-09-23-pastor-core.md. Spec: docs/superpowers/specs/2026-09-23-pastor-design.md."
```
