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

/// `machine add --agent-key` writes the machine's marked line in the head's
/// authorized_keys, besides the user's own; the same key again, by `machine
/// authorized-key --write`, changes nothing; a new key replaces the line; a
/// dry run, a bad key and a machine already there change nothing.
#[test]
fn machine_add_writes_the_agent_key_line_once() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();
    let auth = tmp.path().join("authorized_keys");
    let theirs = "ssh-ed25519 AAAAmine me@laptop\n";
    std::fs::write(&auth, theirs).unwrap();
    let key = |name: &str, body: &str| {
        let f = tmp.path().join(name);
        std::fs::write(&f, format!("ssh-ed25519 {body} user@pi-9\n")).unwrap();
        f
    };
    let (one, two) = (
        key("one.pub", "AAAAC3NzaONE"),
        key("two.pub", "AAAAC3NzaTWO"),
    );
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .env("PASTOR_AUTHORIZED_KEYS", &auth)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    let text = |out: &std::process::Output| String::from_utf8_lossy(&out.stdout).to_string();
    let file = || std::fs::read_to_string(&auth).unwrap();
    let flock = || std::fs::read_to_string(config.join("flock.toml")).unwrap_or_default();
    let (one_s, two_s) = (one.to_str().unwrap(), two.to_str().unwrap());

    // A dry run says both and changes neither.
    let out = run(&[
        "machine",
        "add",
        "pi-9",
        "user@pi-9",
        "--agent-key",
        one_s,
        "--dry-run",
    ]);
    assert!(out.status.success(), "{out:?}");
    let said = text(&out);
    assert!(said.contains("would add machine pi-9"), "{said}");
    assert!(said.contains("pastor:agent:pi-9"), "{said}");
    assert!(said.contains("AAAAC3NzaONE"), "{said}");
    assert_eq!((file().as_str(), flock().contains("pi-9")), (theirs, false));

    // A bad key stops before the flock changes.
    let bad = key("bad.pub", "AAAA\"; rm -rf");
    let out = run(&[
        "machine",
        "add",
        "pi-9",
        "user@pi-9",
        "--agent-key",
        bad.to_str().unwrap(),
    ]);
    assert!(!out.status.success(), "{out:?}");
    assert_eq!((file().as_str(), flock().contains("pi-9")), (theirs, false));

    // The add writes the line, next to theirs.
    let out = run(&["machine", "add", "pi-9", "user@pi-9", "--agent-key", one_s]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        text(&out).contains("wrote pi-9's agent key line"),
        "{}",
        text(&out)
    );
    assert!(flock().contains("pi-9"));
    let exe = std::fs::canonicalize(env!("CARGO_BIN_EXE_pastor")).unwrap();
    let lines: Vec<String> = file().lines().map(str::to_string).collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0], theirs.trim());
    assert!(
        lines[1].starts_with(&format!(
            "restrict,command=\"{} bridge --agent --machine pi-9\",",
            exe.display()
        )) && lines[1].contains("AAAAC3NzaONE")
            && lines[1].ends_with("pastor:agent:pi-9"),
        "{}",
        lines[1]
    );

    // The same key again changes nothing, not even the bytes.
    let before = file();
    let out = run(&[
        "machine",
        "authorized-key",
        "pi-9",
        "--key",
        one_s,
        "--write",
    ]);
    assert!(out.status.success(), "{out:?}");
    assert!(text(&out).contains("already in"), "{}", text(&out));
    assert_eq!(file(), before);
    // A machine already in the flock is refused as before, the file untouched.
    let out = run(&["machine", "add", "pi-9", "user@pi-9", "--agent-key", one_s]);
    assert!(!out.status.success());
    assert_eq!(file(), before);

    // Another key replaces the line, and theirs stays.
    let out = run(&[
        "machine",
        "authorized-key",
        "pi-9",
        "--key",
        two_s,
        "--write",
    ]);
    assert!(out.status.success(), "{out:?}");
    let lines: Vec<String> = file().lines().map(str::to_string).collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[1].contains("AAAAC3NzaTWO") && !file().contains("AAAAC3NzaONE"));
    assert_eq!(lines[0], theirs.trim());

    // Without --write it only prints, as ever.
    let out = run(&["machine", "authorized-key", "pi-9", "--key", one_s]);
    assert!(text(&out).starts_with("command="), "{}", text(&out));
    assert!(file().contains("AAAAC3NzaTWO"));
}

/// Pastor's marked lines for one machine, a user's own line, another
/// machine's, and a `known_hosts` with the same names.
struct Keys {
    tmp: tempfile::TempDir,
    config: std::path::PathBuf,
    state: std::path::PathBuf,
    auth: std::path::PathBuf,
}

const THEIRS: &str = "ssh-ed25519 AAAAmine me@laptop";
const PI9_MACHINE: &str = "restrict,command=\"/p bridge --shepherd --machine pi-9\" ssh-ed25519 AAAAnine pastor:machine:pi-9";
const PI9_AGENT: &str = "restrict,command=\"/p bridge --agent --machine pi-9\" ssh-ed25519 AAAAagent9 pastor:agent:pi-9";
const PI8_AGENT: &str = "restrict,command=\"/p bridge --agent --machine pi-8\" ssh-ed25519 AAAAagent8 pastor:agent:pi-8";
const KNOWN: &str = "# pastor's own\npi-9 ssh-ed25519 AAAAhost9\n[pi-9]:2222 ssh-ed25519 AAAAhost9b\npi-8 ssh-ed25519 AAAAhost8\n";

