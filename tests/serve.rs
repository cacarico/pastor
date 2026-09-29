//! `pastor serve` in the background, `serve stop` and `serve status`, with the
//! real binary. The flock's one machine is a command that exits at once, so
//! the head runs with it lost and nothing here talks to a herdr.
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, Instant};

mod common;

const WAIT: Duration = Duration::from_secs(60);

struct Env {
    _tmp: tempfile::TempDir,
    config: PathBuf,
    state: PathBuf,
}

impl Env {
    fn new() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("c");
        let state = tmp.path().join("s");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("flock.toml"),
            "[[machine]]\nname = \"m\"\ncommand = [\"false\"]\n",
        )
        .unwrap();
        Env {
            _tmp: tmp,
            config,
            state,
        }
    }

    fn cmd(&self, args: &[&str]) -> Output {
        let mut c = common::pastor();
        c.env("PASTOR_CONFIG_DIR", &self.config)
            .env("PASTOR_STATE_DIR", &self.state)
            .env("PASTOR_DATA_DIR", self.state.join("data"))
            .args(args);
        // `output()` waits for stdout and stderr to close as well as for the
        // process; a background head must not hold either open.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(c.output().unwrap());
        });
        rx.recv_timeout(WAIT)
            .unwrap_or_else(|_| panic!("pastor {args:?} did not return"))
    }

    fn status(&self) -> serde_json::Value {
        let out = self.cmd(&["serve", "status", "--json"]);
        assert!(out.status.success(), "{}", stderr(&out));
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

/// Whatever head a test left running is stopped with the test.
impl Drop for Env {
    fn drop(&mut self) {
        let out = self.cmd(&["serve", "status", "--json"]);
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout)
            && let Some(pid) = v["pid"].as_i64()
        {
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        }
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn error_code(out: &Output) -> String {
    assert_eq!(out.status.code(), Some(1), "{}", stderr(out));
    let v: serde_json::Value =
        serde_json::from_slice(&out.stderr).unwrap_or_else(|_| panic!("{}", stderr(out)));
    v["code"].as_str().unwrap().to_string()
}

fn alive(pid: i64) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn wait_gone(pid: i64) {
    let deadline = Instant::now() + WAIT;
    while alive(pid) {
        assert!(Instant::now() < deadline, "pid {pid} still running");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn log(state: &Path) -> String {
    std::fs::read_to_string(state.join("serve.log")).unwrap_or_default()
}

/// A bare `pastor serve` returns once the head answers, and the head keeps
/// running without it; `serve status` finds it, `serve stop` ends it.
#[test]
fn serve_starts_the_head_in_the_background_and_stop_ends_it() {
    let env = Env::new();
    let out = env.cmd(&["serve"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(said.contains("serve.log"), "{said}");

    let status = env.status();
    assert_eq!(status["running"], true, "{status}");
    assert_eq!(status["role"], "head", "{status}");
    assert_eq!(status["service"], serde_json::Value::Null, "{status}");
    assert_eq!(
        status["log"].as_str().unwrap(),
        env.state.join("serve.log").to_str().unwrap()
    );
    let pid = status["pid"].as_i64().unwrap();
    assert!(alive(pid));
    // The log is the head's own: tracing goes there, not to the caller.
    assert!(
        log(&env.state).contains("pastor serve"),
        "{}",
        log(&env.state)
    );

    let text = String::from_utf8_lossy(&env.cmd(&["serve", "status"]).stdout).into_owned();
    assert!(text.contains(&pid.to_string()), "{text}");
    assert!(text.contains("head"), "{text}");

    // A second one finds the first and says so, before it starts anything.
    assert_eq!(error_code(&env.cmd(&["serve"])), "head_running");

    let out = env.cmd(&["serve", "stop"]);
    assert!(out.status.success(), "{}", stderr(&out));
    wait_gone(pid);
    assert_eq!(
        error_code(&env.cmd(&["serve", "status"])),
        "not_running",
        "stopped"
    );
    // Stopping what is not running is not an error.
    let out = env.cmd(&["serve", "stop"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("not running"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// A head that cannot start fails the `pastor serve` that launched it with
/// its own error, not a timeout.
#[test]
fn serve_reports_why_a_background_head_did_not_start() {
    let env = Env::new();
    std::fs::write(env.config.join("flock.toml"), "").unwrap();
    let out = env.cmd(&["serve"]);
    assert_eq!(error_code(&out), "serve_failed");
    assert!(stderr(&out).contains("flock is empty"), "{}", stderr(&out));
    assert!(stderr(&out).contains("serve.log"), "{}", stderr(&out));
}

/// `--foreground` is today's `pastor serve`: it runs until a signal, and
/// `serve status` sees it too.
#[test]
fn serve_foreground_keeps_the_head_in_the_terminal() {
    let env = Env::new();
    let mut child = common::pastor()
        .env("PASTOR_CONFIG_DIR", &env.config)
        .env("PASTOR_STATE_DIR", &env.state)
        .env("PASTOR_DATA_DIR", env.state.join("data"))
        .args(["serve", "-f"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + WAIT;
    let status = loop {
        let out = env.cmd(&["serve", "status", "--json"]);
        if out.status.success() {
            break serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap();
        }
        assert!(child.try_wait().unwrap().is_none(), "serve -f exited");
        assert!(Instant::now() < deadline, "serve -f never answered");
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(status["pid"].as_i64().unwrap(), child.id() as i64);
    assert_eq!(status["log"], serde_json::Value::Null, "{status}");
    assert!(!env.state.join("serve.log").exists());
    // Reaped as it exits, the way its shell would, so stop sees it go.
    let reaped = std::thread::spawn(move || child.wait().unwrap());
    let out = env.cmd(&["serve", "stop"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let code = reaped.join().unwrap();
    assert!(code.success(), "{code:?}");
}

/// `serve status` and `serve stop` with nothing running.
#[test]
fn serve_status_without_a_head_is_not_running() {
    let env = Env::new();
    assert_eq!(error_code(&env.cmd(&["serve", "status"])), "not_running");
    assert_eq!(
        error_code(&env.cmd(&["serve", "status", "--json"])),
        "not_running"
    );
}

/// `--foreground` is for starting a head; it means nothing to stop or status.
#[test]
fn serve_foreground_does_not_combine_with_a_subcommand() {
    let env = Env::new();
    let out = env.cmd(&["serve", "--foreground", "status"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}

/// A head a service manager runs reports it, and `serve stop` leaves it to
/// that manager, which would only start it again.
#[test]
fn serve_stop_refuses_a_head_a_service_runs() {
    let env = Env::new();
    let out = env.cmd(&["serve"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let pid = env.status()["pid"].as_i64().unwrap();
    // What a head started by systemd writes at start.
    std::fs::write(
        env.state.join("serve.json"),
        format!("{{\"pid\":{pid},\"service\":\"systemd\",\"log\":null}}\n"),
    )
    .unwrap();
    assert_eq!(env.status()["service"], "systemd");
    let text = String::from_utf8_lossy(&env.cmd(&["serve", "status"]).stdout).into_owned();
    assert!(text.contains("service: systemd"), "{text}");
    let out = env.cmd(&["serve", "stop"]);
    assert_eq!(error_code(&out), "service_managed");
    assert!(
        stderr(&out).contains("pastor setup systemd --stop"),
        "{}",
        stderr(&out)
    );
    assert!(alive(pid));
    // A record for some other pid is not this head's.
    std::fs::write(
        env.state.join("serve.json"),
        "{\"pid\":1,\"service\":\"systemd\",\"log\":null}\n",
    )
    .unwrap();
    assert_eq!(env.status()["service"], serde_json::Value::Null);
    let out = env.cmd(&["serve", "stop"]);
    assert!(out.status.success(), "{}", stderr(&out));
    wait_gone(pid);
}
