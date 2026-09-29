use crate::helpers::*;

#[test]
fn sigterm_shuts_the_daemon_down_cleanly() {
    assert_daemon_shuts_down_cleanly_on("TERM");
}

#[test]
fn sighup_shuts_the_daemon_down_cleanly() {
    assert_daemon_shuts_down_cleanly_on("HUP");
}

/// Copilot 4106353416, 4106669969: a head that is listening but does not
/// answer ping is a hard error (`head_unresponsive`) on every path that talks
/// to or reloads the head, flocks or not. Taking it for no head would let
/// `tick` start a second scheduler next to it, or an edit or a prune go
/// offline behind it. The head is pinged once per command: no second probe
/// can read it differently later in the same command.
#[test]
fn an_unresponsive_head_is_a_hard_error_on_every_head_path() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let flock_file = config.join("flock.toml");
    let before = "[[machine]]\nname = \"pi-1\"\nlocal = true\n";
    std::fs::write(&flock_file, before).unwrap();
    let ops = fake_head(&state.join("pastor.sock"), NO_PONG);

    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .output()
            .unwrap()
    };
    let paths: [&[&str]; 14] = [
        &["tick"],
        &["job", "list"],
        &["job", "enable", "j"],
        &["task", "run", "hi"],
        &["task", "run", "hi", "--flock", "work"],
        &["task", "list"],
        &["task", "describe", "t-1"],
        &["task", "prune", "--done", "--older-than", "3d"],
        &["machine", "list"],
        &["machine", "add", "pi-2", "--local"],
        &["machine", "move", "pi-1", "default"],
        &["flock", "list"],
        &["flock", "add", "work"],
        &["flock", "remove", "default"],
    ];
    for args in paths {
        let out = run(args);
        assert_eq!(error_code(&out), "head_unresponsive", "{args:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("not answering") && !stderr.contains("start pastor serve"),
            "{args:?}: {stderr}"
        );
        assert_eq!(std::fs::read_to_string(&flock_file).unwrap(), before);
    }
    assert!(!state.join("pastor.db").exists(), "nothing ran offline");
    // `task attach` goes to the machine directly, so a head that does not
    // answer must not stop it: it fails on the missing task, not the head.
    let out = run(&["task", "attach", "t-9"]);
    assert_ne!(error_code(&out), "head_unresponsive");
    let ops = ops.lock().unwrap();
    assert_eq!(ops.len(), paths.len(), "one ping per command: {ops:?}");
    assert!(ops.iter().all(|op| op == "ping"), "only pings: {ops:?}");
}
