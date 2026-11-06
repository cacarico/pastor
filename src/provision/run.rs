//! Running a provisioner's checks on one machine, for `pastor machine
//! check`: what the machine is missing, changing nothing there but a run
//! directory of its own, which is removed again.
//!
//! A run copies the provisioner (a copied one's whole directory, so its
//! checks may call scripts shipped beside the manifest; a built-in's
//! manifest) into a fresh `~/.cache/pastor/setup/XXXXXX` on the machine
//! with `tar`, then runs each check from there in a login shell, in file
//! order, and removes the directory: at the end, on failure, and when the
//! run's future is dropped (Ctrl-C). Fixes are never run here.
//!
//! A check's status is its exit code (0 passes, 1 needs fixing, anything
//! else is an error) and the first line of its own output, which starts
//! after a marker the run prints just before the command: a `~/.profile`
//! that echoes a banner never becomes a status.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::catalog::{BUILTIN, Provisioner};
use super::manifest::{Account, Check, MANIFEST_FILE};
use crate::config::Paths;
use crate::config::flock::MachineConfig;
use crate::herdr::transport::{
    Endpoint, PROBE_TIMEOUT, probe_output_feeding, shell_argv, shell_quote,
};

/// The code of a machine pastor cannot run a shell on: a `command` bridge
/// speaks herdr's API only, and the head never connects to a pull machine.
pub const MACHINE_CHECK_UNSUPPORTED: &str = "machine_check_unsupported";

/// Where run directories go, under the machine's `$HOME`.
pub const SETUP_ROOT: &str = ".cache/pastor/setup";

/// What a run needs on the machine before anything else: the shell, and
/// what unpacks, makes and removes the run directory.
pub const BASE_TOOLS: [&str; 5] = ["sh", "tar", "mktemp", "rm", "chmod"];

/// Printed before every answer pastor reads, so what the machine's rc files
/// print first is skipped.
const MARK: &str = "pastor-check-5d1a";

/// What one command on the machine gave back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ran {
    /// `None` when a signal ended it.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub type ShellFuture<'a> = Pin<Box<dyn Future<Output = Result<Ran, String>> + Send + 'a>>;

/// How a run reaches the machine: a shell there, for `MachineShell`, or a
/// fake in tests.
pub trait Shell: Send + Sync {
    /// Run `script` (a POSIX shell command) on the machine with `input` on
    /// its stdin (none when empty). Err says why it gave no answer: past
    /// `time`, more than the probes' 64 KiB on either stream, or not
    /// started.
    fn run(&self, script: String, input: Vec<u8>, time: Duration) -> ShellFuture<'_>;
}

/// A machine's shell as the transport's probes reach it: `sh -c` for a
/// local machine, ssh for an ssh one, with the probes' limits.
pub struct MachineShell {
    ep: Endpoint,
}

impl MachineShell {
    pub fn new(m: &MachineConfig, paths: &Paths) -> Result<MachineShell, RunError> {
        if m.pull {
            return Err(RunError::Unsupported(format!(
                "machine {} is a pull machine, which the head never connects to; run machine check on a machine reached by ssh or local",
                m.name
            )));
        }
        let ep = Endpoint::from_machine(m, paths);
        if let Endpoint::Command { .. } = ep {
            return Err(RunError::Unsupported(format!(
                "machine {} is reached through a command bridge, which speaks herdr's API and runs no shell; machine check needs a machine reached by ssh or local",
                m.name
            )));
        }
        Ok(MachineShell { ep })
    }
}

impl Shell for MachineShell {
    fn run(&self, script: String, input: Vec<u8>, time: Duration) -> ShellFuture<'_> {
        Box::pin(async move {
            let argv = shell_argv(&self.ep, script)
                .map_err(|e| e.message)?
                .ok_or("this machine runs no shell")?;
            let out = probe_output_feeding(&argv, &input, time)
                .await
                .map_err(|e| e.message)?;
            Ok(Ran {
                code: out.status.code(),
                stdout: out.stdout,
                stderr: out.stderr,
            })
        })
    }
}

/// What to run, and for whom.
pub struct Request<'a> {
    /// The machine's name, for messages and `PASTOR_MACHINE`.
    pub machine: &'a str,
    pub provisioner: &'a Provisioner,
    /// The accounts the machine's flocks ask for: the checks run are
    /// `Manifest::checks_for` these.
    pub accounts: &'a [Account],
    /// The agents the machine runs, as `PASTOR_AGENTS`, comma-separated.
    pub agents: &'a [String],
    /// Env set for every check, a leading `~` expanded against the
    /// machine's home.
    pub env: &'a BTreeMap<String, String>,
    /// The agents that can land on the machine: a check with `agent` set
    /// runs once for each of them of that kind, with its env over `env`,
    /// and not at all when there is none.
    pub agent_envs: &'a [AgentEnv],
}

/// An agent that can land on a machine, for its kind's checks: its env
/// says which account they ask about (`CLAUDE_CONFIG_DIR` for `claude auth
/// status`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEnv {
    /// Its name in `[agents]`, which its rows carry.
    pub name: String,
    /// The herdr kind it starts, which a check's `agent` names.
    pub kind: String,
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pass,
    /// Exit 1: the machine needs the check's fix.
    Fail,
    /// Any other exit, no answer, or past a limit.
    Error,
    /// A check it `needs` did not pass, so it did not run.
    Skipped,
    /// Something to look at that does not make the machine miss anything:
    /// no failure, no exit 1, no event.
    Warn,
}

/// One check's result, for the caller to print.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    pub id: String,
    /// The agent a check for its kind ran for; `None` for any other check.
    pub agent: Option<String>,
    pub status: Status,
    /// The first line the command printed, stdout and stderr together,
    /// without control characters; empty when it printed nothing.
    pub first_line: String,
    /// Why it is an error or skipped.
    pub reason: Option<String>,
}

/// Why a run gave no rows at all. A single check going wrong is a row.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunError {
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    Failed(String),
}

impl RunError {
    pub fn code(&self) -> &'static str {
        match self {
            RunError::Unsupported(_) => MACHINE_CHECK_UNSUPPORTED,
            RunError::Failed(_) => "runtime_error",
        }
    }
}

/// Run `req`'s checks on the machine behind `shell`, one row per check in
/// file order.
pub async fn run(shell: Arc<dyn Shell>, req: &Request<'_>) -> Result<Vec<Row>, RunError> {
    let tarball = {
        let copy = LocalCopy::new(req.provisioner).map_err(RunError::Failed)?;
        copy.tar().await.map_err(RunError::Failed)?
    };
    let dir = unpack(shell.clone(), req.machine, tarball).await?;
    let rows = run_checks(&*shell, req, &dir.home, dir.path()).await;
    dir.remove().await;
    Ok(rows)
}

