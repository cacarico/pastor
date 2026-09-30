use crate::helpers::*;

#[test]
fn machine_add_and_remove_edit_the_file() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .output()
            .unwrap()
    };
    let out = run(&[
        "machine",
        "add",
        "pi-3",
        "fleet@pi-3",
        "--max-agents",
        "3",
        "--tag",
        "fast",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = std::fs::read_to_string(config.join("flock.toml")).unwrap();
    assert!(
        text.contains("name = \"pi-3\"")
            && text.contains("ssh = \"fleet@pi-3\"")
            && text.contains("max_agents = 3"),
        "{text}"
    );
    let out = run(&["machine", "add", "pi-3", "fleet@pi-3"]);
    assert!(!out.status.success());
    // `--branch` only means something for a worktree; clap rejects it alone
    // before any daemon is asked.
    let out = run(&["task", "run", "hi", "--branch", "b"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(run(&["machine", "remove", "pi-3"]).status.success());
    assert!(!run(&["machine", "remove", "pi-3"]).status.success());
}

/// The line pastor prints for a machine's key names this very binary, and
/// the machine must be in flock.toml. The key comes from a file or stdin.
#[test]
fn authorized_key_prints_the_locked_line() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("flock.toml"),
        "[[machine]]\nname = \"pi-1\"\nssh = \"user@pi-1\"\n",
    )
    .unwrap();
    let key = tmp.path().join("id.pub");
    std::fs::write(&key, "ssh-ed25519 AAAAC3Nza user@pi-1\n").unwrap();
    let run = |args: &[&str], stdin: &str| {
        let mut child = pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    let exe = std::fs::canonicalize(env!("CARGO_BIN_EXE_pastor")).unwrap();
    let want = format!(
        "command=\"{} bridge --agent --machine pi-1\",no-pty,no-user-rc,no-port-forwarding,no-agent-forwarding,no-X11-forwarding ssh-ed25519 AAAAC3Nza user@pi-1\n",
        exe.display()
    );
    let out = run(
        &[
            "machine",
            "authorized-key",
            "pi-1",
            "--key",
            key.to_str().unwrap(),
        ],
        "",
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), want);
    let out = run(
        &["machine", "authorized-key", "pi-1", "--key", "-"],
        "ssh-ed25519 AAAAC3Nza user@pi-1\n",
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), want);
    let out = run(
        &[
            "machine",
            "authorized-key",
            "pi-9",
            "--key",
            key.to_str().unwrap(),
        ],
        "",
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown_machine"));
}

/// Copilot 4102376166: a socket that accepts but never answers `Ping` is
/// `Unresponsive` (a busy head), not the same as no daemon at all, so
/// `machine add` must not send the "start pastor serve" advice. Copilot
/// 4106353416, 4106669969: nor may it write the edit next to a head it
/// cannot check; it is refused (`head_unresponsive`) with flock.toml
/// untouched.
#[test]
fn machine_add_refuses_a_wedged_head_rather_than_taking_it_for_no_head() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&state).unwrap();
    let socket = state.join("pastor.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    // Accept the connection and never reply, so the probe times out into
    // `Unresponsive` rather than reading as an absent daemon.
    std::thread::spawn(move || {
        let _ = listener.accept();
        std::thread::sleep(Duration::from_secs(10));
    });
    let out = pastor()
        .args(["machine", "add", "pi-9", "fleet@pi-9"])
        .env("PASTOR_CONFIG_DIR", &config)
        .env("PASTOR_STATE_DIR", &state)
        .output()
        .unwrap();
    assert_eq!(error_code(&out), "head_unresponsive");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        !stderr.contains("start pastor serve") && stderr.contains("not answering"),
        "a busy head must not get the start advice: {stderr}"
    );
    assert!(!config.join("flock.toml").exists());
}

