use crate::helpers::*;

/// A head from before descriptions would drop `--description` without a
/// word, so the CLI refuses to send it one; without the flag it still goes.
#[test]
fn description_flags_refuse_a_head_from_before_descriptions() {
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
    let reqs = text_head(&state.join("pastor.sock"), 10);
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .output()
            .unwrap()
    };
    for args in [
        &["task", "run", "hi", "--description", "d"][..],
        &["flock", "add", "work", "--description", "d"],
        &["machine", "add", "pi-2", "--local", "--description", "d"],
    ] {
        assert_eq!(error_code(&run(args)), "head_too_old", "{args:?}");
    }
    assert!(
        reqs.lock().unwrap().iter().all(|r| r["op"] == "ping"),
        "only pings"
    );
    ok(run(&["flock", "add", "work"]));
}

#[test]
fn describe_a_job_flock_and_task_without_a_head() {
    let o = offline();
    std::fs::write(
        o.config.join("flock.toml"),
        "[[flock]]\nname = \"home\"\ndefault = true\n\n[[flock]]\nname = \"work\"\nagent = \"codex\"\nagent_args = [\"--full-auto\"]\ndeny = [\"Bash(rm:*)\"]\n\n[[machine]]\nname = \"pi-1\"\nlocal = true\nflock = \"work\"\ntags = [\"arm\"]\n",
    )
    .unwrap();
    ok(o.cmd(&["tick", "--job", "nightly"]));

    let j: serde_json::Value =
        serde_json::from_str(&ok(o.cmd(&["job", "describe", "nightly", "--json"]))).unwrap();
    assert_eq!(j["name"], "nightly");
    assert_eq!(j["schedule"], "every 1h");
    assert_eq!(j["enabled"], true);
    assert_eq!(j["connector"]["use"], "clock");
    assert_eq!(j["dispatch"]["repo"], "/tmp/r");
    assert_eq!(j["flock"], "home");
    assert!(j["last_run_at"].is_string(), "{j}");
    assert!(j["next_due"].is_string(), "{j}");
    assert_eq!(j["tasks"][0]["id"], 1);
    assert!(j["file"].as_str().unwrap().ends_with("jobs/nightly.toml"));
    let text = ok(o.cmd(&["job", "describe", "nightly"]));
    for want in ["every 1h", "clock", "/tmp/r", "next run:", "t-1", "sweep"] {
        assert!(text.contains(want), "{want}: {text}");
    }
    assert_eq!(
        error_code(&o.cmd(&["job", "describe", "ghost"])),
        "job_not_found"
    );

    let f: serde_json::Value =
        serde_json::from_str(&ok(o.cmd(&["flock", "describe", "work", "--json"]))).unwrap();
    assert_eq!(f["name"], "work");
    assert_eq!(f["default"], false);
    assert_eq!(f["agent"], "codex");
    assert_eq!(f["agent_args"], serde_json::json!(["--full-auto"]));
    assert_eq!(f["deny"], serde_json::json!(["Bash(rm:*)"]));
    assert_eq!(f["machines"], serde_json::json!(["pi-1"]));
    assert_eq!(f["tasks"], serde_json::json!([]));
    let h: serde_json::Value =
        serde_json::from_str(&ok(o.cmd(&["flock", "describe", "home", "--json"]))).unwrap();
    assert_eq!(h["default"], true);
    assert_eq!(h["tasks"][0]["state"], "queued");
    let text = ok(o.cmd(&["flock", "describe", "work"]));
    for want in ["default:", "no", "codex", "--full-auto", "pi-1"] {
        assert!(text.contains(want), "{want}: {text}");
    }
    assert_eq!(
        error_code(&o.cmd(&["flock", "describe", "nope"])),
        "unknown_flock"
    );

    // Without a head a machine is probed directly; a local one with no
    // herdr server reads as down.
    let m: serde_json::Value =
        serde_json::from_str(&ok(o.cmd(&["machine", "describe", "pi-1", "--json"]))).unwrap();
    assert_eq!(m["name"], "pi-1");
    assert_eq!(m["flock"], "work");
    assert_eq!(m["tags"], serde_json::json!(["arm"]));
    assert_eq!(
        error_code(&o.cmd(&["machine", "describe", "nope"])),
        "unknown_machine"
    );
}

