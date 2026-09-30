//! The `--json` shapes skills and orchestrators parse: `task list`, `task
//! describe`, `queue`, `machine list`, `job list`, `flock describe`,
//! `machine describe` and `serve status`. Each test pins the sorted key set
//! and the value types, never the values. A shape built only in `main.rs`
//! is read from the binary, against temp dirs and, for `serve status`, a
//! scripted head.
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};

use pastor::cli::{HeadRow, MachineRow, machine_list_json};
use pastor::config::flock::Flock;
use pastor::describe::{MachineDescription, flock_description};
use pastor::ipc::{IpcResponse, parse_request_line};
use pastor::machine::FlockSeat;
use pastor::queue::QueueEntry;
use pastor::scheduler::JobStatus;
use pastor::store::{NewTask, Store};
use pastor::task::{DispatchSpec, Task, TaskState};

mod common;

/// What a failing test says: the skill documents these shapes.
const UPDATE_SKILL: &str = "agents parse this --json; if the change is on purpose, update \
     skills/pastor/SKILL.md (and anything it tells agents to read) with it";

fn type_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// `v` is an object with the keys of `want`, each of a type its entry
/// allows (`"string|null"` takes either); only the keys in
/// `may_be_absent` may be missing.
fn assert_shape(what: &str, v: &Value, want: &[(&str, &str)], may_be_absent: &[&str]) {
    let Value::Object(o) = v else {
        panic!("{what}: not an object: {v}; {UPDATE_SKILL}");
    };
    for k in may_be_absent {
        assert!(
            want.iter().any(|(w, _)| w == k),
            "{what}: {k} may be absent but is not in the shape"
        );
    }
    let got: Vec<&str> = o.keys().map(String::as_str).collect();
    let mut want_keys: Vec<&str> = want
        .iter()
        .map(|(k, _)| *k)
        .filter(|k| o.contains_key(*k) || !may_be_absent.contains(k))
        .collect();
    want_keys.sort_unstable();
    assert_eq!(got, want_keys, "{what}: keys changed; {UPDATE_SKILL}");
    for (k, types) in want {
        let Some(val) = o.get(*k) else { continue };
        let t = type_of(val);
        assert!(
            types.split('|').any(|w| w == t),
            "{what}: {k} is {t}, not {types}; {UPDATE_SKILL}"
        );
    }
}

/// A value built with every field set has no null at its top, so each
/// type in its shape is the one it carries when set; `but` may still be
/// null there.
fn assert_filled(what: &str, v: &Value, but: &[&str]) {
    for (k, val) in v.as_object().unwrap() {
        assert!(
            !val.is_null() || but.contains(&k.as_str()),
            "{what}: {k} is null in the fullest value"
        );
    }
}

/// A value built with every optional field unset carries null for each key
/// its shape lets be null, but those in `but`, so a key left out when
/// unset fails here and not in an agent.
fn assert_emptied(what: &str, v: &Value, want: &[(&str, &str)], but: &[&str]) {
    for (k, types) in want {
        if !types.split('|').any(|t| t == "null") || but.contains(k) {
            continue;
        }
        assert_eq!(
            v.get(*k),
            Some(&Value::Null),
            "{what}: {k} is not null when unset; {UPDATE_SKILL}"
        );
    }
}

/// A sparse `v` has every key of `want` but `may_be_absent`.
fn assert_sparse_keys(what: &str, v: &Value, want: &[(&str, &str)], may_be_absent: &[&str]) {
    let got: BTreeMap<&str, ()> = v
        .as_object()
        .unwrap_or_else(|| panic!("{what}: not an object: {v}"))
        .keys()
        .map(|k| (k.as_str(), ()))
        .collect();
    let want: BTreeMap<&str, ()> = want
        .iter()
        .map(|(k, _)| (*k, ()))
        .filter(|(k, _)| !may_be_absent.contains(k))
        .collect();
    assert_eq!(
        got.keys().collect::<Vec<_>>(),
        want.keys().collect::<Vec<_>>(),
        "{what}: keys of a sparse value changed; {UPDATE_SKILL}"
    );
}

