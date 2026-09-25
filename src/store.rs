use std::path::Path;
use std::sync::Mutex;

use anyhow::Context;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::Value;

use crate::task::{DispatchSpec, PANE_OWNING_STATES, Task, TaskState};

const SCHEMA_VERSION: i64 = 2;

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

pub struct Store {
    conn: Mutex<Connection>,
}

/// `update_task` found the row changed since this copy was read. The caller holds
/// stale data; reload and decide again rather than overwrite.
#[derive(Debug, thiserror::Error)]
#[error("task t-{id} changed underneath this update; reload it and apply again")]
pub struct Conflict {
    pub id: i64,
}

#[derive(Debug, Clone)]
pub struct NewTask {
    pub job: String,
    pub item: Value,
    pub prompt: String,
    pub spec: DispatchSpec,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TaskFilter {
    pub job: Option<String>,
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
    pub fn open(path: &Path) -> anyhow::Result<Store> {
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
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
                        created_at TEXT NOT NULL,
                        started_at TEXT,
                        finished_at TEXT,
                        updated_at TEXT NOT NULL
                     );
                     CREATE INDEX IF NOT EXISTS tasks_state ON tasks(state);
                     CREATE INDEX IF NOT EXISTS tasks_machine ON tasks(machine);",
                )?;
                tx.execute_batch(V2_TABLES)?;
                tx.execute(
                    "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)",
                    params![SCHEMA_VERSION.to_string()],
                )?;
            }
            // The pastor before plan 2 already wrote version 2 without the
            // job tables, so a current file still gets them if missing.
            Some(v) if v == SCHEMA_VERSION => tx.execute_batch(V2_TABLES)?,
            Some(v) if v < SCHEMA_VERSION => {
                // One `if v < N` block per migration.
                if v < 2 {
                    tx.execute_batch(V2_TABLES)?;
                    tx.execute(
                        "ALTER TABLE tasks ADD COLUMN prompt_pending INTEGER NOT NULL DEFAULT 0",
                        [],
                    )?;
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
            "INSERT INTO tasks (job, item, prompt, spec, state, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, 'queued', ?5, ?5)",
            params![t.job, serde_json::to_string(&t.item)?, t.prompt, serde_json::to_string(&t.spec)?, now],
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
                prompt_pending = ?14
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

    pub fn list_tasks(&self, f: &TaskFilter) -> anyhow::Result<Vec<Task>> {
        let mut sql = String::from("SELECT * FROM tasks WHERE 1=1");
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(job) = &f.job {
            args.push(Box::new(job.clone()));
            sql.push_str(&format!(" AND job = ?{}", args.len()));
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

    pub fn queued_tasks(&self) -> anyhow::Result<Vec<Task>> {
        let mut v = self.list_tasks(&TaskFilter {
            states: Some(vec![TaskState::Queued]),
            ..Default::default()
        })?;
        v.reverse();
        Ok(v)
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
            "INSERT INTO tasks (job, item, prompt, spec, state, created_at, updated_at) VALUES (?1, ?2, '', '{}', 'queued', ?3, ?3)",
            params![job, serde_json::to_string(item)?, now],
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
        // Held in the machine actor's memory, never stored.
        activity_seen: false,
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

    fn spec() -> DispatchSpec {
        DispatchSpec {
            agent: "claude".into(),
            agent_args: vec!["--model".into(), "x".into()],
            repo: Some("~/w".into()),
            worktree: true,
            branch: Some("b".into()),
            machine: None,
            tags: vec!["fast".into()],
            timeout_secs: 60,
        }
    }

    fn new_task(job: &str) -> NewTask {
        NewTask {
            job: job.into(),
            item: serde_json::json!({"key": "k1", "title": "t"}),
            prompt: "do it\nnow \"quoted\" {{ x }}".into(),
            spec: spec(),
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

    /// The pastor before plan 2 already wrote schema 2 (with prompt_pending)
    /// but had no seen or job_state tables.
    #[test]
    fn a_v2_database_without_the_job_tables_gains_them() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.db");
        {
            let s = Store::open(&path).unwrap();
            s.insert_task(new_task("run")).unwrap();
            s.execute_raw("DROP TABLE seen; DROP TABLE job_state;");
        }
        assert_eq!(table_names(&path), vec!["meta", "tasks"]);
        let s = Store::open(&path).unwrap();
        assert_eq!(
            table_names(&path),
            vec!["job_state", "meta", "seen", "tasks"]
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
            vec!["job_state", "meta", "seen", "tasks"]
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
            .insert_job_task("j", &item, |id| {
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
            s.insert_job_task("j", &item, |_| Ok(("x".into(), spec())))
                .is_err()
        );
        assert_eq!(s.list_tasks(&TaskFilter::default()).unwrap().len(), before);

        // A render failure rolls the whole thing back: no task, key still unseen.
        let item2 = serde_json::json!({"key": "k2"});
        let err = s
            .insert_job_task("j", &item2, |_| Err("nope".into()))
            .unwrap_err();
        assert!(err.to_string().contains("nope"), "{err}");
        assert_eq!(s.list_tasks(&TaskFilter::default()).unwrap().len(), before);
        assert!(!s.is_seen("j", "k2").unwrap());

        // No string key: refused up front.
        assert!(
            s.insert_job_task("j", &serde_json::json!({"title": "no key"}), |_| Ok((
                "x".into(),
                spec()
            )))
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
            // A genuine v1 file has neither the plan-2 tables nor the
            // prompt_pending column; v2 gained both.
            s.execute_raw(
                "DROP TABLE seen; DROP TABLE job_state;
                 ALTER TABLE tasks DROP COLUMN prompt_pending;
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
        assert_eq!(v, "2");
    }
}
