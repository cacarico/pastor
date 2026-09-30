use crate::helpers::*;

/// A head from before flocks ignores the `flock` field of a `run` request
/// (serde skips unknown fields) and would queue the task in any flock. The
/// CLI asks the head's IPC protocol first and refuses `--flock` on an old
/// one, before anything is queued; `task list --flock` likewise, since an
/// old head would list every flock's tasks. Flock edits to flock.toml are
/// refused too, since the old head would reload them as one flock.
#[test]
fn flock_flags_refuse_a_head_from_before_flocks() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let ops = fake_head(&state.join("pastor.sock"), OLD_PONG);

    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .output()
            .unwrap()
    };
    for args in [
        &["task", "run", "hi", "--flock", "work"][..],
        &["task", "list", "--flock", "work"],
        // Copilot 4106353529: an old head's machines carry no flock.
        &["machine", "list", "--flock", "work"],
    ] {
        assert_eq!(error_code(&run(args)), "head_too_old", "{args:?}");
    }

    // Copilot 4106204671: flock.toml edits the old head would reload but not
    // honour (it reads every machine as one flock) are refused before the
    // file is written.
    let flock_file = config.join("flock.toml");
    let before = "[[machine]]\nname = \"pi-1\"\nlocal = true\n";
    std::fs::write(&flock_file, before).unwrap();
    for args in [
        &["flock", "add", "work"][..],
        &["flock", "add", "work", "--default"],
        &["flock", "default", "set", "default"],
        &["flock", "remove", "default"],
        &["machine", "add", "pi-2", "--local"],
        &["machine", "move", "pi-1", "default"],
        &["machine", "remove", "pi-1"],
    ] {
        assert_eq!(error_code(&run(args)), "head_too_old", "{args:?}");
        assert_eq!(std::fs::read_to_string(&flock_file).unwrap(), before);
    }
    let ops = ops.lock().unwrap();
    assert!(ops.iter().all(|op| op == "ping"), "only pings: {ops:?}");
}

