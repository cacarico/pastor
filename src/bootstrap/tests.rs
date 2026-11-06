use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::*;
use crate::harness_install::{Action, Row};
use crate::setup::CmdOutput;

/// A machine in a temp dir: the login user's home, pastor's config and
/// state, and the binary bootstrap copies.
struct Machine {
    _dir: tempfile::TempDir,
    home: PathBuf,
    paths: Paths,
    pastor_from: PathBuf,
}

impl Machine {
    fn new() -> Machine {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let paths = Paths {
            config_dir: home.join(".config/pastor"),
            state_dir: home.join(".local/state/pastor"),
            data_dir: home.join(".local/share/pastor"),
        };
        let pastor_from = dir.path().join("pastor-new");
        std::fs::write(&pastor_from, b"pastor binary").unwrap();
        Machine {
            _dir: dir,
            home,
            paths,
            pastor_from,
        }
    }

    fn options(&self) -> Options {
        Options {
            name: "m1".into(),
            head: Some("u@head".into()),
            head_key: Some("ssh-ed25519 HEADKEY head@head".into()),
            pastor_from: self.pastor_from.clone(),
            paths: self.paths.clone(),
            herdr_pin: None,
        }
    }
}

/// What a scripted command printed; `None` for one that failed.
type Answer = Option<String>;

/// A `Runner` that answers from a script, as a healthy Linux machine
/// unless told otherwise, and records every command it is given. ssh-keygen
/// makes the key files, as the real one would.
struct Fake {
    home: PathBuf,
    state: PathBuf,
    client: PathBuf,
    answers: HashMap<String, Answer>,
    calls: RefCell<Vec<String>>,
    /// At `pastor setup`: whether the machine key and client.toml were there.
    at_services: RefCell<Option<(bool, bool)>>,
    /// The `PATH=` the commands run through `env` were given.
    env_paths: RefCell<Vec<String>>,
}

impl Fake {
    fn linux(m: &Machine) -> Fake {
        let home = m.home.display().to_string();
        let mut answers: HashMap<String, Answer> = [
            ("uname -s", "Linux"),
            ("uname -m", "x86_64"),
            ("getconf LONG_BIT", "64"),
            ("test -d /run/systemd/system", ""),
            ("id -u", "1000"),
            ("id -un", "u"),
            ("test -d /etc/ssl/certs", ""),
            ("loginctl show-user u --property=Linger --value", "yes"),
            ("printenv XDG_RUNTIME_DIR", "/run/user/1000"),
            ("herdr --version", "herdr 0.9.5"),
            (
                "cat /etc/ssh/ssh_host_ed25519_key.pub",
                "ssh-ed25519 HOSTKEY root@m1",
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), Some(v.to_string())))
        .collect();
        answers.insert(
            "getent passwd u".into(),
            Some(format!("u:x:1000:1000::{home}:/bin/bash")),
        );
        for tool in ["git", "ssh-keygen", "curl", "tar"] {
            answers.insert(
                format!("sh -c command -v {tool}"),
                Some(format!("/usr/bin/{tool}")),
            );
        }
        Fake {
            home: m.home.clone(),
            state: m.paths.state_dir.clone(),
            client: crate::head::client_file(&m.paths),
            answers,
            calls: RefCell::new(Vec::new()),
            at_services: RefCell::new(None),
            env_paths: RefCell::new(Vec::new()),
        }
    }

    fn macos(m: &Machine) -> Fake {
        let mut f = Fake::linux(m);
        for k in [
            "getent passwd u",
            "test -d /run/systemd/system",
            "loginctl show-user u --property=Linger --value",
            "printenv XDG_RUNTIME_DIR",
            "test -d /etc/ssl/certs",
        ] {
            f.answers.remove(k);
        }
        f.set("uname -s", Some("Darwin"));
        f.set("uname -m", Some("arm64"));
        let home = m.home.display().to_string();
        f.answers.insert(
            "dscl . -read /Users/u NFSHomeDirectory".into(),
            Some(format!("NFSHomeDirectory: {home}")),
        );
        f
    }

    fn set(&mut self, command: &str, answer: Option<&str>) {
        self.answers
            .insert(command.to_string(), answer.map(str::to_string));
    }

    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }

    fn ran(&self, prefix: &str) -> usize {
        self.calls()
            .iter()
            .filter(|c| c.starts_with(prefix))
            .count()
    }
}

