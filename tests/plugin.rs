//! Connector protocol tests with real child processes: the shell fixtures in
//! tests/fixtures/plugin/ emit items, cursors and bad lines, and fail, hang or
//! crash on demand.
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{TimeZone, Utc};
use pastor::config::Paths;
use pastor::connector::process::{self, StreamSource};
use pastor::connector::{ItemSource, RunInput};
use pastor::plugin::{Discovered, Plugin, discover};
use serde_json::json;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/plugin")
        .join(name)
}

struct Env {
    _tmp: tempfile::TempDir,
    paths: Paths,
}

/// Temp config/state/data dirs with the named fixtures linked in.
fn env_with(fixtures: &[&str]) -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let paths =
        Paths::new(tmp.path().join("c"), tmp.path().join("s")).with_data_dir(tmp.path().join("d"));
    std::fs::create_dir_all(paths.plugins_dir()).unwrap();
    for f in fixtures {
        std::os::unix::fs::symlink(fixture(f), paths.plugins_dir().join(f)).unwrap();
    }
    Env { _tmp: tmp, paths }
}

impl Env {
    fn plugin(&self, id: &str) -> Arc<Plugin> {
        match discover(&self.paths)
            .unwrap()
            .into_iter()
            .find(|d| d.id() == id)
            .unwrap()
        {
            Discovered::Valid(p) => Arc::from(p),
            other => panic!("{other:?}"),
        }
    }

    fn dotenv(&self, id: &str, text: &str) {
        let f = self.paths.plugin_env_file(id);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, text).unwrap();
    }

    fn logs(&self, job: &str) -> Vec<String> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(self.paths.runs_dir(job))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        files.sort();
        files
            .iter()
            .map(|p| std::fs::read_to_string(p).unwrap())
            .collect()
    }
}

fn input(config: serde_json::Value, cursor: Option<&str>) -> RunInput {
    let at = Utc.with_ymd_and_hms(2026, 9, 23, 9, 0, 0).unwrap();
    RunInput {
        config,
        cursor: cursor.map(str::to_string),
        since: at,
        now: at,
    }
}