/// Each command's request is checked against the head as it is sent
/// (`IpcRequest::min_protocol`): a head one protocol short of what the
/// request needs gets only the ping, and the command answers `head_too_old`
/// naming what the head lacks.
#[test]
fn each_request_refuses_a_head_one_protocol_short_of_it() {
    use pastor::ipc;
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        config.join("flock.toml"),
        "[[machine]]\nname = \"pi-1\"\nlocal = true\n",
    )
    .unwrap();
    let socket = state.join("pastor.sock");
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .env("EDITOR", "true")
            .output()
            .unwrap()
    };
    let cases: &[(&[&str], u32, &str)] = &[
        (
            &["task", "run", "hi"],
            ipc::PROFILE_PROTOCOL,
            "permission profiles",
        ),
        (
            &["task", "retry", "t-1"],
            ipc::PROFILE_PROTOCOL,
            "permission profiles",
        ),
        (
            &["task", "retry", "t-1", "--place", "pastor"],
            ipc::PROFILE_PROTOCOL,
            "--place",
        ),
        (
            &["job", "run", "j"],
            ipc::PROFILE_PROTOCOL,
            "permission profiles",
        ),
        (&["tick"], ipc::PROFILE_PROTOCOL, "permission profiles"),
        (
            &["task", "run", "hi", "--role", "orchestrator"],
            ipc::PROFILE_PROTOCOL,
            "roles",
        ),
        (
            &["task", "priority", "t-1", "low"],
            ipc::PRIORITY_PROTOCOL,
            "priority",
        ),
        (
            &["task", "priority", "t-1", "critical", "--preempt"],
            ipc::PREEMPT_PROTOCOL,
            "pausing",
        ),
        (
            &["task", "run", "hi", "--priority", "critical", "--preempt"],
            ipc::PREEMPT_PROTOCOL,
            "pausing",
        ),
        (
            &["task", "run", "hi", "--label", "x"],
            ipc::LABEL_PROTOCOL,
            "labels",
        ),
        (
            &["task", "run", "hi", "--now", "--machine", "pi-1"],
            ipc::NOW_PROTOCOL,
            "--now",
        ),
        (
            &["task", "run", "hi", "--summary", "require"],
            ipc::SUMMARY_MODE_PROTOCOL,
            "summary setting",
        ),
        (
            &["task", "run", "hi", "--keep-pane"],
            ipc::KEEP_PANE_PROTOCOL,
            "keeping a task's pane",
        ),
        (&["queue"], ipc::QUEUE_PROTOCOL, "pastor queue"),
        (
            &["queue", "move", "t-1", "--top"],
            ipc::QUEUE_PROTOCOL,
            "pastor queue",
        ),
        (&["flock", "edit"], ipc::FILE_PROTOCOL, "edits"),
        (&["config", "edit"], ipc::FILE_PROTOCOL, "edits"),
        (&["job", "edit", "j"], ipc::FILE_PROTOCOL, "edits"),
        (
            &["job", "describe", "j"],
            ipc::FILE_PROTOCOL,
            "job requests",
        ),
        (&["job", "enable", "j"], ipc::FILE_PROTOCOL, "job requests"),
        (&["trust", "list"], ipc::HEAD_READS_PROTOCOL, "trust"),
        (
            &["flock", "describe", "f"],
            ipc::HEAD_READS_PROTOCOL,
            "describe",
        ),
        (
            &["machine", "describe", "pi-1"],
            ipc::HEAD_READS_PROTOCOL,
            "describe",
        ),
        (
            &["orchestrator", "list"],
            ipc::ORCHESTRATOR_PROTOCOL,
            "orchestrator",
        ),
        (
            &["orchestrator", "start", "o"],
            ipc::SESSION_PROTOCOL,
            "session",
        ),
    ];
    for (args, protocol, feature) in cases {
        let _ = std::fs::remove_file(&socket);
        let reqs = text_head(&socket, protocol - 1);
        let out = run(args);
        assert_eq!(error_code(&out), "head_too_old", "{args:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(feature), "{args:?}: {stderr}");
        let reqs = reqs.lock().unwrap();
        assert!(
            reqs.iter().all(|r| r["op"] == "ping"),
            "{args:?}: only pings: {reqs:?}"
        );
    }
}

/// With a head running, `flock add|default` and `machine add|remove|move`
/// send the edit to the head and print its answer: the CLI leaves its own
/// flock.toml alone, since the head's is the one that counts. A head from
/// before these requests is refused with `head_too_old` before anything is
/// sent.
#[test]
fn flock_and_machine_edits_go_through_the_head() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let flock_file = config.join("flock.toml");
    std::fs::write(&flock_file, NAMED_FLOCKS).unwrap();
    let socket = state.join("pastor.sock");
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .output()
            .unwrap()
    };
    let cases: [(&[&str], serde_json::Value); 6] = [
        (
            &["flock", "add", "spare"],
            serde_json::json!({"op": "flock_add", "name": "spare", "default": false}),
        ),
        (
            &["flock", "add", "spare", "--default"],
            serde_json::json!({"op": "flock_add", "name": "spare", "default": true}),
        ),
        (
            &["flock", "default", "set", "work"],
            serde_json::json!({"op": "flock_set_default", "name": "work"}),
        ),
        (
            &[
                "machine", "add", "pi-2", "--local", "--flock", "work", "--tag", "gpu",
            ],
            serde_json::json!({"op": "machine_add", "machine": {
                "name": "pi-2", "local": true, "session": "default", "max_agents": 2,
                "tags": ["gpu"], "flock": "work"}}),
        ),
        (
            &["machine", "remove", "pi-1"],
            serde_json::json!({"op": "machine_remove", "name": "pi-1"}),
        ),
        (
            &["machine", "move", "pi-1", "work"],
            serde_json::json!({"op": "machine_move", "name": "pi-1", "flock": "work"}),
        ),
    ];

    let reqs = text_head(&socket, pastor::ipc::FLEET_EDIT_PROTOCOL);
    for (args, want) in &cases {
        let out = run(args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "said by the head\n",
            "{args:?}"
        );
        let sent = reqs.lock().unwrap().last().cloned().unwrap();
        assert_eq!(&sent, want, "{args:?}");
    }
    assert_eq!(std::fs::read_to_string(&flock_file).unwrap(), NAMED_FLOCKS);

    std::fs::remove_file(&socket).unwrap();
    let reqs = text_head(&socket, pastor::ipc::FLEET_EDIT_PROTOCOL - 1);
    for (args, _) in &cases {
        assert_eq!(error_code(&run(args)), "head_too_old", "{args:?}");
    }
    assert_eq!(std::fs::read_to_string(&flock_file).unwrap(), NAMED_FLOCKS);
    let reqs = reqs.lock().unwrap();
    assert!(
        reqs.iter().all(|r| r["op"] == "ping"),
        "only pings: {reqs:?}"
    );
}

