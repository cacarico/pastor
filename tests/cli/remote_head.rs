use crate::helpers::*;

/// With a head set, `task run`, `task list`, `task show` and `machine list`
/// go to it over ssh and `pastor bridge`, and print and exit as they do on
/// the head's own machine. Nothing reads or writes the CLI's own files but
/// client.toml.
#[test]
fn a_remote_head_answers_what_the_local_one_would() {
    let env = start();
    let c = client(Some(&env));
    let out = ok(c.head_set("head-up", &[]));
    assert!(out.starts_with("head: head-up (remote, pastor "), "{out}");
    assert_eq!(ok(c.cmd(&["head", "show"])), "head: head-up (remote)\n");
    let v: serde_json::Value =
        serde_json::from_str(&ok(c.cmd(&["head", "show", "--json"]))).unwrap();
    assert_eq!(v["remote"], true);
    assert_eq!(v["ssh"], "head-up");
    assert_eq!(v["from"], "file");

    let t: serde_json::Value = serde_json::from_str(&ok(
        c.cmd(&["task", "run", "hello", "--repo", "/tmp", "--json"])
    ))
    .unwrap();
    assert_eq!(t["machine"], "fake");
    assert_eq!(t["agent_name"], "t-1");
    env.wait_done("t-1");

    for args in [
        &["task", "list", "--all", "--json"][..],
        &["task", "describe", "t-1", "--json"],
        &["task", "list"],
    ] {
        let (remote, local) = (c.cmd(args), env.cmd(args));
        assert_eq!(remote.status.code(), local.status.code(), "{args:?}");
        assert_eq!(
            String::from_utf8_lossy(&remote.stdout),
            String::from_utf8_lossy(&local.stdout),
            "{args:?}"
        );
    }
    let (remote, local) = (
        c.cmd(&["task", "describe", "t-9"]),
        env.cmd(&["task", "describe", "t-9"]),
    );
    assert_eq!(error_code(&remote), "task_not_found");
    assert_eq!(remote.stderr, local.stderr);

    // The machines are the head's; the head's line names where it is.
    let remote = ok(c.cmd(&["machine", "list"]));
    let local = ok(env.cmd(&["machine", "list"]));
    assert!(
        remote.starts_with(&format!(
            "pastor {} on head-up (herdr -), 1 machine",
            env!("CARGO_PKG_VERSION")
        )),
        "{remote}"
    );
    let table = |s: &str| s.lines().skip(1).collect::<Vec<_>>().join("\n");
    assert_eq!(table(&remote), table(&local));
    let v: serde_json::Value =
        serde_json::from_str(&ok(c.cmd(&["machine", "list", "--json"]))).unwrap();
    assert_eq!(v["head"]["host"], "head-up");
    assert_eq!(v["machines"][0]["name"], "fake");

    // `--head` and PASTOR_HEAD name a head for one command, over the file.
    let out = c.cmd(&["--head", "unreachable", "task", "list"]);
    assert_eq!(error_code(&out), "head_unreachable");

    let names: Vec<String> = std::fs::read_dir(&c.config)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["client.toml"], "nothing else written here");
    assert!(!c.state.join("pastor.db").exists());
}

