use super::*;

/// One scripted answer: `(success, stdout, stderr)`.
type Answer = (bool, String, String);

/// What a `FakeRunner` answers: the user a wrapped call runs as (`None`
/// for this login), the program, its args.
type AnswerFn = Box<dyn Fn(Option<&str>, &str, &[&str]) -> Option<Answer>>;

/// Records every `Runner::run` call and answers from `answer`, which sees
/// the program and the args with any `setsid ... env -i ... LANG=C` wrapper
/// peeled off, plus the user it came wrapped as. A call it has
/// no answer for succeeds with no output.
struct FakeRunner {
    calls: std::sync::Mutex<Vec<(String, Vec<String>)>>,
    answer: AnswerFn,
}

impl FakeRunner {
    fn new(answer: impl Fn(Option<&str>, &str, &[&str]) -> Option<Answer> + 'static) -> Self {
        FakeRunner {
            calls: Default::default(),
            answer: Box::new(answer),
        }
    }

    fn calls(&self) -> Vec<(String, Vec<String>)> {
        self.calls.lock().unwrap().clone()
    }

    /// The commands run as pastor-agent, wrapper peeled off.
    fn as_agent(&self) -> Vec<Vec<String>> {
        self.as_user("pastor-agent")
    }

    /// The commands run as `name`, wrapper peeled off.
    fn as_user(&self, name: &str) -> Vec<Vec<String>> {
        self.calls()
            .into_iter()
            .filter(|(p, a)| p == "setsid" && user_of(a) == name)
            .map(|(_, a)| peel(&a).to_vec())
            .collect()
    }

    /// The commands run directly, as this login.
    fn direct(&self) -> Vec<Vec<String>> {
        self.calls()
            .into_iter()
            .filter(|(p, _)| p != "setsid")
            .map(|(p, a)| std::iter::once(p).chain(a).collect())
            .collect()
    }
}

/// What a `setsid -w sudo -n -u U -- env -i ... LANG=C prog args` runs.
fn peel<T: AsRef<str>>(args: &[T]) -> &[T] {
    let at = args
        .iter()
        .position(|a| a.as_ref() == "LANG=C")
        .expect("bootstrap's wrapper");
    &args[at + 1..]
}

/// The user a `setsid -w sudo -n -u U ...` (or `runuser -u U`) runs as.
fn user_of<T: AsRef<str>>(args: &[T]) -> &str {
    let at = args
        .iter()
        .position(|a| a.as_ref() == "-u")
        .expect("bootstrap's wrapper");
    args[at + 1].as_ref()
}

impl Runner for FakeRunner {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<CmdOutput> {
        self.calls.lock().unwrap().push((
            program.to_string(),
            args.iter().map(|a| a.to_string()).collect(),
        ));
        let (wrapped, prog, rest) = if program == "setsid" {
            let inner = peel(args);
            (Some(user_of(args)), inner[0], &inner[1..])
        } else {
            (None, program, args)
        };
        let (success, stdout, stderr) =
            (self.answer)(wrapped, prog, rest).unwrap_or((true, String::new(), String::new()));
        Ok(CmdOutput {
            success,
            stdout,
            stderr,
        })
    }
}

fn ok(stdout: &str) -> Option<Answer> {
    Some((true, stdout.to_string(), String::new()))
}

fn fail(stderr: &str) -> Option<Answer> {
    Some((false, String::new(), stderr.to_string()))
}

/// What root mode on Linux leaves: this login `pi` (uid 1000), and a
/// pastor-agent (uid 1500) whose own pastor keeps its state under its home.
fn root_mode(wrapped: Option<&str>, prog: &str, args: &[&str]) -> Option<Answer> {
    match (wrapped, prog, args) {
        (None, "id", ["-u"]) => ok("1000\n"),
        (None, "id", ["-un"]) => ok("pi\n"),
        (None, "getent", ["passwd", "pastor-agent"]) => {
            ok("pastor-agent:x:1500:1500::/home/pastor-agent:/bin/bash\n")
        }
        (Some("pastor-agent"), "pastor", ["bootstrap", "--print-state-dir"]) => {
            ok("/home/pastor-agent/.local/state/pastor\n")
        }
        _ => None,
    }
}

