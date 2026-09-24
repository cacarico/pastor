//! `pastor task retry|close|prune`: argument types and handlers. `main.rs` only
//! holds one `TaskCmd` variant per command and calls these.
use clap::{ArgGroup, Args};

use crate::cli::{CliError, TASK_HEADER, table, task_rows};
use crate::config::{Paths, parse_duration};
use crate::ipc::{IpcRequest, IpcResponse, daemon_running, request};
use crate::store::Store;
use crate::task::{Task, TaskState, parse_task_id};

#[derive(Args, Debug)]
pub struct RetryArgs {
    /// A failed or stale task, like t-12
    pub task: String,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct CloseArgs {
    /// A task, or an orphaned agent named like one (t-12)
    pub task: String,
    /// Remove the task's worktree too (refused if it has uncommitted changes)
    #[arg(long)]
    pub remove_worktree: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
#[command(group(ArgGroup::new("prune_states").required(true).multiple(true)))]
pub struct PruneArgs {
    /// Prune done tasks
    #[arg(long, group = "prune_states")]
    pub done: bool,
    /// Prune failed tasks
    #[arg(long, group = "prune_states")]
    pub failed: bool,
    /// Prune closed tasks
    #[arg(long, group = "prune_states")]
    pub closed: bool,
    /// Only tasks that finished longer ago than this (30m, 12h, 3d)
    #[arg(long, value_name = "DURATION")]
    pub older_than: String,
    #[arg(long)]
    pub json: bool,
}

fn task_id(s: &str) -> anyhow::Result<i64> {
    parse_task_id(s)
        .ok_or_else(|| CliError::err("usage_error", format!("{s} is not a task id like t-12")))
}

/// One request to the daemon. Retry and close need it: one dispatches, the
/// other talks to herdr on the task's machine.
async fn ask(paths: &Paths, req: IpcRequest) -> anyhow::Result<IpcResponse> {
    let resp = request(&paths.socket_file(), &req).await.map_err(|e| {
        CliError::err(
            "daemon_not_running",
            format!("pastor serve is not running ({e}); start it with `pastor serve`"),
        )
    })?;
    match resp {
        IpcResponse::Error { code, message } => Err(CliError::err(&code, message)),
        other => Ok(other),
    }
}

fn print_task(t: &Task, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(t)?);
    } else {
        println!(
            "{}",
            table(&TASK_HEADER, &task_rows(std::slice::from_ref(t)))
        );
    }
    Ok(())
}

fn unexpected(resp: IpcResponse) -> anyhow::Error {
    CliError::err("internal", format!("unexpected daemon reply: {resp:?}"))
}

/// `pastor task retry t-N`: a new task copying t-N, dispatched now.
pub async fn retry(paths: &Paths, a: RetryArgs) -> anyhow::Result<()> {
    let id = task_id(&a.task)?;
    match ask(paths, IpcRequest::TaskRetry { id }).await? {
        IpcResponse::Task(t) => print_task(&t, a.json),
        other => Err(unexpected(other)),
    }
}

/// `pastor task close t-N [--remove-worktree]`.
pub async fn close(paths: &Paths, a: CloseArgs) -> anyhow::Result<()> {
    let id = task_id(&a.task)?;
    let req = IpcRequest::TaskClose {
        id,
        remove_worktree: a.remove_worktree,
    };
    match ask(paths, req).await? {
        IpcResponse::Task(t) => print_task(&t, a.json),
        // An orphaned agent with no row: there is no task to print.
        IpcResponse::Text(msg) if a.json => {
            println!("{}", serde_json::json!({"message": msg}));
            Ok(())
        }
        IpcResponse::Text(msg) => {
            println!("{msg}");
            Ok(())
        }
        other => Err(unexpected(other)),
    }
}

impl PruneArgs {
    pub fn states(&self) -> Vec<TaskState> {
        [
            (self.done, TaskState::Done),
            (self.failed, TaskState::Failed),
            (self.closed, TaskState::Closed),
        ]
        .into_iter()
        .filter_map(|(on, s)| on.then_some(s))
        .collect()
    }
}

/// `pastor task prune --done|--failed|--closed --older-than D`. Through the
/// daemon when one runs; otherwise straight on the database, since it needs no
/// machine.
pub async fn prune(paths: &Paths, a: PruneArgs) -> anyhow::Result<()> {
    let older_than = parse_duration(&a.older_than).map_err(|e| CliError::err("usage_error", e))?;
    let states = a.states();
    let n = if daemon_running(&paths.socket_file()).await {
        let req = IpcRequest::TaskPrune {
            states,
            older_than_secs: older_than.as_secs(),
        };
        match ask(paths, req).await? {
            IpcResponse::Text(n) => n
                .parse::<usize>()
                .map_err(|_| CliError::err("internal", format!("prune count {n:?}")))?,
            other => return Err(unexpected(other)),
        }
    } else {
        paths.ensure()?;
        Store::open(&paths.db_file())?.prune(&states, older_than)?
    };
    if a.json {
        println!("{}", serde_json::json!({ "pruned": n }));
    } else {
        let what: Vec<&str> = a.states().iter().map(TaskState::as_str).collect();
        println!(
            "pruned {n} {} task{} older than {}",
            what.join("/"),
            if n == 1 { "" } else { "s" },
            a.older_than
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser, Debug)]
    struct P {
        #[command(flatten)]
        a: PruneArgs,
    }

    #[test]
    fn prune_needs_a_state_and_takes_several() {
        assert!(P::try_parse_from(["p", "--older-than", "3d"]).is_err());
        let p = P::try_parse_from(["p", "--done", "--closed", "--older-than", "3d"]).unwrap();
        assert_eq!(p.a.states(), vec![TaskState::Done, TaskState::Closed]);
    }

    #[test]
    fn task_ids_are_usage_errors() {
        let err = task_id("x-1").unwrap_err();
        assert_eq!(err.downcast_ref::<CliError>().unwrap().code, "usage_error");
        assert_eq!(task_id("t-7").unwrap(), 7);
    }
}
