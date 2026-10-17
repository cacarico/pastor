//! Push bootstrap: the head sets a machine up over ssh, as its login user.
//! `machine add --bootstrap` and `machine bootstrap` both end here.
//!
//! The head pins the machine's host key, reads its platform, copies the
//! pastor and herdr binaries from its own checked cache over its own ssh
//! (never through the daemon socket), runs `pastor bootstrap --json` there
//! and records what it printed through the same path a pull machine's
//! `BootstrapReport` takes (`Fleet::report_bootstrap`), so state and events
//! are written once.
//!
//! Every value that goes into a remote command (the head's address, its
//! key, the service user, a checksum) is shell-quoted: the remote login
//! shell parses the line, and none of them is pastor's own text.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;

use super::{AGENT_USER, Isolation, Mode, Outcome, StepResult, StepStatus};
use crate::artifacts::{Artifact, Fetch};
use crate::cli::CliError;
use crate::config::flock::MachineConfig;
use crate::config::{Paths, ProvisionConfig};
use crate::daemon::Fleet;
use crate::herdr::transport::shell_quote;
use crate::setup::CmdOutput;
use crate::store::{BootstrapState, MachineState, Store};

/// Where the binaries land on the machine, under the login user's home.
/// The remote shell expands `$HOME`; nothing else in it varies.
const REMOTE_DIR: &str = "\"$HOME\"/.cache/pastor-bootstrap";

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

/// What a push needs besides the machine: the flags, and what the head
/// read from its own config and fleet.
pub struct PushOpts {
    /// `--sudo`: root mode on a machine whose login user is not root.
    pub sudo: bool,
    /// `--service-user`, checked with bootstrap's own `check_user`.
    pub service_user: Option<String>,
    /// `--host-key`: the machine's ed25519 host key must be this, as a
    /// `SHA256:` fingerprint or a `ssh-ed25519 AAAA…` key. Unset, the key
    /// is trusted the first time it is seen.
    pub host_key: Option<String>,
    /// `machine bootstrap --repin`: the host key pinned for the machine is
    /// dropped and the one it offers now is pinned instead. The only way a
    /// changed host key is trusted again; without it a changed key stops
    /// the push.
    pub repin: bool,
    /// `machine add --bootstrap`: the entry to write when the flock has no
    /// machine of that name. `None` for `machine bootstrap`, whose machine
    /// must exist.
    pub machine: Option<MachineConfig>,
    /// The head's ssh destination as the machine reaches it
    /// (`head_address` in pastor.toml).
    pub head: String,
    pub provision: ProvisionConfig,
    /// The herdr versions the fleet's machines report, for
    /// `artifacts::herdr_version`.
    pub fleet_versions: Vec<String>,
    /// The head's `authorized_keys`, for the machine's agent line.
    pub authorized_keys: PathBuf,
    pub runner: Arc<dyn PushRunner>,
    pub fetch: Arc<dyn Fetch + Send>,
}

/// The ssh login `machine bootstrap NAME` uses: `--ssh` when given, else
/// `stored`, the login its last good bootstrap went through as
/// (`machine_state.bootstrap_login`), else the entry's own `ssh`. A
/// root-mode machine's entry names the agent user, who has no sudo and was
/// never the login, so with nothing stored it needs `--ssh`.
pub fn login_for(
    entry: &MachineConfig,
    given: Option<&str>,
    stored: Option<&str>,
) -> anyhow::Result<String> {
    if let Some(login) = given.or(stored) {
        return Ok(login.to_string());
    }
    match entry.ssh.as_deref() {
        Some(ssh) if ssh.starts_with(&format!("{AGENT_USER}@")) => Err(CliError::err(
            "usage_error",
            format!(
                "{} is reached as {ssh}, which cannot rerun bootstrap; pass --ssh with the login user, like --ssh me@{}",
                entry.name,
                host_part(ssh)
            ),
        )),
        Some(ssh) => Ok(ssh.to_string()),
        None => Err(CliError::err(
            "usage_error",
            format!(
                "{} is not reached over ssh, so it cannot be bootstrapped from the head",
                entry.name
            ),
        )),
    }
}

