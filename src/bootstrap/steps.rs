//! The steps, in order. Each checks before it acts, so a rerun after a
//! failure carries on where it stopped. Every one runs as the login user:
//! none calls sudo, installs an OS package or touches another user.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use super::facts::{Facts, Init, stdout};
use super::outcome::{StepResult, StepStatus};
use super::{Harnesses, Local, Options};
use crate::harness_install::Action;
use crate::setup::{CmdOutput, Runner};

/// The steps in the order they run. The keys and client.toml come before
/// the services: a unit starts the moment its step runs, and pastor reads at
/// startup whether it is a head's client. The harnesses and the check come
/// last, once the machine runs what they need.
pub const STEPS: [&str; 10] = [
    "facts",
    "platform",
    "prerequisites",
    "pastor",
    "herdr",
    "keys",
    "client",
    "services",
    "harnesses",
    "check",
];

/// The steps of a pull machine that joined with a token: the same, with its
/// own serve's service after herdr's. The shepherd comes once the keys, the
/// head in client.toml and herdr are there, which it reads as it starts.
pub const PULL_STEPS: [&str; 12] = [
    "facts",
    "platform",
    "prerequisites",
    "pastor",
    "herdr",
    "keys",
    "client",
    "shepherd_name",
    "services",
    "shepherd",
    "harnesses",
    "check",
];

/// What the steps share: the options and the facts, plus what they find
/// for the outcome.
pub(super) struct Ctx<'a> {
    pub o: &'a Options,
    pub facts: Facts,
    pub found: RefCell<Found>,
}

/// What the steps read for the outcome.
#[derive(Debug, Default)]
pub(super) struct Found {
    pub machine_key: Option<String>,
    pub agent_key: Option<String>,
    pub host_key: Option<String>,
    pub herdr_version: Option<String>,
    pub pastor_version: Option<String>,
    pub check: Option<crate::machine_check::Report>,
}

impl Ctx<'_> {
    fn bin(&self) -> PathBuf {
        self.facts.home.join(".local/bin")
    }
}

/// Run the step called `name`.
pub(super) async fn run(name: &str, r: &dyn Runner, c: &Ctx<'_>, local: &dyn Local) -> StepResult {
    let result = match name {
        "facts" => facts(c),
        "platform" => platform(c),
        "prerequisites" => prerequisites(r, c),
        "pastor" => pastor(r, c),
        "herdr" => herdr(r, c, local).await,
        "keys" => keys(r, c),
        "client" => client(c),
        "services" => services(r, c),
        "shepherd_name" => shepherd_name(c),
        "shepherd" => shepherd(r, c),
        "harnesses" => harnesses(c, local).await,
        "check" => check(c, local).await,
        other => unreachable!("no bootstrap step {other}"),
    };
    result.unwrap_or_else(|failed| failed)
}

/// A step's body: `Err` is the failure that ended it, so `?` stops a step
/// at its first failed command.
type StepOutcome = Result<StepResult, StepResult>;

/// Done when the step changed something, else already there.
fn changed(step: &str, changed: bool) -> StepResult {
    if changed {
        StepResult::done(step)
    } else {
        StepResult::already_there(step)
    }
}

/// `out` if the command exited 0, else the step's failure naming it.
fn must(
    step: &str,
    out: std::io::Result<CmdOutput>,
    program: &str,
    args: &[&str],
    fix: &str,
) -> Result<CmdOutput, StepResult> {
    let shown = std::iter::once(program)
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ");
    match out {
        Ok(o) if o.success => Ok(o),
        Ok(o) => Err(StepResult::failed(
            step,
            format!("`{shown}` failed: {}", o.stderr.trim()),
            fix,
        )),
        Err(e) => Err(StepResult::failed(
            step,
            format!("`{shown}` did not run: {e}"),
            fix,
        )),
    }
}

/// A failure of `step` from what went wrong on this machine's own files.
fn io_failed(step: &str, what: &str, e: impl std::fmt::Display) -> StepResult {
    StepResult::failed(
        step,
        format!("{what}: {e}"),
        "fix what the error names, then rerun",
    )
}

