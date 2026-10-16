use std::path::PathBuf;
use std::sync::Mutex;

use super::*;
use crate::cli::CliError;
use crate::setup::CmdOutput;

/// The wrapper another user's command runs under: no controlling tty
/// (`setsid -w`), and an environment of nothing but these (`env -i`).
macro_rules! sudo_pastor {
    () => {
        "setsid -w sudo -n -u pastor -- env -i \
         PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/home/pastor/.local/bin \
         HOME=/home/pastor XDG_RUNTIME_DIR=/run/user/991 LANG=C"
    };
}
macro_rules! sudo_agent {
    () => {
        "setsid -w sudo -n -u pastor-agent -- env -i \
         PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/home/pastor-agent/.local/bin \
         HOME=/home/pastor-agent XDG_RUNTIME_DIR=/run/user/990 LANG=C"
    };
}
macro_rules! runuser_agent {
    () => {
        "setsid -w runuser -u pastor-agent -- env -i \
         PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/home/pastor-agent/.local/bin \
         HOME=/home/pastor-agent XDG_RUNTIME_DIR=/run/user/990 LANG=C"
    };
}

/// Answers each call by the first rule whose prefix it starts with, after
/// the `sudo -n` or `setsid -w sudo -n -u U -- env -i …` or `setsid -w
/// runuser -u U -- env -i …` wrapper is taken off, so a rule reads the same
/// in user and root mode. `bootstrap --print-state-dir` no rule names prints
/// the usual dir in the home of the user it runs as (`me` unwrapped), and a
/// `stat` no rule names finds a dir that user owns at mode 755. Any other
/// call no rule names succeeds with no output.
pub(super) struct FakeRunner {
    rules: Vec<(String, bool, String)>,
    calls: Mutex<Vec<String>>,
}

impl FakeRunner {
    /// A Debian x86_64 box with systemd and apt, logged in as `me` (uid
    /// 1000) with passwordless sudo, with every package there and nothing of
    /// pastor's yet. `over` rules come first, so they win.
    pub(super) fn new(over: &[(&str, bool, &str)]) -> FakeRunner {
        let base: &[(&str, bool, &str)] = &[
            ("uname -s", true, "Linux"),
            ("uname -m", true, "x86_64"),
            ("getconf LONG_BIT", true, "64"),
            ("test -d /run/systemd/system", true, ""),
            ("sh -c for p in", true, "apt-get"),
            ("id -un", true, "me"),
            ("id -u pastor-agent", false, ""),
            ("id -u pastor", false, ""),
            ("id -u", true, "1000"),
            ("sudo -n true", true, ""),
            ("printenv HOME", true, "/root"),
            (
                "getent passwd me",
                true,
                "me:x:1000:1000::/home/me:/bin/bash",
            ),
            ("mktemp -d", true, "/var/tmp/pastor-bootstrap.X"),
            ("test -L", false, ""),
            ("printenv XDG_RUNTIME_DIR", true, "/run/user/1000"),
            (
                "getent passwd pastor-agent",
                true,
                "pastor-agent:x:990:990::/home/pastor-agent:/bin/sh",
            ),
            (
                "getent passwd pastor",
                true,
                "pastor:x:991:991::/home/pastor:/bin/sh",
            ),
            ("loginctl show-user", true, "yes"),
            ("test -e", false, ""),
            ("cmp -s", false, ""),
            ("grep -qxF", false, ""),
            ("/home/pastor-agent/.local/bin/herdr agent list", false, ""),
            ("/home/me/.local/bin/herdr agent list", false, ""),
            (
                "/home/pastor-agent/.local/bin/herdr --version",
                true,
                "herdr 0.9.5",
            ),
            ("/home/me/.local/bin/herdr --version", true, "herdr 0.9.5"),
            (
                "cat /etc/ssh/ssh_host_ed25519_key.pub",
                true,
                "ssh-ed25519 HOST root@box",
            ),
            ("cat", true, "ssh-ed25519 KEY"),
        ];
        let rules = over
            .iter()
            .chain(base)
            .map(|(p, ok, out)| (p.to_string(), *ok, out.to_string()))
            .collect();
        FakeRunner {
            rules,
            calls: Mutex::new(Vec::new()),
        }
    }

    pub(super) fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn called(&self, call: &str) -> bool {
        self.calls().iter().any(|c| c == call)
    }

    fn called_containing(&self, part: &str) -> bool {
        self.calls().iter().any(|c| c.contains(part))
    }
}