fn keys() -> Keys {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        config.join("flock.toml"),
        "[[machine]]\nname = \"pi-9\"\nssh = \"user@pi-9\"\n[[machine]]\nname = \"pi-8\"\nssh = \"user@pi-8\"\n",
    )
    .unwrap();
    let auth = tmp.path().join("authorized_keys");
    std::fs::write(
        &auth,
        format!("{THEIRS}\n{PI9_MACHINE}\n{PI8_AGENT}\n{PI9_AGENT}\n"),
    )
    .unwrap();
    std::fs::write(state.join("known_hosts"), KNOWN).unwrap();
    let store = pastor::store::Store::open(&state.join("pastor.db")).unwrap();
    for name in ["pi-9", "pi-8"] {
        store
            .put_machine_state(&pastor::store::MachineState {
                machine: name.into(),
                bootstrap: pastor::store::BootstrapState::Failed,
                isolation: None,
                failed_step: None,
                why: None,
                host_key: None,
                herdr_version: None,
                pastor_version: None,
                generation: 1,
                updated_at: chrono::Utc::now(),
            })
            .unwrap();
    }
    Keys {
        tmp,
        config,
        state,
        auth,
    }
}

impl Keys {
    fn run(&self, args: &[&str]) -> std::process::Output {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &self.config)
            .env("PASTOR_STATE_DIR", &self.state)
            .env("PASTOR_AUTHORIZED_KEYS", &self.auth)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn auth(&self) -> String {
        std::fs::read_to_string(&self.auth).unwrap()
    }

    fn known(&self) -> String {
        std::fs::read_to_string(self.state.join("known_hosts")).unwrap()
    }

    fn has_state(&self, machine: &str) -> bool {
        pastor::store::Store::open(&self.state.join("pastor.db"))
            .unwrap()
            .machine_state(machine)
            .unwrap()
            .is_some()
    }
}

/// `machine remove` takes the machine's marked authorized_keys lines, its
/// known_hosts entries and its bootstrap row with its flock.toml entry, and
/// nothing of a user's or of another machine's. `--dry-run` lists them and
/// changes nothing.
#[test]
fn machine_remove_takes_the_machines_keys_and_state() {
    let k = keys();
    let flock = || std::fs::read_to_string(k.config.join("flock.toml")).unwrap();
    let before = (k.auth(), k.known(), flock());

    let out = k.run(&["machine", "remove", "pi-9", "--dry-run"]);
    assert!(out.status.success(), "{out:?}");
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(said.contains("would remove pi-9 from flock.toml"), "{said}");
    assert!(said.contains("would remove 2 lines"), "{said}");
    assert!(said.contains("would remove pi-9 from"), "{said}");
    assert!(
        said.contains("would remove pi-9's bootstrap state"),
        "{said}"
    );
    assert_eq!((k.auth(), k.known(), flock()), before);
    assert!(k.has_state("pi-9"));

    let out = k.run(&["machine", "remove", "pi-9"]);
    assert!(out.status.success(), "{out:?}");
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(said.contains("removed pi-9"), "{said}");
    assert!(said.contains("removed 2 lines"), "{said}");
    assert_eq!(k.auth(), format!("{THEIRS}\n{PI8_AGENT}\n"));
    assert_eq!(k.known(), "# pastor's own\npi-8 ssh-ed25519 AAAAhost8\n");
    assert!(!flock().contains("pi-9") && flock().contains("pi-8"));
    assert!(!k.has_state("pi-9"));
    assert!(k.has_state("pi-8"), "another machine's row stays");

    // A machine that is not there is refused, and nothing is touched.
    let out = k.run(&["machine", "remove", "pi-9", "--dry-run"]);
    assert!(!out.status.success());
    assert_eq!(k.auth(), format!("{THEIRS}\n{PI8_AGENT}\n"));
}