/// `Task::to_json`, as `task list` and `task describe` print each task.
const TASK: &[(&str, &str)] = &[
    ("aged_at", "string"),
    ("aged_from", "string"),
    ("agent_name", "string|null"),
    ("created_at", "string"),
    ("description", "string"),
    ("description_from", "string"),
    ("ended", "bool"),
    ("error", "string|null"),
    ("finished_at", "string|null"),
    ("fallback", "array"),
    ("flock", "string|null"),
    ("id", "number"),
    ("item", "object|string|number|bool|array|null"),
    ("job", "string"),
    ("last_completion_seq", "number|null"),
    ("machine", "string|null"),
    ("model", "string|null"),
    ("pane_id", "string|null"),
    ("paused_at", "string"),
    ("paused_for", "number"),
    ("preempt", "bool"),
    ("priority", "string"),
    ("priority_from", "string"),
    ("profile", "string|null"),
    ("prompt", "string"),
    ("prompt_pending", "bool"),
    ("queue_pos", "number"),
    ("resumed_at", "string"),
    ("retry_of", "number|null"),
    ("role", "string"),
    ("spec", "object"),
    ("started_at", "string|null"),
    ("state", "string"),
    ("summary", "object"),
    ("updated_at", "string"),
    ("workspace_id", "string|null"),
];

/// Left out of a task's JSON unless set.
const TASK_MAY_BE_ABSENT: &[&str] = &[
    "aged_at",
    "aged_from",
    "ended",
    "paused_at",
    "paused_for",
    "preempt",
    "priority_from",
    "resumed_at",
    "summary",
];

/// `TaskSummary`, as `summary` and each of `summaries` carry it.
const SUMMARY: &[(&str, &str)] = &[
    ("at", "string"),
    ("outcome", "string"),
    ("round", "number"),
    ("source", "string"),
    ("text", "string"),
];

fn spec() -> DispatchSpec {
    DispatchSpec {
        now: false,
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
        cwd: None,
        keep_pane: None,
        keep_pane_from: None,
        rounds: Default::default(),
    }
}

fn new_task() -> NewTask {
    NewTask {
        job: "nightly".into(),
        item: json!({"key": "k"}),
        prompt: "p".into(),
        spec: spec(),
        flock: "default".into(),
        description: None,
    }
}

/// A done task in `store` whose round ended with a summary: the store
/// reads one only for a task that is done, failed or closed.
fn done_with_summary(store: &Store) -> Task {
    let mut t = store.insert_task(new_task()).unwrap();
    t.state = TaskState::Done;
    store.update_task(&mut t).unwrap();
    store.end_round(t.id, Some("fixed it")).unwrap();
    t
}

/// A task with every field set, so each optional key shows.
fn full_task() -> Task {
    let store = Store::open_in_memory().unwrap();
    let t = done_with_summary(&store);
    let mut t = store.get_task(t.id).unwrap().unwrap();
    let now = chrono::Utc::now();
    t.machine = Some("pi-1".into());
    t.workspace_id = Some("w1".into());
    t.pane_id = Some("p1".into());
    t.agent_name = Some("t-1".into());
    t.error = Some("boom".into());
    t.last_completion_seq = Some(7);
    t.ended = true;
    t.retry_of = Some(1);
    t.priority_from = Some("task run".into());
    t.aged_from = Some(pastor::task::Priority::Low);
    t.aged_at = Some(now);
    t.description = Some("fix the suite".into());
    t.pause.preempt = true;
    t.pause.paused_at = Some(now);
    t.pause.paused_for = Some(2);
    t.pause.resumed_at = Some(now);
    t.started_at = Some(now);
    t.finished_at = Some(now);
    t.spec.agent_source = Some(Box::new(
        serde_json::from_value(json!({
            "ask": {},
            "agent": "claude",
            "model": "opus",
            "fallback": ["sonnet"],
            "fallback_from": "task run",
            "profile": "safe",
        }))
        .unwrap(),
    ));
    assert!(t.summary.is_some(), "end_round gave the task a summary");
    t
}

/// A task with every optional field unset, so each nullable key is null.
fn bare_task() -> Task {
    let store = Store::open_in_memory().unwrap();
    let mut t = store
        .insert_task(NewTask {
            item: Value::Null,
            ..new_task()
        })
        .unwrap();
    t.flock = None;
    t
}

fn full_task_json_has_the_task_shape(what: &str, v: &Value) {
    assert_shape(what, v, TASK, &[]);
    assert_filled(what, v, &[]);
    assert_shape(&format!("{what} summary"), &v["summary"], SUMMARY, &[]);
}