/// `call` without its sudo wrapper.
pub(super) fn unwrapped(call: &str) -> &str {
    let other = call
        .strip_prefix("setsid -w runuser -u ")
        .or_else(|| call.strip_prefix("setsid -w sudo -n -u "));
    if let Some(rest) = other {
        // `U -- env -i PATH=P HOME=H XDG_RUNTIME_DIR=D LANG=C <command>`
        return rest.splitn(9, ' ').nth(8).unwrap_or(rest);
    }
    call.strip_prefix("sudo -n ").unwrap_or(call)
}

/// The user a call runs as: `-u U` of its wrapper, else `me`.
pub(super) fn runs_as(call: &str) -> &str {
    call.strip_prefix("setsid -w sudo -n -u ")
        .or_else(|| call.strip_prefix("setsid -w runuser -u "))
        .and_then(|rest| rest.split(' ').next())
        .unwrap_or("me")
}

fn matches(call: &str, prefix: &str) -> bool {
    call == prefix || call.starts_with(&format!("{prefix} "))
}

impl Runner for FakeRunner {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<CmdOutput> {
        let call = std::iter::once(program)
            .chain(args.iter().copied())
            .collect::<Vec<_>>()
            .join(" ");
        self.calls.lock().unwrap().push(call.clone());
        let inner = unwrapped(&call);
        let rule = self
            .rules
            .iter()
            .find(|(p, _, _)| matches(inner, p) || matches(&call, p));
        let (success, stdout) = match rule {
            Some((_, ok, out)) => (*ok, format!("{out}\n")),
            None if inner.ends_with(" bootstrap --print-state-dir") => (
                true,
                format!("/home/{}/.local/state/pastor\n", runs_as(&call)),
            ),
            None if inner.starts_with("stat -c %U %a ") => {
                (true, format!("{} 755\n", runs_as(&call)))
            }
            None if inner.starts_with("stat -c %a ") => (true, "755\n".to_string()),
            None => (true, String::new()),
        };
        Ok(CmdOutput {
            success,
            stdout,
            stderr: if success {
                String::new()
            } else {
                "nope".into()
            },
        })
    }
}

fn options(sudo: bool) -> Options {
    Options {
        sudo,
        service_user: None,
        head: "me@head".into(),
        head_key: Some("ssh-ed25519 HEADKEY me@head".into()),
        herdr_from: Some(PathBuf::from("/tmp/cache/herdr")),
        pastor_from: PathBuf::from("/tmp/cache/pastor"),
    }
}

pub(super) fn step<'a>(o: &'a Outcome, name: &str) -> &'a StepResult {
    o.steps
        .iter()
        .find(|s| s.step == name)
        .unwrap_or_else(|| panic!("no step {name} in {:#?}", o.steps))
}

fn code(err: &anyhow::Error) -> String {
    err.downcast_ref::<CliError>()
        .map(|e| e.code.clone())
        .unwrap_or_else(|| format!("not a CliError: {err:#}"))
}

fn linux(arch: &str, bits: u8) -> Facts {
    Facts {
        os: "linux".into(),
        arch: arch.into(),
        userland_bits: bits,
        init: Init::Systemd,
        pkg: Some(Pkg::Apt),
        uid: 1000,
        sudo: false,
        home: PathBuf::from("/home/me"),
        hostname: "box".into(),
    }
}

#[test]
fn facts_parse_debian_arm64_32bit_userland() {
    let r = FakeRunner::new(&[
        ("uname -m", true, "aarch64"),
        ("getconf LONG_BIT", true, "32"),
    ]);
    let f = facts(&r);
    assert_eq!(f.os, "linux");
    assert_eq!(f.arch, "aarch64");
    assert_eq!(f.userland_bits, 32);
    assert_eq!(f.init, Init::Systemd);
    assert_eq!(f.pkg, Some(Pkg::Apt));
    assert_eq!(f.uid, 1000);
    assert!(f.sudo);
    assert_eq!(f.home, PathBuf::from("/home/me"));
    let err = target_for(&f, "herdr").unwrap_err();
    assert_eq!(code(&err), "bootstrap_unsupported_platform");
    assert_eq!(
        target_for(&f, "pastor").unwrap(),
        "armv7-unknown-linux-musleabihf"
    );
}