/// The same through a running head: it removes the machine's lines, host and
/// bootstrap row itself, on its own files, and `--dry-run` asks it for the
/// list without removing anything.
#[test]
fn machine_remove_through_the_head_cleans_up_too() {
    let socket_dir = tempfile::tempdir().unwrap();
    let socket = socket_dir.path().join("herdr.sock");
    let (_herdr, command) = fake_herdr_at(&socket, &[]);
    let env = check_env(&format!(
        "[[machine]]\nname = \"pi-9\"\nssh = \"user@pi-9\"\n\
         [[machine]]\nname = \"fake\"\ncommand = {command}\n"
    ));
    let auth = env.tmp.path().join("authorized_keys");
    std::fs::write(&auth, format!("{THEIRS}\n{PI9_AGENT}\n{PI8_AGENT}\n")).unwrap();
    std::fs::create_dir_all(&env.state).unwrap();
    std::fs::write(env.state.join("known_hosts"), KNOWN).unwrap();
    {
        let store = pastor::store::Store::open(&env.state.join("pastor.db")).unwrap();
        store
            .put_machine_state(&pastor::store::MachineState {
                machine: "pi-9".into(),
                bootstrap: pastor::store::BootstrapState::Failed,
                isolation: None,
                failed_step: None,
                why: None,
                host_key: None,
                herdr_version: None,
                pastor_version: None,
                generation: 1,
                updated_at: chrono::Utc::now(),
            })
            .unwrap();
    }
    let run = |args: &[&str]| {
        env.command()
            .args(args)
            .env("PASTOR_AUTHORIZED_KEYS", &auth)
            .output()
            .unwrap()
    };
    let log = std::fs::File::create(env.tmp.path().join("serve.log")).unwrap();
    let serve = env
        .command()
        .args(["serve", "--foreground"])
        .env("PASTOR_AUTHORIZED_KEYS", &auth)
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .unwrap();
    let _served = Served {
        child: serve,
        log: env.tmp.path().join("serve.log"),
    };
    let deadline = Instant::now() + WAIT;
    while !String::from_utf8_lossy(&run(&["machine", "list", "--json"]).stdout)
        .contains("\"connected\"")
    {
        assert!(Instant::now() < deadline, "the head never came up");
        std::thread::sleep(Duration::from_millis(100));
    }
    let known = || std::fs::read_to_string(env.state.join("known_hosts")).unwrap();

    let out = run(&["machine", "remove", "pi-9", "--dry-run"]);
    assert!(out.status.success(), "{out:?}");
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(said.contains("would remove pi-9 from flock.toml"), "{said}");
    assert!(said.contains("would remove 1 line "), "{said}");
    assert!(said.contains("bootstrap state"), "{said}");
    assert!(std::fs::read_to_string(&auth).unwrap().contains("pi-9"));
    assert_eq!(known(), KNOWN);

    let out = run(&["machine", "remove", "pi-9"]);
    assert!(out.status.success(), "{out:?}");
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(said.contains("removed 1 line "), "{said}");
    assert_eq!(
        std::fs::read_to_string(&auth).unwrap(),
        format!("{THEIRS}\n{PI8_AGENT}\n")
    );
    assert_eq!(known(), "# pastor's own\npi-8 ssh-ed25519 AAAAhost8\n");
    let store = pastor::store::Store::open(&env.state.join("pastor.db")).unwrap();
    assert!(store.machine_state("pi-9").unwrap().is_none());
}

/// `machine cordon` and `uncordon` through a running head: the list and the
/// description show the cordon with its reason, `task run --now` onto the
/// machine is refused with the code that says it is not a floor, and
/// uncordoning clears all three.
#[test]
fn machine_cordon_shows_in_list_and_describe_and_refuses_now() {
    let socket_dir = tempfile::tempdir().unwrap();
    let socket = socket_dir.path().join("herdr.sock");
    let (_herdr, command) = fake_herdr_at(&socket, &[]);
    let env = check_env(&format!(
        "[[machine]]\nname = \"fake\"\ncommand = {command}\n"
    ));
    let run = |args: &[&str]| env.command().args(args).output().unwrap();
    let log = std::fs::File::create(env.tmp.path().join("serve.log")).unwrap();
    let serve = env
        .command()
        .args(["serve", "--foreground"])
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .unwrap();
    let _served = Served {
        child: serve,
        log: env.tmp.path().join("serve.log"),
    };
    let deadline = Instant::now() + WAIT;
    while !String::from_utf8_lossy(&run(&["machine", "list", "--json"]).stdout)
        .contains("\"connected\"")
    {
        assert!(Instant::now() < deadline, "the head never came up");
        std::thread::sleep(Duration::from_millis(100));
    }

    let out = run(&["machine", "cordon", "nowhere"]);
    assert_eq!(error_code(&out), "unknown_machine");
    let out = run(&["machine", "cordon", "fake", "--reason", "kernel update"]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("fake is cordoned: kernel update"),
        "{out:?}"
    );
    let list = String::from_utf8_lossy(&run(&["machine", "list"]).stdout).to_string();
    assert!(list.contains("cordoned: kernel update"), "{list}");
    let describe =
        String::from_utf8_lossy(&run(&["machine", "describe", "fake"]).stdout).to_string();
    assert!(
        describe
            .lines()
            .any(|l| l.starts_with("cordoned:") && l.ends_with(": kernel update")),
        "{describe}"
    );
    let json: serde_json::Value =
        serde_json::from_slice(&run(&["machine", "list", "--json"]).stdout).unwrap();
    let fake_row = |json: &serde_json::Value| {
        json["machines"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == "fake")
            .unwrap()
            .clone()
    };
    assert_eq!(
        fake_row(&json)["cordon"]["reason"],
        "kernel update",
        "{json}"
    );

    let out = run(&[
        "task",
        "run",
        "go",
        "--machine",
        "fake",
        "--now",
        "--repo",
        "/tmp",
    ]);
    assert_eq!(error_code(&out), "machine_cordoned");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("room floor"),
        "{out:?}"
    );

    let out = run(&["machine", "uncordon", "fake"]);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("takes tasks again"),
        "{out:?}"
    );
    let list = String::from_utf8_lossy(&run(&["machine", "list"]).stdout).to_string();
    assert!(!list.contains("cordoned"), "{list}");
    let json: serde_json::Value =
        serde_json::from_slice(&run(&["machine", "list", "--json"]).stdout).unwrap();
    assert!(fake_row(&json).get("cordon").is_none(), "{json}");
}