/// With a head set, `pastor events` pages through the head's log with
/// `EventsSince` and prints what the head's own `pastor events` prints, with
/// `--json` and `--task` too; `--follow` asks again every second and prints
/// what the head logs after it started.
#[test]
fn events_from_a_remote_head_match_the_local_log() {
    use std::io::BufRead;
    let env = start();
    let c = client(Some(&env));
    ok(c.head_set("head-up", &[]));
    ok(c.cmd(&["task", "run", "one", "--repo", "/tmp"]));
    env.wait_done("t-1");

    for args in [
        &["events"][..],
        &["events", "--json"],
        &["events", "--task", "t-1"],
        &["events", "--task", "t-9", "--json"],
    ] {
        // The head may log a machine event between the two reads; compare
        // the lines both saw.
        let local = ok(env.cmd(args));
        let remote = ok(c.cmd(args));
        assert!(!remote.is_empty() || args.contains(&"t-9"), "{args:?}");
        assert!(
            remote.starts_with(&local) || local.starts_with(&remote),
            "{args:?}\nremote:\n{remote}\nlocal:\n{local}"
        );
    }
    assert!(ok(c.cmd(&["events", "--task", "t-1"])).contains("task.done"));
    assert_eq!(
        error_code(&c.cmd(&["events", "--task", "x"])),
        error_code(&env.cmd(&["events", "--task", "x"]))
    );
    assert!(!c.state.join("events.jsonl").exists());

    let mut follow = pastor()
        .args(["events", "--follow", "--task", "t-2", "--json"])
        .env("PASTOR_CONFIG_DIR", &c.config)
        .env("PASTOR_STATE_DIR", &c.state)
        .env("PATH", &c.path)
        .env_remove("PASTOR_HEAD")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = std::io::BufReader::new(follow.stdout.take().unwrap()).lines();
    ok(c.cmd(&["task", "run", "two", "--repo", "/tmp"]));
    let mut kinds = vec![];
    while !kinds.iter().any(|k| k == "task.done") {
        let line = lines.next().expect("follow ended").unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["task"]["id"], 2, "{v}");
        kinds.push(v["type"].as_str().unwrap().to_string());
    }
    follow.kill().unwrap();
    follow.wait().unwrap();
    assert_eq!(kinds[0], "task.queued", "{kinds:?}");
    let local: Vec<String> = ok(env.cmd(&["events", "--task", "t-2", "--json"]))
        .lines()
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(kinds, local[..kinds.len()]);
}

/// The flock, machine, trust, profile and config commands give the same
/// stdout, stderr and exit code through a remote head as on the head's own
/// machine. Two heads start alike; each step runs on one directly and on the
/// other through the fake `ssh`, and the files they leave are the same. The
/// client's own config dir holds nothing but client.toml.
#[test]
fn a_remote_head_runs_fleet_commands_as_the_local_one() {
    use std::os::unix::fs::PermissionsExt;
    let here = start();
    let there = start();
    let c = client(Some(&there));
    ok(c.head_set("head-up", &[]));
    // An editor that changes nothing, and one that adds a comment.
    let bin = c.tmp.path().join("editors");
    std::fs::create_dir_all(&bin).unwrap();
    let append = bin.join("append");
    std::fs::write(&append, "#!/bin/sh\necho '# edited' >> \"$1\"\n").unwrap();
    std::fs::set_permissions(&append, std::fs::Permissions::from_mode(0o755)).unwrap();
    let append = append.to_str().unwrap().to_string();
    let steps: &[(&[&str], &str)] = &[
        (&["flock", "list"], "true"),
        (&["flock", "list", "--json"], "true"),
        (&["flock", "default", "show"], "true"),
        (
            &["flock", "add", "work", "--description", "work boxes"],
            "true",
        ),
        (&["flock", "add", "work"], "true"),
        (&["flock", "default", "set", "work"], "true"),
        (&["flock", "default", "show"], "true"),
        (&["flock", "describe", "work"], "true"),
        (&["flock", "describe", "work", "--json"], "true"),
        (&["flock", "describe", "nope"], "true"),
        (&["machine", "add", "pi-1", "--command", "false"], "true"),
        (&["machine", "move", "pi-1", "work"], "true"),
        (&["machine", "move", "nope", "work"], "true"),
        (&["machine", "describe", "nope"], "true"),
        (&["machine", "open", "fake"], "true"),
        (&["machine", "open", "nope"], "true"),
        (&["task", "attach", "t-9"], "true"),
        (&["trust", "add", "fake", "/repo"], "true"),
        (&["trust", "add", "fake", "/repo"], "true"),
        (&["trust", "remove", "fake", "/repo"], "true"),
        (&["trust", "remove", "fake", "/repo"], "true"),
        (&["trust", "list"], "true"),
        (&["trust", "list", "--json"], "true"),
        (&["profile", "list"], "true"),
        (&["profile", "list", "--json"], "true"),
        (&["profile", "describe", "develop"], "true"),
        (&["profile", "describe", "nope"], "true"),
        (&["machine", "remove", "pi-1"], "true"),
        (&["machine", "remove", "pi-1"], "true"),
        (&["flock", "remove", "work"], "true"),
        (&["flock", "edit"], "true"),
        (&["config", "edit"], "true"),
        (&["config", "edit"], &append),
        (&["profile", "list"], "true"),
    ];
    let root = |e: &Env| e._tmp.path().to_str().unwrap().to_string();
    let norm = |bytes: &[u8]| {
        String::from_utf8_lossy(bytes)
            .replace(&root(&here), "<tmp>")
            .replace(&root(&there), "<tmp>")
    };
    for (args, editor) in steps {
        let local = pastor()
            .args(*args)
            .env("PASTOR_CONFIG_DIR", &here.config)
            .env("PASTOR_STATE_DIR", &here.state)
            .env_remove("VISUAL")
            .env("EDITOR", editor)
            .output()
            .unwrap();
        let remote = pastor()
            .args(*args)
            .env("PASTOR_CONFIG_DIR", &c.config)
            .env("PASTOR_STATE_DIR", &c.state)
            .env("PASTOR_DATA_DIR", &c.data)
            .env("PATH", &c.path)
            .env_remove("PASTOR_HEAD")
            .env_remove("VISUAL")
            .env("EDITOR", editor)
            .output()
            .unwrap();
        assert_eq!(remote.status.code(), local.status.code(), "{args:?}");
        assert_eq!(norm(&remote.stdout), norm(&local.stdout), "{args:?}");
        assert_eq!(norm(&remote.stderr), norm(&local.stderr), "{args:?}");
    }
    for file in ["flock.toml", "pastor.toml"] {
        let read = |e: &Env| {
            norm(
                std::fs::read_to_string(e.config.join(file))
                    .unwrap()
                    .as_bytes(),
            )
        };
        assert_eq!(read(&there), read(&here), "{file}");
    }
    assert!(
        std::fs::read_to_string(there.config.join("pastor.toml"))
            .unwrap()
            .contains("# edited")
    );
    let names: Vec<String> = std::fs::read_dir(&c.config)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["client.toml"], "nothing else written here");
    assert!(!c.state.join("pastor.db").exists());
}