/// Without a head: descriptions of jobs, flocks, machines and tasks show
/// in `--wide` lists (whole, since stdout is not a terminal), in
/// `describe`, and always in `--json`; `flock add` and `machine add` write
/// them.
#[test]
fn descriptions_show_in_lists_describe_and_json_without_a_head() {
    let o = offline();
    std::fs::write(
        o.config.join("jobs/nightly.toml"),
        format!("description = \"Sweep the repo every night\"\n{NIGHTLY}"),
    )
    .unwrap();
    std::fs::write(
        o.config.join("jobs/bare.toml"),
        "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\ndescription = \"#{{ item.key }}\"\nprompt = \"p\"\n",
    )
    .unwrap();
    std::fs::write(
        o.config.join("flock.toml"),
        "[[flock]]\nname = \"home\"\ndefault = true\ndescription = \"Personal errands\"\n\n\
         [[machine]]\nname = \"pi-1\"\nlocal = true\ndescription = \"The desk one\"\n",
    )
    .unwrap();
    ok(o.cmd(&["tick", "--job", "nightly"]));

    let plain = ok(o.cmd(&["job", "list"]));
    assert!(!plain.contains("DESCRIPTION"), "{plain}");
    let wide = ok(o.cmd(&["job", "list", "--wide"]));
    let header = wide.lines().next().unwrap();
    assert!(header.ends_with("DESCRIPTION"), "{wide}");
    assert!(wide.contains("Sweep the repo every night"), "{wide}");
    let bare = wide.lines().find(|l| l.starts_with("bare")).unwrap();
    assert!(bare.ends_with(" -"), "no description reads -: {wide}");
    let jobs: serde_json::Value =
        serde_json::from_str(&ok(o.cmd(&["job", "list", "--json", "-w"]))).unwrap();
    assert_eq!(jobs[0]["name"], "bare");
    assert_eq!(jobs[0]["description"], serde_json::Value::Null);
    assert_eq!(jobs[1]["description"], "Sweep the repo every night");
    let j: serde_json::Value =
        serde_json::from_str(&ok(o.cmd(&["job", "describe", "nightly", "--json"]))).unwrap();
    assert_eq!(j["description"], "Sweep the repo every night");
    let text = ok(o.cmd(&["job", "describe", "bare"]));
    assert!(text.contains("\ndescription:"), "{text}");

    // The clock item's title is the task's description.
    let tasks: serde_json::Value =
        serde_json::from_str(&ok(o.cmd(&["task", "list", "--json"]))).unwrap();
    let title = tasks[0]["item"]["title"].as_str().unwrap().to_string();
    assert_eq!(tasks[0]["description"], title.as_str());
    assert_eq!(tasks[0]["description_from"], "job nightly");
    let wide = ok(o.cmd(&["task", "list", "-w"]));
    assert!(
        wide.lines().next().unwrap().ends_with("DESCRIPTION"),
        "{wide}"
    );
    assert!(wide.lines().nth(1).unwrap().ends_with(&title), "{wide}");
    let text = ok(o.cmd(&["task", "describe", "t-1"]));
    assert!(
        text.contains(&format!("description: {title} (from job nightly)")),
        "{text}"
    );

    let wide = ok(o.cmd(&["flock", "list", "--wide"]));
    assert!(wide.contains("Personal errands"), "{wide}");
    let flocks: serde_json::Value =
        serde_json::from_str(&ok(o.cmd(&["flock", "list", "--json"]))).unwrap();
    assert_eq!(flocks[0]["description"], "Personal errands");
    let f: serde_json::Value =
        serde_json::from_str(&ok(o.cmd(&["flock", "describe", "home", "--json"]))).unwrap();
    assert_eq!(f["description"], "Personal errands");

    let wide = ok(o.cmd(&["machine", "list", "--wide"]));
    assert!(wide.contains("The desk one"), "{wide}");
    let ms: serde_json::Value =
        serde_json::from_str(&ok(o.cmd(&["machine", "list", "--json"]))).unwrap();
    assert_eq!(ms["machines"][0]["description"], "The desk one");
    let m: serde_json::Value =
        serde_json::from_str(&ok(o.cmd(&["machine", "describe", "pi-1", "--json"]))).unwrap();
    assert_eq!(m["description"], "The desk one");
    let text = ok(o.cmd(&["machine", "describe", "pi-1"]));
    assert!(text.contains("description: The desk one"), "{text}");

    let wide = ok(o.cmd(&["connector", "list", "--wide"]));
    assert!(
        wide.lines().next().unwrap().ends_with("DESCRIPTION"),
        "{wide}"
    );

    ok(o.cmd(&["flock", "add", "work", "--description", " Paid work "]));
    ok(o.cmd(&[
        "machine",
        "add",
        "pi-2",
        "user@pi-2",
        "--flock",
        "work",
        "--description",
        "A spare",
    ]));
    let file = std::fs::read_to_string(o.config.join("flock.toml")).unwrap();
    assert!(file.contains("description = \"Paid work\""), "{file}");
    assert!(file.contains("description = \"A spare\""), "{file}");
}

