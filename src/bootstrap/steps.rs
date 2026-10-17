//! The steps, in order. Each checks before it acts, so a rerun after a
//! failure carries on where it stopped.

use std::cell::RefCell;
use std::path::PathBuf;

use super::facts::{Facts, Init, Pkg, passwd, stdout, target_for};
use super::outcome::{Mode, StepResult};
use super::{AGENT_USER, Options};
use crate::cli::CliError;
use crate::setup::{CmdOutput, Runner};

/// A step. The run loop fills in its name.
pub(super) type Step = fn(&dyn Runner, &Ctx) -> StepResult;

/// The steps in the order they run. The keys and each user's client.toml
/// come before the services: a unit starts the moment its step runs, and
/// pastor reads at startup whether it is a head's client or a shepherd.
pub(super) const STEPS: [(&str, Step); 9] = [
    ("facts", facts),
    ("platform", platform),
    ("packages", packages),
    ("users", users),
    ("pastor", pastor),
    ("herdr", herdr),
    ("keys", keys),
    ("client", client),
    ("services", services),
];

/// The step of [`STEPS`] called `name`.
pub(super) fn named(name: &str) -> Step {
    STEPS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, step)| *step)
        .unwrap_or_else(|| panic!("no bootstrap step {name}"))
}

/// What the steps share: the options, the facts and the mode, plus what the
/// steps find for the outcome.
pub(super) struct Ctx<'a> {
    pub o: &'a Options,
    /// What `pastor join` adds for its own steps; `None` for `pastor
    /// bootstrap`.
    pub pull: Option<&'a super::pull::Pull>,
    pub facts: Facts,
    pub mode: Mode,
    /// The login user's name (`id -un`).
    pub login: String,
    /// `XDG_RUNTIME_DIR` is set, so `systemctl --user` reaches a user bus.
    /// Root mode makes each user's own, so it is true there.
    pub user_bus: bool,
    pub found: RefCell<Found>,
    /// Users already looked up, with their state dirs.
    pub known: RefCell<Vec<User>>,
    /// Root mode's staging dir: root-owned and 0755, so every user can read
    /// what root copies there, and none can change it. Under /var/tmp, since
    /// /tmp may be noexec and the staged pastor runs from it.
    pub stage: RefCell<Option<String>>,
    /// The files staged there so far.
    pub staged: RefCell<Vec<String>>,
}

/// What the steps read for the outcome.
#[derive(Debug, Default)]
pub(super) struct Found {
    pub machine_key: Option<String>,
    pub agent_key: Option<String>,
    pub host_key: Option<String>,
    pub herdr_version: Option<String>,
    pub pastor_version: Option<String>,
}

/// A user bootstrap acts as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct User {
    pub name: String,
    pub home: PathBuf,
    pub uid: u32,
    /// The state dir this user's pastor uses, as it prints it: its keys
    /// live here.
    pub state: PathBuf,
}

impl User {
    pub(super) fn bin(&self) -> PathBuf {
        self.home.join(".local/bin")
    }
}

/// A step's body: `Err` is the failure that ended it, so `?` stops a step
/// at its first failed command.
pub(super) type StepOutcome = Result<StepResult, StepResult>;

