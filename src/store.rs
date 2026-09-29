use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::queue::QueueSpot;
use crate::sync::Recover;
use crate::task::{
    DispatchSpec, Outcome, PANE_OWNING_STATES, Priority, SummarySource, Task, TaskState,
    TaskSummary,
};

const SCHEMA_VERSION: i64 = 13;

/// The tables schema 2 added: created on a fresh database and by the v1
/// migration.
const V2_TABLES: &str = "CREATE TABLE IF NOT EXISTS seen (
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
     );";

/// Schema 5: the (machine, repo) pairs whose folder-trust prompt pastor
/// answers on its own. `repo` is a task's `--repo` as given, so every
/// worktree task of one repo shares its entry.
const V5_TABLES: &str = "CREATE TABLE IF NOT EXISTS trusted_repos (
        machine TEXT NOT NULL,
        repo TEXT NOT NULL,
        trusted_at TEXT NOT NULL,
        PRIMARY KEY (machine, repo)
     );";

/// Schema 8: the last sequence number given to an event record
/// (`Store::next_event_seq`), one row, so numbers keep growing across head
/// restarts.
const V8_TABLES: &str = "CREATE TABLE IF NOT EXISTS event_seq (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        last INTEGER NOT NULL
     );
     INSERT OR IGNORE INTO event_seq (id, last) VALUES (1, 0);";

/// Schema 12: how each round of a task ended (`TaskSummary`), one row per
/// round, numbered from 1 for each task.
const V13_TABLES: &str = "CREATE TABLE IF NOT EXISTS task_summaries (
        task_id INTEGER NOT NULL,
        round INTEGER NOT NULL,
        outcome TEXT NOT NULL,
        text TEXT NOT NULL,
        source TEXT NOT NULL,
        at TEXT NOT NULL,
        PRIMARY KEY (task_id, round)
     );";

/// A task row with its last round's summary as JSON (`summary_json`), which
/// `row_to_task` reads when the query has it.
const TASK_WITH_SUMMARY: &str = "SELECT tasks.*,
        (SELECT json_object('round', s.round, 'outcome', s.outcome, 'text', s.text,
                            'source', s.source, 'at', s.at)
           FROM task_summaries s WHERE s.task_id = tasks.id
          ORDER BY s.round DESC LIMIT 1) AS summary_json
     FROM tasks";

/// One saved trust, as `pastor trust list` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedRepo {
    pub machine: String,
    pub repo: String,
    pub trusted_at: DateTime<Utc>,
}

/// Runs a call that may wait on SQLite (the connection's lock, then up to
/// `BUSY_TIMEOUT` for another connection's) without holding up the tokio
/// worker it was made on: `block_in_place` hands the worker's other tasks to
/// another thread first. Store methods are called straight from async code
/// all over the head, so wrapping them here spares every call site a
/// `spawn_blocking`. Outside a multi-thread runtime (a CLI, a
/// `current_thread` test) the call runs as it is.
fn blocking<T>(f: impl FnOnce() -> T) -> T {
    use tokio::runtime::{Handle, RuntimeFlavor};
    match Handle::try_current() {
        Ok(h) if h.runtime_flavor() == RuntimeFlavor::MultiThread => tokio::task::block_in_place(f),
        _ => f(),
    }
}

pub struct Store {
    conn: Mutex<Connection>,
    /// The last lines read from a finishing task's pane, by task id; in
    /// memory only (see `note_pane_tail`).
    pane_tails: Mutex<PaneTails>,
}

/// Tails kept at once. Only a task's finish command asks for one, so most are
/// never taken; the oldest go first.
const PANE_TAILS_MAX: usize = 64;

/// Lines of a pane kept as a task's tail.
const PANE_TAIL_LINES: usize = 40;

#[derive(Default)]
struct PaneTails(std::collections::VecDeque<(i64, String)>);

/// `update_task` found the row changed since this copy was read. The caller holds
/// stale data; reload and decide again rather than overwrite.
/// What `Store::prune` did: how many rows it deleted, and the ids of old
/// rows it kept because their worktree may still be on disk.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneOutcome {
    pub pruned: usize,
    pub kept_worktrees: Vec<i64>,
}

#[derive(Debug, thiserror::Error)]
#[error("task t-{id} changed underneath this update; reload it and apply again")]
pub struct Conflict {
    pub id: i64,
}

/// Why `insert_retry` made no copy. Each has its own stable IPC code, so a
/// row pruned under a concurrent retry, or a storage failure, is not
/// reported as `not_retryable`.
#[derive(Debug, thiserror::Error)]
pub enum RetryError {
    #[error("task t-{0} not found")]
    NotFound(i64),
    #[error("t-{id} is {state}; only failed or stale tasks can be retried")]
    NotRetryable { id: i64, state: TaskState },
    #[error(transparent)]
    Store(#[from] anyhow::Error),
}

/// Why `move_queued` changed nothing, each with its own IPC code. The id
/// is the task moved or the one it was to go before or after.
#[derive(Debug, thiserror::Error)]
pub enum MoveError {
    #[error("task t-{0} not found")]
    NotFound(i64),
    #[error("t-{id} is {state}; only queued tasks have a place in the queue")]
    NotQueued { id: i64, state: TaskState },
    /// A dispatch pass placed it and is sending it to a machine
    /// (`Fleet::in_flight`): the row still says queued, but the move would
    /// not change where it goes.
    #[error("t-{0} is being sent to a machine; only queued tasks have a place in the queue")]
    InFlight(i64),
    #[error(transparent)]
    Store(#[from] anyhow::Error),
}

impl From<rusqlite::Error> for MoveError {
    fn from(err: rusqlite::Error) -> Self {
        MoveError::Store(err.into())
    }
}

/// What `move_queued` did: the task, its level before, and where it is now
/// (from 1) in a queue of `of` tasks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Moved {
    pub task: Task,
    pub was: Priority,
    pub pos: usize,
    pub of: usize,
}

/// The order dispatch takes queued tasks in: by level, highest first, then
/// a paused task before a queued one (only a `low` task is ever paused, so
/// it goes first among the `low` ones), then by position, then oldest
/// first.
const QUEUE_ORDER: &str = "CASE priority WHEN 'critical' THEN 3 WHEN 'high' THEN 2
                                  WHEN 'normal' THEN 1 ELSE 0 END DESC,
                         state = 'paused' DESC,
                         COALESCE(queue_pos, id), created_at, id";

/// Why `set_priority` changed nothing, each with its own IPC code.
#[derive(Debug, thiserror::Error)]
pub enum PriorityError {
    #[error("task t-{0} not found")]
    NotFound(i64),
    #[error("t-{id} is {state}; only a queued task's priority can change")]
    NotQueued { id: i64, state: TaskState },
    /// As `MoveError::InFlight`: the pass already decided on the old level
    /// and `preempt`, so the change would be reported but not applied.
    #[error("t-{0} is being sent to a machine; only a queued task's priority can change")]
    InFlight(i64),
    #[error(transparent)]
    Store(#[from] anyhow::Error),
}

impl From<rusqlite::Error> for PriorityError {
    fn from(err: rusqlite::Error) -> Self {
        PriorityError::Store(err.into())
    }
}

impl From<rusqlite::Error> for RetryError {
    fn from(err: rusqlite::Error) -> Self {
        RetryError::Store(err.into())
    }
}

#[derive(Debug, Clone)]
pub struct NewTask {
    pub job: String,
    pub item: Value,
    pub prompt: String,
    pub spec: DispatchSpec,
    /// The flock the task targets, already resolved (see
    /// `Flock::task_flock`): only its machines take the task.
    pub flock: String,
    /// `task run --description`, trimmed; `None` reads as the prompt's
    /// first line (`Task::description_text`).
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TaskFilter {
    pub job: Option<String>,
    #[serde(default)]
    pub flock: Option<String>,
    pub machine: Option<String>,
    pub states: Option<Vec<TaskState>>,
}

/// Per-job bookkeeping the scheduler needs across restarts.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct JobState {
    pub name: String,
    /// Start of the last attempt, successful or not; drives the schedule.
    pub last_run_at: Option<DateTime<Utc>>,
    /// Start of the last successful run; the next run's `since`.
    pub last_ok_at: Option<DateTime<Utc>>,
    pub last_result: Option<String>,
    pub last_error: Option<String>,
    pub cursor: Option<String>,
    pub failures: u32,
    pub backoff_until: Option<DateTime<Utc>>,
}

impl Store {
    /// How long one connection waits for another's lock before giving up.
    const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    pub fn open(path: &Path) -> anyhow::Result<Store> {
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.pragma_update(None, "busy_timeout", Self::BUSY_TIMEOUT.as_millis() as i64)?;
        // SQLite does not run the busy handler while it switches the journal
        // mode, so a lock another connection holds at that moment (a CLI that
        // opened the file a moment before `pastor serve` did, or the other
        // way round) fails the switch at once with "database is locked".
        // Retry it for as long as the busy handler would have waited.
        let deadline = std::time::Instant::now() + Self::BUSY_TIMEOUT;
        loop {
            match conn.pragma_update(None, "journal_mode", "WAL") {
                Ok(()) => break,
                Err(rusqlite::Error::SqliteFailure(e, _))
                    if matches!(
                        e.code,
                        rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                    ) && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(e) => return Err(e).context("switch the database to WAL"),
            }
        }
        Self::init(conn)
    }

    /// The store for a reader that must never write or wait long: shell
    /// completion. A missing file stays missing, nothing is migrated, and a
    /// store of another schema is an error rather than something to read.
    pub fn open_read_only(path: &Path) -> anyhow::Result<Store> {
        use rusqlite::OpenFlags;
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("open {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_millis(200))?;
        let version: String = conn.query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )?;
        anyhow::ensure!(
            version.parse::<i64>().ok() == Some(SCHEMA_VERSION),
            "schema {version}, not {SCHEMA_VERSION}"
        );
        Ok(Store {
            conn: Mutex::new(conn),
            pane_tails: Mutex::default(),
        })
    }

    pub fn open_in_memory() -> anyhow::Result<Store> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> anyhow::Result<Store> {
        // Read the version before writing anything: a database from a newer
        // pastor, or one whose version cannot be read, is refused untouched.
        // An immediate transaction keeps a second process from migrating the
        // same file between this read and the writes below.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let has_meta: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta')",
            [],
            |r| r.get(0),
        )?;
        // Only a missing meta table means a fresh database. One that exists
        // without the row has lost its version: of unknown shape, so refused.
        let version: Option<String> = if has_meta {
            let v = tx
                .query_row(
                    "SELECT value FROM meta WHERE key = 'schema_version'",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            Some(
                v.context("database has a meta table but no schema_version; refusing to touch it")?,
            )
        } else {
            None
        };
        // An unreadable version is not an old one: guessing would let the
        // newer-schema guard below be skipped on a database of unknown shape.
        let version = version
            .map(|v| {
                v.parse::<i64>()
                    .with_context(|| format!("database schema_version {v:?} is not a number"))
            })
            .transpose()?;
        match version {
            None => {
                tx.execute_batch(
                    "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
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
                     CREATE INDEX IF NOT EXISTS tasks_machine ON tasks(machine);",
                )?;
                tx.execute_batch(V2_TABLES)?;
                tx.execute_batch(V5_TABLES)?;
                tx.execute_batch(V8_TABLES)?;
                tx.execute_batch(V13_TABLES)?;
                tx.execute(
                    "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)",
                    params![SCHEMA_VERSION.to_string()],
                )?;
            }
            // A pastor from before the job tables already wrote version 2
            // without them, so a current file still gets them if missing.
            Some(v) if v == SCHEMA_VERSION => {
                tx.execute_batch(V2_TABLES)?;
                tx.execute_batch(V5_TABLES)?;
                tx.execute_batch(V8_TABLES)?;
                tx.execute_batch(V13_TABLES)?;
            }
            Some(v) if v < SCHEMA_VERSION => {
                // One `if v < N` block per migration. The job tables go in
                // first whatever the version: an early version-2 file may
                // lack them.
                //
                // All steps and the version bump share the transaction opened
                // above, so an interrupted start leaves the old version and
                // the old shape together. Each ALTER still checks for its
                // column first: a file half migrated by a pastor from before
                // the transaction has the column at the old version, and a
                // bare ALTER would fail on it at every start.
                tx.execute_batch(V2_TABLES)?;
                if v < 2 {
                    add_column(
                        &tx,
                        "prompt_pending",
                        "prompt_pending INTEGER NOT NULL DEFAULT 0",
                    )?;
                }
                if v < 3 {
                    add_column(&tx, "retry_of", "retry_of INTEGER")?;
                }
                // Rows from before flocks get none here: the store does not
                // know which flock is the default. `adopt_default_flock`
                // fills them in once the caller has read flock.toml.
                if v < 4 {
                    add_column(&tx, "flock", "flock TEXT")?;
                }
                // Saved folder trust, and whether a task has had its trust
                // keys sent (`claim_trust_sent`).
                if v < 5 {
                    tx.execute_batch(V5_TABLES)?;
                    add_column(&tx, "trust_sent", "trust_sent INTEGER NOT NULL DEFAULT 0")?;
                }
                // Whether pastor has seen the task's agent at work since the
                // prompt (`Task::activity_seen`), so a restart keeps it.
                if v < 6 {
                    add_column(
                        &tx,
                        "activity_seen",
                        "activity_seen INTEGER NOT NULL DEFAULT 0",
                    )?;
                }
                // Whether the agent said it is finished (`Task::ended`).
                if v < 7 {
                    add_column(&tx, "ended", "ended INTEGER NOT NULL DEFAULT 0")?;
                }
                // Event sequence numbers (`next_event_seq`). Records already
                // in the log have none and read as 0; the first new one is 1.
                if v < 8 {
                    tx.execute_batch(V8_TABLES)?;
                }
                // A task's level and its place in the queue
                // (`queued_tasks`). Rows from before are `normal`, placed
                // by id: the order they had.
                if v < 9 {
                    add_column(&tx, "priority", "priority TEXT NOT NULL DEFAULT 'normal'")?;
                    add_column(&tx, "priority_from", "priority_from TEXT")?;
                    add_column(&tx, "queue_pos", "queue_pos INTEGER")?;
                    tx.execute(
                        "UPDATE tasks SET queue_pos = id WHERE queue_pos IS NULL",
                        [],
                    )?;
                }
                // What the task's agent may change (`Task::role`); every
                // older row is a plain agent.
                if v < 10 {
                    add_column(&tx, "role", "role TEXT NOT NULL DEFAULT 'agent'")?;
                }
                // What the task is about (`Task::description`). Older rows
                // get none and read as their prompt's first line.
                if v < 11 {
                    add_column(&tx, "description", "description TEXT")?;
                }
                // Pausing a low task for a critical one (`Task::pause`):
                // whether a task may pause one, and when a paused one was
                // paused, for whom, and when it resumed. Older rows neither
                // pause nor were paused.
                if v < 12 {
                    add_column(&tx, "preempt", "preempt INTEGER NOT NULL DEFAULT 0")?;
                    add_column(&tx, "paused_at", "paused_at TEXT")?;
                    add_column(&tx, "paused_for", "paused_for INTEGER")?;
                    add_column(&tx, "resumed_at", "resumed_at TEXT")?;
                }
                // How each round of a task ended. Older tasks have no rows
                // and show no summary.
                if v < 13 {
                    tx.execute_batch(V13_TABLES)?;
                }
                tx.execute(
                    "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
                    params![SCHEMA_VERSION.to_string()],
                )?;
            }
            Some(v) => anyhow::bail!(
                "database schema {v} is newer than this pastor ({SCHEMA_VERSION}); refusing to touch it"
            ),
        }
        tx.commit()?;
        Ok(Store {
            conn: Mutex::new(conn),
            pane_tails: Mutex::default(),
        })
    }

    pub fn insert_task(&self, t: NewTask) -> anyhow::Result<Task> {
        self.insert_task_at(t, Priority::Normal, None, crate::task::TaskRole::Agent)
    }

    /// `insert_task` at `priority`, which `from` set (`Task::priority_from`),
    /// for a task of `role` (`task run --role`). The task goes last among
    /// its level's queued tasks: its position is its id.
    pub fn insert_task_at(
        &self,
        t: NewTask,
        priority: Priority,
        from: Option<&str>,
        role: crate::task::TaskRole,
    ) -> anyhow::Result<Task> {
        self.insert_task_preempting(t, priority, from, role, false)
    }

    /// `insert_task_at`, with `preempt` (`task run --preempt`): the caller
    /// has checked that `priority` is critical.
    pub fn insert_task_preempting(
        &self,
        t: NewTask,
        priority: Priority,
        from: Option<&str>,
        role: crate::task::TaskRole,
        preempt: bool,
    ) -> anyhow::Result<Task> {
        let now = Utc::now().to_rfc3339();
        blocking(|| {
            let mut conn = self.conn.lock().recover();
            let tx = conn.transaction()?;
            tx.execute(
            "INSERT INTO tasks (job, item, prompt, spec, flock, state, priority, priority_from, role, description, preempt, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6, ?7, ?8, ?9, ?11, ?10, ?10)",
            params![t.job, serde_json::to_string(&t.item)?, t.prompt, serde_json::to_string(&t.spec)?, t.flock, priority.as_str(), from, role.as_str(), t.description, now, preempt],
        )?;
            let id = tx.last_insert_rowid();
            place_last(&tx, id)?;
            tx.commit()?;
            drop(conn);
            self.get_task(id)?.context("task vanished after insert")
        })
    }