#[test]
fn target_mapping() {
    let x86 = linux("x86_64", 64);
    assert_eq!(
        target_for(&x86, "pastor").unwrap(),
        "x86_64-unknown-linux-musl"
    );
    assert_eq!(target_for(&x86, "herdr").unwrap(), "linux-x86_64");
    let arm = linux("aarch64", 64);
    assert_eq!(
        target_for(&arm, "pastor").unwrap(),
        "aarch64-unknown-linux-musl"
    );
    assert_eq!(target_for(&arm, "herdr").unwrap(), "linux-aarch64");
    let mac = Facts {
        os: "darwin".into(),
        init: Init::Launchd,
        pkg: Some(Pkg::Brew),
        ..linux("aarch64", 64)
    };
    assert_eq!(target_for(&mac, "pastor").unwrap(), "aarch64-apple-darwin");
    assert_eq!(target_for(&mac, "herdr").unwrap(), "macos-aarch64");
    let armv7 = linux("armv7l", 32);
    assert_eq!(
        target_for(&armv7, "pastor").unwrap(),
        "armv7-unknown-linux-musleabihf"
    );
    assert_eq!(
        code(&target_for(&armv7, "herdr").unwrap_err()),
        "bootstrap_unsupported_platform"
    );
    let riscv = linux("riscv64", 64);
    assert_eq!(
        target_for(&riscv, "pastor").unwrap(),
        "riscv64gc-unknown-linux-musl"
    );
    assert_eq!(
        code(&target_for(&riscv, "herdr").unwrap_err()),
        "bootstrap_unsupported_platform"
    );
}

#[test]
fn user_mode_without_sudo() {
    let r = FakeRunner::new(&[]);
    let o = run(&r, &options(false));
    assert!(o.ok, "{:#?}", o.steps);
    assert_eq!(o.mode, Mode::User);
    assert_eq!(o.isolation, Isolation::None);
    assert_eq!(o.service_user.as_deref(), Some("me"), "the login user");
    assert!(!r.called_containing("useradd"), "{:#?}", r.calls());
    // Nothing runs through sudo but the probe.
    assert!(
        r.calls()
            .iter()
            .all(|c| !c.starts_with("sudo ") || c == "sudo -n true"),
        "{:#?}",
        r.calls()
    );
    let names: Vec<_> = o.steps.iter().map(|s| s.step.as_str()).collect();
    assert_eq!(
        names,
        [
            "facts", "platform", "packages", "users", "pastor", "herdr", "services", "keys",
            "client"
        ]
    );
    // Everything as the login user: both keys in one state dir.
    assert!(r.called("ssh-keygen -t ed25519 -N  -f /home/me/.local/state/pastor/machine_key"));
    assert!(r.called("ssh-keygen -t ed25519 -N  -f /home/me/.local/state/pastor/agent_key"));
    assert!(r.called("install -m 0755 /tmp/cache/herdr /home/me/.local/bin/herdr"));
    assert!(r.called("/home/me/.local/bin/pastor setup systemd --herdr --yes --require-linger"));
    assert!(r.called("/home/me/.local/bin/pastor bootstrap --write-client me@head"));
    assert_eq!(o.machine_key.as_deref(), Some("ssh-ed25519 KEY"));
    assert_eq!(o.agent_key.as_deref(), Some("ssh-ed25519 KEY"));
    assert_eq!(o.host_key.as_deref(), Some("ssh-ed25519 HOST root@box"));
    assert_eq!(o.herdr_version.as_deref(), Some("0.9.5"));
}