/// `--herdr` keeps herdr's saved-machine list in step with the flock: `add`
/// runs `herdr machine add`, `remove` resolves the label to a profile id via
/// `herdr machine list` and runs `herdr machine remove`. A fake `herdr` script
/// first on PATH records what pastor asked of it.
#[test]
fn herdr_flag_adds_and_removes_the_saved_machine_too() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = tmp.path().join("herdr.log");
    let script = format!(
        "#!/bin/sh\necho \"$@\" >> {log}\ncase \"$*\" in *boom*) echo 'herdr says no' >&2; exit 1;; esac\n\
         if [ \"$1 $2\" = \"machine list\" ]; then printf 'id-1\\tother\\tx@y\\tdefault\\tenabled\\nid-2\\tpi-3\\tfleet@pi-3\\tdefault\\tenabled\\n'; fi\n",
        log = log.display()
    );
    std::fs::write(bin.join("herdr"), script).unwrap();
    std::fs::set_permissions(bin.join("herdr"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PATH", &path)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .output()
            .unwrap()
    };
    let calls = || std::fs::read_to_string(&log).unwrap_or_default();

    let out = run(&[
        "machine",
        "add",
        "pi-3",
        "fleet@pi-3",
        "--session",
        "work",
        "--herdr",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        calls().trim(),
        "machine add fleet@pi-3 --label pi-3 --remote-session work"
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        !text.contains("to see it in your laptop"),
        "the hint is replaced by the action: {text}"
    );
    assert!(text.contains("saved in herdr"), "{text}");

    let out = run(&["machine", "remove", "pi-3", "--herdr"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = calls();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[1], "machine list");
    assert_eq!(
        lines[2], "machine remove id-2",
        "resolved by label, not by name"
    );
    assert!(
        !std::fs::read_to_string(config.join("flock.toml"))
            .unwrap()
            .contains("pi-3")
    );

    // Not saved in herdr under that label: the flock edit still happens, a
    // note says so, exit 0. herdr does know the same host under another label
    // (`other`, id-1), so the note names it with the command that removes it,
    // and pastor does not remove it on its own.
    assert!(run(&["machine", "add", "pi-9", "x@y"]).status.success());
    let out = run(&["machine", "remove", "pi-9", "--herdr"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no saved machine"), "{err}");
    assert!(
        err.contains("other") && err.contains("herdr machine remove id-1"),
        "the hint names the saved machine at the same target: {err}"
    );
    assert!(
        !calls()
            .lines()
            .any(|l| l.starts_with("machine remove id-") && l != "machine remove id-2")
    );

    // herdr failing: the flock edit is kept, the failure is reported with herdr's stderr, exit 1.
    let out = run(&["machine", "add", "boom", "fleet@boom", "--herdr"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("herdr_error") && err.contains("herdr says no"),
        "{err}"
    );
    assert!(
        std::fs::read_to_string(config.join("flock.toml"))
            .unwrap()
            .contains("name = \"boom\"")
    );
}

/// `machine list` without a head connects directly, so nothing else has made the
/// state dir yet. The ssh ControlMaster socket directory must exist, private,
/// before ssh starts. A fake `ssh` first on PATH records that it did and fails
/// the way an unreachable host does, so no real ssh runs.
#[test]
fn machine_list_without_daemon_creates_the_ssh_dir_private_before_ssh_runs() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let seen = tmp.path().join("seen");
    let script = format!(
        "#!/bin/sh\n[ -d {ssh} ] && echo present > {seen}\necho 'no route to host' >&2\nexit 255\n",
        ssh = state.join("ssh").display(),
        seen = seen.display()
    );
    std::fs::write(bin.join("ssh"), script).unwrap();
    std::fs::set_permissions(bin.join("ssh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PATH", &path)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .output()
            .unwrap()
    };
    assert!(
        run(&["machine", "add", "pi-3", "fleet@pi-3"])
            .status
            .success()
    );
    let _ = std::fs::remove_dir_all(&state);

    let out = run(&["machine", "list", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&seen).unwrap_or_default().trim(),
        "present",
        "ssh ran before its ControlPath directory existed"
    );
    for dir in [state.clone(), state.join("ssh")] {
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{} is {mode:o}", dir.display());
    }
}

/// With a head running, `machine list` starts with the head's own row and
/// gives each machine its host; `--json` keeps the head out of `machines`.
#[test]
fn machine_list_opens_with_a_line_about_the_head() {
    let env = start();
    let out = env.cmd(&["machine", "list"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    // The head runs no agents here (its one machine is a command bridge), so
    // the line does not end by naming it. The hostname and herdr version
    // are this host's.
    let first = stdout.lines().next().unwrap();
    assert!(
        first.starts_with(&format!("pastor {} on ", env!("CARGO_PKG_VERSION"))),
        "{stdout}"
    );
    assert!(first.contains(" (herdr "), "{stdout}");
    assert!(first.ends_with("), 1 machine"), "{stdout}");
    assert_eq!(stdout.lines().nth(1), Some(""), "{stdout}");
    let lines: Vec<Vec<&str>> = stdout
        .lines()
        .skip(2)
        .map(|l| l.split_whitespace().collect())
        .collect();
    assert_eq!(
        lines[0],
        [
            "NAME", "HOST", "FLOCKS", "PROFILE", "CHANNEL", "HERDR", "PASTOR", "AGENTS", "ORPHANS",
            "TAGS", "ERROR"
        ],
        "{stdout}"
    );
    assert_eq!(
        lines[1][..5],
        ["fake", "fake-herdr", "default", "-", "connected"],
        "{stdout}"
    );
    // A command bridge cannot say which pastor is behind it.
    assert_eq!(lines[1][6..8], ["-", "0/2+1j+1b"], "{stdout}");
    assert_eq!(lines.len(), 2, "{stdout}");

    let out = env.cmd(&["machine", "list", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["head"]["name"], "pastor");
    assert_eq!(v["head"]["channel"], "head");
    assert_eq!(v["head"]["pastor_version"], env!("CARGO_PKG_VERSION"));
    let ms = v["machines"].as_array().unwrap();
    assert_eq!(ms.len(), 1, "{v}");
    assert_eq!(ms[0]["name"], "fake");
    assert!(ms[0]["pastor_version"].is_null(), "{v}");
    assert_eq!(ms[0]["host"], "fake-herdr");
    assert_eq!(ms[0]["channel"], "connected");
    assert_eq!(ms[0]["flock"], "default");

    // --flock narrows the machines, and the line counts what it shows.
    ok(env.cmd(&["flock", "add", "work"]));
    let out = ok(env.cmd(&["machine", "list", "--flock", "work"]));
    assert!(
        out.lines().next().unwrap().ends_with("), 0 machines"),
        "{out}"
    );
    assert_eq!(out.lines().count(), 3, "the header only: {out}");
    let v: serde_json::Value = serde_json::from_str(&ok(
        env.cmd(&["machine", "list", "--json", "--flock", "default"])
    ))
    .unwrap();
    assert_eq!(v["machines"].as_array().unwrap().len(), 1, "{v}");
}

/// With no head, `machine list` probes each machine itself: a reachable one
/// reads `probed` with herdr's agent count, one that cannot be reached reads
/// `unreachable` with the reason. The note goes to stderr only on success, so
/// a failure still leaves exactly one JSON value there.
#[test]
fn machine_list_without_daemon_probes_each_machine() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();
    let socket = tmp.path().join("herdr.sock");
    // Killed on drop, so a failed assertion does not leave it running.
    struct Server(std::process::Child);
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _herdr = Server(
        Command::new(env!("CARGO_BIN_EXE_fake-herdr"))
            .arg("--listen")
            .arg(&socket)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + WAIT;
    while !socket.exists() {
        assert!(Instant::now() < deadline, "fake herdr never listened");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::fs::write(
        config.join("flock.toml"),
        format!(
            "[[machine]]\nname = \"fake\"\ncommand = [\"{}\", \"--connect\", \"{}\"]\nmax_agents = 2\ntags = [\"arm\"]\n\n\
             [[machine]]\nname = \"gone\"\ncommand = [\"{}\"]\nmax_agents = 1\n",
            env!("CARGO_BIN_EXE_fake-herdr"),
            socket.display(),
            tmp.path().join("no-such-bridge").display()
        ),
    )
    .unwrap();
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .output()
            .unwrap()
    };

    let out = run(&["machine", "list"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "pastor serve is not running; probed the machines directly"
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<Vec<&str>> = stdout
        .lines()
        .map(|l| l.split_whitespace().collect())
        .collect();
    // No head, no line about it: the stderr notice says so instead.
    assert_eq!(
        lines[0][..5],
        ["NAME", "HOST", "FLOCKS", "PROFILE", "CHANNEL"],
        "{stdout}"
    );
    assert_eq!(
        lines[1][..5],
        ["fake", "fake-herdr", "default", "-", "probed"],
        "{stdout}"
    );
    assert_eq!(&lines[1][6..], ["-", "0/2+1j+1b", "-", "arm"], "{stdout}");
    assert_eq!(
        lines[2][..5],
        ["gone", "no-such-bridge", "default", "-", "unreachable"],
        "{stdout}"
    );
    assert_eq!(lines[2][5..8], ["-", "-", "-/1+1j+1b"], "{stdout}");
    assert!(lines[2].len() > 10, "ERROR should say why: {stdout}");

    // The same JSON shape as with a head.
    let out = run(&["machine", "list", "--json"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["head"]["name"], "pastor");
    let ms = v["machines"].as_array().unwrap();
    assert_eq!(ms.len(), 2, "{v}");
    assert_eq!(ms[1]["name"], "gone");
    assert_eq!(ms[1]["channel"], "unreachable");
    assert!(ms[1]["error"].is_string(), "{v}");
    assert_eq!(ms[0]["channel"], "probed");
    assert_eq!(ms[0]["live"], 0);
    assert_eq!(ms[0]["job_slots"], 1, "{v}");
    assert_eq!(ms[0]["burst"], 1, "{v}");

    std::fs::write(config.join("flock.toml"), "[[machine]\n").unwrap();
    let out = run(&["machine", "list"]);
    assert_eq!(out.status.code(), Some(1));
    let err: serde_json::Value = serde_json::from_slice(&out.stderr)
        .unwrap_or_else(|e| panic!("stderr is not one JSON value ({e})"));
    assert!(err["code"].is_string(), "{err}");
}

/// A `--local` machine with no herdr listening fails to connect with a message
/// naming its own `herdr.sock`, which `probe_fields` reads as `server down`
/// rather than the generic `unreachable` a network-level failure gets.
/// `XDG_CONFIG_HOME` points `local_socket_path` at a directory with nothing
/// listening, so the connect fails the same way it would on a machine that has
/// never run `herdr server`.
#[test]
fn machine_list_without_daemon_reads_a_local_machine_with_no_server_as_down() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    let xdg = tmp.path().join("xdg");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("flock.toml"),
        "[[machine]]\nname = \"loopback\"\nlocal = true\nsession = \"default\"\nmax_agents = 1\n",
    )
    .unwrap();

    let out = pastor()
        .args(["machine", "list"])
        .env("PASTOR_CONFIG_DIR", &config)
        .env("PASTOR_STATE_DIR", &state)
        .env("XDG_CONFIG_HOME", &xdg)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<Vec<&str>> = stdout
        .lines()
        .map(|l| l.split_whitespace().collect())
        .collect();
    assert_eq!(lines[1][..2], ["loopback", "local"], "{stdout}");
    // "server down" has a space, so it lands split across two columns.
    assert_eq!(lines[1][4..6], ["server", "down"], "{stdout}");

    let out = pastor()
        .args(["machine", "list", "--json"])
        .env("PASTOR_CONFIG_DIR", &config)
        .env("PASTOR_STATE_DIR", &state)
        .env("XDG_CONFIG_HOME", &xdg)
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let ms = v["machines"].as_array().unwrap();
    assert_eq!(ms.len(), 1, "{v}");
    assert_eq!(ms[0]["channel"], "server down", "{v}");
    let error = ms[0]["error"].as_str().expect("error string");
    assert!(error.contains("herdr.sock"), "{error}");
}

/// With no head, a `--local` machine that answers is the head's own host, so
/// its PASTOR is this binary's version. `XDG_CONFIG_HOME` points
/// `local_socket_path` at a fake herdr listening where herdr's default
/// session would.
#[test]
fn machine_list_without_daemon_shows_a_local_machine_s_pastor_version() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    let xdg = tmp.path().join("xdg");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(xdg.join("herdr")).unwrap();
    let socket = xdg.join("herdr").join("herdr.sock");
    struct Server(std::process::Child);
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _herdr = Server(
        Command::new(env!("CARGO_BIN_EXE_fake-herdr"))
            .arg("--listen")
            .arg(&socket)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + WAIT;
    while !socket.exists() {
        assert!(Instant::now() < deadline, "fake herdr never listened");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::fs::write(
        config.join("flock.toml"),
        "[[machine]]\nname = \"here\"\nlocal = true\nsession = \"default\"\nmax_agents = 1\n",
    )
    .unwrap();
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .env("XDG_CONFIG_HOME", &xdg)
            .output()
            .unwrap()
    };

    let out = run(&["machine", "list"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<Vec<&str>> = stdout
        .lines()
        .map(|l| l.split_whitespace().collect())
        .collect();
    assert_eq!(lines[0][5..7], ["HERDR", "PASTOR"], "{stdout}");
    assert_eq!(
        lines[1][..7],
        [
            "here",
            "local",
            "default",
            "-",
            "probed",
            "fake",
            env!("CARGO_PKG_VERSION")
        ],
        "{stdout}"
    );

    let out = run(&["machine", "list", "--json"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v["machines"][0]["pastor_version"],
        env!("CARGO_PKG_VERSION"),
        "{v}"
    );
}

/// A socket that accepts connections but does not answer `Ping` in time is
/// `Unresponsive`, the same state a head busy mid-dispatch is in. `machine
/// list` must not probe the machines around it (which would also print the
/// "not running" note for a head that is only busy): the command stops with
/// `head_unresponsive`. The fake daemon answers the ping with something other
/// than `Pong`, which reads as `Unresponsive` at once, with no wait for the
/// real 2s ping timeout.
#[test]
fn machine_list_treats_an_unresponsive_daemon_as_running_not_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        config.join("flock.toml"),
        "[[machine]]\nname = \"pi-3\"\nssh = \"fleet@pi-3\"\nmax_agents = 1\n",
    )
    .unwrap();

    let socket = state.join("pastor.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || {
        use std::io::Write;
        if let Ok((mut stream, _)) = listener.accept() {
            let reply =
                serde_json::to_string(&pastor::ipc::IpcResponse::error("boom", "nope")).unwrap();
            let _ = stream.write_all(reply.as_bytes());
            let _ = stream.write_all(b"\n");
        }
        if let Ok((stream, _)) = listener.accept() {
            drop(stream);
        }
    });

    let out = pastor()
        .args(["machine", "list"])
        .env("PASTOR_CONFIG_DIR", &config)
        .env("PASTOR_STATE_DIR", &state)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "an unresponsive daemon must not be probed around: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap_or_else(|e| {
        panic!(
            "stderr is not one JSON value ({e}): {}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    assert_eq!(err["code"], "head_unresponsive");
    let message = err["message"].as_str().unwrap();
    assert!(message.contains("not answering"), "{message}");
    assert!(
        !message.contains("not running"),
        "an unresponsive daemon must not be reported as absent: {message}"
    );
}