/// A login with no pastor-agent: user-mode bootstrap.
fn user_mode(wrapped: Option<&str>, prog: &str, args: &[&str]) -> Option<Answer> {
    match (wrapped, prog, args) {
        (None, "getent", ["passwd", "pastor-agent"]) => fail(""),
        _ => root_mode(wrapped, prog, args),
    }
}

struct Env {
    _tmp: tempfile::TempDir,
    paths: Paths,
    here: Here,
}

/// This login with both units and both keys (each with its `.pub`).
fn env(os: Os) -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
    paths.ensure().unwrap();
    for key in [paths.machine_key(), paths.agent_key()] {
        std::fs::write(&key, "key").unwrap();
        std::fs::write(format!("{}.pub", key.display()), "pub").unwrap();
    }
    let unit_dir = tmp.path().join("units");
    std::fs::create_dir_all(&unit_dir).unwrap();
    for unit in [Unit::Pastor, Unit::Herdr] {
        let file = match os {
            Os::Linux => unit.file_name(),
            Os::Macos => unit.plist_name(),
        };
        std::fs::write(unit_dir.join(file), "unit").unwrap();
    }
    let sudoers_dir = tmp.path().join("sudoers.d");
    Env {
        _tmp: tmp,
        paths,
        here: Here {
            os,
            unit_dir,
            sudoers_dir,
        },
    }
}

