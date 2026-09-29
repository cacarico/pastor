use crate::helpers::*;

/// An agent pastor started has `PASTOR_TASK` in its pane. `pastor task run`
/// from it gets a clear refusal and queues nothing; reads still work.
#[test]
fn an_agent_pastor_started_is_refused_a_task_run() {
    let env = start();
    let out = pastor()
        .args(["task", "run", "go on", "--repo", "/tmp"])
        .env("PASTOR_CONFIG_DIR", &env.config)
        .env("PASTOR_STATE_DIR", &env.state)
        .env("PASTOR_TASK", "t-3")
        .output()
        .unwrap();
    assert_eq!(error_code(&out), "agent_refused");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("t-3"), "{err}");
    assert!(err.contains("agents_change_fleet"), "{err}");
    let out = pastor()
        .args(["task", "list", "--json"])
        .env("PASTOR_CONFIG_DIR", &env.config)
        .env("PASTOR_STATE_DIR", &env.state)
        .env("PASTOR_TASK", "t-3")
        .output()
        .unwrap();
    let tasks: serde_json::Value = serde_json::from_slice(&ok(out).into_bytes()).unwrap();
    assert_eq!(tasks, serde_json::json!([]), "nothing was queued");
}

/// An agent may end its own task, `pastor task done` from its pane, and
/// nobody else's; outside a task's pane the command needs a task.
#[test]
fn an_agent_may_end_its_own_task_only() {
    let env = start();
    for _ in 0..2 {
        env.json(&["task", "run", "go on", "--repo", "/tmp", "--json"]);
    }
    let as_agent = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &env.config)
            .env("PASTOR_STATE_DIR", &env.state)
            .env("PASTOR_TASK", "t-1")
            .output()
            .unwrap()
    };
    assert_eq!(
        error_code(&as_agent(&["task", "done", "t-2"])),
        "agent_refused"
    );
    let ended: serde_json::Value =
        serde_json::from_str(&ok(as_agent(&["task", "done", "--json"]))).unwrap();
    assert_eq!(ended["id"], 1, "{ended}");
    assert_eq!(ended["state"], "done", "{ended}");
    assert_eq!(ended["ended"], true, "{ended}");
    ok(as_agent(&["task", "done", "t-1"]));
    let other = env.json(&["task", "describe", "t-2", "--json"]);
    assert!(other.get("ended").is_none(), "{other}");
    env.fails_with(&["task", "done"], "usage_error");
    // A human may end any task.
    assert_eq!(env.json(&["task", "done", "t-2", "--json"])["ended"], true);
}

/// A person starts an orchestrator with `task run --role orchestrator`; from
/// its pane it may run and close tasks, but not prune them or make another
/// orchestrator, and no plain agent may do any of these. `task describe` and
/// `task list --json` name the role.
#[test]
fn an_orchestrator_runs_tasks_but_only_a_person_starts_one() {
    let env = start();
    let o = env.json(&[
        "task",
        "run",
        "plan",
        "--repo",
        "/tmp",
        "--role",
        "orchestrator",
        "--json",
    ]);
    assert_eq!(o["role"], "orchestrator", "{o}");
    let from = |task: &str, args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &env.config)
            .env("PASTOR_STATE_DIR", &env.state)
            .env("PASTOR_TASK", task)
            .output()
            .unwrap()
    };
    let worker: serde_json::Value = serde_json::from_str(&ok(from(
        "t-1",
        &["task", "run", "go", "--repo", "/tmp", "--json"],
    )))
    .unwrap();
    assert_eq!(worker["role"], "agent", "{worker}");
    for task in ["t-1", "t-2"] {
        let out = from(
            task,
            &[
                "task",
                "run",
                "go",
                "--repo",
                "/tmp",
                "--role",
                "orchestrator",
            ],
        );
        assert_eq!(error_code(&out), "role_refused", "{task}");
    }
    // t-1's guard runs before the head is ever asked, so it must still know
    // t-1 is an orchestrator: its refusal names the role, not "agent".
    let pruned = from("t-1", &["task", "prune", "--done", "--older-than", "1d"]);
    assert_eq!(error_code(&pruned), "agent_refused");
    let err = String::from_utf8_lossy(&pruned.stderr);
    assert!(err.contains("orchestrator"), "{err}");
    assert!(!err.contains("is an agent pastor started"), "{err}");
    assert_eq!(
        error_code(&from("t-2", &["task", "run", "go", "--repo", "/tmp"])),
        "agent_refused"
    );
    assert_eq!(
        error_code(&from("t-2", &["task", "close", "t-2"])),
        "agent_refused"
    );
    let closed: serde_json::Value =
        serde_json::from_str(&ok(from("t-1", &["task", "close", "t-2", "--json"]))).unwrap();
    assert_eq!(closed["state"], "closed", "{closed}");
    let listed = env.json(&["task", "list", "--all", "--json"]);
    let roles: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["agent", "orchestrator"], "{listed}");
    let text = ok(env.cmd(&["task", "describe", "t-1"]));
    assert!(text.contains("role:       orchestrator"), "{text}");
}

