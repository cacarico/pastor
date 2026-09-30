use crate::helpers::*;

#[test]
fn run_list_show_read_end_to_end() {
    let env = start();
    let out = env.cmd(&[
        "task",
        "run",
        "say hello\nthen stop",
        "--repo",
        "/tmp",
        "--json",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let task: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(task["state"], "running");
    assert_eq!(task["machine"], "fake");
    assert_eq!(task["agent_name"], "t-1");

    let out = env.cmd(&["task", "read", "t-1"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("fake output"));

    env.wait_done("t-1");
    // A done task is finished: the default list hides it and says where it
    // went, `--all` shows it, and `--json` follows the same selection.
    let out = env.cmd(&["task", "list"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("t-1"), "{text}");
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("no live tasks; pastor task list --all shows finished ones"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = env.cmd(&["task", "list", "--all"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("t-1") && text.contains("done"), "{text}");
    let out = env.cmd(&["task", "list", "--json"]);
    let tasks: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert!(tasks.is_empty(), "{tasks:?}");
    let out = env.cmd(&["task", "list", "--all", "--json"]);
    let tasks: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(tasks.len(), 1, "{tasks:?}");
    assert_eq!(tasks[0]["agent_name"], "t-1");
    assert_eq!(tasks[0]["state"], "done");

    let out = env.cmd(&["task", "describe", "t-9"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("task_not_found"));
}

#[test]
fn list_without_daemon_reads_the_database() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    let out = pastor()
        .args(["task", "list"])
        .env("PASTOR_CONFIG_DIR", &config)
        .env("PASTOR_STATE_DIR", &state)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not running"), "{stderr}");
    assert!(stderr.contains("no live tasks"), "{stderr}");
    assert!(
        out.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let out = pastor()
        .args(["task", "list", "--all"])
        .env("PASTOR_CONFIG_DIR", &config)
        .env("PASTOR_STATE_DIR", &state)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("no tasks"));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("no live tasks"));
}

#[test]
fn task_retry_close_and_prune_end_to_end() {
    let env = start();
    let t1 = env.json(&["task", "run", "first", "--json"]);
    assert_eq!(t1["state"], "running");
    env.wait_for("t-1 done", &["task", "describe", "t-1", "--json"], |t| {
        t.contains("\"done\"")
    });
    // Only failed or stale tasks retry.
    env.fails_with(&["task", "retry", "t-1"], "not_retryable");
    env.fails_with(&["task", "retry", "t-99"], "task_not_found");

    // A failed row whose agent is still up: retry makes a new task, and the
    // old agent is an orphan until it is closed.
    env.set_state(1, pastor::task::TaskState::Failed);
    let t2 = env.json(&["task", "retry", "t-1", "--json"]);
    assert_eq!(t2["id"], 2);
    assert_eq!(t2["retry_of"], 1);
    assert_eq!(t2["state"], "running");
    let list = env.wait_for("the orphan in list", &["task", "list"], |t| {
        t.contains("orphan")
    });
    assert!(list.contains("t-1") && list.contains("fake"), "{list}");
    assert!(
        list.contains("retry of t-1"),
        "the new row points back: {list}"
    );
    let status = env.json(&["machine", "list", "--json"]);
    assert_eq!(
        status["machines"][0]["orphans"],
        serde_json::json!(["t-1"]),
        "{status}"
    );
    // An orphan has no row, so no state or job: a view narrowed by either
    // leaves it out, and --all keeps it.
    for args in [
        &["task", "list", "--done"][..],
        &["task", "list", "--blocked"],
        &["task", "list", "--job", "run"],
    ] {
        let out = env.cmd(args);
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(!text.contains("orphan"), "{args:?}: {text}");
    }
    let all = env.cmd(&["task", "list", "--all"]);
    assert!(String::from_utf8_lossy(&all.stdout).contains("orphan"));
    // --flock narrows the orphans to that flock's machines too.
    env.cmd(&["flock", "add", "work"]);
    let work = env.cmd(&["task", "list", "--all", "--flock", "work"]);
    let text = String::from_utf8_lossy(&work.stdout);
    assert!(!text.contains("orphan"), "{text}");
    let default = env.cmd(&["task", "list", "--all", "--flock", "default"]);
    assert!(String::from_utf8_lossy(&default.stdout).contains("orphan"));

    let closed = env.json(&["task", "close", "t-1", "--json"]);
    assert_eq!(closed["state"], "closed");
    env.wait_for("no orphan", &["task", "list"], |t| !t.contains("orphan"));

    // Close with and without --remove-worktree.
    env.fails_with(
        &["task", "close", "t-2", "--remove-worktree"],
        "no_worktree",
    );
    let out = env.cmd(&["task", "close", "t-2"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("closed"));
    let t3 = env.json(&[
        "task",
        "run",
        "third",
        "--worktree",
        "--repo",
        "/tmp/r",
        "--json",
    ]);
    assert_eq!(t3["state"], "running");
    let closed = env.json(&["task", "close", "t-3", "--remove-worktree", "--json"]);
    assert_eq!(closed["state"], "closed");
    let out = env.cmd(&["task", "run", "no repo", "--worktree"]);
    assert_eq!(out.status.code(), Some(2), "clap refuses --worktree alone");

    // Prune: every closed task finished before "now minus 0s".
    let out = env.cmd(&["task", "prune", "--older-than", "3d"]);
    assert_eq!(out.status.code(), Some(2), "a state flag is required");
    let pruned = env.json(&["task", "prune", "--done", "--older-than", "3d", "--json"]);
    assert_eq!(pruned["pruned"], 0);
    let pruned = env.json(&["task", "prune", "--closed", "--older-than", "0s", "--json"]);
    assert_eq!(
        pruned["pruned"], 2,
        "t-3, the newest, stays so its id is not reused"
    );
    env.fails_with(&["task", "describe", "t-1"], "task_not_found");
    assert_eq!(
        env.json(&["task", "describe", "t-3", "--json"])["state"],
        "closed"
    );
    let out = env.cmd(&[
        "task",
        "prune",
        "--failed",
        "--closed",
        "--older-than",
        "1d",
    ]);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("pruned 0"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// `task close` takes several ids: one result line each, every id tried, and
/// exit 1 naming the ones that failed.
#[test]
fn task_close_takes_several_ids() {
    let env = start();
    for prompt in ["one", "two", "three", "four"] {
        env.json(&["task", "run", prompt, "--json"]);
    }
    let out = env.cmd(&["task", "close", "t-1", "t-2"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines, ["t-1 closed", "t-2 closed"], "{text}");

    // One that fails does not stop the rest, and the exit says which.
    let out = env.cmd(&["task", "close", "t-99", "t-3", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    let results: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(results[0]["task"], "t-99");
    assert_eq!(results[0]["code"], "task_not_found");
    assert_eq!(results[1]["task"], "t-3");
    assert_eq!(results[1]["state"], "closed");
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["code"], "close_failed", "{err}");
    assert!(err["message"].as_str().unwrap().contains("t-99"), "{err}");

    // --remove-worktree applies to every id: t-4 has none.
    let out = env.cmd(&["task", "close", "t-4", "t-1", "--remove-worktree"]);
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.lines().next().unwrap().starts_with("t-4 no_worktree"),
        "{text}"
    );
    assert_ne!(
        env.json(&["task", "describe", "t-4", "--json"])["state"],
        "closed"
    );
}

/// Without a head, prune still works on the database; retry and close need
/// the daemon and say so with a stable code.
#[test]
fn prune_works_without_a_daemon_and_retry_does_not() {
    let tmp = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", tmp.path().join("c"))
            .env("PASTOR_STATE_DIR", tmp.path().join("s"))
            .output()
            .unwrap()
    };
    let out = run(&["task", "prune", "--done", "--older-than", "1d", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["pruned"], 0);
    for args in [["task", "retry", "t-1"], ["task", "close", "t-1"]] {
        let out = run(&args);
        assert_eq!(out.status.code(), Some(1));
        let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(err["code"], "daemon_not_running", "{err}");
    }
    let out = run(&["task", "retry", "x-1"]);
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["code"], "usage_error");
}

#[test]
fn task_send_types_into_a_live_task_and_refuses_a_finished_one() {
    let env = start();
    let out = env.cmd(&["task", "run", "hi", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let task: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let pane = task["pane_id"].clone();

    let out = env.cmd(&["task", "send", "t-1", "go on"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = env.cmd(&["task", "send", "t-1", "--key", "esc", "--key", "Down"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = env.cmd(&["task", "send", "t-1", "draft", "--no-enter"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        herdr_calls(&env, "pane.send_text"),
        vec![
            serde_json::json!({"pane_id": pane, "text": "go on"}),
            serde_json::json!({"pane_id": pane, "text": "draft"}),
        ]
    );
    assert_eq!(
        herdr_calls(&env, "pane.send_keys"),
        vec![
            serde_json::json!({"pane_id": pane, "keys": ["Enter"]}),
            serde_json::json!({"pane_id": pane, "keys": ["esc", "Down"]}),
        ]
    );
    // The events log has the input, never the text.
    let log = std::fs::read_to_string(env.state.join("events.jsonl")).unwrap();
    assert!(log.contains("task.input"), "{log}");
    assert!(!log.contains("go on") && !log.contains("draft"), "{log}");

    // Usage: nothing to send, or --no-enter without text.
    let out = env.cmd(&["task", "send", "t-1"]);
    assert_eq!(out.status.code(), Some(2));
    let out = env.cmd(&["task", "send", "t-1", "--key", "Enter", "--no-enter"]);
    assert_eq!(out.status.code(), Some(2));

    // A done task whose pane is still open takes input and runs again.
    env.wait_done("t-1");
    let out = env.cmd(&["task", "send", "t-1", "commit and push"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let t = env.json(&["task", "describe", "t-1", "--json"]);
    assert_eq!(t["state"], "running", "{t}");

    // A closed one does not.
    let out = env.cmd(&["task", "close", "t-1"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = env.cmd(&["task", "send", "t-1", "more"]);
    assert_eq!(out.status.code(), Some(1));
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["code"], "task_not_live", "{err}");
}

#[test]
fn run_reads_the_prompt_from_a_file_or_stdin() {
    let env = start();
    // Quotes, a backtick, a dollar sign and a blank line: what one shell
    // argument cannot carry. The trailing newline is the editor's, not the
    // prompt's.
    let prompt = "say \"hi\" and 'bye'\n\n`date` costs $5";
    let file = env._tmp.path().join("prompt.md");
    std::fs::write(&file, format!("{prompt}\n")).unwrap();
    let out = env.cmd(&[
        "task",
        "run",
        "--prompt-file",
        file.to_str().unwrap(),
        "--json",
    ]);
    let t: serde_json::Value = serde_json::from_str(&ok(out)).unwrap();
    assert_eq!(t["prompt"], prompt, "{t}");

    let out = env.cmd_stdin(
        &["task", "run", "--prompt-file", "-", "--json"],
        "from stdin\r\n\n",
    );
    let t: serde_json::Value = serde_json::from_str(&ok(out)).unwrap();
    assert_eq!(t["prompt"], "from stdin", "{t}");
}

#[test]
fn run_prompt_file_errors_carry_stable_codes() {
    let env = start();
    let dir = env._tmp.path();
    let empty = dir.join("empty.md");
    std::fs::write(&empty, "").unwrap();
    let blank = dir.join("blank.md");
    std::fs::write(&blank, "\n \n").unwrap();
    let binary = dir.join("binary.md");
    std::fs::write(&binary, [0xff, 0xfe, 0x00]).unwrap();
    for (path, code) in [
        (empty.to_str().unwrap(), "prompt_file_empty"),
        (blank.to_str().unwrap(), "prompt_file_empty"),
        (binary.to_str().unwrap(), "prompt_file_unreadable"),
        (
            dir.join("missing.md").to_str().unwrap(),
            "prompt_file_unreadable",
        ),
        // A directory opens fine and fails on read.
        (dir.to_str().unwrap(), "prompt_file_unreadable"),
    ] {
        let out = env.cmd(&["task", "run", "--prompt-file", path]);
        assert_eq!(error_code(&out), code, "{path}");
    }
    let out = env.cmd_stdin(&["task", "run", "--prompt-file", "-"], "\n");
    assert_eq!(error_code(&out), "prompt_file_empty");

    // Nothing was dispatched.
    let listed = ok(env.cmd(&["task", "list", "--all", "--json"]));
    assert_eq!(listed.trim(), "[]", "{listed}");
}

#[test]
fn run_takes_exactly_one_of_prompt_and_prompt_file() {
    let env = start();
    let file = env._tmp.path().join("p.md");
    std::fs::write(&file, "hi\n").unwrap();
    // clap usage errors: plain text, exit 2.
    for args in [
        &["task", "run", "hi", "--prompt-file", file.to_str().unwrap()][..],
        &["task", "run"][..],
    ] {
        let out = env.cmd(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(serde_json::from_slice::<serde_json::Value>(&out.stderr).is_err());
    }
}

/// `task done --summary` (or `--summary-file`, `-` for stdin) ends the
/// round with what the agent said: `task describe` shows it, `--all-summaries`
/// every round's, `task list --wide` its outcome as RESULT, and `--json` the
/// `summary` object.
#[test]
fn a_task_done_with_a_summary_shows_it() {
    let env = start();
    for _ in 0..2 {
        env.json(&["task", "run", "go on", "--repo", "/tmp", "--json"]);
    }
    let ended = env.json(&[
        "task",
        "done",
        "t-1",
        "--summary",
        "Partial: tests left\npushed pastor/t-1",
        "--json",
    ]);
    assert_eq!(ended["summary"]["outcome"], "partial", "{ended}");
    assert_eq!(ended["summary"]["source"], "agent", "{ended}");
    assert_eq!(ended["summary"]["round"], 1, "{ended}");
    // Said again, it replaces the round's summary.
    let out = env.cmd_stdin(
        &["task", "done", "t-1", "--summary-file", "-", "--json"],
        "nothing to do\nalready merged\n",
    );
    let again: serde_json::Value = serde_json::from_str(&ok(out)).unwrap();
    assert_eq!(again["summary"]["outcome"], "nothing to do", "{again}");
    let text = ok(env.cmd(&["task", "describe", "t-1"]));
    assert!(
        text.contains("summary:    nothing to do (round 1, from the agent,"),
        "{text}"
    );
    assert!(text.contains("\n  already merged\n"), "{text}");
    let all = env.json(&["task", "describe", "t-1", "--all-summaries", "--json"]);
    assert_eq!(all["summaries"].as_array().unwrap().len(), 1, "{all}");
    let listed = ok(env.cmd(&["task", "list", "--all", "--wide"]));
    let header = listed.lines().next().unwrap();
    assert!(
        header.contains(" RESULT ") && header.ends_with("DESCRIPTION"),
        "{listed}"
    );
    let row = listed.lines().find(|l| l.starts_with("t-1 ")).unwrap();
    assert!(row.contains("nothing to do"), "{listed}");
    let row = listed.lines().find(|l| l.starts_with("t-2 ")).unwrap();
    assert!(row.contains(" - "), "{listed}");
    let tasks = env.json(&["task", "list", "--all", "--json"]);
    let t1 = tasks
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == 1)
        .unwrap();
    assert_eq!(
        t1["summary"]["text"], "nothing to do\nalready merged",
        "{t1}"
    );
    env.fails_with(&["task", "done", "t-2", "--summary", "  "], "summary_empty");
    env.fails_with(
        &[
            "task",
            "done",
            "t-2",
            "--summary-file",
            "/nonexistent/summary",
        ],
        "summary_file_unreadable",
    );
    // With no summary the round keeps the pane's last lines instead.
    let t2 = env.json(&["task", "done", "t-2", "--json"]);
    assert_eq!(t2["summary"]["outcome"], "no summary", "{t2}");
    assert_eq!(t2["summary"]["source"], "pane", "{t2}");
}

/// With a head running, `limit` goes through it; an agent pastor started
/// may list the limits but not clear one.
#[test]
fn limit_list_and_clear_go_through_the_head() {
    let env = start();
    seed_limit(&env.state);
    let list = env.json(&["limit", "list", "--json"]);
    assert_eq!(list[0]["account"], "me", "{list}");
    assert_eq!(list[0]["task_id"], 7, "{list}");
    let text = ok(env.cmd(&["limit", "list"]));
    assert!(text.contains("ACCOUNT"), "{text}");
    assert!(text.contains("5-hour limit"), "{text}");
    assert!(text.contains("t-7 on pi-1"), "{text}");
    let as_agent = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &env.config)
            .env("PASTOR_STATE_DIR", &env.state)
            .env("PASTOR_TASK", "t-1")
            .output()
            .unwrap()
    };
    assert!(ok(as_agent(&["limit", "list"])).contains("me"));
    assert_eq!(
        error_code(&as_agent(&["limit", "clear", "me"])),
        "agent_refused"
    );
    assert_eq!(
        error_code(&env.cmd(&["limit", "clear", "me", "--model", "opus"])),
        "not_exhausted"
    );
    assert!(ok(env.cmd(&["limit", "clear", "me"])).contains("cleared me"));
    assert_eq!(
        error_code(&env.cmd(&["limit", "clear", "me"])),
        "not_exhausted"
    );
    assert!(ok(env.cmd(&["limit", "list"])).contains("no account is exhausted"));
}

#[test]
fn limit_list_and_clear_without_a_head() {
    let o = offline();
    assert!(ok(o.cmd(&["limit", "list"])).contains("no account is exhausted"));
    seed_limit(&o.state);
    let list: serde_json::Value =
        serde_json::from_str(&ok(o.cmd(&["limit", "list", "--json"]))).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1, "{list}");
    ok(o.cmd(&["limit", "clear", "me"]));
    assert_eq!(
        error_code(&o.cmd(&["limit", "clear", "me"])),
        "not_exhausted"
    );
}

/// `--priority` queues the task at that level, which list, describe and
/// the JSON show; `task priority` moves a queued task and refuses one that
/// left the queue; a word that is not a level is `unknown_priority`.
#[test]
fn a_tasks_priority_is_set_shown_and_changed() {
    let env = start();
    let out = env.cmd(&["task", "run", "hi", "--priority", "high", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let task: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(task["priority"], "high", "{task}");
    assert_eq!(task["priority_from"], "task run", "{task}");
    let out = env.cmd(&["task", "describe", "t-1"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("priority:   high (from task run)\n"),
        "{text}"
    );
    let out = env.cmd(&["task", "list", "--all"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.lines().next().unwrap().contains("STATE    PRIORITY"),
        "{text}"
    );
    // t-1 went to a machine at once: it has left the queue.
    let out = env.cmd(&["task", "priority", "t-1", "low"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(last_error(&out).0, "not_queued");

    for args in [
        &["task", "run", "x", "--priority", "urgent"][..],
        &["task", "priority", "t-1", "urgent"],
    ] {
        let out = env.cmd(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert_eq!(last_error(&out).0, "unknown_priority", "{args:?}");
    }
}

/// `task attach` on a task whose pane is gone: one of a kind with no
/// session keeps the old error with a hint that only Claude tasks reopen; a
/// Claude task goes on to its machine to reopen the session, which a
/// `command` machine cannot show.
#[test]
fn attach_on_a_closed_task_reopens_only_a_claude_session() {
    let env = start();
    let t = env.json(&["task", "run", "hi", "--agent", "opencode", "--json"]);
    assert_eq!(t["spec"].get("session_id"), None, "{t}");
    env.json(&["task", "close", "t-1", "--json"]);
    let out = env.cmd(&["task", "attach", "t-1"]);
    assert_eq!(error_code(&out), "no_agent");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("t-1 is closed; nothing to attach to"), "{err}");
    assert!(err.contains("only Claude tasks can be reopened"), "{err}");

    let t = env.json(&["task", "run", "hi", "--json"]);
    assert!(t["spec"]["session_id"].is_string(), "{t}");
    env.json(&["task", "close", "t-2", "--json"]);
    env.fails_with(&["task", "attach", "t-2"], "no_terminal");
}