/// The last word of a `--version` line (`herdr 0.9.5` reads `0.9.5`).
fn version(out: std::io::Result<CmdOutput>) -> Option<String> {
    out.ok()
        .filter(|o| o.success)
        .and_then(|o| o.stdout.split_whitespace().last().map(str::to_string))
}

fn facts(c: &Ctx) -> StepOutcome {
    let f = &c.facts;
    if f.uid == 0 {
        return Err(StepResult::failed(
            "facts",
            "bootstrap runs as the user pastor logs in as, never as root",
            "run it as that user; pastor needs no root on the machine",
        ));
    }
    if f.login.is_empty() || f.home.as_os_str().is_empty() {
        return Err(StepResult::failed(
            "facts",
            format!(
                "the user database names no home for this user ({:?})",
                f.login
            ),
            "run bootstrap as a user with an entry and a home in the user database",
        ));
    }
    Ok(StepResult::done("facts"))
}

fn platform(c: &Ctx) -> StepOutcome {
    let f = &c.facts;
    match f.os.as_str() {
        "linux" if f.init != Init::Systemd => {
            let init = match &f.init {
                Init::Other(name) if !name.is_empty() => name.clone(),
                _ => "an unknown init".to_string(),
            };
            return Err(StepResult {
                missing: vec!["systemd".into()],
                ..StepResult::failed(
                    "platform",
                    format!("this Linux runs {init}, not systemd, which herdr's service needs"),
                    "use a machine with systemd",
                )
            });
        }
        "linux" | "darwin" => {}
        other => {
            return Err(StepResult::failed(
                "platform",
                format!(
                    "{other:?} is not supported; bootstrap runs on Linux with systemd and on macOS"
                ),
                "use a Linux or macOS machine",
            ));
        }
    }
    for name in ["pastor", "herdr"] {
        if let Err(e) = crate::artifacts::target_for(&f.release(), name) {
            let why = e
                .downcast_ref::<crate::cli::CliError>()
                .map_or_else(|| format!("{e:#}"), |e| e.message.clone());
            return Err(StepResult::failed(
                "platform",
                why,
                "use a machine pastor and herdr ship for: Linux or macOS, x86_64 or aarch64, with a 64-bit userland",
            ));
        }
    }
    Ok(StepResult::done("platform"))
}

/// A command the machine needs, and how to see it is there.
struct Need {
    name: &'static str,
    check: &'static [&'static str],
}

const NEEDS: [Need; 5] = [
    Need {
        name: "git",
        check: &["sh", "-c", "command -v git"],
    },
    Need {
        name: "ssh-keygen",
        check: &["sh", "-c", "command -v ssh-keygen"],
    },
    Need {
        name: "curl",
        check: &["sh", "-c", "command -v curl"],
    },
    Need {
        name: "tar",
        check: &["sh", "-c", "command -v tar"],
    },
    Need {
        name: "ca-certificates",
        check: &["test", "-d", "/etc/ssl/certs"],
    },
];

