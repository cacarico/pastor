use crate::helpers::*;

#[test]
fn agent_args_reach_herdr_from_the_flags_or_the_defaults() {
    let env = start();
    let out = env.cmd(&[
        "task",
        "run",
        "hi",
        "--agent-arg=--model",
        "--agent-arg",
        "claude-opus-5-5",
        "--json",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let want = serde_json::json!(["--model", "claude-opus-5-5"]);
    let task: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(task["spec"]["agent_args"], want);
    let start = env.agent_start_params("t-1");
    // A claude agent starts on a session of its own, after its args, and
    // the task records it.
    let session = start["args"][3].as_str().unwrap().to_string();
    assert_eq!(start["args"][2], "--session-id", "{start}");
    assert_eq!(without_session(&start["args"]), want, "{start}");
    assert_eq!(start["kind"], "claude");

    let out = env.cmd(&["task", "describe", "t-1"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("agent args: --model claude-opus-5-5"),
        "{text}"
    );
    assert!(text.contains(&format!("session:    {session}\n")), "{text}");
    let out = env.cmd(&["task", "describe", "t-1", "--json"]);
    let task: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(task["spec"]["session_id"], session.as_str(), "{task}");
    let out = env.cmd(&["task", "list", "--all", "--json"]);
    let tasks: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(tasks[0]["spec"]["agent_args"], want, "{tasks}");

    // `pastor task run` reads pastor.toml itself, so the daemon need not restart.
    std::fs::write(
        env.config.join("pastor.toml"),
        "tick = \"1s\"\nsettle = \"1s\"\nreconcile_every = \"1s\"\n\
         [defaults]\nagent_args = [\"--model\", \"claude-sonnet-5\"]\n",
    )
    .unwrap();
    let out = env.cmd(&["task", "run", "hi again", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let start = env.agent_start_params("t-2");
    assert_eq!(
        without_session(&start["args"]),
        serde_json::json!(["--model", "claude-sonnet-5"]),
        "{start}"
    );

    // A flock's own agent_args come before `[defaults]`, its deny list
    // reaches claude as --disallowedTools, and `task describe` prints what the
    // task resolved to.
    let flock = env.config.join("flock.toml");
    // A third slot, so t-3 need not wait for the first two to settle.
    let machines = std::fs::read_to_string(&flock)
        .unwrap()
        .replace("max_agents = 2", "max_agents = 3");
    std::fs::write(
        &flock,
        format!(
            "[[flock]]\nname = \"default\"\ndefault = true\nagent_args = [\"--model\", \"claude-haiku-4-5\"]\ndeny = [\"WebFetch\"]\n\n{machines}"
        ),
    )
    .unwrap();
    assert!(env.cmd(&["job", "reload"]).status.success());
    let out = env.cmd(&["task", "run", "third", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let start = env.agent_start_params("t-3");
    assert_eq!(
        without_session(&start["args"]),
        serde_json::json!([
            "--model",
            "claude-haiku-4-5",
            "--disallowedTools",
            "WebFetch"
        ]),
        "{start}"
    );
    let out = env.cmd(&["task", "describe", "t-3"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("agent:      claude (from defaults)\n"),
        "{text}"
    );
    assert!(
        text.contains("agent args: --model claude-haiku-4-5"),
        "{text}"
    );
    assert!(text.contains("deny:       WebFetch\n"), "{text}");
}

/// `[agents.claude-personal] kind = "claude"` with an env: herdr starts a
/// claude in a workspace created with that env, and the task keeps the name.
#[test]
fn an_agent_definition_reaches_herdr_as_its_kind_and_env() {
    let env = start();
    std::fs::write(
        env.config.join("pastor.toml"),
        "tick = \"1s\"\nsettle = \"1s\"\nreconcile_every = \"1s\"\n\
         [agents.claude-personal]\nkind = \"claude\"\n\
         env = { CLAUDE_CONFIG_DIR = \"/srv/claude-personal\" }\n",
    )
    .unwrap();
    let out = env.cmd(&["task", "run", "hi", "--agent", "claude-personal", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let start = env.agent_start_params("t-1");
    assert_eq!(start["kind"], "claude", "{start}");
    let reqs: Vec<serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(&env.herdr_log).unwrap()).unwrap();
    let ws = reqs
        .iter()
        .find(|r| r["method"] == "workspace.create")
        .unwrap();
    assert_eq!(
        ws["params"]["env"],
        serde_json::json!({"CLAUDE_CONFIG_DIR": "/srv/claude-personal", "PASTOR_TASK": "t-1"}),
        "{ws}"
    );
    let out = env.cmd(&["task", "describe", "t-1"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("agent:      claude-personal (from task run)\n"),
        "{text}"
    );
}

/// `--model sonnet` starts the agent with the model's args before its own,
/// and describe, list and the JSON name it; a name `[models]` lacks, one of
/// another kind than `--agent`, and raw args in `--model` are refused.
#[test]
fn a_named_model_reaches_herdr_and_bad_ones_are_refused() {
    let env = start();
    std::fs::write(
        env.config.join("pastor.toml"),
        "tick = \"1s\"\nsettle = \"1s\"\nreconcile_every = \"1s\"\n\
         [defaults]\nagent_args = [\"-v\"]\n\
         [models.sonnet]\nkind = \"claude\"\nargs = [\"--model\", \"claude-sonnet-5\"]\n",
    )
    .unwrap();
    let out = env.cmd(&["task", "run", "hi", "--model", "sonnet", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let task: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(task["model"], "sonnet", "{task}");
    let start = env.agent_start_params("t-1");
    assert_eq!(
        without_session(&start["args"]),
        serde_json::json!(["--model", "claude-sonnet-5", "-v"]),
        "{start}"
    );
    let out = env.cmd(&["task", "describe", "t-1"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("model:      sonnet (from task run)\n"),
        "{text}"
    );
    let out = env.cmd(&["task", "list", "--all"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.lines().next().unwrap().contains("AGENT   MODEL"),
        "{text}"
    );
    assert!(text.contains("claude  sonnet"), "{text}");

    for (args, code) in [
        (&["--model", "haiku"][..], "unknown_model"),
        (&["--model=--model claude-opus-5-5"], "unknown_model"),
        (
            &["--model", "sonnet", "--agent", "codex"],
            "model_kind_mismatch",
        ),
    ] {
        let out = env.cmd(&[&["task", "run", "x"][..], args].concat());
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert_eq!(last_error(&out).0, code, "{args:?}");
    }
}

/// A model of another kind than the machine's agent runs as the agent its
/// `agents` names for that kind, with only the model's args, and describe
/// says where it came from; without that entry, a task pinned there is
/// refused.
#[test]
fn a_model_of_another_kind_runs_on_the_machines_agent_for_it() {
    let env = start();
    std::fs::write(
        env.config.join("pastor.toml"),
        "tick = \"1s\"\nsettle = \"1s\"\nreconcile_every = \"1s\"\n\
         [defaults]\nagent_args = [\"-v\"]\n\
         [models.gpt]\nkind = \"opencode\"\nargs = [\"--model\", \"openai/gpt-5.5\"]\n",
    )
    .unwrap();
    let flock_file = env.config.join("flock.toml");
    let plain = std::fs::read_to_string(&flock_file).unwrap();
    std::fs::write(
        &flock_file,
        format!("{plain}agents = {{ opencode = \"opencode\" }}\n"),
    )
    .unwrap();
    env.json(&["task", "run", "hi", "--model", "gpt", "--json"]);
    let start = env.agent_start_params("t-1");
    assert_eq!(start["kind"], "opencode", "{start}");
    assert_eq!(
        start["args"],
        serde_json::json!(["--model", "openai/gpt-5.5"]),
        "{start}"
    );
    let out = env.cmd(&["task", "describe", "t-1"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("agent:      opencode (from machine fake agents.opencode)\n"),
        "{text}"
    );
    let out = env.cmd(&["machine", "describe", "fake"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("opencode=opencode"), "{text}");

    std::fs::write(&flock_file, plain).unwrap();
    env.fails_with(
        &["task", "run", "x", "--model", "gpt", "--machine", "fake"],
        "model_kind_mismatch",
    );
}

/// `--profile ci` starts Claude with `--permission-mode dontAsk` and the
/// profile's lists as its flags, and describe and the JSON name it; an
/// unknown name, raw args in `--profile`, `unrestricted` pinned to a machine
/// that is not, and args that pick a permission mode are refused.
#[test]
fn a_profile_reaches_herdr_and_bad_ones_are_refused() {
    let env = start();
    std::fs::write(
        env.config.join("pastor.toml"),
        "tick = \"1s\"\nsettle = \"1s\"\nreconcile_every = \"1s\"\n\
         [profiles.ci]\nallow = [\"Bash(make:*)\"]\ndeny = [\"WebFetch\"]\n",
    )
    .unwrap();
    let t = env.json(&["task", "run", "hi", "--profile", "ci", "--json"]);
    assert_eq!(t["profile"], "ci", "{t}");
    let start = env.agent_start_params("t-1");
    assert_eq!(
        without_session(&start["args"]),
        serde_json::json!([
            "--permission-mode",
            "dontAsk",
            "--allowedTools",
            "Bash(make:*)",
            "--disallowedTools",
            "WebFetch"
        ]),
        "{start}"
    );
    let out = env.cmd(&["task", "describe", "t-1"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("profile:    ci (from task run)\n"), "{text}");

    for (args, code) in [
        (&["--profile", "nope"][..], "unknown_profile"),
        (&["--profile=--permission-mode"], "unknown_profile"),
        (
            &["--profile", "unrestricted", "--machine", "fake"],
            "profile_not_allowed",
        ),
        (
            &[
                "--profile",
                "ci",
                "--agent-arg",
                "--dangerously-skip-permissions",
            ],
            "profile_args_conflict",
        ),
    ] {
        let out = env.cmd(&[&["task", "run", "x"][..], args].concat());
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert_eq!(last_error(&out).0, code, "{args:?}");
    }
}

/// opencode has no flags for tool lists, so a profiled opencode task is
/// taken, and its agent starts with no permission args: its rules go in the
/// pane's env (`OPENCODE_PERMISSION`, checked in `dispatch`'s tests).
#[test]
fn a_profile_reaches_opencode_without_flags() {
    let env = start();
    let t = env.json(&[
        "task",
        "run",
        "hi",
        "--agent",
        "opencode",
        "--profile",
        "review",
        "--json",
    ]);
    assert_eq!(t["profile"], "review", "{t}");
    let start = env.agent_start_params("t-1");
    assert_eq!(start["kind"], "opencode", "{start}");
    assert_eq!(start["args"], serde_json::json!([]), "{start}");
}

/// `make smoke-profiles`'s script, against the fake herdr: a review task
/// per agent that ends done passes, and the tasks are closed after; with no
/// machine named it refuses to start.
#[test]
fn the_profile_smoke_script_passes_tasks_that_end_done() {
    let env = start();
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/smoke-profiles.sh");
    let run = |vars: &[(&str, &str)]| {
        let mut c = Command::new("sh");
        common::scrub(&mut c);
        c.arg(script)
            .env("PASTOR", env!("CARGO_BIN_EXE_pastor"))
            .env("PASTOR_CONFIG_DIR", &env.config)
            .env("PASTOR_STATE_DIR", &env.state)
            .env("POLL", "1")
            .env("TIMEOUT", "30")
            .env_remove("CLAUDE")
            .env_remove("OPENCODE");
        for (k, v) in vars {
            c.env(k, v);
        }
        c.output().unwrap()
    };
    let out = run(&[("REPO", "/srv/app")]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");

    let out = run(&[
        ("REPO", "/srv/app"),
        ("CLAUDE", "fake"),
        ("OPENCODE", "fake"),
    ]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(text.contains("PASS claude on fake: t-1 done"), "{text}");
    assert!(text.contains("PASS opencode on fake: t-2 done"), "{text}");
    for (t, agent) in [("t-1", "claude"), ("t-2", "opencode")] {
        let got = env.json(&["task", "describe", t, "--json"]);
        assert_eq!(got["state"], "closed", "{got}");
        assert_eq!(got["profile"], "review", "{got}");
        assert_eq!(got["spec"]["agent"], agent, "{got}");
    }
}

/// `pastor profile list|describe` read pastor.toml with no head: the
/// built-in profiles and the written ones, `extends` followed, and an
/// unknown name refused.
#[test]
fn profile_list_and_describe_show_the_profiles() {
    let (_tmp, config, state) = completion_config();
    std::fs::write(
        config.join("pastor.toml"),
        "[profiles.ci]\ndescription = \"develop, plus docker\"\nextends = \"develop\"\nallow = [\"Bash(docker:*)\"]\n",
    )
    .unwrap();
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .env("PASTOR_DATA_DIR", state.join("data"))
            .output()
            .unwrap()
    };
    let out = run(&["profile", "list"]);
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    let names: Vec<&str> = text
        .lines()
        .skip(1)
        .map(|l| l.split_whitespace().next().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["ci", "develop", "review", "unrestricted"],
        "{text}"
    );
    assert!(
        text.contains("pastor.toml  develop  develop, plus docker"),
        "{text}"
    );

    let out = run(&["profile", "describe", "ci", "--json"]);
    assert!(out.status.success(), "{out:?}");
    let ci: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(ci["chain"], serde_json::json!(["ci", "develop"]));
    assert_eq!(ci["source"], "config");
    assert_eq!(
        ci["allow"].as_array().unwrap().last().unwrap(),
        "Bash(docker:*)"
    );
    assert!(
        ci["deny"]
            .as_array()
            .unwrap()
            .contains(&"Bash(sudo:*)".into())
    );

    let out = run(&["profile", "describe", "review"]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("source: built-in"), "{text}");
    assert!(text.contains("deny: Edit, Write"), "{text}");

    assert_eq!(
        last_error(&run(&["profile", "describe", "nope"])).0,
        "unknown_profile"
    );
    let (ok, out) = complete(&config, &state, &["profile", "describe", ""]);
    assert!(ok);
    assert!(
        out.starts_with("ci\tdevelop, plus docker\ndevelop\t"),
        "{out}"
    );
}