fn ok(stdout: &str) -> std::io::Result<CmdOutput> {
    Ok(CmdOutput {
        success: true,
        stdout: format!("{stdout}\n"),
        stderr: String::new(),
    })
}

fn failed() -> std::io::Result<CmdOutput> {
    Ok(CmdOutput {
        success: false,
        stdout: String::new(),
        stderr: "not there".into(),
    })
}

impl Runner for Fake {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<CmdOutput> {
        // `env PATH=… program args`: the command is the one that runs.
        if program == "env"
            && let Some((path, [inner, rest @ ..])) = args
                .split_first()
                .and_then(|(p, rest)| p.strip_prefix("PATH=").map(|p| (p, rest)))
        {
            self.env_paths.borrow_mut().push(path.to_string());
            return self.run(inner, rest);
        }
        let line = std::iter::once(program)
            .chain(args.iter().copied())
            .collect::<Vec<_>>()
            .join(" ");
        self.calls.borrow_mut().push(line.clone());
        if program == "ssh-keygen" {
            let key = args.last().unwrap();
            let name = Path::new(key).file_name().unwrap().to_string_lossy();
            std::fs::write(key, "private").unwrap();
            std::fs::write(
                format!("{key}.pub"),
                format!("ssh-ed25519 AAAA{name} u@m1\n"),
            )
            .unwrap();
            return ok("");
        }
        let pastor = self.home.join(".local/bin/pastor").display().to_string();
        if program == pastor {
            return match args.first() {
                Some(&"--version") => ok("pastor 0.10.0"),
                Some(&"setup") => {
                    *self.at_services.borrow_mut() = Some((
                        self.state.join("machine_key").exists(),
                        self.client.exists(),
                    ));
                    ok("herdr.service written")
                }
                _ => failed(),
            };
        }
        match self.answers.get(&line) {
            Some(Some(out)) => ok(out),
            _ => failed(),
        }
    }
}

/// A `Local` with set answers.
struct FakeLocal {
    herdr: Action,
    harnesses: fn() -> Harnesses,
    /// The rows `machine check` finds.
    check_rows: fn() -> Vec<crate::provision::run::Row>,
}

impl Default for FakeLocal {
    fn default() -> FakeLocal {
        FakeLocal {
            herdr: Action::Kept,
            harnesses: || Harnesses::NotHere,
            check_rows: Vec::new,
        }
    }
}

impl Local for FakeLocal {
    fn herdr<'a>(&'a self, pin: &'a str) -> LocalFuture<'a, Row> {
        Box::pin(async move {
            Row {
                kind: "herdr".into(),
                pin: pin.to_string(),
                before: Some("0.9.4".into()),
                after: None,
                source: None,
                action: self.herdr,
                steps: Vec::new(),
                detail: Some("2 agents at work".into()),
            }
        })
    }

    fn harnesses(&self) -> LocalFuture<'_, Harnesses> {
        Box::pin(async move { (self.harnesses)() })
    }

    fn check(&self) -> LocalFuture<'_, Result<crate::machine_check::Report, String>> {
        Box::pin(async move {
            Ok(crate::machine_check::Report {
                machine: "m1".into(),
                provisioner: None,
                version: None,
                agents: Vec::new(),
                rows: (self.check_rows)(),
            })
        })
    }
}

fn bootstrap(r: &Fake, o: &Options, local: &FakeLocal) -> Outcome {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(run(r, o, local))
}

fn step<'a>(o: &'a Outcome, name: &str) -> &'a StepResult {
    o.steps
        .iter()
        .find(|s| s.step == name)
        .unwrap_or_else(|| panic!("no step {name} in {:?}", o.steps))
}