#[test]
fn root_mode_creates_both_users() {
    let r = FakeRunner::new(&[]);
    let o = run(
        &r,
        &Options {
            service_user: Some("pastor".into()),
            ..options(true)
        },
    );
    assert!(o.ok, "{:#?}", o.steps);
    assert_eq!(o.mode, Mode::Root);
    assert_eq!(o.isolation, Isolation::Users);
    assert_eq!(o.service_user.as_deref(), Some("pastor"));
    for user in ["pastor", "pastor-agent"] {
        assert!(
            r.called(&format!(
                "sudo -n useradd --system --create-home --shell /bin/sh {user}"
            )),
            "{:#?}",
            r.calls()
        );
        assert!(r.called(&format!("sudo -n loginctl enable-linger {user}")));
    }
    // Each user's own work runs as that user, on its own bus.
    assert!(r.called(concat!(
        sudo_pastor!(),
        " \
         ssh-keygen -t ed25519 -N  -f /home/pastor/.local/state/pastor/machine_key"
    )));
    assert!(r.called(concat!(
        sudo_agent!(),
        " \
         ssh-keygen -t ed25519 -N  -f /home/pastor-agent/.local/state/pastor/agent_key"
    )));
    assert!(r.called(concat!(
        sudo_agent!(),
        " \
         /home/pastor-agent/.local/bin/pastor setup systemd --herdr --yes --require-linger"
    )));
    // The head's key goes to the agent user, the one the head reaches.
    assert!(r.called_containing("/home/pastor-agent/.ssh/authorized_keys"));
    assert!(!r.called_containing("/home/me/.ssh/authorized_keys"));
    // pastor for both users, herdr for the agent user only: root stages
    // each binary in its own dir, and each user installs its own copy.
    assert!(r.called("sudo -n mktemp -d /var/tmp/pastor-bootstrap.XXXXXX"));
    assert!(r.called("sudo -n chmod 755 /var/tmp/pastor-bootstrap.X"));
    assert!(
        r.called("sudo -n install -m 0755 /tmp/cache/pastor /var/tmp/pastor-bootstrap.X/pastor")
    );
    assert!(r.called("sudo -n install -m 0755 /tmp/cache/herdr /var/tmp/pastor-bootstrap.X/herdr"));
    assert!(r.called(concat!(
        sudo_pastor!(),
        " \
         install -m 0755 /var/tmp/pastor-bootstrap.X/pastor /home/pastor/.local/bin/pastor"
    )));
    assert!(r.called(concat!(
        sudo_agent!(),
        " \
         install -m 0755 /var/tmp/pastor-bootstrap.X/pastor /home/pastor-agent/.local/bin/pastor"
    )));
    assert!(r.called(concat!(
        sudo_agent!(),
        " \
         install -m 0755 /var/tmp/pastor-bootstrap.X/herdr /home/pastor-agent/.local/bin/herdr"
    )));
    assert!(!r.called_containing("/home/pastor/.local/bin/herdr"));
    // Root never writes into, or reads through, another user's paths.
    assert!(
        !r.calls().iter().any(|c| c.starts_with("sudo -n install -o")
            || c.starts_with("sudo -n chown")
            || c.starts_with("sudo -n cmp")
            || c.starts_with("sudo -n test")
            || c.starts_with("sudo -n stat")),
        "{:#?}",
        r.calls()
    );
    assert!(r.called("sudo -n rm -rf -- /var/tmp/pastor-bootstrap.X"));
}

#[test]
fn existing_user_reused() {
    let r = FakeRunner::new(&[("id -u pastor-agent", true, "990")]);
    let o = run(&r, &options(true));
    assert!(o.ok, "{:#?}", o.steps);
    assert!(!r.called_containing("useradd"), "{:#?}", r.calls());
    // The service user is the login user, which needs no creating.
    assert!(!r.called_containing("useradd --system --create-home --shell /bin/sh me"));
}

#[test]
fn needs_sudo_when_asked() {
    let r = FakeRunner::new(&[("sudo -n true", false, "")]);
    let o = run(&r, &options(true));
    assert!(!o.ok);
    let last = o.steps.last().unwrap();
    assert_eq!(last.step, "facts");
    assert_eq!(last.status, StepStatus::Failed);
    assert!(last.why.as_deref().unwrap().contains("sudo -n true"));
    assert_eq!(code(&error(&o).unwrap()), "bootstrap_needs_sudo");
    assert!(!r.called_containing("useradd"));
}

#[test]
fn linger_off_fails_services_step() {
    let r = FakeRunner::new(&[("loginctl show-user", true, "no")]);
    let o = run(&r, &options(false));
    assert!(!o.ok);
    let s = step(&o, "services");
    assert_eq!(s.status, StepStatus::Failed);
    assert!(
        s.fix.as_deref().unwrap().contains("loginctl enable-linger"),
        "{s:#?}"
    );
    assert_eq!(o.steps.last().unwrap().step, "services");
    assert!(!r.called_containing("setup systemd"));
}

#[test]
fn no_user_bus_fails_services_step() {
    let r = FakeRunner::new(&[("printenv XDG_RUNTIME_DIR", false, "")]);
    let o = run(&r, &options(false));
    let s = step(&o, "services");
    assert_eq!(s.status, StepStatus::Failed);
    assert!(s.why.as_deref().unwrap().contains("XDG_RUNTIME_DIR"));
    assert!(s.fix.as_deref().unwrap().contains("--sudo"));
}