/// The destination without its user: what `pastor-agent@` goes in front
/// of. An ssh config alias stays an alias, so its `Port` and `HostName`
/// still apply.
fn host_part(login: &str) -> &str {
    login.rsplit_once('@').map_or(login, |(_, host)| host)
}

/// The step that stopped a push on the head's side, before or after the
/// machine's own steps.
struct Stop(StepResult);

impl Stop {
    fn new(step: &str, why: impl Into<String>, fix: impl Into<String>) -> Stop {
        let mut r = StepResult::failed(why, fix);
        r.step = step.to_string();
        Stop(r)
    }
}

/// Set `name` up over ssh as `login`, and record how it ended. A failed
/// step comes back as an `Outcome` with `ok: false`, already recorded as
/// `Failed`; an `Err` is a push that never started (`machine_exists`, an
/// unknown machine, a bad flag) or a store that would not write.
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
    if let Some(user) = &opts.service_user {
        super::check_user(user)?;
    }
    let head_key = head_key(paths, &opts.runner).await?;
    let before = record(paths, store, fleet, name, login, &opts).await?;
    let stored = before.as_ref().and_then(|s| s.host_key.clone());
    let pinned = match prepare(paths, name, login, stored.as_deref(), &opts).await {
        Ok(pinned) => pinned,
        Err(stop) => {
            let outcome = stopped(stop, &opts, None);
            if before
                .as_ref()
                .is_some_and(|s| s.bootstrap == BootstrapState::Ok)
            {
                fleet.report_bootstrap_kept(name, &outcome)?;
            } else {
                fleet.report_bootstrap(name, outcome.clone())?;
            }
            close_master(paths, name, login, &opts.runner).await;
            return Ok(outcome);
        }
    };
    // `--repin` pinned a new key: it is the one on record from now on, so
    // the report below, and every ssh after it, checks against it.
    let before = before.map(|mut s| {
        s.host_key = Some(pinned.clone());
        s
    });
    if let Some(s) = before
        .as_ref()
        .filter(|s| opts.repin && s.bootstrap != BootstrapState::Ok)
    {
        store.put_machine_state(s)?;
    }
    if before
        .as_ref()
        .is_some_and(|s| s.bootstrap == BootstrapState::Ok)
    {
        store.put_machine_state(&pending(name, before.as_ref()))?;
    }
    let mut finished: Option<Finished> = None;
    let outcome = match run_bootstrap(paths, login, &head_key, &opts).await {
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
                    &opts,
                    Some(outcome),
                ),
                _ if outcome.ok => match finish(paths, fleet, name, login, &outcome, &opts).await {
                    Ok(done) => {
                        finished = Some(done);
                        outcome
                    }
                    Err(stop) => stopped(stop, &opts, Some(outcome)),
                },
                _ => outcome,
            }
        }
        Err(stop) => stopped(stop, &opts, None),
    };
    if let Err(err) = fleet.report_bootstrap(name, outcome.clone()) {
        // Not on record as ok, so its agent line and its new ssh do not
        // stay either.
        if let Some(done) = finished {
            undo_finish(paths, fleet, name, &opts, done).await;
        }
        return Err(err);
    }
    // The login that got through, kept apart from the entry's `ssh`, which
    // root mode just moved to the agent user: a later `machine bootstrap`
    // (an upgrade, a `--sudo` rerun) connects as it again, not as a user
    // with no sudo.
    if outcome.ok
        && !login.starts_with(&format!("{AGENT_USER}@"))
        && let Err(err) = store.set_bootstrap_login(name, login)
    {
        tracing::warn!(machine = %name, %err, "could not keep the bootstrap login; the next `machine bootstrap` needs --ssh");
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
        updated_at: chrono::Utc::now().to_rfc3339(),
        ..before.cloned().unwrap_or(MachineState {
            machine: name.to_string(),
            bootstrap: BootstrapState::Pending,
            mode: None,
            isolation: None,
            failed_step: None,
            why: None,
            host_key: None,
            herdr_version: None,
            pastor_version: None,
            service_user: None,
            updated_at: String::new(),
        })
    }
}

