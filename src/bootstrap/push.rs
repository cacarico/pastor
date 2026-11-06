//! Push bootstrap: the head sets a machine up over ssh, as its login user.
//! `machine add --bootstrap` and `machine bootstrap` both end here.
//!
//! The head pins the machine's host key, reads its platform, copies pastor
//! and herdr from its own checked cache over its own ssh (never through the
//! daemon socket), runs `pastor bootstrap --json` there and records what it
//! printed through the path a pull machine's `BootstrapReport` takes
//! (`Fleet::report_bootstrap`), so state and events are written once. The
//! login user does everything: nothing here calls sudo.
//!
//! Every value that goes into a remote command (the head's address, its key,
//! a checksum) is shell-quoted: the remote login shell parses the line, and
//! none of them is pastor's own text.
//!
//! A host key is pinned once, in pastor's own `known_hosts`, and every ssh
//! after it checks the machine against that pin strictly, so a changed key
//! stops the push at its first connection. It is never replaced by a rerun:
//! a machine that was reinstalled is removed and added again.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;

use super::{Isolation, Outcome, StepResult, StepStatus};
use crate::artifacts::{Artifact, Fetch};
use crate::cli::CliError;
use crate::config::Paths;
use crate::config::flock::MachineConfig;
use crate::daemon::Fleet;
use crate::herdr::transport::shell_quote;
use crate::setup::CmdOutput;
use crate::store::{BootstrapState, MachineState, Store};

/// Where pastor lands on the machine before it runs: under the login user's
/// home. The remote shell expands `$HOME`; nothing else in it varies.
const REMOTE_DIR: &str = "\"$HOME\"/.cache/pastor-bootstrap";

/// Where herdr lands: the directory `pastor bootstrap` looks for it in.
const REMOTE_BIN: &str = "\"$HOME\"/.local/bin";

/// Runs one program on the head, with stdin from a file or from nothing.
/// The copy step needs the file; tests record the calls instead.
pub trait PushRunner: Send + Sync {
    fn run(
        &self,
        program: &str,
        args: &[String],
        stdin: Option<&Path>,
    ) -> std::io::Result<CmdOutput>;
}

pub struct SystemPushRunner;