impl Ctx<'_> {
    pub(super) fn service_name(&self) -> String {
        self.o
            .service_user
            .clone()
            .unwrap_or_else(|| self.login.clone())
    }

    fn agent_name(&self) -> String {
        match self.mode {
            Mode::Root => AGENT_USER.to_string(),
            Mode::User => self.login.clone(),
        }
    }

    /// `name` from the user database (never `$HOME`, which `sudo sh`
    /// keeps), with the state dir its own pastor reports.
    fn user(&self, r: &dyn Runner, name: &str) -> Result<User, StepResult> {
        if let Some(u) = self.known.borrow().iter().find(|u| u.name == name) {
            return Ok(u.clone());
        }
        let Some((uid, home)) = passwd(r, &self.facts.os, name) else {
            return Err(StepResult::failed(
                format!("the user database names no user {name} with a home"),
                "rerun bootstrap with --sudo; its users step creates the user",
            ));
        };
        let mut u = User {
            name: name.to_string(),
            home,
            uid,
            state: PathBuf::new(),
        };
        u.state = self.state_dir(r, &u)?;
        self.known.borrow_mut().push(u.clone());
        Ok(u)
    }

    /// `u`'s state dir as its own pastor sees it, so XDG_STATE_HOME and
    /// PASTOR_STATE_DIR count as they will when that user runs pastor.
    fn state_dir(&self, r: &dyn Runner, u: &User) -> Result<PathBuf, StepResult> {
        let pastor = self.readable(r, &path(&self.o.pastor_from), "pastor")?;
        let args = ["bootstrap", "--print-state-dir"];
        let out = must(
            self.as_user(r, u, &pastor, &args),
            &pastor,
            &args,
            "check this pastor runs on this machine, then rerun",
        )?;
        let dir = out.stdout.trim();
        if !dir.starts_with('/') {
            return Err(StepResult::failed(
                format!("{}'s pastor printed no state dir: {dir:?}", u.name),
                "check this pastor runs on this machine, then rerun",
            ));
        }
        Ok(PathBuf::from(dir))
    }

    /// `src` at a path every user can read. In user mode that is `src`
    /// itself. In root mode root copies it, once, into the staging dir, so
    /// the source may sit where only root reads it, and no other user's
    /// path is ever written by root.
    pub(super) fn readable(
        &self,
        r: &dyn Runner,
        src: &str,
        name: &str,
    ) -> Result<String, StepResult> {
        if self.mode == Mode::User {
            return Ok(src.to_string());
        }
        let made = self.stage.borrow().clone();
        let dir = match made {
            Some(dir) => dir,
            None => {
                let args = ["-d", "/var/tmp/pastor-bootstrap.XXXXXX"];
                let dir = must(self.as_root(r, "mktemp", &args), "mktemp", &args, "rerun")?
                    .stdout
                    .trim()
                    .to_string();
                if !dir.starts_with('/') {
                    return Err(StepResult::failed(
                        format!("`mktemp -d` printed no dir: {dir:?}"),
                        "rerun",
                    ));
                }
                *self.stage.borrow_mut() = Some(dir.clone());
                let args = ["755", dir.as_str()];
                must(self.as_root(r, "chmod", &args), "chmod", &args, "rerun")?;
                dir
            }
        };
        let staged = format!("{dir}/{name}");
        if self.staged.borrow().contains(&staged) {
            return Ok(staged);
        }
        let args = ["-m", "0755", src, staged.as_str()];
        must(
            self.as_root(r, "install", &args),
            "install",
            &args,
            "check the source file is readable, then rerun",
        )?;
        self.staged.borrow_mut().push(staged.clone());
        Ok(staged)
    }

    /// Remove root mode's staging dir, if one was made.
    pub(super) fn unstage(&self, r: &dyn Runner) {
        if let Some(dir) = self.stage.borrow_mut().take() {
            let _ = self.as_root(r, "rm", &["-rf", "--", &dir]);
        }
    }

    pub(super) fn service(&self, r: &dyn Runner) -> Result<User, StepResult> {
        self.user(r, &self.service_name())
    }

    pub(super) fn agent(&self, r: &dyn Runner) -> Result<User, StepResult> {
        self.user(r, &self.agent_name())
    }

    /// The service user, then the agent user, once each.
    pub(super) fn users(&self, r: &dyn Runner) -> Result<Vec<User>, StepResult> {
        let service = self.service(r)?;
        let agent = self.agent(r)?;
        Ok(if service == agent {
            vec![service]
        } else {
            vec![service, agent]
        })
    }

    /// Run as `u`. Another user's command runs with that user's home and
    /// runtime dir, so `systemctl --user` reaches its own manager, and
    /// nothing else of the caller's (`other_user_argv`).
    pub(super) fn as_user(
        &self,
        r: &dyn Runner,
        u: &User,
        program: &str,
        args: &[&str],
    ) -> std::io::Result<CmdOutput> {
        if u.name == self.login {
            return r.run(program, args);
        }
        let (wrapper, full) =
            other_user_argv(self.facts.uid == 0, &u.name, &u.home, u.uid, program, args);
        let full: Vec<&str> = full.iter().map(String::as_str).collect();
        r.run(wrapper, &full)
    }

    pub(super) fn as_root(
        &self,
        r: &dyn Runner,
        program: &str,
        args: &[&str],
    ) -> std::io::Result<CmdOutput> {
        if self.facts.uid == 0 {
            return r.run(program, args);
        }
        let mut full = vec!["-n", program];
        full.extend_from_slice(args);
        r.run("sudo", &full)
    }
}