fn bare_task_json_has_the_task_shape(what: &str, v: &Value) {
    assert_sparse_keys(what, v, TASK, TASK_MAY_BE_ABSENT);
    assert_shape(what, v, TASK, TASK_MAY_BE_ABSENT);
    assert_emptied(what, v, TASK, &[]);
}

fn pastor(config: &Path, state: &Path) -> Command {
    let mut c = common::pastor();
    c.env("PASTOR_CONFIG_DIR", config)
        .env("PASTOR_STATE_DIR", state)
        .env("PASTOR_DATA_DIR", state.join("data"));
    c
}

struct Dirs {
    _tmp: tempfile::TempDir,
    config: std::path::PathBuf,
    state: std::path::PathBuf,
}

fn dirs() -> Dirs {
    let tmp = tempfile::tempdir().unwrap();
    let (config, state) = (tmp.path().join("c"), tmp.path().join("s"));
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    Dirs {
        _tmp: tmp,
        config,
        state,
    }
}

/// `args` run with no head; its stdout as JSON.
fn json_of(d: &Dirs, args: &[&str]) -> Value {
    let out = pastor(&d.config, &d.state).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{args:?}: {e}: {}", String::from_utf8_lossy(&out.stdout)))
}

/// A store in `d` with one task that has a summary, and one without.
fn store_with_tasks(d: &Dirs) {
    let store = Store::open(&d.state.join("pastor.db")).unwrap();
    done_with_summary(&store);
    store.insert_task(new_task()).unwrap();
}

#[test]
fn task_list_json_is_an_array_of_tasks() {
    full_task_json_has_the_task_shape("task list", &full_task().to_json());
    bare_task_json_has_the_task_shape("task list", &bare_task().to_json());

    let d = dirs();
    store_with_tasks(&d);
    let v = json_of(&d, &["task", "list", "--all", "--json"]);
    let tasks = v.as_array().unwrap_or_else(|| panic!("not an array: {v}"));
    assert_eq!(tasks.len(), 2, "{v}");
    assert!(tasks.iter().any(|t| t["summary"].is_object()), "{v}");
    for t in tasks {
        assert_shape("task list", t, TASK, TASK_MAY_BE_ABSENT);
    }
    let sparse = tasks.iter().find(|t| t.get("summary").is_none()).unwrap();
    assert_sparse_keys("task list", sparse, TASK, TASK_MAY_BE_ABSENT);
}

#[test]
fn task_describe_json_is_a_task_and_its_summaries() {
    full_task_json_has_the_task_shape("task describe", &full_task().to_json());
    bare_task_json_has_the_task_shape("task describe", &bare_task().to_json());

    let d = dirs();
    store_with_tasks(&d);
    let v = json_of(&d, &["task", "describe", "t-2", "--json"]);
    assert_sparse_keys("task describe", &v, TASK, TASK_MAY_BE_ABSENT);
    assert_shape("task describe", &v, TASK, TASK_MAY_BE_ABSENT);

    // --all-summaries adds every round's summary beside the task.
    let v = json_of(
        &d,
        &["task", "describe", "t-1", "--all-summaries", "--json"],
    );
    let mut with: Vec<(&str, &str)> = TASK.to_vec();
    with.push(("summaries", "array"));
    assert_shape(
        "task describe --all-summaries",
        &v,
        &with,
        TASK_MAY_BE_ABSENT,
    );
    assert_shape("task describe summary", &v["summary"], SUMMARY, &[]);
    for s in v["summaries"].as_array().unwrap() {
        assert_shape("task describe summaries", s, SUMMARY, &[]);
    }
}