    /// A task the head handed this machine (`IpcRequest::TaskClaim`), as a
    /// queued row under the head's own id, so this machine's actor
    /// dispatches it as it would one of its own (`claim_task`) and every
    /// report names the head's row. A row already there (a claim this
    /// serve took before a restart) is left as it is and returned.
    pub fn adopt_claimed(&self, t: &Task) -> anyhow::Result<Task> {
        let now = Utc::now().to_rfc3339();
        blocking(|| {
            let mut conn = self.conn.lock().recover();
            let tx = conn.transaction()?;
            let n = tx.execute(
            "INSERT OR IGNORE INTO tasks (id, job, item, prompt, spec, flock, state, priority, priority_from, role, description, preempt, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'queued', ?7, ?8, ?9, ?10, ?11, ?12, ?12)",
            params![t.id, t.job, serde_json::to_string(&t.item)?, t.prompt, serde_json::to_string(&t.spec)?, t.flock, t.priority.as_str(), t.priority_from, t.role.as_str(), t.description, t.pause.preempt, now],
        )?;
            if n == 1 {
                place_last(&tx, t.id)?;
            }
            tx.commit()?;
            drop(conn);
            self.get_task(t.id)?.context("task vanished after insert")
        })
    }

    /// Delete task `id`'s row and its summaries: a claimed task whose end
    /// the head has heard of (`shepherd`), so the rows here do not pile up.
    pub fn forget_task(&self, id: i64) -> anyhow::Result<()> {
        blocking(|| {
            let mut conn = self.conn.lock().recover();
            let tx = conn.transaction()?;
            tx.execute("DELETE FROM task_summaries WHERE task_id = ?1", params![id])?;
            tx.execute("DELETE FROM tasks WHERE id = ?1", params![id])?;
            tx.commit()?;
            Ok(())
        })
    }

    /// End a round of task `id`: a new summary row, numbered one past its
    /// last. `summary` is what the agent said (`task done --summary`),
    /// capped at `SUMMARY_MAX` characters; with none (or only blanks) the
    /// round is `no summary` and keeps the pane's last lines, as far as
    /// pastor read them (`note_pane_tail`), marked `source = pane`.
    pub fn end_round(&self, id: i64, summary: Option<&str>) -> anyhow::Result<TaskSummary> {
        let row = self.summary_row(id, summary);
        self.insert_round(id, row)
    }

    /// `end_round` for a round a person ended by hand (`task done t-N`) on
    /// a task that requires a summary: `no summary`, with `ENDED_BY_HAND`
    /// in place of the pane's last lines.
    pub fn end_round_by_hand(&self, id: i64) -> anyhow::Result<TaskSummary> {
        let row = TaskSummary {
            round: 0,
            outcome: Outcome::NoSummary,
            text: crate::task::ENDED_BY_HAND.into(),
            source: SummarySource::Pane,
            at: Utc::now(),
        };
        self.insert_round(id, row)
    }

    /// Store `row` as task `id`'s next round.
    fn insert_round(&self, id: i64, row: TaskSummary) -> anyhow::Result<TaskSummary> {
        blocking(|| {
            let conn = self.conn.lock().recover();
            let round: u32 = conn.query_row(
                "INSERT INTO task_summaries (task_id, round, outcome, text, source, at)
             SELECT ?1, COALESCE(MAX(round), 0) + 1, ?2, ?3, ?4, ?5
               FROM task_summaries WHERE task_id = ?1
             RETURNING round",
                params![
                    id,
                    row.outcome.as_str(),
                    row.text,
                    row.source.as_str(),
                    row.at.to_rfc3339()
                ],
                |r| r.get(0),
            )?;
            Ok(TaskSummary { round, ..row })
        })
    }

    /// Put `summary` in place of task `id`'s last round's, for an agent
    /// that says what it did after pastor already found the task done. A
    /// task with no rounds yet gets its first.
    pub fn replace_last_summary(&self, id: i64, summary: &str) -> anyhow::Result<TaskSummary> {
        let row = self.summary_row(id, Some(summary));
        blocking(|| {
            let conn = self.conn.lock().recover();
            let round: Option<u32> = conn
                .query_row(
                    "UPDATE task_summaries SET outcome = ?2, text = ?3, source = ?4, at = ?5
                  WHERE task_id = ?1
                    AND round = (SELECT MAX(round) FROM task_summaries WHERE task_id = ?1)
                 RETURNING round",
                    params![
                        id,
                        row.outcome.as_str(),
                        row.text,
                        row.source.as_str(),
                        row.at.to_rfc3339()
                    ],
                    |r| r.get(0),
                )
                .optional()?;
            drop(conn);
            match round {
                Some(round) => Ok(TaskSummary { round, ..row }),
                None => self.end_round(id, Some(summary)),
            }
        })
    }