/// The system dirs every command run as another user searches, before that
/// user's own `~/.local/bin`.
const SAFE_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// The program and argv that run `program args` as user `name`: through
/// runuser when bootstrap is root, else `sudo -n`. `env -i` hands it an
/// environment of only PATH, HOME and XDG_RUNTIME_DIR from the user
/// database, and LANG=C, so nothing of the caller's (PASTOR_STATE_DIR,
/// XDG_STATE_HOME, an operator's `sudo sh` env) reaches that user's
/// pastor. `setsid -w` starts it in a session of its own with no
/// controlling tty, so it cannot push input into the caller's terminal
/// (TIOCSTI), and waits for its exit status; stdin is /dev/null
/// (`SystemRunner`).
pub(crate) fn other_user_argv(
    root: bool,
    name: &str,
    home: &std::path::Path,
    uid: u32,
    program: &str,
    args: &[&str],
) -> (&'static str, Vec<String>) {
    let mut full: Vec<String> = vec!["-w".into()];
    if root {
        full.extend(["runuser".into(), "-u".into(), name.into()]);
    } else {
        full.extend(["sudo".into(), "-n".into(), "-u".into(), name.into()]);
    }
    full.extend([
        "--".into(),
        "env".into(),
        "-i".into(),
        format!("PATH={SAFE_PATH}:{}", path(&home.join(".local/bin"))),
        format!("HOME={}", path(home)),
        format!("XDG_RUNTIME_DIR=/run/user/{uid}"),
        "LANG=C".into(),
        program.into(),
    ]);
    full.extend(args.iter().map(|a| a.to_string()));
    ("setsid", full)
}

pub(super) fn succeeded(out: std::io::Result<CmdOutput>) -> bool {
    out.is_ok_and(|o| o.success)
}

/// `out` if the command exited 0, else the step's failure naming it.
pub(super) fn must(
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
            format!("`{shown}` failed: {}", o.stderr.trim()),
            fix,
        )),
        Err(e) => Err(StepResult::failed(
            format!("`{shown}` did not run: {e}"),
            fix,
        )),
    }
}

pub(super) fn path(p: &std::path::Path) -> String {
    p.to_string_lossy().into_owned()
}

/// The last word of a `--version` line (`herdr 0.9.5` reads `0.9.5`).
fn version(out: std::io::Result<CmdOutput>) -> Option<String> {
    out.ok()
        .filter(|o| o.success)
        .and_then(|o| o.stdout.split_whitespace().last().map(str::to_string))
}

fn facts(_r: &dyn Runner, c: &Ctx) -> StepResult {
    if c.o.sudo && c.facts.uid != 0 && !c.facts.sudo {
        return StepResult::failed(
            "--sudo needs passwordless sudo, and `sudo -n true` failed",
            "allow this user sudo without a password, or rerun without --sudo for user mode",
        );
    }
    StepResult::done()
}