#[test]
fn keys_not_regenerated() {
    let r = FakeRunner::new(&[("test -e", true, "")]);
    let o = run(&r, &options(false));
    assert!(o.ok, "{:#?}", o.steps);
    assert!(!r.called_containing("ssh-keygen -t"), "{:#?}", r.calls());
    assert_eq!(o.machine_key.as_deref(), Some("ssh-ed25519 KEY"));
    assert_eq!(o.agent_key.as_deref(), Some("ssh-ed25519 KEY"));
}

#[test]
fn service_state_dir_is_0700_and_owned() {
    let r = FakeRunner::new(&[]);
    let o = run(
        &r,
        &Options {
            service_user: Some("pastor".into()),
            ..options(true)
        },
    );
    assert!(o.ok, "{:#?}", o.steps);
    // Each user closes its own state dir; root touches neither.
    let as_pastor = sudo_pastor!();
    assert!(
        r.called(&format!(
            "{as_pastor} chmod 700 /home/pastor/.local/state/pastor"
        )),
        "{:#?}",
        r.calls()
    );
    // And the home it sits in is closed to the agent user too.
    assert!(r.called(&format!("{as_pastor} chmod 700 /home/pastor")));
    assert!(!r.called_containing("chown"));
}

/// Root mode with the service user `pastor` and both users already there.
fn both_users(over: &[(&str, bool, &str)]) -> FakeRunner {
    let mut rules = over.to_vec();
    rules.extend([
        ("id -u pastor-agent", true, "990"),
        ("id -u pastor", true, "991"),
    ]);
    FakeRunner::new(&rules)
}

fn service_pastor(sudo: bool) -> Options {
    Options {
        service_user: Some("pastor".into()),
        ..options(sudo)
    }
}

#[test]
fn state_dir_already_right_is_already_there() {
    let r = both_users(&[
        (
            "stat -c %U %a /home/pastor/.local/state/pastor",
            true,
            "pastor 700",
        ),
        (
            "stat -c %U %a /home/pastor-agent/.local/state/pastor",
            true,
            "pastor-agent 700",
        ),
        ("stat -c %a /home/pastor", true, "700"),
    ]);
    let o = run(&r, &service_pastor(true));
    assert!(o.ok, "{:#?}", o.steps);
    assert_eq!(step(&o, "users").status, StepStatus::AlreadyThere);
    assert!(!r.called_containing("chmod 700"), "{:#?}", r.calls());
    // The owner looks, since only it can enter its 0700 home.
    assert!(r.called(concat!(
        sudo_pastor!(),
        " \
         stat -c %U %a /home/pastor/.local/state/pastor"
    )));
}

#[test]
fn a_symlinked_state_dir_fails_and_is_not_repaired() {
    let state = "/home/pastor-agent/.local/state/pastor";
    // pastor's own dir passes, so the agent user's is the one refused.
    let r = both_users(&[(&format!("test -L {state}"), true, "")]);
    let o = run(&r, &service_pastor(true));
    let s = step(&o, "users");
    assert_eq!(s.status, StepStatus::Failed);
    assert!(s.why.as_deref().unwrap().contains("symlink"), "{s:#?}");
    assert!(s.fix.as_deref().unwrap().contains(state), "{s:#?}");
    assert!(!r.called_containing(&format!("chmod 700 {state}")));
    assert!(!r.called_containing("chown"));
}

#[test]
fn a_state_dir_owned_by_someone_else_fails() {
    let state = "/home/pastor-agent/.local/state/pastor";
    let r = both_users(&[(&format!("stat -c %U %a {state}"), true, "root 755")]);
    let o = run(&r, &service_pastor(true));
    let s = step(&o, "users");
    assert_eq!(s.status, StepStatus::Failed);
    assert!(s.why.as_deref().unwrap().contains("\"root\""), "{s:#?}");
    assert!(!r.called_containing(&format!("chmod 700 {state}")));
}

