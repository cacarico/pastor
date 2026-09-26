use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::task::{DispatchSpec, PANE_OWNING_STATES, Task, TaskState};

const SCHEMA_VERSION: i64 = 7;

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

/// One saved trust, as `pastor trust list` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedRepo {
    pub machine: String,
    pub repo: String,
    pub trusted_at: DateTime<Utc>,
}

pub struct Store {
    conn: Mutex<Connection>,
}

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
        })
    }

    pub fn insert_task(&self, t: NewTask) -> anyhow::Result<Task> {
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO tasks (job, item, prompt, spec, flock, state, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6, ?6)",
            params![t.job, serde_json::to_string(&t.item)?, t.prompt, serde_json::to_string(&t.spec)?, t.flock, now],
        )?;
        let id = conn.last_insert_rowid();
        drop(conn);
        self.get_task(id)?.context("task vanished after insert")
    }

    pub fn get_task(&self, id: i64) -> anyhow::Result<Option<Task>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT * FROM tasks WHERE id = ?1",
                params![id],
                row_to_task,
            )
            .optional()?)
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
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE tasks SET machine = ?2, workspace_id = ?3, pane_id = ?4, agent_name = ?5, state = ?6, error = ?7,
                last_completion_seq = ?8, started_at = ?9, finished_at = ?10, updated_at = ?11, prompt = ?12, spec = ?13,
                prompt_pending = ?14, activity_seen = ?16, ended = ?17
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
    }

    /// The one transition dispatch is allowed to make on its own, done in SQL so
    /// concurrent dispatch passes cannot both take a task: `queued` -> `starting`
    /// on `machine`, with the agent name dispatch will use. `None` means the task
    /// was not queued any more (or never existed). This is `Observed::DispatchStarting`
    /// as a conditional UPDATE; `task::next_state` keeps the rule readable.
    pub fn claim_task(&self, id: i64, machine: &str) -> anyhow::Result<Option<Task>> {
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
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
        let conn = self.conn.lock().unwrap();
        // Check and copy in one statement, so a task closed, pruned or
        // finished by another writer in between is not retried.
        let n = conn.execute(
            "INSERT INTO tasks (job, item, prompt, spec, flock, state, retry_of, created_at, updated_at)
             SELECT job, item, prompt,
                    json_patch(CASE WHEN COALESCE(json_extract(spec, '$.worktree'), 0) = 0
                         THEN json_remove(spec, '$.checkout', '$.reopen')
                         WHEN state = 'failed' AND json_extract(spec, '$.checkout') IS NOT NULL
                         THEN json_set(json_remove(spec, '$.branch', '$.checkout'), '$.reopen',
                                       json_object('branch', json_extract(spec, '$.checkout.branch'),
                                                   'path', json_extract(spec, '$.checkout.path'),
                                                   'agent', COALESCE(agent_name, 't-' || id)))
                         ELSE json_remove(spec, '$.branch', '$.checkout', '$.reopen') END, ?3),
                    flock, 'queued', id, ?2, ?2 FROM tasks
             WHERE id = ?1 AND state IN ('failed', 'stale')",
            params![of, now, patch],
        )?;
        if n == 0 {
            let state: Option<String> = conn
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
        let id = conn.last_insert_rowid();
        drop(conn);
        Ok(self.get_task(id)?.context("task vanished after insert")?)
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

    /// Close task `id` only if it is still `queued`, in one conditional
    /// UPDATE. A queued task has no machine yet, so nothing but its row to
    /// close; `None` means it was not queued any more, typically because a
    /// dispatch claimed it in between (`claim_task`), and the close must then
    /// go through that machine. The counterpart of `claim_task`.
    pub fn close_queued(&self, id: i64) -> anyhow::Result<Option<Task>> {
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE tasks SET state = 'closed', finished_at = COALESCE(finished_at, ?2), updated_at = ?2
             WHERE id = ?1 AND state = 'queued'",
            params![id, now],
        )?;
        drop(conn);
        if n == 0 {
            return Ok(None);
        }
        self.get_task(id)
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
        let mut conn = self.conn.lock().unwrap();
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
        tx.commit()?;
        Ok(PruneOutcome {
            pruned,
            kept_worktrees,
        })
    }

    pub fn list_tasks(&self, f: &TaskFilter) -> anyhow::Result<Vec<Task>> {
        let mut sql = String::from("SELECT * FROM tasks WHERE 1=1");
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
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params_from_iter(args.iter().map(|a| a.as_ref())),
            row_to_task,
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
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

    pub fn queued_tasks(&self) -> anyhow::Result<Vec<Task>> {
        let mut v = self.list_tasks(&TaskFilter {
            states: Some(vec![TaskState::Queued]),
            ..Default::default()
        })?;
        v.reverse();
        Ok(v)
    }

    /// Put every task that has no flock, a row from before flocks, in
    /// `default`: the default flock of flock.toml as the caller read it.
    /// Called wherever the store is opened with the flock file at hand, so
    /// such a row is only ever seen flockless between a migration and the
    /// first read of flock.toml. Returns how many rows it changed.
    pub fn adopt_default_flock(&self, default: &str) -> anyhow::Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "UPDATE tasks SET flock = ?1 WHERE flock IS NULL",
            params![default],
        )?)
    }

    pub fn find_by_pane(&self, machine: &str, pane_id: &str) -> anyhow::Result<Option<Task>> {
        Ok(self
            .tasks_on_machine(machine)?
            .into_iter()
            .find(|t| t.pane_id.as_deref() == Some(pane_id)))
    }

    pub fn job_state(&self, name: &str) -> anyhow::Result<Option<JobState>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT * FROM job_state WHERE name = ?1",
                params![name],
                row_to_job_state,
            )
            .optional()?)
    }

    pub fn job_states(&self) -> anyhow::Result<Vec<JobState>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM job_state ORDER BY name")?;
        let rows = stmt.query_map([], row_to_job_state)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn save_job_state(&self, s: &JobState) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
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
    }

    pub fn is_seen(&self, job: &str, key: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM seen WHERE job = ?1 AND key = ?2",
            params![job, key],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Insert a queued task for `item`, render its prompt and spec with the id
    /// it was given, and record `(job, key)` as seen: one transaction, so no
    /// reader ever sees an unrendered task and a render failure leaves the key
    /// unseen. A key already in `seen` violates the primary key and nothing is
    /// written.
    pub fn insert_job_task(
        &self,
        job: &str,
        flock: &str,
        item: &Value,
        render: impl FnOnce(i64) -> Result<(String, DispatchSpec), String>,
    ) -> anyhow::Result<Task> {
        let key = item
            .get("key")
            .and_then(Value::as_str)
            .context("item has no string key")?
            .to_string();
        let now = Utc::now().to_rfc3339();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO tasks (job, item, prompt, spec, flock, state, created_at, updated_at) VALUES (?1, ?2, '', '{}', ?3, 'queued', ?4, ?4)",
            params![job, serde_json::to_string(item)?, flock, now],
        )?;
        let id = tx.last_insert_rowid();
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
    }

    /// Test-only escape hatch to corrupt rows directly and check that reads
    /// surface it instead of reinterpreting it.
    #[cfg(test)]
    pub(crate) fn execute_raw(&self, sql: &str) {
        self.conn.lock().unwrap().execute_batch(sql).unwrap();
    }

    /// Save `repo` on `machine` as trusted. Returns whether it was new.
    pub fn trust_repo(&self, machine: &str, repo: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "INSERT OR IGNORE INTO trusted_repos (machine, repo, trusted_at) VALUES (?1, ?2, ?3)",
            params![machine, repo, Utc::now().to_rfc3339()],
        )?;
        Ok(n == 1)
    }

    pub fn is_trusted(&self, machine: &str, repo: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM trusted_repos WHERE machine = ?1 AND repo = ?2)",
            params![machine, repo],
            |r| r.get(0),
        )?)
    }

    /// Every saved trust, by machine then repo.
    pub fn trusted_repos(&self) -> anyhow::Result<Vec<TrustedRepo>> {
        let conn = self.conn.lock().unwrap();
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
    }

    /// Forget a saved trust. Returns whether there was one.
    pub fn untrust(&self, machine: &str, repo: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM trusted_repos WHERE machine = ?1 AND repo = ?2",
            params![machine, repo],
        )?;
        Ok(n == 1)
    }

    /// Whether task `id` has had its trust keys sent. False for no such row.
    pub fn trust_sent(&self, id: i64) -> anyhow::Result<bool> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT trust_sent FROM tasks WHERE id = ?1",
                params![id],
                |r| r.get::<_, bool>(0),
            )
            .optional()?
            .unwrap_or(false))
    }

    /// Mark task `id` as having had its trust keys sent. True only for the
    /// call that set it, so the keys go to a task once, across restarts.
    /// `update_task` never writes the column, so no stale copy resets it.
    pub fn claim_trust_sent(&self, id: i64) -> anyhow::Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE tasks SET trust_sent = 1 WHERE id = ?1 AND trust_sent = 0",
            params![id],
        )?;
        Ok(n == 1)
    }

    #[cfg(test)]
    pub(crate) fn meta(&self, key: &str) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()?)
    }
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

