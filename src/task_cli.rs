//! `pastor task retry|close|prune|send`: argument types and handlers. `main.rs` only
//! holds one `TaskCmd` variant per command and calls these.
use clap::{ArgGroup, Args};

use crate::cli::{CliError, ask, print_task, unexpected};
use crate::config::parse_duration;
use crate::ipc::{Client, Head, IpcRequest, IpcResponse};
use crate::machine::SendInput;
use crate::store::{PruneOutcome, Store};
use crate::task::{Priority, TaskState, UNKNOWN_PRIORITY, parse_task_id};

#[derive(Args, Debug)]
pub struct RetryArgs {
    /// A task, like t-12 or 12: a failed or stale one
    pub task: String,
    /// Where the new task's pane goes instead of the old one's: repo, own,
    /// pastor or pane:<workspace>
    #[arg(long, value_name = "PLACE")]
    pub place: Option<crate::task::Place>,
    /// Print as a JSON object
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct PriorityArgs {
    /// A task, like t-12 or 12: a queued one
    pub task: String,
    /// Its new level: low, normal, high or critical
    #[arg(value_name = "LEVEL")]
    pub level: String,
    /// Critical only: let the task pause the newest low Claude task on a
    /// full machine to start; without it the task's flag goes
    #[arg(long)]
    pub preempt: bool,
    /// Print as a JSON object
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct CloseArgs {
    /// Tasks, like t-12 or 12, or orphaned agents named like one; each is
    /// closed in turn, and one that fails does not stop the rest
    #[arg(required = true, value_name = "TASK")]
    pub tasks: Vec<String>,
    /// Remove each task's worktree too (refused if it has uncommitted changes)
    #[arg(long)]
    pub remove_worktree: bool,
    /// Print as a JSON object, or an array of them for several tasks
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct DoneArgs {
    /// A task, like t-12 or 12: the one to end (default: the task this pane
    /// runs, from PASTOR_TASK)
    pub task: Option<String>,
    /// What you did, its first line the outcome: done, partial, blocked or
    /// nothing to do (kept to 2,000 characters)
    #[arg(long, value_name = "TEXT", conflicts_with = "summary_file")]
    pub summary: Option<String>,
    /// Read the summary from a file, or - for stdin
    #[arg(long, value_name = "PATH")]
    pub summary_file: Option<String>,
    /// Print as a JSON object
    #[arg(long)]
    pub json: bool,
}

impl DoneArgs {
    /// The summary given: `--summary` as it is, or `--summary-file` read
    /// (`-` is stdin). A blank one is an error, not a round with none.
    pub fn summary_text(&self) -> anyhow::Result<Option<String>> {
        let text = match (&self.summary, self.summary_file.as_deref()) {
            (Some(s), _) => s.clone(),
            (None, Some(path)) => {
                use std::io::Read;
                let mut text = String::new();
                if path == "-" {
                    std::io::stdin().read_to_string(&mut text)
                } else {
                    std::fs::File::open(path).and_then(|mut f| f.read_to_string(&mut text))
                }
                .map_err(|e| {
                    CliError::err(
                        "summary_file_unreadable",
                        format!("cannot read the summary from {path}: {e}"),
                    )
                })?;
                text
            }
            (None, None) => return Ok(None),
        };
        if text.trim().is_empty() {
            return Err(CliError::err(
                "summary_empty",
                "the summary is empty; its first line names the outcome: done, partial, blocked or nothing to do",
            ));
        }
        Ok(Some(text))
    }

    /// Whether this ends `own`, the task the caller runs in: no task given,
    /// or that one.
    pub fn ends(&self, own: &str) -> bool {
        self.task
            .as_deref()
            .is_none_or(|t| parse_task_id(t).is_some_and(|id| parse_task_id(own) == Some(id)))
    }
}

#[derive(Args, Debug)]
#[command(group(ArgGroup::new("send_input").required(true).multiple(true)))]
pub struct SendArgs {
    /// A task, like t-12 or 12: a live one (starting, running or blocked)
    pub task: String,
    /// Text to type into the agent, followed by Enter
    #[arg(group = "send_input")]
    pub text: Option<String>,
    /// A named key to press after the text (Enter, Down, esc, ctrl+c); repeat for more, in order
    #[arg(long = "key", value_name = "KEY", group = "send_input")]
    pub keys: Vec<String>,
    /// Type the text without pressing Enter after it
    #[arg(long, requires = "text")]
    pub no_enter: bool,
    /// Accept the agent's folder-trust prompt with its trust keys, and trust the task's repo on its machine from now on
    #[arg(long, group = "send_input", conflicts_with_all = ["text", "keys", "no_enter"])]
    pub trust: bool,
    /// Print as a JSON object
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
    /// Print as a JSON object
    #[arg(long)]
    pub json: bool,
}

/// A level as `--priority` and `task priority` take it, or
/// `unknown_priority`.
pub fn parse_priority(s: &str) -> anyhow::Result<Priority> {
    s.parse()
        .map_err(|e: String| CliError::err(UNKNOWN_PRIORITY, e))
}

fn task_id(s: &str) -> anyhow::Result<i64> {
    parse_task_id(s).ok_or_else(|| CliError::err("usage_error", crate::task::bad_task_id(s)))
}

/// `pastor task retry t-N`: a new task copying t-N, dispatched now.
pub async fn retry(client: &Client, a: RetryArgs) -> anyhow::Result<()> {
    let id = task_id(&a.task)?;
    let req = IpcRequest::TaskRetry { id, place: a.place };
    match ask(client, req).await? {
        IpcResponse::Task(t) => print_task(&t, a.json),
        other => Err(unexpected(other)),
    }
}

/// `pastor task priority t-N LEVEL`: a queued task at another level.
pub async fn priority(client: &Client, a: PriorityArgs) -> anyhow::Result<()> {
    let id = task_id(&a.task)?;
    let priority = parse_priority(&a.level)?;
    match ask(
        client,
        IpcRequest::TaskPriority {
            id,
            priority,
            preempt: a.preempt,
        },
    )
    .await?
    {
        IpcResponse::Task(t) => print_task(&t, a.json),
        other => Err(unexpected(other)),
    }
}

/// `pastor task close t-N... [--remove-worktree]`. One task prints as it
/// always has; several print one line (or JSON object) each, and any that
/// failed make it `close_failed`.
pub async fn close(client: &Client, a: CloseArgs) -> anyhow::Result<()> {
    if let [task] = a.tasks.as_slice() {
        return match close_one(client, task, a.remove_worktree).await? {
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
        };
    }
    let mut results = Vec::new();
    let mut failed = Vec::new();
    for task in &a.tasks {
        let (line, json) = match close_one(client, task, a.remove_worktree).await {
            Ok(IpcResponse::Task(t)) => (
                format!("{} {}", t.display_id(), t.state),
                serde_json::json!({"task": t.display_id(), "state": t.state}),
            ),
            Ok(IpcResponse::Text(msg)) => (
                format!("{task} {msg}"),
                serde_json::json!({"task": task, "message": msg}),
            ),
            Ok(other) => return Err(unexpected(other)),
            Err(e) => {
                failed.push(task.as_str());
                let (code, message) = match e.downcast_ref::<CliError>() {
                    Some(e) => (e.code.clone(), e.message.clone()),
                    None => ("runtime_error".to_string(), format!("{e:#}")),
                };
                (
                    format!("{task} {code}: {message}"),
                    serde_json::json!({"task": task, "code": code, "message": message}),
                )
            }
        };
        if a.json {
            results.push(json);
        } else {
            println!("{line}");
        }
    }
    if a.json {
        println!("{}", serde_json::to_string_pretty(&results)?);
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(CliError::err(
            "close_failed",
            format!(
                "{} of {} not closed: {}",
                failed.len(),
                a.tasks.len(),
                failed.join(", ")
            ),
        ))
    }
}

async fn close_one(
    client: &Client,
    task: &str,
    remove_worktree: bool,
) -> anyhow::Result<IpcResponse> {
    let id = task_id(task)?;
    Ok(ask(
        client,
        IpcRequest::TaskClose {
            id,
            remove_worktree,
        },
    )
    .await?)
}

/// `pastor task done [t-N]`: the task given, or the one this pane runs.
pub async fn done(client: &Client, a: DoneArgs) -> anyhow::Result<()> {
    let task = match a.task.clone().or_else(|| client.caller.task.clone()) {
        Some(t) => t,
        None => {
            return Err(CliError::err(
                "usage_error",
                format!(
                    "name a task (t-12), or run this from a task's pane, where {} names it",
                    crate::ipc::TASK_ENV
                ),
            ));
        }
    };
    let id = task_id(&task)?;
    let summary = a.summary_text()?;
    match ask(client, IpcRequest::TaskDone { id, summary }).await? {
        IpcResponse::Task(t) => print_task(&t, a.json),
        other => Err(unexpected(other)),
    }
}

/// `pastor task send t-N [TEXT] [--key K]... [--no-enter] | --trust`.
pub async fn send(client: &Client, a: SendArgs) -> anyhow::Result<()> {
    let id = task_id(&a.task)?;
    let input = SendInput {
        enter: a.text.is_some() && !a.no_enter,
        text: a.text,
        keys: a.keys,
        trust: a.trust,
    };
    match ask(client, IpcRequest::TaskSend { id, input }).await? {
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
/// daemon when one runs; straight on the database, since it needs no
/// machine, only when nothing holds the socket. `head` is what the command's
/// one probe found: a head that holds the socket but does not answer may be
/// busy mid-request, with actors still writing tasks, so it stopped the
/// command before this (`head_unresponsive`) rather than let prune race it.
pub async fn prune(client: &Client, a: PruneArgs, head: Head) -> anyhow::Result<()> {
    let older_than = parse_duration(&a.older_than).map_err(|e| CliError::err("usage_error", e))?;
    let states = a.states();
    let out = match head {
        Head::Live => {
            let req = IpcRequest::TaskPrune {
                states,
                older_than_secs: older_than.as_secs(),
            };
            match ask(client, req).await? {
                IpcResponse::Pruned(out) => out,
                other => return Err(unexpected(other)),
            }
        }
        Head::Absent => {
            let paths = &client.paths;
            paths.ensure()?;
            Store::open(&paths.db_file())?.prune(&states, older_than)?
        }
    };
    if a.json {
        println!("{}", serde_json::to_string(&out)?);
    } else {
        print!("{}", prune_summary(&a, &out));
    }
    Ok(())
}

/// What `task prune` prints. The kept line says why and what to run, since
/// a row kept for its worktree stays kept until someone acts on it.
fn prune_summary(a: &PruneArgs, out: &PruneOutcome) -> String {
    let what: Vec<&str> = a.states().iter().map(TaskState::as_str).collect();
    let n = out.pruned;
    let mut s = format!(
        "pruned {n} {} task{} older than {}\n",
        what.join("/"),
        if n == 1 { "" } else { "s" },
        a.older_than
    );
    for id in &out.kept_worktrees {
        s.push_str(&format!(
            "kept t-{id}: its worktree may still be on disk; \
             `pastor task close t-{id} --remove-worktree` removes it or says how, then prune takes the row\n"
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::RequestError;
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
    fn prune_summary_names_each_kept_worktree_task() {
        let p = P::try_parse_from(["p", "--closed", "--older-than", "3d"]).unwrap();
        let out = PruneOutcome {
            pruned: 1,
            kept_worktrees: vec![4],
        };
        let s = prune_summary(&p.a, &out);
        assert!(s.starts_with("pruned 1 closed task older than 3d\n"), "{s}");
        assert!(s.contains("kept t-4"), "{s}");
        assert!(s.contains("pastor task close t-4 --remove-worktree"), "{s}");
        let none = prune_summary(&p.a, &PruneOutcome::default());
        assert!(!none.contains("kept"), "{none}");
    }

    /// A retry that timed out may still land, so telling the user pastor
    /// serve is not running would send them to run it again and queue a
    /// second task. Only a refused or missing socket means nothing is there.
    #[test]
    fn request_failures_keep_their_own_codes() {
        let code_and_message = |err: RequestError| {
            let e = crate::cli::request_error(&err);
            (e.code, e.message)
        };
        let (code, message) =
            code_and_message(RequestError::Timeout(std::time::Duration::from_secs(120)));
        assert_eq!(code, "timeout");
        assert!(message.contains("may still complete"), "{message}");
        assert!(!message.contains("not running"), "{message}");

        for kind in [
            std::io::ErrorKind::ConnectionRefused,
            std::io::ErrorKind::NotFound,
        ] {
            let (code, message) = code_and_message(RequestError::Connect(kind.into()));
            assert_eq!(code, "daemon_not_running", "{kind:?}");
            assert!(message.contains("start it with"), "{message}");
        }
        let (code, message) = code_and_message(RequestError::Connect(
            std::io::ErrorKind::PermissionDenied.into(),
        ));
        assert_eq!(code, "runtime_error");
        assert!(!message.contains("not running"), "{message}");
        let (code, message) =
            code_and_message(RequestError::Exchange(anyhow::anyhow!("closed early")));
        assert_eq!(code, "runtime_error");
        assert!(message.contains("dropped the request"), "{message}");
    }

    #[test]
    fn task_ids_are_usage_errors() {
        let err = task_id("x-1").unwrap_err();
        assert_eq!(err.downcast_ref::<CliError>().unwrap().code, "usage_error");
        assert_eq!(task_id("t-7").unwrap(), 7);
    }
}