/// A failed outcome for a step the head ran, keeping what the machine
/// reported when it got that far.
fn stopped(Stop(step): Stop, opts: &PushOpts, from: Option<Outcome>) -> Outcome {
    let mode = if opts.sudo { Mode::Root } else { Mode::User };
    let mut o = from.unwrap_or(Outcome {
        ok: false,
        mode,
        isolation: match mode {
            Mode::Root => Isolation::Users,
            Mode::User => Isolation::None,
        },
        steps: Vec::new(),
        machine_key: None,
        agent_key: None,
        host_key: None,
        herdr_version: None,
        pastor_version: None,
        service_user: None,
    });
    o.ok = false;
    o.steps.push(step);
    o
}

/// Step 1: the attempt is on record before anything touches the machine,
/// so dispatch skips it from the moment its entry exists. A rerun of a
/// pending or failed `machine add --bootstrap` with the same login goes on;
/// anything else of that name is `machine_exists`. A machine already `Ok`
/// is left `Ok` here (`push` marks it pending at step 5). Returns the row
/// as it was.
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
    // A root-mode machine's entry names the agent user by now, which cannot
    // rerun bootstrap: say which login can.
    let exists = || {
        let rerun = match entry.as_ref().and_then(|e| e.ssh.as_deref()) {
            Some(ssh) if ssh == login => format!("pastor machine bootstrap {name}"),
            _ => format!("pastor machine bootstrap {name} --ssh {login}"),
        };
        CliError::err(
            "machine_exists",
            format!("machine {name} already exists; to set it up again run `{rerun}`"),
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
        let added = fleet
            .edit_flock_file(&paths.flock_file(), |file| {
                crate::fleet_edit::add_machine(file, m)
            })
            .await;
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

/// The head's own key, made with `ssh-keygen` on first use. Its public half
/// goes in the agent user's `authorized_keys` on a root-mode machine.
async fn head_key(paths: &Paths, runner: &Arc<dyn PushRunner>) -> anyhow::Result<String> {
    use std::os::unix::fs::PermissionsExt;
    // Two pushes at once must not both run ssh-keygen on one path.
    static MAKING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _one = MAKING.lock().await;
    let key = paths.head_key();
    let mut public = key.clone().into_os_string();
    public.push(".pub");
    let public = PathBuf::from(public);
    if !key.exists() {
        if let Some(dir) = key.parent() {
            crate::config::create_private_dir(dir)?;
        }
        let args: Vec<String> = ["-q", "-t", "ed25519", "-N", "", "-C", "pastor-head", "-f"]
            .into_iter()
            .map(String::from)
            .chain([key.display().to_string()])
            .collect();
        let out = run(runner, "ssh-keygen", args, None).await?;
        if !out.success {
            anyhow::bail!(
                "ssh-keygen could not make the head key: {}",
                out.stderr.trim()
            );
        }
    }
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("make {} private", key.display()))?;
    let text =
        std::fs::read_to_string(&public).with_context(|| format!("read {}", public.display()))?;
    Ok(text.trim().to_string())
}

/// Steps 2 to 4: pin, read, copy. Nothing on the machine changes before
/// the end of these but a file under its cache. Returns the pinned key.
async fn prepare(
    paths: &Paths,
    name: &str,
    login: &str,
    stored: Option<&str>,
    opts: &PushOpts,
) -> Result<String, Stop> {
    let pinned = pin_host_key(paths, name, login, stored, opts).await?;
    let ssh = |remote: &str| ssh_args(paths, login, remote);
    let (pastor_target, herdr_target) = platform(opts, ssh("uname -sm; getconf LONG_BIT")).await?;
    let herdr_version = crate::artifacts::herdr_version(&opts.provision, &opts.fleet_versions);
    for (name, version, target) in [
        (
            "pastor",
            env!("CARGO_PKG_VERSION").to_string(),
            pastor_target,
        ),
        ("herdr", herdr_version, herdr_target),
    ] {
        let (file, sum) = cached(paths, opts, name, &version, &target).await?;
        copy(opts, &ssh(&copy_command(name, &sum)), name, &file, &sum).await?;
    }
    Ok(pinned)
}

/// Step 5: the machine's own steps, `pastor bootstrap` there.
async fn run_bootstrap(
    paths: &Paths,
    login: &str,
    head_key: &str,
    opts: &PushOpts,
) -> Result<Outcome, Stop> {
    let out = run(
        &opts.runner,
        "ssh",
        ssh_args(paths, login, &bootstrap_command(head_key, opts)),
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
                format!("run it by hand to see why: ssh {login} {REMOTE_DIR}/pastor bootstrap --head {}", opts.head),
            )
        })
}