fn row_to_task(row: &Row<'_>) -> rusqlite::Result<Task> {
    let parse_dt = |s: &str| -> rusqlite::Result<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(s)
            .map(|d| d.with_timezone(&Utc))
            .map_err(conversion_failure)
    };
    let item: String = row.get("item")?;
    let spec: String = row.get("spec")?;
    let state: String = row.get("state")?;
    let created_at: String = row.get("created_at")?;
    let updated_at: String = row.get("updated_at")?;
    let started_at: Option<String> = row.get("started_at")?;
    let finished_at: Option<String> = row.get("finished_at")?;
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
    use crate::task::{Checkout, Reopen};

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
        }
    }

    fn new_task(job: &str) -> NewTask {
        NewTask {
            job: job.into(),
            item: serde_json::json!({"key": "k1", "title": "t"}),
            prompt: "do it\nnow \"quoted\" {{ x }}".into(),
            spec: spec(),
            flock: "default".into(),
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
        assert_eq!(table_names(&path), vec!["meta", "tasks", "trusted_repos"]);
        let s = Store::open(&path).unwrap();
        assert_eq!(
            table_names(&path),
            vec!["job_state", "meta", "seen", "tasks", "trusted_repos"]
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
            vec!["job_state", "meta", "seen", "tasks", "trusted_repos"]
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
            .insert_job_task("j", "default", &item, |id| {
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
            s.insert_job_task("j", "default", &item, |_| Ok(("x".into(), spec())))
                .is_err()
        );
        assert_eq!(s.list_tasks(&TaskFilter::default()).unwrap().len(), before);

        // A render failure rolls the whole thing back: no task, key still unseen.
        let item2 = serde_json::json!({"key": "k2"});
        let err = s
            .insert_job_task("j", "default", &item2, |_| Err("nope".into()))
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
            .insert_job_task("j", "work", &serde_json::json!({"key": "k"}), |_| {
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
                s.update_task(&mut t).unwrap();
            }
            let r = s.insert_retry(t.id).unwrap();
            assert_eq!(r.spec.checkout, None, "a retry owns no checkout yet");
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
                 DROP TABLE trusted_repos;
                 UPDATE meta SET value = '1' WHERE key = 'schema_version';
                 CREATE TRIGGER no_bump BEFORE UPDATE ON meta
                   WHEN NEW.value = '7' BEGIN SELECT RAISE(ABORT, 'boom'); END;",
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
                || c == "ended"),
            "rolled back: {cols:?}"
        );
        drop(conn);
        assert!(
            !table_names(&path).contains(&"trusted_repos".to_string()),
            "rolled back"
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
            .insert_job_task("j", "default", &item, |_| Ok(("p".into(), spec())))
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
                .insert_job_task("j", "default", &item, |_| Ok(("p".into(), spec())))
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
