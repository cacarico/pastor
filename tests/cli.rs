//! Drives the real binaries: pastor serve with a fake-herdr machine, then run/list/task.
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
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();
    // Fast tick/settle/reconcile so the test doesn't wait out the 60s defaults.
    std::fs::write(
        config.join("pastor.toml"),
        "tick = \"1s\"\nsettle = \"1s\"\nreconcile_every = \"1s\"\n",
    )
    .unwrap();
    // A herdr connection carries one request, so the `command` transport spawns a
    // bridge process per request. The state has to outlive those: `fake-herdr
    // --listen` is the server (herdr's role) and `--connect` is the bridge
    // (`remote-api-bridge`'s role), which is exactly the shape pastor talks to
    // over ssh.
    let socket = tmp.path().join("herdr.sock");
    let herdr = Command::new(env!("CARGO_BIN_EXE_fake-herdr"))
        .arg("--listen")
        .arg(&socket)
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
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let out = env.cmd(&["flock", "list", "--json"]);
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
    let out = env.cmd(&["run", "say hello\nthen stop", "--repo", "/tmp", "--json"]);
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
    let out = env.cmd(&["list"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("t-1") && text.contains("done"), "{text}");

    let out = env.cmd(&["task", "show", "t-9"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("task_not_found"));
}

#[test]
fn flock_add_and_remove_edit_the_file() {
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
        "flock",
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
    let out = run(&["flock", "add", "pi-3", "fleet@pi-3"]);
    assert!(!out.status.success());
    let out = run(&["flock", "list"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("pi-3"));
    assert!(run(&["flock", "remove", "pi-3"]).status.success());
    assert!(!run(&["flock", "remove", "pi-3"]).status.success());
}

#[test]
fn list_without_daemon_reads_the_database() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    let out = pastor()
        .args(["list"])
        .env("PASTOR_CONFIG_DIR", &config)
        .env("PASTOR_STATE_DIR", &state)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("no tasks"));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not running"));
}