#[test]
fn root_without_sudo_uses_runuser() {
    let r = FakeRunner::new(&[
        ("id -un", true, "root"),
        ("id -u pastor-agent", false, ""),
        ("id -u", true, "0"),
        ("sudo -n true", false, ""),
        (
            "getent passwd root",
            true,
            "root:x:0:0:root:/root:/bin/bash",
        ),
        ("stat -c %U %a /root/.local/state/pastor", true, "root 700"),
        (
            concat!(
                runuser_agent!(),
                " \
             /var/tmp/pastor-bootstrap.X/pastor bootstrap --print-state-dir"
            ),
            true,
            "/home/pastor-agent/.local/state/pastor",
        ),
        (
            "/var/tmp/pastor-bootstrap.X/pastor bootstrap --print-state-dir",
            true,
            "/root/.local/state/pastor",
        ),
    ]);
    let o = run(&r, &options(false));
    assert!(o.ok, "{:#?}", o.steps);
    assert_eq!(o.mode, Mode::Root);
    assert!(
        r.calls()
            .iter()
            .all(|c| !c.starts_with("sudo ") || c == "sudo -n true"),
        "{:#?}",
        r.calls()
    );
    let as_agent = runuser_agent!();
    assert!(r.called(&format!(
        "{as_agent} ssh-keygen -t ed25519 -N  -f /home/pastor-agent/.local/state/pastor/agent_key"
    )), "{:#?}", r.calls());
    assert!(r.called(&format!(
        "{as_agent} /home/pastor-agent/.local/bin/pastor setup systemd --herdr --yes --require-linger"
    )));
    // Root's own work runs as itself, with no wrapper.
    assert!(r.called("useradd --system --create-home --shell /bin/sh pastor-agent"));
    assert!(r.called("ssh-keygen -t ed25519 -N  -f /root/.local/state/pastor/machine_key"));
}

#[test]
fn home_from_getent_not_env() {
    // `sudo sh` keeps the caller's HOME; the user database is right.
    let r = FakeRunner::new(&[("printenv HOME", true, "/home/someone-else")]);
    let f = facts(&r);
    assert_eq!(f.home, PathBuf::from("/home/me"));
    let o = run(&r, &options(false));
    assert!(o.ok, "{:#?}", o.steps);
    assert!(r.called("install -m 0755 /tmp/cache/herdr /home/me/.local/bin/herdr"));
    assert!(!r.called_containing("/home/someone-else"));
}

#[test]
fn state_dir_from_users_pastor() {
    let r = both_users(&[(
        concat!(
            sudo_pastor!(),
            " \
         /var/tmp/pastor-bootstrap.X/pastor bootstrap --print-state-dir"
        ),
        true,
        "/srv/pastor-state",
    )]);
    let o = run(&r, &service_pastor(true));
    assert!(o.ok, "{:#?}", o.steps);
    assert!(
        r.called(concat!(
            sudo_pastor!(),
            " \
         ssh-keygen -t ed25519 -N  -f /srv/pastor-state/machine_key"
        )),
        "{:#?}",
        r.calls()
    );
    assert!(r.called(concat!(sudo_pastor!(), " chmod 700 /srv/pastor-state")));
    assert!(r.called(concat!(
        sudo_agent!(),
        " \
         ssh-keygen -t ed25519 -N  -f /home/pastor-agent/.local/state/pastor/agent_key"
    )));
}

#[test]
fn service_user_rejects_option_like_name() {
    let long = "a".repeat(33);
    for name in ["-o", "a b", "Root", "", long.as_str()] {
        let args = Args {
            sudo: true,
            service_user: Some(name.to_string()),
            head_key: None,
            herdr_from: None,
            json: true,
            write_client: None,
            print_state_dir: false,
            write_shepherd: None,
            write_report: None,
        };
        let err = cli(&args, Some("me@head")).unwrap_err();
        assert_eq!(code(&err), "invalid_user", "{name:?}");
    }
    for name in ["pastor", "_svc", "svc-1_a"] {
        assert!(check_user(name).is_ok(), "{name:?}");
    }
}

#[test]
fn macos_root_unsupported() {
    let r = FakeRunner::new(&[
        ("uname -s", true, "Darwin"),
        ("uname -m", true, "arm64"),
        ("id -u", true, "0"),
        ("sh -c for p in", true, "brew"),
    ]);
    let f = facts(&r);
    assert_eq!(f.init, Init::Launchd);
    assert_eq!(f.arch, "aarch64");
    let o = run(&r, &options(false));
    assert!(!o.ok);
    let last = o.steps.last().unwrap();
    assert_eq!(last.step, "platform");
    assert_eq!(last.status, StepStatus::Failed);
    assert_eq!(code(&error(&o).unwrap()), "bootstrap_unsupported_platform");
    assert!(!r.called_containing("useradd"));
}