#[test]
fn queue_json_is_entries_with_their_task() {
    let mut task = full_task();
    task.state = TaskState::Queued;
    task.spec.machine = Some("pi-1".into());
    let entry = QueueEntry {
        pos: 1,
        flock: "default".into(),
        why: "every machine is full".into(),
        task,
    };
    let want = [
        ("aged_from", "string|null"),
        ("flock", "string"),
        ("from", "string"),
        ("id", "string"),
        ("machine", "string|null"),
        ("pos", "number"),
        ("priority", "string"),
        ("task", "object"),
        ("waited_secs", "number"),
        ("where", "string"),
        ("why", "string"),
    ];
    let v = entry.to_json();
    assert_shape("queue", &v, &want, &[]);
    assert_filled("queue", &v, &[]);
    full_task_json_has_the_task_shape("queue task", &v["task"]);

    // A task for any machine.
    let mut task = bare_task();
    task.state = TaskState::Queued;
    let entry = QueueEntry { task, ..entry };
    let v = entry.to_json();
    assert_shape("queue", &v, &want, &[]);
    assert_emptied("queue", &v, &want, &[]);
    bare_task_json_has_the_task_shape("queue task", &v["task"]);
}

/// `MachineRow`, as `machine list` prints each machine and `machine
/// describe` starts.
const MACHINE: &[(&str, &str)] = &[
    ("burst", "number"),
    ("channel", "string"),
    ("description", "string|null"),
    ("endpoint", "string"),
    ("error", "string|null"),
    ("flock", "string"),
    ("flocks", "array"),
    ("herdr_version", "string|null"),
    ("host", "string"),
    ("job_slots", "number"),
    ("live", "number|null"),
    ("max_agents", "number"),
    ("name", "string"),
    ("now", "array"),
    ("orphans", "array"),
    ("pastor_version", "string|null"),
    ("profile", "string|null"),
    ("protocol", "number|null"),
    ("tags", "array"),
];

/// Left out of a machine's JSON when it is in no named flock, or starts
/// no `--now` task.
const MACHINE_MAY_BE_ABSENT: &[&str] = &["flocks", "now"];

fn machine_row() -> MachineRow {
    MachineRow {
        now: vec!["t-9".into()],
        name: "pi-1".into(),
        host: "user@pi-1".into(),
        endpoint: "ssh user@pi-1".into(),
        flock: "work".into(),
        flocks: vec![FlockSeat {
            name: "work".into(),
            share: Some(2),
            max: Some(2),
            live: 1,
        }],
        channel: "connected".into(),
        herdr_version: Some("0.9.1".into()),
        pastor_version: Some("0.5.0".into()),
        protocol: Some(22),
        error: Some("slow".into()),
        live: Some(1),
        max_agents: 2,
        job_slots: 1,
        burst: 1,
        tags: vec!["arm".into()],
        orphans: vec!["t-9".into()],
        profile: Some("safe".into()),
        description: Some("the one by the window".into()),
    }
}

/// A machine never reached, in no named flock, with nothing optional set.
fn bare_machine_row() -> MachineRow {
    MachineRow {
        now: Vec::new(),
        name: "pi-1".into(),
        host: "user@pi-1".into(),
        endpoint: "ssh user@pi-1".into(),
        flock: "default".into(),
        flocks: vec![],
        channel: "unreachable".into(),
        herdr_version: None,
        pastor_version: None,
        protocol: None,
        error: None,
        live: None,
        max_agents: 1,
        job_slots: 1,
        burst: 0,
        tags: vec![],
        orphans: vec![],
        profile: None,
        description: None,
    }
}

fn assert_machine_row(what: &str, v: &Value) {
    assert_shape(what, v, MACHINE, &[]);
    assert_filled(what, v, &[]);
    for seat in v["flocks"].as_array().unwrap() {
        assert_shape(
            &format!("{what} flocks"),
            seat,
            &[
                ("live", "number"),
                ("max", "number|null"),
                ("name", "string"),
                ("share", "number|null"),
            ],
            &[],
        );
    }
}

#[test]
fn machine_list_json_is_the_head_and_its_machines() {
    let head_shape = [
        ("channel", "string"),
        ("herdr_version", "string|null"),
        ("host", "string"),
        ("name", "string"),
        ("pastor_version", "string"),
    ];
    let head = HeadRow::new("pi-1".into(), Some("0.9.1".into()));
    let rows = [machine_row()];
    let v = serde_json::to_value(machine_list_json(&head, &rows)).unwrap();
    assert_shape(
        "machine list",
        &v,
        &[("head", "object"), ("machines", "array")],
        &[],
    );
    assert_shape("machine list head", &v["head"], &head_shape, &[]);
    assert_filled("machine list head", &v["head"], &[]);
    assert_machine_row("machine list machines", &v["machines"][0]);

    // A head without herdr, and a machine never reached.
    let head = HeadRow::new("pi-1".into(), None);
    let rows = [bare_machine_row()];
    let v = serde_json::to_value(machine_list_json(&head, &rows)).unwrap();
    assert_shape("machine list head", &v["head"], &head_shape, &[]);
    assert_emptied("machine list head", &v["head"], &head_shape, &[]);
    let m = &v["machines"][0];
    assert_sparse_keys("machine list machines", m, MACHINE, MACHINE_MAY_BE_ABSENT);
    assert_shape("machine list machines", m, MACHINE, MACHINE_MAY_BE_ABSENT);
    assert_emptied("machine list machines", m, MACHINE, &[]);
}