/// Copilot 4106353474: once flock.toml declares named flocks, a command with
/// no `--flock` still means "the default flock", which an old head would
/// read as every machine. So every path that talks to or reloads the head
/// asks its protocol first, not only the ones that take `--flock`.
#[test]
fn named_flocks_refuse_every_head_path_on_a_head_from_before_flocks() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(config.join("flock.toml"), NAMED_FLOCKS).unwrap();
    let ops = fake_head(&state.join("pastor.sock"), OLD_PONG);

    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .output()
            .unwrap()
    };
    for args in [
        &["task", "run", "hi"][..],
        &["task", "list"],
        &["task", "describe", "t-1"],
        &["machine", "list"],
        &["flock", "list"],
        &["job", "list"],
        &["tick"],
    ] {
        assert_eq!(error_code(&run(args)), "head_too_old", "{args:?}");
    }
    let ops = ops.lock().unwrap();
    assert!(ops.iter().all(|op| op == "ping"), "only pings: {ops:?}");
}

/// `machine add` and `machine remove` reach a running head without a
/// restart, and a task left on a removed machine shows as such (Review
/// Focus 3).
#[test]
fn flock_edits_reach_a_running_head() {
    let env = start();
    let socket = env.config.parent().unwrap().join("herdr.sock");
    let bridge = format!(
        "{} --connect {}",
        env!("CARGO_BIN_EXE_fake-herdr"),
        socket.display()
    );
    let out = env.cmd(&[
        "machine",
        "add",
        "second",
        "--command",
        &bridge,
        "--max-agents",
        "1",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let said = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(said.contains("picked it up"), "{said}");
    wait_for_machine(&env, "second", true);

    let out = env.cmd(&["task", "run", "hello", "--machine", "second", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let task: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(task["machine"], "second", "{task}");

    let out = env.cmd(&["machine", "remove", "second"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("picked it up"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    wait_for_machine(&env, "second", false);
    // --all: the fake finishes agents on its own, and a task it finished
    // before the removal is done, which the default live view hides.
    let list = String::from_utf8_lossy(&env.cmd(&["task", "list", "--all"]).stdout).into_owned();
    assert!(list.contains("second (removed)"), "{list}");

    // Copilot 4103271200, 4103271289, 4103271156: a flock.toml that is
    // momentarily absent (an editor's delete-and-rename, a race with
    // `machine add|remove` rewriting it) must not be read as an empty
    // flock, which would mark every machine "(removed)". `machine remove`
    // above already rewrote the file without "second"; deleting it outright
    // stands in for that window.
    std::fs::remove_file(env.config.join("flock.toml")).unwrap();
    let list = String::from_utf8_lossy(&env.cmd(&["task", "list", "--all"]).stdout).into_owned();
    assert!(
        list.contains("second") && !list.contains("(removed)"),
        "{list}"
    );
}

/// `flock default show` prints the default flock and edits nothing.
#[test]
fn flock_default_show_prints_the_default() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .output()
            .unwrap()
    };
    // No flock.toml yet: the implicit flock.
    assert_eq!(ok(run(&["flock", "default", "show"])), "default\n");

    let text = "[[flock]]\nname = \"home\"\ndefault = true\n\n[[flock]]\nname = \"work\"\n";
    std::fs::write(config.join("flock.toml"), text).unwrap();
    assert_eq!(ok(run(&["flock", "default", "show"])), "home\n");
    assert_eq!(
        std::fs::read_to_string(config.join("flock.toml")).unwrap(),
        text
    );

    ok(run(&["flock", "default", "set", "work"]));
    assert_eq!(ok(run(&["flock", "default", "show"])), "work\n");
}

/// `flock` and `machine move` edit flock.toml in place: what they do not
/// touch, comments included, stays as the user wrote it.
#[test]
fn flock_commands_edit_the_file_and_keep_its_comments() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("flock.toml"),
        "# the fleet\n\n[[machine]]\nname = \"pi-1\"   # desk\nlocal = true\n",
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
    let file = || std::fs::read_to_string(config.join("flock.toml")).unwrap();
    let out = ok(run(&["flock", "add", "work"]));
    assert!(
        out.starts_with("added flock work; machine pi-1 stays in flock default;"),
        "{out}"
    );
    let text = file();
    assert!(text.starts_with("# the fleet\n"), "{text}");
    assert!(text.contains("name = \"pi-1\"   # desk"), "{text}");
    ok(run(&[
        "machine",
        "add",
        "pi-3",
        "user@pi-3",
        "--flock",
        "work",
    ]));
    assert_eq!(
        error_code(&run(&[
            "machine",
            "add",
            "pi-4",
            "user@pi-4",
            "--flock",
            "nope"
        ])),
        "unknown_flock"
    );

    let list: serde_json::Value =
        serde_json::from_str(&ok(run(&["flock", "list", "--json"]))).unwrap();
    assert_eq!(list[0]["name"], "default");
    assert_eq!(list[0]["default"], true);
    assert_eq!(list[0]["machines"], serde_json::json!(["pi-1"]));
    assert_eq!(list[1]["name"], "work");
    assert_eq!(list[1]["default"], false);
    assert_eq!(list[1]["machines"], serde_json::json!(["pi-3"]));
    assert_eq!(list[1]["queued"], 0);
    let table = ok(run(&["flock", "list"]));
    let header: Vec<&str> = table.lines().next().unwrap().split_whitespace().collect();
    assert_eq!(header, ["NAME", "DEFAULT", "MACHINES", "AGENTS", "QUEUED"]);

    assert_eq!(
        error_code(&run(&["flock", "remove", "work"])),
        "flock_not_empty"
    );
    assert_eq!(
        error_code(&run(&["flock", "remove", "default"])),
        "flock_is_default"
    );
    assert_eq!(
        error_code(&run(&["machine", "move", "pi-1", "nope"])),
        "unknown_flock"
    );
    assert_eq!(
        error_code(&run(&["machine", "move", "nope", "work"])),
        "unknown_machine"
    );
    ok(run(&["machine", "move", "pi-3", "default"]));
    ok(run(&["flock", "remove", "work"]));
    assert_eq!(
        error_code(&run(&["flock", "remove", "work"])),
        "unknown_flock"
    );

    // A new default takes new work; the machines stay where they were.
    let out = ok(run(&["flock", "add", "play", "--default"]));
    assert!(
        out.starts_with("added flock play, now the default; machine pi-1 stays in flock default;"),
        "{out}"
    );
    let list: serde_json::Value =
        serde_json::from_str(&ok(run(&["flock", "list", "--json"]))).unwrap();
    assert_eq!(list[1]["name"], "play");
    assert_eq!(list[1]["default"], true);
    assert_eq!(list[0]["machines"], serde_json::json!(["pi-1", "pi-3"]));
    ok(run(&["flock", "default", "set", "default"]));
    assert_eq!(
        error_code(&run(&["flock", "default", "set", "nope"])),
        "unknown_flock"
    );
    assert!(file().contains("# desk"), "{}", file());
}

/// The first flock added as the default takes the machines that name no
/// flock along, and says so; no empty `default` flock is left behind.
#[test]
fn a_first_default_flock_takes_the_machines_along() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("flock.toml"),
        "[[machine]]\nname = \"pi-1\"\nlocal = true\n\n[[machine]]\nname = \"pi-2\"\nlocal = true\n",
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
    let out = ok(run(&["flock", "add", "personal", "--default"]));
    assert!(
        out.starts_with("added flock personal, now the default; machines pi-1, pi-2 moved to it;"),
        "{out}"
    );
    let list: serde_json::Value =
        serde_json::from_str(&ok(run(&["flock", "list", "--json"]))).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1, "{list}");
    assert_eq!(list[0]["name"], "personal");
    assert_eq!(list[0]["default"], true);
    assert_eq!(list[0]["machines"], serde_json::json!(["pi-1", "pi-2"]));
}

/// A task queued in the implicit flock keeps its machines there when the
/// first flock is added as the default, and the output names the task.
#[test]
fn a_queued_task_keeps_the_machines_in_the_implicit_flock() {
    let env = start();
    // `fake` takes two agents; the third task waits in `default`.
    let mut last = serde_json::Value::Null;
    for _ in 0..3 {
        last = serde_json::from_str(&ok(env.cmd(&["task", "run", "hi", "--json"]))).unwrap();
    }
    assert_eq!(last["state"], "queued", "{last}");
    let id = last["id"].as_i64().unwrap();
    let out = ok(env.cmd(&["flock", "add", "personal", "--default"]));
    assert!(
        out.starts_with(&format!(
            "added flock personal, now the default; machine fake stays in flock default, which has queued tasks: t-{id};"
        )),
        "{out}"
    );
    let list: serde_json::Value =
        serde_json::from_str(&ok(env.cmd(&["flock", "list", "--json"]))).unwrap();
    assert_eq!(list[0]["name"], "default", "{list}");
    assert_eq!(list[0]["machines"], serde_json::json!(["fake"]));
    assert_eq!(list[1]["name"], "personal");
    assert_eq!(list[1]["default"], true);
}

/// `task run --flock` against a running head: the task waits for a machine
/// of its flock, the flock cannot be removed under it, `--machine` outside
/// it is refused, `task list --flock`
/// narrows to it, and moving a machine in hands it the task.
#[test]
fn a_task_waits_for_its_flock_end_to_end() {
    let env = start();
    ok(env.cmd(&["flock", "add", "work"]));
    let t: serde_json::Value = serde_json::from_str(&ok(
        env.cmd(&["task", "run", "hello", "--flock", "work", "--json"])
    ))
    .unwrap();
    assert_eq!(t["flock"], "work", "{t}");
    assert_eq!(t["state"], "queued", "no machine in work yet: {t}");
    assert_eq!(
        error_code(&env.cmd(&["task", "run", "x", "--flock", "work", "--machine", "fake"])),
        "flock_mismatch"
    );
    assert_eq!(
        error_code(&env.cmd(&["task", "run", "x", "--flock", "nope"])),
        "unknown_flock"
    );
    assert_eq!(
        error_code(&env.cmd(&["flock", "remove", "work"])),
        "flock_has_tasks",
        "a removed flock would strand its queued task"
    );
    let id = t["id"].as_i64().unwrap();
    let listed = |flock: &str| -> Vec<i64> {
        let out = ok(env.cmd(&["task", "list", "--all", "--flock", flock, "--json"]));
        let v: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
        v.iter().map(|t| t["id"].as_i64().unwrap()).collect()
    };
    assert_eq!(listed("work"), [id]);
    assert!(listed("default").is_empty());
    let table = ok(env.cmd(&["task", "list"]));
    let header: Vec<&str> = table.lines().next().unwrap().split_whitespace().collect();
    assert_eq!(
        header[..5],
        ["ID", "STATE", "PRIORITY", "MACHINE", "FLOCK"],
        "{table}"
    );
    assert!(
        ok(env.cmd(&["task", "describe", &format!("t-{id}")])).contains("flock:      work"),
        "task describe names the flock"
    );

    ok(env.cmd(&["machine", "move", "fake", "work"]));
    let deadline = Instant::now() + WAIT;
    loop {
        let t: serde_json::Value = serde_json::from_str(&ok(env.cmd(&[
            "task",
            "describe",
            &format!("t-{id}"),
            "--json",
        ])))
        .unwrap();
        if t["machine"] == "fake" {
            break;
        }
        assert!(Instant::now() < deadline, "never dispatched: {t}");
        std::thread::sleep(Duration::from_millis(100));
    }

    // The head removes an empty flock itself, and refuses a run in it at once.
    ok(env.cmd(&["flock", "add", "spare"]));
    assert!(
        ok(env.cmd(&["flock", "remove", "spare"])).contains("picked it up"),
        "the head did the removal"
    );
    assert_eq!(
        error_code(&env.cmd(&["task", "run", "x", "--flock", "spare"])),
        "unknown_flock"
    );
}

/// `flock join` and `flock leave` without a head: join, join again with
/// `--max`, leave, leave the last flock, `machine move`, and the old `flock`
/// key moved into the flock's `machines` on the first edit, comments kept.
/// `machine list` and `flock list` show where each machine is.
#[test]
fn flock_join_and_leave_edit_membership() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("flock.toml"),
        "# the fleet\n\n[[flock]]\nname = \"home\"\ndefault = true\n\n[[flock]]\nname = \"work\"\n\n\
         [[machine]]\nname = \"desk\"\ncommand = [\"false\"]\nmax_agents = 4\nflock = \"work\"   # for now\n\n\
         # the spare one\n[[machine]]\nname = \"lab\"\ncommand = [\"false\"]\n",
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
    let file = || std::fs::read_to_string(config.join("flock.toml")).unwrap();
    let list = || -> serde_json::Value {
        serde_json::from_str(&ok(run(&["flock", "list", "--json"]))).unwrap()
    };

    let out = ok(run(&["flock", "join", "home", "desk", "--max", "2"]));
    assert!(
        out.starts_with("desk is in flock home with 2; its flocks: home:2,work:4; "),
        "{out}"
    );
    let text = file();
    assert!(!text.contains("flock = \"work\""), "{text}");
    assert!(text.contains("machines = { desk = 4 }"), "{text}");
    assert!(text.starts_with("# the fleet\n"), "{text}");
    assert!(text.contains("# the spare one\n"), "{text}");

    ok(run(&["flock", "join", "home", "desk", "--max", "3"]));
    let l = list();
    assert_eq!(
        l[0]["members"],
        serde_json::json!([
            {"name": "desk", "share": 3, "max": 3, "live": null},
            {"name": "lab", "share": 2, "max": 2, "live": null}
        ])
    );
    let table = ok(run(&["flock", "list"]));
    assert!(table.contains("desk -/3, lab -/2"), "{table}");

    let out = ok(run(&["flock", "leave", "work", "desk"]));
    assert!(
        out.starts_with("desk left flock work; its flocks: home:3; "),
        "{out}"
    );
    assert_eq!(
        error_code(&run(&["flock", "leave", "work", "desk"])),
        "not_in_flock"
    );
    assert_eq!(
        error_code(&run(&["flock", "leave", "home", "lab"])),
        "not_in_flock"
    );
    assert_eq!(
        error_code(&run(&["flock", "join", "nope", "desk"])),
        "unknown_flock"
    );
    assert_eq!(
        error_code(&run(&["flock", "join", "home", "nope"])),
        "unknown_machine"
    );
    assert_eq!(
        error_code(&run(&["flock", "join", "home", "desk", "--max", "0"])),
        "config_error"
    );

    let out = ok(run(&["machine", "move", "lab", "work"]));
    assert!(out.starts_with("moved lab to flock work (work:2)"), "{out}");
    let out = ok(run(&["flock", "leave", "work", "lab"]));
    assert!(
        out.starts_with("lab left flock work; it is back in the default flock home; "),
        "{out}"
    );
    let out = ok(run(&["flock", "add", "play", "desk", "lab"]));
    assert!(
        out.starts_with("added flock play; joined: desk (home:3,play:4), lab (play:2); "),
        "{out}"
    );

    let ms: serde_json::Value =
        serde_json::from_str(&ok(run(&["machine", "list", "--json"]))).unwrap();
    let desk = &ms["machines"][0];
    assert_eq!(desk["flock"], "home");
    assert_eq!(
        desk["flocks"],
        serde_json::json!([
            {"name": "home", "share": 3, "max": 3, "live": 0},
            {"name": "play", "share": 4, "max": 4, "live": 0}
        ])
    );
    let table = ok(run(&["machine", "list"]));
    assert!(table.contains("home:3,play:4"), "{table}");
}

/// `flock join|leave` and `flock add` with machines go to a head of
/// `JOIN_PROTOCOL` or later; an older one would refuse the first two and add
/// the flock without its machines, so it is refused before anything is sent.
#[test]
fn flock_join_and_leave_go_through_the_head() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let flock_file = config.join("flock.toml");
    std::fs::write(&flock_file, NAMED_FLOCKS).unwrap();
    let socket = state.join("pastor.sock");
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .output()
            .unwrap()
    };
    let cases: [(&[&str], serde_json::Value); 4] = [
        (
            &["flock", "join", "work", "pi-1"],
            serde_json::json!({"op": "flock_join", "flock": "work", "machine": "pi-1"}),
        ),
        (
            &["flock", "join", "work", "pi-1", "--max", "3"],
            serde_json::json!({"op": "flock_join", "flock": "work", "machine": "pi-1", "max": 3}),
        ),
        (
            &["flock", "leave", "work", "pi-1"],
            serde_json::json!({"op": "flock_leave", "flock": "work", "machine": "pi-1"}),
        ),
        (
            &["flock", "add", "spare", "pi-1"],
            serde_json::json!({"op": "flock_add", "name": "spare", "default": false, "machines": ["pi-1"]}),
        ),
    ];
    let reqs = text_head(&socket, pastor::ipc::JOIN_PROTOCOL);
    for (args, want) in &cases {
        assert_eq!(ok(run(args)), "said by the head\n", "{args:?}");
        let sent = reqs.lock().unwrap().last().cloned().unwrap();
        assert_eq!(&sent, want, "{args:?}");
    }
    assert_eq!(std::fs::read_to_string(&flock_file).unwrap(), NAMED_FLOCKS);

    std::fs::remove_file(&socket).unwrap();
    let reqs = text_head(&socket, pastor::ipc::JOIN_PROTOCOL - 1);
    for (args, _) in &cases {
        assert_eq!(error_code(&run(args)), "head_too_old", "{args:?}");
    }
    // `flock add` with no machines is older than join.
    ok(run(&["flock", "add", "spare"]));
    let sent: Vec<serde_json::Value> = reqs
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r["op"] != "ping")
        .cloned()
        .collect();
    assert_eq!(
        sent,
        [serde_json::json!({"op": "flock_add", "name": "spare", "default": false})]
    );
}