fn code(o: &Outcome) -> String {
    let err = error(o).expect("a failed outcome");
    err.downcast_ref::<CliError>().unwrap().code.clone()
}

/// A Raspberry Pi with a 64-bit kernel and a 32-bit Debian userland: the
/// userland decides, and herdr ships nothing for it.
#[test]
fn facts_parse_debian_arm64_32bit_userland() {
    let m = Machine::new();
    let mut r = Fake::linux(&m);
    r.set("uname -m", Some("aarch64"));
    r.set("getconf LONG_BIT", Some("32"));
    let f = facts(&r);
    assert_eq!(
        (f.os.as_str(), f.arch.as_str(), f.bits),
        ("linux", "aarch64", 32)
    );
    assert_eq!(f.init, Init::Systemd);
    assert_eq!((f.uid, f.login.as_str()), (1000, "u"));
    assert_eq!(f.home, m.home);

    let o = bootstrap(&r, &m.options(), &FakeLocal::default());
    assert!(!o.ok);
    let last = o.steps.last().unwrap();
    assert_eq!(
        (last.step.as_str(), last.status),
        ("platform", StepStatus::Failed)
    );
    assert!(
        last.why.as_deref().unwrap().contains("32-bit userland"),
        "{last:?}"
    );
    assert_eq!(code(&o), BOOTSTRAP_UNSUPPORTED_PLATFORM);
}

#[test]
fn prerequisites_missing_listed_and_not_installed() {
    let m = Machine::new();
    let mut r = Fake::linux(&m);
    r.set("sh -c command -v curl", None);
    r.set("sh -c command -v tar", None);
    let o = bootstrap(&r, &m.options(), &FakeLocal::default());
    let s = step(&o, "prerequisites");
    assert_eq!(s.status, StepStatus::Failed);
    assert_eq!(s.missing, ["curl", "tar"]);
    assert!(
        s.fix.as_deref().unwrap().contains("pastor installs none"),
        "{s:?}"
    );
    assert_eq!(code(&o), BOOTSTRAP_MISSING);
    // It stops there, and nothing tried to install them.
    assert_eq!(o.steps.last().unwrap().step, "prerequisites");
    for c in r.calls() {
        let program = c.split_whitespace().next().unwrap();
        assert!(
            ![
                "apt", "apt-get", "dnf", "yum", "apk", "pacman", "zypper", "brew", "sudo"
            ]
            .contains(&program),
            "{c}"
        );
    }
    assert!(!m.home.join(".local/bin/pastor").exists());
}

#[test]
fn linger_off_fails_with_owner_fix() {
    let m = Machine::new();
    let mut r = Fake::linux(&m);
    r.set("loginctl show-user u --property=Linger --value", Some("no"));
    let o = bootstrap(&r, &m.options(), &FakeLocal::default());
    let s = step(&o, "prerequisites");
    assert_eq!(s.status, StepStatus::Failed);
    assert_eq!(s.missing, ["lingering"]);
    let fix = s.fix.as_deref().unwrap();
    assert!(
        fix.contains("machine's owner") && fix.contains("loginctl enable-linger u"),
        "{fix}"
    );
    assert_eq!(r.ran("loginctl enable-linger"), 0);
    assert_eq!(code(&o), BOOTSTRAP_MISSING);
}

