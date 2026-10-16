use std::path::PathBuf;
use std::sync::Mutex;

use super::*;
use crate::cli::CliError;
use crate::setup::CmdOutput;

/// Answers each call by the first rule whose prefix it starts with, after
/// the `sudo -n [-u U -H env XDG_RUNTIME_DIR=D]` wrapper is taken off, so a
/// rule reads the same in user and root mode. A call no rule names succeeds
/// with no output.
struct FakeRunner {
    rules: Vec<(String, bool, String)>,
    calls: Mutex<Vec<String>>,
}

impl FakeRunner {
    /// A Debian x86_64 box with systemd and apt, logged in as `me` (uid
    /// 1000) with passwordless sudo, with every package there and nothing of
    /// pastor's yet. `over` rules come first, so they win.
    fn new(over: &[(&str, bool, &str)]) -> FakeRunner {
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
            ("printenv HOME", true, "/home/me"),
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
            ("stat", false, ""),
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

    fn calls(&self) -> Vec<String> {
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
fn unwrapped(call: &str) -> &str {
    let Some(rest) = call.strip_prefix("sudo -n ") else {
        return call;
    };
    let Some(as_user) = rest.strip_prefix("-u ") else {
        return rest;
    };
    // `-u U -H env XDG_RUNTIME_DIR=D <command>`
    as_user.splitn(5, ' ').nth(4).unwrap_or(as_user)
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
        let (success, stdout) = self
            .rules
            .iter()
            .find(|(p, _, _)| matches(inner, p) || matches(&call, p))
            .map_or((true, String::new()), |(_, ok, out)| {
                (*ok, format!("{out}\n"))
            });
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

fn step<'a>(o: &'a Outcome, name: &str) -> &'a StepResult {
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
    assert!(r.called(
        "sudo -n -u pastor -H env XDG_RUNTIME_DIR=/run/user/991 \
         ssh-keygen -t ed25519 -N  -f /home/pastor/.local/state/pastor/machine_key"
    ));
    assert!(r.called(
        "sudo -n -u pastor-agent -H env XDG_RUNTIME_DIR=/run/user/990 \
         ssh-keygen -t ed25519 -N  -f /home/pastor-agent/.local/state/pastor/agent_key"
    ));
    assert!(r.called(
        "sudo -n -u pastor-agent -H env XDG_RUNTIME_DIR=/run/user/990 \
         /home/pastor-agent/.local/bin/pastor setup systemd --herdr --yes --require-linger"
    ));
    // The head's key goes to the agent user, the one the head reaches.
    assert!(r.called_containing("/home/pastor-agent/.ssh/authorized_keys"));
    assert!(!r.called_containing("/home/me/.ssh/authorized_keys"));
    // pastor for both users, herdr for the agent user only.
    assert!(r.called(
        "sudo -n install -o pastor -m 0755 /tmp/cache/pastor /home/pastor/.local/bin/pastor"
    ));
    assert!(r.called(
        "sudo -n install -o pastor-agent -m 0755 /tmp/cache/pastor /home/pastor-agent/.local/bin/pastor"
    ));
    assert!(!r.called_containing("/home/pastor/.local/bin/herdr"));
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
    let state = "/home/pastor/.local/state/pastor";
    assert!(
        r.called(&format!("sudo -n chown pastor: {state}")),
        "{:#?}",
        r.calls()
    );
    assert!(r.called(&format!("sudo -n chmod 700 {state}")));
    // And the home it sits in is closed to the agent user too.
    assert!(r.called("sudo -n chmod 700 /home/pastor"));
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
        ("printenv HOME", true, "/Users/me"),
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