/// A tarball unpacked in a run directory of its own on a machine, under
/// `~/SETUP_ROOT`; removed by `remove`, or when dropped.
pub struct Unpacked {
    /// The machine's `$HOME`.
    pub home: String,
    dir: RunDir,
}

impl Unpacked {
    pub fn path(&self) -> &str {
        self.dir.path()
    }

    pub async fn remove(self) {
        self.dir.remove().await;
    }
}

/// Unpack `tarball` on the machine behind `shell`, in a new run directory.
pub async fn unpack(
    shell: Arc<dyn Shell>,
    machine: &str,
    tarball: Vec<u8>,
) -> Result<Unpacked, RunError> {
    let ran = shell
        .run(setup_script(), tarball, PROBE_TIMEOUT)
        .await
        .map_err(|e| RunError::Failed(format!("machine {machine}: setting up the run: {e}")))?;
    let setup = read_setup(machine, &ran)?;
    let Some(dir) = setup.dir else {
        return Err(RunError::Failed(format!(
            "machine {machine}: could not make a run directory under ~/{SETUP_ROOT}{}",
            stderr_tail(&ran)
        )));
    };
    let dir = RunDir {
        shell,
        machine: machine.to_string(),
        path: Some(dir),
    };
    if !setup.ready {
        let why = format!(
            "machine {machine}: unpacking in {} failed{}",
            dir.path(),
            stderr_tail(&ran)
        );
        dir.remove().await;
        return Err(RunError::Failed(why));
    }
    Ok(Unpacked {
        home: setup.home,
        dir,
    })
}

/// A new, empty run directory on the machine, removed by `Unpacked::remove`
/// or when dropped: a place to make a throwaway repo under `~/SETUP_ROOT`.
pub async fn empty_dir(shell: Arc<dyn Shell>, machine: &str) -> Result<Unpacked, RunError> {
    // The end of a tar archive: two zeroed blocks, which every `tar -x`
    // takes for an archive of nothing.
    unpack(shell, machine, vec![0; 1024]).await
}

/// What a machine says it is, for picking its provisioner and the builds it
/// gets: `uname -s` lowercased, `uname -m` (macOS's `arm64` read as
/// `aarch64`), `getconf LONG_BIT`, and the `ID` and the `ID_LIKE` words of
/// its `/etc/os-release` where it has them (`ID_LIKE` names the
/// distributions it derives from, the nearest first: Ubuntu says `debian`,
/// EndeavourOS says `arch`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    pub os: String,
    pub arch: String,
    /// The userland's word size: 32 on a 64-bit kernel over a 32-bit
    /// userland, as on many a Pi. 64 when `getconf` gives no number.
    pub bits: u8,
    pub distro: Option<String>,
    pub like: Vec<String>,
}

impl Facts {
    /// From the probe's lines: `uname -s`, `uname -m`, `getconf LONG_BIT`,
    /// then the os-release `ID` and `ID_LIKE`. `None` when `uname -s` said
    /// nothing.
    pub fn read(answer: &str) -> Option<Facts> {
        let mut lines = answer.lines().map(str::trim);
        let os = lines.next().unwrap_or("").to_ascii_lowercase();
        if os.is_empty() {
            return None;
        }
        let arch = match lines.next().unwrap_or("") {
            "arm64" => "aarch64",
            a => a,
        }
        .to_string();
        let bits = lines.next().unwrap_or("").parse().unwrap_or(64);
        let distro = lines.next().filter(|d| !d.is_empty()).map(str::to_string);
        let like = lines
            .next()
            .unwrap_or("")
            .split_whitespace()
            .map(str::to_string)
            .collect();
        Some(Facts {
            os,
            arch,
            bits,
            distro,
            like,
        })
    }
}

/// Asks the machine behind `shell` for its `Facts`.
pub async fn facts(shell: &dyn Shell, machine: &str) -> Result<Facts, RunError> {
    let script = format!(
        "printf '%s\\n' {MARK}; uname -s; uname -m; getconf LONG_BIT 2>/dev/null || echo; if [ -r /etc/os-release ]; then (unset ID ID_LIKE; . /etc/os-release && printf '%s\\n%s\\n' \"$ID\" \"$ID_LIKE\"); fi"
    );
    let ran = shell
        .run(script, Vec::new(), PROBE_TIMEOUT)
        .await
        .map_err(|e| RunError::Failed(format!("machine {machine}: asking what it runs: {e}")))?;
    let stdout = String::from_utf8_lossy(&ran.stdout);
    match Facts::read(after_mark(&stdout).unwrap_or("")) {
        Some(facts) if ran.code == Some(0) => Ok(facts),
        _ => Err(RunError::Failed(format!(
            "machine {machine}: `uname -s` said nothing ({}){}",
            exit(ran.code),
            stderr_tail(&ran)
        ))),
    }
}

/// Runs `provisioner`'s `platform.detect` on the machine the way a check
/// runs, from a run directory holding the provisioner: its row, or `None`
/// when it has no `detect`.
pub async fn detect(
    shell: Arc<dyn Shell>,
    machine: &str,
    provisioner: &Provisioner,
) -> Result<Option<Row>, RunError> {
    let Some(command) = &provisioner.manifest.platform.detect else {
        return Ok(None);
    };
    let mut only = provisioner.clone();
    only.manifest.checks = vec![Check {
        id: "detect".into(),
        description: None,
        command: command.clone(),
        fix: None,
        needs: vec![],
        account: None,
        timeout: super::manifest::DEFAULT_TIMEOUT,
        agent: None,
    }];
    let req = Request {
        machine,
        provisioner: &only,
        accounts: &[],
        agents: &[],
        env: &BTreeMap::new(),
        agent_envs: &[],
    };
    Ok(run(shell, &req).await?.into_iter().next())
}

async fn run_checks(shell: &dyn Shell, req: &Request<'_>, home: &str, dir: &str) -> Vec<Row> {
    let mut rows: Vec<Row> = Vec::new();
    for check in req.provisioner.manifest.checks_for(req.accounts) {
        let runs: Vec<Option<&AgentEnv>> = match &check.agent {
            None => vec![None],
            Some(kind) => req
                .agent_envs
                .iter()
                .filter(|a| &a.kind == kind)
                .map(Some)
                .collect(),
        };
        for agent in runs {
            let name = agent.map(|a| a.name.as_str());
            let unmet = check.needs.iter().find(|n| !need_met(&rows, n, name));
            let mut row = if let Some(need) = unmet {
                Row {
                    id: check.id.clone(),
                    agent: None,
                    status: Status::Skipped,
                    first_line: String::new(),
                    reason: Some(format!("needs {need}, which did not pass")),
                }
            } else {
                match exports(req, agent, home) {
                    Ok(exports) => {
                        let ran = shell
                            .run(
                                CHECK_COMMAND.into(),
                                check_script(dir, &exports, check).into_bytes(),
                                check.timeout,
                            )
                            .await;
                        read_check(check, ran)
                    }
                    Err(why) => Row {
                        id: check.id.clone(),
                        agent: None,
                        status: Status::Error,
                        first_line: String::new(),
                        reason: Some(why),
                    },
                }
            };
            row.agent = name.map(str::to_string);
            rows.push(row);
        }
    }
    rows
}