/// What the machine must already have: the commands pastor and herdr use,
/// and on Linux lingering and a user bus, so herdr's user service runs with
/// nobody logged in and `systemctl --user` can reach it. It installs and
/// enables nothing: the OS is its owner's, so each fix names what that
/// owner does.
fn prerequisites(r: &dyn Runner, c: &Ctx) -> StepOutcome {
    let linux = c.facts.os == "linux";
    let mut missing: Vec<String> = NEEDS
        .iter()
        // macOS keeps its CA roots in the keychain, not /etc/ssl/certs.
        .filter(|n| linux || n.name != "ca-certificates")
        .filter(|n| !r.run(n.check[0], &n.check[1..]).is_ok_and(|o| o.success))
        .map(|n| n.name.to_string())
        .collect();
    let mut fixes = Vec::new();
    if !missing.is_empty() {
        fixes.push(format!(
            "the machine's owner installs {} from the OS's packages; pastor installs none",
            missing.join(", ")
        ));
    }
    if linux {
        let login = &c.facts.login;
        let linger = stdout(
            r,
            "loginctl",
            &["show-user", login, "--property=Linger", "--value"],
        );
        if linger.as_deref() != Some("yes") {
            missing.push("lingering".into());
            fixes.push(format!(
                "the machine's owner turns lingering on for {login} (`loginctl enable-linger {login}`, as root), so herdr keeps running with nobody logged in"
            ));
        }
        if stdout(r, "printenv", &["XDG_RUNTIME_DIR"]).is_none_or(|d| d.is_empty()) {
            missing.push("user bus".into());
            fixes.push(format!(
                "run bootstrap in a session that starts {login}'s systemd (a login, or ssh once lingering is on), so `systemctl --user` has a bus"
            ));
        }
    }
    if missing.is_empty() {
        return Ok(StepResult::already_there("prerequisites"));
    }
    Err(StepResult {
        missing: missing.clone(),
        ..StepResult::failed(
            "prerequisites",
            format!("missing: {}", missing.join(", ")),
            fixes.join("; "),
        )
    })
}

/// Whether `a` and `b` hold the same bytes; a file that cannot be read is
/// not the same.
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::read(a), std::fs::read(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// `src` at `dest`, mode 0755, written beside it and renamed over it, so a
/// running binary keeps its own copy and nothing sees a half-written one.
pub(super) fn install_file(src: &Path, dest: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = dest.parent().expect("a file in a dir");
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        dest.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    std::fs::copy(src, &tmp)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, dest).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// This pastor in `~/.local/bin`, where herdr's unit and the agents find
/// it, unless it is there already.
fn pastor(r: &dyn Runner, c: &Ctx) -> StepOutcome {
    let dest = c.bin().join("pastor");
    let wrote = if same_file(&c.o.pastor_from, &dest) {
        false
    } else {
        install_file(&c.o.pastor_from, &dest)
            .map_err(|e| io_failed("pastor", &format!("copy pastor to {}", dest.display()), e))?;
        true
    };
    c.found.borrow_mut().pastor_version = version(r.run(&dest.to_string_lossy(), &["--version"]));
    Ok(changed("pastor", wrote))
}

/// herdr at its pin when this machine's pastor.toml pins one, through what
/// `pastor machine install` runs: a herdr with task agents at work is left
/// as it is (`Skipped`). With no pin, a herdr already there is enough.
async fn herdr(r: &dyn Runner, c: &Ctx<'_>, local: &dyn Local) -> StepOutcome {
    if let Some(pin) = &c.o.herdr_pin {
        let row = local.herdr(pin).await;
        c.found.borrow_mut().herdr_version = row.after.clone().or(row.before.clone());
        let why = row.detail.clone().unwrap_or_default();
        return match row.action {
            Action::Kept => Ok(StepResult::already_there("herdr")),
            Action::Installed => Ok(StepResult::done("herdr")),
            Action::Skipped => Ok(StepResult::skipped("herdr", why)),
            Action::Failed | Action::Refused => Err(StepResult::failed(
                "herdr",
                format!("herdr {pin} did not install: {why}"),
                "fix what it names, then rerun",
            )),
        };
    }
    let mine = c.bin().join("herdr");
    let program = if mine.exists() {
        mine.to_string_lossy().into_owned()
    } else {
        "herdr".to_string()
    };
    match version(r.run(&program, &["--version"])) {
        Some(v) => {
            c.found.borrow_mut().herdr_version = Some(v);
            Ok(StepResult::already_there("herdr"))
        }
        None => Err(StepResult {
            missing: vec!["herdr".into()],
            ..StepResult::failed(
                "herdr",
                format!("herdr is not on the PATH nor at {}", mine.display()),
                "pin it with `herdr` under [artifacts] in this machine's pastor.toml and rerun, or install herdr into ~/.local/bin",
            )
        }),
    }
}

/// The machine key and the agent key, each made once, and the head's key in
/// this user's `authorized_keys` on its marked line.
fn keys(r: &dyn Runner, c: &Ctx) -> StepOutcome {
    use std::os::unix::fs::DirBuilderExt;
    let state = &c.o.paths.state_dir;
    let mut made = false;
    for file in ["machine_key", "agent_key"] {
        let key = state.join(file);
        if !key.exists() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(state)
                .map_err(|e| io_failed("keys", &format!("make {}", state.display()), e))?;
            let path = key.to_string_lossy().into_owned();
            let args = ["-q", "-t", "ed25519", "-N", "", "-f", path.as_str()];
            must(
                "keys",
                r.run("ssh-keygen", &args),
                "ssh-keygen",
                &args,
                "rerun; a key half-made by an earlier run is replaced only by hand",
            )?;
            made = true;
        }
        let public = state.join(format!("{file}.pub"));
        let line = std::fs::read_to_string(&public)
            .map_err(|e| io_failed("keys", &format!("read {}", public.display()), e))?
            .trim()
            .to_string();
        let mut found = c.found.borrow_mut();
        match file {
            "machine_key" => found.machine_key = Some(line),
            _ => found.agent_key = Some(line),
        }
    }
    c.found.borrow_mut().host_key = stdout(r, "cat", &["/etc/ssh/ssh_host_ed25519_key.pub"]);
    if let Some(key) = &c.o.head_key {
        let file = c.facts.home.join(".ssh/authorized_keys");
        made |= crate::authkeys::upsert_key(&file, &crate::authkeys::Marker::Head, key).map_err(
            |e| {
                StepResult::failed(
                    "keys",
                    format!("the head's key did not go into {}: {e:#}", file.display()),
                    "pass the head's public key as one line, then rerun",
                )
            },
        )?;
    }
    Ok(changed("keys", made))
}