#[test]
fn job_list_json_is_an_array_of_jobs() {
    let now = chrono::Utc::now();
    let jobs = vec![JobStatus {
        name: "nightly".into(),
        schedule: Some("every 1h".into()),
        enabled: true,
        connector: Some("clock".into()),
        error: Some("bad".into()),
        last_run_at: Some(now),
        last_result: Some("queued 1".into()),
        next_due: Some(now),
        running: false,
        flock: Some("work".into()),
        description: Some("the night shift".into()),
    }];
    let v = serde_json::to_value(&jobs).unwrap();
    let want = [
        ("connector", "string|null"),
        ("description", "string|null"),
        ("enabled", "bool"),
        ("error", "string|null"),
        ("flock", "string|null"),
        ("last_result", "string|null"),
        ("last_run_at", "string|null"),
        ("name", "string"),
        ("next_due", "string|null"),
        ("running", "bool"),
        ("schedule", "string|null"),
    ];
    assert_shape("job list", &v[0], &want, &[]);
    assert_filled("job list", &v[0], &[]);

    // A job that never ran, with nothing optional set.
    let bare = JobStatus {
        name: "nightly".into(),
        schedule: None,
        enabled: false,
        connector: None,
        error: None,
        last_run_at: None,
        last_result: None,
        next_due: None,
        running: false,
        flock: None,
        description: None,
    };
    let v = serde_json::to_value(&bare).unwrap();
    assert_shape("job list", &v, &want, &[]);
    assert_emptied("job list", &v, &want, &[]);

    // The binary prints the same shape, with no head.
    let d = dirs();
    std::fs::create_dir_all(d.config.join("jobs")).unwrap();
    std::fs::write(
        d.config.join("jobs/nightly.toml"),
        "schedule = \"every 1h\"\n\n[connector]\nuse = \"static\"\n\n[dispatch]\nprompt = \"hi\"\n",
    )
    .unwrap();
    let v = json_of(&d, &["job", "list", "--json"]);
    let jobs = v.as_array().unwrap_or_else(|| panic!("not an array: {v}"));
    assert_eq!(jobs.len(), 1, "{v}");
    assert_shape("job list", &jobs[0], &want, &[]);
}

const FLOCK_TOML: &str = "[[flock]]\nname = \"work\"\ndefault = true\ndescription = \"work \
     things\"\nagent = \"claude\"\nagent_args = [\"-v\"]\nallow = [\"Bash\"]\ndeny = \
     [\"Web\"]\nmodel = \"opus\"\nfallback = [\"sonnet\"]\nprofile = \"safe\"\ntimeout = \"1h\"\nplace = \"own\"\n\n\
     [[machine]]\nname = \"pi-1\"\nssh = \"user@pi-1\"\nflock = \"work\"\n";

