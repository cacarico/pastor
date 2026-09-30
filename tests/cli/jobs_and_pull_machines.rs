use crate::helpers::*;

#[test]
fn clock_job_creates_tasks_end_to_end() {
    let env = start_with_jobs(&[(
        "tick",
        "every = \"1s\"\n[connector]\nuse = \"clock\"\n[dispatch]\nrepo = \"/tmp\"\nprompt = \"clock {{ item.key }} for {{ job.name }} as {{ task.id }}\"\n",
    )]);
    let deadline = Instant::now() + WAIT;
    let tasks: Vec<serde_json::Value> = loop {
        let out = env.cmd(&["task", "list", "--all", "--job", "tick", "--json"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let tasks: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
        if !tasks.is_empty() {
            break tasks;
        }
        assert!(
            Instant::now() < deadline,
            "the clock job never queued a task"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let t = &tasks[tasks.len() - 1];
    assert_eq!(t["job"], "tick");
    let prompt = t["prompt"].as_str().unwrap();
    assert!(prompt.starts_with("clock 20"), "{prompt}");
    assert!(prompt.contains(" for tick as t-"), "{prompt}");
    assert_eq!(t["item"]["key"], prompt.split(' ').nth(1).unwrap());

    let out = env.cmd(&["job", "list"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("tick") && text.contains("every 1s") && text.contains("ok:"),
        "{text}"
    );

    // The clock connector keys an item by the current wall-clock second, and
    // the background scheduler (also "every 1s") is racing this forced dry
    // run for that same second: whichever gets there first marks it seen. A
    // 0-created dry run right after a real run is correct product behaviour
    // (the item shows up as `skipped_seen` instead), so assert the outcome
    // either way rather than retry for the lucky one.
    let out = env.cmd(&["tick", "--dry-run", "--job", "tick", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let runs: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(runs[0]["outcome"], "dry_run");
    assert_eq!(runs[0]["items"], 1);
    let created = runs[0]["created"].as_array().unwrap().len();
    let skipped = runs[0]["skipped_seen"].as_u64().unwrap();
    assert_eq!(
        created as u64 + skipped,
        1,
        "the one item is either newly created or already claimed by the background tick: {runs:?}"
    );

    let out = env.cmd(&["job", "disable", "tick"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let file = std::fs::read_to_string(env.config.join("jobs/tick.toml")).unwrap();
    assert!(file.contains("enabled = false\n"), "{file}");
    let out = env.cmd(&["job", "list", "--json"]);
    let jobs: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        jobs[0]["enabled"], false,
        "disable reloads the daemon at once"
    );

    let out = env.cmd(&["job", "run", "tick"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("started"));

    let out = env.cmd(&["job", "run", "ghost"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("job_not_found"));

    // `job_path` joins the raw name under the jobs dir; an unvalidated name
    // like "../pastor" would resolve to config/pastor.toml, which exists, so
    // this must be rejected by name before any existence check runs.
    let pastor_toml_before = std::fs::read_to_string(env.config.join("pastor.toml")).unwrap();
    let out = env.cmd(&["job", "disable", "../pastor"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("job_not_found"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(env.config.join("pastor.toml")).unwrap(),
        pastor_toml_before,
        "a path-traversal job name must not touch pastor.toml"
    );
}

#[test]
fn tick_without_daemon_queues_tasks_for_later() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(config.join("jobs")).unwrap();
    std::fs::write(
        config.join("jobs/tick.toml"),
        "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"p {{ task.id }}\"\n",
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
    // No daemon and no background scheduler process exists here, so unlike
    // the e2e test above a dry run is not racing anything: it must create
    // exactly the one item and write nothing to the store.
    let out = run(&["tick", "--dry-run", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let runs: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(runs[0]["outcome"], "dry_run");
    assert_eq!(runs[0]["created"].as_array().unwrap().len(), 1);
    let out = run(&["task", "list", "--json"]);
    let tasks: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert!(tasks.is_empty(), "--dry-run must write nothing: {tasks:?}");

    let out = run(&["tick", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("not running"));
    let runs: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(runs[0]["outcome"], "ran");
    let out = run(&["task", "list", "--json"]);
    let tasks: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["state"], "queued");
    assert_eq!(tasks[0]["prompt"], "p t-1");
    let out = run(&["job", "list"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("ok: 1 items, 1 tasks"));
}

/// Offline `tick` takes each task's flock from flock.toml. One that does not
/// load (here, two defaults) is refused, as the head refuses to start on it,
/// rather than read as the implicit `default` flock, which would queue the
/// job's task for machines the file never put it on.
#[test]
fn tick_without_daemon_refuses_a_flock_file_that_does_not_load() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(config.join("jobs")).unwrap();
    std::fs::write(
        config.join("jobs/tick.toml"),
        "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"p\"\n",
    )
    .unwrap();
    std::fs::write(
        config.join("flock.toml"),
        "[[flock]]\nname = \"work\"\ndefault = true\n[[flock]]\nname = \"home\"\ndefault = true\n",
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
    assert_eq!(error_code(&run(&["tick", "--json"])), "runtime_error");
    std::fs::remove_file(config.join("flock.toml")).unwrap();
    let out = run(&["task", "list", "--all", "--json"]);
    let tasks: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert!(tasks.is_empty(), "nothing queued: {tasks:?}");
}

#[test]
fn job_describe_enable_and_disable_go_through_a_head() {
    let env = start_with_jobs(&[("nightly", NIGHTLY)]);
    let l = laptop(&env);
    // The laptop has no job file: only the head knows the job.
    std::fs::remove_file(l.config.join("jobs/nightly.toml")).unwrap();
    let j: serde_json::Value =
        serde_json::from_str(&ok(l.cmd(&["job", "describe", "nightly", "--json"]))).unwrap();
    assert_eq!(j["name"], "nightly");
    assert_eq!(j["schedule"], "every 1h");
    assert_eq!(j["connector"]["use"], "clock");
    assert!(
        j["file"]
            .as_str()
            .unwrap()
            .starts_with(env.config.to_str().unwrap()),
        "{j}"
    );
    let text = ok(l.cmd(&["job", "describe", "nightly"]));
    assert!(text.contains("every 1h"), "{text}");
    assert_eq!(
        error_code(&l.cmd(&["job", "describe", "ghost"])),
        "job_not_found"
    );

    let head_job = env.config.join("jobs/nightly.toml");
    let text = ok(l.cmd(&["job", "disable", "nightly"]));
    assert!(text.contains("disabled nightly"), "{text}");
    assert!(
        std::fs::read_to_string(&head_job)
            .unwrap()
            .contains("enabled = false")
    );
    let jobs: Vec<serde_json::Value> =
        serde_json::from_str(&ok(env.cmd(&["job", "list", "--json"]))).unwrap();
    assert_eq!(jobs[0]["enabled"], false);
    ok(l.cmd(&["job", "enable", "nightly"]));
    assert!(
        std::fs::read_to_string(&head_job)
            .unwrap()
            .contains("enabled = true")
    );
    assert!(!l.config.join("jobs/nightly.toml").exists());
    assert_eq!(
        error_code(&l.cmd(&["job", "enable", "ghost"])),
        "job_not_found"
    );
}

/// With a head set, `pastor serve` runs headless: this machine's jobs run
/// here, their tasks go to the head, and this machine's hooks hear the
/// head's events. It keeps job state in its own small database, answers
/// only its own job requests on the local socket, and a head refuses to
/// start beside it.
#[test]
fn a_headless_serve_runs_its_jobs_through_the_head() {
    let env = start();
    let c = client(Some(&env));
    let notify =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/connector/notify");
    ok(c.cmd(&["connector", "link", notify.to_str().unwrap()]));
    ok(c.head_set("head-up", &[]));
    std::fs::create_dir_all(c.config.join("jobs")).unwrap();
    std::fs::write(c.config.join("pastor.toml"), "tick = \"1s\"\n").unwrap();
    std::fs::write(
        c.config.join("jobs/sweep.toml"),
        "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nrepo = \"/tmp\"\nprompt = \"sweep {{ item.key }} for {{ job.name }} as {{ task.id }}\"\n",
    )
    .unwrap();
    let mut serve = c.serve();
    serve.wait_log("headless");

    // The job ran here and its task is the head's, rendered with the
    // head's id.
    let deadline = Instant::now() + WAIT;
    let task = loop {
        let out = ok(env.cmd(&["task", "list", "--all", "--json"]));
        let tasks: serde_json::Value = serde_json::from_str(&out).unwrap();
        if let Some(t) = tasks
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["job"] == "sweep")
        {
            break t.clone();
        }
        assert!(
            Instant::now() < deadline,
            "no task reached the head:\n{}",
            serve.log()
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let id = task["agent_name"].as_str().unwrap_or("t-1").to_string();
    assert!(
        task["prompt"]
            .as_str()
            .unwrap()
            .ends_with(&format!("for sweep as {id}")),
        "{task}"
    );
    env.wait_done(&id);

    // The head's task.done reached this machine's hook.
    let heard = c.state.join("connectors/@notify/notify.jsonl");
    let deadline = Instant::now() + WAIT;
    while !std::fs::read_to_string(&heard)
        .unwrap_or_default()
        .contains("sweep")
    {
        assert!(
            Instant::now() < deadline,
            "the hook never heard:\n{}",
            serve.log()
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // Its own small database, never the head's task store.
    assert!(c.state.join("shepherd.db").exists());
    assert!(!c.state.join("pastor.db").exists());

    let pong = c.local(r#"{"op":"ping"}"#);
    assert_eq!(pong["data"]["role"], "shepherd", "{pong}");
    let jobs = c.local(r#"{"op":"job_list"}"#);
    assert_eq!(jobs["data"][0]["name"], "sweep", "{jobs}");
    assert!(
        jobs["data"][0]["last_result"]
            .as_str()
            .unwrap()
            .starts_with("ok:"),
        "{jobs}"
    );
    let run = c.local(r#"{"op":"job_run","name":"ghost"}"#);
    assert_eq!(run["data"]["code"], "job_not_found", "{run}");
    let list = c.local(r#"{"op":"list","filter":{}}"#);
    assert_eq!(list["data"]["code"], "shepherd_unsupported", "{list}");

    // `head set` at this machine is refused, --force or not: it answers,
    // and is not a head.
    let other = client_to(Some((&c.config, &c.state)));
    assert_eq!(
        error_code(&other.head_set("head-up", &[])),
        "shepherd_running"
    );
    assert_eq!(
        error_code(&other.head_set("head-up", &["--force"])),
        "shepherd_running"
    );
    assert!(!other.config.join("client.toml").exists());

    // A second serve, headless or head, refuses the socket.
    assert_eq!(error_code(&c.cmd(&["serve"])), "shepherd_running");
    ok(c.cmd(&["head", "unset"]));
    std::fs::write(
        c.config.join("flock.toml"),
        "[[machine]]\nname = \"m\"\nlocal = true\n",
    )
    .unwrap();
    assert_eq!(error_code(&c.cmd(&["serve"])), "shepherd_running");
    assert!(serve.child.try_wait().unwrap().is_none(), "{}", serve.log());
}

/// A head and a shepherd, each with its own dirs, the shepherd reaching the
/// head through the fake ssh: the task pinned to the shepherd's pull
/// machine waits on the head until the shepherd claims it, runs on the
/// shepherd's herdr, and the head's row and events follow it to done, with
/// the machine named, as for a machine the head runs itself.
#[test]
fn a_shepherd_runs_the_task_pinned_to_it_and_the_head_sees_it() {
    let env = head_with_pull_machine("");
    let tmp = tempfile::tempdir().unwrap();
    let (_herdr, command) = fake_herdr_at(
        &tmp.path().join("laptop.sock"),
        &[("FAKE_HERDR_AUTO_DONE_MS", "300")],
    );
    let t: serde_json::Value = serde_json::from_str(&ok(env.cmd(&[
        "task",
        "run",
        "hello",
        "--machine",
        "laptop",
        "--repo",
        "/tmp",
        "--json",
    ])))
    .unwrap();
    assert_eq!(t["state"], "queued", "{t}");
    let id = format!("t-{}", t["id"]);

    let (_c, serve) = pull_shepherd(&env, &command);
    let running = wait_pulled(&env, &id, "done", &serve);
    assert_eq!(running["machine"], "laptop", "{running}");
    assert!(running["pane_id"].is_string(), "{running}");

    // `wait_pulled` above only waits for the task's row to read `done`; the
    // event that says so is queued separately (`Actor::send_event`) and can
    // still be in flight, so wait for it too instead of reading the log once.
    let out = env.wait_for("task.done", &["events", "--task", &id, "--json"], |text| {
        text.contains("\"task.done\"")
    });
    let events: Vec<(String, String)> = out
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .map(|e| {
            (
                e["type"].as_str().unwrap().to_string(),
                e["task"]["machine"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect();
    let lap = |k: &str| (k.to_string(), "laptop".to_string());
    assert_eq!(
        events,
        [
            ("task.queued".to_string(), String::new()),
            lap("task.running"),
            lap("task.done")
        ],
        "{out}"
    );
    let machines = ok(env.cmd(&["machine", "list", "--json"]));
    let laptop = serde_json::from_str::<serde_json::Value>(&machines).unwrap()["machines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "laptop")
        .cloned()
        .unwrap();
    assert_eq!(laptop["channel"], "connected", "{laptop}");
}

/// A pull machine that stops asking is lost after `pull_lost_after`: its
/// running task goes stale on the head, with the reason.
#[test]
fn a_silent_pull_machine_s_tasks_go_stale() {
    let env = head_with_pull_machine("pull_lost_after = \"3s\"\n");
    let tmp = tempfile::tempdir().unwrap();
    let (_herdr, command) = fake_herdr_at(&tmp.path().join("laptop.sock"), &[]);
    let t: serde_json::Value = serde_json::from_str(&ok(env.cmd(&[
        "task",
        "run",
        "hello",
        "--machine",
        "laptop",
        "--repo",
        "/tmp",
        "--json",
    ])))
    .unwrap();
    let id = format!("t-{}", t["id"]);
    let (_c, serve) = pull_shepherd(&env, &command);
    wait_pulled(&env, &id, "running", &serve);
    // The shepherd stops: nothing claims or reports for the machine now.
    drop(serve);
    let deadline = Instant::now() + WAIT;
    let t = loop {
        let t = task_state(&env, &id);
        if t["state"] == "stale" {
            break t;
        }
        assert!(Instant::now() < deadline, "never stale: {t}");
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(
        t["error"]
            .as_str()
            .unwrap()
            .contains("has not claimed or reported"),
        "{t}"
    );
}

/// A head this machine cannot reach is a warning, not a reason to stop:
/// the headless serve keeps running and asks again each tick.
#[test]
fn a_headless_serve_waits_for_an_unreachable_head() {
    let c = client(None);
    ok(c.head_set("unreachable", &["--force"]));
    std::fs::create_dir_all(&c.config).unwrap();
    std::fs::write(c.config.join("pastor.toml"), "tick = \"1s\"\n").unwrap();
    let mut serve = c.serve();
    serve.wait_log("shepherd_needs_head");
    serve.wait_log("headless");
    let pong = c.local(r#"{"op":"ping"}"#);
    assert_eq!(pong["data"]["role"], "shepherd", "{pong}");
}

/// A head that holds this machine's socket keeps a headless serve from
/// starting.
#[test]
fn a_headless_serve_refuses_a_running_head() {
    let env = start();
    let c = Client {
        config: env.config.clone(),
        state: env.state.clone(),
        ..client(Some(&env))
    };
    let out = c.cmd(&["--head", "head-up", "serve"]);
    assert_eq!(error_code(&out), "head_running");
}

/// With a head set, `job list` shows the head's jobs and this machine's in
/// two tables, a side with none saying so, and `--json` one flat array whose
/// jobs say `where` they live. `job run|enable|disable|describe|edit` go to
/// wherever the job's file is: here, or the head.
#[test]
fn a_shepherd_lists_and_drives_its_jobs_and_the_head_s() {
    use std::os::unix::fs::PermissionsExt;
    let env = start_with_jobs(&[("nightly", NIGHTLY)]);
    let head_job = env.config.join("jobs/nightly.toml");
    let c = client(Some(&env));
    ok(c.head_set("head-up", &[]));

    let out = c.cmd(&["job", "list"]);
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let sections = job_sections(&ok(out));
    assert_eq!(sections.len(), 2, "{sections:?}");
    assert_eq!(sections[0].0, "head: head-up");
    assert!(sections[0].1.starts_with("NAME"), "{sections:?}");
    assert!(sections[0].1.contains("nightly"), "{sections:?}");
    assert!(sections[1].0.starts_with("shepherd: "), "{sections:?}");
    assert!(sections[1].0.ends_with(" (this machine)"), "{sections:?}");
    assert_eq!(sections[1].1, "no jobs");
    assert!(stderr.contains("not running"), "{stderr}");

    std::fs::create_dir_all(c.config.join("jobs")).unwrap();
    let here_job = c.config.join("jobs/sweep.toml");
    std::fs::write(&here_job, SWEEP).unwrap();
    let sections = job_sections(&ok(c.cmd(&["job", "list"])));
    assert!(!sections[0].1.contains("sweep"), "{sections:?}");
    assert!(sections[1].1.starts_with("NAME"), "{sections:?}");
    assert!(sections[1].1.contains("sweep"), "{sections:?}");
    assert!(!sections[1].1.contains("nightly"), "{sections:?}");
    let jobs: Vec<serde_json::Value> =
        serde_json::from_str(&ok(c.cmd(&["job", "list", "--json"]))).unwrap();
    let wheres: Vec<(&str, &str)> = jobs
        .iter()
        .map(|j| (j["name"].as_str().unwrap(), j["where"].as_str().unwrap()))
        .collect();
    assert_eq!(wheres, [("nightly", "head"), ("sweep", "shepherd")]);

    // Each job where its file is, with no serve here.
    let text = ok(c.cmd(&["job", "disable", "sweep"]));
    assert!(text.contains("disabled sweep"), "{text}");
    assert!(
        std::fs::read_to_string(&here_job)
            .unwrap()
            .contains("enabled = false")
    );
    let text = ok(c.cmd(&["job", "disable", "nightly"]));
    assert!(text.contains("disabled nightly"), "{text}");
    assert!(
        std::fs::read_to_string(&head_job)
            .unwrap()
            .contains("enabled = false")
    );
    assert!(!c.config.join("jobs/nightly.toml").exists());
    let d: serde_json::Value =
        serde_json::from_str(&ok(c.cmd(&["job", "describe", "sweep", "--json"]))).unwrap();
    assert!(
        d["file"]
            .as_str()
            .unwrap()
            .starts_with(c.config.to_str().unwrap()),
        "{d}"
    );
    assert_eq!(d["enabled"], false, "{d}");
    let d: serde_json::Value =
        serde_json::from_str(&ok(c.cmd(&["job", "describe", "nightly", "--json"]))).unwrap();
    assert!(
        d["file"]
            .as_str()
            .unwrap()
            .starts_with(env.config.to_str().unwrap()),
        "{d}"
    );
    assert_eq!(
        error_code(&c.cmd(&["job", "describe", "ghost"])),
        "job_not_found"
    );
    // A job here runs in this machine's serve, which is down.
    let out = c.cmd(&["job", "run", "sweep"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not running"));

    let editor = c.tmp.path().join("ed.sh");
    std::fs::write(&editor, "#!/bin/sh\necho '# edited' >> \"$1\"\n").unwrap();
    std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o755)).unwrap();
    let text = ok(c.edit(&editor, &["job", "edit", "sweep"]));
    assert!(text.contains("saved"), "{text}");
    assert!(
        std::fs::read_to_string(&here_job)
            .unwrap()
            .ends_with("# edited\n")
    );
    assert!(
        !std::fs::read_to_string(&head_job)
            .unwrap()
            .contains("# edited")
    );

    // With this machine's serve up, its jobs come from it.
    let mut serve = c.serve();
    serve.wait_log("headless");
    let out = c.cmd(&["job", "list", "--json"]);
    assert!(!String::from_utf8_lossy(&out.stderr).contains("not running"));
    let jobs: Vec<serde_json::Value> = serde_json::from_str(&ok(out)).unwrap();
    assert_eq!(jobs[1]["name"], "sweep", "{jobs:?}");
    assert_eq!(jobs[1]["where"], "shepherd", "{jobs:?}");
    let text = ok(c.cmd(&["job", "enable", "sweep"]));
    assert!(text.contains("enabled sweep"), "{text}");
    assert!(text.contains("picked it up"), "{text}");
    let jobs: Vec<serde_json::Value> =
        serde_json::from_str(&ok(c.cmd(&["job", "list", "--json"]))).unwrap();
    assert_eq!(jobs[1]["enabled"], true, "{jobs:?}");
    ok(c.cmd(&["job", "run", "sweep"]));
    ok(c.cmd(&["job", "enable", "nightly"]));
    assert!(
        std::fs::read_to_string(&head_job)
            .unwrap()
            .contains("enabled = true")
    );
    let text = ok(c.edit(&editor, &["job", "edit", "sweep"]));
    assert!(text.contains("picked it up"), "{text}");
    assert!(serve.child.try_wait().unwrap().is_none(), "{}", serve.log());
}