    /// Every round's summary of task `id`, the first first.
    pub fn summaries(&self, id: i64) -> anyhow::Result<Vec<TaskSummary>> {
        blocking(|| {
            let conn = self.conn.lock().recover();
            let mut stmt = conn.prepare(
                "SELECT round, outcome, text, source, at FROM task_summaries
              WHERE task_id = ?1 ORDER BY round",
            )?;
            let rows = stmt.query_map(params![id], |r| {
                let outcome: String = r.get(1)?;
                let source: String = r.get(3)?;
                let at: String = r.get(4)?;
                Ok(TaskSummary {
                    round: r.get(0)?,
                    outcome: outcome.parse().map_err(conversion_failure)?,
                    text: r.get(2)?,
                    source: match source.as_str() {
                        "agent" => SummarySource::Agent,
                        "pane" => SummarySource::Pane,
                        other => {
                            return Err(conversion_failure(format!("unknown source {other:?}")));
                        }
                    },
                    at: DateTime::parse_from_rfc3339(&at)
                        .map(|d| d.with_timezone(&Utc))
                        .map_err(conversion_failure)?,
                })
            })?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
    }

    /// The row `end_round` writes for task `id`, round not yet numbered.
    fn summary_row(&self, id: i64, summary: Option<&str>) -> TaskSummary {
        let at = Utc::now();
        match summary
            .map(crate::task::cap_summary)
            .filter(|s| !s.is_empty())
        {
            Some(text) => TaskSummary {
                round: 0,
                outcome: Outcome::parse(&text),
                text,
                source: SummarySource::Agent,
                at,
            },
            None => TaskSummary {
                round: 0,
                outcome: Outcome::NoSummary,
                text: self
                    .pane_tail(id)
                    .map(|t| crate::task::cap_pane_tail(&t))
                    .unwrap_or_default(),
                source: SummarySource::Pane,
                at,
            },
        }
    }

    /// Remember the last lines of `text`, what pastor read from `task_id`'s
    /// pane when it judged the task done, for the finish command of its
    /// connector (`take_pane_tail`). Kept in memory, not the database: it is
    /// pane text, it is only useful for the minute between the read and the
    /// finish, and a restart in between just leaves the command with none.
    pub fn note_pane_tail(&self, task_id: i64, text: &str) {
        let lines: Vec<&str> = text.trim_end().lines().collect();
        let tail = lines[lines.len().saturating_sub(PANE_TAIL_LINES)..].join("\n");
        let mut tails = self.pane_tails.lock().recover();
        tails.0.retain(|(id, _)| *id != task_id);
        if tails.0.len() >= PANE_TAILS_MAX {
            tails.0.pop_front();
        }
        tails.0.push_back((task_id, tail));
    }

    /// The tail `note_pane_tail` kept for `task_id`, left for the finish
    /// command to take (an orchestrator's post script reads it too).
    pub fn pane_tail(&self, task_id: i64) -> Option<String> {
        let tails = self.pane_tails.lock().recover();
        tails
            .0
            .iter()
            .find(|(id, _)| *id == task_id)
            .map(|(_, t)| t.clone())
    }

    /// The tail `note_pane_tail` kept for `task_id`, once.
    pub fn take_pane_tail(&self, task_id: i64) -> Option<String> {
        let mut tails = self.pane_tails.lock().recover();
        let at = tails.0.iter().position(|(id, _)| *id == task_id)?;
        tails.0.remove(at).map(|(_, t)| t)
    }

    /// The next event sequence number: one more than the last one given,
    /// saved before it is returned, so no number is given twice, across
    /// restarts too. The first is 1.
    pub fn next_event_seq(&self) -> anyhow::Result<u64> {
        blocking(|| {
            let conn = self.conn.lock().recover();
            let seq: i64 = conn.query_row(
                "UPDATE event_seq SET last = last + 1 WHERE id = 1 RETURNING last",
                [],
                |r| r.get(0),
            )?;
            Ok(seq as u64)
        })
    }

    /// The last event sequence number given; 0 before the first.
    pub fn last_event_seq(&self) -> anyhow::Result<u64> {
        blocking(|| {
            let conn = self.conn.lock().recover();
            let seq: i64 =
                conn.query_row("SELECT last FROM event_seq WHERE id = 1", [], |r| r.get(0))?;
            Ok(seq as u64)
        })
    }

    pub fn get_task(&self, id: i64) -> anyhow::Result<Option<Task>> {
        blocking(|| {
            let conn = self.conn.lock().recover();
            Ok(conn
                .query_row(
                    &format!("{TASK_WITH_SUMMARY} WHERE id = ?1"),
                    params![id],
                    row_to_task,
                )
                .optional()?)
        })
    }

    /// Optimistic: the write lands only if the row still carries `t.updated_at`.
    /// On success `t.updated_at` is advanced to the value written, so the same
    /// copy can be updated again. A row that moved on is a `Conflict`; a row
    /// that is gone is a plain error.
    pub fn update_task(&self, t: &mut Task) -> anyhow::Result<()> {
        // Strictly later than the value being replaced, so two writes within one
        // clock tick still produce distinct stamps and the next check can tell
        // them apart.
        let now = Utc::now().max(t.updated_at + chrono::Duration::nanoseconds(1));
        blocking(|| {
            let conn = self.conn.lock().recover();
            let n = conn.execute(
            "UPDATE tasks SET machine = ?2, workspace_id = ?3, pane_id = ?4, agent_name = ?5, state = ?6, error = ?7,
                last_completion_seq = ?8, started_at = ?9, finished_at = ?10, updated_at = ?11, prompt = ?12, spec = ?13,
                prompt_pending = ?14, activity_seen = ?16, ended = ?17,
                paused_at = ?18, paused_for = ?19, resumed_at = ?20
             WHERE id = ?1 AND updated_at = ?15",
            params![
                t.id,
                t.machine,
                t.workspace_id,
                t.pane_id,
                t.agent_name,
                t.state.as_str(),
                t.error,
                t.last_completion_seq.map(|v| v as i64),
                t.started_at.map(|d| d.to_rfc3339()),
                t.finished_at.map(|d| d.to_rfc3339()),
                now.to_rfc3339(),
                t.prompt,
                serde_json::to_string(&t.spec)?,
                t.prompt_pending,
                t.updated_at.to_rfc3339(),
                t.activity_seen,
                t.ended,
                t.pause.paused_at.map(|d| d.to_rfc3339()),
                t.pause.paused_for,
                t.pause.resumed_at.map(|d| d.to_rfc3339()),
            ],
        )?;
            if n == 1 {
                t.updated_at = now;
                return Ok(());
            }
            let exists: bool = conn.query_row(
                "SELECT COUNT(*) FROM tasks WHERE id = ?1",
                params![t.id],
                |r| r.get::<_, i64>(0),
            )? > 0;
            if exists {
                Err(Conflict { id: t.id }.into())
            } else {
                anyhow::bail!("task {} not found", t.id)
            }
        })
    }

    /// The one transition dispatch is allowed to make on its own, done in SQL so
    /// concurrent dispatch passes cannot both take a task: `queued` -> `starting`
    /// on `machine`, with the agent name dispatch will use. `None` means the task
    /// was not queued any more (or never existed). This is `Observed::DispatchStarting`
    /// as a conditional UPDATE; `task::next_state` keeps the rule readable.
    pub fn claim_task(&self, id: i64, machine: &str) -> anyhow::Result<Option<Task>> {
        let now = Utc::now().to_rfc3339();
        blocking(|| {
            let conn = self.conn.lock().recover();
            let n = conn.execute(
            "UPDATE tasks SET state = 'starting', machine = ?2, agent_name = ?3, error = NULL, updated_at = ?4
             WHERE id = ?1 AND state = 'queued'",
            params![id, machine, Task::agent_name_for(id), now],
        )?;
            drop(conn);
            if n == 0 {
                return Ok(None);
            }
            self.get_task(id)
        })
    }

    /// `claim_task` for a paused task: `paused` -> `starting` on the machine
    /// it was paused on, and only there, stamping `resumed_at`. `None` means
    /// it was not paused there any more (closed meanwhile, or unknown).
    pub fn claim_paused(&self, id: i64, machine: &str) -> anyhow::Result<Option<Task>> {
        let now = Utc::now().to_rfc3339();
        blocking(|| {
            let conn = self.conn.lock().recover();
            let n = conn.execute(
            "UPDATE tasks SET state = 'starting', agent_name = ?3, error = NULL, resumed_at = ?4, updated_at = ?4
             WHERE id = ?1 AND state = 'paused' AND machine = ?2",
            params![id, machine, Task::agent_name_for(id), now],
        )?;
            drop(conn);
            if n == 0 {
                return Ok(None);
            }
            self.get_task(id)
        })
    }

    /// Queue a fresh task that copies job, item, prompt, spec and flock from
    /// task `of`, with `retry_of = of`. A new row and id rather than a reset of the
    /// old one: the agent is named after the id, and the old agent `t-<of>`
    /// may still be alive on its machine (a stale task always is). Refused
    /// unless `of` is failed or stale. The `seen` row keeps pointing at `of`.
    ///
    /// A worktree task's retry drops the branch, even one the job named, so
    /// it gets its own (`pastor/t-<new id>`) and a new worktree. The one
    /// exception is a failed task that owns a checkout (dispatch recorded it
    /// in `spec.checkout`): the retry carries that checkout and the agent
    /// that owned it in `spec.reopen`, and `dispatch::reopenable` decides at
    /// dispatch whether to go back to it. A stale task's agent is still at
    /// work, so its retry carries nothing.
    pub fn insert_retry(&self, of: i64) -> Result<Task, RetryError> {
        self.insert_retry_placed(of, None)
    }

    /// `insert_retry`, with the copy's `place` replaced when one is given
    /// (`task retry --place`).
    pub fn insert_retry_placed(
        &self,
        of: i64,
        place: Option<&crate::task::Place>,
    ) -> Result<Task, RetryError> {
        let now = Utc::now().to_rfc3339();
        let patch = match place {
            Some(p) => serde_json::json!({ "place": p }),
            None => serde_json::json!({}),
        }
        .to_string();
        blocking(|| {
            let mut conn = self.conn.lock().recover();
            let tx = conn.transaction()?;
            // Check and copy in one statement, so a task closed, pruned or
            // finished by another writer in between is not retried. The copy
            // keeps the level, its role, description and where it came from,
            // but queues last in it.
            let n = tx.execute(
            "INSERT INTO tasks (job, item, prompt, spec, flock, role, description, state, retry_of, priority, priority_from, preempt, created_at, updated_at)
             SELECT job, item, prompt,
                    json_patch(json_remove(CASE WHEN COALESCE(json_extract(spec, '$.worktree'), 0) = 0
                         THEN json_remove(spec, '$.checkout', '$.reopen')
                         WHEN state = 'failed' AND json_extract(spec, '$.checkout') IS NOT NULL
                         THEN json_set(json_remove(spec, '$.branch', '$.checkout'), '$.reopen',
                                       json_object('branch', json_extract(spec, '$.checkout.branch'),
                                                   'path', json_extract(spec, '$.checkout.path'),
                                                   'agent', COALESCE(agent_name, 't-' || id)))
                         ELSE json_remove(spec, '$.branch', '$.checkout', '$.reopen') END,
                         '$.session_id', '$.label.name', '$.label.note'), ?3),
                    flock, role, description, 'queued', id, priority, priority_from, preempt, ?2, ?2 FROM tasks
             WHERE id = ?1 AND state IN ('failed', 'stale')",
            params![of, now, patch],
        )?;
            if n == 0 {
                let state: Option<String> = tx
                    .query_row("SELECT state FROM tasks WHERE id = ?1", params![of], |r| {
                        r.get(0)
                    })
                    .optional()?;
                return Err(match state {
                    None => RetryError::NotFound(of),
                    Some(state) => RetryError::NotRetryable {
                        id: of,
                        state: state.parse().map_err(anyhow::Error::msg)?,
                    },
                });
            }
            let id = tx.last_insert_rowid();
            place_last(&tx, id)?;
            if place.is_some() {
                // `agent_source.place_from` is read by `task describe`: an
                // overridden place explains itself as `task retry`, not as
                // whatever placed the failed run. A task queued before
                // `agent_source` existed has none to update.
                tx.execute(
                "UPDATE tasks SET spec = json_set(spec, '$.agent_source.place_from', 'task retry')
                 WHERE id = ?1 AND json_extract(spec, '$.agent_source') IS NOT NULL",
                params![id],
            )?;
            }
            tx.commit()?;
            drop(conn);
            Ok(self.get_task(id)?.context("task vanished after insert")?)
        })
    }

    /// Mark task `id` closed, keeping the finish time of a task that already
    /// finished (`done -> closed` is the end of the same cycle, not a new
    /// one). Closing a closed task returns it unchanged. Optimistic like
    /// `update_task`: a row that moves on between the read and the write is a
    /// `Conflict`. Only the row changes; herdr is the machine actor's job.
    pub fn close_task(&self, id: i64) -> anyhow::Result<Task> {
        let mut t = self
            .get_task(id)?
            .with_context(|| format!("task t-{id} not found"))?;
        if t.state == TaskState::Closed {
            return Ok(t);
        }
        t.state = TaskState::Closed;
        t.finished_at = t.finished_at.or_else(|| Some(Utc::now()));
        self.update_task(&mut t)?;
        Ok(t)
    }

    /// Close task `id` only if it is still `queued` (or `paused`), in one
    /// conditional UPDATE. A queued task has no machine yet, and a paused one
    /// no pane, so nothing but its row to close; `None` means it was not queued any more, typically because a
    /// dispatch claimed it in between (`claim_task`), and the close must then
    /// go through that machine. The counterpart of `claim_task`.
    pub fn close_queued(&self, id: i64) -> anyhow::Result<Option<Task>> {
        let now = Utc::now().to_rfc3339();
        blocking(|| {
            let conn = self.conn.lock().recover();
            let n = conn.execute(
            "UPDATE tasks SET state = 'closed', finished_at = COALESCE(finished_at, ?2), updated_at = ?2
             WHERE id = ?1 AND state IN ('queued', 'paused')",
            params![id, now],
        )?;
            drop(conn);
            if n == 0 {
                return Ok(None);
            }
            self.get_task(id)
        })
    }

    /// Delete tasks in `states` that finished more than `older_than` ago
    /// (`finished_at`, or `updated_at` for a row that never recorded one).
    /// Their `seen` rows stay, so the items never trigger again. Only
    /// finished states may be pruned, and never the newest task (so ids are
    /// not reused). A worktree task that still records a workspace stays
    /// too: a plain close leaves its checkout on disk, and the row is the
    /// only record of it. `task close --remove-worktree` clears the
    /// workspace, after which prune takes the row.
    pub fn prune(
        &self,
        states: &[TaskState],
        older_than: Duration,
    ) -> anyhow::Result<PruneOutcome> {
        if states.is_empty() {
            return Ok(PruneOutcome::default());
        }
        if let Some(s) = states.iter().find(|s| !s.is_prunable()) {
            anyhow::bail!("{s} tasks cannot be pruned; only done, failed and closed");
        }
        let cutoff = Utc::now()
            - chrono::Duration::from_std(older_than).context("--older-than is too large")?;
        let mut args: Vec<String> = vec![cutoff.to_rfc3339()];
        let placeholders: Vec<String> = states
            .iter()
            .map(|s| {
                args.push(s.as_str().to_string());
                format!("?{}", args.len())
            })
            .collect();
        // julianday parses the stored RFC 3339 text, offset and fraction
        // included; comparing the strings would not order them reliably.
        // The newest row always stays: `id` has no AUTOINCREMENT, so SQLite
        // gives the next task MAX(id) + 1, and deleting the newest row would
        // hand its id (and its agent name t-<id>) out again.
        let old = format!(
            "state IN ({})
             AND julianday(COALESCE(finished_at, updated_at)) < julianday(?1)
             AND id < (SELECT MAX(id) FROM tasks)",
            placeholders.join(",")
        );
        // `spec` is JSON text; `worktree` is left out of specs that predate
        // it, which is false.
        let on_disk = "COALESCE(json_extract(spec, '$.worktree'), 0) != 0
             AND workspace_id IS NOT NULL";
        // List and delete under one lock and transaction, so the list
        // matches what the delete left behind.
        blocking(|| {
            let mut conn = self.conn.lock().recover();
            let tx = conn.transaction()?;
            let kept_worktrees = tx
                .prepare(&format!(
                    "SELECT id FROM tasks WHERE {old} AND {on_disk} ORDER BY id"
                ))?
                .query_map(rusqlite::params_from_iter(args.iter()), |r| r.get(0))?
                .collect::<Result<Vec<i64>, _>>()?;
            let pruned = tx.execute(
                &format!("DELETE FROM tasks WHERE {old} AND NOT ({on_disk})"),
                rusqlite::params_from_iter(args.iter()),
            )?;
            tx.execute(
                "DELETE FROM task_summaries WHERE task_id NOT IN (SELECT id FROM tasks)",
                [],
            )?;
            tx.commit()?;
            Ok(PruneOutcome {
                pruned,
                kept_worktrees,
            })
        })
    }

    pub fn list_tasks(&self, f: &TaskFilter) -> anyhow::Result<Vec<Task>> {
        let mut sql = format!("{TASK_WITH_SUMMARY} WHERE 1=1");
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(job) = &f.job {
            args.push(Box::new(job.clone()));
            sql.push_str(&format!(" AND job = ?{}", args.len()));
        }
        if let Some(flock) = &f.flock {
            args.push(Box::new(flock.clone()));
            sql.push_str(&format!(" AND flock = ?{}", args.len()));
        }
        if let Some(m) = &f.machine {
            args.push(Box::new(m.clone()));
            sql.push_str(&format!(" AND machine = ?{}", args.len()));
        }
        if let Some(states) = &f.states {
            let placeholders: Vec<String> = states
                .iter()
                .map(|s| {
                    args.push(Box::new(s.as_str().to_string()));
                    format!("?{}", args.len())
                })
                .collect();
            sql.push_str(&format!(" AND state IN ({})", placeholders.join(",")));
        }
        sql.push_str(" ORDER BY id DESC");
        blocking(|| {
            let conn = self.conn.lock().recover();
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(
                rusqlite::params_from_iter(args.iter().map(|a| a.as_ref())),
                row_to_task,
            )?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
    }

    /// Open tasks that hold a pane on this machine (for capacity and reconciliation).
    ///
    /// Filters pane-owning states in SQL rather than loading every historical
    /// task for the machine and filtering in Rust: a machine with a long
    /// closed/failed history would otherwise read rows it never needed.
    pub fn tasks_on_machine(&self, machine: &str) -> anyhow::Result<Vec<Task>> {
        self.list_tasks(&TaskFilter {
            machine: Some(machine.into()),
            states: Some(PANE_OWNING_STATES.to_vec()),
            ..Default::default()
        })
    }

    /// `tasks_on_machine`, plus the failed tasks there that name an agent:
    /// a dispatch can fail after its agent started, and that agent may still
    /// be at work (an orphan to reconcile). For finding who works in a
    /// checkout, where a live agent counts whatever its row says.
    pub fn tasks_with_agents_on_machine(&self, machine: &str) -> anyhow::Result<Vec<Task>> {
        let mut states = PANE_OWNING_STATES.to_vec();
        states.push(TaskState::Failed);
        let tasks = self.list_tasks(&TaskFilter {
            machine: Some(machine.into()),
            states: Some(states),
            ..Default::default()
        })?;
        Ok(tasks
            .into_iter()
            .filter(|t| t.state != TaskState::Failed || t.agent_name.is_some())
            .collect())
    }

    /// Queued and paused tasks in the order dispatch takes them: by level,
    /// highest first, a paused task first in its level, then by position,
    /// then oldest first.
    pub fn queued_tasks(&self) -> anyhow::Result<Vec<Task>> {
        blocking(|| {
            let conn = self.conn.lock().recover();
            let mut stmt = conn.prepare(&format!(
                "SELECT * FROM tasks WHERE state IN ('queued', 'paused') ORDER BY {QUEUE_ORDER}"
            ))?;
            let rows = stmt.query_map([], row_to_task)?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
    }

    /// Put queued task `id` at `priority`, set by `from`. Refused unless the
    /// task is queued: one a machine took has left the queue. It keeps its
    /// position, so among its new level's tasks it goes by when it was
    /// queued.
    pub fn set_priority(
        &self,
        id: i64,
        priority: Priority,
        from: &str,
    ) -> Result<Task, PriorityError> {
        self.set_priority_preempting(id, priority, from, false)
    }

    /// `set_priority`, setting the task's `preempt` to `preempt` (`task
    /// priority --preempt`; without it the flag goes): the caller has
    /// checked that `priority` is critical when `preempt` is set.
    pub fn set_priority_preempting(
        &self,
        id: i64,
        priority: Priority,
        from: &str,
        preempt: bool,
    ) -> Result<Task, PriorityError> {
        let now = Utc::now().to_rfc3339();
        blocking(|| {
            let conn = self.conn.lock().recover();
            let n = conn.execute(
                "UPDATE tasks SET priority = ?2, priority_from = ?3, preempt = ?5, updated_at = ?4
             WHERE id = ?1 AND state = 'queued'",
                params![id, priority.as_str(), from, now, preempt],
            )?;
            if n == 0 {
                let state: Option<String> = conn
                    .query_row("SELECT state FROM tasks WHERE id = ?1", params![id], |r| {
                        r.get(0)
                    })
                    .optional()?;
                return Err(match state {
                    None => PriorityError::NotFound(id),
                    Some(state) => PriorityError::NotQueued {
                        id,
                        state: state.parse().map_err(anyhow::Error::msg)?,
                    },
                });
            }
            drop(conn);
            Ok(self.get_task(id)?.context("task vanished after update")?)
        })
    }

    /// Move queued task `id` to `spot` (`pastor queue move`). It takes the
    /// level of where it lands: lifted when the task behind it is higher,
    /// lowered when the one ahead is lower (at the top there is none ahead,
    /// so `--top` only lifts). Its level's queued tasks then share out the
    /// positions they held between them in their new order, so the moved
    /// one sits between its neighbours and a new task, placed by its id,
    /// still queues last. Refused unless both it and the task it is moved
    /// before or after are queued. `exclude` (a dispatch pass's in-flight
    /// task ids, `Fleet::in_flight`) drops those rows from the queue this
    /// works out positions and levels from, and from being a valid `before`
    /// or `after`: their row still says `queued`, but they are on their way
    /// out of it, and `--to`'s position must not count them either.
    pub fn move_queued(
        &self,
        id: i64,
        spot: QueueSpot,
        exclude: &[i64],
    ) -> Result<Moved, MoveError> {
        let now = Utc::now().to_rfc3339();
        blocking(|| {
            let mut conn = self.conn.lock().recover();
            let tx = conn.transaction()?;
            let mut queue: Vec<(i64, Priority, i64)> = {
                let mut stmt = tx.prepare(&format!(
                    "SELECT id, priority, COALESCE(queue_pos, id) FROM tasks
                 WHERE state = 'queued' ORDER BY {QUEUE_ORDER}"
                ))?;
                let rows = stmt.query_map([], |r| {
                    let p: String = r.get(1)?;
                    Ok((r.get(0)?, p.parse().unwrap_or_default(), r.get(2)?))
                })?;
                rows.collect::<Result<_, _>>()?
            };
            queue.retain(|(qid, ..)| !exclude.contains(qid));
            let refusal = |tx: &rusqlite::Transaction, id: i64| -> MoveError {
                if exclude.contains(&id) {
                    return MoveError::InFlight(id);
                }
                let state: rusqlite::Result<Option<String>> = tx
                    .query_row("SELECT state FROM tasks WHERE id = ?1", params![id], |r| {
                        r.get(0)
                    })
                    .optional();
                match state {
                    Err(err) => err.into(),
                    Ok(None) => MoveError::NotFound(id),
                    Ok(Some(state)) => match state.parse() {
                        Ok(state) => MoveError::NotQueued { id, state },
                        Err(err) => MoveError::Store(anyhow::anyhow!(err)),
                    },
                }
            };
            let index =
                |queue: &[(i64, Priority, i64)], id: i64| queue.iter().position(|q| q.0 == id);
            let Some(at) = index(&queue, id) else {
                return Err(refusal(&tx, id));
            };
            if let QueueSpot::Before(other) | QueueSpot::After(other) = spot
                && index(&queue, other).is_none()
            {
                return Err(refusal(&tx, other));
            }
            let (_, was, own) = queue.remove(at);
            let i = match spot {
                QueueSpot::Top => 0,
                QueueSpot::To(n) => n.saturating_sub(1).min(queue.len()),
                QueueSpot::Before(other) | QueueSpot::After(other) if other == id => at,
                QueueSpot::Before(other) => index(&queue, other).expect("checked"),
                QueueSpot::After(other) => index(&queue, other).expect("checked") + 1,
            };
            let mut level = was;
            if let Some(behind) = queue.get(i)
                && behind.1 > level
            {
                level = behind.1;
            }
            if i > 0 && queue[i - 1].1 < level {
                level = queue[i - 1].1;
            }
            queue.insert(i, (id, level, own));
            let mine: Vec<(i64, i64)> = queue
                .iter()
                .filter(|q| q.1 == level)
                .map(|q| (q.0, q.2))
                .collect();
            let mut slots: Vec<i64> = mine.iter().map(|m| m.1).collect();
            slots.sort_unstable();
            // Ties (a hand edit) would leave the order to age: make them strict.
            for k in 1..slots.len() {
                slots[k] = slots[k].max(slots[k - 1] + 1);
            }
            for ((task, pos), slot) in mine.iter().zip(slots) {
                if *pos != slot {
                    tx.execute(
                        "UPDATE tasks SET queue_pos = ?2 WHERE id = ?1",
                        params![task, slot],
                    )?;
                }
            }
            if level != was {
                tx.execute(
                    "UPDATE tasks SET priority = ?2, priority_from = 'queue move' WHERE id = ?1",
                    params![id, level.as_str()],
                )?;
            }
            tx.execute(
                "UPDATE tasks SET updated_at = ?2 WHERE id = ?1",
                params![id, now],
            )?;
            tx.commit()?;
            drop(conn);
            let task = self.get_task(id)?.context("task vanished after move")?;
            Ok(Moved {
                task,
                was,
                pos: i + 1,
                of: queue.len(),
            })
        })
    }

    /// Put every task that has no flock, a row from before flocks, in
    /// `default`: the default flock of flock.toml as the caller read it.
    /// Called wherever the store is opened with the flock file at hand, so
    /// such a row is only ever seen flockless between a migration and the
    /// first read of flock.toml. Returns how many rows it changed.
    pub fn adopt_default_flock(&self, default: &str) -> anyhow::Result<usize> {
        blocking(|| {
            let conn = self.conn.lock().recover();
            Ok(conn.execute(
                "UPDATE tasks SET flock = ?1 WHERE flock IS NULL",
                params![default],
            )?)
        })
    }

    pub fn find_by_pane(&self, machine: &str, pane_id: &str) -> anyhow::Result<Option<Task>> {
        Ok(self
            .tasks_on_machine(machine)?
            .into_iter()
            .find(|t| t.pane_id.as_deref() == Some(pane_id)))
    }

    pub fn job_state(&self, name: &str) -> anyhow::Result<Option<JobState>> {
        blocking(|| {
            let conn = self.conn.lock().recover();
            Ok(conn
                .query_row(
                    "SELECT * FROM job_state WHERE name = ?1",
                    params![name],
                    row_to_job_state,
                )
                .optional()?)
        })
    }

    pub fn job_states(&self) -> anyhow::Result<Vec<JobState>> {
        blocking(|| {
            let conn = self.conn.lock().recover();
            let mut stmt = conn.prepare("SELECT * FROM job_state ORDER BY name")?;
            let rows = stmt.query_map([], row_to_job_state)?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
    }

    pub fn save_job_state(&self, s: &JobState) -> anyhow::Result<()> {
        blocking(|| {
            let conn = self.conn.lock().recover();
            conn.execute(
            "INSERT OR REPLACE INTO job_state (name, last_run_at, last_ok_at, last_result, last_error, cursor, failures, backoff_until)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                s.name,
                s.last_run_at.map(|d| d.to_rfc3339()),
                s.last_ok_at.map(|d| d.to_rfc3339()),
                s.last_result,
                s.last_error,
                s.cursor,
                s.failures as i64,
                s.backoff_until.map(|d| d.to_rfc3339()),
            ],
        )?;
            Ok(())
        })
    }

    /// The task `(job, key)` was seen as: `None` when it is unseen,
    /// `Some(None)` when seen with no task id recorded.
    pub fn seen_task(&self, job: &str, key: &str) -> anyhow::Result<Option<Option<i64>>> {
        blocking(|| {
            let conn = self.conn.lock().recover();
            Ok(conn
                .query_row(
                    "SELECT task_id FROM seen WHERE job = ?1 AND key = ?2",
                    params![job, key],
                    |r| r.get(0),
                )
                .optional()?)
        })
    }

    pub fn is_seen(&self, job: &str, key: &str) -> anyhow::Result<bool> {
        blocking(|| {
            let conn = self.conn.lock().recover();
            let n: i64 = conn.query_row(
                "SELECT COUNT(*) FROM seen WHERE job = ?1 AND key = ?2",
                params![job, key],
                |r| r.get(0),
            )?;
            Ok(n > 0)
        })
    }

    /// Insert a queued task for `item`, render its prompt and spec with the id
    /// it was given, and record `(job, key)` as seen: one transaction, so no
    /// reader ever sees an unrendered task and a render failure leaves the key
    /// unseen. A key already in `seen` violates the primary key and nothing is
    /// written. `description` is the task's, already rendered; `None` reads
    /// as the prompt's first line.
    pub fn insert_job_task(
        &self,
        job: &str,
        flock: &str,
        item: &Value,
        description: Option<&str>,
        render: impl FnOnce(i64) -> Result<(String, DispatchSpec), String>,
    ) -> anyhow::Result<Task> {
        self.insert_job_task_at(
            job,
            flock,
            item,
            (Priority::Normal, None),
            false,
            description,
            render,
        )
    }

    /// `insert_job_task` at a level, and what set it (`Task::priority_from`).
    #[allow(clippy::too_many_arguments)]
    pub fn insert_job_task_at(
        &self,
        job: &str,
        flock: &str,
        item: &Value,
        (priority, from): (Priority, Option<&str>),
        preempt: bool,
        description: Option<&str>,
        render: impl FnOnce(i64) -> Result<(String, DispatchSpec), String>,
    ) -> anyhow::Result<Task> {
        let key = item
            .get("key")
            .and_then(Value::as_str)
            .context("item has no string key")?
            .to_string();
        let now = Utc::now().to_rfc3339();
        blocking(|| {
            let mut conn = self.conn.lock().recover();
            let tx = conn.transaction()?;
            tx.execute(
            "INSERT INTO tasks (job, item, prompt, spec, flock, description, state, priority, priority_from, preempt, created_at, updated_at) VALUES (?1, ?2, '', '{}', ?3, ?7, 'queued', ?5, ?6, ?8, ?4, ?4)",
            params![job, serde_json::to_string(item)?, flock, now, priority.as_str(), from, description, preempt],
        )?;
            let id = tx.last_insert_rowid();
            place_last(&tx, id)?;
            let (prompt, spec) =
                render(id).map_err(|e| anyhow::anyhow!("render task t-{id} for job {job}: {e}"))?;
            tx.execute(
                "UPDATE tasks SET prompt = ?2, spec = ?3 WHERE id = ?1",
                params![id, prompt, serde_json::to_string(&spec)?],
            )?;
            tx.execute(
                "INSERT INTO seen (job, key, task_id, seen_at) VALUES (?1, ?2, ?3, ?4)",
                params![job, key, id, now],
            )?;
            tx.commit()?;
            drop(conn);
            self.get_task(id)?.context("task vanished after insert")
        })
    }

    /// Test-only escape hatch to corrupt rows directly and check that reads
    /// surface it instead of reinterpreting it.
    #[cfg(test)]
    pub(crate) fn execute_raw(&self, sql: &str) {
        self.conn.lock().recover().execute_batch(sql).unwrap();
    }

    /// Save `repo` on `machine` as trusted. Returns whether it was new.
    pub fn trust_repo(&self, machine: &str, repo: &str) -> anyhow::Result<bool> {
        blocking(|| {
            let conn = self.conn.lock().recover();
            let n = conn.execute(
            "INSERT OR IGNORE INTO trusted_repos (machine, repo, trusted_at) VALUES (?1, ?2, ?3)",
            params![machine, repo, Utc::now().to_rfc3339()],
        )?;
            Ok(n == 1)
        })
    }

    pub fn is_trusted(&self, machine: &str, repo: &str) -> anyhow::Result<bool> {
        blocking(|| {
            let conn = self.conn.lock().recover();
            Ok(conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM trusted_repos WHERE machine = ?1 AND repo = ?2)",
                params![machine, repo],
                |r| r.get(0),
            )?)
        })
    }

    /// Every saved trust, by machine then repo.
    pub fn trusted_repos(&self) -> anyhow::Result<Vec<TrustedRepo>> {
        blocking(|| {
            let conn = self.conn.lock().recover();
            let mut stmt = conn.prepare(
                "SELECT machine, repo, trusted_at FROM trusted_repos ORDER BY machine, repo",
            )?;
            let rows = stmt.query_map([], |r| {
                let at: String = r.get(2)?;
                Ok(TrustedRepo {
                    machine: r.get(0)?,
                    repo: r.get(1)?,
                    trusted_at: DateTime::parse_from_rfc3339(&at)
                        .map(|d| d.with_timezone(&Utc))
                        .map_err(conversion_failure)?,
                })
            })?;
            Ok(rows.collect::<Result<_, _>>()?)
        })
    }

    /// Forget a saved trust. Returns whether there was one.
    pub fn untrust(&self, machine: &str, repo: &str) -> anyhow::Result<bool> {
        blocking(|| {
            let conn = self.conn.lock().recover();
            let n = conn.execute(
                "DELETE FROM trusted_repos WHERE machine = ?1 AND repo = ?2",
                params![machine, repo],
            )?;
            Ok(n == 1)
        })
    }

    /// Whether task `id` has had its trust keys sent. False for no such row.
    pub fn trust_sent(&self, id: i64) -> anyhow::Result<bool> {
        blocking(|| {
            let conn = self.conn.lock().recover();
            Ok(conn
                .query_row(
                    "SELECT trust_sent FROM tasks WHERE id = ?1",
                    params![id],
                    |r| r.get::<_, bool>(0),
                )
                .optional()?
                .unwrap_or(false))
        })
    }

    /// Mark task `id` as having had its trust keys sent. True only for the
    /// call that set it, so the keys go to a task once, across restarts.
    /// `update_task` never writes the column, so no stale copy resets it.
    pub fn claim_trust_sent(&self, id: i64) -> anyhow::Result<bool> {
        blocking(|| {
            let conn = self.conn.lock().recover();
            let n = conn.execute(
                "UPDATE tasks SET trust_sent = 1 WHERE id = ?1 AND trust_sent = 0",
                params![id],
            )?;
            Ok(n == 1)
        })
    }

    /// Record `key` as seen for `job` without a task row here: a headless
    /// serve's item became task `task_id` on the head, whose store holds
    /// the row, or `None` when the head no longer has it. Seen already is
    /// not an error.
    pub fn mark_seen(&self, job: &str, key: &str, task_id: Option<i64>) -> anyhow::Result<()> {
        blocking(|| {
            let conn = self.conn.lock().recover();
            conn.execute(
                "INSERT OR IGNORE INTO seen (job, key, task_id, seen_at) VALUES (?1, ?2, ?3, ?4)",
                params![job, key, task_id, Utc::now().to_rfc3339()],
            )?;
            Ok(())
        })
    }

    /// Set `key` in the meta table, where the schema version lives too.
    pub fn set_meta(&self, key: &str, value: &str) -> anyhow::Result<()> {
        anyhow::ensure!(key != "schema_version", "schema_version is not a setting");
        blocking(|| {
            let conn = self.conn.lock().recover();
            conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = ?2",
            params![key, value],
        )?;
            Ok(())
        })
    }

    pub fn meta(&self, key: &str) -> anyhow::Result<Option<String>> {
        blocking(|| {
            let conn = self.conn.lock().recover();
            Ok(conn
                .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                    r.get(0)
                })
                .optional()?)
        })
    }
}

/// Give new task `id` its position: its id, so it queues after every task
/// of its level already there.
fn place_last(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute("UPDATE tasks SET queue_pos = id WHERE id = ?1", params![id])?;
    Ok(())
}

/// `ALTER TABLE tasks ADD COLUMN <definition>` unless `tasks` already has
/// `name`, so a migration step can run again on a file it half changed.
fn add_column(conn: &Connection, name: &str, definition: &str) -> anyhow::Result<()> {
    let present: bool = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('tasks') WHERE name = ?1",
        params![name],
        |r| r.get::<_, i64>(0),
    )? > 0;
    if !present {
        conn.execute(&format!("ALTER TABLE tasks ADD COLUMN {definition}"), [])?;
    }
    Ok(())
}

/// A corrupt database must be surfaced, never silently reinterpreted: any column
/// that fails to parse turns into a `FromSqlConversionFailure` carrying the
/// offending value, instead of falling back to a default.
fn conversion_failure<E>(e: E) -> rusqlite::Error
where
    E: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, e.into())
}

/// The last round's summary a task row was read with (`TASK_WITH_SUMMARY`),
/// on a task that is done, failed or closed: a round still going has none
/// yet, and an older one says nothing about it. `None` from a query without
/// the column.
fn summary_of(row: &Row<'_>, state: &str) -> rusqlite::Result<Option<TaskSummary>> {
    if !matches!(state, "done" | "failed" | "closed") {
        return Ok(None);
    }
    let Ok(json) = row.get::<_, Option<String>>("summary_json") else {
        return Ok(None);
    };
    json.map(|j| serde_json::from_str(&j).map_err(conversion_failure))
        .transpose()
}

fn row_to_task(row: &Row<'_>) -> rusqlite::Result<Task> {
    let parse_dt = |s: &str| -> rusqlite::Result<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(s)
            .map(|d| d.with_timezone(&Utc))
            .map_err(conversion_failure)
    };
    let item: String = row.get("item")?;
    let spec: String = row.get("spec")?;
    let state: String = row.get("state")?;
    let priority: String = row.get("priority")?;
    let created_at: String = row.get("created_at")?;
    let updated_at: String = row.get("updated_at")?;
    let started_at: Option<String> = row.get("started_at")?;
    let finished_at: Option<String> = row.get("finished_at")?;
    let role: String = row.get("role")?;
    let paused_at: Option<String> = row.get("paused_at")?;
    let resumed_at: Option<String> = row.get("resumed_at")?;
    Ok(Task {
        id: row.get("id")?,
        job: row.get("job")?,
        item: serde_json::from_str(&item).map_err(conversion_failure)?,
        prompt: row.get("prompt")?,
        spec: serde_json::from_str(&spec).map_err(conversion_failure)?,
        machine: row.get("machine")?,
        workspace_id: row.get("workspace_id")?,
        pane_id: row.get("pane_id")?,
        agent_name: row.get("agent_name")?,
        state: state
            .parse()
            .map_err(|_| conversion_failure(format!("unknown task state {state:?}")))?,
        error: row.get("error")?,
        last_completion_seq: row
            .get::<_, Option<i64>>("last_completion_seq")?
            .map(u64::try_from)
            .transpose()
            .map_err(conversion_failure)?,
        prompt_pending: row.get("prompt_pending")?,
        activity_seen: row.get("activity_seen")?,
        ended: row.get("ended")?,
        retry_of: row.get("retry_of")?,
        flock: row.get("flock")?,
        priority: priority
            .parse()
            .map_err(|_| conversion_failure(format!("unknown task priority {priority:?}")))?,
        priority_from: row.get("priority_from")?,
        queue_pos: row
            .get::<_, Option<i64>>("queue_pos")?
            .unwrap_or(row.get("id")?),
        role: role.parse().map_err(conversion_failure::<String>)?,
        description: row.get("description")?,
        pause: crate::task::Preemption {
            preempt: row.get("preempt")?,
            paused_at: paused_at.as_deref().map(parse_dt).transpose()?,
            paused_for: row.get("paused_for")?,
            resumed_at: resumed_at.as_deref().map(parse_dt).transpose()?,
        },
        summary: summary_of(row, &state)?,
        created_at: parse_dt(&created_at)?,
        started_at: started_at.as_deref().map(parse_dt).transpose()?,
        finished_at: finished_at.as_deref().map(parse_dt).transpose()?,
        updated_at: parse_dt(&updated_at)?,
    })
}