/// `pastor leave` removes the lines pastor marked, whoever they name, and the
/// key files pastor keeps in its state directory, and nothing else: not the
/// user's lines, not a link or a directory at a key's name, not the database
/// or the config.
#[test]
fn leave_takes_only_what_pastor_put_there() {
    let k = keys();
    for name in ["head_key", "head_key.pub", "agent_key", "bootstrap.json"] {
        std::fs::write(k.state.join(name), "key\n").unwrap();
    }
    // A link at machine_key's name is not a file pastor made.
    let elsewhere = k.tmp.path().join("elsewhere");
    std::fs::write(&elsewhere, "mine\n").unwrap();
    std::os::unix::fs::symlink(&elsewhere, k.state.join("machine_key")).unwrap();
    let flock = k.config.join("flock.toml");

    let out = k.run(&["leave", "--dry-run"]);
    assert!(out.status.success(), "{out:?}");
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        said.contains("would remove 3 lines pastor marked"),
        "{said}"
    );
    assert!(said.contains("head_key"), "{said}");
    assert!(
        said.contains("left ") && said.contains("machine_key"),
        "{said}"
    );
    assert!(k.state.join("head_key").exists() && k.auth().contains("pastor:agent:pi-9"));

    let out = k.run(&["leave"]);
    assert!(out.status.success(), "{out:?}");
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(said.contains("removed 3 lines pastor marked"), "{said}");
    assert_eq!(k.auth(), format!("{THEIRS}\n"));
    for gone in [
        "head_key",
        "head_key.pub",
        "agent_key",
        "known_hosts",
        "bootstrap.json",
    ] {
        assert!(!k.state.join(gone).exists(), "{gone}");
    }
    assert!(std::fs::symlink_metadata(k.state.join("machine_key")).is_ok());
    assert_eq!(std::fs::read_to_string(&elsewhere).unwrap(), "mine\n");
    assert!(k.state.join("pastor.db").exists() && flock.exists());

    // A second time there is nothing.
    let out = k.run(&["leave"]);
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(said.contains("left ") || said.contains("nothing"), "{said}");
    assert_eq!(k.auth(), format!("{THEIRS}\n"));
}

/// `machine authorized-key --shepherd` locks a pull machine's own key to
/// `bridge --shepherd --machine <name> --gen <n>`, `n` its enrollment: 0 in a
/// new `machine_state` row when it had none, else the row's. It sits on its
/// own marked line beside the agent key's; a push machine, which runs no
/// shepherd, is refused, and a dry run writes nothing.
#[test]
fn authorized_key_shepherd_writes_the_locked_line() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        config.join("flock.toml"),
        "[[machine]]\nname = \"laptop\"\npull = true\n\n[[machine]]\nname = \"pi-1\"\nssh = \"user@pi-1\"\n",
    )
    .unwrap();
    let auth = tmp.path().join("authorized_keys");
    let key = tmp.path().join("id.pub");
    std::fs::write(&key, "ssh-ed25519 AAAAC3NzaPULL user@laptop\n").unwrap();
    let key = key.to_str().unwrap();
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .env("PASTOR_AUTHORIZED_KEYS", &auth)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    let file = || std::fs::read_to_string(&auth).unwrap_or_default();
    let shepherd = |extra: &[&str]| {
        let mut args = vec![
            "machine",
            "authorized-key",
            "laptop",
            "--key",
            key,
            "--shepherd",
        ];
        args.extend_from_slice(extra);
        run(&args)
    };
    let exe = std::fs::canonicalize(env!("CARGO_BIN_EXE_pastor")).unwrap();
    let command = |generation: u32| {
        format!(
            "restrict,command=\"{} bridge --shepherd --machine laptop --gen {generation}\",",
            exe.display()
        )
    };

    let out = run(&[
        "machine",
        "authorized-key",
        "pi-1",
        "--key",
        key,
        "--shepherd",
    ]);
    assert!(!out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not_pull_machine"),
        "{out:?}"
    );

    let out = shepherd(&["--write", "--dry-run"]);
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains(&command(0)));
    assert_eq!(file(), "");

    let out = shepherd(&["--write"]);
    assert!(out.status.success(), "{out:?}");
    let lines: Vec<String> = file().lines().map(str::to_string).collect();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].starts_with(&command(0))
            && lines[0].contains("AAAAC3NzaPULL")
            && lines[0].ends_with("pastor:machine:laptop"),
        "{}",
        lines[0]
    );

    // The agent key's line is another line.
    let out = run(&[
        "machine",
        "authorized-key",
        "laptop",
        "--key",
        key,
        "--write",
    ]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(file().lines().count(), 2, "{}", file());

    // A new enrollment moves the line on to it.
    let store = pastor::store::Store::open(
        &pastor::config::Paths::new(config.clone(), state.clone()).db_file(),
    )
    .unwrap();
    let mut row = store.machine_state("laptop").unwrap().expect("a row");
    assert_eq!(row.generation, 0);
    // Ok, as no row read: a pending one would keep its tasks from it.
    assert_eq!(row.bootstrap, pastor::store::BootstrapState::Ok);
    row.generation = 3;
    store.put_machine_state(&row).unwrap();
    let out = shepherd(&["--write"]);
    assert!(out.status.success(), "{out:?}");
    let text = file();
    assert_eq!(text.lines().count(), 2, "{text}");
    assert!(
        text.contains(&command(3)) && !text.contains(&command(0)),
        "{text}"
    );

    // Without --write it only prints.
    let out = shepherd(&[]);
    assert!(String::from_utf8_lossy(&out.stdout).starts_with(&command(3)));
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
            "NAME", "HOST", "FLOCKS", "PROFILE", "CHANNEL", "HERDR", "PASTOR", "HARNESS", "AGENTS",
            "ORPHANS", "TAGS", "ERROR"
        ],
        "{stdout}"
    );
    assert_eq!(
        lines[1][..5],
        ["fake", "fake-herdr", "default", "-", "connected"],
        "{stdout}"
    );
    // A command bridge cannot say which pastor is behind it.
    assert_eq!(lines[1][6..9], ["-", "-", "0/2+1j+1b"], "{stdout}");
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

