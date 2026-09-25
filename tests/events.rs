//! `pastor events` against a log on disk, with no daemon running.
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::Duration;

use chrono::Utc;
use pastor::events::{DEFAULT_MAX_BYTES, EventRecord, LogWriter};
use pastor::store::{NewTask, Store};
use pastor::task::{DispatchSpec, Task};

/// How long a test waits for the daemon or the fake herdr to do something.
/// Generous on purpose: a CI runner under load has taken more than 10s to
/// bring a daemon up, and a wait that ends early only ever fails a good run.
const WAIT: Duration = Duration::from_secs(60);

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
                allow: vec![],
                deny: vec![],
                repo: None,
                worktree: false,
                branch: None,
                machine: None,
                tags: vec![],
                timeout_secs: 60,
                checkout: None,
                reopen: None,
                agent_source: None,
            },
            flock: "default".into(),
        })
        .unwrap();
    t.id = id;
    t
}

fn record(kind: &str, t: Option<&Task>, job: Option<&str>) -> EventRecord {
    EventRecord {
        detail: None,
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

/// The daemon's log task: a dispatched task lands in events.jsonl with its row
/// and job. (machine.connected only follows an announced outage, so the
/// machine record is covered by the unit tests in `events`.)
#[tokio::test]
async fn the_daemon_writes_the_events_log() {
    use std::sync::Arc;

    use pastor::config::flock::{Flock, MachineConfig};
    use pastor::config::{PastorConfig, Paths};
    use pastor::daemon::Daemon;
    use pastor::herdr::Connector;
    use pastor::herdr::fake::FakeHerdr;
    use pastor::ipc::{IpcRequest, IpcResponse};

    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
    let flock = Flock {
        flocks: vec![],
        machines: vec![MachineConfig {
            name: "m".into(),
            local: false,
            ssh: None,
            command: Some(vec!["fake".into()]),
            session: "default".into(),
            max_agents: 1,
            tags: vec![],
            flock: None,
            agent: None,
            agent_args: None,
        }],
    };
    let fake: Arc<dyn Connector> = Arc::new(FakeHerdr::new());
    let (daemon, listener) = Daemon::bind_and_start(
        paths.clone(),
        PastorConfig::default(),
        flock,
        pastor::scheduler::ConfigFingerprint::sample(&paths),
        Some(Arc::new(move |_m: &MachineConfig| fake.clone()) as pastor::daemon::ConnectorFactory),
    )
    .await
    .unwrap();
    let socket = daemon.socket_path();
    let serve = tokio::spawn(daemon.run_with_listener(listener));
    let deadline = std::time::Instant::now() + WAIT;
    loop {
        if let Ok(IpcResponse::Machines(ms)) =
            pastor::ipc::request(&socket, &IpcRequest::FlockList).await
            && ms.iter().all(|m| m.channel.accepts_dispatch())
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "machine never connected"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let log = paths.events_file();
    let wait_for = async |kind: &str| -> EventRecord {
        let deadline = std::time::Instant::now() + WAIT;
        loop {
            if let Some(r) = pastor::events::read(&log, None)
                .unwrap()
                .into_iter()
                .find(|r| r.kind == kind)
            {
                return r;
            }
            assert!(std::time::Instant::now() < deadline, "no {kind} in the log");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };

    let resp = pastor::ipc::request(
        &socket,
        &IpcRequest::Run {
            prompt: "hi".into(),
            spec: DispatchSpec {
                agent: "claude".into(),
                agent_args: vec![],
                allow: vec![],
                deny: vec![],
                repo: None,
                worktree: false,
                branch: None,
                machine: None,
                tags: vec![],
                timeout_secs: 60,
                checkout: None,
                reopen: None,
                agent_source: None,
            },
            flock: None,
            agent: None,
        },
    )
    .await
    .unwrap();
    let IpcResponse::Task(t) = resp else {
        panic!("{resp:?}")
    };
    let queued = wait_for("task.queued").await;
    assert_eq!(queued.task.unwrap().id, t.id);
    assert_eq!(queued.job.as_deref(), Some("run"));
    let running = wait_for("task.running").await;
    assert_eq!(running.task.unwrap().id, t.id);
    assert_eq!(running.job.as_deref(), Some("run"));
    serve.abort();
}