fn row_to_job_state(row: &Row<'_>) -> rusqlite::Result<JobState> {
    let parse_dt = |s: Option<String>| -> rusqlite::Result<Option<DateTime<Utc>>> {
        s.as_deref()
            .map(|s| {
                DateTime::parse_from_rfc3339(s)
                    .map(|d| d.with_timezone(&Utc))
                    .map_err(conversion_failure)
            })
            .transpose()
    };
    Ok(JobState {
        name: row.get("name")?,
        last_run_at: parse_dt(row.get("last_run_at")?)?,
        last_ok_at: parse_dt(row.get("last_ok_at")?)?,
        last_result: row.get("last_result")?,
        last_error: row.get("last_error")?,
        cursor: row.get("cursor")?,
        failures: {
            let idx = row.as_ref().column_index("failures")?;
            u32::try_from(row.get::<_, i64>(idx)?).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    idx,
                    rusqlite::types::Type::Integer,
                    format!("failures: {e}").into(),
                )
            })?
        },
        backoff_until: parse_dt(row.get("backoff_until")?)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{Checkout, Reopen, TaskRole};

    /// A panic while a request holds the connection poisons its lock; later
    /// requests still get the connection instead of panicking until restart.
    #[test]
    fn store_works_after_a_panic_under_its_lock() {
        let s = std::sync::Arc::new(Store::open_in_memory().unwrap());
        let held = std::sync::Arc::clone(&s);
        let r = std::thread::spawn(move || {
            let _conn = held.conn.lock().unwrap();
            panic!("panic while holding the store lock");
        })
        .join();
        assert!(r.is_err());
        assert!(s.conn.is_poisoned());
        s.mark_seen("j", "k", None).unwrap();
        assert!(s.is_seen("j", "k").unwrap());
    }

    /// A store call stuck on a busy database (here another connection's
    /// write lock, which `busy_timeout` waits out for up to 5s) leaves the
    /// tokio worker it was called on free for other tasks. With one worker,
    /// an inline wait would hold back every other request until it ended.
    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn a_slow_store_call_does_not_stall_other_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.db");
        let s = std::sync::Arc::new(Store::open(&path).unwrap());
        let other = Connection::open(&path).unwrap();
        other.execute_batch("BEGIN IMMEDIATE").unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let writer = {
            let s = std::sync::Arc::clone(&s);
            tokio::spawn(async move {
                started_tx.send(()).unwrap();
                s.trust_repo("a", "/r")
            })
        };
        started_rx.await.unwrap();
        // The wall clock, not tokio's: its timers stall with the worker.
        std::thread::sleep(std::time::Duration::from_millis(100));
        let asked = std::time::Instant::now();
        assert_eq!(tokio::spawn(async { 7 }).await.unwrap(), 7);
        let waited = asked.elapsed();
        other.execute_batch("COMMIT").unwrap();
        assert!(
            waited < std::time::Duration::from_secs(2),
            "an unrelated task waited {waited:?} for the store"
        );
        assert!(writer.await.unwrap().unwrap());
    }

    /// A headless serve keeps seen keys and its event cursor with no task
    /// rows of its own.
    #[test]
    fn seen_keys_and_settings_without_tasks() {
        let s = Store::open_in_memory().unwrap();
        s.mark_seen("j", "k1", Some(7)).unwrap();
        s.mark_seen("j", "k1", Some(8)).unwrap();
        s.mark_seen("j", "k3", None).unwrap();
        assert!(s.is_seen("j", "k1").unwrap());
        assert!(!s.is_seen("j", "k2").unwrap());
        assert_eq!(s.seen_task("j", "k1").unwrap(), Some(Some(7)));
        assert_eq!(s.seen_task("j", "k2").unwrap(), None);
        assert_eq!(s.seen_task("j", "k3").unwrap(), Some(None));
        assert_eq!(s.meta("head_event_seq").unwrap(), None);
        s.set_meta("head_event_seq", "12").unwrap();
        s.set_meta("head_event_seq", "13").unwrap();
        assert_eq!(s.meta("head_event_seq").unwrap().as_deref(), Some("13"));
        assert!(s.set_meta("schema_version", "1").is_err());
        assert_eq!(
            s.meta("schema_version").unwrap(),
            Some(SCHEMA_VERSION.to_string())
        );
    }

    #[test]
    fn a_pane_tail_is_the_last_lines_and_taken_once() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.take_pane_tail(1), None);
        let text: String = (1..=100).map(|n| format!("line {n}\n")).collect();
        s.note_pane_tail(1, &text);
        let tail = s.take_pane_tail(1).unwrap();
        assert_eq!(tail.lines().count(), PANE_TAIL_LINES);
        assert!(
            tail.starts_with("line 61\n") && tail.ends_with("line 100"),
            "{tail}"
        );
        assert_eq!(s.take_pane_tail(1), None, "taken once");
        s.note_pane_tail(2, "a");
        s.note_pane_tail(2, "b");
        assert_eq!(s.take_pane_tail(2).as_deref(), Some("b"), "the newest read");
    }

    #[test]
    fn pane_tails_are_bounded_and_the_oldest_go() {
        let s = Store::open_in_memory().unwrap();
        for id in 0..(PANE_TAILS_MAX as i64 + 5) {
            s.note_pane_tail(id, "x");
        }
        assert_eq!(s.take_pane_tail(0), None);
        assert_eq!(s.take_pane_tail(4), None);
        assert!(s.take_pane_tail(5).is_some());
        assert!(s.take_pane_tail(PANE_TAILS_MAX as i64 + 4).is_some());
    }

    fn spec() -> DispatchSpec {
        DispatchSpec {
            agent: "claude".into(),
            agent_args: vec!["--model".into(), "x".into()],
            allow: vec![],
            deny: vec![],
            repo: Some("~/w".into()),
            worktree: true,
            branch: Some("b".into()),
            machine: None,
            tags: vec!["fast".into()],
            timeout_secs: 60,
            checkout: None,
            reopen: None,
            agent_source: None,
            place: Default::default(),
            session_id: None,
            label: Default::default(),
            summary: Default::default(),
            cwd: None,
        }
    }

    fn new_task(job: &str) -> NewTask {
        NewTask {
            job: job.into(),
            item: serde_json::json!({"key": "k1", "title": "t"}),
            prompt: "do it\nnow \"quoted\" {{ x }}".into(),
            spec: spec(),
            flock: "default".into(),
            description: None,
        }
    }

    #[test]
    fn insert_get_update_roundtrip() {
        let s = Store::open_in_memory().unwrap();
        let t = s.insert_task(new_task("run")).unwrap();
        assert_eq!(t.id, 1);
        assert_eq!(t.state, TaskState::Queued);
        assert_eq!(t.display_id(), "t-1");
        let mut got = s.get_task(1).unwrap().unwrap();
        assert_eq!(got.prompt, "do it\nnow \"quoted\" {{ x }}");
        assert_eq!(got.spec, spec());
        assert_eq!(got.item["key"], "k1");
        got.state = TaskState::Running;
        got.machine = Some("pi-3".into());
        got.pane_id = Some("w2:p1".into());
        got.agent_name = Some("t-1".into());
        got.last_completion_seq = Some(4);
        got.started_at = Some(Utc::now());
        s.update_task(&mut got).unwrap();
        let again = s.get_task(1).unwrap().unwrap();
        assert_eq!(again.state, TaskState::Running);
        assert_eq!(again.machine.as_deref(), Some("pi-3"));
        assert_eq!(again.last_completion_seq, Some(4));
        assert!(again.updated_at >= got.updated_at);
        assert!(s.get_task(99).unwrap().is_none());
    }

    #[test]
    fn filters_and_helpers() {
        let s = Store::open_in_memory().unwrap();
        let mut a = s.insert_task(new_task("slack")).unwrap();
        let mut b = s.insert_task(new_task("slack")).unwrap();
        let c = s.insert_task(new_task("asana")).unwrap();
        a.state = TaskState::Running;
        a.machine = Some("pi-3".into());
        a.pane_id = Some("w1:p1".into());
        b.state = TaskState::Closed;
        b.machine = Some("pi-3".into());
        b.pane_id = Some("w2:p1".into());
        s.update_task(&mut a).unwrap();
        s.update_task(&mut b).unwrap();
        let newest_first = s.list_tasks(&TaskFilter::default()).unwrap();
        assert_eq!(
            newest_first.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![3, 2, 1]
        );
        assert_eq!(
            s.list_tasks(&TaskFilter {
                job: Some("slack".into()),
                ..Default::default()
            })
            .unwrap()
            .len(),
            2
        );
        assert_eq!(
            s.list_tasks(&TaskFilter {
                states: Some(vec![TaskState::Running, TaskState::Closed]),
                ..Default::default()
            })
            .unwrap()
            .len(),
            2
        );
        assert_eq!(
            s.tasks_on_machine("pi-3")
                .unwrap()
                .iter()
                .map(|t| t.id)
                .collect::<Vec<_>>(),
            vec![1]
        );
        assert_eq!(
            s.queued_tasks()
                .unwrap()
                .iter()
                .map(|t| t.id)
                .collect::<Vec<_>>(),
            vec![c.id]
        );
        assert_eq!(s.find_by_pane("pi-3", "w1:p1").unwrap().unwrap().id, 1);
        assert!(s.find_by_pane("pi-3", "w2:p1").unwrap().is_none());
    }

    #[test]
    fn tasks_on_machine_filters_pane_owning_states_in_sql() {
        let s = Store::open_in_memory().unwrap();
        let mut open = s.insert_task(new_task("run")).unwrap();
        let mut closed = s.insert_task(new_task("run")).unwrap();
        let mut failed = s.insert_task(new_task("run")).unwrap();
        open.state = TaskState::Running;
        open.machine = Some("pi-3".into());
        closed.state = TaskState::Closed;
        closed.machine = Some("pi-3".into());
        failed.state = TaskState::Failed;
        failed.machine = Some("pi-3".into());
        s.update_task(&mut open).unwrap();
        s.update_task(&mut closed).unwrap();
        s.update_task(&mut failed).unwrap();

        let on_machine = s.tasks_on_machine("pi-3").unwrap();
        assert_eq!(
            on_machine.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![open.id],
            "closed and failed history must not come back from tasks_on_machine"
        );

        // The closed row is corrupted so a read that touched it would fail; a
        // filter applied in Rust after loading every row would trip this.
        s.execute_raw(&format!(
            "UPDATE tasks SET state = 'not-a-real-state' WHERE id = {}",
            closed.id
        ));
        let still_open = s.tasks_on_machine("pi-3").unwrap();
        assert_eq!(
            still_open.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![open.id],
            "the SQL filter must exclude the closed row before it is decoded"
        );
    }

    #[test]
    fn corrupt_rows_are_reported_not_reinterpreted() {
        let s = Store::open_in_memory().unwrap();
        s.insert_task(new_task("run")).unwrap();

        s.execute_raw("UPDATE tasks SET state = 'bogus' WHERE id = 1");
        let err = s.get_task(1).unwrap_err();
        assert!(err.to_string().contains("bogus"), "error was: {err}");

        s.execute_raw("UPDATE tasks SET state = 'queued', created_at = 'not a date' WHERE id = 1");
        assert!(s.get_task(1).is_err());

        s.execute_raw(
            "UPDATE tasks SET created_at = '2024-01-01T00:00:00Z', item = '{not json' WHERE id = 1",
        );
        assert!(s.get_task(1).is_err());

        s.execute_raw("UPDATE tasks SET item = '{}', last_completion_seq = -1 WHERE id = 1");
        assert!(s.get_task(1).is_err());

        s.execute_raw("UPDATE tasks SET last_completion_seq = 3 WHERE id = 1");
        assert_eq!(s.get_task(1).unwrap().unwrap().last_completion_seq, Some(3));

        let t2 = s.insert_task(new_task("run")).unwrap();
        assert!(s.get_task(t2.id).unwrap().is_some());
    }

    /// A v1 database predates `prompt_pending`; opening it adds the column
    /// and keeps the rows.
    #[test]
    fn a_v1_database_is_migrated() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.db");
        {
            let s = Store::open(&path).unwrap();
            s.insert_task(new_task("run")).unwrap();
        }
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "ALTER TABLE tasks DROP COLUMN prompt_pending;
                 ALTER TABLE tasks DROP COLUMN retry_of;
                 UPDATE meta SET value = '1' WHERE key = 'schema_version';",
            )
            .unwrap();
        }
        let s = Store::open(&path).unwrap();
        let mut t = s.get_task(1).unwrap().unwrap();
        assert!(!t.prompt_pending);
        t.prompt_pending = true;
        s.update_task(&mut t).unwrap();
        assert!(s.get_task(1).unwrap().unwrap().prompt_pending);
    }

    #[test]
    fn a_v11_database_gains_the_summaries_table() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.db");
        {
            let s = Store::open(&path).unwrap();
            s.insert_task(new_task("run")).unwrap();
            s.execute_raw(
                "DROP TABLE task_summaries;
                 UPDATE meta SET value = '11' WHERE key = 'schema_version'",
            );
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.meta("schema_version").unwrap().unwrap(), "13");
        assert_eq!(s.get_task(1).unwrap().unwrap().summary, None);
        assert_eq!(s.end_round(1, Some("done")).unwrap().round, 1);
    }

    /// Each round of a task gets its own row, numbered from 1; a done,
    /// failed or closed task is read with its last round's, a live one with
    /// none.
    #[test]
    fn rounds_are_numbered_and_a_finished_task_shows_its_last() {
        let s = Store::open_in_memory().unwrap();
        let mut t = s.insert_task(new_task("run")).unwrap();
        let first = s
            .end_round(t.id, Some("  Partial: tests left\nsee the branch\n"))
            .unwrap();
        assert_eq!(first.round, 1);
        assert_eq!(first.outcome, Outcome::Partial);
        assert_eq!(first.text, "Partial: tests left\nsee the branch");
        assert_eq!(first.source, SummarySource::Agent);
        assert_eq!(s.get_task(t.id).unwrap().unwrap().summary, None, "queued");
        t.state = TaskState::Done;
        s.update_task(&mut t).unwrap();
        assert_eq!(
            s.get_task(t.id).unwrap().unwrap().summary,
            Some(first.clone())
        );
        let second = s.end_round(t.id, Some("done")).unwrap();
        assert_eq!(second.round, 2);
        let listed = s.list_tasks(&TaskFilter::default()).unwrap();
        assert_eq!(listed[0].summary, Some(second.clone()));
        assert_eq!(s.summaries(t.id).unwrap(), vec![first, second]);
        t.state = TaskState::Running;
        s.update_task(&mut t).unwrap();
        assert_eq!(s.get_task(t.id).unwrap().unwrap().summary, None, "running");
    }

    /// A round that ends with no summary keeps the pane's last lines, and
    /// leaves them for the finish command.
    #[test]
    fn a_round_with_no_summary_keeps_the_pane_tail() {
        let s = Store::open_in_memory().unwrap();
        let t = s.insert_task(new_task("run")).unwrap();
        let none = s.end_round(t.id, None).unwrap();
        assert_eq!(none.outcome, Outcome::NoSummary);
        assert_eq!(none.source, SummarySource::Pane);
        assert_eq!(none.text, "");
        s.note_pane_tail(t.id, "built it\npushed\n");
        let blank = s.end_round(t.id, Some("  \n")).unwrap();
        assert_eq!(blank.round, 2);
        assert_eq!(blank.outcome, Outcome::NoSummary);
        assert_eq!(blank.text, "built it\npushed");
        assert_eq!(s.take_pane_tail(t.id).as_deref(), Some("built it\npushed"));
    }

    #[test]
    fn a_late_summary_replaces_the_last_round() {
        let s = Store::open_in_memory().unwrap();
        let t = s.insert_task(new_task("run")).unwrap();
        let first = s.replace_last_summary(t.id, "blocked: no token").unwrap();
        assert_eq!((first.round, first.outcome), (1, Outcome::Blocked));
        s.end_round(t.id, None).unwrap();
        let late = s.replace_last_summary(t.id, "nothing to do").unwrap();
        assert_eq!((late.round, late.outcome), (2, Outcome::NothingToDo));
        assert_eq!(late.source, SummarySource::Agent);
        assert_eq!(s.summaries(t.id).unwrap().len(), 2);
    }

    #[test]
    fn pruning_a_task_drops_its_summaries() {
        let s = Store::open_in_memory().unwrap();
        let mut old = s.insert_task(new_task("run")).unwrap();
        s.insert_task(new_task("run")).unwrap();
        old.state = TaskState::Done;
        old.finished_at = Some(Utc::now() - chrono::Duration::days(3));
        s.update_task(&mut old).unwrap();
        s.end_round(old.id, Some("done")).unwrap();
        let out = s
            .prune(&[TaskState::Done], Duration::from_secs(3600))
            .unwrap();
        assert_eq!(out.pruned, 1);
        assert!(s.summaries(old.id).unwrap().is_empty());
    }

    #[test]
    fn unreadable_schema_version_refuses_to_open() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.db");
        drop(Store::open(&path).unwrap());
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute(
                "UPDATE meta SET value = 'x' WHERE key = 'schema_version'",
                [],
            )
            .unwrap();
        }
        let err = Store::open(&path).err().expect("must not open");
        assert!(err.to_string().contains("not a number"), "error was: {err}");
        // Left alone for a human to look at, not relabelled as current.
        let conn = Connection::open(&path).unwrap();
        let v: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, "x");
    }

    #[test]
    fn a_negative_failure_count_is_reported_not_wrapped() {
        let s = Store::open_in_memory().unwrap();
        s.save_job_state(&JobState {
            name: "a".into(),
            ..Default::default()
        })
        .unwrap();
        s.execute_raw("UPDATE job_state SET failures = -1 WHERE name = 'a'");
        assert!(s.job_state("a").is_err(), "-1 must not read as 4294967295");
        assert!(s.job_states().is_err());
    }

    #[test]
    fn a_negative_failure_count_names_its_column_and_type() {
        let s = Store::open_in_memory().unwrap();
        s.save_job_state(&JobState {
            name: "a".into(),
            ..Default::default()
        })
        .unwrap();
        s.execute_raw("UPDATE job_state SET failures = -1 WHERE name = 'a'");
        let err = s.job_state("a").unwrap_err();
        match err.downcast_ref::<rusqlite::Error>() {
            Some(rusqlite::Error::FromSqlConversionFailure(idx, ty, _)) => {
                assert_eq!(*idx, 6, "failures is column 6 of job_state");
                assert_eq!(*ty, rusqlite::types::Type::Integer);
            }
            other => panic!("unexpected error {other:?}"),
        }
        assert!(err.to_string().contains("failures"), "{err}");
    }

    fn table_names(path: &Path) -> Vec<String> {
        let conn = Connection::open(path).unwrap();
        conn.prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    /// An early pastor wrote schema 2 (with prompt_pending) but had no seen
    /// or job_state tables.
    #[test]
    fn a_v2_database_without_the_job_tables_gains_them() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.db");
        {
            let s = Store::open(&path).unwrap();
            s.insert_task(new_task("run")).unwrap();
            s.execute_raw("DROP TABLE seen; DROP TABLE job_state;");
        }
        assert_eq!(
            table_names(&path),
            vec![
                "event_seq",
                "meta",
                "task_summaries",
                "tasks",
                "trusted_repos"
            ]
        );
        let s = Store::open(&path).unwrap();
        assert_eq!(
            table_names(&path),
            vec![
                "event_seq",
                "job_state",
                "meta",
                "seen",
                "task_summaries",
                "tasks",
                "trusted_repos"
            ]
        );
        assert!(!s.is_seen("j", "k").unwrap());
        assert!(s.job_state("j").unwrap().is_none());
        assert_eq!(s.list_tasks(&TaskFilter::default()).unwrap().len(), 1);
    }

    #[test]
    fn a_meta_table_without_a_version_is_refused_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
                .unwrap();
        }
        let err = Store::open(&path).err().expect("must not open");
        assert!(
            err.to_string().contains("schema_version"),
            "error was: {err}"
        );
        assert_eq!(table_names(&path), vec!["meta"], "nothing created");
        let conn = Connection::open(&path).unwrap();
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM meta", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0, "not stamped with a version");

        // No meta table at all is a fresh database and is created.
        let fresh = tmp.path().join("fresh.db");
        drop(Store::open(&fresh).unwrap());
        assert_eq!(
            table_names(&fresh),
            vec![
                "event_seq",
                "job_state",
                "meta",
                "seen",
                "task_summaries",
                "tasks",
                "trusted_repos"
            ]
        );
    }

    #[test]
    fn a_newer_schema_is_refused_before_anything_is_created() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO meta (key, value) VALUES ('schema_version', '99');",
            )
            .unwrap();
        }
        let err = Store::open(&path).err().expect("must not open");
        assert!(err.to_string().contains("newer"), "error was: {err}");
        let conn = Connection::open(&path).unwrap();
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(tables, vec!["meta"], "a newer database is left untouched");
    }

    #[test]
    fn open_on_disk_twice_keeps_data() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.db");
        {
            Store::open(&path)
                .unwrap()
                .insert_task(new_task("run"))
                .unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.list_tasks(&TaskFilter::default()).unwrap().len(), 1);
    }

    #[test]
    fn update_task_refuses_a_stale_copy() {
        let s = Store::open_in_memory().unwrap();
        let t = s.insert_task(new_task("run")).unwrap();
        let mut a = s.get_task(t.id).unwrap().unwrap();
        let mut b = s.get_task(t.id).unwrap().unwrap();
        a.state = TaskState::Running;
        s.update_task(&mut a).unwrap();
        assert!(
            a.updated_at > b.updated_at,
            "a successful update must advance the in-memory updated_at"
        );
        b.state = TaskState::Closed;
        let err = s.update_task(&mut b).unwrap_err();
        assert!(err.downcast_ref::<Conflict>().is_some(), "{err}");
        assert_eq!(
            s.get_task(t.id).unwrap().unwrap().state,
            TaskState::Running,
            "the stale write must not land"
        );
        // The fresh copy keeps working, and a second write on it too.
        a.state = TaskState::Done;
        s.update_task(&mut a).unwrap();
        a.state = TaskState::Closed;
        s.update_task(&mut a).unwrap();
        assert_eq!(s.get_task(t.id).unwrap().unwrap().state, TaskState::Closed);
    }

    #[test]
    fn update_task_still_reports_a_missing_row() {
        let s = Store::open_in_memory().unwrap();
        let mut t = s.insert_task(new_task("run")).unwrap();
        t.id = 99;
        let err = s.update_task(&mut t).unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
        assert!(err.downcast_ref::<Conflict>().is_none());
    }

    #[test]
    fn claim_task_is_exclusive() {
        let s = Store::open_in_memory().unwrap();
        let t = s.insert_task(new_task("run")).unwrap();
        let claimed = s
            .claim_task(t.id, "pi-3")
            .unwrap()
            .expect("first claim wins");
        assert_eq!(claimed.state, TaskState::Starting);
        assert_eq!(claimed.machine.as_deref(), Some("pi-3"));
        assert_eq!(claimed.agent_name.as_deref(), Some("t-1"));
        assert!(
            s.claim_task(t.id, "pi-1").unwrap().is_none(),
            "a second claim must find nothing to claim"
        );
        assert!(s.claim_task(99, "pi-1").unwrap().is_none());
        // A claimed copy is fresh: updating it must not conflict.
        let mut c = claimed;
        c.state = TaskState::Running;
        s.update_task(&mut c).unwrap();
    }

    #[test]
    fn job_state_round_trips_and_lists() {
        let s = Store::open_in_memory().unwrap();
        assert!(s.job_state("a").unwrap().is_none());
        let now = Utc::now();
        let st = JobState {
            name: "a".into(),
            last_run_at: Some(now),
            last_ok_at: Some(now),
            last_result: Some("ok: 1 items, 1 tasks".into()),
            last_error: None,
            cursor: Some("c1".into()),
            failures: 0,
            backoff_until: None,
        };
        s.save_job_state(&st).unwrap();
        let got = s.job_state("a").unwrap().unwrap();
        assert_eq!(got.cursor.as_deref(), Some("c1"));
        assert_eq!(
            got.last_run_at.unwrap().timestamp_millis(),
            now.timestamp_millis()
        );
        let failed = JobState {
            failures: 2,
            backoff_until: Some(now),
            last_error: Some("boom".into()),
            ..st.clone()
        };
        s.save_job_state(&failed).unwrap();
        assert_eq!(
            s.job_state("a").unwrap().unwrap().failures,
            2,
            "replace, not duplicate"
        );
        s.save_job_state(&JobState {
            name: "b".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            s.job_states()
                .unwrap()
                .iter()
                .map(|j| j.name.clone())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn insert_job_task_renders_with_its_id_and_marks_seen() {
        let s = Store::open_in_memory().unwrap();
        let item = serde_json::json!({"key": "k1", "title": "t"});
        assert!(!s.is_seen("j", "k1").unwrap());
        let t = s
            .insert_job_task("j", "default", &item, None, |id| {
                Ok((
                    format!("prompt for t-{id}"),
                    DispatchSpec {
                        branch: Some(format!("pastor/t-{id}")),
                        ..spec()
                    },
                ))
            })
            .unwrap();
        assert_eq!(t.job, "j");
        assert_eq!(t.state, TaskState::Queued);
        assert_eq!(t.prompt, format!("prompt for t-{}", t.id));
        assert_eq!(
            t.spec.branch.as_deref(),
            Some(format!("pastor/t-{}", t.id).as_str())
        );
        assert_eq!(t.item["key"], "k1");
        assert!(s.is_seen("j", "k1").unwrap());
        assert!(!s.is_seen("other", "k1").unwrap(), "seen is per job");

        // The same key again: refused, nothing written.
        let before = s.list_tasks(&TaskFilter::default()).unwrap().len();
        assert!(
            s.insert_job_task("j", "default", &item, None, |_| Ok(("x".into(), spec())))
                .is_err()
        );
        assert_eq!(s.list_tasks(&TaskFilter::default()).unwrap().len(), before);

        // A render failure rolls the whole thing back: no task, key still unseen.
        let item2 = serde_json::json!({"key": "k2"});
        let err = s
            .insert_job_task("j", "default", &item2, None, |_| Err("nope".into()))
            .unwrap_err();
        assert!(err.to_string().contains("nope"), "{err}");
        assert_eq!(s.list_tasks(&TaskFilter::default()).unwrap().len(), before);
        assert!(!s.is_seen("j", "k2").unwrap());

        // No string key: refused up front.
        assert!(
            s.insert_job_task(
                "j",
                "default",
                &serde_json::json!({"title": "no key"}),
                None,
                |_| Ok(("x".into(), spec()))
            )
            .is_err()
        );
    }

    #[test]
    fn schema_v1_databases_are_migrated_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.db");
        {
            let s = Store::open(&path).unwrap();
            s.insert_task(new_task("run")).unwrap();
            // A genuine v1 file has neither the job tables nor the
            // prompt_pending column; v2 gained both, v3 retry_of.
            s.execute_raw(
                "DROP TABLE seen; DROP TABLE job_state;
                 ALTER TABLE tasks DROP COLUMN prompt_pending;
                 ALTER TABLE tasks DROP COLUMN retry_of;
                 ALTER TABLE tasks DROP COLUMN flock;
                 UPDATE meta SET value = '1' WHERE key = 'schema_version'",
            );
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(
            s.list_tasks(&TaskFilter::default()).unwrap().len(),
            1,
            "data survives"
        );
        assert!(!s.is_seen("j", "k").unwrap(), "the new tables exist");
        assert!(
            !s.get_task(1).unwrap().unwrap().prompt_pending,
            "the new column exists with its default"
        );
        let v: String = s.meta("schema_version").unwrap().unwrap();
        assert_eq!(v, SCHEMA_VERSION.to_string());
    }

    /// A task row stores its flock; `task list --flock` narrows to it.
    #[test]
    fn a_task_keeps_its_flock_and_lists_filter_on_it() {
        let s = Store::open_in_memory().unwrap();
        let home = s.insert_task(new_task("run")).unwrap();
        let work = s
            .insert_task(NewTask {
                flock: "work".into(),
                ..new_task("run")
            })
            .unwrap();
        assert_eq!(home.flock.as_deref(), Some("default"));
        assert_eq!(work.flock.as_deref(), Some("work"));
        let ids = |flock: &str| -> Vec<i64> {
            s.list_tasks(&TaskFilter {
                flock: Some(flock.into()),
                ..Default::default()
            })
            .unwrap()
            .iter()
            .map(|t| t.id)
            .collect()
        };
        assert_eq!(ids("work"), [work.id]);
        assert_eq!(ids("default"), [home.id]);
        assert!(ids("nope").is_empty());
        let job = s
            .insert_job_task("j", "work", &serde_json::json!({"key": "k"}), None, |_| {
                Ok(("p".into(), spec()))
            })
            .unwrap();
        assert_eq!(job.flock.as_deref(), Some("work"));
    }

    /// A v3 database predates flocks: opening it adds the column empty, and
    /// `adopt_default_flock` puts those rows in the default flock, once.
    #[test]
    fn a_v3_database_gains_flock_and_its_rows_join_the_default() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.db");
        {
            let s = Store::open(&path).unwrap();
            s.insert_task(new_task("run")).unwrap();
            s.insert_task(new_task("run")).unwrap();
            s.execute_raw(
                "ALTER TABLE tasks DROP COLUMN flock;
                 UPDATE meta SET value = '3' WHERE key = 'schema_version'",
            );
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(
            s.meta("schema_version").unwrap().unwrap(),
            SCHEMA_VERSION.to_string()
        );
        assert_eq!(s.get_task(1).unwrap().unwrap().flock, None);
        let fresh = s
            .insert_task(NewTask {
                flock: "work".into(),
                ..new_task("run")
            })
            .unwrap();
        assert_eq!(s.adopt_default_flock("personal").unwrap(), 2);
        assert_eq!(
            s.get_task(1).unwrap().unwrap().flock.as_deref(),
            Some("personal")
        );
        assert_eq!(
            s.get_task(fresh.id).unwrap().unwrap().flock.as_deref(),
            Some("work"),
            "a row that has a flock keeps it"
        );
        assert_eq!(s.adopt_default_flock("other").unwrap(), 0);
    }

    #[test]
    fn a_retry_stays_in_its_flock() {
        let s = Store::open_in_memory().unwrap();
        let t = s
            .insert_task(NewTask {
                flock: "work".into(),
                ..new_task("run")
            })
            .unwrap();
        set_state(&s, t.id, TaskState::Failed);
        let r = s.insert_retry(t.id).unwrap();
        assert_eq!(r.flock.as_deref(), Some("work"));
    }

    /// A task keeps the description it was queued with; one queued without
    /// reads as its prompt's first line. A retry copies it.
    #[test]
    fn a_task_keeps_its_description_and_a_retry_copies_it() {
        let s = Store::open_in_memory().unwrap();
        let given = s
            .insert_task(NewTask {
                description: Some("Fix the flaky test".into()),
                ..new_task("run")
            })
            .unwrap();
        assert_eq!(given.description.as_deref(), Some("Fix the flaky test"));
        assert_eq!(given.description_text(), "Fix the flaky test");
        assert_eq!(given.description_from(), "--description");
        let bare = s.insert_task(new_task("run")).unwrap();
        assert_eq!(bare.description, None);
        assert_eq!(bare.description_text(), "do it");
        assert_eq!(bare.description_from(), "the prompt");
        let json = bare.to_json();
        assert_eq!(json["description"], "do it");
        assert_eq!(json["description_from"], "the prompt");

        let job = s
            .insert_job_task(
                "j",
                "default",
                &serde_json::json!({"key": "k"}),
                Some("Card title"),
                |_| Ok(("p".into(), spec())),
            )
            .unwrap();
        assert_eq!(job.description.as_deref(), Some("Card title"));
        assert_eq!(job.description_from(), "job j");

        set_state(&s, given.id, TaskState::Failed);
        let r = s.insert_retry(given.id).unwrap();
        assert_eq!(r.description.as_deref(), Some("Fix the flaky test"));
    }

    /// A v11 database has no pause columns: opening it adds them, and its
    /// rows neither preempt nor were paused.
    #[test]
    fn a_v11_database_gains_the_pause_columns() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.db");
        {
            let s = Store::open(&path).unwrap();
            s.insert_task(new_task("run")).unwrap();
            s.execute_raw(
                "ALTER TABLE tasks DROP COLUMN preempt;
                 ALTER TABLE tasks DROP COLUMN paused_at;
                 ALTER TABLE tasks DROP COLUMN paused_for;
                 ALTER TABLE tasks DROP COLUMN resumed_at;
                 UPDATE meta SET value = '11' WHERE key = 'schema_version'",
            );
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(
            s.meta("schema_version").unwrap().unwrap(),
            SCHEMA_VERSION.to_string()
        );
        let old = s.get_task(1).unwrap().unwrap();
        assert_eq!(old.pause, crate::task::Preemption::default());
        let json = serde_json::to_value(&old).unwrap();
        assert!(json.get("preempt").is_none() && json.get("paused_at").is_none());
        let new = s
            .insert_task_preempting(
                new_task("run"),
                Priority::Critical,
                None,
                TaskRole::Agent,
                true,
            )
            .unwrap();
        assert!(new.pause.preempt);
        assert_eq!(serde_json::to_value(&new).unwrap()["preempt"], true);
    }

    /// A paused task goes first among the `low` tasks, after every higher
    /// one, and is claimed back only on the machine it was paused on.
    #[test]
    fn a_paused_task_is_first_among_low_tasks_and_claimed_on_its_machine() {
        let s = Store::open_in_memory().unwrap();
        let at = |p| {
            s.insert_task_at(new_task("run"), p, None, TaskRole::Agent)
                .unwrap()
        };
        let low = at(Priority::Low);
        let high = at(Priority::High);
        let paused = at(Priority::Low);
        let mut t = s.claim_task(paused.id, "a").unwrap().unwrap();
        t.state = TaskState::Paused;
        t.pause.paused_at = Some(Utc::now());
        t.pause.paused_for = Some(high.id);
        s.update_task(&mut t).unwrap();
        let order: Vec<i64> = s.queued_tasks().unwrap().iter().map(|t| t.id).collect();
        assert_eq!(order, vec![high.id, paused.id, low.id]);
        assert!(s.claim_paused(paused.id, "b").unwrap().is_none());
        assert!(
            s.claim_task(paused.id, "a").unwrap().is_none(),
            "not queued"
        );
        let back = s.claim_paused(paused.id, "a").unwrap().unwrap();
        assert_eq!(back.state, TaskState::Starting);
        assert!(back.pause.resumed_at.is_some());
        assert_eq!(back.pause.paused_for, Some(high.id));
        assert!(
            s.claim_paused(paused.id, "a").unwrap().is_none(),
            "claimed once"
        );
    }

    /// A retry keeps `preempt` with the level it copies.
    #[test]
    fn a_retry_keeps_preempt() {
        let s = Store::open_in_memory().unwrap();
        let t = s
            .insert_task_preempting(
                new_task("run"),
                Priority::Critical,
                None,
                TaskRole::Agent,
                true,
            )
            .unwrap();
        set_state(&s, t.id, TaskState::Failed);
        assert!(s.insert_retry(t.id).unwrap().pause.preempt);
    }

    /// A v8 database has no description column: opening it adds one, and
    /// its rows read as their prompt's first line.
    #[test]
    fn a_v8_database_gains_the_description_column() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.db");
        {
            let s = Store::open(&path).unwrap();
            s.insert_task(new_task("run")).unwrap();
            s.execute_raw(
                "ALTER TABLE tasks DROP COLUMN description;
                 UPDATE meta SET value = '8' WHERE key = 'schema_version'",
            );
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(
            s.meta("schema_version").unwrap().unwrap(),
            SCHEMA_VERSION.to_string()
        );
        let old = s.get_task(1).unwrap().unwrap();
        assert_eq!(old.description, None);
        assert_eq!(old.description_text(), "do it");
        let new = s
            .insert_task(NewTask {
                description: Some("d".into()),
                ..new_task("run")
            })
            .unwrap();
        assert_eq!(new.description.as_deref(), Some("d"));
    }

    /// `task retry --place` replaces where the copy's pane goes and keeps
    /// everything else; without it the copy keeps the original's place.
    #[test]
    fn a_retry_can_change_its_place() {
        let s = Store::open_in_memory().unwrap();
        let t = s
            .insert_task(NewTask {
                spec: DispatchSpec {
                    place: crate::task::Place::Pastor,
                    ..spec()
                },
                ..new_task("run")
            })
            .unwrap();
        set_state(&s, t.id, TaskState::Failed);
        let same = s.insert_retry(t.id).unwrap();
        assert_eq!(same.spec.place, crate::task::Place::Pastor);
        let moved = s
            .insert_retry_placed(t.id, Some(&crate::task::Place::Pane("work".into())))
            .unwrap();
        assert_eq!(moved.spec.place, crate::task::Place::Pane("work".into()));
        assert_eq!(moved.spec.repo, t.spec.repo);
        let home = s
            .insert_retry_placed(t.id, Some(&crate::task::Place::Repo))
            .unwrap();
        assert_eq!(home.spec.place, crate::task::Place::Repo);
    }

    /// `task retry --place` records itself as the override's source, not
    /// whatever placed the original run, so `task describe` does not lie
    /// about where the new place came from.
    #[test]
    fn a_retry_that_changes_its_place_updates_its_source() {
        let s = Store::open_in_memory().unwrap();
        let t = s
            .insert_task(NewTask {
                spec: DispatchSpec {
                    place: crate::task::Place::Pastor,
                    agent_source: Some(Box::new(crate::task::AgentSource {
                        ask: Default::default(),
                        agent: "claude".into(),
                        agent_args: None,
                        model: None,
                        model_from: None,
                        fallback: vec![],
                        fallback_from: None,
                        profile: None,
                        profile_from: None,
                        timeout_from: None,
                        place_from: Some("flock default".into()),
                    })),
                    ..spec()
                },
                ..new_task("run")
            })
            .unwrap();
        set_state(&s, t.id, TaskState::Failed);
        let moved = s
            .insert_retry_placed(t.id, Some(&crate::task::Place::Pane("work".into())))
            .unwrap();
        assert_eq!(
            moved.spec.agent_source.unwrap().place_from.as_deref(),
            Some("task retry")
        );
        let same = s.insert_retry(t.id).unwrap();
        assert_eq!(
            same.spec.agent_source.unwrap().place_from.as_deref(),
            Some("flock default")
        );
    }

    /// A retry keeps the label template and where it came from, and drops
    /// the workspace the failed task was in: its own dispatch names one.
    #[test]
    fn a_retry_keeps_its_label_template_only() {
        let s = Store::open_in_memory().unwrap();
        let label = crate::task::WorkspaceLabel {
            template: Some("{{ machine }}".into()),
            from: Some("flock work".into()),
            name: Some("pi-1".into()),
            note: Some("joined workspace".into()),
        };
        let t = s
            .insert_task(NewTask {
                spec: DispatchSpec { label, ..spec() },
                ..new_task("run")
            })
            .unwrap();
        set_state(&s, t.id, TaskState::Failed);
        let r = s.insert_retry(t.id).unwrap();
        assert_eq!(
            r.spec.label,
            crate::task::WorkspaceLabel {
                template: Some("{{ machine }}".into()),
                from: Some("flock work".into()),
                ..Default::default()
            }
        );
    }

    /// A retry of a failed worktree task that owns a checkout (dispatch
    /// recorded it on the task) carries that checkout and the agent that
    /// owned it, for dispatch to reopen. Any other worktree retry, a stale
    /// task's or one of a task that failed before its worktree was made,
    /// drops the branch, even one the job named, and gets its own. A task
    /// without a worktree has nothing to reopen.
    #[test]
    fn a_retry_carries_only_a_failed_task_s_own_checkout() {
        let s = Store::open_in_memory().unwrap();
        let checkout = Checkout {
            branch: "fix/x".into(),
            path: "/wt/fix-x".into(),
            already_open: false,
        };
        let task = |worktree: bool, owned: bool, state| {
            let mut n = new_task("run");
            n.spec.repo = Some("/r".into());
            n.spec.worktree = worktree;
            n.spec.branch = Some("fix/x".into());
            let t = s.insert_task(n).unwrap();
            let mut t = set_state(&s, t.id, state);
            if owned {
                t.agent_name = Some(Task::agent_name_for(t.id));
                t.spec.checkout = Some(Box::new(checkout.clone()));
                t.spec.session_id = Some("0d5bd3a4-2f35-4e1c-9f59-7c1c3a7b8e21".into());
                s.update_task(&mut t).unwrap();
            }
            let r = s.insert_retry(t.id).unwrap();
            assert_eq!(r.spec.checkout, None, "a retry owns no checkout yet");
            assert_eq!(r.spec.session_id, None, "nor a session");
            (t.id, r.spec.branch, r.spec.reopen)
        };
        let (id, branch, reopen) = task(true, true, TaskState::Failed);
        assert_eq!(branch, None);
        assert_eq!(
            reopen,
            Some(Box::new(Reopen {
                branch: "fix/x".into(),
                path: "/wt/fix-x".into(),
                agent: format!("t-{id}"),
            }))
        );
        let dropped = |(_, branch, reopen)| (branch, reopen);
        assert_eq!(dropped(task(true, false, TaskState::Failed)), (None, None));
        assert_eq!(dropped(task(true, true, TaskState::Stale)), (None, None));
        assert_eq!(
            dropped(task(false, false, TaskState::Failed)),
            (Some("fix/x".into()), None)
        );

        // A failed retry that never made its own checkout passes on nothing,
        // not the checkout it was handed.
        let (id, ..) = task(true, true, TaskState::Failed);
        let r = s.get_task(id + 1).unwrap().unwrap();
        assert!(r.spec.reopen.is_some());
        set_state(&s, r.id, TaskState::Failed);
        let again = s.insert_retry(r.id).unwrap();
        assert_eq!((again.spec.branch, again.spec.reopen), (None, None));
    }

    #[test]
    fn trusted_repos_are_saved_listed_and_removed() {
        let s = Store::open_in_memory().unwrap();
        assert!(!s.is_trusted("m", "~/src/app").unwrap());
        assert!(s.trust_repo("m", "~/src/app").unwrap(), "newly trusted");
        assert!(!s.trust_repo("m", "~/src/app").unwrap(), "already trusted");
        s.trust_repo("a", "/srv/x").unwrap();
        assert!(s.is_trusted("m", "~/src/app").unwrap());
        // Keyed on both: the same repo on another machine is another folder.
        assert!(!s.is_trusted("a", "~/src/app").unwrap());
        let list: Vec<(String, String)> = s
            .trusted_repos()
            .unwrap()
            .into_iter()
            .map(|t| (t.machine, t.repo))
            .collect();
        assert_eq!(
            list,
            [
                ("a".to_string(), "/srv/x".to_string()),
                ("m".to_string(), "~/src/app".to_string())
            ]
        );
        assert!(s.untrust("m", "~/src/app").unwrap());
        assert!(!s.untrust("m", "~/src/app").unwrap());
        assert!(!s.is_trusted("m", "~/src/app").unwrap());
    }

    #[test]
    fn trust_is_sent_to_a_task_once() {
        let s = Store::open_in_memory().unwrap();
        let t = s.insert_task(new_task("run")).unwrap();
        assert!(!s.trust_sent(t.id).unwrap());
        assert!(s.claim_trust_sent(t.id).unwrap());
        assert!(s.trust_sent(t.id).unwrap());
        assert!(!s.claim_trust_sent(t.id).unwrap());
        // A write of the row from a copy read before does not reset it.
        let mut copy = s.get_task(t.id).unwrap().unwrap();
        copy.error = Some("x".into());
        s.update_task(&mut copy).unwrap();
        assert!(!s.claim_trust_sent(t.id).unwrap());
    }

    /// A v4 database predates saved trust; opening it adds the table and
    /// the column.
    #[test]
    fn a_v4_database_gains_saved_trust() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.db");
        {
            let s = Store::open(&path).unwrap();
            s.insert_task(new_task("run")).unwrap();
            s.execute_raw(
                "DROP TABLE trusted_repos;
                 ALTER TABLE tasks DROP COLUMN trust_sent;
                 UPDATE meta SET value = '4' WHERE key = 'schema_version'",
            );
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(
            s.meta("schema_version").unwrap().unwrap(),
            SCHEMA_VERSION.to_string()
        );
        assert!(s.trust_repo("m", "/r").unwrap());
        assert!(s.claim_trust_sent(1).unwrap());
    }

    /// A v5 database keeps no `activity_seen`; opening it adds the column,
    /// unset, and a row then stores it.
    #[test]
    fn a_v5_database_gains_activity_seen() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.db");
        {
            let s = Store::open(&path).unwrap();
            s.insert_task(new_task("run")).unwrap();
            s.execute_raw(
                "ALTER TABLE tasks DROP COLUMN activity_seen;
                 UPDATE meta SET value = '5' WHERE key = 'schema_version'",
            );
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(
            s.meta("schema_version").unwrap().unwrap(),
            SCHEMA_VERSION.to_string()
        );
        let mut t = s.get_task(1).unwrap().unwrap();
        assert!(!t.activity_seen);
        t.activity_seen = true;
        s.update_task(&mut t).unwrap();
        assert!(s.get_task(1).unwrap().unwrap().activity_seen);
    }

    /// A v6 database keeps no `ended`; opening it adds the column, unset,
    /// and a row then stores it.
    #[test]
    fn a_v6_database_gains_ended() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.db");
        {
            let s = Store::open(&path).unwrap();
            s.insert_task(new_task("run")).unwrap();
            s.execute_raw(
                "ALTER TABLE tasks DROP COLUMN ended;
                 UPDATE meta SET value = '6' WHERE key = 'schema_version'",
            );
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(
            s.meta("schema_version").unwrap().unwrap(),
            SCHEMA_VERSION.to_string()
        );
        let mut t = s.get_task(1).unwrap().unwrap();
        assert!(!t.ended);
        t.ended = true;
        s.update_task(&mut t).unwrap();
        assert!(s.get_task(1).unwrap().unwrap().ended);
    }

    /// A v8 database has no roles; opening it adds the column and every
    /// row reads as a plain agent.
    #[test]
    fn a_v8_database_gains_role_as_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.db");
        {
            let s = Store::open(&path).unwrap();
            s.insert_task(new_task("run")).unwrap();
            s.execute_raw(
                "ALTER TABLE tasks DROP COLUMN role;
                 UPDATE meta SET value = '8' WHERE key = 'schema_version'",
            );
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(
            s.meta("schema_version").unwrap().unwrap(),
            SCHEMA_VERSION.to_string()
        );
        assert_eq!(s.get_task(1).unwrap().unwrap().role, TaskRole::Agent);
    }

    /// A task keeps the role it was queued with, and its retry, the same
    /// task again, keeps it too.
    #[test]
    fn a_task_keeps_its_role_and_a_retry_copies_it() {
        let s = Store::open_in_memory().unwrap();
        let plain = s.insert_task(new_task("run")).unwrap();
        assert_eq!(plain.role, TaskRole::Agent);
        let o = s
            .insert_task_at(
                new_task("run"),
                Priority::Normal,
                None,
                TaskRole::Orchestrator,
            )
            .unwrap();
        assert_eq!(o.role, TaskRole::Orchestrator);
        let mut o = s.get_task(o.id).unwrap().unwrap();
        assert_eq!(o.role, TaskRole::Orchestrator);
        o.state = TaskState::Failed;
        s.update_task(&mut o).unwrap();
        let r = s.insert_retry(o.id).unwrap();
        assert_eq!(r.role, TaskRole::Orchestrator);
    }

    /// A v8 database has no levels or positions: its rows become `normal`,
    /// placed by id, so the queue keeps the order it had.
    #[test]
    fn a_v8_database_gains_priority_and_queue_pos() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.db");
        {
            let s = Store::open(&path).unwrap();
            s.insert_task(new_task("run")).unwrap();
            s.insert_task(new_task("run")).unwrap();
            s.execute_raw(
                "ALTER TABLE tasks DROP COLUMN priority;
                 ALTER TABLE tasks DROP COLUMN priority_from;
                 ALTER TABLE tasks DROP COLUMN queue_pos;
                 UPDATE meta SET value = '8' WHERE key = 'schema_version'",
            );
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(
            s.meta("schema_version").unwrap().unwrap(),
            SCHEMA_VERSION.to_string()
        );
        let q = s.queued_tasks().unwrap();
        assert_eq!(q.iter().map(|t| t.id).collect::<Vec<_>>(), vec![1, 2]);
        for t in &q {
            assert_eq!(t.priority, Priority::Normal);
            assert_eq!(t.priority_from, None);
            assert_eq!(t.queue_pos, t.id);
        }
        let conn = Connection::open(&path).unwrap();
        let unset: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE queue_pos IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unset, 0);
    }

    /// Dispatch takes queued tasks by level, highest first, then by
    /// position, then oldest first; a task that left the queue is not in it.
    #[test]
    fn queued_tasks_go_by_level_then_position_then_age() {
        let s = Store::open_in_memory().unwrap();
        let at = |p: Priority| {
            s.insert_task_at(new_task("run"), p, None, TaskRole::Agent)
                .unwrap()
                .id
        };
        let low = at(Priority::Low);
        let normal = at(Priority::Normal);
        let high = at(Priority::High);
        let critical = at(Priority::Critical);
        let high2 = at(Priority::High);
        let normal2 = at(Priority::Normal);
        let taken = at(Priority::Critical);
        s.claim_task(taken, "m").unwrap().unwrap();
        let order = |s: &Store| {
            s.queued_tasks()
                .unwrap()
                .iter()
                .map(|t| t.id)
                .collect::<Vec<_>>()
        };
        assert_eq!(order(&s), vec![critical, high, high2, normal, normal2, low]);
        // Position before age: a task moved ahead of its level goes first.
        s.execute_raw(&format!(
            "UPDATE tasks SET queue_pos = 0 WHERE id = {high2}"
        ));
        assert_eq!(order(&s), vec![critical, high2, high, normal, normal2, low]);
        // Age breaks a tie in position.
        s.execute_raw(&format!(
            "UPDATE tasks SET queue_pos = 5, created_at = '2000-01-01T00:00:00+00:00' WHERE id = {normal2};
             UPDATE tasks SET queue_pos = 5 WHERE id = {normal}"
        ));
        assert_eq!(order(&s), vec![critical, high2, high, normal2, normal, low]);
    }

    /// A job's task and a retry keep the level they are given, and each new
    /// task is placed by its id.
    #[test]
    fn job_tasks_and_retries_keep_their_level() {
        let s = Store::open_in_memory().unwrap();
        let t = s
            .insert_job_task_at(
                "j",
                "default",
                &serde_json::json!({"key": "k"}),
                (Priority::High, Some("job j")),
                false,
                None,
                |_| Ok(("p".into(), spec())),
            )
            .unwrap();
        assert_eq!(t.priority, Priority::High);
        assert_eq!(t.priority_from.as_deref(), Some("job j"));
        assert_eq!(t.queue_pos, t.id);
        set_state(&s, t.id, TaskState::Failed);
        let r = s.insert_retry(t.id).unwrap();
        assert_eq!(r.priority, Priority::High);
        assert_eq!(r.priority_from.as_deref(), Some("job j"));
        assert_eq!(r.queue_pos, r.id);
    }

    /// The queue as (id, level) pairs, in dispatch order.
    fn queue_of(s: &Store) -> Vec<(i64, Priority)> {
        s.queued_tasks()
            .unwrap()
            .iter()
            .map(|t| (t.id, t.priority))
            .collect()
    }

    /// `--before`, `--after` and `--to` put a task where they say within
    /// its level, leaving its level alone, and the level's positions are
    /// shared out again so a new task still queues last.
    #[test]
    fn move_queued_places_a_task_within_its_level() {
        use crate::queue::QueueSpot::*;
        use Priority::Normal as N;
        let s = Store::open_in_memory().unwrap();
        let ids: Vec<i64> = (0..4)
            .map(|_| s.insert_task(new_task("run")).unwrap().id)
            .collect();
        let [a, b, c, d] = ids[..] else { panic!() };
        let m = s.move_queued(d, Before(b), &[]).unwrap();
        assert_eq!((m.pos, m.of, m.was), (2, 4, N));
        assert_eq!(m.task.priority, N);
        assert_eq!(m.task.priority_from, None, "the level did not change");
        assert_eq!(queue_of(&s), [(a, N), (d, N), (b, N), (c, N)]);
        let m = s.move_queued(a, After(c), &[]).unwrap();
        assert_eq!(m.pos, 4);
        assert_eq!(queue_of(&s), [(d, N), (b, N), (c, N), (a, N)]);
        s.move_queued(c, To(1), &[]).unwrap();
        assert_eq!(queue_of(&s), [(c, N), (d, N), (b, N), (a, N)]);
        let m = s.move_queued(c, To(99), &[]).unwrap();
        assert_eq!(m.pos, 4, "past the end is last");
        assert_eq!(queue_of(&s), [(d, N), (b, N), (a, N), (c, N)]);
        let m = s.move_queued(b, Before(b), &[]).unwrap();
        assert_eq!(m.pos, 2, "before itself stays put");
        assert_eq!(queue_of(&s), [(d, N), (b, N), (a, N), (c, N)]);
        let e = s.insert_task(new_task("run")).unwrap().id;
        assert_eq!(queue_of(&s).last(), Some(&(e, N)));
        let pos: Vec<i64> = s
            .queued_tasks()
            .unwrap()
            .iter()
            .map(|t| t.queue_pos)
            .collect();
        assert!(pos.windows(2).all(|w| w[0] < w[1]), "{pos:?}");
    }

    /// A task moved in front of a higher one is lifted to its level, and
    /// one moved behind a lower one lowered to it; the move is what set
    /// the level. `--top` on a high task in front of a critical one lifts it,
    /// and on the first task changes nothing.
    #[test]
    fn move_queued_lifts_and_lowers() {
        use crate::queue::QueueSpot::*;
        use crate::task::TaskRole;
        use Priority::*;
        let s = Store::open_in_memory().unwrap();
        let at = |p: Priority| {
            s.insert_task_at(new_task("run"), p, Some("task run"), TaskRole::Agent)
                .unwrap()
                .id
        };
        let crit = at(Critical);
        let high = at(High);
        let normal = at(Normal);
        let low = at(Low);
        let m = s.move_queued(normal, Before(high), &[]).unwrap();
        assert_eq!((m.was, m.task.priority, m.pos), (Normal, High, 2));
        assert_eq!(m.task.priority_from.as_deref(), Some("queue move"));
        assert_eq!(
            queue_of(&s),
            [(crit, Critical), (normal, High), (high, High), (low, Low)]
        );
        let m = s.move_queued(high, After(low), &[]).unwrap();
        assert_eq!((m.was, m.task.priority, m.pos), (High, Low, 4));
        assert_eq!(
            queue_of(&s),
            [(crit, Critical), (normal, High), (low, Low), (high, Low)]
        );
        let m = s.move_queued(low, Top, &[]).unwrap();
        assert_eq!((m.was, m.task.priority, m.pos), (Low, Critical, 1));
        assert_eq!(
            queue_of(&s),
            [
                (low, Critical),
                (crit, Critical),
                (normal, High),
                (high, Low)
            ]
        );
        let m = s.move_queued(low, Top, &[]).unwrap();
        assert_eq!((m.was, m.task.priority, m.pos), (Critical, Critical, 1));
        // Behind a lower task at the very end lowers too, but between two
        // of its own level it stays.
        let m = s.move_queued(crit, To(4), &[]).unwrap();
        assert_eq!((m.was, m.task.priority), (Critical, Low));
        assert_eq!(
            queue_of(&s),
            [(low, Critical), (normal, High), (high, Low), (crit, Low)]
        );
    }

    /// The queue is one order across flocks: a task can go before a task
    /// of another flock, and keeps its own flock.
    #[test]
    fn move_queued_crosses_flocks() {
        use crate::queue::QueueSpot::*;
        let s = Store::open_in_memory().unwrap();
        let home = s.insert_task(new_task("run")).unwrap().id;
        let work = s
            .insert_task(NewTask {
                flock: "work".into(),
                ..new_task("run")
            })
            .unwrap()
            .id;
        let m = s.move_queued(work, Before(home), &[]).unwrap();
        assert_eq!(m.pos, 1);
        assert_eq!(m.task.flock.as_deref(), Some("work"));
        assert_eq!(
            queue_of(&s),
            [(work, Priority::Normal), (home, Priority::Normal)]
        );
    }

    /// Only a queued task moves, and only before or after a queued one;
    /// a refusal names the task at fault and changes nothing.
    #[test]
    fn move_queued_refuses_what_is_not_queued() {
        use crate::queue::QueueSpot::*;
        let s = Store::open_in_memory().unwrap();
        let taken = s.insert_task(new_task("run")).unwrap().id;
        let q = s.insert_task(new_task("run")).unwrap().id;
        s.claim_task(taken, "m").unwrap().unwrap();
        match s.move_queued(taken, Top, &[]) {
            Err(MoveError::NotQueued { id, state }) => {
                assert_eq!((id, state), (taken, TaskState::Starting))
            }
            other => panic!("{other:?}"),
        }
        match s.move_queued(q, Before(taken), &[]) {
            Err(MoveError::NotQueued { id, .. }) => assert_eq!(id, taken),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            s.move_queued(99, Top, &[]),
            Err(MoveError::NotFound(99))
        ));
        assert!(matches!(
            s.move_queued(q, After(98), &[]),
            Err(MoveError::NotFound(98))
        ));
    }

    /// `exclude` (a dispatch pass's in-flight ids) drops those rows from
    /// the queue a move works out positions and levels from, even though
    /// their row still says `queued`: naming one `--before` or `--after`
    /// is refused as `InFlight`, as if it had already left the queue, and
    /// `--to` does not count it toward a position.
    #[test]
    fn move_queued_excludes_in_flight_tasks() {
        use crate::queue::QueueSpot::*;
        let s = Store::open_in_memory().unwrap();
        let a = s.insert_task(new_task("run")).unwrap().id;
        let flighty = s.insert_task(new_task("run")).unwrap().id;
        let c = s.insert_task(new_task("run")).unwrap().id;
        assert!(matches!(
            s.move_queued(a, Before(flighty), &[flighty]),
            Err(MoveError::InFlight(id)) if id == flighty
        ));
        assert!(matches!(
            s.move_queued(a, After(flighty), &[flighty]),
            Err(MoveError::InFlight(id)) if id == flighty
        ));
        // With `flighty` excluded, the queue `--to` counts over is just
        // [a, c]: position 2 is last, not `of` 3 with `flighty` still in it.
        let m = s.move_queued(c, To(2), &[flighty]).unwrap();
        assert_eq!(m.task.id, c);
        assert_eq!((m.pos, m.of), (2, 2));
    }

    /// Only a queued task's level changes; the refusal names the state.
    #[test]
    fn set_priority_changes_only_a_queued_task() {
        let s = Store::open_in_memory().unwrap();
        let a = s.insert_task(new_task("run")).unwrap();
        let b = s.insert_task(new_task("run")).unwrap();
        let t = s
            .set_priority(b.id, Priority::Critical, "task priority")
            .unwrap();
        assert_eq!(t.priority, Priority::Critical);
        assert_eq!(t.priority_from.as_deref(), Some("task priority"));
        assert_eq!(t.queue_pos, b.id);
        assert_eq!(s.queued_tasks().unwrap()[0].id, b.id);
        s.claim_task(a.id, "m").unwrap().unwrap();
        match s.set_priority(a.id, Priority::Low, "task priority") {
            Err(PriorityError::NotQueued { state, .. }) => assert_eq!(state, TaskState::Starting),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            s.set_priority(99, Priority::Low, "task priority"),
            Err(PriorityError::NotFound(99))
        ));
        assert_eq!(
            s.get_task(a.id).unwrap().unwrap().priority,
            Priority::Normal
        );
    }

    /// Event sequence numbers keep growing across a restart of the store,
    /// and a v7 database, which has none, starts them at 1.
    #[test]
    fn event_seq_survives_a_reopen_and_a_v7_database_gains_it() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.db");
        {
            let s = Store::open(&path).unwrap();
            assert_eq!(s.last_event_seq().unwrap(), 0);
            assert_eq!(s.next_event_seq().unwrap(), 1);
            assert_eq!(s.next_event_seq().unwrap(), 2);
        }
        {
            let s = Store::open(&path).unwrap();
            assert_eq!(s.last_event_seq().unwrap(), 2);
            assert_eq!(s.next_event_seq().unwrap(), 3);
            s.execute_raw(
                "DROP TABLE event_seq;
                 UPDATE meta SET value = '7' WHERE key = 'schema_version'",
            );
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(
            s.meta("schema_version").unwrap().unwrap(),
            SCHEMA_VERSION.to_string()
        );
        assert_eq!(s.next_event_seq().unwrap(), 1);
    }

    /// A v2 database predates `retry_of`; opening it adds the column empty.
    #[test]
    fn a_v2_database_gains_retry_of() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.db");
        {
            let s = Store::open(&path).unwrap();
            s.insert_task(new_task("run")).unwrap();
            s.execute_raw(
                "ALTER TABLE tasks DROP COLUMN retry_of;
                 ALTER TABLE tasks DROP COLUMN flock;
                 UPDATE meta SET value = '2' WHERE key = 'schema_version'",
            );
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.get_task(1).unwrap().unwrap().retry_of, None);
        assert_eq!(
            s.meta("schema_version").unwrap().unwrap(),
            SCHEMA_VERSION.to_string()
        );
    }

    /// A v2 -> v3 migration that added `retry_of` but died before recording
    /// version 3 (a pastor from before the migration ran in a transaction)
    /// must still open: the step sees the column and skips the ALTER.
    #[test]
    fn a_half_applied_migration_still_opens() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.db");
        {
            let s = Store::open(&path).unwrap();
            s.insert_task(new_task("run")).unwrap();
            s.execute_raw("UPDATE meta SET value = '2' WHERE key = 'schema_version'");
        }
        let s = Store::open(&path).expect("column present, version 2");
        assert_eq!(
            s.meta("schema_version").unwrap().unwrap(),
            SCHEMA_VERSION.to_string()
        );
        assert_eq!(s.get_task(1).unwrap().unwrap().retry_of, None);
    }

    /// A migration step that fails part way leaves the file as it was, at
    /// its old version, instead of half changed.
    #[test]
    fn a_failed_migration_changes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.db");
        {
            let s = Store::open(&path).unwrap();
            // A v1 file whose version bump fails (a trigger aborts it)
            // after every ALTER has run: nothing of the migration may stay.
            s.execute_raw(
                "ALTER TABLE tasks DROP COLUMN prompt_pending;
                 ALTER TABLE tasks DROP COLUMN retry_of;
                 ALTER TABLE tasks DROP COLUMN flock;
                 ALTER TABLE tasks DROP COLUMN trust_sent;
                 ALTER TABLE tasks DROP COLUMN activity_seen;
                 ALTER TABLE tasks DROP COLUMN ended;
                 ALTER TABLE tasks DROP COLUMN priority;
                 ALTER TABLE tasks DROP COLUMN priority_from;
                 ALTER TABLE tasks DROP COLUMN queue_pos;
                 ALTER TABLE tasks DROP COLUMN role;
                 ALTER TABLE tasks DROP COLUMN description;
                 ALTER TABLE tasks DROP COLUMN preempt;
                 ALTER TABLE tasks DROP COLUMN paused_at;
                 ALTER TABLE tasks DROP COLUMN paused_for;
                 ALTER TABLE tasks DROP COLUMN resumed_at;
                 DROP TABLE trusted_repos;
                 DROP TABLE event_seq;
                 DROP TABLE task_summaries;
                 UPDATE meta SET value = '1' WHERE key = 'schema_version';
                 CREATE TRIGGER no_bump BEFORE UPDATE ON meta
                   WHEN NEW.value = '13' BEGIN SELECT RAISE(ABORT, 'boom'); END;",
            );
        }
        assert!(Store::open(&path).is_err());
        let conn = Connection::open(&path).unwrap();
        let v: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, "1");
        let cols: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('tasks')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            !cols.iter().any(|c| c == "prompt_pending"
                || c == "retry_of"
                || c == "flock"
                || c == "trust_sent"
                || c == "activity_seen"
                || c == "ended"
                || c == "priority"
                || c == "queue_pos"
                || c == "role"
                || c == "description"
                || c == "preempt"
                || c == "paused_at"),
            "rolled back: {cols:?}"
        );
        drop(conn);
        let tables = table_names(&path);
        assert!(
            !tables.contains(&"trusted_repos".to_string())
                && !tables.contains(&"event_seq".to_string())
                && !tables.contains(&"task_summaries".to_string()),
            "rolled back: {tables:?}"
        );
    }

    /// `tasks.id` has no AUTOINCREMENT, so SQLite hands out MAX(id) + 1:
    /// deleting the newest row would give its id to the next task, and with
    /// it the agent name `t-<id>` and branch the old one may still hold.
    /// Prune keeps the newest row so ids never repeat.
    #[test]
    fn prune_never_frees_the_newest_id() {
        let s = Store::open_in_memory().unwrap();
        let old = Utc::now() - chrono::Duration::days(4);
        for _ in 0..3 {
            let t = s.insert_task(new_task("run")).unwrap();
            let mut t = set_state(&s, t.id, TaskState::Closed);
            t.finished_at = Some(old);
            s.update_task(&mut t).unwrap();
        }
        let n = s
            .prune(&[TaskState::Closed], Duration::from_secs(86400))
            .unwrap()
            .pruned;
        assert_eq!(n, 2);
        assert!(s.get_task(3).unwrap().is_some(), "the newest row stays");
        assert_eq!(s.insert_task(new_task("run")).unwrap().id, 4);
    }

    #[test]
    fn a_closed_task_is_not_retried_and_the_error_names_its_state() {
        let s = Store::open_in_memory().unwrap();
        let t = s.insert_task(new_task("run")).unwrap();
        set_state(&s, t.id, TaskState::Closed);
        let before = s.list_tasks(&TaskFilter::default()).unwrap().len();
        let err = s.insert_retry(t.id).unwrap_err();
        assert!(err.to_string().contains("is closed"), "{err}");
        assert_eq!(s.list_tasks(&TaskFilter::default()).unwrap().len(), before);
        let err = s.insert_retry(99).unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }

    fn set_state(s: &Store, id: i64, state: TaskState) -> Task {
        let mut t = s.get_task(id).unwrap().unwrap();
        t.state = state;
        s.update_task(&mut t).unwrap();
        t
    }

    #[test]
    fn insert_retry_copies_the_work_and_links_back() {
        let s = Store::open_in_memory().unwrap();
        let item = serde_json::json!({"key": "k1", "title": "t"});
        let old = s
            .insert_job_task("j", "default", &item, None, |_| Ok(("p".into(), spec())))
            .unwrap();
        for state in [
            TaskState::Queued,
            TaskState::Starting,
            TaskState::Running,
            TaskState::Blocked,
            TaskState::Done,
            TaskState::Closed,
        ] {
            set_state(&s, old.id, state);
            let err = s.insert_retry(old.id).unwrap_err();
            assert!(
                matches!(err, RetryError::NotRetryable { id, state: st } if id == old.id && st == state),
                "{err}"
            );
            assert!(err.to_string().contains("only failed or stale"), "{err}");
        }
        assert!(matches!(
            s.insert_retry(99).unwrap_err(),
            RetryError::NotFound(99)
        ));

        for state in [TaskState::Failed, TaskState::Stale] {
            let mut o = set_state(&s, old.id, state);
            o.machine = Some("pi-1".into());
            o.pane_id = Some("w1:p1".into());
            o.error = Some("boom".into());
            s.update_task(&mut o).unwrap();
            let r = s.insert_retry(old.id).unwrap();
            assert_ne!(r.id, old.id);
            assert_eq!(r.retry_of, Some(old.id));
            assert_eq!(r.state, TaskState::Queued);
            assert_eq!((r.job.as_str(), r.prompt.as_str()), ("j", "p"));
            assert_eq!(r.item, item);
            // What a retry changes in the spec: see
            // `a_retry_carries_only_a_failed_task_s_own_checkout`. This one
            // owns no checkout, so it drops the branch.
            let copied = DispatchSpec {
                branch: None,
                ..spec()
            };
            assert_eq!(r.spec, copied);
            assert_eq!((r.machine, r.pane_id, r.error), (None, None, None));
            assert_eq!(s.get_task(r.id).unwrap().unwrap().retry_of, Some(old.id));
        }
        assert_eq!(
            s.get_task(old.id).unwrap().unwrap().state,
            TaskState::Stale,
            "the old row is left alone"
        );
        assert!(s.is_seen("j", "k1").unwrap());
    }

    #[test]
    fn insert_retry_reports_a_storage_failure_as_such() {
        let s = Store::open_in_memory().unwrap();
        let t = s.insert_task(new_task("run")).unwrap();
        set_state(&s, t.id, TaskState::Failed);
        s.execute_raw(
            "CREATE TRIGGER no_insert BEFORE INSERT ON tasks BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
        );
        assert!(matches!(
            s.insert_retry(t.id).unwrap_err(),
            RetryError::Store(_)
        ));
    }

    #[test]
    fn close_task_keeps_a_finish_time_and_is_idempotent() {
        let s = Store::open_in_memory().unwrap();
        let t = s.insert_task(new_task("run")).unwrap();
        let mut done = set_state(&s, t.id, TaskState::Done);
        let finished = Utc::now() - chrono::Duration::hours(2);
        done.finished_at = Some(finished);
        s.update_task(&mut done).unwrap();
        let closed = s.close_task(t.id).unwrap();
        assert_eq!(closed.state, TaskState::Closed);
        assert_eq!(closed.finished_at, Some(finished));
        let again = s.close_task(t.id).unwrap();
        assert_eq!(again.updated_at, closed.updated_at, "nothing written");

        let r = s.insert_task(new_task("run")).unwrap();
        set_state(&s, r.id, TaskState::Running);
        let closed = s.close_task(r.id).unwrap();
        assert!(closed.finished_at.is_some(), "a running task finishes now");
        assert_eq!(s.get_task(r.id).unwrap().unwrap().state, TaskState::Closed);
        assert!(s.close_task(99).is_err());
    }

    /// A queued task has no machine to close it through, so its row is
    /// closed in SQL, and only while it is still queued: a dispatch that
    /// claimed it in between wins, and the close must go to its machine.
    #[test]
    fn close_queued_closes_only_a_task_nobody_claimed() {
        let s = Store::open_in_memory().unwrap();
        let q = s.insert_task(new_task("run")).unwrap();
        let closed = s.close_queued(q.id).unwrap().expect("still queued");
        assert_eq!(closed.state, TaskState::Closed);
        assert!(closed.finished_at.is_some());
        assert!(s.close_queued(q.id).unwrap().is_none(), "closed already");

        let c = s.insert_task(new_task("run")).unwrap();
        s.claim_task(c.id, "m").unwrap().expect("claimed");
        assert!(s.close_queued(c.id).unwrap().is_none(), "lost to the claim");
        let row = s.get_task(c.id).unwrap().unwrap();
        assert_eq!(row.state, TaskState::Starting);
        assert_eq!(row.machine.as_deref(), Some("m"));
        assert!(s.close_queued(99).unwrap().is_none());
    }

    /// A worktree task's checkout outlives a plain `task close`, which only
    /// closes the pane; the row is then the only record of which workspace
    /// to remove. Prune keeps such a row until `--remove-worktree` has
    /// cleared its workspace, and says how many it kept.
    #[test]
    fn prune_keeps_a_row_whose_worktree_may_still_be_on_disk() {
        let s = Store::open_in_memory().unwrap();
        let old = Utc::now() - chrono::Duration::days(4);
        let mk = |worktree: bool, workspace: Option<&str>| {
            let t = s
                .insert_task(NewTask {
                    spec: DispatchSpec { worktree, ..spec() },
                    ..new_task("run")
                })
                .unwrap();
            let mut t = set_state(&s, t.id, TaskState::Closed);
            t.workspace_id = workspace.map(str::to_string);
            t.finished_at = Some(old);
            s.update_task(&mut t).unwrap();
            t.id
        };
        let on_disk = mk(true, Some("w1"));
        let removed = mk(true, None);
        let plain = mk(false, Some("w3"));
        let _newest = mk(false, None);

        let out = s
            .prune(&[TaskState::Closed], Duration::from_secs(86400))
            .unwrap();
        assert_eq!(
            out,
            PruneOutcome {
                pruned: 2,
                kept_worktrees: vec![on_disk]
            }
        );
        assert!(
            s.get_task(on_disk).unwrap().is_some(),
            "its checkout may remain"
        );
        assert!(s.get_task(removed).unwrap().is_none());
        assert!(s.get_task(plain).unwrap().is_none());
    }

    #[test]
    fn prune_deletes_old_finished_rows_and_keeps_seen() {
        let s = Store::open_in_memory().unwrap();
        let old = Utc::now() - chrono::Duration::days(4);
        let mk = |key: &str, state: TaskState, finished: Option<DateTime<Utc>>| {
            let item = serde_json::json!({ "key": key });
            let t = s
                .insert_job_task("j", "default", &item, None, |_| Ok(("p".into(), spec())))
                .unwrap();
            let mut t = set_state(&s, t.id, state);
            t.finished_at = finished;
            s.update_task(&mut t).unwrap();
            t.id
        };
        let old_done = mk("a", TaskState::Done, Some(old));
        let new_done = mk("b", TaskState::Done, Some(Utc::now()));
        let old_closed = mk("c", TaskState::Closed, Some(old));
        let old_failed = mk("d", TaskState::Failed, Some(old));
        let running = mk("e", TaskState::Running, None);
        let three_days = Duration::from_secs(3 * 86400);

        assert_eq!(s.prune(&[TaskState::Done], three_days).unwrap().pruned, 1);
        assert!(s.get_task(old_done).unwrap().is_none());
        assert!(s.get_task(new_done).unwrap().is_some());
        assert!(s.get_task(old_closed).unwrap().is_some());
        assert_eq!(
            s.prune(&[TaskState::Closed, TaskState::Failed], three_days)
                .unwrap()
                .pruned,
            2
        );
        assert!(s.get_task(old_failed).unwrap().is_none());
        assert!(s.get_task(running).unwrap().is_some());
        assert!(s.is_seen("j", "a").unwrap(), "a pruned item stays seen");
        assert_eq!(s.prune(&[], three_days).unwrap().pruned, 0);
        let err = s.prune(&[TaskState::Running], three_days).unwrap_err();
        assert!(err.to_string().contains("cannot be pruned"), "{err}");
        assert!(s.get_task(running).unwrap().is_some());
    }

    /// `pastor machine list` and `pastor task list` open the database while
    /// `pastor serve` may be creating it; whichever comes second must wait
    /// for the other's transaction, not fail with "database is locked".
    #[test]
    fn open_waits_for_another_connections_write_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.db");
        let mut holder = Connection::open(&path).unwrap();
        let tx = holder
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let opener = {
            let path = path.clone();
            std::thread::spawn(move || Store::open(&path).map(|_| ()))
        };
        std::thread::sleep(std::time::Duration::from_millis(300));
        tx.commit().unwrap();
        let opened = opener.join().unwrap();
        assert!(opened.is_ok(), "{:#}", opened.unwrap_err());
    }
}