/// With a head: `task run --description` sets the task's; without it the
/// prompt's first line stands in.
#[test]
fn task_run_takes_a_description_through_the_head() {
    let env = start();
    let t: serde_json::Value = serde_json::from_str(&ok(env.cmd(&[
        "task",
        "run",
        "fix it\nthen stop",
        "--description",
        "Fix the flaky test",
        "--json",
    ])))
    .unwrap();
    assert_eq!(t["description"], "Fix the flaky test");
    assert_eq!(t["description_from"], "--description");
    let bare: serde_json::Value = serde_json::from_str(&ok(env.cmd(&[
        "task",
        "run",
        "look around\nslowly",
        "--json",
    ])))
    .unwrap();
    assert_eq!(bare["description"], "look around");
    assert_eq!(bare["description_from"], "the prompt");
    let wide = ok(env.cmd(&["task", "list", "--all", "--wide"]));
    assert!(wide.contains("Fix the flaky test"), "{wide}");
    assert!(wide.contains("look around"), "{wide}");
    let text = ok(env.cmd(&["task", "describe", &format!("t-{}", t["id"])]));
    assert!(
        text.contains("description: Fix the flaky test (from --description)"),
        "{text}"
    );
    ok(env.cmd(&["flock", "add", "lab", "--description", "Test rigs"]));
    let file = std::fs::read_to_string(env.config.join("flock.toml")).unwrap();
    assert!(file.contains("description = \"Test rigs\""), "{file}");
    let ms: serde_json::Value =
        serde_json::from_str(&ok(env.cmd(&["machine", "list", "--json", "--wide"]))).unwrap();
    assert_eq!(ms["machines"][0]["description"], serde_json::Value::Null);
}

#[test]
fn describe_a_machine_and_its_flock_with_a_head() {
    let env = start();
    let t: serde_json::Value =
        serde_json::from_str(&ok(env.cmd(&["task", "run", "hi", "--json"]))).unwrap();
    let id = format!("t-{}", t["id"]);
    env.wait_done(&id);
    let m: serde_json::Value =
        serde_json::from_str(&ok(env.cmd(&["machine", "describe", "fake", "--json"]))).unwrap();
    assert_eq!(m["name"], "fake");
    assert_eq!(m["channel"], "connected");
    assert_eq!(m["flock"], "default");
    assert_eq!(m["max_agents"], 2);
    assert!(m["herdr_version"].is_string(), "{m}");
    assert_eq!(m["tasks"][0]["id"], t["id"]);
    let text = ok(env.cmd(&["machine", "describe", "fake"]));
    for want in ["channel:", "connected", "herdr:", &id] {
        assert!(text.contains(want), "{want}: {text}");
    }
    let f: serde_json::Value =
        serde_json::from_str(&ok(env.cmd(&["flock", "describe", "default", "--json"]))).unwrap();
    assert_eq!(f["default"], true);
    assert_eq!(f["machines"], serde_json::json!(["fake"]));
    assert!(f["agents"].is_u64(), "a head knows the live agents: {f}");

    // A flock.toml that no longer loads leaves the head's flock in use, and
    // the CLI describes that one, not the file.
    std::fs::write(env.config.join("flock.toml"), "not [[ toml").unwrap();
    let m: serde_json::Value =
        serde_json::from_str(&ok(env.cmd(&["machine", "describe", "fake", "--json"]))).unwrap();
    assert_eq!(m["channel"], "connected");
    assert_eq!(m["tasks"][0]["id"], t["id"]);
    let f: serde_json::Value =
        serde_json::from_str(&ok(env.cmd(&["flock", "describe", "default", "--json"]))).unwrap();
    assert_eq!(f["machines"], serde_json::json!(["fake"]));
    assert_eq!(
        error_code(&env.cmd(&["machine", "describe", "nope"])),
        "unknown_machine"
    );
    assert_eq!(
        error_code(&env.cmd(&["flock", "describe", "nope"])),
        "unknown_flock"
    );
}