/// With a head set, every command reaches it or stays here on purpose; none
/// falls back to this machine's flock.toml, pastor.toml or trust table. A
/// head that does not answer stops them. `machine authorized-key` prints a
/// line for the head's own authorized_keys, so it is refused here with the
/// head named, and `serve` would be a second head. `config edit --local`
/// edits this machine's pastor.toml.
#[test]
fn a_remote_head_refuses_what_would_act_on_local_files() {
    let c = client(None);
    ok(c.head_set("unreachable", &["--force"]));
    for args in [
        &["machine", "add", "pi-1", "--local"][..],
        &["machine", "open", "pi-1"],
        &["flock", "list"],
        &["flock", "default", "show"],
        &["trust", "list"],
        &["config", "edit"],
        &["profile", "list"],
        &["task", "attach", "t-1"],
    ] {
        let out = c.cmd(args);
        assert_eq!(error_code(&out), "head_unreachable", "{args:?}");
    }
    let out = c.cmd(&["machine", "authorized-key", "pi-1", "--key", "-"]);
    assert_eq!(error_code(&out), "remote_head_unsupported");
    let v: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    let message = v["message"].as_str().unwrap();
    assert!(message.contains("machine authorized-key"), "{v}");
    assert!(message.contains("unreachable"), "{v}");
    assert!(!c.config.join("flock.toml").exists());
    assert!(!c.config.join("pastor.toml").exists());
    ok(c.cmd(&["completions", "bash"]));
    ok(c.cmd(&["connector", "list"]));

    // `--local` is this machine's pastor.toml, checked as the head checks it.
    let editor = c.tmp.path().join("tick-editor");
    std::fs::write(&editor, "#!/bin/sh\necho 'tick = \"5s\"' > \"$1\"\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let out = pastor()
        .args(["config", "edit", "--local"])
        .env("PASTOR_CONFIG_DIR", &c.config)
        .env("PASTOR_STATE_DIR", &c.state)
        .env("PATH", &c.path)
        .env_remove("PASTOR_HEAD")
        .env_remove("VISUAL")
        .env("EDITOR", &editor)
        .output()
        .unwrap();
    let saved = ok(out);
    assert!(saved.starts_with("saved "), "{saved}");
    assert!(
        saved.contains("applies when this machine's pastor serve starts"),
        "{saved}"
    );
    assert_eq!(
        std::fs::read_to_string(c.config.join("pastor.toml")).unwrap(),
        "tick = \"5s\"\n"
    );

    ok(c.cmd(&["head", "unset"]));
    assert_eq!(ok(c.cmd(&["head", "show"])), "head: this machine\n");
    let out = c.cmd(&["task", "list"]);
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("pastor serve is not running"),
        "back to the local head, and there is none"
    );
}