#[test]
fn macos_user_mode_uses_launchd() {
    let r = FakeRunner::new(&[
        ("uname -s", true, "Darwin"),
        ("uname -m", true, "arm64"),
        ("sh -c for p in", true, "brew"),
        (
            "dscl . -read /Users/me UniqueID NFSHomeDirectory",
            true,
            "UniqueID: 501\nNFSHomeDirectory: /Users/me",
        ),
        (
            "/tmp/cache/pastor bootstrap --print-state-dir",
            true,
            "/Users/me/.local/state/pastor",
        ),
        ("/Users/me/.local/bin/herdr agent list", false, ""),
        ("/Users/me/.local/bin/herdr --version", true, "herdr 0.9.5"),
    ]);
    let o = run(&r, &options(false));
    assert!(o.ok, "{:#?}", o.steps);
    assert!(r.called("/Users/me/.local/bin/pastor setup launchd --herdr --yes"));
    assert!(!r.called_containing("loginctl"));
}

#[test]
fn herdr_swap_waits_while_agents_run() {
    let r = FakeRunner::new(&[
        ("test -e", true, ""),
        ("/home/me/.local/bin/herdr --version", true, "herdr 0.9.1"),
        (
            "/home/me/.local/bin/herdr agent list",
            true,
            "t-3 claude working\n",
        ),
    ]);
    let o = run(&r, &options(false));
    let s = step(&o, "herdr");
    assert_eq!(s.status, StepStatus::Skipped);
    assert_eq!(
        s.why.as_deref(),
        Some("herdr 0.9.1 runs agents; rerun when idle")
    );
    assert!(o.ok, "a skip is not a failure: {:#?}", o.steps);
    assert!(!r.called_containing("/home/me/.local/bin/herdr.tmp"));
    assert!(!r.called_containing("install -m 0755 /tmp/cache/herdr"));
    assert!(!r.called_containing("restart herdr.service"));
}

#[test]
fn herdr_swap_restarts_when_idle() {
    let r = FakeRunner::new(&[
        ("test -e", true, ""),
        ("/home/me/.local/bin/herdr agent list", true, "[]"),
    ]);
    let o = run(&r, &options(false));
    assert_eq!(step(&o, "herdr").status, StepStatus::Done);
    assert!(r.called("install -m 0755 /tmp/cache/herdr /home/me/.local/bin/herdr"));
    assert!(r.called("systemctl --user restart herdr.service"));
}

#[test]
fn herdr_up_to_date_is_left_alone() {
    let r = FakeRunner::new(&[("test -e", true, ""), ("cmp -s", true, "")]);
    let o = run(&r, &options(false));
    assert!(o.ok);
    assert_eq!(step(&o, "herdr").status, StepStatus::AlreadyThere);
    assert_eq!(step(&o, "pastor").status, StepStatus::AlreadyThere);
    assert!(!r.called_containing("install -m"));
}

#[test]
fn herdr_missing_without_a_file_fails() {
    let r = FakeRunner::new(&[]);
    let o = run(
        &r,
        &Options {
            herdr_from: None,
            ..options(false)
        },
    );
    let s = step(&o, "herdr");
    assert_eq!(s.status, StepStatus::Failed);
    assert_eq!(s.missing, ["herdr"]);
    assert_eq!(code(&error(&o).unwrap()), "bootstrap_missing");
}

#[test]
fn user_mode_reports_missing_packages() {
    let r = FakeRunner::new(&[("sh -c command -v git", false, "")]);
    let o = run(&r, &options(false));
    let s = step(&o, "packages");
    assert_eq!(s.status, StepStatus::Failed);
    assert_eq!(s.missing, ["git"]);
    assert_eq!(code(&error(&o).unwrap()), "bootstrap_missing");
    assert!(!r.called_containing("apt-get install"));
}

#[test]
fn head_key_already_authorized_is_not_added_again() {
    let r = FakeRunner::new(&[("grep -qxF", true, "")]);
    let o = run(&r, &options(false));
    assert!(o.ok);
    assert!(!r.called_containing(">>"), "{:#?}", r.calls());
}

#[test]
fn a_head_key_with_a_line_break_is_refused() {
    let r = FakeRunner::new(&[]);
    let o = run(
        &r,
        &Options {
            head_key: Some("ssh-ed25519 A\ncommand=\"x\" ssh-ed25519 B".into()),
            ..options(false)
        },
    );
    assert_eq!(step(&o, "keys").status, StepStatus::Failed);
    assert!(!r.called_containing("authorized_keys"));
}