#[test]
fn keys_not_regenerated() {
    let m = Machine::new();
    let ak = m.home.join(".ssh/authorized_keys");
    std::fs::create_dir_all(ak.parent().unwrap()).unwrap();
    std::fs::write(&ak, "ssh-ed25519 MINE me@laptop\n").unwrap();
    let r = Fake::linux(&m);
    let first = bootstrap(&r, &m.options(), &FakeLocal::default());
    assert!(first.ok, "{:?}", first.steps);
    assert_eq!(step(&first, "keys").status, StepStatus::Done);
    assert_eq!(r.ran("ssh-keygen"), 2);
    let machine_key = first.machine_key.clone().unwrap();
    assert!(machine_key.contains("machine_key"), "{machine_key}");
    assert!(first.agent_key.as_deref().unwrap().contains("agent_key"));
    assert_eq!(
        first.host_key.as_deref(),
        Some("ssh-ed25519 HOSTKEY root@m1")
    );

    let r = Fake::linux(&m);
    let again = bootstrap(&r, &m.options(), &FakeLocal::default());
    assert_eq!(r.ran("ssh-keygen"), 0, "{:?}", r.calls());
    assert_eq!(step(&again, "keys").status, StepStatus::AlreadyThere);
    assert_eq!(again.machine_key.as_deref(), Some(machine_key.as_str()));
    let text = std::fs::read_to_string(&ak).unwrap();
    assert_eq!(
        text,
        "ssh-ed25519 MINE me@laptop\nssh-ed25519 HEADKEY pastor:head\n"
    );
}

#[test]
fn services_step_runs_after_keys_and_client() {
    let m = Machine::new();
    let r = Fake::linux(&m);
    let o = bootstrap(&r, &m.options(), &FakeLocal::default());
    assert!(o.ok, "{:?}", o.steps);
    let order: Vec<&str> = o.steps.iter().map(|s| s.step.as_str()).collect();
    assert_eq!(order, STEPS);
    assert_eq!(*r.at_services.borrow(), Some((true, true)));
    let setting = crate::head::load(&crate::head::client_file(&m.paths))
        .unwrap()
        .unwrap();
    assert_eq!(setting.ssh, "u@head");
    let pastor = m.home.join(".local/bin/pastor");
    assert_eq!(
        r.ran(&format!("{} setup systemd --herdr --yes", pastor.display())),
        1
    );
}

/// `pastor setup` looks herdr up on its PATH, and an ssh command's PATH
/// lacks `~/.local/bin`, where bootstrap has just put it: the services step
/// adds that directory itself.
#[test]
fn services_run_with_local_bin_on_the_path() {
    let m = Machine::new();
    let r = Fake::linux(&m);
    let o = bootstrap(&r, &m.options(), &FakeLocal::default());
    assert!(o.ok, "{:?}", o.steps);
    let bin = m.home.join(".local/bin").display().to_string();
    let paths = r.env_paths.borrow().clone();
    assert_eq!(paths.len(), 1, "{paths:?}");
    assert!(paths[0].starts_with(&format!("{bin}:")), "{paths:?}");
}

/// What would need root: sudo or its kin, or a package manager. A run
/// through every step, then a second one, calls none.
#[test]
fn no_step_calls_sudo() {
    let m = Machine::new();
    let mut o = m.options();
    o.herdr_pin = Some("0.9.5".into());
    let local = FakeLocal {
        herdr: Action::Installed,
        harnesses: || Harnesses::NonePinned("no agent pins a harness".into()),
        ..FakeLocal::default()
    };
    for _ in 0..2 {
        let r = Fake::linux(&m);
        let out = bootstrap(&r, &o, &local);
        assert!(out.ok, "{:?}", out.steps);
        assert_eq!(out.steps.len(), STEPS.len());
        for c in r.calls() {
            for word in c.split_whitespace() {
                let base = word.rsplit('/').next().unwrap();
                assert!(
                    ![
                        "sudo", "su", "doas", "pkexec", "runuser", "useradd", "chown"
                    ]
                    .contains(&base),
                    "{c}"
                );
            }
        }
    }
}

#[test]
fn macos_user_mode_ok() {
    let m = Machine::new();
    let r = Fake::macos(&m);
    let o = bootstrap(&r, &m.options(), &FakeLocal::default());
    assert!(o.ok, "{:?}", o.steps);
    assert_eq!(o.isolation, Isolation::None);
    assert_eq!(step(&o, "prerequisites").status, StepStatus::AlreadyThere);
    assert_eq!(r.ran("loginctl"), 0);
    assert_eq!(r.ran("getent"), 0);
    let pastor = m.home.join(".local/bin/pastor");
    assert_eq!(
        r.ran(&format!("{} setup launchd --herdr --yes", pastor.display())),
        1,
        "{:?}",
        r.calls()
    );
    assert_eq!(o.pastor_version.as_deref(), Some("0.10.0"));
    assert_eq!(o.herdr_version.as_deref(), Some("0.9.5"));
}