/// Whether `need` passed for a check run for `agent`: its row for that
/// agent, when it ran for it. Otherwise (a check for no agent, or for
/// another kind) it must have run and passed every time, for no agent or
/// for every agent of its kind.
fn need_met(rows: &[Row], need: &str, agent: Option<&str>) -> bool {
    let of: Vec<&Row> = rows.iter().filter(|r| r.id == need).collect();
    let passed = |r: &Row| r.status == Status::Pass;
    if let Some(own) = of
        .iter()
        .find(|r| agent.is_some() && r.agent.as_deref() == agent)
    {
        return passed(own);
    }
    !of.is_empty() && of.iter().all(|r| passed(r))
}

/// What every check gets in its environment, after its login shell has
/// read the rc files: the request's env, the agent's over it, then
/// pastor's own, which wins.
fn exports(
    req: &Request<'_>,
    agent: Option<&AgentEnv>,
    home: &str,
) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for (key, value) in req.env.iter().chain(agent.into_iter().flat_map(|a| &a.env)) {
        let name_ok = key
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !name_ok {
            return Err(format!("env {key:?} is not a shell variable name"));
        }
        out.push((key.clone(), expand_home(key, value, home)?));
    }
    out.push(("PASTOR_MACHINE".into(), req.machine.into()));
    out.push(("PASTOR_VERSION".into(), env!("CARGO_PKG_VERSION").into()));
    out.push(("PASTOR_AGENTS".into(), req.agents.join(",")));
    Ok(out)
}

/// A leading `~` or `~/` is the machine's home, as for a repo path
/// (`dispatch::expand_home`); `~user` is refused.
fn expand_home(key: &str, value: &str, home: &str) -> Result<String, String> {
    match value.strip_prefix('~') {
        None => Ok(value.to_string()),
        Some("") => Ok(home.to_string()),
        Some(rest) if rest.starts_with('/') => Ok(format!("{}{rest}", home.trim_end_matches('/'))),
        Some(_) => Err(format!(
            "env {key}={value}: only ~ and ~/ are expanded, not ~user; use an absolute path"
        )),
    }
}

/// Checks the base tools, prints the home, sweeps run directories older
/// than a day, makes this run's and unpacks stdin into it. Each answer is a
/// line after `MARK`: `missing <tools>`, `nohome`, `home <path>`,
/// `dir <path>`, then `ready` once unpacked.
fn setup_script() -> String {
    let tools = BASE_TOOLS.join(" ");
    format!(
        r#"missing=; for t in {tools}; do command -v "$t" >/dev/null 2>&1 || missing="$missing $t"; done; printf '%s\n' {MARK}; if [ -n "$missing" ]; then printf 'missing%s\n' "$missing"; exit 3; fi; case "$HOME" in /*) ;; *) echo nohome; exit 3 ;; esac; printf 'home %s\n' "$HOME"; r="$HOME/{SETUP_ROOT}"; umask 077; mkdir -p "$r" || exit 3; if command -v find >/dev/null 2>&1; then find "$r/." ! -name . -prune -type d -mtime +0 -exec rm -rf {{}} + 2>/dev/null; fi; d=$(mktemp -d "$r/XXXXXX" 2>/dev/null || {{ mkdir -p "$r" && mktemp -d "$r/XXXXXX"; }}) || exit 3; printf 'dir %s\n' "$d"; chmod 700 "$d" && cd "$d" && tar -xf - && echo ready"#
    )
}

struct Setup {
    home: String,
    dir: Option<String>,
    ready: bool,
}

fn read_setup(machine: &str, ran: &Ran) -> Result<Setup, RunError> {
    let failed = |why: String| RunError::Failed(format!("machine {machine}: {why}"));
    let stdout = String::from_utf8_lossy(&ran.stdout);
    let Some(answer) = after_mark(&stdout) else {
        // The script never ran: no `sh` there answers 127 from the login
        // shell, which is the one base tool it cannot report itself.
        if ran.code == Some(127) {
            return Err(failed(
                "sh is missing, which pastor needs on the machine".into(),
            ));
        }
        return Err(failed(format!(
            "the run's setup gave no answer ({}){}",
            exit(ran.code),
            stderr_tail(ran)
        )));
    };
    let mut home = None;
    let mut dir = None;
    let mut ready = false;
    for line in answer.lines() {
        if let Some(tools) = line.strip_prefix("missing ") {
            let tools: Vec<&str> = tools.split_whitespace().collect();
            return Err(failed(format!(
                "{} missing, which pastor needs on the machine",
                tools.join(", ")
            )));
        } else if line == "nohome" {
            return Err(failed("$HOME is not an absolute path".into()));
        } else if let Some(h) = line.strip_prefix("home ") {
            home = Some(h.to_string());
        } else if let Some(d) = line.strip_prefix("dir ") {
            dir = Some(d.to_string());
        } else if line == "ready" {
            ready = true;
        }
    }
    let Some(home) = home else {
        return Err(failed(format!(
            "the run's setup did not say the home ({}){}",
            exit(ran.code),
            stderr_tail(ran)
        )));
    };
    // Only a directory just under the setup root is ever removed, whatever
    // the machine printed.
    let root = format!("{home}/{SETUP_ROOT}/");
    let dir = dir.filter(|d| {
        d.strip_prefix(&root).is_some_and(|name| {
            !name.is_empty() && !name.contains('/') && name != "." && name != ".."
        })
    });
    Ok(Setup {
        home,
        dir,
        ready: ready && ran.code == Some(0),
    })
}

/// What runs a check: a login `sh` that reads `check_script` on its stdin.
/// Not `sh -l -c <script>`: the script's own quoting would then sit inside
/// `posix_command`'s, and a fish login shell reads the `\'` that makes
/// inside single quotes as an escaped quote, so no check would run.
pub(crate) const CHECK_COMMAND: &str = "exec sh -l";