/// client.toml naming the head, through `head::save`, which keeps the rest
/// of the file. No head given, nothing to name.
fn client(c: &Ctx) -> StepOutcome {
    let Some(dest) = &c.o.head else {
        return Ok(StepResult::skipped(
            "client",
            "no --head: this machine is told of no head",
        ));
    };
    let file = crate::head::client_file(&c.o.paths);
    let have = crate::head::load(&file)
        .map_err(|e| io_failed("client", &format!("read {}", file.display()), e))?;
    if have.is_some_and(|h| &h.ssh == dest) {
        return Ok(StepResult::already_there("client"));
    }
    let setting = crate::head::HeadSetting {
        ssh: dest.clone(),
        pastor: None,
    };
    crate::head::save(&file, Some(&setting))
        .map_err(|e| io_failed("client", &format!("write {}", file.display()), e))?;
    Ok(StepResult::done("client"))
}

/// herdr's user service, through `pastor setup`, which owns the unit file.
fn services(r: &dyn Runner, c: &Ctx) -> StepOutcome {
    let pastor = c.bin().join("pastor").to_string_lossy().into_owned();
    let args: &[&str] = if c.facts.os == "darwin" {
        &["setup", "launchd", "--herdr", "--yes"]
    } else {
        &["setup", "systemd", "--herdr", "--yes"]
    };
    // An ssh command's PATH lacks ~/.local/bin, where herdr is by now, and
    // the unit's PATH comes from this one.
    let path = format!(
        "PATH={}:{}",
        c.bin().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut with_path = vec![path.as_str(), pastor.as_str()];
    with_path.extend_from_slice(args);
    let out = must(
        "services",
        r.run("env", &with_path),
        &pastor,
        args,
        "read the error above, fix it, then rerun",
    )?;
    Ok(changed("services", !out.stdout.contains("is up to date")))
}

/// `[shepherd] machine` in pastor.toml, keeping the rest: the name this
/// machine has in the head's flock.toml, which its serve claims and reports
/// under and which survives its restarts.
fn shepherd_name(c: &Ctx) -> StepOutcome {
    let file = c.o.paths.config_file();
    let wrote = super::pull::write_shepherd(&file, &c.o.name)
        .map_err(|e| io_failed("shepherd_name", &format!("write {}", file.display()), e))?;
    Ok(changed("shepherd_name", wrote))
}

/// A pull machine's own serve: its user service, through `pastor setup`,
/// which owns the unit file. The serve reads the head from client.toml and
/// claims tasks.
fn shepherd(r: &dyn Runner, c: &Ctx) -> StepOutcome {
    let pastor = c.bin().join("pastor").to_string_lossy().into_owned();
    let args: &[&str] = if c.facts.os == "darwin" {
        &["setup", "launchd", "--yes"]
    } else {
        &["setup", "systemd", "--yes"]
    };
    let out = must(
        "shepherd",
        r.run(&pastor, args),
        &pastor,
        args,
        "read the error above, fix it, then rerun",
    )?;
    Ok(changed("shepherd", !out.stdout.contains("is up to date")))
}

/// The harnesses of the agents that can land here at their pins, through
/// what `pastor machine install` runs, when this machine's own flock.toml
/// names it; else they are the head's to install.
async fn harnesses(c: &Ctx<'_>, local: &dyn Local) -> StepOutcome {
    let name = &c.o.name;
    let report = match local.harnesses().await {
        Harnesses::NotHere => {
            return Ok(StepResult::skipped(
                "harnesses",
                format!(
                    "this machine's flock.toml does not name {name}, so its pins are the head's; `pastor machine install {name}` there installs them"
                ),
            ));
        }
        Harnesses::NonePinned(why) => return Ok(StepResult::skipped("harnesses", why)),
        Harnesses::Failed(why) => {
            return Err(StepResult::failed(
                "harnesses",
                why,
                "fix what it names, then rerun",
            ));
        }
        Harnesses::Report(report) => report,
    };
    let say = |rows: Vec<&crate::harness_install::Row>| {
        rows.iter()
            .map(|r| match &r.detail {
                Some(d) => format!("{} {}: {d}", r.kind, r.pin),
                None => format!("{} {}", r.kind, r.pin),
            })
            .collect::<Vec<_>>()
            .join("; ")
    };
    let of = |want: &[Action]| -> Vec<&crate::harness_install::Row> {
        report
            .rows
            .iter()
            .filter(|r| want.contains(&r.action))
            .collect()
    };
    let failed = of(&[Action::Failed, Action::Refused]);
    if !failed.is_empty() {
        return Err(StepResult::failed(
            "harnesses",
            format!("did not install: {}", say(failed)),
            "fix what it names, then rerun",
        ));
    }
    let waiting = of(&[Action::Skipped]);
    if !waiting.is_empty() {
        return Ok(StepResult::skipped("harnesses", say(waiting)));
    }
    Ok(changed("harnesses", !of(&[Action::Installed]).is_empty()))
}

/// `pastor machine check` of this machine, once the rest is done: its rows
/// go in the outcome. What it finds missing, a login say, does not fail the
/// bootstrap; a check that could not run at all is skipped.
async fn check(c: &Ctx<'_>, local: &dyn Local) -> StepOutcome {
    match local.check().await {
        Ok(report) => {
            let failed = report.failed().count();
            let logins = crate::machine_check::login_commands(&report);
            let result = if failed == 0 {
                StepResult::done("check")
            } else {
                StepResult {
                    why: Some(format!(
                        "machine check: {failed} of {} rows failed or could not run",
                        report.rows.len()
                    )),
                    // A signed-out harness is the head's to sign in.
                    fix: (!logins.is_empty()).then(|| {
                        format!(
                            "sign the agents in from the head: {}",
                            logins
                                .iter()
                                .map(|c| format!("`{c}`"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    }),
                    ..StepResult::done("check")
                }
            };
            c.found.borrow_mut().check = Some(report);
            Ok(result)
        }
        Err(why) => Ok(StepResult::skipped("check", why)),
    }
}

/// Whether a step's result ends the run.
pub(super) fn ends_run(s: &StepResult) -> bool {
    s.status == StepStatus::Failed
}
