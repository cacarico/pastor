//! Drives the real binaries: pastor serve with a fake-herdr machine, then run/list/task.
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn pastor() -> Command {
    Command::new(env!("CARGO_BIN_EXE_pastor"))
}

struct Env {
    _tmp: tempfile::TempDir,
    config: std::path::PathBuf,
    state: std::path::PathBuf,
    serve: std::process::Child,
    herdr: std::process::Child,
    /// Every request the fake herdr has received, as a JSON array
    /// (`FAKE_HERDR_REQUEST_LOG`).
    herdr_log: std::path::PathBuf,
}

impl Drop for Env {
    fn drop(&mut self) {
        for child in [&mut self.serve, &mut self.herdr] {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn start() -> Env {
    start_with_jobs(&[])
}

/// Like `start()`, but writes each `(name, text)` to `config/jobs/<name>.toml`
/// before spawning `pastor serve`, so the scheduler picks the jobs up at start.
fn start_with_jobs(jobs: &[(&str, &str)]) -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(config.join("jobs")).unwrap();
    // Fast tick/settle/reconcile so the test doesn't wait out the 60s defaults.
    std::fs::write(
        config.join("pastor.toml"),
        "tick = \"1s\"\nsettle = \"1s\"\nreconcile_every = \"1s\"\n",
    )
    .unwrap();
    for (name, text) in jobs {
        std::fs::write(config.join("jobs").join(format!("{name}.toml")), text).unwrap();
    }
    // A herdr connection carries one request, so the `command` transport spawns a
    // bridge process per request. The state has to outlive those: `fake-herdr
    // --listen` is the server (herdr's role) and `--connect` is the bridge
    // (`remote-api-bridge`'s role), which is exactly the shape pastor talks to
    // over ssh.
    let socket = tmp.path().join("herdr.sock");
    let herdr_log = tmp.path().join("herdr-requests.json");
    let herdr = Command::new(env!("CARGO_BIN_EXE_fake-herdr"))
        .arg("--listen")
        .arg(&socket)
        .env("FAKE_HERDR_REQUEST_LOG", &herdr_log)
        .env("FAKE_HERDR_AUTO_DONE_MS", "300")
        // Agents spend a moment launching, as they do under a real herdr, so
        // this run exercises dispatch's readiness wait and not just the happy
        // path where the agent is up the instant it is started.
        .env("FAKE_HERDR_READY_MS", "200")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::fs::write(
        config.join("flock.toml"),
        format!(
            "[[machine]]\nname = \"fake\"\ncommand = [\"{}\", \"--connect\", \"{}\"]\nmax_agents = 2\n",
            env!("CARGO_BIN_EXE_fake-herdr"),
            socket.display()
        ),
    )
    .unwrap();
    let serve = pastor()
        .args(["serve"])
        .env("PASTOR_CONFIG_DIR", &config)
        .env("PASTOR_STATE_DIR", &state)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let env = Env {
        _tmp: tmp,
        config,
        state,
        serve,
        herdr,
        herdr_log,
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let out = env.cmd(&["machine", "list", "--json"]);
        if out.status.success() && String::from_utf8_lossy(&out.stdout).contains("\"connected\"") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "daemon never reported the machine connected: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    env
}

impl Env {
    /// The `params` of the `agent.start` the fake herdr got for `agent`.
    fn agent_start_params(&self, agent: &str) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(text) = std::fs::read_to_string(&self.herdr_log)
                && let Ok(reqs) = serde_json::from_str::<Vec<serde_json::Value>>(&text)
                && let Some(r) = reqs
                    .iter()
                    .find(|r| r["method"] == "agent.start" && r["params"]["name"] == agent)
            {
                return r["params"].clone();
            }
            assert!(
                Instant::now() < deadline,
                "no agent.start for {agent} in {}",
                self.herdr_log.display()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn cmd(&self, args: &[&str]) -> std::process::Output {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &self.config)
            .env("PASTOR_STATE_DIR", &self.state)
            .output()
            .unwrap()
    }
}

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

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let out = env.cmd(&["task", "show", "t-1", "--json"]);
        let t: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        if t["state"] == "done" {
            break;
        }
        assert!(Instant::now() < deadline, "task never became done: {t}");
        std::thread::sleep(Duration::from_millis(100));
    }
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

    let out = env.cmd(&["task", "show", "t-9"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("task_not_found"));
}

/// `--agent-arg` reaches herdr's `agent.start` as `args`, in order, and shows
/// in `task show` and `task list --json`; without it, `[defaults] agent_args` does.
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
    assert_eq!(start["args"], want, "{start}");
    assert_eq!(start["kind"], "claude");

    let out = env.cmd(&["task", "show", "t-1"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("agent args: --model claude-opus-5-5"),
        "{text}"
    );
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
        start["args"],
        serde_json::json!(["--model", "claude-sonnet-5"]),
        "{start}"
    );
}

#[test]
fn machine_add_and_remove_edit_the_file() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .output()
            .unwrap()
    };
    let out = run(&[
        "machine",
        "add",
        "pi-3",
        "fleet@pi-3",
        "--max-agents",
        "3",
        "--tag",
        "fast",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = std::fs::read_to_string(config.join("flock.toml")).unwrap();
    assert!(
        text.contains("name = \"pi-3\"")
            && text.contains("ssh = \"fleet@pi-3\"")
            && text.contains("max_agents = 3"),
        "{text}"
    );
    let out = run(&["machine", "add", "pi-3", "fleet@pi-3"]);
    assert!(!out.status.success());
    let out = run(&["machine", "list"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("pi-3"));
    // Without a head the table cannot know anything live; both the note and the
    // ERROR column say so in pastor's own words rather than "daemon down".
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no head is running"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("no head running"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let out = run(&["machine", "status", "pi-4"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("\"unknown_machine\""),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // `--branch` only means something for a worktree; clap rejects it alone
    // before any daemon is asked.
    let out = run(&["task", "run", "hi", "--branch", "b"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(run(&["machine", "remove", "pi-3"]).status.success());
    assert!(!run(&["machine", "remove", "pi-3"]).status.success());
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

/// `--herdr` keeps herdr's saved-machine list in step with the flock: `add`
/// runs `herdr machine add`, `remove` resolves the label to a profile id via
/// `herdr machine list` and runs `herdr machine remove`. A fake `herdr` script
/// first on PATH records what pastor asked of it.
#[test]
fn herdr_flag_adds_and_removes_the_saved_machine_too() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = tmp.path().join("herdr.log");
    let script = format!(
        "#!/bin/sh\necho \"$@\" >> {log}\ncase \"$*\" in *boom*) echo 'herdr says no' >&2; exit 1;; esac\n\
         if [ \"$1 $2\" = \"machine list\" ]; then printf 'id-1\\tother\\tx@y\\tdefault\\tenabled\\nid-2\\tpi-3\\tfleet@pi-3\\tdefault\\tenabled\\n'; fi\n",
        log = log.display()
    );
    std::fs::write(bin.join("herdr"), script).unwrap();
    std::fs::set_permissions(bin.join("herdr"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PATH", &path)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .output()
            .unwrap()
    };
    let calls = || std::fs::read_to_string(&log).unwrap_or_default();

    let out = run(&[
        "machine",
        "add",
        "pi-3",
        "fleet@pi-3",
        "--session",
        "work",
        "--herdr",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        calls().trim(),
        "machine add fleet@pi-3 --label pi-3 --remote-session work"
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        !text.contains("to see it in your laptop"),
        "the hint is replaced by the action: {text}"
    );
    assert!(text.contains("saved in herdr"), "{text}");

    let out = run(&["machine", "remove", "pi-3", "--herdr"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = calls();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[1], "machine list");
    assert_eq!(
        lines[2], "machine remove id-2",
        "resolved by label, not by name"
    );
    assert!(
        !std::fs::read_to_string(config.join("flock.toml"))
            .unwrap()
            .contains("pi-3")
    );

    // Not saved in herdr under that label: the flock edit still happens, a
    // note says so, exit 0. herdr does know the same host under another label
    // (`other`, id-1), so the note names it with the command that removes it,
    // and pastor does not remove it on its own.
    assert!(run(&["machine", "add", "pi-9", "x@y"]).status.success());
    let out = run(&["machine", "remove", "pi-9", "--herdr"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no saved machine"), "{err}");
    assert!(
        err.contains("other") && err.contains("herdr machine remove id-1"),
        "the hint names the saved machine at the same target: {err}"
    );
    assert!(
        !calls()
            .lines()
            .any(|l| l.starts_with("machine remove id-") && l != "machine remove id-2")
    );

    // herdr failing: the flock edit is kept, the failure is reported with herdr's stderr, exit 1.
    let out = run(&["machine", "add", "boom", "fleet@boom", "--herdr"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("herdr_error") && err.contains("herdr says no"),
        "{err}"
    );
    assert!(
        std::fs::read_to_string(config.join("flock.toml"))
            .unwrap()
            .contains("name = \"boom\"")
    );
}

#[test]
fn setup_systemd_confirmation_accepts_and_installs() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    let xdg = tmp.path().join("xdg");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let calls = tmp.path().join("systemctl.log");
    std::fs::write(
        bin.join("systemctl"),
        format!("#!/bin/sh\necho \"$@\" >> {}\n", calls.display()),
    )
    .unwrap();
    std::fs::write(
        bin.join("loginctl"),
        "#!/bin/sh\nif [ \"$1\" = show-user ]; then echo Linger=yes; fi\n",
    )
    .unwrap();
    std::fs::set_permissions(
        bin.join("systemctl"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::fs::set_permissions(bin.join("loginctl"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let mut child = pastor()
        .args(["setup", "systemd"])
        .env("PATH", &path)
        .env("XDG_CONFIG_HOME", &xdg)
        .env("PASTOR_CONFIG_DIR", &config)
        .env("PASTOR_STATE_DIR", &state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(b"yes\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        xdg.join("systemd/user/pastor.service").exists(),
        "confirmation should install the unit"
    );
    let calls = std::fs::read_to_string(calls).unwrap();
    assert!(calls.contains("--user daemon-reload"), "{calls}");
    assert!(
        calls.contains("--user enable --now pastor.service"),
        "{calls}"
    );
}

#[test]
fn setup_systemd_confirmation_rejects_before_mutating() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    let xdg = tmp.path().join("xdg");
    let mut child = pastor()
        .args(["setup", "systemd"])
        .env("XDG_CONFIG_HOME", &xdg)
        .env("PASTOR_CONFIG_DIR", &config)
        .env("PASTOR_STATE_DIR", &state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.as_mut().unwrap().write_all(b"no\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("aborted"));
    assert!(
        !xdg.join("systemd/user/pastor.service").exists(),
        "rejection must not write the unit"
    );
    assert!(
        !config.exists() && !state.exists(),
        "rejection must happen before permission hardening mutates paths"
    );
}

#[test]
fn clock_job_creates_tasks_end_to_end() {
    let env = start_with_jobs(&[(
        "tick",
        "every = \"1s\"\n[connector]\nuse = \"clock\"\n[dispatch]\nrepo = \"/tmp\"\nprompt = \"clock {{ item.key }} for {{ job.name }} as {{ task.id }}\"\n",
    )]);
    let deadline = Instant::now() + Duration::from_secs(10);
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

/// `machine status` connects without the daemon, so nothing else has made the
/// state dir yet. The ssh ControlMaster socket directory must exist, private,
/// before ssh starts. A fake `ssh` first on PATH records that it did and fails
/// the way an unreachable host does, so no real ssh runs.
#[test]
fn machine_status_creates_the_ssh_dir_private_before_ssh_runs() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let seen = tmp.path().join("seen");
    let script = format!(
        "#!/bin/sh\n[ -d {ssh} ] && echo present > {seen}\necho 'no route to host' >&2\nexit 255\n",
        ssh = state.join("ssh").display(),
        seen = seen.display()
    );
    std::fs::write(bin.join("ssh"), script).unwrap();
    std::fs::set_permissions(bin.join("ssh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PATH", &path)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .output()
            .unwrap()
    };
    assert!(
        run(&["machine", "add", "pi-3", "fleet@pi-3"])
            .status
            .success()
    );
    let _ = std::fs::remove_dir_all(&state);

    let out = run(&["machine", "status", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&seen).unwrap_or_default().trim(),
        "present",
        "ssh ran before its ControlPath directory existed"
    );
    for dir in [state.clone(), state.join("ssh")] {
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{} is {mode:o}", dir.display());
    }
}
