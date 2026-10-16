use super::*;

/// One scripted answer: `(success, stdout, stderr)`.
type Answer = (bool, String, String);

/// What a `FakeRunner` answers: whether the call came wrapped, the program, its args.
type AnswerFn = Box<dyn Fn(bool, &str, &[&str]) -> Option<Answer>>;

/// Records every `Runner::run` call and answers from `answer`, which sees
/// the program and the args with any `setsid ... env -i ... LANG=C` wrapper
/// peeled off, plus whether it came wrapped (as pastor-agent). A call it has
/// no answer for succeeds with no output.
struct FakeRunner {
    calls: std::sync::Mutex<Vec<(String, Vec<String>)>>,
    answer: AnswerFn,
}

impl FakeRunner {
    fn new(answer: impl Fn(bool, &str, &[&str]) -> Option<Answer> + 'static) -> Self {
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
        self.calls()
            .into_iter()
            .filter(|(p, _)| p == "setsid")
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

impl Runner for FakeRunner {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<CmdOutput> {
        self.calls.lock().unwrap().push((
            program.to_string(),
            args.iter().map(|a| a.to_string()).collect(),
        ));
        let (wrapped, prog, rest) = if program == "setsid" {
            let inner = peel(args);
            (true, inner[0], &inner[1..])
        } else {
            (false, program, args)
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
fn root_mode(wrapped: bool, prog: &str, args: &[&str]) -> Option<Answer> {
    match (wrapped, prog, args) {
        (false, "id", ["-u"]) => ok("1000\n"),
        (false, "id", ["-un"]) => ok("pi\n"),
        (false, "getent", ["passwd", "pastor-agent"]) => {
            ok("pastor-agent:x:1500:1500::/home/pastor-agent:/bin/bash\n")
        }
        (true, "pastor", ["bootstrap", "--print-state-dir"]) => {
            ok("/home/pastor-agent/.local/state/pastor\n")
        }
        _ => None,
    }
}

/// A login with no pastor-agent: user-mode bootstrap.
fn user_mode(wrapped: bool, prog: &str, args: &[&str]) -> Option<Answer> {
    match (wrapped, prog, args) {
        (false, "getent", ["passwd", "pastor-agent"]) => fail(""),
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
    Env {
        _tmp: tmp,
        paths,
        here: Here { os, unit_dir },
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
        (false, "systemctl", ["--user", "disable", "--now", "herdr.service"]) => {
            fail("Failed to connect to bus: No medium found")
        }
        (false, "systemctl", ["--user", "daemon-reload"]) => fail("no bus"),
        (true, "test", _) => Some((true, String::new(), String::new())),
        (true, "rm", ["-f", "--", k, _]) if k.ends_with("agent_key") => {
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
        (true, "pastor", _) => fail("sudo: a password is required"),
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
        (true, "test", _) => fail(""),
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
        (false, "launchctl", ["bootout", "gui/1000/pastor.herdr"]) => {
            fail("Boot-out failed: 3: No such process")
        }
        (false, "dscl", _) => fail(""),
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
