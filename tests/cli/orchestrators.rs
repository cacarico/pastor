use crate::helpers::*;

/// The noun end to end: `list` shows each file's kind and state, an
/// invalid one with its error; `run` fires a run whose pre script printed
/// nothing, so no agent starts; `disable` turns it off; a person keeps its
/// note with `--name`; `describe` shows the run and the note.
#[test]
fn orchestrator_files_list_run_disable_and_note() {
    let env = start();
    write_orchestrator(&env, "merge", MERGE, "echo looked >&2");
    std::fs::write(
        env.config.join("orchestrators/night.toml"),
        "hours = { start = \"22:00\", stop = \"08:00\" }\nprompt = \"p\"\n",
    )
    .unwrap();
    let list = env.json(&["orchestrator", "list", "--json"]);
    assert_eq!(list[0]["name"], "merge", "{list}");
    assert_eq!(list[0]["kind"], "scheduled", "{list}");
    assert_eq!(list[0]["state"], "idle", "{list}");
    assert_eq!(list[1]["name"], "night", "{list}");
    assert_eq!(list[1]["state"], "invalid", "{list}");
    assert!(
        list[1]["error"]
            .as_str()
            .unwrap()
            .contains("kind is required"),
        "{list}"
    );
    let text = ok(env.cmd(&["orchestrator", "list"]));
    assert!(text.contains("NAME") && text.contains("merge"), "{text}");

    ok(env.cmd(&["orchestrator", "run", "merge"]));
    let deadline = Instant::now() + WAIT;
    let d = loop {
        let d = env.json(&["orchestrator", "describe", "merge", "--json"]);
        if d["runs"].as_array().is_some_and(|r| !r.is_empty()) {
            break d;
        }
        assert!(Instant::now() < deadline, "the run never finished: {d}");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(d["runs"][0]["outcome"], "no_lines", "{d}");
    assert_eq!(d["last_result"], "no lines", "{d}");
    let listed = env.json(&["task", "list", "--all", "--json"]);
    assert_eq!(listed.as_array().unwrap().len(), 0, "{listed}");
    assert_eq!(
        error_code(&env.cmd(&["orchestrator", "run", "night"])),
        "orchestrator_invalid"
    );
    assert_eq!(
        error_code(&env.cmd(&["orchestrator", "run", "nope"])),
        "orchestrator_not_found"
    );

    ok(env.cmd(&["orchestrator", "disable", "merge"]));
    let list = env.json(&["orchestrator", "list", "--json"]);
    assert_eq!(list[0]["state"], "off", "{list}");
    assert!(list[0]["next_run"].is_null(), "{list}");
    ok(env.cmd(&["orchestrator", "enable", "merge"]));

    assert_eq!(
        error_code(&env.cmd(&["orchestrator", "note", "x"])),
        "orchestrator_not_found"
    );
    ok(env.cmd(&["orchestrator", "note", "--name", "merge", "merged #31"]));
    let text = ok(env.cmd(&["orchestrator", "describe", "merge"]));
    assert!(text.contains("merged #31"), "{text}");
    assert!(text.contains("no lines"), "{text}");
}

/// A session orchestrator end to end, outside its hours: nothing starts on
/// its own, `run` is for the scheduled kind and `start` for the session
/// one, `start` starts it by hand, and `stop` ends it.
#[test]
fn a_session_orchestrator_starts_and_stops_by_hand() {
    use chrono::Timelike;
    let env = start();
    write_orchestrator(&env, "merge", MERGE, "");
    let hour = |ahead: u32| format!("{:02}:00", (chrono::Local::now().hour() + ahead) % 24);
    std::fs::write(
        env.config.join("orchestrators/night.toml"),
        format!(
            "kind = \"session\"\nhours = {{ start = \"{}\", stop = \"{}\" }}\nstop_grace = \"1s\"\nprompt = \"Watch the night.\"\n",
            hour(2),
            hour(3)
        ),
    )
    .unwrap();
    let list = env.json(&["orchestrator", "list", "--json"]);
    assert_eq!(list[1]["kind"], "session", "{list}");
    assert_eq!(list[1]["state"], "idle", "{list}");
    assert!(list[1]["next_run"].is_string(), "{list}");
    assert_eq!(
        error_code(&env.cmd(&["orchestrator", "run", "night"])),
        "orchestrator_kind"
    );
    assert_eq!(
        error_code(&env.cmd(&["orchestrator", "start", "merge"])),
        "orchestrator_kind"
    );
    assert_eq!(
        error_code(&env.cmd(&["orchestrator", "stop", "night"])),
        "orchestrator_not_running"
    );
    // This flock has no machine of the head's own, where an orchestrator's
    // agent runs: the session starts and holds its slot, and says why its
    // agent waits.
    let out = env.cmd(&["orchestrator", "start", "night"]);
    assert_eq!(error_code(&out), "runtime_error");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("local = true"),
        "{out:?}"
    );
    let d = env.json(&["orchestrator", "describe", "night", "--json"]);
    assert_eq!(d["state"], "running", "{d}");
    assert_eq!(d["session"]["started_by"], "hand", "{d}");
    let text = ok(env.cmd(&["orchestrator", "describe", "night"]));
    assert!(
        text.contains("session:") && text.contains("by hand"),
        "{text}"
    );
    let said = ok(env.cmd(&["orchestrator", "start", "night"]));
    assert!(said.contains("already runs"), "{said}");
    let said = ok(env.cmd(&["orchestrator", "stop", "night"]));
    assert!(said.contains("stopped"), "{said}");
    let d = env.json(&["orchestrator", "describe", "night", "--json"]);
    assert!(d["session"].is_null(), "{d}");
    assert_eq!(
        d["runs"].as_array().unwrap().last().unwrap()["outcome"],
        "stopped",
        "{d}"
    );
}

/// `task list` puts orchestrator tasks in a table of their own, above the
/// others; `--json` keeps one array.
#[test]
fn task_list_shows_orchestrators_in_their_own_table_first() {
    let env = start();
    ok(env.cmd(&["task", "run", "work", "--repo", "/tmp"]));
    ok(env.cmd(&[
        "task",
        "run",
        "plan",
        "--repo",
        "/tmp",
        "--role",
        "orchestrator",
    ]));
    let text = ok(env.cmd(&["task", "list", "--all"]));
    let o = text.find("orchestrators:").expect(&text);
    let t = text.find("tasks:").expect(&text);
    assert!(o < t, "{text}");
    let (orchestrators, tasks) = text.split_at(t);
    assert!(
        orchestrators.contains("t-2") && !orchestrators.contains("t-1 "),
        "{text}"
    );
    assert!(tasks.contains("t-1"), "{text}");
    let listed = env.json(&["task", "list", "--all", "--json"]);
    assert_eq!(listed.as_array().unwrap().len(), 2, "{listed}");
}