fn platform(_r: &dyn Runner, c: &Ctx) -> StepResult {
    let f = &c.facts;
    match f.os.as_str() {
        "linux" if f.init != Init::Systemd => {
            let init = match &f.init {
                Init::Other(name) if !name.is_empty() => name.clone(),
                _ => "an unknown init".to_string(),
            };
            return StepResult::failed(
                format!("this Linux runs {init}, not systemd, which herdr's service needs"),
                "use a machine with systemd",
            )
            .with_missing(vec!["systemd".into()]);
        }
        "linux" => {}
        // brew refuses root, and launchd agents are per user.
        "darwin" if c.mode == Mode::Root => {
            return StepResult::failed(
                "macOS is bootstrapped in user mode only",
                "rerun as a normal user, without --sudo",
            );
        }
        "darwin" => {}
        other => {
            return StepResult::failed(
                format!(
                    "{other:?} is not supported; bootstrap runs on Linux with systemd and on macOS"
                ),
                "use a Linux or macOS machine",
            );
        }
    }
    match target_for(f, "herdr") {
        Ok(_) => StepResult::done(),
        Err(e) => {
            let why = e
                .downcast_ref::<CliError>()
                .map_or_else(|| format!("{e:#}"), |e| e.message.clone());
            StepResult::failed(
                why,
                "use a machine herdr ships for: Linux or macOS, x86_64 or aarch64",
            )
        }
    }
}

/// A command the machine needs, how to see it is there, and its package on
/// apt, dnf and pacman.
struct Need {
    name: &'static str,
    check: &'static [&'static str],
    apt: &'static str,
    dnf: &'static str,
    pacman: &'static str,
}

const NEEDS: [Need; 5] = [
    Need {
        name: "git",
        check: &["sh", "-c", "command -v git"],
        apt: "git",
        dnf: "git",
        pacman: "git",
    },
    Need {
        name: "ssh-keygen",
        check: &["sh", "-c", "command -v ssh-keygen"],
        apt: "openssh-client",
        dnf: "openssh-clients",
        pacman: "openssh",
    },
    Need {
        name: "curl",
        check: &["sh", "-c", "command -v curl"],
        apt: "curl",
        dnf: "curl",
        pacman: "curl",
    },
    Need {
        name: "tar",
        check: &["sh", "-c", "command -v tar"],
        apt: "tar",
        dnf: "tar",
        pacman: "tar",
    },
    Need {
        name: "ca-certificates",
        check: &["test", "-d", "/etc/ssl/certs"],
        apt: "ca-certificates",
        dnf: "ca-certificates",
        pacman: "ca-certificates",
    },
];

fn packages(r: &dyn Runner, c: &Ctx) -> StepResult {
    // macOS keeps its CA roots in the keychain, not /etc/ssl/certs.
    let linux = c.facts.os == "linux";
    let missing = || -> Vec<&Need> {
        NEEDS
            .iter()
            .filter(|n| linux || n.name != "ca-certificates")
            .filter(|n| !succeeded(r.run(n.check[0], &n.check[1..])))
            .collect()
    };
    let names =
        |needs: &[&Need]| -> Vec<String> { needs.iter().map(|n| n.name.to_string()).collect() };
    let first = missing();
    if first.is_empty() {
        return StepResult::already_there();
    }
    let can_install = c.mode == Mode::Root && linux;
    let install: Option<Vec<Vec<String>>> = match c.facts.pkg {
        Some(Pkg::Apt) if can_install => {
            let mut install = vec![
                "env".to_string(),
                "DEBIAN_FRONTEND=noninteractive".into(),
                "apt-get".into(),
                "install".into(),
                "-y".into(),
                "-q".into(),
            ];
            install.extend(first.iter().map(|n| n.apt.to_string()));
            Some(vec![
                vec![
                    "env".into(),
                    "DEBIAN_FRONTEND=noninteractive".into(),
                    "apt-get".into(),
                    "update".into(),
                    "-q".into(),
                ],
                install,
            ])
        }
        Some(Pkg::Dnf) if can_install => {
            let mut install = vec!["dnf".to_string(), "install".into(), "-y".into()];
            install.extend(first.iter().map(|n| n.dnf.to_string()));
            Some(vec![install])
        }
        Some(Pkg::Pacman) if can_install => {
            let mut install = vec![
                "pacman".to_string(),
                "-S".into(),
                "--noconfirm".into(),
                "--needed".into(),
            ];
            install.extend(first.iter().map(|n| n.pacman.to_string()));
            Some(vec![install])
        }
        _ => None,
    };
    let list = names(&first).join(", ");
    let Some(commands) = install else {
        let fix = if c.mode == Mode::User && linux {
            format!("install {list}, or rerun with --sudo so bootstrap installs them")
        } else if linux {
            format!("install {list}; bootstrap knows apt, dnf and pacman only")
        } else {
            format!("install {list} (brew, or the Xcode command line tools)")
        };
        return StepResult::failed(format!("missing: {list}"), fix).with_missing(names(&first));
    };
    for command in &commands {
        let args: Vec<&str> = command[1..].iter().map(String::as_str).collect();
        if let Err(failed) = must(
            c.as_root(r, &command[0], &args),
            &command[0],
            &args,
            &format!("install {list} by hand, then rerun"),
        ) {
            return failed.with_missing(names(&first));
        }
    }
    let after = missing();
    if after.is_empty() {
        StepResult::done()
    } else {
        let list = names(&after).join(", ");
        StepResult::failed(
            format!("still missing after the install: {list}"),
            format!("install {list} by hand, then rerun"),
        )
        .with_missing(names(&after))
    }
}