/// A flock's pins reach the actor: `machine list` has a HARNESS cell for a
/// machine with pins, and `--json` carries each pinned kind with the pin and
/// what the machine runs. A command bridge cannot say what runs behind it, so
/// nothing drifts. A change of pins in flock.toml reaches a running head
/// (a bad pin is refused at load: `pins_are_read_checked_and_overridden_by_the_machine`).
#[test]
fn machine_list_shows_the_harness_pins() {
    let env = start();
    let cell = |env: &Env| -> Vec<String> {
        let out = env.cmd(&["machine", "list"]);
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let lines: Vec<&str> = stdout.lines().skip(2).collect();
        let at = lines[0].find("HARNESS").unwrap();
        lines[1][at..]
            .split_whitespace()
            .take(1)
            .map(str::to_string)
            .collect()
    };
    assert_eq!(cell(&env), ["-"]);
    let flock = env.config.join("flock.toml");
    let text = std::fs::read_to_string(&flock).unwrap();
    std::fs::write(
        &flock,
        text.replace(
            "max_agents = 2",
            "max_agents = 2\npins = { claude = \"2.1.310\", agy = \"1.4\" }",
        ),
    )
    .unwrap();
    assert!(env.cmd(&["job", "reload"]).status.success());
    env.wait_for("the pins reach the machine", &["machine", "list"], |text| {
        let lines: Vec<&str> = text.lines().skip(2).collect();
        lines.len() > 1
            && lines[1]
                .get(lines[0].find("HARNESS").unwrap_or(0)..)
                .is_some_and(|rest| rest.starts_with("ok"))
    });
    let v = env.json(&["machine", "list", "--json"]);
    let harness = &v["machines"][0]["harness"];
    assert_eq!(harness[0]["kind"], "agy", "{v}");
    assert_eq!(harness[0]["pin"], "1.4");
    assert_eq!(harness[0]["version"], serde_json::Value::Null);
    assert_eq!(harness[1]["kind"], "claude");
    assert_eq!(harness[1]["drift"], false);
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
    assert_eq!(
        &lines[1][6..],
        ["-", "-", "0/2+1j+1b", "-", "arm"],
        "{stdout}"
    );
    assert_eq!(
        lines[2][..5],
        ["gone", "no-such-bridge", "default", "-", "unreachable"],
        "{stdout}"
    );
    assert_eq!(lines[2][5..9], ["-", "-", "-", "-/1+1j+1b"], "{stdout}");
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

/// A machine `here` on this host, with a copied provisioner `mine` for this
/// OS: one check that passes, one that fails and one that cannot run, and
/// `erring` whose one check cannot run. `HOME` and
/// `XDG_CONFIG_HOME` are the test's own, so the run directory lands there
/// and no real herdr on this host is reached.
struct CheckEnv {
    tmp: tempfile::TempDir,
    config: std::path::PathBuf,
    state: std::path::PathBuf,
}

/// A `gpg` that signs and verifies anything, for git to run: it prints the
/// status lines git reads for a signature made and for a good one. It reads
/// all of its stdin first: git writes the signed data to it, and a `gpg` that
/// exits before that is written kills git with SIGPIPE, silently.
const STAND_IN_GPG: &str = r#"#!/bin/sh
case " $* " in
*" --verify "*)
  cat >/dev/null
  echo "[GNUPG:] NEWSIG"
  echo "[GNUPG:] GOODSIG ABCDEF0123456789 Check <check@example.com>"
  echo "[GNUPG:] VALIDSIG ABCDEF0123456789 2026-01-01 1 0 4 0 1 10 00 ABCDEF0123456789"
  echo "[GNUPG:] TRUST_ULTIMATE 0 pgp"
  ;;
*)
  cat >/dev/null
  echo "[GNUPG:] SIG_CREATED D 1 10 00 1 ABCDEF0123456789" >&2
  printf -- '-----BEGIN PGP SIGNATURE-----\n\nZmFrZQ==\n-----END PGP SIGNATURE-----\n'
  ;;
esac
"#;

