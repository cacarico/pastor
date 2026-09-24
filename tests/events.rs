//! `pastor events` against a log on disk, with no daemon running.
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::Duration;

use chrono::Utc;
use pastor::events::{DEFAULT_MAX_BYTES, EventRecord, LogWriter};
use pastor::store::{NewTask, Store};
use pastor::task::{DispatchSpec, Task};

fn pastor(state: &std::path::Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_pastor"));
    c.env("PASTOR_CONFIG_DIR", state.join("c"))
        .env("PASTOR_STATE_DIR", state.join("s"));
    c
}

fn task(id: i64) -> Task {
    let store = Store::open_in_memory().unwrap();
    let mut t = store
        .insert_task(NewTask {
            job: "triage".into(),
            item: serde_json::json!({"key": "k"}),
            prompt: "p".into(),
            spec: DispatchSpec {
                agent: "claude".into(),
                agent_args: vec![],
                repo: None,
                worktree: false,
                branch: None,
                machine: None,
                tags: vec![],
                timeout_secs: 60,
            },
        })
        .unwrap();
    t.id = id;
    t
}

fn record(kind: &str, t: Option<&Task>, job: Option<&str>) -> EventRecord {
    EventRecord {
        at: Utc::now(),
        kind: kind.into(),
        task: t.cloned(),
        job: job.map(Into::into),
        machine: None,
    }
}

fn setup() -> (tempfile::TempDir, LogWriter) {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("s")).unwrap();
    let w = LogWriter::new(tmp.path().join("s/events.jsonl"), DEFAULT_MAX_BYTES);
    let (t1, t2) = (task(1), task(2));
    w.append(&record("task.queued", Some(&t1), Some("triage")))
        .unwrap();
    w.append(&record("task.queued", Some(&t2), Some("triage")))
        .unwrap();
    w.append(&record("job.failed", None, Some("nightly")))
        .unwrap();
    w.append(&record("task.done", Some(&t1), Some("triage")))
        .unwrap();
    (tmp, w)
}

fn stdout(out: std::process::Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn events_prints_the_log_without_a_daemon() {
    let (tmp, _w) = setup();
    let out = stdout(pastor(tmp.path()).arg("events").output().unwrap());
    let lines: Vec<_> = out.lines().collect();
    assert_eq!(lines.len(), 4, "{out}");
    assert!(lines[0].contains("task.queued") && lines[0].contains("t-1"));
    assert!(lines[2].contains("job.failed") && lines[2].contains("job=nightly"));
}

#[test]
fn events_filters_by_task_and_prints_json() {
    let (tmp, _w) = setup();
    let out = stdout(
        pastor(tmp.path())
            .args(["events", "--task", "t-1", "--json"])
            .output()
            .unwrap(),
    );
    let kinds: Vec<String> = out
        .lines()
        .map(|l| {
            serde_json::from_str::<serde_json::Value>(l).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(kinds, ["task.queued", "task.done"]);
}

#[test]
fn events_with_no_log_prints_nothing_and_a_bad_task_id_is_a_json_error() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(
        stdout(pastor(tmp.path()).arg("events").output().unwrap()),
        ""
    );
    let out = pastor(tmp.path())
        .args(["events", "--task", "nope"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["code"], "runtime_error");
}

#[test]
fn events_follow_streams_new_records() {
    let (tmp, w) = setup();
    struct Kill(std::process::Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Kill(
        pastor(tmp.path())
            .args(["events", "--follow", "--task", "2"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut lines = BufReader::new(child.0.stdout.take().unwrap()).lines();
    assert!(lines.next().unwrap().unwrap().contains("task.queued"));
    std::thread::sleep(Duration::from_millis(300));
    w.append(&record("task.blocked", Some(&task(2)), Some("triage")))
        .unwrap();
    let line = lines.next().unwrap().unwrap();
    assert!(
        line.contains("task.blocked") && line.contains("t-2"),
        "{line}"
    );
}