fn users(r: &dyn Runner, c: &Ctx) -> StepResult {
    users_inner(r, c).unwrap_or_else(|e| e)
}

fn users_inner(r: &dyn Runner, c: &Ctx) -> StepOutcome {
    let service = c.service_name();
    if c.mode == Mode::User {
        if service != c.login {
            return Err(StepResult::failed(
                format!("--service-user {service} needs root to create the user"),
                "rerun with --sudo",
            ));
        }
        return Ok(StepResult::skipped(format!(
            "user mode: everything runs as {}, so agents can read the machine key",
            c.login
        )));
    }
    if service == AGENT_USER {
        return Err(StepResult::failed(
            format!("the service user cannot be {AGENT_USER}, which agents run as"),
            "pick another --service-user, or leave it out to use the login user",
        ));
    }
    let mut changed = false;
    let mut create = vec![AGENT_USER.to_string()];
    if service != c.login {
        create.insert(0, service.clone());
    }
    for name in &create {
        if succeeded(r.run("id", &["-u", name])) {
            continue;
        }
        let args = [
            "--system",
            "--create-home",
            "--shell",
            "/bin/sh",
            name.as_str(),
        ];
        must(
            c.as_root(r, "useradd", &args),
            "useradd",
            &args,
            "create the user by hand, then rerun",
        )?;
        changed = true;
    }
    // Lingering keeps each user's manager, and so its services, up with
    // nobody logged in. enable-linger is a no-op when it is on already.
    for name in [service.as_str(), AGENT_USER] {
        let args = ["enable-linger", name];
        must(
            c.as_root(r, "loginctl", &args),
            "loginctl",
            &args,
            &format!("run `loginctl enable-linger {name}` as root, then rerun"),
        )?;
    }
    // Each user's state dir is its own and 0700: the agent user must never
    // read the machine key in the service user's. Each user makes, checks
    // and closes its own: root never acts on a path another user controls,
    // where a planted symlink would turn a chmod on /etc.
    for u in c.users(r)? {
        let state = path(&u.state);
        let mkdir = ["-p", state.as_str()];
        must(c.as_user(r, &u, "mkdir", &mkdir), "mkdir", &mkdir, "rerun")?;
        let fix = format!(
            "remove {state} as {} and rerun; bootstrap makes it again",
            u.name
        );
        if succeeded(c.as_user(r, &u, "test", &["-L", &state]))
            || !succeeded(c.as_user(r, &u, "test", &["-d", &state]))
        {
            return Err(StepResult::failed(
                format!(
                    "{}'s state dir {state} is a symlink or not a directory",
                    u.name
                ),
                fix,
            ));
        }
        let have = c
            .as_user(r, &u, "stat", &["-c", "%U %a", &state])
            .ok()
            .filter(|o| o.success)
            .map(|o| o.stdout.trim().to_string())
            .unwrap_or_default();
        let (owner, mode) = have.split_once(' ').unwrap_or((have.as_str(), ""));
        if owner != u.name {
            return Err(StepResult::failed(
                format!("{}'s state dir {state} is owned by {owner:?}", u.name),
                fix,
            ));
        }
        if mode != "700" {
            let args = ["700", state.as_str()];
            must(c.as_user(r, &u, "chmod", &args), "chmod", &args, "rerun")?;
            changed = true;
        }
        // A service user bootstrap made has a home only it may enter. The
        // login user's home is left as its owner set it. As that user too.
        if u.name == service && service != c.login {
            let home = path(&u.home);
            let mode = c
                .as_user(r, &u, "stat", &["-c", "%a", &home])
                .ok()
                .filter(|o| o.success)
                .map(|o| o.stdout.trim().to_string());
            if mode.as_deref() != Some("700") {
                let args = ["700", home.as_str()];
                must(c.as_user(r, &u, "chmod", &args), "chmod", &args, "rerun")?;
                changed = true;
            }
        }
    }
    Ok(StepResult::changed(changed))
}

