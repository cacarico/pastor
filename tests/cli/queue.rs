use crate::helpers::*;

/// `pastor queue` lists the queued tasks in the order they will start,
/// numbered over the whole queue, with where they wait, who queued them and
/// why they have not started; `--flock`, `--machine` and `--json` narrow
/// and shape it. `queue move` puts a task where it says, lifting or
/// lowering its level to fit, and refuses a task that is not queued, a
/// flag short or too many, and an agent pastor started.
#[test]
fn the_queue_lists_and_moves_tasks() {
    let env = start();
    ok(env.cmd(&["flock", "add", "work"]));
    ok(env.cmd(&["flock", "add", "lab"]));
    let run = |args: &[&str]| -> String {
        let mut argv = vec!["task", "run"];
        argv.extend_from_slice(args);
        argv.push("--json");
        let t: serde_json::Value = serde_json::from_str(&ok(env.cmd(&argv))).unwrap();
        assert_eq!(t["state"], "queued", "{t}");
        format!("t-{}", t["id"])
    };
    let a = run(&["a", "--flock", "work"]);
    let b = run(&["b", "--flock", "work", "--priority", "high"]);
    let c = run(&["c", "--flock", "lab"]);
    let ids = |args: &[&str]| -> Vec<(u64, String)> {
        let mut argv = vec!["queue", "--json"];
        argv.extend_from_slice(args);
        let v: Vec<serde_json::Value> = serde_json::from_str(&ok(env.cmd(&argv))).unwrap();
        v.iter()
            .map(|e| {
                (
                    e["pos"].as_u64().unwrap(),
                    e["id"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    };
    let pos = |n: u64, id: &str| (n, id.to_string());
    assert_eq!(ids(&[]), [pos(1, &b), pos(2, &a), pos(3, &c)]);
    assert_eq!(ids(&["--flock", "lab"]), [pos(3, &c)]);
    assert!(ids(&["--machine", "fake"]).is_empty());

    let table = ok(env.cmd(&["queue"]));
    let mut lines = table.lines();
    let header: Vec<&str> = lines.next().unwrap().split_whitespace().collect();
    assert_eq!(
        header,
        [
            "POS", "TASK", "LEVEL", "WHERE", "FROM", "WAITED", "WHY", "NOT", "YET"
        ],
        "{table}"
    );
    let first: Vec<&str> = lines.next().unwrap().split_whitespace().collect();
    assert_eq!(
        first[..6],
        ["1", b.as_str(), "high", "flock", "work", "task"],
        "{table}"
    );
    assert!(table.contains("flock work has no machines"), "{table}");
    let v: Vec<serde_json::Value> =
        serde_json::from_str(&ok(env.cmd(&["queue", "--json"]))).unwrap();
    assert_eq!(v[2]["where"], "flock lab", "{v:?}");
    assert_eq!(v[2]["from"], "task run", "{v:?}");
    assert_eq!(v[2]["why"], "flock lab has no machines", "{v:?}");

    // To the top in front of a high task: lifted.
    let out = ok(env.cmd(&["queue", "move", &c, "--top"]));
    assert_eq!(
        out.trim(),
        format!("{c} is 1 of 3 in the queue; lifted from normal to high")
    );
    assert_eq!(ids(&[]), [pos(1, &c), pos(2, &b), pos(3, &a)]);
    // Behind a normal task: lowered.
    let out = ok(env.cmd(&["queue", "move", &b, "--after", &a]));
    assert_eq!(
        out.trim(),
        format!("{b} is 3 of 3 in the queue; lowered from high to normal")
    );
    // Between two tasks of its own level: kept.
    let v: serde_json::Value = serde_json::from_str(&ok(
        env.cmd(&["queue", "move", &b, "--before", &a, "--json"])
    ))
    .unwrap();
    assert_eq!(
        (v["pos"].as_u64(), v["of"].as_u64()),
        (Some(2), Some(3)),
        "{v}"
    );
    assert_eq!(v["priority_was"], "normal", "{v}");
    assert_eq!(v["task"]["priority"], "normal", "{v}");
    let out = ok(env.cmd(&["queue", "move", &a, "--to", "1"]));
    assert_eq!(
        out.trim(),
        format!("{a} is 1 of 3 in the queue; lifted from normal to high")
    );
    // --top on the first task changes nothing.
    let out = ok(env.cmd(&["queue", "move", &a, "--top"]));
    assert_eq!(out.trim(), format!("{a} is 1 of 3 in the queue"));

    // A task a machine took has left the queue, and so has no place in it.
    let running: serde_json::Value =
        serde_json::from_str(&ok(env.cmd(&["task", "run", "d", "--json"]))).unwrap();
    let d = format!("t-{}", running["id"]);
    assert_eq!(
        error_code(&env.cmd(&["queue", "move", &d, "--top"])),
        "not_queued"
    );
    assert_eq!(
        error_code(&env.cmd(&["queue", "move", &a, "--before", &d])),
        "not_queued"
    );
    assert_eq!(
        error_code(&env.cmd(&["queue", "move", "t-99", "--top"])),
        "task_not_found"
    );
    for args in [
        &["queue", "move", "t-1"][..],
        &["queue", "move", "t-1", "--top", "--to", "2"],
        &["queue", "move", "t-1", "--to", "0"],
        &["queue", "--json", "move", "t-1", "--top"],
    ] {
        assert_eq!(env.cmd(args).status.code(), Some(2), "{args:?}");
    }
    let from_agent = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &env.config)
            .env("PASTOR_STATE_DIR", &env.state)
            .env("PASTOR_TASK", "t-3")
            .output()
            .unwrap()
    };
    assert_eq!(
        error_code(&from_agent(&["queue", "move", &c, "--top"])),
        "agent_refused"
    );
    assert!(
        from_agent(&["queue"]).status.success(),
        "reading the queue is fine"
    );
}