/// A flock.toml that gives a flock a share and a max on a machine needs a
/// head that reads them: an older one would fail to reload the file and
/// keep its old flocks, so the CLI refuses it before sending anything.
#[test]
fn a_share_and_a_max_refuse_a_head_from_before_them() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        config.join("flock.toml"),
        "[[flock]]\nname = \"work\"\ndefault = true\nmachines = { pi-1 = { share = 1, max = 2 } }\n\n\
         [[machine]]\nname = \"pi-1\"\nlocal = true\n",
    )
    .unwrap();
    let socket = state.join("pastor.sock");
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .output()
            .unwrap()
    };
    let reqs = text_head(&socket, pastor::ipc::FLOCK_SHARE_PROTOCOL - 1);
    let out = run(&["flock", "add", "spare"]);
    assert_eq!(error_code(&out), "head_too_old");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("share and max"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(reqs.lock().unwrap().iter().all(|r| r["op"] == "ping"));

    std::fs::remove_file(&socket).unwrap();
    text_head(&socket, pastor::ipc::FLOCK_SHARE_PROTOCOL);
    assert_eq!(ok(run(&["flock", "add", "spare"])), "said by the head\n");
}

/// A flock.toml that sets a flock's `keep_pane` needs a head that reads
/// it, for the same reason as a share and a max.
#[test]
fn a_flock_keep_pane_refuses_a_head_from_before_it() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        config.join("flock.toml"),
        "[[flock]]\nname = \"work\"\ndefault = true\nkeep_pane = true\n\n\
         [[machine]]\nname = \"pi-1\"\nlocal = true\n",
    )
    .unwrap();
    let socket = state.join("pastor.sock");
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .output()
            .unwrap()
    };
    let reqs = text_head(&socket, pastor::ipc::KEEP_PANE_PROTOCOL - 1);
    let out = run(&["flock", "add", "spare"]);
    assert_eq!(error_code(&out), "head_too_old");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("keep_pane"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(reqs.lock().unwrap().iter().all(|r| r["op"] == "ping"));

    std::fs::remove_file(&socket).unwrap();
    text_head(&socket, pastor::ipc::KEEP_PANE_PROTOCOL);
    assert_eq!(ok(run(&["flock", "add", "spare"])), "said by the head\n");
}