fn v(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

fn keys_gone(paths: &Paths) {
    for key in [paths.machine_key(), paths.agent_key()] {
        assert!(!key.exists(), "{} left", key.display());
        let public = PathBuf::from(format!("{}.pub", key.display()));
        assert!(!public.exists(), "{} left", public.display());
    }
}

/// Root mode: both users' units go (stopped, removed, the manager reloaded),
/// pastor-agent's are found through its own pastor's state dir and reached
/// through bootstrap's clean-env wrapper, and every key goes with its `.pub`.
#[test]
fn leave_in_root_mode_removes_both_users() {
    let e = env(Os::Linux);
    let r = FakeRunner::new(root_mode);
    let report = run(&e.paths, &e.here, &r);
    assert!(report.failed.is_empty(), "{report:?}");

    let direct = r.direct();
    for unit in ["pastor.service", "herdr.service"] {
        assert!(
            direct.contains(&v(&["systemctl", "--user", "disable", "--now", unit])),
            "{direct:?}"
        );
        assert!(!e.here.unit_dir.join(unit).exists());
    }
    assert!(
        direct.contains(&v(&["systemctl", "--user", "daemon-reload"])),
        "{direct:?}"
    );
    keys_gone(&e.paths);

    let agent = r.as_agent();
    assert!(
        agent.contains(&v(&["pastor", "bootstrap", "--print-state-dir"])),
        "{agent:?}"
    );
    for unit in ["pastor.service", "herdr.service"] {
        let path = format!("/home/pastor-agent/.config/systemd/user/{unit}");
        assert!(agent.contains(&v(&["systemctl", "--user", "disable", "--now", unit])));
        assert!(agent.contains(&v(&["rm", "-f", "--", &path])), "{agent:?}");
    }
    assert!(
        agent.contains(&v(&["systemctl", "--user", "daemon-reload"])),
        "{agent:?}"
    );
    assert!(
        agent.contains(&v(&[
            "rm",
            "-f",
            "--",
            "/home/pastor-agent/.local/state/pastor/agent_key",
            "/home/pastor-agent/.local/state/pastor/agent_key.pub",
        ])),
        "{agent:?}"
    );
    // As bootstrap runs other users' commands: no caller env, no tty.
    let wrapped = r
        .calls()
        .into_iter()
        .find(|(p, _)| p == "setsid")
        .unwrap()
        .1;
    assert!(
        wrapped
            .windows(4)
            .any(|w| w == ["sudo", "-n", "-u", "pastor-agent"]),
        "{wrapped:?}"
    );
    assert!(wrapped.contains(&"-i".to_string()), "{wrapped:?}");
    assert!(wrapped.contains(&"XDG_RUNTIME_DIR=/run/user/1500".to_string()));
}

/// User mode: no pastor-agent, so nothing runs as anyone else.
#[test]
fn leave_in_user_mode_touches_only_this_login() {
    let e = env(Os::Linux);
    let r = FakeRunner::new(user_mode);
    let report = run(&e.paths, &e.here, &r);
    assert!(report.failed.is_empty(), "{report:?}");
    assert!(r.as_agent().is_empty(), "{:?}", r.calls());
    assert!(!r.calls().iter().any(|(p, _)| p == "sudo" || p == "runuser"));
    keys_gone(&e.paths);
    assert!(
        report.done.iter().any(|l| l.contains("removed")),
        "{report:?}"
    );
    assert!(report.finish().is_ok());
}

/// Every step that fails is in the report, the rest still run, and the
/// command ends in `leave_incomplete`.
#[test]
fn leave_reports_each_failed_step() {
    let e = env(Os::Linux);
    // A key that cannot be removed as a file.
    std::fs::remove_file(e.paths.agent_key()).unwrap();
    std::fs::create_dir(e.paths.agent_key()).unwrap();
    let r = FakeRunner::new(|wrapped, prog, args| match (wrapped, prog, args) {
        (None, "systemctl", ["--user", "disable", "--now", "herdr.service"]) => {
            fail("Failed to connect to bus: No medium found")
        }
        (None, "systemctl", ["--user", "daemon-reload"]) => fail("no bus"),
        (Some(_), "test", _) => Some((true, String::new(), String::new())),
        (Some(_), "rm", ["-f", "--", k, _]) if k.ends_with("agent_key") => {
            fail("rm: cannot remove: Permission denied")
        }
        _ => root_mode(wrapped, prog, args),
    });
    let report = run(&e.paths, &e.here, &r);
    let failed = report.failed.join("\n");
    assert!(
        failed.contains("could not stop and disable herdr.service"),
        "{failed}"
    );
    assert!(failed.contains("No medium found"), "{failed}");
    assert!(
        failed.contains("could not reload its systemd user manager"),
        "{failed}"
    );
    assert!(
        failed.contains("could not delete the agent key"),
        "{failed}"
    );
    assert!(
        failed.contains("pastor-agent: could not delete its agent key")
            && failed.contains("Permission denied"),
        "{failed}"
    );
    // The machine key and both units still went.
    assert!(!e.paths.machine_key().exists());
    assert!(!e.here.unit_dir.join("herdr.service").exists());
    let err = report.finish().unwrap_err();
    assert_eq!(
        err.downcast_ref::<CliError>().unwrap().code,
        "leave_incomplete"
    );
}

/// pastor-agent's state dir comes from its own pastor; when that cannot be
/// asked (sudo refuses), leave says so and guesses no path.
#[test]
fn leave_never_guesses_the_agent_state_dir() {
    let e = env(Os::Linux);
    let r = FakeRunner::new(|wrapped, prog, args| match (wrapped, prog, args) {
        (Some(_), "pastor", _) => fail("sudo: a password is required"),
        _ => root_mode(wrapped, prog, args),
    });
    let report = run(&e.paths, &e.here, &r);
    let failed = report.failed.join("\n");
    assert!(
        failed.contains("could not ask its pastor for its state dir")
            && failed.contains("a password is required"),
        "{failed}"
    );
    let agent = r.as_agent();
    assert_eq!(
        agent,
        vec![v(&["pastor", "bootstrap", "--print-state-dir"])]
    );
}

/// A unit that is not there is not touched, and a login with nothing left
/// finishes cleanly.
#[test]
fn leave_skips_what_is_not_there() {
    let e = env(Os::Linux);
    std::fs::remove_file(e.here.unit_dir.join("pastor.service")).unwrap();
    let r = FakeRunner::new(|wrapped, prog, args| match (wrapped, prog, args) {
        (Some(_), "test", _) => fail(""),
        _ => root_mode(wrapped, prog, args),
    });
    let report = run(&e.paths, &e.here, &r);
    assert!(report.failed.is_empty(), "{report:?}");
    let direct = r.direct();
    assert!(
        !direct
            .iter()
            .any(|c| c.contains(&"pastor.service".to_string()))
    );
    assert!(direct.contains(&v(&[
        "systemctl",
        "--user",
        "disable",
        "--now",
        "herdr.service"
    ])));
    // pastor-agent has nothing: only its state dir and the looks.
    for c in r.as_agent() {
        assert!(c[0] == "test" || c[0] == "pastor", "{c:?}");
    }
}

/// macOS: each LaunchAgent is booted out of the login's gui domain and its
/// plist removed; one not loaded is no failure.
#[test]
fn leave_on_macos_boots_out_launch_agents() {
    let e = env(Os::Macos);
    let r = FakeRunner::new(|wrapped, prog, args| match (wrapped, prog, args) {
        (None, "launchctl", ["bootout", "gui/1000/pastor.herdr"]) => {
            fail("Boot-out failed: 3: No such process")
        }
        (None, "dscl", _) => fail(""),
        _ => root_mode(wrapped, prog, args),
    });
    let report = run(&e.paths, &e.here, &r);
    assert!(report.failed.is_empty(), "{report:?}");
    let direct = r.direct();
    for label in ["pastor.serve", "pastor.herdr"] {
        assert!(
            direct.contains(&v(&["launchctl", "bootout", &format!("gui/1000/{label}")])),
            "{direct:?}"
        );
        assert!(!e.here.unit_dir.join(format!("{label}.plist")).exists());
    }
    assert!(!direct.iter().any(|c| c[0] == "systemctl"), "{direct:?}");
}

/// A head here refuses; a headless serve (a shepherd) or none goes ahead.
#[test]
fn leave_refuses_on_the_head() {
    let pong = |role: Option<&str>| HeadPing::Pong {
        version: "0.9.0".into(),
        protocol: 1,
        role: role.map(String::from),
    };
    let err = refuse_on_head(&pong(None)).unwrap_err();
    assert_eq!(
        err.downcast_ref::<CliError>().unwrap().code,
        "leave_on_head"
    );
    assert!(refuse_on_head(&pong(Some(SHEPHERD_ROLE))).is_ok());
    assert!(refuse_on_head(&HeadPing::NotRunning).is_ok());
}

/// A root login (`curl … | sudo sh -s -- join` with no --service-user): uid
/// 0, with the shepherd run as `pastor` (uid 1400), which root mode's
/// sudoers drop-in names, beside pastor-agent.
fn root_login(wrapped: Option<&str>, prog: &str, args: &[&str]) -> Option<Answer> {
    match (wrapped, prog, args) {
        (None, "id", ["-u"]) => ok("0\n"),
        (None, "id", ["-un"]) => ok("root\n"),
        (None, "getent", ["passwd", "pastor"]) => ok("pastor:x:1400:1400::/home/pastor:/bin/sh\n"),
        (Some("pastor"), "pastor", ["bootstrap", "--print-state-dir"]) => {
            ok("/home/pastor/.local/state/pastor\n")
        }
        _ => root_mode(wrapped, prog, args),
    }
}

/// The shepherd's service user is not this login: its units, its machine
/// key and root mode's sudoers drop-in go too, as that user, before
/// pastor-agent's.
#[test]
fn leave_as_root_removes_the_service_user() {
    let e = env(Os::Linux);
    std::fs::create_dir_all(&e.here.sudoers_dir).unwrap();
    let drop_in = e.here.sudoers_dir.join("pastor-pastor");
    std::fs::write(&drop_in, "pastor ALL=(pastor-agent) NOPASSWD: ...").unwrap();
    let r = FakeRunner::new(root_login);
    let report = run(&e.paths, &e.here, &r);
    assert!(report.failed.is_empty(), "{report:?}");

    let shepherd = r.as_user("pastor");
    for unit in ["pastor.service", "herdr.service"] {
        let path = format!("/home/pastor/.config/systemd/user/{unit}");
        assert!(
            shepherd.contains(&v(&["systemctl", "--user", "disable", "--now", unit])),
            "{shepherd:?}"
        );
        assert!(
            shepherd.contains(&v(&["rm", "-f", "--", &path])),
            "{shepherd:?}"
        );
    }
    for key in ["machine_key", "agent_key"] {
        let state = "/home/pastor/.local/state/pastor";
        assert!(
            shepherd.contains(&v(&[
                "rm",
                "-f",
                "--",
                &format!("{state}/{key}"),
                &format!("{state}/{key}.pub"),
            ])),
            "{shepherd:?}"
        );
    }
    assert!(!drop_in.exists());
    // As root: runuser, never sudo.
    assert!(
        r.calls()
            .iter()
            .filter(|(p, _)| p == "setsid")
            .all(|(_, a)| a[1] == "runuser"),
        "{:?}",
        r.calls()
    );
    // The shepherd goes before the herdr it drives.
    let calls = r.calls();
    let first = |user: &str| {
        calls
            .iter()
            .position(|(p, a)| p == "setsid" && user_of(a) == user)
            .unwrap()
    };
    assert!(first("pastor") < first("pastor-agent"));
    assert!(!r.as_agent().is_empty());
    assert!(report.finish().is_ok());
}

/// With no drop-in to name it, a `pastor` user is checked first: one with
/// no pastor and no pastor unit in its home is someone else's and is left
/// alone.
#[test]
fn leave_leaves_a_pastor_user_that_is_not_ours() {
    let e = env(Os::Linux);
    let r = FakeRunner::new(|wrapped, prog, args| match (wrapped, prog, args) {
        (Some("pastor"), "test", _) => fail(""),
        _ => root_login(wrapped, prog, args),
    });
    let report = run(&e.paths, &e.here, &r);
    assert!(report.failed.is_empty(), "{report:?}");
    for c in r.as_user("pastor") {
        assert!(c[0] == "true" || c[0] == "test", "{c:?}");
    }
}

/// A `pastor` user with pastor's unit that this process cannot run
/// commands as (sudo wants a password) is not passed over in silence: the
/// shepherd may still run, so leave ends in `leave_incomplete`.
#[test]
fn leave_fails_when_it_cannot_reach_the_service_user() {
    let e = env(Os::Linux);
    let r = FakeRunner::new(|wrapped, prog, args| match (wrapped, prog, args) {
        (Some("pastor"), _, _) => fail("sudo: a password is required"),
        (None, "id", ["-u"]) => ok("1000\n"),
        (None, "id", ["-un"]) => ok("pi\n"),
        _ => root_login(wrapped, prog, args),
    });
    let report = run(&e.paths, &e.here, &r);
    let failed = report.failed.join("\n");
    assert!(
        failed.contains("pastor: could not check whether it runs pastor's shepherd"),
        "{failed}"
    );
    let err = report.finish().unwrap_err();
    assert_eq!(
        err.downcast_ref::<CliError>().unwrap().code,
        "leave_incomplete"
    );
}

/// A drop-in names a user that is gone: the drop-in still goes, and the
/// report says what it could not check.
#[test]
fn leave_reports_a_recorded_service_user_that_is_gone() {
    let e = env(Os::Linux);
    std::fs::create_dir_all(&e.here.sudoers_dir).unwrap();
    let drop_in = e.here.sudoers_dir.join("pastor-svc");
    std::fs::write(&drop_in, "svc ALL=...").unwrap();
    let r = FakeRunner::new(|wrapped, prog, args| match (wrapped, prog, args) {
        (None, "getent", ["passwd", "svc"]) => fail(""),
        (None, "getent", ["passwd", "pastor"]) => fail(""),
        _ => root_login(wrapped, prog, args),
    });
    let report = run(&e.paths, &e.here, &r);
    assert!(!drop_in.exists());
    let failed = report.failed.join("\n");
    assert!(
        failed.contains("svc: its sudoers drop-in names it"),
        "{failed}"
    );
}