#[test]
fn flock_describe_json_is_the_flock_and_its_tasks() {
    let want = [
        ("agent", "string|null"),
        ("agent_args", "array|null"),
        ("agents", "number|null"),
        ("agents_by_kind", "object"),
        ("allow", "array"),
        ("default", "bool"),
        ("deny", "array"),
        ("description", "string|null"),
        ("fallback", "array|null"),
        ("machines", "array"),
        ("model", "string|null"),
        ("name", "string"),
        ("place", "string|null"),
        ("profile", "string|null"),
        ("tasks", "array"),
        ("timeout", "string|null"),
    ];
    let f = Flock::parse(Path::new("flock.toml"), FLOCK_TOML).unwrap();
    let d = flock_description(&f, "work", None, vec![full_task()]).unwrap();
    let v = serde_json::to_value(&d).unwrap();
    assert_shape("flock describe", &v, &want, &[]);
    // Live agents are known only from a running head.
    assert_filled("flock describe", &v, &["agents"]);
    assert_eq!(v["machines"], json!(["pi-1"]));
    // The tasks are the rows as serde writes them, not `Task::to_json`.
    assert!(v["tasks"][0]["id"].is_number(), "{v}");

    // A flock that sets nothing but its name.
    let bare = "[[flock]]\nname = \"work\"\ndefault = true\n\n[[machine]]\nname = \"pi-1\"\nssh = \
         \"user@pi-1\"\nflock = \"work\"\n";
    let f = Flock::parse(Path::new("flock.toml"), bare).unwrap();
    let d = flock_description(&f, "work", None, vec![]).unwrap();
    let v = serde_json::to_value(&d).unwrap();
    assert_shape("flock describe", &v, &want, &[]);
    assert_emptied("flock describe", &v, &want, &[]);
}

#[test]
fn machine_describe_json_is_the_row_its_tasks_and_errors() {
    let d = MachineDescription {
        row: machine_row(),
        session: "default".into(),
        model: Some("opus".into()),
        fallback: Some(vec!["sonnet".into()]),
        agents_by_kind: Default::default(),
        tasks: vec![full_task()],
        recent_errors: vec![],
    };
    let v = serde_json::to_value(&d).unwrap();
    let mut want: Vec<(&str, &str)> = MACHINE.to_vec();
    want.extend([
        ("agents_by_kind", "object"),
        ("fallback", "array|null"),
        ("model", "string|null"),
        ("recent_errors", "array"),
        ("session", "string"),
        ("tasks", "array"),
    ]);
    assert_shape("machine describe", &v, &want, &[]);
    assert_filled("machine describe", &v, &[]);
    assert!(v["tasks"][0]["id"].is_number(), "{v}");

    // A machine never reached, with no model and no tasks.
    let d = MachineDescription {
        row: bare_machine_row(),
        session: "default".into(),
        model: None,
        fallback: None,
        agents_by_kind: Default::default(),
        tasks: vec![],
        recent_errors: vec![],
    };
    let v = serde_json::to_value(&d).unwrap();
    assert_shape("machine describe", &v, &want, MACHINE_MAY_BE_ABSENT);
    assert_sparse_keys("machine describe", &v, &want, MACHINE_MAY_BE_ABSENT);
    assert_emptied("machine describe", &v, &want, &[]);
}

/// A head on `socket` that answers every ping.
fn pinging_head(socket: &Path) {
    let listener = UnixListener::bind(socket).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut line = String::new();
            let _ = BufReader::new(&stream).read_line(&mut line);
            if parse_request_line(&line).is_err() {
                continue;
            }
            let pong = IpcResponse::Pong {
                version: "test".into(),
                protocol: pastor::ipc::IPC_PROTOCOL,
                role: None,
            };
            let mut out = serde_json::to_string(&pong).unwrap();
            out.push('\n');
            let _ = stream.write_all(out.as_bytes());
        }
    });
}

#[test]
fn serve_status_json_says_what_runs_and_where() {
    let d = dirs();
    let socket = d.state.join("pastor.sock");
    pinging_head(&socket);
    // The head's record: this process holds the socket, so its pid is the
    // one `serve status` reads from the peer.
    std::fs::write(
        d.state.join("serve.json"),
        json!({
            "pid": std::process::id(),
            "service": "systemd",
            "log": d.state.join("serve.log"),
        })
        .to_string(),
    )
    .unwrap();
    let want = [
        ("log", "string|null"),
        ("pid", "number|null"),
        ("protocol", "number"),
        ("role", "string"),
        ("running", "bool"),
        ("service", "string|null"),
        ("socket", "string"),
        ("version", "string"),
    ];
    let v = json_of(&d, &["serve", "status", "--json"]);
    assert_shape("serve status", &v, &want, &[]);
    assert_filled("serve status", &v, &[]);

    // A head an older pastor started wrote no record. The pid stays: this
    // process holds the socket, and its peer pid is always readable here.
    std::fs::remove_file(d.state.join("serve.json")).unwrap();
    let v = json_of(&d, &["serve", "status", "--json"]);
    assert_shape("serve status", &v, &want, &[]);
    assert_emptied("serve status", &v, &want, &["pid"]);
}