/// Copy `src` (readable by `u`, see [`Ctx::readable`]) to `dest` in `u`'s
/// bin dir unless it is there already. Returns whether it copied.
fn place(r: &dyn Runner, c: &Ctx, u: &User, src: &str, dest: &str) -> Result<bool, StepResult> {
    if succeeded(c.as_user(r, u, "cmp", &["-s", src, dest])) {
        return Ok(false);
    }
    copy(r, c, u, src, dest)?;
    Ok(true)
}

/// `install` unlinks the old file before it writes, so a running binary
/// keeps its own copy and never sees a half-written one. It runs as `u`,
/// whose bin dir it is, never as root.
fn copy(r: &dyn Runner, c: &Ctx, u: &User, src: &str, dest: &str) -> Result<(), StepResult> {
    let bin = path(&u.bin());
    let mkdir = ["-p", bin.as_str()];
    must(c.as_user(r, u, "mkdir", &mkdir), "mkdir", &mkdir, "rerun")?;
    let args = ["-m", "0755", src, dest];
    must(
        c.as_user(r, u, "install", &args),
        "install",
        &args,
        "check the source file is readable, then rerun",
    )?;
    Ok(())
}

fn pastor(r: &dyn Runner, c: &Ctx) -> StepResult {
    pastor_inner(r, c).unwrap_or_else(|e| e)
}

/// pastor for the service user (the shepherd) and the agent user (agents
/// call `pastor task done`).
fn pastor_inner(r: &dyn Runner, c: &Ctx) -> StepOutcome {
    let src = c.readable(r, &path(&c.o.pastor_from), "pastor")?;
    let mut changed = false;
    let users = c.users(r)?;
    for u in &users {
        let dest = path(&u.bin().join("pastor"));
        changed |= place(r, c, u, &src, &dest)?;
    }
    let service = &users[0];
    let dest = path(&service.bin().join("pastor"));
    c.found.borrow_mut().pastor_version = version(c.as_user(r, service, &dest, &["--version"]));
    Ok(StepResult::changed(changed))
}