/// `head set` pings through the bridge first: ssh that cannot connect is
/// `head_unreachable` with ssh's own words, a machine with no head running is
/// `no_head`, a pastor with no `bridge` is `head_too_old`. Nothing is saved
/// unless `--force` says so.
#[test]
fn head_set_refuses_a_head_that_does_not_answer() {
    use std::os::unix::fs::PermissionsExt;
    let c = client(None);
    let out = c.head_set("unreachable", &[]);
    assert_eq!(error_code(&out), "head_unreachable");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("Connection refused"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!c.config.join("client.toml").exists());

    assert_eq!(error_code(&c.head_set("no-head", &[])), "no_head");
    assert_eq!(ok(c.cmd(&["head", "show"])), "head: this machine\n");

    let old = c.tmp.path().join("old-pastor");
    std::fs::write(
        &old,
        "#!/bin/sh\necho \"error: unrecognized subcommand '$1'\" >&2\nexit 2\n",
    )
    .unwrap();
    std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = c.cmd(&["head", "set", "no-head", "--pastor", old.to_str().unwrap()]);
    assert_eq!(error_code(&out), "head_too_old");

    let out = c.head_set("no-head", &["--force"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("saved anyway"));
    assert_eq!(ok(c.cmd(&["head", "show"])), "head: no-head (remote)\n");
    let text = std::fs::read_to_string(c.config.join("client.toml")).unwrap();
    assert!(text.contains("ssh = \"no-head\""), "{text}");
    // A command then says the same, and never falls back to this machine.
    assert_eq!(error_code(&c.cmd(&["task", "list"])), "no_head");
    assert_eq!(error_code(&c.cmd(&["task", "describe", "t-1"])), "no_head");
}

/// `shepherd_running` is only for a role of exactly `SHEPHERD_ROLE`; any
/// other role a pong carries is an ordinary head that answers as usual.
#[test]
fn head_set_treats_an_unfamiliar_role_as_an_ordinary_head() {
    use std::os::unix::fs::PermissionsExt;
    let c = client(None);
    let fake = c.tmp.path().join("fake-pastor");
    std::fs::write(
        &fake,
        "#!/bin/sh\nread line\necho '{\"kind\":\"pong\",\"data\":{\"version\":\"9.9.9\",\"protocol\":999999,\"role\":\"orchestrator\"}}'\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = c.cmd(&["head", "set", "no-head", "--pastor", fake.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("9.9.9"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// A pong answering with a protocol older than this CLI's is `head_too_old`,
/// even though the reply itself came through fine: this is not the
/// `unrecognized subcommand` case, where the remote has no `bridge` at all.
#[test]
fn head_set_refuses_a_pong_with_an_old_protocol() {
    use std::os::unix::fs::PermissionsExt;
    let c = client(None);
    let fake = c.tmp.path().join("fake-pastor");
    std::fs::write(
        &fake,
        "#!/bin/sh\nread line\necho '{\"kind\":\"pong\",\"data\":{\"version\":\"0.1.0\",\"protocol\":1}}'\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = c.cmd(&["head", "set", "no-head", "--pastor", fake.to_str().unwrap()]);
    assert_eq!(error_code(&out), "head_too_old");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("0.1.0"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!c.config.join("client.toml").exists());
}

/// A head of another version may answer a request with a variant this CLI
/// does not expect. The command stops with the usual JSON error on stderr,
/// code `internal`, and exit 1, never a Rust panic (exit 101).
#[test]
fn an_unexpected_head_reply_is_a_json_error() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let _reqs = text_head(&state.join("pastor.sock"), pastor::ipc::IPC_PROTOCOL);
    for args in [
        &["job", "list"][..],
        &["task", "list"],
        &["task", "describe", "t-1"],
        &["task", "retry", "t-1"],
        &["queue"],
    ] {
        let out = pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {stderr}");
        let err: serde_json::Value = serde_json::from_str(stderr.trim())
            .unwrap_or_else(|e| panic!("{args:?}: {e}: {stderr}"));
        assert_eq!(err["code"], "internal", "{args:?}: {err}");
    }
}
