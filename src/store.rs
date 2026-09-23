use std::path::Path;
use std::sync::Mutex;

use anyhow::Context;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::Value;

use crate::task::{DispatchSpec, Task, TaskState};

const SCHEMA_VERSION: i64 = 1;

pub struct Store {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone)]
pub struct NewTask {
    pub job: String,
    pub item: Value,
    pub prompt: String,
    pub spec: DispatchSpec,
}

#[derive(Debug, Clone, Default)]
pub struct TaskFilter {
    pub job: Option<String>,
    pub machine: Option<String>,
    pub states: Option<Vec<TaskState>>,
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

    fn init(conn: Connection) -> anyhow::Result<Store> {
        conn.execute_batch(
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
                created_at TEXT NOT NULL,
                started_at TEXT,
                finished_at TEXT,
                updated_at TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS tasks_state ON tasks(state);
             CREATE INDEX IF NOT EXISTS tasks_machine ON tasks(machine);",
        )?;
        let version: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        match version.map(|v| v.parse::<i64>().unwrap_or(0)) {
            None => {
                conn.execute(
                    "INSERT INTO meta (key, value) VALUES ('schema_version', ?1)",
                    params![SCHEMA_VERSION.to_string()],
                )?;
            }
            Some(v) if v == SCHEMA_VERSION => {}
            Some(v) if v < SCHEMA_VERSION => {
                // Future migrations go here, one `if v < N` block each.
                conn.execute(
                    "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
                    params![SCHEMA_VERSION.to_string()],
                )?;
            }
            Some(v) => anyhow::bail!(
                "database schema {v} is newer than this pastor ({SCHEMA_VERSION}); refusing to touch it"
            ),
        }
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

    pub fn update_task(&self, t: &Task) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE tasks SET machine = ?2, workspace_id = ?3, pane_id = ?4, agent_name = ?5, state = ?6, error = ?7,
                last_completion_seq = ?8, started_at = ?9, finished_at = ?10, updated_at = ?11, prompt = ?12, spec = ?13
             WHERE id = ?1",
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
                Utc::now().to_rfc3339(),
                t.prompt,
                serde_json::to_string(&t.spec)?,
            ],
        )?;
        anyhow::ensure!(n == 1, "task {} not found", t.id);
        Ok(())
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
    pub fn tasks_on_machine(&self, machine: &str) -> anyhow::Result<Vec<Task>> {
        let all = self.list_tasks(&TaskFilter {
            machine: Some(machine.into()),
            ..Default::default()
        })?;
        Ok(all
            .into_iter()
            .filter(|t| t.state.occupies_pane())
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

    pub fn find_by_pane(&self, machine: &str, pane_id: &str) -> anyhow::Result<Option<Task>> {
        Ok(self
            .tasks_on_machine(machine)?
            .into_iter()
            .find(|t| t.pane_id.as_deref() == Some(pane_id)))
    }

    /// Test-only escape hatch to corrupt rows directly and check that reads
    /// surface it instead of reinterpreting it.
    #[cfg(test)]
    pub(crate) fn execute_raw(&self, sql: &str) {
        self.conn.lock().unwrap().execute_batch(sql).unwrap();
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
            .map(|v| v as u64),
        created_at: parse_dt(&created_at)?,
        started_at: started_at.as_deref().map(parse_dt).transpose()?,
        finished_at: finished_at.as_deref().map(parse_dt).transpose()?,
        updated_at: parse_dt(&updated_at)?,
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
        s.update_task(&got).unwrap();
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
        s.update_task(&a).unwrap();
        s.update_task(&b).unwrap();
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

        s.execute_raw("UPDATE tasks SET item = '{}' WHERE id = 1");

        let t2 = s.insert_task(new_task("run")).unwrap();
        assert!(s.get_task(t2.id).unwrap().is_some());
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
}