fn check_env(extra_machines: &str) -> CheckEnv {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(tmp.path().join("h")).unwrap();
    std::fs::write(
        config.join("pastor.toml"),
        "tick = \"1s\"\nsettle = \"1s\"\nreconcile_every = \"1s\"\n",
    )
    .unwrap();
    std::fs::write(
        config.join("flock.toml"),
        format!(
            "[[machine]]\nname = \"here\"\nlocal = true\nprovisioner = \"mine\"\n{extra_machines}"
        ),
    )
    .unwrap();
    // Git there signs with a stand-in `gpg`, so the signing row passes.
    let gpg = tmp.path().join("gpg");
    std::fs::write(&gpg, STAND_IN_GPG).unwrap();
    std::fs::set_permissions(&gpg, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        tmp.path().join("h/.gitconfig"),
        format!(
            "[user]\n\tname = Check\n\temail = check@example.com\n\tsigningkey = ABCDEF\n\
             [gpg]\n\tprogram = {}\n",
            gpg.display()
        ),
    )
    .unwrap();
    // `uname -s` says darwin where Rust says macos.
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        os => os,
    };
    let erring =
        "[[check]]\nid = \"erring\"\ncommand = [\"sh\", \"-c\", \"echo no answer; exit 2\"]\n";
    for (id, checks) in [
        (
            "mine",
            format!(
                "[[check]]\nid = \"fine\"\ncommand = [\"sh\", \"-c\", \"echo all good\"]\n\
                 [[check]]\nid = \"broken\"\ncommand = [\"sh\", \"-c\", \"echo it is missing; exit 1\"]\n\
                 {erring}"
            ),
        ),
        ("erring", erring.to_string()),
    ] {
        let prov = tmp.path().join("d/provisioners").join(id);
        std::fs::create_dir_all(&prov).unwrap();
        std::fs::write(
            prov.join("pastor-provisioner.toml"),
            format!("id = \"{id}\"\nversion = \"0.1.0\"\n[platform]\nos = \"{os}\"\n{checks}"),
        )
        .unwrap();
    }
    CheckEnv { tmp, config, state }
}

impl CheckEnv {
    fn command(&self) -> Command {
        let mut c = pastor();
        c.env("PASTOR_CONFIG_DIR", &self.config)
            .env("PASTOR_STATE_DIR", &self.state)
            .env("PASTOR_DATA_DIR", self.tmp.path().join("d"))
            .env("HOME", self.tmp.path().join("h"))
            .env("XDG_CONFIG_HOME", self.tmp.path().join("x"));
        c
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        self.command().args(args).output().unwrap()
    }
}