/// Runs the check's argv from the run directory, with `~/.local/bin` first
/// on the `PATH` and `exports` set after the rc files, printing `MARK` just
/// before it. The check gets `/dev/null` for stdin, not the rest of the
/// script. stderr goes with stdout: `gh auth status` says what it found
/// there.
fn check_script(dir: &str, exports: &[(String, String)], check: &Check) -> String {
    let mut inner = String::from(r#"PATH="$HOME/.local/bin:$PATH"; export PATH"#);
    for (key, value) in exports {
        inner.push_str(&format!("; {key}={}; export {key}", shell_quote(value)));
    }
    inner.push_str(&format!(
        "; cd {} || exit 3; printf '%s\\n' {MARK}; exec",
        shell_quote(dir)
    ));
    for arg in &check.command {
        inner.push(' ');
        inner.push_str(&shell_quote(arg));
    }
    inner.push_str(" </dev/null 2>&1\n");
    inner
}

fn read_check(check: &Check, ran: Result<Ran, String>) -> Row {
    let row = |status, first_line: String, reason: Option<String>| Row {
        id: check.id.clone(),
        agent: None,
        status,
        first_line,
        reason,
    };
    let ran = match ran {
        Ok(ran) => ran,
        Err(why) => return row(Status::Error, String::new(), Some(why)),
    };
    let stdout = String::from_utf8_lossy(&ran.stdout);
    let Some(own) = after_mark(&stdout) else {
        return row(
            Status::Error,
            String::new(),
            Some(format!(
                "printed nothing ({}): its login shell ended before the check ran{}",
                exit(ran.code),
                stderr_tail(&ran)
            )),
        );
    };
    let first_line: String = own
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    match ran.code {
        Some(0) => row(Status::Pass, first_line, None),
        Some(1) => row(Status::Fail, first_line, None),
        code => row(Status::Error, first_line, Some(exit(code))),
    }
}

/// What follows the first `MARK` line that starts a line.
fn after_mark(stdout: &str) -> Option<&str> {
    let line = format!("{MARK}\n");
    stdout
        .match_indices(&line)
        .map(|(i, _)| i)
        .find(|&i| i == 0 || stdout.as_bytes()[i - 1] == b'\n')
        .map(|i| &stdout[i + line.len()..])
}

pub(crate) fn exit(code: Option<i32>) -> String {
    match code {
        Some(n) => format!("exit {n}"),
        None => "killed by a signal".into(),
    }
}

pub(crate) fn stderr_tail(ran: &Ran) -> String {
    let err = String::from_utf8_lossy(&ran.stderr);
    match err.trim().lines().last() {
        Some(line) if !line.is_empty() => format!(": {line}"),
        _ => String::new(),
    }
}

/// The run directory on the machine, removed by `remove`, or when dropped
/// before that (the run's future cancelled) by a task `finish_cleanups`
/// waits for.
struct RunDir {
    shell: Arc<dyn Shell>,
    machine: String,
    path: Option<String>,
}

impl RunDir {
    fn path(&self) -> &str {
        self.path.as_deref().unwrap_or_default()
    }

    async fn remove(mut self) {
        if let Some(path) = self.path.take() {
            remove_dir(&*self.shell, &self.machine, &path).await;
        }
    }
}

impl Drop for RunDir {
    fn drop(&mut self) {
        let Some(path) = self.path.take() else {
            return;
        };
        let shell = self.shell.clone();
        let machine = std::mem::take(&mut self.machine);
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => {
                let task = rt.spawn(async move { remove_dir(&*shell, &machine, &path).await });
                CLEANUPS
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(task);
            }
            // The next run's sweep takes it after a day.
            Err(_) => {
                tracing::warn!(%machine, dir = %path, "left a run directory behind")
            }
        }
    }
}

/// Removals started by a dropped run, for `finish_cleanups`.
static CLEANUPS: Mutex<Vec<tokio::task::JoinHandle<()>>> = Mutex::new(Vec::new());

/// Wait for the run directories of cancelled runs to be removed: a CLI that
/// stops a run on Ctrl-C calls this before it exits.
pub async fn finish_cleanups() {
    let tasks = std::mem::take(&mut *CLEANUPS.lock().unwrap_or_else(|e| e.into_inner()));
    for task in tasks {
        let _ = task.await;
    }
}

/// Remove the run directory, then the setup root and `~/.cache/pastor`
/// if that leaves them empty: a run leaves nothing behind.
async fn remove_dir(shell: &dyn Shell, machine: &str, path: &str) {
    let script = format!(
        r#"d={}; rm -rf "$d" && {{ rmdir "${{d%/*}}" "${{d%/*/*}}" 2>/dev/null; true; }}"#,
        shell_quote(path)
    );
    match shell.run(script, Vec::new(), PROBE_TIMEOUT).await {
        Ok(ran) if ran.code == Some(0) => {}
        Ok(ran) => tracing::warn!(
            %machine, dir = %path, status = %exit(ran.code), stderr = %String::from_utf8_lossy(&ran.stderr).trim(),
            "could not remove a run directory; the next run removes it after a day"
        ),
        Err(e) => tracing::warn!(
            %machine, dir = %path, error = %e,
            "could not remove a run directory; the next run removes it after a day"
        ),
    }
}

/// The provisioner as it is sent: a copy in a fresh local directory,
/// removed when dropped, so what is packed is what the catalog read and a
/// built-in's manifest is a file like any other.
struct LocalCopy {
    path: PathBuf,
}