#[test]
fn outcome_json_is_snake_case() {
    let r = FakeRunner::new(&[]);
    let o = run(&r, &options(false));
    let v = serde_json::to_value(&o).unwrap();
    assert_eq!(v["mode"], "user");
    assert_eq!(v["isolation"], "none");
    assert_eq!(v["steps"][0]["status"], "done");
    let back: Outcome = serde_json::from_value(v).unwrap();
    assert_eq!(back, o);
}

#[test]
fn hostname_falls_back_to_gethostname() {
    let missing = std::path::Path::new("/nonexistent/pastor/hostname");
    let name = crate::config::hostname_from(&[missing], None);
    assert_ne!(name, "-");
    assert!(!name.is_empty());
    // What the kernel says, as `uname -n` prints it.
    let uname = std::process::Command::new("uname")
        .arg("-n")
        .output()
        .unwrap();
    assert_eq!(name, String::from_utf8_lossy(&uname.stdout).trim());
}

#[test]
fn write_client_writes_once_and_keeps_the_rest() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("pastor/client.toml");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "# mine\n").unwrap();
    assert!(write_client(&file, "me@head").unwrap());
    assert!(!write_client(&file, "me@head").unwrap());
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.contains("# mine\n"), "{text}");
    assert_eq!(crate::head::load(&file).unwrap().unwrap().ssh, "me@head");
    assert!(write_client(&file, "-oProxyCommand=x").is_err());
}

#[test]
fn another_users_command_has_a_clean_env_and_no_tty() {
    for (root, runner) in [(true, "runuser"), (false, "sudo")] {
        let (program, argv) = steps::other_user_argv(
            root,
            "pastor-agent",
            std::path::Path::new("/home/pastor-agent"),
            990,
            "/var/tmp/pastor-bootstrap.X/pastor",
            &["bootstrap", "--print-state-dir"],
        );
        assert_eq!(program, "setsid");
        let call = std::iter::once(program)
            .chain(argv.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        let head = if root {
            "setsid -w runuser -u pastor-agent -- env -i"
        } else {
            "setsid -w sudo -n -u pastor-agent -- env -i"
        };
        assert!(call.starts_with(head), "{runner}: {call}");
        // The only variables set are the ones bootstrap names.
        let env = argv.iter().position(|a| a == "-i").unwrap();
        let vars: Vec<&str> = argv[env + 1..]
            .iter()
            .take_while(|a| a.contains('='))
            .map(String::as_str)
            .collect();
        assert_eq!(
            vars,
            [
                "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:\
                 /home/pastor-agent/.local/bin",
                "HOME=/home/pastor-agent",
                "XDG_RUNTIME_DIR=/run/user/990",
                "LANG=C",
            ],
            "{runner}"
        );
        assert_eq!(
            &argv[env + 1 + vars.len()..],
            [
                "/var/tmp/pastor-bootstrap.X/pastor",
                "bootstrap",
                "--print-state-dir"
            ],
        );
    }
}

#[test]
fn callers_pastor_state_dir_never_reaches_another_user() {
    // `env -i` drops whatever the caller has, so only argv can carry a
    // variable to the other user; run the real wrapper through a real
    // `env` to prove it.
    let (_, argv) = steps::other_user_argv(
        true,
        "pastor-agent",
        std::path::Path::new("/home/pastor-agent"),
        990,
        "printenv",
        &[],
    );
    assert!(
        !argv
            .iter()
            .any(|a| a.contains("PASTOR_") || a.contains("XDG_STATE"))
    );
    let env = argv.iter().position(|a| a == "env").unwrap();
    let out = std::process::Command::new("env")
        .args(&argv[env + 1..])
        .env("PASTOR_STATE_DIR", "/tmp/evil")
        .env("PASTOR_CONFIG_DIR", "/tmp/evil")
        .env("XDG_STATE_HOME", "/tmp/evil")
        .output()
        .unwrap();
    let printed = String::from_utf8(out.stdout).unwrap();
    assert!(out.status.success());
    assert!(!printed.contains("/tmp/evil"), "{printed}");
    assert!(printed.contains("HOME=/home/pastor-agent\n"), "{printed}");
}

#[test]
fn root_stages_under_var_tmp() {
    // /tmp may be noexec; the staged pastor has to run.
    let r = FakeRunner::new(&[]);
    let o = run(&r, &options(true));
    assert!(o.ok, "{:#?}", o.steps);
    assert!(
        r.called("sudo -n mktemp -d /var/tmp/pastor-bootstrap.XXXXXX"),
        "{:#?}",
        r.calls()
    );
    assert!(!r.called_containing(" /tmp/pastor-bootstrap"));
}