/// `flock remove` with no head checks the store and edits flock.toml under
/// the fleet lock a starting head takes, so a head that starts in between
/// cannot queue a task in the flock between the check and the save: the
/// edit waits for the head to let go.
#[test]
fn offline_flock_remove_waits_for_a_starting_head() {
    let tmp = tempfile::tempdir().unwrap();
    let (config, state) = spare_flock(tmp.path());
    let paths = pastor::config::Paths::new(&config, &state);
    let held = pastor::fleet_edit::lock_fleet(&paths, Duration::from_secs(5)).unwrap();
    let child = pastor()
        .args(["flock", "remove", "spare"])
        .env("PASTOR_CONFIG_DIR", &config)
        .env("PASTOR_STATE_DIR", &state)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    let file = std::fs::read_to_string(config.join("flock.toml")).unwrap();
    assert!(file.contains("spare"), "edited under the lock: {file}");
    drop(held);
    let out = ok(child.wait_with_output().unwrap());
    assert!(out.contains("removed flock spare"), "{out}");
    let file = std::fs::read_to_string(config.join("flock.toml")).unwrap();
    assert!(!file.contains("spare"), "{file}");
}

/// A head that starts listening while an offline edit waits on the lock
/// could take a task the edit's store check never saw, so the edit stops
/// with `head_started` instead of saving.
#[test]
fn offline_flock_remove_refuses_a_head_that_started_while_it_waited() {
    let tmp = tempfile::tempdir().unwrap();
    let (config, state) = spare_flock(tmp.path());
    let paths = pastor::config::Paths::new(&config, &state);
    let held = pastor::fleet_edit::lock_fleet(&paths, Duration::from_secs(5)).unwrap();
    let child = pastor()
        .args(["flock", "remove", "spare"])
        .env("PASTOR_CONFIG_DIR", &config)
        .env("PASTOR_STATE_DIR", &state)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Past the ping that found no head, the edit waits on the lock.
    wait_for_fleet_lock_open(child.id(), &state);
    let _head = std::os::unix::net::UnixListener::bind(state.join("pastor.sock")).unwrap();
    drop(held);
    let out = child.wait_with_output().unwrap();
    assert_eq!(error_code(&out), "head_started");
    let file = std::fs::read_to_string(config.join("flock.toml")).unwrap();
    assert!(file.contains("spare"), "{file}");
}