impl LocalCopy {
    fn new(p: &Provisioner) -> Result<LocalCopy, String> {
        use std::os::unix::fs::DirBuilderExt;
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!(
            "pastor-setup-{}-{}-{nanos}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let copy = LocalCopy { path };
        match &p.dir {
            Some(dir) => copy_dir(dir, &copy.path, 0)?,
            None => {
                let (_, text) = BUILTIN
                    .iter()
                    .find(|(id, _)| *id == p.id)
                    .ok_or_else(|| format!("no built-in provisioner is called {:?}", p.id))?;
                let file = copy.path.join(MANIFEST_FILE);
                std::fs::write(&file, text).map_err(|e| format!("{}: {e}", file.display()))?;
            }
        }
        Ok(copy)
    }

    async fn tar(&self) -> Result<Vec<u8>, String> {
        tar_dir(&self.path).await
    }
}

impl Drop for LocalCopy {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// `dir` as a tarball, for `unpack`.
pub async fn tar_dir(dir: &Path) -> Result<Vec<u8>, String> {
    let out = tokio::process::Command::new("tar")
        .args(["-cf", "-", "."])
        .current_dir(dir)
        // macOS's tar would add `._` files for extended attributes.
        .env("COPYFILE_DISABLE", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| format!("tar: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "tar of {} failed ({}): {}",
            dir.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

/// Copy `from` into `to`, following symlinks as the catalog does; anything
/// neither a file nor a directory is left out.
fn copy_dir(from: &Path, to: &Path, depth: usize) -> Result<(), String> {
    if depth > 32 {
        return Err(format!(
            "{}: nested too deep (a symlink loop?)",
            from.display()
        ));
    }
    let entries = std::fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", from.display()))?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        let meta = std::fs::metadata(&src).map_err(|e| format!("{}: {e}", src.display()))?;
        if meta.is_dir() {
            std::fs::create_dir(&dst).map_err(|e| format!("{}: {e}", dst.display()))?;
            copy_dir(&src, &dst, depth + 1)?;
        } else if meta.is_file() {
            std::fs::copy(&src, &dst).map_err(|e| format!("{}: {e}", src.display()))?;
        }
    }
    Ok(())
}

/// A fake transport for tests here and in `machine_check`.
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use crate::herdr::transport::posix_command;

    /// The fake transport: the script runs here as ssh hands it to a login
    /// shell, `posix_command` and all, with `HOME` a temp directory standing
    /// for the machine's home, through the same bounded runner as a probe.
    /// `path` replaces the `PATH` the machine's shell starts with; `login`
    /// is the login shell's argv before the command (`sh -c` unless a test
    /// says otherwise); `vars` are exported before it, as the machine's
    /// shell would have them.
    pub struct Fake {
        pub home: PathBuf,
        pub path: Option<String>,
        pub scripts: Mutex<Vec<String>>,
        pub login: Vec<String>,
        pub vars: Vec<(String, String)>,
    }

    impl Fake {
        pub fn new(home: &Path, path: Option<String>) -> Fake {
            Fake {
                home: home.to_path_buf(),
                path,
                scripts: Mutex::new(vec![]),
                login: vec!["/bin/sh".into(), "-c".into()],
                vars: vec![],
            }
        }
    }

    /// This OS as `uname -s` and a manifest's `os` name it.
    pub fn os_here() -> &'static str {
        match std::env::consts::OS {
            "macos" => "darwin",
            os => os,
        }
    }

    impl Shell for Fake {
        fn run(&self, script: String, input: Vec<u8>, time: Duration) -> ShellFuture<'_> {
            self.scripts.lock().unwrap().push(script.clone());
            let mut env = format!(
                "HOME={}; export HOME; ",
                shell_quote(&self.home.to_string_lossy())
            );
            if let Some(path) = &self.path {
                env.push_str(&format!("PATH={}; export PATH; ", shell_quote(path)));
            }
            for (key, value) in &self.vars {
                env.push_str(&format!("{key}={}; export {key}; ", shell_quote(value)));
            }
            let command = env + &script;
            // Inside `posix_command`'s single quotes a fish login shell reads
            // `\'` and `\\` as escapes where sh reads a backslash.
            assert!(
                !command.contains("\\'") && !command.contains("\\\\"),
                "a fish login shell would misread this: {command}"
            );
            let mut argv = self.login.clone();
            argv.push(posix_command(&command));
            Box::pin(async move {
                let out = probe_output_feeding(&argv, &input, time)
                    .await
                    .map_err(|e| e.message)?;
                Ok(Ran {
                    code: out.status.code(),
                    stdout: out.stdout,
                    stderr: out.stderr,
                })
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::Fake;
    use super::*;
    use crate::provision::catalog::load_manifest;

    struct Machine {
        _tmp: tempfile::TempDir,
        home: PathBuf,
        fake: Arc<Fake>,
        provisioner: Provisioner,
    }

    impl Machine {
        fn setup_root(&self) -> PathBuf {
            self.home.join(SETUP_ROOT)
        }

        /// The run directories left on the machine.
        fn run_dirs(&self) -> Vec<String> {
            let mut names: Vec<String> = match std::fs::read_dir(self.setup_root()) {
                Ok(entries) => entries
                    .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                    .collect(),
                Err(_) => vec![],
            };
            names.sort();
            names
        }

        async fn run(&self, env: &BTreeMap<String, String>) -> Result<Vec<Row>, RunError> {
            self.run_for(env, &[]).await
        }

        async fn run_for(
            &self,
            env: &BTreeMap<String, String>,
            agent_envs: &[AgentEnv],
        ) -> Result<Vec<Row>, RunError> {
            let agents = vec!["claude".to_string(), "codex".to_string()];
            let req = Request {
                machine: "pi-3",
                provisioner: &self.provisioner,
                accounts: &[],
                agents: &agents,
                env,
                agent_envs,
            };
            run(self.fake.clone(), &req).await
        }
    }

    /// A machine whose provisioner is the given checks, copied by hand into
    /// a directory with a helper script beside the manifest.
    fn machine(checks: &str) -> Machine {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let dir = tmp.path().join("prov").join("test");
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(
            dir.join(MANIFEST_FILE),
            format!("id = \"test\"\nversion = \"0.1.0\"\n[platform]\nos = \"linux\"\n{checks}"),
        )
        .unwrap();
        std::fs::write(
            dir.join("bin/hello.sh"),
            "echo hello from the provisioner\n",
        )
        .unwrap();
        let manifest = load_manifest(&dir, Some("test")).unwrap();
        Machine {
            fake: Arc::new(Fake::new(&home, None)),
            home,
            _tmp: tmp,
            provisioner: Provisioner {
                id: "test".into(),
                dir: Some(dir),
                manifest,
            },
        }
    }

    fn check(id: &str, script: &str) -> String {
        format!(
            "[[check]]\nid = \"{id}\"\ncommand = [\"sh\", \"-c\", {}]\n",
            toml::Value::String(script.into())
        )
    }

    fn row<'a>(rows: &'a [Row], id: &str) -> &'a Row {
        rows.iter().find(|r| r.id == id).unwrap()
    }

    /// Exit 0 passes, 1 needs fixing and anything else is an error; a check
    /// whose need did not pass is skipped; the rows come in file order and
    /// the run directory is gone after.
    #[tokio::test]
    async fn checks_run_in_order_and_the_dir_is_removed() {
        let m = machine(&format!(
            "{}{}{}{}[[check]]\nid = \"after\"\ncommand = [\"true\"]\nneeds = [\"fail\"]\n{}",
            check("pass", "echo all good; echo second line"),
            check("fail", "echo git is missing; exit 1"),
            check("error", "echo broke >&2; exit 2"),
            check("silent", "exit 0"),
            check("script", "sh bin/hello.sh"),
        ));
        let rows = m.run(&BTreeMap::new()).await.unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["pass", "fail", "error", "silent", "after", "script"]);
        let pass = row(&rows, "pass");
        assert_eq!(
            (pass.status, pass.first_line.as_str()),
            (Status::Pass, "all good")
        );
        assert_eq!(pass.reason, None);
        let fail = row(&rows, "fail");
        assert_eq!(
            (fail.status, fail.first_line.as_str()),
            (Status::Fail, "git is missing")
        );
        let error = row(&rows, "error");
        assert_eq!(
            (error.status, error.first_line.as_str()),
            (Status::Error, "broke")
        );
        assert_eq!(error.reason.as_deref(), Some("exit 2"));
        // `sudo -n true` says nothing when it passes.
        let silent = row(&rows, "silent");
        assert_eq!(
            (silent.status, silent.first_line.as_str()),
            (Status::Pass, "")
        );
        let after = row(&rows, "after");
        assert_eq!(after.status, Status::Skipped);
        assert!(
            after.reason.as_deref().unwrap().contains("fail"),
            "{after:?}"
        );
        // The whole copied directory is there, and checks start in it.
        let script = row(&rows, "script");
        assert_eq!(
            (script.status, script.first_line.as_str()),
            (Status::Pass, "hello from the provisioner")
        );
        // The run dir goes, and the setup root and `~/.cache/pastor` with it.
        assert!(!m.setup_root().exists());
        assert!(!m.home.join(".cache/pastor").exists());
        assert!(m.home.join(".cache").is_dir());
        // Never a fix: the setup, five checks (one skipped) and the removal.
        assert_eq!(m.fake.scripts.lock().unwrap().len(), 1 + 5 + 1);
    }

    /// The run directory and its root are the machine user's alone.
    #[tokio::test]
    async fn the_run_dir_is_private() {
        let m = machine(&check(
            "mode",
            r#"echo $(stat -c %a . .. 2>/dev/null || stat -f %Lp . ..)"#,
        ));
        let rows = m.run(&BTreeMap::new()).await.unwrap();
        let modes = &row(&rows, "mode").first_line;
        assert_eq!(modes.split_whitespace().next(), Some("700"), "{modes}");
        let root = u32::from_str_radix(modes.split_whitespace().nth(1).unwrap(), 8).unwrap();
        assert_eq!(root & 0o077, 0, "{modes}");
    }

    /// A check that prints nothing at all, not even the marker (its login
    /// shell exited first), 70 KiB, or never exits is an error with its
    /// reason; the other checks still run and the directory still goes.
    #[tokio::test]
    async fn a_misbehaving_check_is_an_error_row() {
        let m = machine(&format!(
            "{}{}[[check]]\nid = \"hang\"\ncommand = [\"sleep\", \"30\"]\ntimeout = \"1s\"\n{}",
            // Leaves the file that makes the next login shell exit early.
            check(
                "flood",
                "touch \"$HOME/exit-early\"; head -c 71680 /dev/zero"
            ),
            check("nothing", "echo never seen"),
            check("ok", "echo fine"),
        ));
        std::fs::write(
            m.home.join(".profile"),
            "if [ -e \"$HOME/exit-early\" ]; then rm -f \"$HOME/exit-early\"; exit 0; fi\n",
        )
        .unwrap();
        let started = std::time::Instant::now();
        let rows = m.run(&BTreeMap::new()).await.unwrap();
        assert!(started.elapsed() < Duration::from_secs(20));
        let flood = row(&rows, "flood");
        assert_eq!(flood.status, Status::Error);
        assert!(
            flood
                .reason
                .as_deref()
                .unwrap()
                .contains("more than 65536 bytes"),
            "{flood:?}"
        );
        let nothing = row(&rows, "nothing");
        assert_eq!(nothing.status, Status::Error);
        assert!(
            nothing
                .reason
                .as_deref()
                .unwrap()
                .contains("printed nothing"),
            "{nothing:?}"
        );
        let hang = row(&rows, "hang");
        assert_eq!(hang.status, Status::Error);
        assert!(
            hang.reason
                .as_deref()
                .unwrap()
                .contains("timed out after 1s"),
            "{hang:?}"
        );
        assert_eq!(row(&rows, "ok").status, Status::Pass);
        assert_eq!(m.run_dirs(), Vec::<String>::new());
    }

    /// A run dropped half way (Ctrl-C) still removes its directory.
    #[tokio::test]
    async fn a_cancelled_run_removes_its_dir() {
        let m = machine(&check("slow", "touch \"$HOME/started\"; sleep 30"));
        let env = BTreeMap::new();
        let mut run = Box::pin(m.run(&env));
        let started = m.home.join("started");
        for _ in 0..200 {
            tokio::select! {
                r = &mut run => panic!("the run ended: {r:?}"),
                () = tokio::time::sleep(Duration::from_millis(25)) => {}
            }
            if started.exists() {
                break;
            }
        }
        assert!(started.exists(), "the check never started");
        assert_eq!(m.run_dirs().len(), 1);
        drop(run);
        finish_cleanups().await;
        assert_eq!(m.run_dirs(), Vec::<String>::new());
    }

    /// A run directory left over more than a day goes with the next run; a
    /// newer one, perhaps another run's, stays.
    #[tokio::test]
    async fn a_stale_run_dir_is_swept() {
        let m = machine(&check("ok", "true"));
        let root = m.setup_root();
        std::fs::create_dir_all(root.join("old123/inner")).unwrap();
        std::fs::create_dir_all(root.join("new456")).unwrap();
        let touched = std::process::Command::new("touch")
            .args(["-t", "202001010000"])
            .arg(root.join("old123"))
            .status()
            .unwrap();
        assert!(touched.success());
        m.run(&BTreeMap::new()).await.unwrap();
        assert_eq!(m.run_dirs(), ["new456"]);
    }

    /// What `~/.profile` prints is not the status, and what it sets for
    /// `PATH` does not push `~/.local/bin` back; pastor's variables are set.
    #[tokio::test]
    async fn the_login_shell_banner_is_not_a_status() {
        let m = machine(&format!(
            "{}{}",
            check("which", "echo found it"),
            check(
                "env",
                r#"printf '%s|%s|%s|%s\n' "${PATH%%:*}" "$PASTOR_MACHINE" "$PASTOR_AGENTS" "$PASTOR_VERSION""#
            ),
        ));
        std::fs::write(
            m.home.join(".profile"),
            "echo 'Welcome to pi-3, last login yesterday'\necho pastor-check-5d1a-not-quite\nPATH=/usr/bin:/bin\n",
        )
        .unwrap();
        let rows = m.run(&BTreeMap::new()).await.unwrap();
        assert_eq!(row(&rows, "which").first_line, "found it");
        assert_eq!(
            row(&rows, "env").first_line,
            format!(
                "{}/.local/bin|pi-3|claude,codex|{}",
                m.home.display(),
                env!("CARGO_PKG_VERSION")
            )
        );
    }

    /// An agent's env reaches each check with `~` expanded against the
    /// machine's home; one that cannot be expanded fails the checks, with
    /// the reason, but not the run.
    #[tokio::test]
    async fn an_agents_env_is_expanded() {
        let m = machine(&check("dir", r#"echo "$CLAUDE_CONFIG_DIR""#));
        let env = BTreeMap::from([("CLAUDE_CONFIG_DIR".to_string(), "~/.claude-alt".to_string())]);
        let rows = m.run(&env).await.unwrap();
        let dir = row(&rows, "dir");
        assert_eq!(dir.status, Status::Pass);
        assert_eq!(dir.first_line, format!("{}/.claude-alt", m.home.display()));

        let env = BTreeMap::from([("CLAUDE_CONFIG_DIR".to_string(), "~bob/.claude".to_string())]);
        let rows = m.run(&env).await.unwrap();
        let dir = row(&rows, "dir");
        assert_eq!(dir.status, Status::Error);
        assert!(
            dir.reason.as_deref().unwrap().contains("~bob/.claude"),
            "{dir:?}"
        );
        assert_eq!(m.run_dirs(), Vec::<String>::new());
    }

    fn agent_env(name: &str, kind: &str, env: &[(&str, &str)]) -> AgentEnv {
        AgentEnv {
            name: name.into(),
            kind: kind.into(),
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    /// A check for a kind runs once per agent of it, with that agent's env
    /// over the run's, and its row names the agent; a kind with no agent
    /// there gives no row. Its `needs` are met per agent.
    #[tokio::test]
    async fn a_kind_check_runs_once_per_agent_with_its_env() {
        let m = machine(&format!(
            "{}[[check]]\nid = \"login\"\nagent = \"claude\"\ncommand = [\"sh\", \"-c\", \"echo $CLAUDE_CONFIG_DIR $BASE; test $CLAUDE_CONFIG_DIR != {home}/.claude-broken\"]\n\
             [[check]]\nid = \"after\"\nagent = \"claude\"\nneeds = [\"login\"]\ncommand = [\"true\"]\n\
             [[check]]\nid = \"codex-login\"\nagent = \"codex\"\ncommand = [\"true\"]\n",
            check("base", "true"),
            home = "$HOME",
        ));
        let env = BTreeMap::from([("BASE".to_string(), "everyone".to_string())]);
        let agents = [
            agent_env("claude", "claude", &[("CLAUDE_CONFIG_DIR", "~/.claude")]),
            agent_env(
                "claude-broken",
                "claude",
                &[("CLAUDE_CONFIG_DIR", "~/.claude-broken")],
            ),
        ];
        let rows = m.run_for(&env, &agents).await.unwrap();
        let got: Vec<(&str, Option<&str>, Status)> = rows
            .iter()
            .map(|r| (r.id.as_str(), r.agent.as_deref(), r.status))
            .collect();
        assert_eq!(
            got,
            [
                ("base", None, Status::Pass),
                ("login", Some("claude"), Status::Pass),
                ("login", Some("claude-broken"), Status::Fail),
                ("after", Some("claude"), Status::Pass),
                ("after", Some("claude-broken"), Status::Skipped),
            ],
            "{rows:?}"
        );
        let home = m.home.display();
        assert_eq!(rows[1].first_line, format!("{home}/.claude everyone"));
        assert_eq!(
            rows[2].first_line,
            format!("{home}/.claude-broken everyone")
        );
        assert_eq!(m.run_dirs(), Vec::<String>::new());
    }

    /// A check's need is met per agent when both run for the same one;
    /// anywhere else (a check for no agent, or for another kind) only when
    /// the needed check ran and passed every time. A kind check may need a
    /// plain check. An agent's env wins over the run's for a key both set.
    #[tokio::test]
    async fn needs_across_agents_and_kinds() {
        let kind = |id: &str, kind: &str, script: &str, needs: &str| {
            format!(
                "[[check]]\nid = \"{id}\"\nagent = \"{kind}\"\nneeds = [{needs}]\ncommand = [\"sh\", \"-c\", {}]\n",
                toml::Value::String(script.into())
            )
        };
        let m = machine(&format!(
            "{}{}{}{}{}{}[[check]]\nid = \"all-logins\"\nneeds = [\"login\"]\ncommand = [\"true\"]\n{}{}{}",
            check("base", "true"),
            check("base-fail", "exit 1"),
            kind("login", "claude", "test \"$OK\" = yes", ""),
            kind("on-base", "claude", "true", "\"base\""),
            kind("on-base-fail", "claude", "true", "\"base-fail\""),
            kind("shared", "codex", "echo $SHARED", ""),
            kind("codex-on-login", "codex", "true", "\"login\""),
            kind("codex-on-base", "codex", "true", "\"on-base\""),
            kind("claude-on-codex", "claude", "true", "\"shared\""),
        ));
        let env = BTreeMap::from([("SHARED".to_string(), "run".to_string())]);
        let agents = [
            agent_env("claude", "claude", &[("OK", "yes")]),
            agent_env("claude-main", "claude", &[("OK", "no")]),
            agent_env("codex", "codex", &[("SHARED", "agent")]),
        ];
        let rows = m.run_for(&env, &agents).await.unwrap();
        let got: Vec<(&str, Option<&str>, Status)> = rows
            .iter()
            .map(|r| (r.id.as_str(), r.agent.as_deref(), r.status))
            .collect();
        assert_eq!(
            got,
            [
                ("base", None, Status::Pass),
                ("base-fail", None, Status::Fail),
                ("login", Some("claude"), Status::Pass),
                ("login", Some("claude-main"), Status::Fail),
                ("on-base", Some("claude"), Status::Pass),
                ("on-base", Some("claude-main"), Status::Pass),
                ("on-base-fail", Some("claude"), Status::Skipped),
                ("on-base-fail", Some("claude-main"), Status::Skipped),
                ("shared", Some("codex"), Status::Pass),
                ("all-logins", None, Status::Skipped),
                ("codex-on-login", Some("codex"), Status::Skipped),
                ("codex-on-base", Some("codex"), Status::Pass),
                ("claude-on-codex", Some("claude"), Status::Pass),
                ("claude-on-codex", Some("claude-main"), Status::Pass),
            ],
            "{rows:?}"
        );
        assert_eq!(row(&rows, "shared").first_line, "agent");
    }

    /// The machine's `uname -s` and os-release `ID`, after whatever its rc
    /// files print.
    #[tokio::test]
    async fn facts_are_the_os_and_distro() {
        let m = machine(&check("ok", "true"));
        std::fs::write(m.home.join(".profile"), "echo welcome to the pi\n").unwrap();
        let facts = facts(&*m.fake, "pi-3").await.unwrap();
        assert_eq!(facts.os, testing::os_here());
        let arch = std::env::consts::ARCH;
        assert_eq!(facts.arch, arch);
        assert_eq!(facts.bits, 64);
        let release = std::fs::read_to_string("/etc/os-release").ok();
        let id = release.as_deref().and_then(|r| {
            r.lines()
                .find_map(|l| l.strip_prefix("ID="))
                .map(|v| v.trim_matches('"').to_string())
        });
        assert_eq!(facts.distro, id);
        let like = release.as_deref().and_then(|r| {
            r.lines().find_map(|l| l.strip_prefix("ID_LIKE=")).map(|v| {
                v.trim_matches('"')
                    .split_whitespace()
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
        });
        assert_eq!(facts.like, like.unwrap_or_default());
    }

    /// macOS's `arm64` is `aarch64`; a `LONG_BIT` that is not a number, or
    /// none, counts as 64; a Pi's 32-bit userland stays 32.
    #[test]
    fn facts_read_the_probe() {
        let mac = Facts::read("Darwin\narm64\n64\n").unwrap();
        assert_eq!(
            mac,
            Facts {
                os: "darwin".into(),
                arch: "aarch64".into(),
                bits: 64,
                distro: None,
                like: vec![],
            }
        );
        let pi = Facts::read("Linux\naarch64\n32\nraspbian\ndebian\n").unwrap();
        assert_eq!((pi.bits, pi.distro.as_deref()), (32, Some("raspbian")));
        assert_eq!(pi.like, ["debian"]);
        assert_eq!(Facts::read("Linux\nx86_64\n\n").unwrap().bits, 64);
        assert_eq!(Facts::read("\n"), None);
    }

    /// `detect` runs from a run directory holding the provisioner, so it
    /// may call a script shipped beside the manifest.
    #[tokio::test]
    async fn detect_runs_like_a_check() {
        let mut m = machine(&check("ok", "true"));
        assert_eq!(
            detect(m.fake.clone(), "pi-3", &m.provisioner)
                .await
                .unwrap(),
            None
        );
        m.provisioner.manifest.platform.detect = Some(vec![
            "sh".into(),
            "-c".into(),
            "sh bin/hello.sh; exit 1".into(),
        ]);
        let row = detect(m.fake.clone(), "pi-3", &m.provisioner)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (row.status, row.first_line.as_str()),
            (Status::Fail, "hello from the provisioner")
        );
        assert_eq!(m.run_dirs(), Vec::<String>::new());
    }

    /// sshd hands the command to the user's login shell, which may be
    /// fish: the checks run under it as under sh, quotes in their words
    /// and env included. Skipped where fish is not installed; the fake's
    /// own look at every script covers what fish would misread.
    #[tokio::test]
    async fn a_fish_login_shell_runs_the_checks() {
        let Some(fish) = [
            "/usr/bin/fish",
            "/usr/local/bin/fish",
            "/opt/homebrew/bin/fish",
        ]
        .into_iter()
        .find(|p| Path::new(p).exists()) else {
            eprintln!("no fish here; skipped");
            return;
        };
        let mut m = machine(&format!(
            "{}{}",
            check("quotes", "echo \"it's $NOTE\""),
            check("script", "sh bin/hello.sh"),
        ));
        let mut fake = Fake::new(&m.home, None);
        fake.login = vec![fish.into(), "--no-config".into(), "-c".into()];
        m.fake = Arc::new(fake);
        let env = BTreeMap::from([("NOTE".to_string(), "a 'quoted' word".to_string())]);
        let rows = m.run(&env).await.unwrap();
        let quotes = row(&rows, "quotes");
        assert_eq!(
            (quotes.status, quotes.first_line.as_str()),
            (Status::Pass, "it's a 'quoted' word"),
            "{quotes:?}"
        );
        let script = row(&rows, "script");
        assert_eq!(
            (script.status, script.first_line.as_str()),
            (Status::Pass, "hello from the provisioner"),
            "{script:?}"
        );
        assert_eq!(m.run_dirs(), Vec::<String>::new());
    }

    /// A machine without one of the base tools fails the run with its name,
    /// before anything is made there.
    #[tokio::test]
    async fn a_missing_base_tool_fails_the_run() {
        let mut m = machine(&check("ok", "true"));
        let bin = m.home.parent().unwrap().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink("/bin/sh", bin.join("sh")).unwrap();
        m.fake = Arc::new(Fake::new(&m.home, Some(bin.to_string_lossy().into_owned())));
        let err = m.run(&BTreeMap::new()).await.unwrap_err();
        assert_eq!(err.code(), "runtime_error");
        let message = err.to_string();
        assert!(
            message.contains("tar, mktemp, rm, chmod missing"),
            "{message}"
        );
        assert!(!m.setup_root().exists());
    }

    /// No `sh` on the machine is the one tool the script cannot report: the
    /// login shell answers 127.
    #[test]
    fn no_sh_is_named_too() {
        let ran = Ran {
            code: Some(127),
            stdout: vec![],
            stderr: b"bash: sh: command not found\n".to_vec(),
        };
        let Err(err) = read_setup("pi-3", &ran) else {
            panic!("no error");
        };
        assert!(err.to_string().contains("sh is missing"), "{err}");
    }

    /// The machine's answer only ever removes a directory just under the
    /// setup root.
    #[test]
    fn only_a_run_dir_under_the_root_is_trusted() {
        for (dir, ok) in [
            ("/h/.cache/pastor/setup/abc123", true),
            ("/h/.cache/pastor/setup/", false),
            ("/h/.cache/pastor/setup/..", false),
            ("/h/.cache/pastor/setup/a/b", false),
            ("/h", false),
        ] {
            let ran = Ran {
                code: Some(0),
                stdout: format!("{MARK}\nhome /h\ndir {dir}\nready\n").into_bytes(),
                stderr: vec![],
            };
            let setup = read_setup("m", &ran).ok().unwrap();
            assert_eq!(setup.dir.is_some(), ok, "{dir}");
        }
    }

    /// A built-in provisioner goes over as its manifest.
    #[tokio::test]
    async fn a_built_in_is_sent_as_its_manifest() {
        let catalog = crate::provision::Catalog::builtin();
        let arch = catalog.get("arch").unwrap();
        let copy = LocalCopy::new(arch).unwrap();
        let text = std::fs::read_to_string(copy.path.join(MANIFEST_FILE)).unwrap();
        assert_eq!(
            text,
            BUILTIN.iter().find(|(id, _)| *id == "arch").unwrap().1
        );
        let tarball = copy.tar().await.unwrap();
        assert!(!tarball.is_empty());
        let path = copy.path.clone();
        drop(copy);
        assert!(!path.exists());
    }

    /// A machine reached through a command bridge, or a pull machine, has
    /// no shell pastor can run checks in.
    #[test]
    fn a_bridge_machine_is_unsupported() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        for text in [
            "name = \"b\"\ncommand = [\"fake-herdr\", \"bridge\"]\n",
            "name = \"b\"\npull = true\n",
        ] {
            let m: MachineConfig = toml::from_str(text).unwrap();
            let Err(err) = MachineShell::new(&m, &paths) else {
                panic!("{text}: accepted");
            };
            assert_eq!(err.code(), MACHINE_CHECK_UNSUPPORTED, "{text}");
            assert!(err.to_string().contains("machine b"), "{err}");
        }
        let m: MachineConfig = toml::from_str("name = \"here\"\nlocal = true\n").unwrap();
        assert!(MachineShell::new(&m, &paths).is_ok());
    }

    /// A local machine runs the same scripts under `sh -c`, no ssh.
    #[tokio::test]
    async fn a_local_machine_runs_under_sh() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        let m: MachineConfig = toml::from_str("name = \"here\"\nlocal = true\n").unwrap();
        let shell = MachineShell::new(&m, &paths).unwrap();
        let ran = shell
            .run(
                "printf '%s\\n' \"$0\"; cat".into(),
                b"fed".to_vec(),
                PROBE_TIMEOUT,
            )
            .await
            .unwrap();
        assert_eq!(ran.code, Some(0));
        assert_eq!(String::from_utf8_lossy(&ran.stdout), "sh\nfed");
    }
}