/// Whether `herdr agent list` names a task's agent (`t-12`).
fn runs_task_agents(list: &str) -> bool {
    list.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-'))
        .filter_map(|word| word.strip_prefix("t-"))
        .any(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn herdr(r: &dyn Runner, c: &Ctx) -> StepResult {
    herdr_inner(r, c).unwrap_or_else(|e| e)
}

fn herdr_inner(r: &dyn Runner, c: &Ctx) -> StepOutcome {
    let u = c.agent(r)?;
    let dest = path(&u.bin().join("herdr"));
    let read_version = || version(c.as_user(r, &u, &dest, &["--version"]));
    let exists = succeeded(c.as_user(r, &u, "test", &["-e", &dest]));
    let Some(src) = &c.o.herdr_from else {
        if exists {
            c.found.borrow_mut().herdr_version = read_version();
            return Ok(StepResult::already_there());
        }
        return Err(StepResult::failed(
            format!("herdr is not at {dest}, and no --herdr-from names one to install"),
            "rerun with --herdr-from <a herdr binary for this machine>",
        )
        .with_missing(vec!["herdr".into()]));
    };
    let src = c.readable(r, &path(src), "herdr")?;
    if exists && succeeded(c.as_user(r, &u, "cmp", &["-s", &src, &dest])) {
        c.found.borrow_mut().herdr_version = read_version();
        return Ok(StepResult::already_there());
    }
    // Another version is there. A herdr that answers is running; restarting
    // it would end every agent pane, so it waits until no task's agent is.
    let running = exists
        && match c.as_user(r, &u, &dest, &["agent", "list"]) {
            Ok(out) if out.success => {
                if runs_task_agents(&out.stdout) {
                    let old = read_version().unwrap_or_else(|| "(version unknown)".into());
                    c.found.borrow_mut().herdr_version = Some(old.clone());
                    return Ok(StepResult::skipped(format!(
                        "herdr {old} runs agents; rerun when idle"
                    )));
                }
                true
            }
            _ => false,
        };
    copy(r, c, &u, &src, &dest)?;
    if running {
        let (program, args): (&str, Vec<String>) = if c.facts.os == "darwin" {
            (
                "launchctl",
                vec![
                    "kickstart".into(),
                    "-k".into(),
                    format!("gui/{}/pastor.herdr", u.uid),
                ],
            )
        } else {
            (
                "systemctl",
                vec!["--user".into(), "restart".into(), "herdr.service".into()],
            )
        };
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        must(
            c.as_user(r, &u, program, &args),
            program,
            &args,
            "restart herdr by hand",
        )?;
    }
    c.found.borrow_mut().herdr_version = read_version();
    Ok(StepResult::done())
}

fn services(r: &dyn Runner, c: &Ctx) -> StepResult {
    services_inner(r, c).unwrap_or_else(|e| e)
}

/// herdr's service as the agent user, through `pastor setup`, which owns
/// the unit file.
fn services_inner(r: &dyn Runner, c: &Ctx) -> StepOutcome {
    let u = c.agent(r)?;
    let pastor = path(&u.bin().join("pastor"));
    let args: &[&str] = if c.facts.os == "darwin" {
        &["setup", "launchd", "--herdr", "--yes"]
    } else {
        if !c.user_bus {
            return Err(StepResult::failed(
                "XDG_RUNTIME_DIR is unset, so `systemctl --user` has no user bus to reach",
                "rerun with --sudo, which gives each user its own bus, or from a login session",
            ));
        }
        let linger = stdout(
            r,
            "loginctl",
            &["show-user", &u.name, "--property=Linger", "--value"],
        );
        if linger.as_deref() != Some("yes") {
            return Err(StepResult::failed(
                format!(
                    "lingering is off for {}, so herdr would stop at logout",
                    u.name
                ),
                format!(
                    "run `sudo loginctl enable-linger {}`, or rerun with --sudo",
                    u.name
                ),
            ));
        }
        wait_runtime_dir(r, c, &u)?;
        &["setup", "systemd", "--herdr", "--yes", "--require-linger"]
    };
    let out = must(
        c.as_user(r, &u, &pastor, args),
        &pastor,
        args,
        "read the error above, fix it, then rerun",
    )?;
    Ok(StepResult::changed(!out.stdout.contains("is up to date")))
}

/// How many times, and how far apart, a user's runtime dir is looked for.
const RUNTIME_DIR_TRIES: u32 = if cfg!(test) { 2 } else { 20 };
const RUNTIME_DIR_PAUSE: std::time::Duration =
    std::time::Duration::from_millis(if cfg!(test) { 1 } else { 250 });

/// Root mode on Linux: `u`'s `/run/user/<uid>`, which systemd makes when it
/// starts the user's manager. Right after `loginctl enable-linger` it may
/// not be there yet, and `systemctl --user` then has no bus. So it is
/// waited for, then the manager (`user@<uid>.service`) is started by hand
/// and it is waited for again.
pub(super) fn wait_runtime_dir(r: &dyn Runner, c: &Ctx, u: &User) -> Result<(), StepResult> {
    if c.mode != Mode::Root || c.facts.os != "linux" {
        return Ok(());
    }
    let dir = format!("/run/user/{}", u.uid);
    let there = || {
        (0..RUNTIME_DIR_TRIES).any(|i| {
            if i > 0 {
                std::thread::sleep(RUNTIME_DIR_PAUSE);
            }
            succeeded(r.run("test", &["-d", &dir]))
        })
    };
    if there() {
        return Ok(());
    }
    let unit = format!("user@{}.service", u.uid);
    let _ = c.as_root(r, "systemctl", &["start", &unit]);
    if there() {
        return Ok(());
    }
    Err(StepResult::failed(
        format!(
            "{dir} is not there with lingering on for {}, so `systemctl --user` has no bus to reach",
            u.name
        ),
        format!("run `systemctl start {unit}` as root, then rerun"),
    ))
}

fn keys(r: &dyn Runner, c: &Ctx) -> StepResult {
    keys_inner(r, c).unwrap_or_else(|e| e)
}

/// The machine key (service user) and the agent key (agent user), made
/// once, and the head's key in the agent user's authorized_keys.
fn keys_inner(r: &dyn Runner, c: &Ctx) -> StepOutcome {
    let service = c.service(r)?;
    let agent = c.agent(r)?;
    let mut changed = false;
    for (u, file) in [(&service, "machine_key"), (&agent, "agent_key")] {
        let state = path(&u.state);
        let key = format!("{state}/{file}");
        if !succeeded(c.as_user(r, u, "test", &["-e", &key])) {
            let mkdir = ["-p", state.as_str()];
            must(c.as_user(r, u, "mkdir", &mkdir), "mkdir", &mkdir, "rerun")?;
            let args = ["-t", "ed25519", "-N", "", "-f", key.as_str()];
            must(
                c.as_user(r, u, "ssh-keygen", &args),
                "ssh-keygen",
                &args,
                "rerun; a key half-made by an earlier run is replaced only by hand",
            )?;
            changed = true;
        }
        let public = format!("{key}.pub");
        let line = must(
            c.as_user(r, u, "cat", &[&public]),
            "cat",
            &[&public],
            "rerun",
        )?
        .stdout
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
        let key = key.trim();
        // One line, or the rest would be a second, unchecked key.
        if key.is_empty() || key.contains(['\n', '\r']) {
            return Err(StepResult::failed(
                "--head-key is not one public key line",
                "pass the head's public key as one line",
            ));
        }
        let ssh = path(&agent.home.join(".ssh"));
        let file = format!("{ssh}/authorized_keys");
        if !succeeded(c.as_user(r, &agent, "grep", &["-qxF", "-e", key, "--", &file])) {
            let mkdir = ["-p", "-m", "700", ssh.as_str()];
            must(
                c.as_user(r, &agent, "mkdir", &mkdir),
                "mkdir",
                &mkdir,
                "rerun",
            )?;
            // `$1` and `$2` keep the key out of the shell's parsing.
            let append = [
                "-c",
                "umask 077 && printf '%s\\n' \"$1\" >> \"$2\"",
                "sh",
                key,
                file.as_str(),
            ];
            must(
                c.as_user(r, &agent, "sh", &append),
                "sh",
                &append[..2],
                "rerun",
            )?;
            changed = true;
        }
    }
    Ok(StepResult::changed(changed))
}

fn client(r: &dyn Runner, c: &Ctx) -> StepResult {
    client_inner(r, c).unwrap_or_else(|e| e)
}

/// client.toml naming the head, for each user: written by that user's own
/// pastor, so the file is theirs and `head::save` keeps the rest of it.
fn client_inner(r: &dyn Runner, c: &Ctx) -> StepOutcome {
    let mut changed = false;
    for u in c.users(r)? {
        let pastor = path(&u.bin().join("pastor"));
        let args = ["bootstrap", "--write-client", c.o.head.as_str()];
        let out = must(c.as_user(r, &u, &pastor, &args), &pastor, &args, "rerun")?;
        changed |= out.stdout.trim() != "unchanged";
    }
    Ok(StepResult::changed(changed))
}
