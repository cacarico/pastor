//! `pastor watch` against a head this test scripts: events, tasks and jobs
//! it serves, a head that goes down and comes back, and connectors whose
//! `[watch]` command prints repeats or fails.
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pastor::events::{EventRecord, EventsPage};
use pastor::ipc::{IpcRequest, IpcResponse, parse_request_line};
use pastor::scheduler::JobStatus;
use pastor::store::{NewTask, Store};
use pastor::task::{DispatchSpec, Task, TaskState};

const WAIT: Duration = Duration::from_secs(30);

mod common;

fn pastor(config: &Path, state: &Path) -> Command {
    let mut c = common::pastor();
    c.env("PASTOR_CONFIG_DIR", config)
        .env("PASTOR_STATE_DIR", state)
        // Never the real data dir: connectors are linked there.
        .env("PASTOR_DATA_DIR", state.join("data"));
    c
}

/// What the scripted head serves.
#[derive(Default)]
struct Served {
    events: Vec<EventRecord>,
    tasks: Vec<Task>,
    jobs: Vec<JobStatus>,
}

/// A head on `socket` answering from `served`, until `stop`.
struct FakeHead {
    socket: PathBuf,
    served: Arc<Mutex<Served>>,
    running: Option<(Arc<AtomicBool>, std::thread::JoinHandle<()>)>,
}

impl FakeHead {
    fn new(socket: &Path) -> FakeHead {
        let mut h = FakeHead {
            socket: socket.to_path_buf(),
            served: Arc::default(),
            running: None,
        };
        h.start();
        h
    }

    fn start(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
        let listener = UnixListener::bind(&self.socket).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let (served, halt) = (self.served.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if halt.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(mut stream) = stream else { return };
                let mut line = String::new();
                let _ = BufReader::new(&stream).read_line(&mut line);
                let Ok((req, _)) = parse_request_line(&line) else {
                    continue;
                };
                let resp = answer(&served.lock().unwrap(), req);
                let mut out = serde_json::to_string(&resp).unwrap();
                out.push('\n');
                let _ = stream.write_all(out.as_bytes());
            }
        });
        self.running = Some((stop, thread));
    }

    /// Stop answering: the socket goes, as it does when `pastor serve` stops.
    fn stop(&mut self) {
        if let Some((stop, thread)) = self.running.take() {
            stop.store(true, Ordering::SeqCst);
            let _ = std::os::unix::net::UnixStream::connect(&self.socket);
            thread.join().unwrap();
            let _ = std::fs::remove_file(&self.socket);
        }
    }

    fn push(&self, kind: &str, task: &Task) {
        let mut s = self.served.lock().unwrap();
        let seq = s.events.last().map_or(1, |r| r.seq + 1);
        s.events.push(EventRecord {
            summary: None,
            seq,
            at: chrono::Utc::now(),
            kind: kind.into(),
            task: Some(task.clone()),
            model: None,
            job: Some(task.job.clone()),
            machine: None,
            detail: None,
        });
    }
}

impl Drop for FakeHead {
    fn drop(&mut self) {
        self.stop();
    }
}

fn answer(s: &Served, req: IpcRequest) -> IpcResponse {
    match req {
        IpcRequest::Ping => IpcResponse::Pong {
            version: "test".into(),
            protocol: pastor::ipc::IPC_PROTOCOL,
            role: None,
        },
        IpcRequest::EventsSince { after, limit, .. } => IpcResponse::Events(EventsPage {
            events: s
                .events
                .iter()
                .filter(|r| r.seq > after)
                .take(limit as usize)
                .cloned()
                .collect(),
            gap: false,
            oldest: s.events.first().map(|r| r.seq),
            newest: s.events.last().map(|r| r.seq),
        }),
        IpcRequest::List { filter } => IpcResponse::Tasks(
            s.tasks
                .iter()
                .filter(|t| {
                    filter
                        .states
                        .as_ref()
                        .is_none_or(|st| st.contains(&t.state))
                })
                .cloned()
                .collect(),
        ),
        IpcRequest::JobList => IpcResponse::Jobs(s.jobs.clone()),
        other => IpcResponse::Error {
            code: "unexpected".into(),
            message: format!("{other:?}"),
        },
    }
}

fn task(id: i64, state: TaskState) -> Task {
    let store = Store::open_in_memory().unwrap();
    let mut t = store
        .insert_task(NewTask {
            job: "nightly".into(),
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
                place: Default::default(),
                session_id: None,
                label: Default::default(),
                summary: Default::default(),
            },
            flock: "default".into(),
            description: None,
        })
        .unwrap();
    t.id = id;
    t.state = state;
    t.machine = Some("pi-1".into());
    t
}