/// macOS has neither hostname file and a non-interactive shell there sets no
/// `HOSTNAME`: the kernel's own answer is the name.
#[test]
fn hostname_falls_back_to_gethostname() {
    let dir = tempfile::tempdir().unwrap();
    let none = dir.path().join("hostname");
    let name = crate::config::hostname_from(&[none.as_path()], None);
    assert_ne!(name, "-");
    assert!(!name.is_empty());
    // The files and `HOSTNAME` still come first.
    std::fs::write(&none, "from-file\n").unwrap();
    assert_eq!(
        crate::config::hostname_from(&[none.as_path()], Some("from-env".into())),
        "from-file"
    );
}

/// A herdr with agents at work is left alone and the run goes on.
#[test]
fn herdr_at_work_is_skipped() {
    let m = Machine::new();
    let mut o = m.options();
    o.herdr_pin = Some("0.9.5".into());
    let local = FakeLocal {
        herdr: Action::Skipped,
        ..FakeLocal::default()
    };
    let r = Fake::linux(&m);
    let out = bootstrap(&r, &o, &local);
    let s = step(&out, "herdr");
    assert_eq!(s.status, StepStatus::Skipped);
    assert_eq!(s.why.as_deref(), Some("2 agents at work"));
    assert!(out.ok, "{:?}", out.steps);
    assert_eq!(out.herdr_version.as_deref(), Some("0.9.4"));
}

#[test]
fn root_is_refused() {
    let m = Machine::new();
    let mut r = Fake::linux(&m);
    r.set("id -u", Some("0"));
    let o = bootstrap(&r, &m.options(), &FakeLocal::default());
    assert_eq!(o.steps.len(), 1);
    assert_eq!(step(&o, "facts").status, StepStatus::Failed);
    assert_eq!(code(&o), BOOTSTRAP_FAILED);
}

mod joining;

/// A harness installed but signed out: the check step's fix names the
/// command that signs it in from the head, for claude and agy alike, and for
/// no kind that has no interactive login. The bootstrap itself stays ok.
#[test]
fn check_names_pastor_login_for_a_signed_out_harness() {
    use crate::provision::run::{Row, Status};
    fn rows() -> Vec<Row> {
        let row = |id: &str, agent: &str, status| Row {
            id: id.into(),
            agent: Some(agent.into()),
            status,
            first_line: "Not logged in".into(),
            reason: None,
        };
        let mut rows = vec![
            row("claude-login", "claude", Status::Fail),
            row("claude-login", "claude-main", Status::Pass),
            row("agy-login", "agy", Status::Fail),
            row("codex-login", "codex", Status::Fail),
        ];
        crate::machine_check::name_login_commands(
            "m1",
            crate::harness_manifest::Harnesses::builtin(),
            &mut rows,
        );
        rows
    }
    let m = Machine::new();
    let local = FakeLocal {
        check_rows: rows,
        ..FakeLocal::default()
    };
    let out = bootstrap(&Fake::linux(&m), &m.options(), &local);
    assert!(out.ok, "{:?}", out.steps);
    let s = step(&out, "check");
    assert!(s.why.as_deref().unwrap().contains("3 of 4"), "{s:?}");
    let fix = s.fix.as_deref().expect("a fix");
    assert!(fix.contains("`pastor login m1 claude`"), "{fix}");
    assert!(fix.contains("`pastor login m1 agy`"), "{fix}");
    assert!(
        !fix.contains("codex") && !fix.contains("claude-main"),
        "{fix}"
    );
    assert!(
        human(&out).contains("pastor login m1 agy"),
        "{}",
        human(&out)
    );

    // Nothing signed out, no fix.
    let out = bootstrap(&Fake::linux(&m), &m.options(), &FakeLocal::default());
    assert_eq!(step(&out, "check").fix, None);
}