#[tokio::test]
async fn a_poll_connector_gets_the_handshake_and_its_lines_are_parsed() {
    let env = env_with(&["echo"]);
    env.dotenv("echo", "FIXTURE_TOKEN=tok-sekrit-42\n");
    let src = process::source(
        env.plugin("echo"),
        env.paths.clone(),
        Some("support".into()),
    );
    assert_eq!(src.id(), "echo");
    let out = src
        .run(input(json!({"channel": "C0123"}), Some("c-0")))
        .await
        .unwrap();

    let keys: Vec<&str> = out.items.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(keys, vec!["k1", "k2", "k1"], "dedup is the scheduler's job");
    assert_eq!(out.items[0].fields["author"], "ana");
    assert_eq!(out.items[1].fields["extra"]["n"], 2);
    assert_eq!(out.cursor.as_deref(), Some("c-2"), "the last cursor wins");

    let handshake = &out.logs[0];
    assert!(
        handshake.starts_with("info: handshake ")
            && handshake.contains(r#""config":{"channel":"C0123"}"#)
            && handshake.contains(r#""cursor":"c-0""#)
            && handshake.contains(r#""since":"2026-09-23T09:00:00Z""#),
        "{handshake}"
    );
    let cwd = std::fs::canonicalize(fixture("echo")).unwrap();
    assert_eq!(
        out.logs[1],
        format!("debug: env echo support {}", cwd.display())
    );
    let skipped: Vec<&String> = out.logs.iter().filter(|l| l.starts_with("warn:")).collect();
    assert_eq!(skipped.len(), 3, "{:?}", out.logs);
    assert!(skipped[0].contains("line 4") && skipped[0].contains("not JSON"));

    let logs = env.logs("support");
    assert_eq!(logs.len(), 1);
    assert!(
        logs[0].contains("token is [redacted:FIXTURE_TOKEN]"),
        "{}",
        logs[0]
    );
    assert!(!logs[0].contains("tok-sekrit-42"));
    assert!(logs[0].contains("[pastor: skipped stdout line 4: not JSON"));
    assert!(logs[0].ends_with("[pastor: exit 0]\n"), "{}", logs[0]);
    assert!(env.paths.plugin_state_dir("support").is_dir());
}

#[tokio::test]
async fn a_failing_poll_is_an_error_and_its_output_is_discarded() {
    let env = env_with(&["echo"]);
    env.dotenv("echo", "FIXTURE_MODE=fail\n");
    let src = process::source(env.plugin("echo"), env.paths.clone(), Some("j".into()));
    let err = src.run(input(json!({}), None)).await.unwrap_err();
    assert!(
        err.starts_with("exit 4: failing on purpose (log: "),
        "{err}"
    );
    assert!(err.contains("/runs/j/"), "{err}");
    assert!(env.logs("j")[0].contains("failing on purpose"));
}

#[tokio::test]
async fn a_hanging_poll_times_out() {
    let env = env_with(&["echo"]);
    env.dotenv("echo", "FIXTURE_MODE=hang\n");
    let src = process::source(env.plugin("echo"), env.paths.clone(), Some("j".into()));
    let started = Instant::now();
    let err = src.run(input(json!({}), None)).await.unwrap_err();
    assert!(err.starts_with("timed out after 5s"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(9));
}

#[tokio::test]
async fn a_broken_env_file_fails_the_run() {
    let env = env_with(&["echo"]);
    env.dotenv("echo", "not an assignment\n");
    let src = process::source(env.plugin("echo"), env.paths.clone(), Some("j".into()));
    let err = src.run(input(json!({}), None)).await.unwrap_err();
    assert!(err.contains(".env") && err.contains("line 1"), "{err}");
}

#[tokio::test]
async fn without_a_job_logs_go_under_the_plugin_id() {
    let env = env_with(&["echo"]);
    let src = process::source(env.plugin("echo"), env.paths.clone(), None);
    let out = src.run(input(json!({}), None)).await.unwrap();
    assert!(
        out.logs[1].starts_with("debug: env echo none "),
        "{:?}",
        out.logs
    );
    assert_eq!(env.logs("@echo").len(), 1);
}

async fn drain_until(
    src: &dyn ItemSource,
    cfg: &serde_json::Value,
    want: usize,
) -> (Vec<String>, Vec<Option<String>>, Vec<String>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut keys = Vec::new();
    let mut cursors = Vec::new();
    let mut logs = Vec::new();
    while keys.len() < want {
        assert!(Instant::now() < deadline, "only got {keys:?}");
        if let Ok(out) = src.run(input(cfg.clone(), Some("cur-0"))).await {
            keys.extend(out.items.into_iter().map(|i| i.key));
            cursors.push(out.cursor);
            logs.extend(out.logs);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    (keys, cursors, logs)
}

#[tokio::test]
async fn a_stream_stays_up_and_each_run_drains_what_it_emitted() {
    let env = env_with(&["stream"]);
    let src = process::source(env.plugin("stream"), env.paths.clone(), Some("s".into()));
    let cfg = json!({"room": "r1"});
    let (keys, cursors, logs) = drain_until(src.as_ref(), &cfg, 1).await;
    assert_eq!(keys, vec!["start-1"]);
    assert!(
        logs.iter()
            .any(|l| l.contains(r#""config":{"room":"r1"}"#) && l.contains(r#""cursor":"cur-0""#)),
        "the first start gets the job's cursor: {logs:?}"
    );
    assert!(cursors.contains(&Some("cur-1".into())));
    // Nothing new: an empty, successful run, and the process is not restarted.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let out = src.run(input(cfg.clone(), None)).await.unwrap();
    assert!(out.items.is_empty());
    let starts = std::fs::read_to_string(env.paths.plugin_state_dir("s").join("starts")).unwrap();
    assert_eq!(starts.trim(), "1");
    // A changed config restarts it, resuming from the newest cursor.
    let (keys, _, logs) = drain_until(src.as_ref(), &json!({"room": "r2"}), 1).await;
    assert_eq!(keys, vec!["start-2"]);
    assert!(
        logs.iter()
            .any(|l| l.contains(r#""config":{"room":"r2"}"#) && l.contains(r#""cursor":"cur-1""#)),
        "{logs:?}"
    );
    let logs = env.logs("s");
    assert!(logs.len() >= 2, "one log per start");
    drop(src);
}

#[tokio::test]
async fn a_crashing_stream_is_restarted_with_backoff() {
    let env = env_with(&["stream"]);
    env.dotenv("stream", "FIXTURE_MODE=crash\n");
    let src = StreamSource::with_backoff(
        env.plugin("stream"),
        env.paths.clone(),
        Some("s".into()),
        Duration::from_millis(100),
    );
    let cfg = json!({});
    let (keys, _, logs) = drain_until(&src, &cfg, 3).await;
    assert!(
        logs.iter()
            .any(|l| l.contains("start 3 handshake") && l.contains(r#""cursor":"cur-2""#)),
        "a restart resumes from the newest cursor: {logs:?}"
    );
    assert_eq!(&keys[..3], &["start-1", "start-2", "start-3"]);
    // Down, nothing buffered: the run fails, naming why.
    let deadline = Instant::now() + Duration::from_secs(10);
    let err = loop {
        match src.run(input(cfg.clone(), None)).await {
            Err(e) => break e,
            Ok(_) => {
                assert!(Instant::now() < deadline);
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    };
    assert!(
        err.starts_with("stream connector stopped: exit 1: crashing on purpose"),
        "{err}"
    );
    let logs = env.logs("s");
    assert!(logs.iter().any(|l| l.contains("crashing on purpose")));
}
