use crate::helpers::*;

#[test]
fn task_send_trust_answers_the_prompt_and_trust_list_and_remove_show_it() {
    let env = start_with(&[], &[("FAKE_HERDR_TRUST_PROMPT", "Down,Enter")]);
    let out = env.cmd(&["task", "run", "hi", "--repo", "/tmp/app", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    wait_state(&env, "t-1", "blocked");

    let out = env.cmd(&["task", "send", "t-1", "--trust", "--key", "Enter"]);
    assert_eq!(out.status.code(), Some(2), "--trust takes no other input");
    let out = env.cmd(&["task", "send", "t-1", "--trust"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("trusted"));
    // Answered, the agent gets the prompt it was started for.
    wait_state(&env, "t-1", "running");
    // `Down` goes on its own, the pane is read to confirm it landed on
    // "Yes, I trust this folder", and only then does `Enter` confirm it.
    let calls = herdr_calls(&env, "pane.send_keys");
    assert_eq!(calls[0]["keys"], serde_json::json!(["Down"]));
    assert_eq!(calls[1]["keys"], serde_json::json!(["Enter"]));

    let out = env.cmd(&["trust", "list", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let list: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1, "{list}");
    assert_eq!(list[0]["machine"], "fake");
    assert_eq!(list[0]["repo"], "/tmp/app");
    let out = env.cmd(&["trust", "list"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("/tmp/app"));

    // The next task of that repo on that machine is answered by the head.
    let out = env.cmd(&["task", "run", "again", "--repo", "/tmp/app", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    wait_state(&env, "t-2", "running");
    // The 2 split calls that answered t-1's own prompt, plus the head's
    // single-call auto_trust for t-2 (never presses a key it hasn't read
    // the pane to confirm needs pressing, so it is not split).
    assert_eq!(herdr_calls(&env, "pane.send_keys").len(), 3);
    let log = std::fs::read_to_string(env.state.join("events.jsonl")).unwrap();
    assert!(log.contains("task.trusted"), "{log}");

    let out = env.cmd(&["trust", "remove", "fake", "/tmp/app"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = env.cmd(&["trust", "remove", "fake", "/tmp/app"]);
    assert_eq!(out.status.code(), Some(1));
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["code"], "not_trusted", "{err}");
    let out = env.cmd(&["trust", "list", "--json"]);
    let list: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(list, serde_json::json!([]));
}

/// A task with no `--repo` on a machine that cannot report its home (the
/// fake's bridge is a `command` machine) starts wherever herdr puts it, so
/// there is no folder to save: `--trust` answers the dialog and says it saved
/// nothing, instead of claiming trust it cannot match later. (With a home,
/// the folder is saved: `trust_for_a_task_with_no_repo_is_saved_for_its_folder`
/// in the machine actor's tests.)
#[test]
fn trust_for_a_task_with_no_folder_on_record_says_it_saved_nothing() {
    let env = start_with(&[], &[("FAKE_HERDR_TRUST_PROMPT", "Down,Enter")]);
    let out = env.cmd(&["task", "run", "hi", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    wait_state(&env, "t-1", "blocked");
    let out = env.cmd(&["task", "send", "t-1", "--trust"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        said.contains("it has no folder on record, so nothing was saved"),
        "{said}"
    );
    wait_state(&env, "t-1", "running");
    let out = env.cmd(&["trust", "list", "--json"]);
    let list: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(list, serde_json::json!([]));
}

/// With a head running, `trust` goes through it: the CLI works with the
/// store out of its reach, as it is on another machine.
#[test]
fn trust_add_list_and_remove_go_through_the_head() {
    use std::os::unix::fs::PermissionsExt;
    let env = start();
    let db = env.state.join("pastor.db");
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o000)).unwrap();
    let text = ok(env.cmd(&["trust", "add", "fake", "/tmp/app"]));
    assert!(text.contains("/tmp/app on fake is trusted"), "{text}");
    let list: serde_json::Value =
        serde_json::from_str(&ok(env.cmd(&["trust", "list", "--json"]))).unwrap();
    assert_eq!(list[0]["machine"], "fake", "{list}");
    assert_eq!(list[0]["repo"], "/tmp/app", "{list}");
    assert!(ok(env.cmd(&["trust", "list"])).contains("/tmp/app"));
    ok(env.cmd(&["trust", "remove", "fake", "/tmp/app"]));
    assert_eq!(
        error_code(&env.cmd(&["trust", "remove", "fake", "/tmp/app"])),
        "not_trusted"
    );
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn trust_add_list_and_remove_without_a_head() {
    let o = offline();
    let text = ok(o.cmd(&["trust", "add", "pi-1", "/srv/app"]));
    assert!(text.contains("/srv/app on pi-1 is trusted"), "{text}");
    let text = ok(o.cmd(&["trust", "add", "pi-1", "/srv/app"]));
    assert!(text.contains("already"), "{text}");
    let list: serde_json::Value =
        serde_json::from_str(&ok(o.cmd(&["trust", "list", "--json"]))).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1, "{list}");
    assert_eq!(list[0]["machine"], "pi-1");
    assert!(list[0]["trusted_at"].is_string(), "{list}");
    ok(o.cmd(&["trust", "remove", "pi-1", "/srv/app"]));
    assert_eq!(
        error_code(&o.cmd(&["trust", "remove", "pi-1", "/srv/app"])),
        "not_trusted"
    );
    assert!(ok(o.cmd(&["trust", "list"])).contains("no trusted repos"));
}