/// With no head, `machine check` runs here: one row per check, exit 1
/// because one failed or could not run, and `--json` is the same report.
/// An agent pastor started may run it: it changes nothing.
#[test]
fn machine_check_without_a_head_runs_here() {
    let env = check_env("");
    let out = env.run(&["machine", "check", "here"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    // The `path` row follows the provisioner's rows; what it holds is this
    // host's PATH, which is not what this test is about.
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let (rows, path): (Vec<&str>, Vec<&str>) = text.lines().partition(|l| !l.contains(" path  "));
    // The inbox row is skipped: no herdr runs here to start a session in.
    let (rows, inbox): (Vec<&str>, Vec<&str>) =
        rows.into_iter().partition(|l| !l.contains(" inbox ("));
    assert_eq!(
        rows.join("\n"),
        "here: provisioner mine 0.1.0\nok     fine  all good\nfail   broken  it is missing\n\
         error  erring  no answer: exit 2\nok     signing  a commit here signs and verifies"
    );
    assert_eq!(inbox.len(), 1, "{text}");
    assert!(
        inbox[0].starts_with("skip   inbox (claude)  no session to test"),
        "{text}"
    );
    assert_eq!(path.len(), 1, "{text}");
    assert!(path[0].contains("PATH "), "{text}");
    let out = env.run(&["machine", "check", "here", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["provisioner"], "mine");
    assert_eq!(report["rows"][1]["id"], "broken");
    assert_eq!(report["rows"][1]["status"], "fail");
    assert_eq!(report["rows"][2]["status"], "error");
    // A check that cannot run is as much a reason to exit 1 as a failed one.
    let out = env.run(&["machine", "check", "here", "--provisioner", "erring"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    // The run directory is gone again, and its empty parents.
    assert!(!env.tmp.path().join("h/.cache/pastor").exists());

    let out = env
        .command()
        .args(["machine", "check", "here"])
        .env("PASTOR_TASK", "t-1")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("fail   broken"));

    let out = env.run(&["machine", "check", "here", "--provisioner", "nope"]);
    assert_eq!(error_code(&out), "unknown_provisioner");
    let out = env.run(&["machine", "check", "nowhere"]);
    assert_eq!(error_code(&out), "unknown_machine");
}

/// A secret-looking value in an `env` of `[defaults]`, a flock or a machine
/// is refused at load, naming the layer and the key and never the value; the same key in an agent's own `env` is not refused.
#[test]
fn a_secret_in_a_shared_env_is_refused_at_load() {
    let secret = format!("gh{}_{}", "p", "aB3dE6gH9jK2mN5pQ8sT1vW4yZ7cF0hJ3kL6");
    for (file, text, says) in [
        (
            "pastor.toml",
            format!("[defaults]\nenv = {{ GH = \"{secret}\" }}\n"),
            "defaults: env: GH",
        ),
        (
            "flock.toml",
            format!(
                "[[flock]]\nname = \"w\"\ndefault = true\nenv = {{ GH = \"{secret}\" }}\n\
                 [[machine]]\nname = \"here\"\nlocal = true\n"
            ),
            "flock w: env: GH",
        ),
        (
            "flock.toml",
            format!("[[machine]]\nname = \"here\"\nlocal = true\nenv = {{ GH = \"{secret}\" }}\n"),
            "machine here: env: GH",
        ),
    ] {
        let env = check_env("");
        std::fs::write(env.config.join(file), text).unwrap();
        let out = env.run(&["machine", "list"]);
        assert_eq!(out.status.code(), Some(1), "{file}: {out:?}");
        let said = String::from_utf8_lossy(&out.stderr);
        assert!(said.contains(says), "{file}: {said}");
        assert!(!said.contains(&secret), "{file}: the value is never said");
    }
    // The agent's own env is the place for one, and loads.
    let env = check_env("");
    std::fs::write(
        env.config.join("pastor.toml"),
        format!("[agents.claude]\nenv = {{ GH = \"{secret}\" }}\n"),
    )
    .unwrap();
    let out = env.run(&["machine", "list"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
}

/// `machine install` of a machine whose agents run no pinned harness fails
/// with `nothing_pinned` before it reaches the machine; an unknown machine
/// is `unknown_machine`.
#[test]
fn machine_install_with_nothing_pinned_says_so() {
    let env = check_env("");
    let out = env.run(&["machine", "install", "here", "--dry-run"]);
    assert_eq!(error_code(&out), "nothing_pinned", "{out:?}");
    let out = env.run(&["machine", "install", "here", "--kind", "claude"]);
    assert_eq!(error_code(&out), "nothing_pinned", "{out:?}");
    let out = env.run(&["machine", "install", "nowhere"]);
    assert_eq!(error_code(&out), "unknown_machine");
    assert!(!env.tmp.path().join("h/.local").exists());
}

/// herdr is installed to `[artifacts] herdr` and to nothing else: `--kind
/// herdr` with no pin is `nothing_pinned`; with a pin, a bare `machine
/// install` and `--kind herdr` plan it, while `--kind claude` and `--agent`
/// are harnesses and so still find nothing pinned. A dry run changes nothing.
#[test]
fn machine_install_takes_herdr_when_pastor_toml_pins_it() {
    let env = check_env("");
    let out = env.run(&["machine", "install", "here", "--kind", "herdr", "--dry-run"]);
    assert_eq!(error_code(&out), "nothing_pinned", "{out:?}");

    let toml = env.config.join("pastor.toml");
    let mut text = std::fs::read_to_string(&toml).unwrap();
    text.push_str("[artifacts]\nherdr = \"0.0.1\"\n");
    std::fs::write(&toml, text).unwrap();
    for args in [
        &["machine", "install", "here", "--dry-run", "--json"][..],
        &[
            "machine",
            "install",
            "here",
            "--kind",
            "herdr",
            "--dry-run",
            "--json",
        ][..],
    ] {
        // No herdr of the host's on the PATH: the plan is for a machine
        // that has none.
        let out = env
            .command()
            .args(args)
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "{args:?}: {out:?}");
        let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(report["dry_run"], true);
        assert_eq!(report["rows"].as_array().unwrap().len(), 1, "{report}");
        assert_eq!(report["rows"][0]["kind"], "herdr");
        assert_eq!(report["rows"][0]["pin"], "0.0.1");
        assert_eq!(report["rows"][0]["action"], "installed");
        assert_eq!(report["rows"][0]["source"], "release");
    }
    for args in [
        [
            "machine",
            "install",
            "here",
            "--kind",
            "claude",
            "--dry-run",
        ],
        [
            "machine",
            "install",
            "here",
            "--agent",
            "claude",
            "--dry-run",
        ],
    ] {
        let out = env.run(&args);
        assert_eq!(error_code(&out), "nothing_pinned", "{args:?}: {out:?}");
    }
    assert!(!env.tmp.path().join("h/.local").exists());
    assert!(!env.state.join("artifacts").exists());
}

/// With a head running, the head runs the check and raises
/// `machine.check_failed` for each row that failed or could not run, and
/// none for the one that passed; a machine it runs no shell on is refused.
#[test]
fn machine_check_through_the_head_raises_an_event_per_failed_row() {
    let socket_dir = tempfile::tempdir().unwrap();
    let socket = socket_dir.path().join("herdr.sock");
    let (_herdr, command) = fake_herdr_at(&socket, &[]);
    let env = check_env(&format!(
        "[[machine]]\nname = \"fake\"\ncommand = {command}\n"
    ));
    let log = std::fs::File::create(env.tmp.path().join("serve.log")).unwrap();
    let serve = env
        .command()
        .args(["serve", "--foreground"])
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .unwrap();
    let _served = Served {
        child: serve,
        log: env.tmp.path().join("serve.log"),
    };
    let deadline = Instant::now() + WAIT;
    while !String::from_utf8_lossy(&env.run(&["machine", "list", "--json"]).stdout)
        .contains("\"connected\"")
    {
        assert!(Instant::now() < deadline, "the head never came up");
        std::thread::sleep(Duration::from_millis(100));
    }

    let out = env.run(&["machine", "check", "here", "--json"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["rows"][1]["status"], "fail");

    let deadline = Instant::now() + WAIT;
    let events = loop {
        let out = env.run(&["events", "--json"]);
        let found: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|e| e["type"] == "machine.check_failed")
            .collect();
        if found.len() >= 2 {
            break found;
        }
        assert!(Instant::now() < deadline, "events so far: {found:?}");
        std::thread::sleep(Duration::from_millis(100));
    };
    let got: Vec<(&str, &str)> = events
        .iter()
        .map(|e| {
            assert_eq!(e["detail"]["provisioner"], "mine");
            (
                e["detail"]["check"].as_str().unwrap(),
                e["detail"]["status"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(got, [("broken", "fail"), ("erring", "error")]);

    let out = env.run(&["machine", "check", "fake"]);
    assert_eq!(error_code(&out), "machine_check_unsupported");
}

/// `machine check` says when the herdr server running on the machine is not
/// the version installed there, by both of its paths: with no head, the CLI
/// pings the machine; with a head running, the head does. A stand-in `herdr`
/// first on the PATH says `0.9.4`; the fake server answers its ping as `fake`.
#[test]
fn machine_check_says_when_the_running_herdr_is_not_the_installed_one() {
    use std::os::unix::fs::PermissionsExt;
    let env = check_env("");
    let socket = env.tmp.path().join("x/herdr/herdr.sock");
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let (_herdr, _) = fake_herdr_at(&socket, &[]);
    let bin = env.tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let standin = bin.join("herdr");
    std::fs::write(&standin, "#!/bin/sh\necho 'herdr 0.9.4'\n").unwrap();
    std::fs::set_permissions(&standin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let last = std::cell::RefCell::new(String::new());
    let running_row = || -> Option<String> {
        let out = env
            .command()
            .args(["machine", "check", "here", "--json"])
            .env("PATH", &path)
            .output()
            .unwrap();
        *last.borrow_mut() = format!("{out:?}");
        let report: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
        report["rows"]
            .as_array()?
            .iter()
            .find(|r| r["id"] == "herdr-running")
            .map(|r| r["first_line"].as_str().unwrap().to_string())
    };
    let wait_for_row = |how: &str| {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(line) = running_row() {
                return line;
            }
            assert!(
                Instant::now() < deadline,
                "no herdr-running row {how}: {}",
                last.borrow()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    let says = "herdr 0.9.4 is installed, herdr fake is running";
    assert_eq!(wait_for_row("without a head"), says);

    let log = std::fs::File::create(env.tmp.path().join("serve.log")).unwrap();
    let serve = env
        .command()
        .args(["serve", "--foreground"])
        .env("PATH", &path)
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .unwrap();
    let _served = Served {
        child: serve,
        log: env.tmp.path().join("serve.log"),
    };
    let deadline = Instant::now() + WAIT;
    while !String::from_utf8_lossy(&env.run(&["machine", "list", "--json"]).stdout)
        .contains("\"connected\"")
    {
        assert!(Instant::now() < deadline, "the head never came up");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(wait_for_row("through the head"), says);
}

/// A flock.toml from before the naming rule loads, and the command says so
/// once, however many times it reads the file; `machine add` refuses a new
/// name that breaks the rule, naming the code.
#[test]
fn an_old_machine_name_is_warned_about_once_and_a_new_one_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("flock.toml"),
        "[[machine]]\nname = \"-old\"\nssh = \"user@old\"\n[[machine]]\nname = \"pi-1\"\nssh = \"user@pi-1\"\n",
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
    for args in [&["machine", "list"][..], &["flock", "list"], &["status"]] {
        let out = run(args);
        let err = String::from_utf8_lossy(&out.stderr);
        let said: Vec<&str> = err
            .lines()
            .filter(|l| l.contains("breaks the naming rule"))
            .collect();
        assert_eq!(said.len(), 1, "{args:?}: {err}");
        assert!(said[0].contains("\"-old\""), "{}", said[0]);
        assert!(!err.contains("pi-1\" breaks"), "{err}");
    }
    let out = run(&["machine", "add", "pi;id", "user@new"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("invalid_machine"), "{err}");
    let text = std::fs::read_to_string(config.join("flock.toml")).unwrap();
    assert!(!text.contains("pi;id"), "{text}");
}

/// `machine bootstrap` and `machine add --bootstrap` run on a head: without
/// one they say so, and with one that has no `head_address` they stop before
/// changing anything; `--bootstrap` needs an ssh target and `--host-key`
/// needs `--bootstrap`.
#[test]
fn bootstrapping_from_the_head_needs_a_head_and_its_address() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    std::fs::create_dir_all(&config).unwrap();
    let off = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", tmp.path().join("s"))
            .output()
            .unwrap()
    };
    assert_eq!(
        error_code(&off(&["machine", "bootstrap", "pi-9"])),
        "no_head"
    );
    assert_eq!(
        error_code(&off(&["machine", "add", "pi-9", "u@pi-9", "--bootstrap"])),
        "no_head"
    );
    assert!(!config.join("flock.toml").exists(), "nothing was added");
    assert_eq!(
        error_code(&off(&["machine", "add", "pi-9", "--local", "--bootstrap"])),
        "usage_error"
    );
    for args in [
        &["machine", "add", "pi-9", "u@pi-9", "--host-key", "SHA256:x"][..],
        &[
            "machine",
            "add",
            "pi-9",
            "u@pi-9",
            "--bootstrap",
            "--dry-run",
        ][..],
    ] {
        assert_eq!(off(args).status.code(), Some(2), "{args:?}");
    }

    // A running head with no head_address: stopped, with nothing added.
    let env = start();
    assert_eq!(
        error_code(&env.cmd(&["machine", "add", "pi-9", "u@pi-9", "--bootstrap"])),
        "no_head_address"
    );
    assert!(
        !String::from_utf8_lossy(&env.cmd(&["machine", "list"]).stdout).contains("pi-9"),
        "not added"
    );
    assert_eq!(
        error_code(&env.cmd(&["machine", "bootstrap", "ghost"])),
        "no_head_address"
    );
}