/// A pre or post script runs with `PASTOR_ORCHESTRATOR`, and its `pastor`
/// calls get the orchestrator role: `task run` passes, `machine add` is
/// refused before it reaches anything, a name the head does not know is
/// refused, and an agent that also sets the variable keeps its own rights.
#[test]
fn a_pre_scripts_pastor_calls_get_the_orchestrator_role() {
    let env = start();
    write_orchestrator(&env, "merge", MERGE, "");
    let as_script = |name: &str, task: Option<&str>, args: &[&str]| {
        let mut c = pastor();
        c.args(args)
            .env("PASTOR_CONFIG_DIR", &env.config)
            .env("PASTOR_STATE_DIR", &env.state)
            .env("PASTOR_ORCHESTRATOR", name);
        if let Some(task) = task {
            c.env("PASTOR_TASK", task);
        }
        c.output().unwrap()
    };
    let t: serde_json::Value = serde_json::from_str(&ok(as_script(
        "merge",
        None,
        &["task", "run", "fix ci", "--repo", "/tmp", "--json"],
    )))
    .unwrap();
    assert_eq!(t["role"], "agent", "{t}");
    let before = std::fs::read_to_string(env.config.join("flock.toml")).unwrap();
    let out = as_script("merge", None, &["machine", "add", "pi-9", "user@pi-9"]);
    assert_eq!(error_code(&out), "agent_refused");
    assert!(String::from_utf8_lossy(&out.stderr).contains("orchestrator merge"));
    assert_eq!(
        std::fs::read_to_string(env.config.join("flock.toml")).unwrap(),
        before
    );
    let out = as_script(
        "merge",
        None,
        &[
            "task",
            "run",
            "x",
            "--repo",
            "/tmp",
            "--role",
            "orchestrator",
        ],
    );
    assert_eq!(error_code(&out), "role_refused");
    let out = as_script("ghost", None, &["task", "run", "x", "--repo", "/tmp"]);
    assert_eq!(error_code(&out), "agent_refused");
    // t-1 is a plain agent: PASTOR_TASK wins over PASTOR_ORCHESTRATOR.
    let out = as_script(
        "merge",
        Some("t-1"),
        &["task", "run", "x", "--repo", "/tmp"],
    );
    assert_eq!(error_code(&out), "agent_refused");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("is an agent pastor started"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    ok(as_script(
        "merge",
        None,
        &["orchestrator", "note", "from the script"],
    ));
    let d = env.json(&["orchestrator", "describe", "merge", "--json"]);
    assert_eq!(d["note"], "from the script", "{d}");
}

/// Machine, flock and job edits need no head, so the CLI refuses them
/// itself, and leaves flock.toml as it was; `agents_change_fleet = true`
/// turns that off.
#[test]
fn an_agent_pastor_started_may_not_edit_the_flock() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();
    let before = "[[machine]]\nname = \"pi-1\"\nlocal = true\n";
    std::fs::write(config.join("flock.toml"), before).unwrap();
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .env("PASTOR_TASK", "t-3")
            .output()
            .unwrap()
    };
    for args in [
        &["flock", "add", "work"][..],
        &["machine", "add", "pi-2", "--local"],
        &["machine", "remove", "pi-1"],
        &["job", "disable", "nightly"],
        // Both apply pastor.toml and flock.toml, head or no head.
        &["job", "reload"],
        &["tick", "--dry-run"],
        // herdr's agent terminal takes keys for any task's pane.
        &["task", "attach", "t-1"],
        // The connector catalog: each edit reloads the head's jobs.
        &["connector", "install", "acme/tools", "--yes"],
        &["connector", "link", tmp.path().to_str().unwrap()],
        &["connector", "uninstall", "echo"],
        &["connector", "unlink", "echo"],
        // A head started from the pane would dispatch with no request to
        // refuse.
        &["serve"],
        &["setup", "systemd"],
        // herdr's full UI drives every pane on the machine.
        &["machine", "open", "pi-1"],
    ] {
        assert_eq!(error_code(&run(args)), "agent_refused", "{args:?}");
    }
    assert_eq!(
        std::fs::read_to_string(config.join("flock.toml")).unwrap(),
        before
    );
    // pastor.toml holds agents_change_fleet, so editing it is refused too.
    assert_eq!(error_code(&run(&["config", "edit"])), "agent_refused");
    ok(run(&["flock", "list"]));
    ok(run(&["flock", "describe", "default"]));
    ok(run(&["machine", "describe", "pi-1"]));
    ok(run(&["connector", "list"]));

    std::fs::write(config.join("pastor.toml"), "agents_change_fleet = true\n").unwrap();
    ok(run(&["flock", "add", "work"]));
}

/// `job enable|disable` on a shepherd's own job (`local_job`) never reaches
/// the head, so a task caller is refused there too: the head is the only
/// place that knows a task's role, and this machine's shepherd keeps no
/// task store to check it. `agents_change_fleet` still lets it through.
#[test]
fn an_agent_task_may_not_toggle_this_machine_s_own_job() {
    let env = start_with_jobs(&[("nightly", NIGHTLY)]);
    let c = client(Some(&env));
    ok(c.head_set("head-up", &[]));
    std::fs::create_dir_all(c.config.join("jobs")).unwrap();
    let here_job = c.config.join("jobs/sweep.toml");
    std::fs::write(&here_job, SWEEP).unwrap();

    let as_agent = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &c.config)
            .env("PASTOR_STATE_DIR", &c.state)
            .env("PASTOR_DATA_DIR", &c.data)
            .env("PATH", &c.path)
            .env_remove("PASTOR_HEAD")
            .env("PASTOR_TASK", "t-3")
            .output()
            .unwrap()
    };
    for args in [
        &["job", "enable", "sweep"][..],
        &["job", "disable", "sweep"],
    ] {
        let out = as_agent(args);
        assert_eq!(error_code(&out), "agent_refused", "{args:?}");
    }
    assert_eq!(
        std::fs::read_to_string(&here_job).unwrap(),
        SWEEP,
        "an agent's refused enable/disable must not touch the file"
    );

    std::fs::write(c.config.join("pastor.toml"), "agents_change_fleet = true\n").unwrap();
    let text = ok(as_agent(&["job", "enable", "sweep"]));
    assert!(text.contains("enabled sweep"), "{text}");
}