impl PushRunner for SystemPushRunner {
    fn run(
        &self,
        program: &str,
        args: &[String],
        stdin: Option<&Path>,
    ) -> std::io::Result<CmdOutput> {
        let stdin = match stdin {
            Some(path) => std::process::Stdio::from(std::fs::File::open(path)?),
            None => std::process::Stdio::null(),
        };
        let out = std::process::Command::new(program)
            .args(args)
            .stdin(stdin)
            .output()?;
        Ok(CmdOutput {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

/// What a push needs besides the machine.
pub struct PushOpts {
    /// `--host-key`: the machine's ed25519 host key must be this, as a
    /// `SHA256:` fingerprint or a `ssh-ed25519 AAAA…` key. Unset, the key
    /// is trusted the first time it is seen.
    pub host_key: Option<String>,
    /// `machine add --bootstrap`: the entry to write when the flock has no
    /// machine of that name. `None` for `machine bootstrap`, whose machine
    /// must exist.
    pub machine: Option<MachineConfig>,
    /// The head's ssh destination as the machine reaches it
    /// (`head_address` in pastor.toml).
    pub head: String,
    /// The head's public key line, for the login user's authorized_keys
    /// there, when the head has one.
    pub head_key: Option<String>,
    pub artifacts: crate::config::ArtifactsConfig,
    /// The herdr versions the fleet's machines report, for
    /// `artifacts::herdr_version`.
    pub fleet_versions: Vec<String>,
    /// The head's `authorized_keys`, for the machine's agent line.
    pub authorized_keys: PathBuf,
    pub runner: Arc<dyn PushRunner>,
    pub fetch: Arc<dyn Fetch + Send>,
}

/// The step that stopped a push on the head's side, before or after the
/// machine's own steps.
struct Stop(StepResult);

impl Stop {
    fn new(step: &str, why: impl Into<String>, fix: impl Into<String>) -> Stop {
        Stop(StepResult::failed(step, why, fix))
    }
}

/// The destination without its user. An ssh config alias stays an alias, so
/// its `Port` and `HostName` still apply.
fn host_part(login: &str) -> &str {
    login.rsplit_once('@').map_or(login, |(_, host)| host)
}

/// The machines a push is running for: a second push of one is refused
/// (`machine_busy`) rather than racing the first on the same files.
static BUSY: std::sync::Mutex<Vec<(PathBuf, String)>> = std::sync::Mutex::new(Vec::new());

struct Busy(PathBuf, String);

impl Busy {
    /// Keyed by the head's state dir too: it is one head's machines.
    fn take(paths: &Paths, name: &str) -> anyhow::Result<Busy> {
        let key = (paths.state_dir.clone(), name.to_string());
        let mut busy = BUSY.lock().unwrap_or_else(|e| e.into_inner());
        if busy.contains(&key) {
            return Err(CliError::err(
                "machine_busy",
                format!("machine {name} is being bootstrapped already; wait for it to end"),
            ));
        }
        busy.push(key);
        Ok(Busy(paths.state_dir.clone(), name.to_string()))
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        let mut busy = BUSY.lock().unwrap_or_else(|e| e.into_inner());
        busy.retain(|b| *b != (self.0.clone(), self.1.clone()));
    }
}

/// Set `name` up over ssh as `login`, and record how it ended. A failed
/// step comes back as an `Outcome` with `ok: false`, already recorded as
/// `Failed`; an `Err` is a push that never started (`machine_exists`, an
/// unknown machine) or a store that would not write.
///
/// A machine already `Ok` (a rerun or an upgrade) stays `Ok` until its own
/// steps start: a failure before that changed nothing there, so it only
/// emits `machine.bootstrap_failed`.
pub async fn push(
    paths: &Paths,
    store: &Store,
    fleet: &Fleet,
    name: &str,
    login: &str,
    opts: PushOpts,
) -> anyhow::Result<Outcome> {
    let _busy = Busy::take(paths, name)?;
    let before = record(paths, store, fleet, name, login, &opts).await?;
    let stored = before.as_ref().and_then(|s| s.host_key.clone());
    let was_ok = before
        .as_ref()
        .is_some_and(|s| s.bootstrap == BootstrapState::Ok);
    let pinned = match pin_host_key(paths, name, login, stored.as_deref(), &opts).await {
        Ok(pinned) => pinned,
        Err(stop) => {
            let outcome = stopped(stop, None);
            if was_ok {
                fleet.report_bootstrap_kept(name, &outcome)?;
            } else {
                fleet.report_bootstrap(name, outcome.clone())?;
            }
            close_master(paths, name, login, &opts.runner).await;
            return Ok(outcome);
        }
    };
    if let Err(stop) = copy_binaries(paths, name, login, &opts).await {
        let outcome = {
            let mut o = stopped(stop, None);
            o.host_key = Some(pinned.clone());
            o
        };
        if was_ok {
            fleet.report_bootstrap_kept(name, &outcome)?;
        } else {
            fleet.report_bootstrap(name, outcome.clone())?;
        }
        close_master(paths, name, login, &opts.runner).await;
        return Ok(outcome);
    }
    // The machine's own steps start now: an `Ok` machine is no longer one
    // until they end well.
    if was_ok {
        store.put_machine_state(&pending(name, before.as_ref()))?;
    }
    let mut finished = false;
    let outcome = match run_bootstrap(paths, name, login, &opts).await {
        Ok(mut outcome) => {
            outcome.ok = succeeded(&outcome);
            // What is stored is the key pastor scanned and pinned, never
            // the machine's own word for it.
            let reported = outcome.host_key.replace(pinned.clone());
            match reported.filter(|k| !same_key(k, &pinned)) {
                Some(other) if outcome.ok => stopped(
                    Stop::new(
                        "host_key",
                        format!(
                            "the machine reports host key {other}, not the {pinned} pastor scanned and pinned"
                        ),
                        "check the machine's /etc/ssh/ssh_host_ed25519_key.pub on its console",
                    ),
                    Some(outcome),
                ),
                _ if outcome.ok => match register(paths, fleet, name, &outcome, &opts) {
                    Ok(()) => {
                        finished = true;
                        outcome
                    }
                    Err(stop) => stopped(stop, Some(outcome)),
                },
                _ => outcome,
            }
        }
        Err(stop) => {
            let mut o = stopped(stop, None);
            o.host_key = Some(pinned.clone());
            o
        }
    };
    if let Err(err) = fleet.report_bootstrap(name, outcome.clone()) {
        // Not on record as ok, so its agent line does not stay either.
        if finished {
            let marker = crate::authkeys::Marker::Agent(name.to_string());
            if let Err(undo) = crate::authkeys::remove(&opts.authorized_keys, &marker) {
                tracing::warn!(machine = %name, %undo, "could not take the agent line back");
            }
        }
        return Err(err);
    }
    close_master(paths, name, login, &opts.runner).await;
    Ok(outcome)
}

/// Ok, and no step says otherwise.
fn succeeded(outcome: &Outcome) -> bool {
    outcome.ok && outcome.steps.iter().all(|s| s.status != StepStatus::Failed)
}

/// Two `keytype base64 [comment]` keys are the same key.
fn same_key(a: &str, b: &str) -> bool {
    let mut a = a.split_whitespace();
    let mut b = b.split_whitespace();
    (a.next(), a.next()) == (b.next(), b.next())
}

/// The pending row for `name`, keeping what `before` knew about it.
fn pending(name: &str, before: Option<&MachineState>) -> MachineState {
    MachineState {
        machine: name.to_string(),
        bootstrap: BootstrapState::Pending,
        failed_step: None,
        why: None,
        updated_at: chrono::Utc::now(),
        ..before.cloned().unwrap_or(MachineState {
            machine: name.to_string(),
            bootstrap: BootstrapState::Pending,
            isolation: None,
            failed_step: None,
            why: None,
            host_key: None,
            herdr_version: None,
            pastor_version: None,
            generation: 1,
            updated_at: chrono::Utc::now(),
        })
    }
}

/// A failed outcome for a step the head ran, keeping what the machine
/// reported when it got that far.
fn stopped(Stop(step): Stop, from: Option<Outcome>) -> Outcome {
    let mut o = from.unwrap_or(Outcome {
        ok: false,
        isolation: Isolation::None,
        steps: Vec::new(),
        machine_key: None,
        agent_key: None,
        host_key: None,
        herdr_version: None,
        pastor_version: None,
        check: None,
    });
    o.ok = false;
    o.steps.push(step);
    o
}

/// Step 1: the attempt is on record before anything touches the machine,
/// so dispatch skips it from the moment its entry exists. A rerun of a
/// pending or failed `machine add --bootstrap` with the same login goes on;
/// anything else of that name is `machine_exists`. A machine already `Ok`
/// is left `Ok` here (`push` marks it pending when its own steps start).
/// Returns the row as it was.
async fn record(
    paths: &Paths,
    store: &Store,
    fleet: &Fleet,
    name: &str,
    login: &str,
    opts: &PushOpts,
) -> anyhow::Result<Option<MachineState>> {
    let entry = fleet.flock().get(name).cloned();
    let before = store.machine_state(name)?;
    let exists = || {
        CliError::err(
            "machine_exists",
            format!(
                "machine {name} already exists; to set it up again run `pastor machine bootstrap {name}`"
            ),
        )
    };
    match (&entry, &opts.machine) {
        (Some(entry), Some(_)) => {
            let resumable = entry.ssh.as_deref() == Some(login)
                && before.as_ref().is_some_and(|s| {
                    matches!(
                        s.bootstrap,
                        BootstrapState::Pending | BootstrapState::Failed
                    )
                });
            if !resumable {
                return Err(exists());
            }
        }
        (Some(_), None) => {}
        (None, None) => {
            return Err(CliError::err(
                "unknown_machine",
                format!("no machine {name} in the flock"),
            ));
        }
        (None, Some(_)) => {}
    }
    if !before
        .as_ref()
        .is_some_and(|s| s.bootstrap == BootstrapState::Ok)
    {
        store.put_machine_state(&pending(name, before.as_ref()))?;
    }
    if let (None, Some(m)) = (&entry, &opts.machine) {
        let catalogs = crate::provision::Catalog::load(paths)
            .and_then(|p| Ok((p, crate::bundle::Catalog::load(paths)?)));
        let added = match catalogs {
            Ok((provisioners, bundles)) => {
                fleet
                    .edit_flock_file(&paths.flock_file(), &provisioners, &bundles, |file| {
                        crate::fleet_edit::add_machine(file, m)
                    })
                    .await
            }
            Err(err) => Err(err),
        };
        if let Err(err) = added {
            // Put the record back as it was: no entry, no pending row.
            let undo = match &before {
                Some(s) => store.put_machine_state(s),
                None => store.drop_machine_state(name),
            };
            if let Err(undo) = undo {
                tracing::warn!(machine = %name, %undo, "could not undo the pending bootstrap row");
            }
            return Err(
                match err.downcast_ref::<crate::config::flock::EditError>() {
                    Some(e) => CliError::err(e.code(), e.to_string()),
                    None => err,
                },
            );
        }
    }
    Ok(before)
}

/// Steps 3 and 4: read the platform, then copy pastor and herdr from the
/// head's cache. Nothing on the machine changes but files under its cache
/// and `~/.local/bin/herdr`.
async fn copy_binaries(
    paths: &Paths,
    name: &str,
    login: &str,
    opts: &PushOpts,
) -> Result<(), Stop> {
    let ssh = |remote: &str| ssh_args(paths, login, remote);
    let (pastor_target, herdr_target) =
        platform(opts, name, ssh("uname -sm; getconf LONG_BIT")).await?;
    let herdr_version = crate::artifacts::herdr_version(&opts.artifacts, &opts.fleet_versions);
    for (what, version, target, dir, file_name) in [
        (
            "pastor",
            env!("CARGO_PKG_VERSION").to_string(),
            pastor_target,
            REMOTE_DIR,
            "pastor",
        ),
        ("herdr", herdr_version, herdr_target, REMOTE_BIN, "herdr"),
    ] {
        let (file, sum) = cached(paths, opts, what, &version, &target).await?;
        copy(
            opts,
            &ssh(&copy_command(dir, file_name, &sum)),
            what,
            &file,
            &sum,
        )
        .await?;
    }
    Ok(())
}

/// Step 5: the machine's own steps, `pastor bootstrap` there.
async fn run_bootstrap(
    paths: &Paths,
    name: &str,
    login: &str,
    opts: &PushOpts,
) -> Result<Outcome, Stop> {
    let out = run(
        &opts.runner,
        "ssh",
        ssh_args(paths, login, &bootstrap_command(name, opts)),
        None,
    )
    .await
    .map_err(|e| {
        Stop::new(
            "bootstrap",
            format!("could not run ssh: {e}"),
            "check that ssh is installed on the head",
        )
    })?;
    // `pastor bootstrap` exits 1 on a failed step but still prints its
    // result, so stdout is read whatever the exit status.
    out.stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
        .and_then(|l| serde_json::from_str::<Outcome>(l).ok())
        .ok_or_else(|| {
            Stop::new(
                "bootstrap",
                format!("pastor bootstrap printed no result: {}", tail(&out.stderr)),
                format!(
                    "run it by hand to see why: ssh {login} {REMOTE_DIR}/pastor bootstrap --json --name {name} --head {}",
                    opts.head
                ),
            )
        })
}

/// Step 6, before the state is set: the machine's agent key on its marked
/// line of the head's `authorized_keys`, locked to the agent bridge.
fn register(
    paths: &Paths,
    fleet: &Fleet,
    name: &str,
    outcome: &Outcome,
    opts: &PushOpts,
) -> Result<(), Stop> {
    let stop = |why: String, fix: &str| Stop::new("register", why, fix);
    let Some(agent_key) = &outcome.agent_key else {
        return Err(stop(
            "pastor bootstrap reported no agent key".into(),
            "rerun the bootstrap",
        ));
    };
    let pastor = std::env::current_exe().map_err(|e| {
        stop(
            format!("cannot find the head's pastor: {e}"),
            "rerun the bootstrap",
        )
    })?;
    // The entry must still be there, as the file has it now: a machine
    // removed mid-push gets no line.
    let there = crate::config::flock::Flock::load(&paths.flock_file())
        .ok()
        .and_then(|f| f.get(name).map(|_| ()))
        .or_else(|| fleet.flock().get(name).map(|_| ()));
    if there.is_none() {
        return Err(stop(
            format!("{name} left the flock while it was set up"),
            "add it again",
        ));
    }
    crate::authkeys::upsert(
        &opts.authorized_keys,
        &crate::authkeys::Marker::Agent(name.to_string()),
        &pastor,
        &format!("bridge --agent --machine {name}"),
        agent_key,
    )
    .map(|_| ())
    .map_err(|e| {
        stop(
            format!(
                "could not add the agent line to {}: {e:#}",
                opts.authorized_keys.display()
            ),
            "check that the head's authorized_keys is writable",
        )
    })
}

/// The ssh argv for one remote command as the login user: pastor's own
/// known_hosts, checked strictly against the ed25519 key just pinned (and
/// only that type asked for, so a host offering another key type is not
/// "new"), no shared master, so a socket opened before the pin cannot skip
/// the check, and timeouts, so a machine that stops answering does not hold
/// the push forever.
pub(crate) fn ssh_args(paths: &Paths, login: &str, remote: &str) -> Vec<String> {
    let known_hosts = paths.known_hosts_file();
    let mut args: Vec<String> = [
        "ConnectTimeout=20",
        "ServerAliveInterval=15",
        "ServerAliveCountMax=4",
        "HostKeyAlgorithms=ssh-ed25519",
    ]
    .into_iter()
    .flat_map(|o| ["-o".to_string(), o.to_string()])
    .collect();
    args.extend(
        crate::ssh::Ssh {
            target: login,
            control_path: None,
            control_persist: "",
            keepalive: false,
            no_tty: true,
            identity: None,
            extra_identity: None,
            known_hosts: Some(&known_hosts),
            accept_new: false,
            config_file: None,
        }
        .args(remote),
    );
    args
}

async fn run(
    runner: &Arc<dyn PushRunner>,
    program: &str,
    args: Vec<String>,
    stdin: Option<PathBuf>,
) -> std::io::Result<CmdOutput> {
    let runner = runner.clone();
    let program = program.to_string();
    tokio::task::spawn_blocking(move || runner.run(&program, &args, stdin.as_deref()))
        .await
        .map_err(std::io::Error::other)?
}

/// The last few lines of a command's stderr, for a reason.
pub(crate) fn tail(stderr: &str) -> String {
    let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = lines[lines.len().saturating_sub(3)..].join("; ");
    if tail.is_empty() {
        "no output".into()
    } else {
        tail
    }
}

/// Step 2: pin the machine's ed25519 key in pastor's own known_hosts, under
/// the name ssh itself looks it up by (what `ssh -G` resolves the login to),
/// once. A host already pinned is not scanned again: `ssh-keyscan` neither
/// authenticates its answer nor goes through ssh's own host key check, so
/// the pinned key stands, and every ssh after this checks the machine
/// against it strictly (`ssh_args`): a changed key stops the rerun at its
/// first connection instead of being trusted. When scanning, with
/// `--host-key` the scanned key must be that one, and a key that differs
/// from the one pinned for the host, in known_hosts or on record for the
/// machine (`stored`), stops the push: that is what a man in the middle
/// looks like, as much as a reinstalled machine. Nothing replaces a pinned
/// key; a reinstalled machine is removed and added again.
async fn pin_host_key(
    paths: &Paths,
    machine: &str,
    login: &str,
    stored: Option<&str>,
    opts: &PushOpts,
) -> Result<String, Stop> {
    let stop = |why: String, fix: &str| Stop::new("host_key", why, fix);
    let reinstalled = format!(
        "if the machine was reinstalled, remove it with `pastor machine remove {machine}` and add it again"
    );
    let fallback = || (host_part(login).to_string(), "22".to_string(), None);
    let (host, port, alias) = match run(
        &opts.runner,
        "ssh",
        vec!["-G".into(), "--".into(), login.into()],
        None,
    )
    .await
    {
        Ok(out) if out.success => resolved_host(&out.stdout)
            .map(|(h, p)| (h, p, host_key_alias(&out.stdout)))
            .unwrap_or_else(fallback),
        _ => fallback(),
    };
    let names = pinned_name(&host, &port, alias.as_deref());
    if let Some(pinned) = pinned_key(&paths.known_hosts_file(), &names) {
        if let Some(stored) = stored.filter(|k| !same_key(k, &pinned)) {
            return Err(stop(
                format!(
                    "{names} is pinned with host key {pinned}, not the {stored} on record for {machine}"
                ),
                &reinstalled,
            ));
        }
        if let Some(want) = &opts.host_key {
            let got = fingerprint(paths, &opts.runner, &names, &pinned).await?;
            if !host_key_matches(want, &pinned, &got) {
                return Err(stop(
                    format!(
                        "{names} is pinned with host key {got}, not the --host-key given ({want})"
                    ),
                    &reinstalled,
                ));
            }
        }
        return Ok(pinned);
    }
    let args = vec![
        "-p".into(),
        port.clone(),
        "-t".into(),
        "ed25519".into(),
        "--".into(),
        host.clone(),
    ];
    let out = run(&opts.runner, "ssh-keyscan", args, None)
        .await
        .map_err(|e| {
            stop(
                format!("could not run ssh-keyscan: {e}"),
                "install openssh-client on the head",
            )
        })?;
    let Some((_, key)) = out.stdout.lines().find_map(scanned) else {
        return Err(stop(
            format!(
                "ssh-keyscan got no ed25519 host key from {host} port {port}: {}",
                tail(&out.stderr)
            ),
            "check that sshd runs there and has an ed25519 host key",
        ));
    };
    if let Some(want) = &opts.host_key {
        let got = fingerprint(paths, &opts.runner, &names, &key).await?;
        if !host_key_matches(want, &key, &got) {
            return Err(stop(
                format!("{host} offered host key {got}, not the --host-key given ({want})"),
                "check the machine's key with ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub on its console",
            ));
        }
    }
    if let Some(stored) = stored.filter(|k| !same_key(k, &key)) {
        return Err(stop(
            format!("{names} offered host key {key}, not the {stored} on record for {machine}"),
            &reinstalled,
        ));
    }
    let file = paths.known_hosts_file();
    let write_failed = |e: anyhow::Error| {
        stop(
            format!("could not write {}: {e:#}", file.display()),
            "check the head's state dir",
        )
    };
    if let Some(dir) = file.parent() {
        crate::config::create_private_dir(dir).map_err(write_failed)?;
    }
    // Two pushes, or a push and a `machine remove`, edit the file in turn.
    let _lock = crate::authkeys::lock(&file).map_err(write_failed)?;
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(write_failed(e.into())),
    };
    for line in text.lines() {
        let Some((pinned_names, pinned)) = scanned(line) else {
            continue;
        };
        if !pinned_names.split(',').any(|n| n == names) {
            continue;
        }
        if pinned == key {
            return Ok(key);
        }
        return Err(stop(
            format!(
                "the host key of {names} is not the one pastor pinned in {}",
                file.display()
            ),
            &reinstalled,
        ));
    }
    let mut out = text;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!("{names} {key}\n"));
    replace_file(&file, &out).map_err(write_failed)?;
    Ok(key)
}

/// Write `text` to `file` by rename, so a reader never sees half of it.
fn replace_file(file: &Path, text: &str) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = PathBuf::from(format!("{}.tmp.{}", file.display(), std::process::id()));
    let result = (|| -> anyhow::Result<()> {
        let _ = std::fs::remove_file(&tmp);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, file).with_context(|| format!("rename onto {}", file.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// The name ssh looks a host up by in known_hosts: its `HostKeyAlias` when
/// the config sets one, else the host, as `[host]:port` off port 22.
fn pinned_name(host: &str, port: &str, alias: Option<&str>) -> String {
    match alias {
        Some(alias) => alias.to_string(),
        None if port == "22" => host.to_string(),
        None => format!("[{host}]:{port}"),
    }
}

/// `hostkeyalias` from `ssh -G`, when the config sets one.
fn host_key_alias(out: &str) -> Option<String> {
    out.lines()
        .find_map(|l| l.strip_prefix("hostkeyalias "))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty() && v != "none")
}

/// The ed25519 key pinned in `file` for `name`, as `keytype base64`.
fn pinned_key(file: &Path, name: &str) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    text.lines().find_map(|line| {
        let (names, key) = scanned(line)?;
        (names.split(',').any(|n| n == name) && key.starts_with("ssh-ed25519 ")).then_some(key)
    })
}

/// `names keytype base64` from a known_hosts or keyscan line, as
/// (`names`, `keytype base64`). Comments and blank lines give nothing.
fn scanned(line: &str) -> Option<(String, String)> {
    let mut parts = line.split_whitespace();
    let names = parts.next().filter(|n| !n.starts_with('#'))?;
    let kind = parts.next()?;
    let b64 = parts.next()?;
    Some((names.to_string(), format!("{kind} {b64}")))
}

/// `--host-key` as a whole key, or as the `SHA256:` fingerprint
/// `ssh-keygen -l` prints.
fn host_key_matches(want: &str, key: &str, fingerprint: &str) -> bool {
    let want = want.trim();
    if want.starts_with("SHA256:") {
        return want == fingerprint;
    }
    same_key(want, key)
}

/// The scanned key's `SHA256:` fingerprint, from `ssh-keygen -l`.
async fn fingerprint(
    paths: &Paths,
    runner: &Arc<dyn PushRunner>,
    names: &str,
    key: &str,
) -> Result<String, Stop> {
    let stop = |why: String| Stop::new("host_key", why, "check that ssh-keygen works on the head");
    // One file per call: two pushes at once must not read each other's.
    let file = paths
        .state_dir
        .join(format!(".scanned{}", crate::artifacts::work_dir_name()));
    std::fs::write(&file, format!("{names} {key}\n"))
        .map_err(|e| stop(format!("could not write {}: {e}", file.display())))?;
    let args = vec![
        "-l".into(),
        "-E".into(),
        "sha256".into(),
        "-f".into(),
        file.display().to_string(),
    ];
    let out = run(runner, "ssh-keygen", args, None).await;
    let _ = std::fs::remove_file(&file);
    let out = out.map_err(|e| stop(format!("could not run ssh-keygen: {e}")))?;
    out.stdout
        .split_whitespace()
        .find(|w| w.starts_with("SHA256:"))
        .map(str::to_string)
        .ok_or_else(|| {
            stop(format!(
                "ssh-keygen -l printed no fingerprint: {}",
                tail(&out.stderr)
            ))
        })
}

/// `hostname` and `port` from `ssh -G`.
fn resolved_host(out: &str) -> Option<(String, String)> {
    let field = |key: &str| {
        out.lines()
            .find_map(|l| l.strip_prefix(key))
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    Some((
        field("hostname ")?,
        field("port ").unwrap_or_else(|| "22".into()),
    ))
}

/// Step 3: the machine's OS, arch and userland bits, and the release of
/// each binary that runs there.
async fn platform(
    opts: &PushOpts,
    machine: &str,
    argv: Vec<String>,
) -> Result<(String, String), Stop> {
    let out = run(&opts.runner, "ssh", argv, None).await.map_err(|e| {
        Stop::new(
            "connect",
            format!("could not run ssh: {e}"),
            "check that ssh is installed on the head",
        )
    })?;
    if !out.success {
        // A pinned host is not scanned again (`pin_host_key`), so a key
        // that changed since shows up here, at ssh's own check.
        let fix = if out.stderr.contains("Host key verification failed") {
            format!(
                "the machine's host key is not the one pastor pinned; if it was reinstalled, remove it with `pastor machine remove {machine}` and add it again"
            )
        } else {
            "check that ssh with this login works without a password (BatchMode)".to_string()
        };
        return Err(Stop::new(
            "connect",
            format!("ssh could not reach the machine: {}", tail(&out.stderr)),
            fix,
        ));
    }
    let mut lines = out.stdout.lines().map(str::trim).filter(|l| !l.is_empty());
    let mut uname = lines.next().unwrap_or_default().split_whitespace();
    let os = uname.next().unwrap_or_default().to_lowercase();
    let arch = match uname.next().unwrap_or_default() {
        "arm64" => "aarch64".to_string(),
        other => other.to_string(),
    };
    let bits = if lines.next() == Some("32") { 32 } else { 64 };
    let facts = crate::provision::run::Facts {
        os,
        arch,
        bits,
        distro: None,
        like: Vec::new(),
    };
    let target = |name| {
        crate::artifacts::target_for(&facts, name).map_err(|e| {
            Stop::new(
                "platform",
                e.to_string(),
                "bootstrap supports Linux with systemd and macOS, on x86_64 and aarch64",
            )
        })
    };
    Ok((target("pastor")?, target("herdr")?))
}

/// One binary from the head's checked cache, fetched first when missing.
/// `ensure` downloads with a blocking `Fetch`, so it runs off the workers.
async fn cached(
    paths: &Paths,
    opts: &PushOpts,
    name: &str,
    version: &str,
    target: &str,
) -> Result<(PathBuf, String), Stop> {
    let stop = |why: String| {
        Stop::new(
            "copy",
            why,
            "check the head's network, or [artifacts] in pastor.toml",
        )
    };
    let artifact = Artifact::new(name, version, target).map_err(|e| stop(e.to_string()))?;
    let (paths, cfg, fetch) = (paths.clone(), opts.artifacts.clone(), opts.fetch.clone());
    let rt = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        rt.block_on(crate::artifacts::ensure(
            &paths,
            &cfg,
            &artifact,
            fetch.as_ref(),
        ))
    })
    .await
    .map_err(|e| stop(e.to_string()))?
    .map_err(|e| stop(format!("{name} {version} for {target}: {e:#}")))
}

/// Write stdin to `<dir>/<name>.part`, check its sha256 there (`sha256sum`
/// on Linux, `shasum` on macOS), and only then move it into place. Prints
/// the sum it found either way.
pub(crate) fn copy_command(dir: &str, name: &str, sum: &str) -> String {
    let part = format!("{dir}/{name}.part");
    format!(
        "set -e; umask 077; mkdir -p {dir}; cat > {part}; chmod 700 {part}; \
         if command -v sha256sum >/dev/null 2>&1; then got=$(sha256sum {part}); \
         else got=$(shasum -a 256 {part}); fi; got=${{got%% *}}; echo \"$got\"; \
         [ \"$got\" = {} ] || exit 3; mv -f {part} {dir}/{name}",
        shell_quote(sum)
    )
}

/// Step 4, for one binary.
async fn copy(
    opts: &PushOpts,
    argv: &[String],
    name: &str,
    file: &Path,
    sum: &str,
) -> Result<(), Stop> {
    let out = run(&opts.runner, "ssh", argv.to_vec(), Some(file.to_path_buf()))
        .await
        .map_err(|e| {
            Stop::new(
                "copy",
                format!("could not run ssh: {e}"),
                "check that ssh is installed on the head",
            )
        })?;
    // The remote prints the sum it found before it compares; nothing
    // printed means the copy never got that far.
    let got = out.stdout.split_whitespace().next().unwrap_or_default();
    if !got.is_empty() && got != sum {
        return Err(Stop::new(
            "checksum",
            format!(
                "{name} arrived with sha256 {got}, not {sum}: {}",
                tail(&out.stderr)
            ),
            "check the machine's free space in ~/.cache, then rerun",
        ));
    }
    if !out.success {
        return Err(Stop::new(
            "copy",
            format!("could not put {name} in place: {}", tail(&out.stderr)),
            "check that ~/.cache and ~/.local/bin are writable for the login user",
        ));
    }
    Ok(())
}

/// Step 5's remote command.
fn bootstrap_command(name: &str, opts: &PushOpts) -> String {
    let mut cmd = format!(
        "{REMOTE_DIR}/pastor bootstrap --json --name {} --head {}",
        shell_quote(name),
        shell_quote(&opts.head),
    );
    if let Some(key) = &opts.head_key {
        cmd.push_str(" --head-key ");
        cmd.push_str(&shell_quote(key));
    }
    cmd
}

/// End the ssh master the head may hold for the login, so the next
/// connection checks the pinned key. Nothing to end is the usual case.
async fn close_master(paths: &Paths, name: &str, login: &str, runner: &Arc<dyn PushRunner>) {
    let Some(control_path) = crate::ssh::fitting_control_path(paths, name) else {
        return;
    };
    let control_path = control_path.to_string_lossy().into_owned();
    let args = ["-S", control_path.as_str(), "-O", "exit", "--", login]
        .map(String::from)
        .to_vec();
    match run(runner, "ssh", args, None).await {
        Ok(out) if out.success || out.stderr.contains("No such file or directory") => {}
        Ok(out) => {
            tracing::debug!(machine = %name, stderr = %out.stderr.trim(), "no ssh master to end")
        }
        Err(err) => tracing::debug!(machine = %name, %err, "no ssh master to end"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host is pinned under the name ssh looks it up by: the host, as
    /// `[host]:port` off port 22, or its `HostKeyAlias`.
    #[test]
    fn a_host_is_pinned_under_the_name_ssh_looks_it_up_by() {
        assert_eq!(pinned_name("pi.example", "22", None), "pi.example");
        assert_eq!(pinned_name("pi.example", "2222", None), "[pi.example]:2222");
        assert_eq!(pinned_name("pi.example", "22", Some("alias")), "alias");
        let g = "user me\nhostname 10.0.0.9\nport 2200\nhostkeyalias pi-nine\n";
        assert_eq!(resolved_host(g), Some(("10.0.0.9".into(), "2200".into())));
        assert_eq!(host_key_alias(g).as_deref(), Some("pi-nine"));
        assert_eq!(host_key_alias("hostkeyalias none\n"), None);
        assert_eq!(resolved_host("user me\n"), None);
    }

    /// `--host-key` is a whole key (its comment does not matter) or the
    /// `SHA256:` fingerprint, and nothing looser.
    #[test]
    fn a_host_key_given_matches_as_a_key_or_a_fingerprint() {
        let key = "ssh-ed25519 AAAAone";
        assert!(host_key_matches(
            "ssh-ed25519 AAAAone me@box",
            key,
            "SHA256:x"
        ));
        assert!(!host_key_matches("ssh-ed25519 AAAAtwo", key, "SHA256:x"));
        assert!(!host_key_matches("ssh-rsa AAAAone", key, "SHA256:x"));
        assert!(host_key_matches("SHA256:x", key, "SHA256:x"));
        assert!(!host_key_matches("SHA256:y", key, "SHA256:x"));
        assert!(!host_key_matches("x", key, "SHA256:x"));
        // A known_hosts line gives its names and its key; comments none.
        assert_eq!(
            scanned("a,b ssh-ed25519 AAAAone comment"),
            Some(("a,b".into(), key.into()))
        );
        assert_eq!(scanned("# a ssh-ed25519 AAAAone"), None);
        assert_eq!(scanned(""), None);
    }

    /// What goes to the machine: the copy checks the sum there before it
    /// moves the file into place, a head address or key with shell syntax is
    /// quoted, and nothing asks for sudo.
    #[test]
    fn the_remote_commands_check_the_sum_and_quote_what_is_not_theirs() {
        let c = copy_command(REMOTE_DIR, "pastor", "abc123");
        let sum_check = c.find("abc123 ] || exit 3").expect("a check");
        let moved = c.find("mv -f").expect("a move");
        assert!(sum_check < moved, "{c}");
        assert!(c.contains(".part") && c.contains("umask 077"), "{c}");
        let opts = PushOpts {
            host_key: None,
            machine: None,
            head: "head@h; touch /x".into(),
            head_key: Some("ssh-ed25519 AAAA it's".into()),
            artifacts: Default::default(),
            fleet_versions: vec![],
            authorized_keys: PathBuf::new(),
            runner: Arc::new(SystemPushRunner),
            fetch: Arc::new(crate::artifacts::CurlFetch),
        };
        let cmd = bootstrap_command("pi-9", &opts);
        assert!(cmd.contains("--head 'head@h; touch /x'"), "{cmd}");
        assert!(
            cmd.contains("--head-key 'ssh-ed25519 AAAA it'\\''s'"),
            "{cmd}"
        );
        assert!(!cmd.contains("sudo") && !c.contains("sudo"));
    }
}
