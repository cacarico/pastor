//! Every old schema, migrated by `Store::open`, ends at the schema a fresh
//! store has. Fresh databases and migrations are built by separate code in
//! `src/store.rs`, so a column added to one and not the other would only
//! show for people upgrading. Each old schema is kept below as the DDL a
//! fresh store of that version ran, taken with `git show <commit>:src/store.rs`
//! from the release that had it, or from the last commit at that version
//! when no release did.
use std::collections::BTreeMap;
use std::path::Path;

use pastor::store::Store;
use rusqlite::Connection;
use rusqlite::types::Value;

/// Schema 1: `src/store.rs` at e957de2459a9 (last commit at schema 1).
const V1: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS tasks (
    id INTEGER PRIMARY KEY,
    job TEXT NOT NULL,
    item TEXT NOT NULL,
    prompt TEXT NOT NULL,
    spec TEXT NOT NULL,
    machine TEXT,
    workspace_id TEXT,
    pane_id TEXT,
    agent_name TEXT,
    state TEXT NOT NULL,
    error TEXT,
    last_completion_seq INTEGER,
    created_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS tasks_state ON tasks(state);
CREATE INDEX IF NOT EXISTS tasks_machine ON tasks(machine);
INSERT INTO meta (key, value) VALUES ('schema_version', '1');";

/// Schema 2: `src/store.rs` at 9db72235a20f (v0.2.0).
const V2: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS tasks (
    id INTEGER PRIMARY KEY,
    job TEXT NOT NULL,
    item TEXT NOT NULL,
    prompt TEXT NOT NULL,
    spec TEXT NOT NULL,
    machine TEXT,
    workspace_id TEXT,
    pane_id TEXT,
    agent_name TEXT,
    state TEXT NOT NULL,
    error TEXT,
    last_completion_seq INTEGER,
    prompt_pending INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS tasks_state ON tasks(state);
CREATE INDEX IF NOT EXISTS tasks_machine ON tasks(machine);
CREATE TABLE IF NOT EXISTS seen (
    job TEXT NOT NULL,
    key TEXT NOT NULL,
    task_id INTEGER,
    seen_at TEXT NOT NULL,
    PRIMARY KEY (job, key)
);
CREATE TABLE IF NOT EXISTS job_state (
    name TEXT PRIMARY KEY,
    last_run_at TEXT,
    last_ok_at TEXT,
    last_result TEXT,
    last_error TEXT,
    cursor TEXT,
    failures INTEGER NOT NULL DEFAULT 0,
    backoff_until TEXT
);
INSERT INTO meta (key, value) VALUES ('schema_version', '2');";

/// Schema 3: `src/store.rs` at 795f1a6a0dde (v0.3.0).
const V3: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS tasks (
    id INTEGER PRIMARY KEY,
    job TEXT NOT NULL,
    item TEXT NOT NULL,
    prompt TEXT NOT NULL,
    spec TEXT NOT NULL,
    machine TEXT,
    workspace_id TEXT,
    pane_id TEXT,
    agent_name TEXT,
    state TEXT NOT NULL,
    error TEXT,
    last_completion_seq INTEGER,
    prompt_pending INTEGER NOT NULL DEFAULT 0,
    retry_of INTEGER,
    created_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS tasks_state ON tasks(state);
CREATE INDEX IF NOT EXISTS tasks_machine ON tasks(machine);
CREATE TABLE IF NOT EXISTS seen (
    job TEXT NOT NULL,
    key TEXT NOT NULL,
    task_id INTEGER,
    seen_at TEXT NOT NULL,
    PRIMARY KEY (job, key)
);
CREATE TABLE IF NOT EXISTS job_state (
    name TEXT PRIMARY KEY,
    last_run_at TEXT,
    last_ok_at TEXT,
    last_result TEXT,
    last_error TEXT,
    cursor TEXT,
    failures INTEGER NOT NULL DEFAULT 0,
    backoff_until TEXT
);
INSERT INTO meta (key, value) VALUES ('schema_version', '3');";

/// Schema 4: `src/store.rs` at a71368f92301 (last commit at schema 4).
const V4: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS tasks (
    id INTEGER PRIMARY KEY,
    job TEXT NOT NULL,
    item TEXT NOT NULL,
    prompt TEXT NOT NULL,
    spec TEXT NOT NULL,
    machine TEXT,
    workspace_id TEXT,
    pane_id TEXT,
    agent_name TEXT,
    state TEXT NOT NULL,
    error TEXT,
    last_completion_seq INTEGER,
    prompt_pending INTEGER NOT NULL DEFAULT 0,
    retry_of INTEGER,
    flock TEXT,
    created_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS tasks_state ON tasks(state);
CREATE INDEX IF NOT EXISTS tasks_machine ON tasks(machine);
CREATE TABLE IF NOT EXISTS seen (
    job TEXT NOT NULL,
    key TEXT NOT NULL,
    task_id INTEGER,
    seen_at TEXT NOT NULL,
    PRIMARY KEY (job, key)
);
CREATE TABLE IF NOT EXISTS job_state (
    name TEXT PRIMARY KEY,
    last_run_at TEXT,
    last_ok_at TEXT,
    last_result TEXT,
    last_error TEXT,
    cursor TEXT,
    failures INTEGER NOT NULL DEFAULT 0,
    backoff_until TEXT
);
INSERT INTO meta (key, value) VALUES ('schema_version', '4');";

/// Schema 5: `src/store.rs` at 7d52d37098c3 (v0.4.0-rc.1).
const V5: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS tasks (
    id INTEGER PRIMARY KEY,
    job TEXT NOT NULL,
    item TEXT NOT NULL,
    prompt TEXT NOT NULL,
    spec TEXT NOT NULL,
    machine TEXT,
    workspace_id TEXT,
    pane_id TEXT,
    agent_name TEXT,
    state TEXT NOT NULL,
    error TEXT,
    last_completion_seq INTEGER,
    prompt_pending INTEGER NOT NULL DEFAULT 0,
    retry_of INTEGER,
    flock TEXT,
    trust_sent INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS tasks_state ON tasks(state);
CREATE INDEX IF NOT EXISTS tasks_machine ON tasks(machine);
CREATE TABLE IF NOT EXISTS seen (
    job TEXT NOT NULL,
    key TEXT NOT NULL,
    task_id INTEGER,
    seen_at TEXT NOT NULL,
    PRIMARY KEY (job, key)
);
CREATE TABLE IF NOT EXISTS job_state (
    name TEXT PRIMARY KEY,
    last_run_at TEXT,
    last_ok_at TEXT,
    last_result TEXT,
    last_error TEXT,
    cursor TEXT,
    failures INTEGER NOT NULL DEFAULT 0,
    backoff_until TEXT
);
CREATE TABLE IF NOT EXISTS trusted_repos (
    machine TEXT NOT NULL,
    repo TEXT NOT NULL,
    trusted_at TEXT NOT NULL,
    PRIMARY KEY (machine, repo)
);
INSERT INTO meta (key, value) VALUES ('schema_version', '5');";

/// Schema 6: `src/store.rs` at 7a79451c576e (v0.5.0).
const V6: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS tasks (
    id INTEGER PRIMARY KEY,
    job TEXT NOT NULL,
    item TEXT NOT NULL,
    prompt TEXT NOT NULL,
    spec TEXT NOT NULL,
    machine TEXT,
    workspace_id TEXT,
    pane_id TEXT,
    agent_name TEXT,
    state TEXT NOT NULL,
    error TEXT,
    last_completion_seq INTEGER,
    prompt_pending INTEGER NOT NULL DEFAULT 0,
    retry_of INTEGER,
    flock TEXT,
    trust_sent INTEGER NOT NULL DEFAULT 0,
    activity_seen INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS tasks_state ON tasks(state);
CREATE INDEX IF NOT EXISTS tasks_machine ON tasks(machine);
CREATE TABLE IF NOT EXISTS seen (
    job TEXT NOT NULL,
    key TEXT NOT NULL,
    task_id INTEGER,
    seen_at TEXT NOT NULL,
    PRIMARY KEY (job, key)
);
CREATE TABLE IF NOT EXISTS job_state (
    name TEXT PRIMARY KEY,
    last_run_at TEXT,
    last_ok_at TEXT,
    last_result TEXT,
    last_error TEXT,
    cursor TEXT,
    failures INTEGER NOT NULL DEFAULT 0,
    backoff_until TEXT
);
CREATE TABLE IF NOT EXISTS trusted_repos (
    machine TEXT NOT NULL,
    repo TEXT NOT NULL,
    trusted_at TEXT NOT NULL,
    PRIMARY KEY (machine, repo)
);
INSERT INTO meta (key, value) VALUES ('schema_version', '6');";

/// Schema 7: `src/store.rs` at 2c33bb30726f (v0.6.0).
const V7: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS tasks (
    id INTEGER PRIMARY KEY,
    job TEXT NOT NULL,
    item TEXT NOT NULL,
    prompt TEXT NOT NULL,
    spec TEXT NOT NULL,
    machine TEXT,
    workspace_id TEXT,
    pane_id TEXT,
    agent_name TEXT,
    state TEXT NOT NULL,
    error TEXT,
    last_completion_seq INTEGER,
    prompt_pending INTEGER NOT NULL DEFAULT 0,
    retry_of INTEGER,
    flock TEXT,
    trust_sent INTEGER NOT NULL DEFAULT 0,
    activity_seen INTEGER NOT NULL DEFAULT 0,
    ended INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS tasks_state ON tasks(state);
CREATE INDEX IF NOT EXISTS tasks_machine ON tasks(machine);
CREATE TABLE IF NOT EXISTS seen (
    job TEXT NOT NULL,
    key TEXT NOT NULL,
    task_id INTEGER,
    seen_at TEXT NOT NULL,
    PRIMARY KEY (job, key)
);
CREATE TABLE IF NOT EXISTS job_state (
    name TEXT PRIMARY KEY,
    last_run_at TEXT,
    last_ok_at TEXT,
    last_result TEXT,
    last_error TEXT,
    cursor TEXT,
    failures INTEGER NOT NULL DEFAULT 0,
    backoff_until TEXT
);
CREATE TABLE IF NOT EXISTS trusted_repos (
    machine TEXT NOT NULL,
    repo TEXT NOT NULL,
    trusted_at TEXT NOT NULL,
    PRIMARY KEY (machine, repo)
);
INSERT INTO meta (key, value) VALUES ('schema_version', '7');";

/// Schema 8: `src/store.rs` at 50c28bf4f3ff (last commit at schema 8).
const V8: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS tasks (
    id INTEGER PRIMARY KEY,
    job TEXT NOT NULL,
    item TEXT NOT NULL,
    prompt TEXT NOT NULL,
    spec TEXT NOT NULL,
    machine TEXT,
    workspace_id TEXT,
    pane_id TEXT,
    agent_name TEXT,
    state TEXT NOT NULL,
    error TEXT,
    last_completion_seq INTEGER,
    prompt_pending INTEGER NOT NULL DEFAULT 0,
    retry_of INTEGER,
    flock TEXT,
    trust_sent INTEGER NOT NULL DEFAULT 0,
    activity_seen INTEGER NOT NULL DEFAULT 0,
    ended INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS tasks_state ON tasks(state);
CREATE INDEX IF NOT EXISTS tasks_machine ON tasks(machine);
CREATE TABLE IF NOT EXISTS seen (
    job TEXT NOT NULL,
    key TEXT NOT NULL,
    task_id INTEGER,
    seen_at TEXT NOT NULL,
    PRIMARY KEY (job, key)
);
CREATE TABLE IF NOT EXISTS job_state (
    name TEXT PRIMARY KEY,
    last_run_at TEXT,
    last_ok_at TEXT,
    last_result TEXT,
    last_error TEXT,
    cursor TEXT,
    failures INTEGER NOT NULL DEFAULT 0,
    backoff_until TEXT
);
CREATE TABLE IF NOT EXISTS trusted_repos (
    machine TEXT NOT NULL,
    repo TEXT NOT NULL,
    trusted_at TEXT NOT NULL,
    PRIMARY KEY (machine, repo)
);
CREATE TABLE IF NOT EXISTS event_seq (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    last INTEGER NOT NULL
);
INSERT OR IGNORE INTO event_seq (id, last) VALUES (1, 0);
INSERT INTO meta (key, value) VALUES ('schema_version', '8');";

/// Schema 9: `src/store.rs` at 0b7a615b4e35 (last commit at schema 9).
const V9: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS tasks (
    id INTEGER PRIMARY KEY,
    job TEXT NOT NULL,
    item TEXT NOT NULL,
    prompt TEXT NOT NULL,
    spec TEXT NOT NULL,
    machine TEXT,
    workspace_id TEXT,
    pane_id TEXT,
    agent_name TEXT,
    state TEXT NOT NULL,
    error TEXT,
    last_completion_seq INTEGER,
    prompt_pending INTEGER NOT NULL DEFAULT 0,
    retry_of INTEGER,
    flock TEXT,
    trust_sent INTEGER NOT NULL DEFAULT 0,
    activity_seen INTEGER NOT NULL DEFAULT 0,
    ended INTEGER NOT NULL DEFAULT 0,
    priority TEXT NOT NULL DEFAULT 'normal',
    priority_from TEXT,
    queue_pos INTEGER,
    created_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS tasks_state ON tasks(state);
CREATE INDEX IF NOT EXISTS tasks_machine ON tasks(machine);
CREATE TABLE IF NOT EXISTS seen (
    job TEXT NOT NULL,
    key TEXT NOT NULL,
    task_id INTEGER,
    seen_at TEXT NOT NULL,
    PRIMARY KEY (job, key)
);
CREATE TABLE IF NOT EXISTS job_state (
    name TEXT PRIMARY KEY,
    last_run_at TEXT,
    last_ok_at TEXT,
    last_result TEXT,
    last_error TEXT,
    cursor TEXT,
    failures INTEGER NOT NULL DEFAULT 0,
    backoff_until TEXT
);
CREATE TABLE IF NOT EXISTS trusted_repos (
    machine TEXT NOT NULL,
    repo TEXT NOT NULL,
    trusted_at TEXT NOT NULL,
    PRIMARY KEY (machine, repo)
);
CREATE TABLE IF NOT EXISTS event_seq (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    last INTEGER NOT NULL
);
INSERT OR IGNORE INTO event_seq (id, last) VALUES (1, 0);
INSERT INTO meta (key, value) VALUES ('schema_version', '9');";

/// Schema 10: `src/store.rs` at ef20056a0580 (last commit at schema 10).
const V10: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS tasks (
    id INTEGER PRIMARY KEY,
    job TEXT NOT NULL,
    item TEXT NOT NULL,
    prompt TEXT NOT NULL,
    spec TEXT NOT NULL,
    machine TEXT,
    workspace_id TEXT,
    pane_id TEXT,
    agent_name TEXT,
    state TEXT NOT NULL,
    error TEXT,
    last_completion_seq INTEGER,
    prompt_pending INTEGER NOT NULL DEFAULT 0,
    retry_of INTEGER,
    flock TEXT,
    trust_sent INTEGER NOT NULL DEFAULT 0,
    activity_seen INTEGER NOT NULL DEFAULT 0,
    ended INTEGER NOT NULL DEFAULT 0,
    priority TEXT NOT NULL DEFAULT 'normal',
    priority_from TEXT,
    queue_pos INTEGER,
    role TEXT NOT NULL DEFAULT 'agent',
    created_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS tasks_state ON tasks(state);
CREATE INDEX IF NOT EXISTS tasks_machine ON tasks(machine);
CREATE TABLE IF NOT EXISTS seen (
    job TEXT NOT NULL,
    key TEXT NOT NULL,
    task_id INTEGER,
    seen_at TEXT NOT NULL,
    PRIMARY KEY (job, key)
);
CREATE TABLE IF NOT EXISTS job_state (
    name TEXT PRIMARY KEY,
    last_run_at TEXT,
    last_ok_at TEXT,
    last_result TEXT,
    last_error TEXT,
    cursor TEXT,
    failures INTEGER NOT NULL DEFAULT 0,
    backoff_until TEXT
);
CREATE TABLE IF NOT EXISTS trusted_repos (
    machine TEXT NOT NULL,
    repo TEXT NOT NULL,
    trusted_at TEXT NOT NULL,
    PRIMARY KEY (machine, repo)
);
CREATE TABLE IF NOT EXISTS event_seq (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    last INTEGER NOT NULL
);
INSERT OR IGNORE INTO event_seq (id, last) VALUES (1, 0);
INSERT INTO meta (key, value) VALUES ('schema_version', '10');";

/// Schema 11: `src/store.rs` at 3b51de129d4a (v0.7.1).
const V11: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS tasks (
    id INTEGER PRIMARY KEY,
    job TEXT NOT NULL,
    item TEXT NOT NULL,
    prompt TEXT NOT NULL,
    spec TEXT NOT NULL,
    machine TEXT,
    workspace_id TEXT,
    pane_id TEXT,
    agent_name TEXT,
    state TEXT NOT NULL,
    error TEXT,
    last_completion_seq INTEGER,
    prompt_pending INTEGER NOT NULL DEFAULT 0,
    retry_of INTEGER,
    flock TEXT,
    trust_sent INTEGER NOT NULL DEFAULT 0,
    activity_seen INTEGER NOT NULL DEFAULT 0,
    ended INTEGER NOT NULL DEFAULT 0,
    priority TEXT NOT NULL DEFAULT 'normal',
    priority_from TEXT,
    queue_pos INTEGER,
    role TEXT NOT NULL DEFAULT 'agent',
    description TEXT,
    created_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS tasks_state ON tasks(state);
CREATE INDEX IF NOT EXISTS tasks_machine ON tasks(machine);
CREATE TABLE IF NOT EXISTS seen (
    job TEXT NOT NULL,
    key TEXT NOT NULL,
    task_id INTEGER,
    seen_at TEXT NOT NULL,
    PRIMARY KEY (job, key)
);
CREATE TABLE IF NOT EXISTS job_state (
    name TEXT PRIMARY KEY,
    last_run_at TEXT,
    last_ok_at TEXT,
    last_result TEXT,
    last_error TEXT,
    cursor TEXT,
    failures INTEGER NOT NULL DEFAULT 0,
    backoff_until TEXT
);
CREATE TABLE IF NOT EXISTS trusted_repos (
    machine TEXT NOT NULL,
    repo TEXT NOT NULL,
    trusted_at TEXT NOT NULL,
    PRIMARY KEY (machine, repo)
);
CREATE TABLE IF NOT EXISTS event_seq (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    last INTEGER NOT NULL
);
INSERT OR IGNORE INTO event_seq (id, last) VALUES (1, 0);
INSERT INTO meta (key, value) VALUES ('schema_version', '11');";

/// Schema 12: `src/store.rs` at 104fc649e655 (last commit at schema 12).
const V12: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS tasks (
    id INTEGER PRIMARY KEY,
    job TEXT NOT NULL,
    item TEXT NOT NULL,
    prompt TEXT NOT NULL,
    spec TEXT NOT NULL,
    machine TEXT,
    workspace_id TEXT,
    pane_id TEXT,
    agent_name TEXT,
    state TEXT NOT NULL,
    error TEXT,
    last_completion_seq INTEGER,
    prompt_pending INTEGER NOT NULL DEFAULT 0,
    retry_of INTEGER,
    flock TEXT,
    trust_sent INTEGER NOT NULL DEFAULT 0,
    activity_seen INTEGER NOT NULL DEFAULT 0,
    ended INTEGER NOT NULL DEFAULT 0,
    priority TEXT NOT NULL DEFAULT 'normal',
    priority_from TEXT,
    queue_pos INTEGER,
    role TEXT NOT NULL DEFAULT 'agent',
    description TEXT,
    preempt INTEGER NOT NULL DEFAULT 0,
    paused_at TEXT,
    paused_for INTEGER,
    resumed_at TEXT,
    created_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS tasks_state ON tasks(state);
CREATE INDEX IF NOT EXISTS tasks_machine ON tasks(machine);
CREATE TABLE IF NOT EXISTS seen (
    job TEXT NOT NULL,
    key TEXT NOT NULL,
    task_id INTEGER,
    seen_at TEXT NOT NULL,
    PRIMARY KEY (job, key)
);
CREATE TABLE IF NOT EXISTS job_state (
    name TEXT PRIMARY KEY,
    last_run_at TEXT,
    last_ok_at TEXT,
    last_result TEXT,
    last_error TEXT,
    cursor TEXT,
    failures INTEGER NOT NULL DEFAULT 0,
    backoff_until TEXT
);
CREATE TABLE IF NOT EXISTS trusted_repos (
    machine TEXT NOT NULL,
    repo TEXT NOT NULL,
    trusted_at TEXT NOT NULL,
    PRIMARY KEY (machine, repo)
);
CREATE TABLE IF NOT EXISTS event_seq (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    last INTEGER NOT NULL
);
INSERT OR IGNORE INTO event_seq (id, last) VALUES (1, 0);
INSERT INTO meta (key, value) VALUES ('schema_version', '12');";

/// Schema 13: `src/store.rs` at 9e4266f (last commit at schema 13).
const V13: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS tasks (
    id INTEGER PRIMARY KEY,
    job TEXT NOT NULL,
    item TEXT NOT NULL,
    prompt TEXT NOT NULL,
    spec TEXT NOT NULL,
    machine TEXT,
    workspace_id TEXT,
    pane_id TEXT,
    agent_name TEXT,
    state TEXT NOT NULL,
    error TEXT,
    last_completion_seq INTEGER,
    prompt_pending INTEGER NOT NULL DEFAULT 0,
    retry_of INTEGER,
    flock TEXT,
    trust_sent INTEGER NOT NULL DEFAULT 0,
    activity_seen INTEGER NOT NULL DEFAULT 0,
    ended INTEGER NOT NULL DEFAULT 0,
    priority TEXT NOT NULL DEFAULT 'normal',
    priority_from TEXT,
    queue_pos INTEGER,
    role TEXT NOT NULL DEFAULT 'agent',
    description TEXT,
    preempt INTEGER NOT NULL DEFAULT 0,
    paused_at TEXT,
    paused_for INTEGER,
    resumed_at TEXT,
    created_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS tasks_state ON tasks(state);
CREATE INDEX IF NOT EXISTS tasks_machine ON tasks(machine);
CREATE TABLE IF NOT EXISTS seen (
    job TEXT NOT NULL,
    key TEXT NOT NULL,
    task_id INTEGER,
    seen_at TEXT NOT NULL,
    PRIMARY KEY (job, key)
);
CREATE TABLE IF NOT EXISTS job_state (
    name TEXT PRIMARY KEY,
    last_run_at TEXT,
    last_ok_at TEXT,
    last_result TEXT,
    last_error TEXT,
    cursor TEXT,
    failures INTEGER NOT NULL DEFAULT 0,
    backoff_until TEXT
);
CREATE TABLE IF NOT EXISTS trusted_repos (
    machine TEXT NOT NULL,
    repo TEXT NOT NULL,
    trusted_at TEXT NOT NULL,
    PRIMARY KEY (machine, repo)
);
CREATE TABLE IF NOT EXISTS event_seq (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    last INTEGER NOT NULL
);
INSERT OR IGNORE INTO event_seq (id, last) VALUES (1, 0);
CREATE TABLE IF NOT EXISTS task_summaries (
    task_id INTEGER NOT NULL,
    round INTEGER NOT NULL,
    outcome TEXT NOT NULL,
    text TEXT NOT NULL,
    source TEXT NOT NULL,
    at TEXT NOT NULL,
    PRIMARY KEY (task_id, round)
);
INSERT INTO meta (key, value) VALUES ('schema_version', '13');";

/// Schema 14: `src/store.rs` at 491cf79 (last commit at schema 14).
const V14: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS tasks (
    id INTEGER PRIMARY KEY,
    job TEXT NOT NULL,
    item TEXT NOT NULL,
    prompt TEXT NOT NULL,
    spec TEXT NOT NULL,
    machine TEXT,
    workspace_id TEXT,
    pane_id TEXT,
    agent_name TEXT,
    state TEXT NOT NULL,
    error TEXT,
    last_completion_seq INTEGER,
    prompt_pending INTEGER NOT NULL DEFAULT 0,
    retry_of INTEGER,
    flock TEXT,
    trust_sent INTEGER NOT NULL DEFAULT 0,
    activity_seen INTEGER NOT NULL DEFAULT 0,
    ended INTEGER NOT NULL DEFAULT 0,
    priority TEXT NOT NULL DEFAULT 'normal',
    priority_from TEXT,
    queue_pos INTEGER,
    aged_from TEXT,
    aged_at TEXT,
    role TEXT NOT NULL DEFAULT 'agent',
    description TEXT,
    preempt INTEGER NOT NULL DEFAULT 0,
    paused_at TEXT,
    paused_for INTEGER,
    resumed_at TEXT,
    created_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS tasks_state ON tasks(state);
CREATE INDEX IF NOT EXISTS tasks_machine ON tasks(machine);
CREATE TABLE IF NOT EXISTS seen (
    job TEXT NOT NULL,
    key TEXT NOT NULL,
    task_id INTEGER,
    seen_at TEXT NOT NULL,
    PRIMARY KEY (job, key)
);
CREATE TABLE IF NOT EXISTS job_state (
    name TEXT PRIMARY KEY,
    last_run_at TEXT,
    last_ok_at TEXT,
    last_result TEXT,
    last_error TEXT,
    cursor TEXT,
    failures INTEGER NOT NULL DEFAULT 0,
    backoff_until TEXT
);
CREATE TABLE IF NOT EXISTS trusted_repos (
    machine TEXT NOT NULL,
    repo TEXT NOT NULL,
    trusted_at TEXT NOT NULL,
    PRIMARY KEY (machine, repo)
);
CREATE TABLE IF NOT EXISTS event_seq (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    last INTEGER NOT NULL
);
INSERT OR IGNORE INTO event_seq (id, last) VALUES (1, 0);
CREATE TABLE IF NOT EXISTS task_summaries (
    task_id INTEGER NOT NULL,
    round INTEGER NOT NULL,
    outcome TEXT NOT NULL,
    text TEXT NOT NULL,
    source TEXT NOT NULL,
    at TEXT NOT NULL,
    PRIMARY KEY (task_id, round)
);
INSERT INTO meta (key, value) VALUES ('schema_version', '14');";

/// Schema 15: `src/store.rs` at 8794195 (last commit at schema 15).
const V15: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS tasks (
    id INTEGER PRIMARY KEY,
    job TEXT NOT NULL,
    item TEXT NOT NULL,
    prompt TEXT NOT NULL,
    spec TEXT NOT NULL,
    machine TEXT,
    workspace_id TEXT,
    pane_id TEXT,
    agent_name TEXT,
    state TEXT NOT NULL,
    error TEXT,
    last_completion_seq INTEGER,
    prompt_pending INTEGER NOT NULL DEFAULT 0,
    retry_of INTEGER,
    flock TEXT,
    trust_sent INTEGER NOT NULL DEFAULT 0,
    activity_seen INTEGER NOT NULL DEFAULT 0,
    ended INTEGER NOT NULL DEFAULT 0,
    priority TEXT NOT NULL DEFAULT 'normal',
    priority_from TEXT,
    queue_pos INTEGER,
    aged_from TEXT,
    aged_at TEXT,
    role TEXT NOT NULL DEFAULT 'agent',
    description TEXT,
    preempt INTEGER NOT NULL DEFAULT 0,
    paused_at TEXT,
    paused_for INTEGER,
    resumed_at TEXT,
    waiting_until TEXT,
    created_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT,
    updated_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS tasks_state ON tasks(state);
CREATE INDEX IF NOT EXISTS tasks_machine ON tasks(machine);
CREATE TABLE IF NOT EXISTS seen (
    job TEXT NOT NULL,
    key TEXT NOT NULL,
    task_id INTEGER,
    seen_at TEXT NOT NULL,
    PRIMARY KEY (job, key)
);
CREATE TABLE IF NOT EXISTS job_state (
    name TEXT PRIMARY KEY,
    last_run_at TEXT,
    last_ok_at TEXT,
    last_result TEXT,
    last_error TEXT,
    cursor TEXT,
    failures INTEGER NOT NULL DEFAULT 0,
    backoff_until TEXT
);
CREATE TABLE IF NOT EXISTS trusted_repos (
    machine TEXT NOT NULL,
    repo TEXT NOT NULL,
    trusted_at TEXT NOT NULL,
    PRIMARY KEY (machine, repo)
);
CREATE TABLE IF NOT EXISTS event_seq (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    last INTEGER NOT NULL
);
INSERT OR IGNORE INTO event_seq (id, last) VALUES (1, 0);
CREATE TABLE IF NOT EXISTS task_summaries (
    task_id INTEGER NOT NULL,
    round INTEGER NOT NULL,
    outcome TEXT NOT NULL,
    text TEXT NOT NULL,
    source TEXT NOT NULL,
    at TEXT NOT NULL,
    PRIMARY KEY (task_id, round)
);
CREATE TABLE IF NOT EXISTS limits (
    account TEXT NOT NULL,
    model TEXT NOT NULL DEFAULT '',
    hard INTEGER NOT NULL,
    no_credit INTEGER NOT NULL DEFAULT 0,
    until TEXT,
    retry_at TEXT NOT NULL,
    line TEXT NOT NULL,
    task_id INTEGER,
    machine TEXT,
    agent TEXT,
    seen_at TEXT NOT NULL,
    PRIMARY KEY (account, model)
);
INSERT INTO meta (key, value) VALUES ('schema_version', '15');";

const OLD: [&str; 15] = [
    V1, V2, V3, V4, V5, V6, V7, V8, V9, V10, V11, V12, V13, V14, V15,
];

/// Everything about a database's shape and contents that a migration could
/// get wrong: each table's columns (by name, since `ALTER TABLE` appends
/// where a fresh `CREATE TABLE` has them in the middle), the indexes, and
/// every row.
/// A column as `PRAGMA table_info` gives it, less its position: name,
/// type, not null, default, primary key.
type Column = (String, String, bool, Option<String>, i64);

#[derive(Debug, PartialEq)]
struct Shape {
    /// table -> its columns, sorted
    columns: BTreeMap<String, Vec<Column>>,
    /// sorted (index, table, sql); `sql` is `None` for the automatic ones
    indexes: Vec<(String, String, Option<String>)>,
    schema_version: String,
}

fn shape(path: &Path) -> Shape {
    let conn = Connection::open(path).unwrap();
    let tables: Vec<String> = conn
        .prepare(
            "SELECT name FROM sqlite_master WHERE type = 'table' \
             AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let mut columns = BTreeMap::new();
    for t in tables {
        let mut cols: Vec<_> = conn
            .prepare(&format!("PRAGMA table_info({t})"))
            .unwrap()
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>("name")?,
                    r.get::<_, String>("type")?,
                    r.get::<_, bool>("notnull")?,
                    r.get::<_, Option<String>>("dflt_value")?,
                    r.get::<_, i64>("pk")?,
                ))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        cols.sort();
        columns.insert(t, cols);
    }
    let indexes = conn
        .prepare("SELECT name, tbl_name, sql FROM sqlite_master WHERE type = 'index' ORDER BY name")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let schema_version = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    Shape {
        columns,
        indexes,
        schema_version,
    }
}

/// Every row of every table, as text, for telling whether a second open
/// wrote anything.
fn rows(path: &Path) -> BTreeMap<String, Vec<Vec<Value>>> {
    let conn = Connection::open(path).unwrap();
    let tables: Vec<String> = conn
        .prepare(
            "SELECT name FROM sqlite_master WHERE type = 'table' \
             AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    tables
        .into_iter()
        .map(|t| {
            let mut stmt = conn
                .prepare(&format!("SELECT * FROM {t} ORDER BY 1"))
                .unwrap();
            let n = stmt.column_count();
            let rows = stmt
                .query_map([], |r| (0..n).map(|i| r.get::<_, Value>(i)).collect())
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            (t, rows)
        })
        .collect()
}

/// Each column a migration adds to `tasks`: the schema that added it, a
/// value unlike its default for a database that already had it, and the
/// raw value the migration should leave in the row of one that did not.
const ADDED: [(usize, &str, &str, &str); 18] = [
    (2, "prompt_pending", "1", "0"),
    (3, "retry_of", "5", "NULL"),
    (4, "flock", "'home'", "NULL"),
    (5, "trust_sent", "1", "0"),
    (6, "activity_seen", "1", "0"),
    (7, "ended", "1", "0"),
    (9, "priority", "'high'", "'normal'"),
    (9, "priority_from", "'job'", "NULL"),
    // placed by id when schema 9 adds it: the row's id is 7
    (9, "queue_pos", "3", "7"),
    (10, "role", "'orchestrator'", "'agent'"),
    (11, "description", "'the fix'", "NULL"),
    (12, "preempt", "1", "0"),
    (12, "paused_at", "'2026-09-01T11:00:00+00:00'", "NULL"),
    (12, "paused_for", "9", "NULL"),
    (12, "resumed_at", "'2026-09-01T11:05:00+00:00'", "NULL"),
    (14, "aged_from", "'low'", "NULL"),
    (14, "aged_at", "'2026-09-01T10:30:00+00:00'", "NULL"),
    (15, "waiting_until", "\'2026-09-01T12:00:00+00:00\'", "NULL"),
];

/// A database at schema `v`, as that pastor would have left it, holding
/// one task written with the columns every schema has, and a value unlike
/// the default in each column schema `v` already added.
fn old_database(path: &Path, v: usize) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(OLD[v - 1]).unwrap();
    conn.execute(
        "INSERT INTO tasks (id, job, item, prompt, spec, machine, state, \
         last_completion_seq, created_at, started_at, updated_at) \
         VALUES (7, 'nightly', '{\"n\":1}', 'fix it', ?1, 'pi-1', 'running', 42, \
         '2026-09-01T10:00:00+00:00', '2026-09-01T10:00:05+00:00', \
         '2026-09-01T10:00:05+00:00')",
        [r#"{"agent":"claude","agent_args":[],"repo":"/srv/repo","worktree":false,"branch":null,"machine":null,"tags":[],"timeout_secs":3600}"#],
    )
    .unwrap();
    for (since, col, seeded, _) in ADDED {
        if since <= v {
            conn.execute(
                &format!("UPDATE tasks SET {col} = {seeded} WHERE id = 7"),
                [],
            )
            .unwrap();
        }
    }
}

/// The raw value of `sql` (a literal) as SQLite stores it.
fn literal(conn: &Connection, sql: &str) -> Value {
    conn.query_row(&format!("SELECT {sql}"), [], |r| r.get(0))
        .unwrap()
}

#[test]
fn every_old_schema_migrates_to_the_fresh_one() {
    let tmp = tempfile::tempdir().unwrap();
    let fresh = tmp.path().join("fresh.db");
    drop(Store::open(&fresh).unwrap());
    let want = shape(&fresh);
    assert_eq!(
        want.schema_version, "16",
        "update this test for the new schema"
    );

    for v in 1..=OLD.len() {
        let path = tmp.path().join(format!("v{v}.db"));
        old_database(&path, v);
        let store = Store::open(&path).unwrap_or_else(|e| panic!("v{v}: {e:#}"));

        let task = store
            .get_task(7)
            .unwrap_or_else(|e| panic!("v{v}: {e:#}"))
            .unwrap_or_else(|| panic!("v{v}: task 7 is gone"));
        assert_eq!(task.job, "nightly", "v{v}");
        assert_eq!(task.prompt, "fix it", "v{v}");
        assert_eq!(task.item, serde_json::json!({"n": 1}), "v{v}");
        assert_eq!(task.spec.agent, "claude", "v{v}");
        assert_eq!(task.spec.repo.as_deref(), Some("/srv/repo"), "v{v}");
        assert_eq!(task.machine.as_deref(), Some("pi-1"), "v{v}");
        assert_eq!(task.state.to_string(), "running", "v{v}");
        assert_eq!(task.last_completion_seq, Some(42), "v{v}");
        drop(store);

        // The raw row, not `get_task`: that reads a NULL `queue_pos` as the
        // id, so a missing backfill would pass through it unseen.
        let conn = Connection::open(&path).unwrap();
        for (since, col, seeded, default) in ADDED {
            let want = literal(&conn, if since <= v { seeded } else { default });
            let got: Value = conn
                .query_row(&format!("SELECT {col} FROM tasks WHERE id = 7"), [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(got, want, "v{v}: {col}");
        }
        drop(conn);

        assert_eq!(shape(&path), want, "v{v} migrated is not the fresh schema");

        let before = rows(&path);
        drop(Store::open(&path).unwrap());
        assert_eq!(shape(&path), want, "v{v} opened twice");
        assert_eq!(rows(&path), before, "v{v}: a second open changed rows");
    }
}