/// The ssh argv for one remote command as the login user: pastor's own
/// known_hosts, checked strictly against the ed25519 key just pinned (and
/// only that type asked for, so a host offering another key type is not
/// "new"), no shared master, so a socket opened before the pin cannot skip
/// the check, and timeouts, so a machine that stops answering does not
/// hold the push (and its `machine_busy`) forever.
fn ssh_args(paths: &Paths, login: &str, remote: &str) -> Vec<String> {
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
fn tail(stderr: &str) -> String {
    let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
    let tail = lines[lines.len().saturating_sub(3)..].join("; ");
    if tail.is_empty() {
        "no output".into()
    } else {
        tail
    }
}

/// Step 2: pin the machine's ed25519 key in pastor's own known_hosts,
/// under the name ssh itself looks it up by (`pinned_name` of what
/// `ssh -G` resolves the login to), once. A host already pinned is not
/// scanned again: `ssh-keyscan` neither authenticates its answer nor goes
/// through ssh's own host key check, so the pinned key stands, and every
/// ssh after this checks the machine against it strictly (`ssh_args`): a
/// changed key stops the rerun at its first connection instead of being
/// trusted. `--repin` is the one explicit re-scan. When scanning, with
/// `--host-key` the scanned key must be that one, and a key that differs
/// from the one pinned for the host, in known_hosts or on record for the
/// machine (`stored`), stops the push: that is what a man in the middle
/// looks like, as much as a reinstalled machine. Only `--repin` replaces
/// it.
async fn pin_host_key(
    paths: &Paths,
    machine: &str,
    login: &str,
    stored: Option<&str>,
    opts: &PushOpts,
) -> Result<String, Stop> {
    let stop = |why: String, fix: &str| Stop::new("host_key", why, fix);
    let repin =
        format!("if the machine was reinstalled, run `pastor machine bootstrap {machine} --repin`");
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
    if !opts.repin
        && let Some(pinned) = pinned_key(&paths.known_hosts_file(), &names)
    {
        if let Some(stored) = stored.filter(|k| !same_key(k, &pinned)) {
            return Err(stop(
                format!(
                    "{names} is pinned with host key {pinned}, not the {stored} on record for {machine}"
                ),
                &repin,
            ));
        }
        if let Some(want) = &opts.host_key {
            let got = fingerprint(paths, &opts.runner, &names, &pinned).await?;
            if !host_key_matches(want, &pinned, &got) {
                return Err(stop(
                    format!(
                        "{names} is pinned with host key {got}, not the --host-key given ({want})"
                    ),
                    &repin,
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
    if !opts.repin
        && let Some(stored) = stored.filter(|k| !same_key(k, &key))
    {
        return Err(stop(
            format!("{names} offered host key {key}, not the {stored} on record for {machine}"),
            &repin,
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
    let named = |line: &str| {
        scanned(line).filter(|(pinned_names, _)| pinned_names.split(',').any(|n| n == names))
    };
    if !opts.repin {
        for line in text.lines() {
            let Some((_, pinned)) = named(line) else {
                continue;
            };
            if pinned == key {
                return Ok(key);
            }
            return Err(stop(
                format!(
                    "the host key of {names} is not the one pastor pinned in {}",
                    file.display()
                ),
                &repin,
            ));
        }
    }
    // Every other line stays as it was; a repin drops the old one for
    // this name.
    let mut out: String = text
        .lines()
        .filter(|l| named(l).is_none())
        .flat_map(|l| [l, "\n"])
        .collect();
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

/// The name ssh looks a host up by in known_hosts: its `HostKeyAlias`
/// when the config sets one, else the host, as `[host]:port` off port 22.
pub(crate) fn pinned_name(host: &str, port: &str, alias: Option<&str>) -> String {
    match alias {
        Some(alias) => alias.to_string(),
        None => crate::ssh::known_hosts_name(host, Some(port)),
    }
}

/// `hostkeyalias` from `ssh -G`, when the config sets one.
pub(crate) fn host_key_alias(out: &str) -> Option<String> {
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
    let mut w = want.split_whitespace();
    let mut k = key.split_whitespace();
    (w.next(), w.next()) == (k.next(), k.next())
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
pub(crate) fn resolved_host(out: &str) -> Option<(String, String)> {
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
async fn platform(opts: &PushOpts, argv: Vec<String>) -> Result<(String, String), Stop> {
    let out = run(&opts.runner, "ssh", argv, None).await.map_err(|e| {
        Stop::new(
            "connect",
            format!("could not run ssh: {e}"),
            "check that ssh is installed on the head",
        )
    })?;
    if !out.success {
        return Err(Stop::new(
            "connect",
            format!("ssh could not reach the machine: {}", tail(&out.stderr)),
            "check that ssh with this login works without a password (BatchMode)",
        ));
    }
    let mut lines = out.stdout.lines().map(str::trim).filter(|l| !l.is_empty());
    let mut uname = lines.next().unwrap_or_default().split_whitespace();
    let os = uname.next().unwrap_or_default().to_lowercase();
    let arch = match uname.next().unwrap_or_default() {
        "arm64" => "aarch64".to_string(),
        other => other.to_string(),
    };
    let userland_bits = if lines.next() == Some("32") { 32 } else { 64 };
    let facts = super::Facts {
        os,
        arch,
        userland_bits,
        init: super::Init::Other(String::new()),
        pkg: None,
        uid: 0,
        sudo: false,
        home: PathBuf::new(),
        hostname: String::new(),
    };
    let target = |name| {
        super::target_for(&facts, name).map_err(|e| {
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
            "check the head's network, or [provision] in pastor.toml",
        )
    };
    let artifact = Artifact::new(name, version, target).map_err(|e| stop(e.to_string()))?;
    let (paths, cfg, fetch) = (paths.clone(), opts.provision.clone(), opts.fetch.clone());
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
fn copy_command(name: &str, sum: &str) -> String {
    let part = format!("{REMOTE_DIR}/{name}.part");
    format!(
        "set -e; umask 077; mkdir -p {REMOTE_DIR}; cat > {part}; chmod 700 {part}; \
         if command -v sha256sum >/dev/null 2>&1; then got=$(sha256sum {part}); \
         else got=$(shasum -a 256 {part}); fi; got=${{got%% *}}; echo \"$got\"; \
         [ \"$got\" = {} ] || exit 3; mv -f {part} {REMOTE_DIR}/{name}",
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
            "check that ~/.cache is writable for the login user",
        ));
    }
    Ok(())
}

/// Step 5's remote command.
fn bootstrap_command(head_key: &str, opts: &PushOpts) -> String {
    let mut cmd = format!(
        "{REMOTE_DIR}/pastor bootstrap --json --head {} --head-key {} --herdr-from {REMOTE_DIR}/herdr",
        shell_quote(&opts.head),
        shell_quote(head_key)
    );
    if opts.sudo {
        cmd.push_str(" --sudo");
    }
    if let Some(user) = &opts.service_user {
        cmd.push_str(" --service-user ");
        cmd.push_str(&shell_quote(user));
    }
    cmd
}

/// What `finish` changed, for `undo_finish` to put back.
struct Finished {
    /// The entry's `ssh` before a root-mode finish moved it to the agent
    /// user.
    moved_from: Option<String>,
}

/// Step 6, before the state is set: the agent line on the head, then, in
/// root mode, the entry's `ssh` moved to the agent user, last of all, so
/// nothing after it can fail and leave the entry pointing at a machine
/// pastor never finished. A failure takes back what this step did.
async fn finish(
    paths: &Paths,
    fleet: &Fleet,
    name: &str,
    login: &str,
    outcome: &Outcome,
    opts: &PushOpts,
) -> Result<Finished, Stop> {
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
    let marker = crate::authkeys::Marker::Agent(name.to_string());
    // As a join checks: a key that already logs in under another line
    // would keep that line's way in, whatever this one restricts.
    let blob = crate::join::key_blob("the agent key", agent_key)
        .map_err(|e| stop(format!("{e:#}"), "rerun the bootstrap"))?;
    let own = [
        marker.clone(),
        crate::authkeys::Marker::Machine(name.to_string()),
    ];
    match crate::authkeys::holds_key(&opts.authorized_keys, blob, &own) {
        Ok(false) => {}
        Ok(true) => {
            return Err(stop(
                format!(
                    "the agent key of {name} is already in {} on another line",
                    opts.authorized_keys.display()
                ),
                "remove that line, or give the machine's agent user a new key, then rerun",
            ));
        }
        Err(e) => {
            return Err(stop(
                format!("could not read {}: {e:#}", opts.authorized_keys.display()),
                "check that the head's authorized_keys is readable",
            ));
        }
    }
    // The entry must still be there, as the file has it now: a machine
    // removed mid-push gets no line.
    let entry = crate::config::flock::Flock::load(&paths.flock_file())
        .ok()
        .and_then(|f| f.get(name).cloned());
    let Some(entry) = entry else {
        return Err(stop(
            format!("{name} left the flock while it was set up"),
            "add it again",
        ));
    };
    // The marked form of `machine authorized-key`'s line: pastor edits
    // only lines it can find again by their marker.
    crate::authkeys::line(
        &pastor,
        &format!("bridge --agent --machine {name}"),
        agent_key,
        &marker,
    )
    .and_then(|line| crate::authkeys::upsert(&opts.authorized_keys, &marker, &line))
    .map_err(|e| {
        stop(
            format!(
                "could not add the agent line to {}: {e:#}",
                opts.authorized_keys.display()
            ),
            "check that the head's authorized_keys is writable",
        )
    })?;
    let mut done = Finished { moved_from: None };
    if outcome.mode == Mode::Root {
        let agent = format!("{AGENT_USER}@{}", host_part(login));
        let was = entry.ssh.clone();
        let moved = fleet
            .edit_flock_file(&paths.flock_file(), |file| {
                crate::fleet_edit::set_machine_ssh(file, name, &agent)
            })
            .await;
        if let Err(e) = moved {
            undo_finish(paths, fleet, name, opts, done).await;
            return Err(stop(
                format!("could not set its ssh to {agent}: {e:#}"),
                "check flock.toml",
            ));
        }
        done.moved_from = was.filter(|w| *w != agent);
    }
    Ok(done)
}

/// Take back what `finish` did: the agent line, and the entry's `ssh` as
/// it was. Each undo that fails is logged; there is nothing else to do.
async fn undo_finish(paths: &Paths, fleet: &Fleet, name: &str, opts: &PushOpts, done: Finished) {
    let marker = crate::authkeys::Marker::Agent(name.to_string());
    if let Err(undo) = crate::authkeys::remove(&opts.authorized_keys, &marker) {
        tracing::warn!(machine = %name, %undo, "could not take the agent line back");
    }
    if let Some(was) = done.moved_from {
        let back = fleet
            .edit_flock_file(&paths.flock_file(), |file| {
                crate::fleet_edit::set_machine_ssh(file, name, &was)
            })
            .await;
        if let Err(undo) = back {
            tracing::warn!(machine = %name, undo = %format!("{undo:#}"), "could not put its ssh back to {was}");
        }
    }
}

/// End the ssh master the head may hold for the login, so the next
/// connection checks the pinned key and goes as the user the entry now
/// names. Nothing to end is the usual case.
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

#[cfg(all(test, feature = "fake-herdr"))]
mod tests;