fn job(name: &str, last: &str) -> JobStatus {
    JobStatus {
        name: name.into(),
        schedule: Some("every 1h".into()),
        enabled: true,
        connector: Some("clock".into()),
        error: None,
        last_run_at: None,
        last_result: Some(last.into()),
        next_due: None,
        running: false,
        flock: None,
        description: None,
    }
}

struct Dirs {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    config: PathBuf,
    state: PathBuf,
}

fn dirs() -> Dirs {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let (config, state) = (root.join("c"), root.join("s"));
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    Dirs {
        _tmp: tmp,
        root,
        config,
        state,
    }
}

/// A running `pastor watch`, its stdout lines on a channel.
struct Watcher {
    child: Child,
    lines: Receiver<String>,
}

impl Watcher {
    fn start(d: &Dirs, args: &[&str]) -> Watcher {
        let mut child = pastor(&d.config, &d.state)
            .arg("watch")
            .args(["--interval", "1s"])
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let out = child.stdout.take().unwrap();
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { return };
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        Watcher { child, lines }
    }

    /// Every line printed until one matching `until`, that one included.
    fn until(&self, until: impl Fn(&str) -> bool) -> Vec<String> {
        let deadline = Instant::now() + WAIT;
        let mut seen = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    let done = until(&line);
                    seen.push(line);
                    if done {
                        return seen;
                    }
                }
                Err(RecvTimeoutError::Timeout) => panic!("no such line; saw {seen:?}"),
                Err(RecvTimeoutError::Disconnected) => panic!("watch exited; saw {seen:?}"),
            }
        }
    }

    /// Whatever it prints within `wait`.
    fn quiet_for(&self, wait: Duration) -> Vec<String> {
        let deadline = Instant::now() + wait;
        let mut seen = Vec::new();
        while let Ok(line) = self
            .lines
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            seen.push(line);
        }
        seen
    }

    fn stop(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A new watcher starts at the end of the log; it prints the task states an
/// orchestrator acts on, a failing job once, the head going down and coming
/// back once each, and a watcher started again with the same name repeats
/// nothing it printed before.
#[test]
fn a_watcher_follows_the_head_through_a_restart_and_repeats_nothing() {
    let d = dirs();
    let mut head = FakeHead::new(&d.state.join("pastor.sock"));
    head.push("task.done", &task(1, TaskState::Done));
    head.served.lock().unwrap().jobs = vec![job("nightly", "failed (1x): boom")];

    let w = Watcher::start(&d, &["--name", "orch"]);
    let first = w.until(|l| l.starts_with("JOB "));
    assert_eq!(
        first,
        ["JOB nightly failing: failed (1x): boom"],
        "not t-1: it came before"
    );

    head.push("task.running", &task(2, TaskState::Running));
    let mut failed = task(2, TaskState::Failed);
    failed.error = Some("agent exited".into());
    head.push("task.failed", &failed);
    assert_eq!(
        w.until(|l| l.starts_with("TASK")),
        ["TASK t-2 failed pi-1 nightly: agent exited"],
        "running is not a state to act on"
    );

    head.stop();
    let down = w.until(|l| l.starts_with("HEAD"));
    assert!(down[0].starts_with("HEAD down: "), "{down:?}");

    head.push("task.blocked", &task(3, TaskState::Blocked));
    head.start();
    assert_eq!(
        w.until(|l| l.starts_with("TASK")),
        ["HEAD up", "TASK t-3 blocked pi-1 nightly"]
    );
    assert!(w.quiet_for(Duration::from_millis(2500)).is_empty());
    w.stop();

    // Started again: nothing from before, only what is new.
    head.push("task.done", &task(4, TaskState::Done));
    let w = Watcher::start(&d, &["--name", "orch"]);
    assert_eq!(
        w.until(|l| l.starts_with("TASK")),
        ["TASK t-4 done pi-1 nightly"]
    );
    assert!(w.quiet_for(Duration::from_millis(2500)).is_empty());
    w.stop();

    // --reset forgets it all: the failing job is news again, t-4 is not.
    let w = Watcher::start(&d, &["--name", "orch", "--reset", "--json"]);
    let line: serde_json::Value = serde_json::from_str(&w.until(|_| true)[0]).unwrap();
    assert_eq!(line["kind"], "JOB");
    assert_eq!(line["job"], "nightly");
    assert_eq!(line["line"], "JOB nightly failing: failed (1x): boom");
    assert!(w.quiet_for(Duration::from_millis(2500)).is_empty());
}

/// A connector directory with a `[watch]` command running `script`.
fn connector(d: &Dirs, id: &str, script: &str) -> PathBuf {
    let dir = d.root.join(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("pastor-connector.toml"),
        format!("id = \"{id}\"\nversion = \"0.1.0\"\n\n[watch]\ncommand = [\"sh\", \"watch.sh\"]\ntimeout = \"10s\"\n"),
    )
    .unwrap();
    std::fs::write(dir.join("watch.sh"), script).unwrap();
    let out = pastor(&d.config, &d.state)
        .args(["connector", "link"])
        .arg(&dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    dir
}

/// Each connector line prints once however often the connector repeats it;
/// a failing connector says so once, and again when it works.
#[test]
fn connector_lines_print_once_and_a_failing_connector_says_so() {
    let d = dirs();
    let _head = FakeHead::new(&d.state.join("pastor.sock"));
    let prs = connector(&d, "prs", "cat lines.txt\n");
    std::fs::write(prs.join("lines.txt"), "PR 3 open\nPR 3 open\nPR 4 open\n").unwrap();
    let flaky = connector(
        &d,
        "flaky",
        "[ -f ok ] || { echo 'gh: not logged in' >&2; exit 1; }\necho fine\n",
    );
    std::fs::write(
        d.config.join("pastor.toml"),
        "[[watch.connector]]\nname = \"prs\"\n\n[[watch.connector]]\nname = \"flaky\"\n",
    )
    .unwrap();

    let w = Watcher::start(&d, &[]);
    assert_eq!(
        w.until(|l| l.starts_with("CONNECTOR")),
        [
            "PR 3 open",
            "PR 4 open",
            "CONNECTOR flaky failing: exit 1: gh: not logged in"
        ]
    );
    std::fs::write(prs.join("lines.txt"), "PR 3 open\nPR 4 open\nPR 3 merged\n").unwrap();
    assert_eq!(w.until(|l| l == "PR 3 merged"), ["PR 3 merged"]);
    std::fs::write(flaky.join("ok"), "").unwrap();
    assert_eq!(w.until(|l| l == "fine"), ["CONNECTOR flaky ok", "fine"]);
    assert!(w.quiet_for(Duration::from_millis(2500)).is_empty());
    w.stop();

    // --connector runs only the one named.
    let w = Watcher::start(&d, &["--name", "prs-only", "--connector", "prs"]);
    assert_eq!(
        w.until(|l| l == "PR 3 merged"),
        ["PR 3 open", "PR 4 open", "PR 3 merged"]
    );
    assert!(w.quiet_for(Duration::from_millis(2500)).is_empty());

    // `connector try <id> watch` runs the same command once.
    let out = pastor(&d.config, &d.state)
        .args(["connector", "try", "prs", "watch"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "PR 3 open\nPR 4 open\nPR 3 merged\n"
    );
    let out = pastor(&d.config, &d.state)
        .args(["connector", "describe", "prs", "--json"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["watch"]["command"], serde_json::json!(["sh", "watch.sh"]));
}

/// `--now` prints what needs attention and exits; with no head it says the
/// head is down and still runs the connectors. An agent pastor started may
/// run it.
#[test]
fn now_prints_what_needs_attention_and_exits() {
    let d = dirs();
    let prs = connector(&d, "prs", "cat lines.txt\n");
    std::fs::write(prs.join("lines.txt"), "PR 3 open\n").unwrap();
    let now = |args: &[&str]| {
        let out = pastor(&d.config, &d.state)
            .env("PASTOR_TASK", "t-9")
            .args(["watch", "--now", "--connector", "prs"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let text = now(&[]);
    assert!(text.starts_with("HEAD down: "), "{text}");
    assert!(text.ends_with("PR 3 open\n"), "{text}");

    let head = FakeHead::new(&d.state.join("pastor.sock"));
    {
        let mut s = head.served.lock().unwrap();
        let mut stale = task(2, TaskState::Stale);
        stale.error = Some("no agent".into());
        s.tasks = vec![
            task(1, TaskState::Running),
            stale,
            task(3, TaskState::Blocked),
        ];
        s.jobs = vec![
            job("nightly", "failed: flock gone"),
            job("ok", "ok: 1 items, 1 tasks"),
        ];
    }
    assert_eq!(
        now(&[]),
        "TASK t-2 stale pi-1 nightly: no agent\nTASK t-3 blocked pi-1 nightly\nJOB nightly failing: failed: flock gone\nPR 3 open\n"
    );
    assert!(now(&["--all"]).contains("TASK t-1 running pi-1 nightly\n"));
}
