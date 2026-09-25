//! `pastor task retry|close|prune|send`: argument types and handlers. `main.rs` only
//! holds one `TaskCmd` variant per command and calls these.
use clap::{ArgGroup, Args};

use crate::cli::{CliError, TASK_HEADER, request_failure, table, task_rows};
use crate::config::{Paths, parse_duration};
use crate::ipc::{
    DaemonProbe, IpcRequest, IpcResponse, RequestError, connect_error_means_no_daemon,
    probe_daemon, request,
};
use crate::machine::SendInput;
use crate::store::{PruneOutcome, Store};
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
#[command(group(ArgGroup::new("send_input").required(true).multiple(true)))]
pub struct SendArgs {
    /// A live task (starting, running or blocked), like t-12
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

/// A request that got no reply, classified as `request_failure` does for
/// the rest of the CLI. These commands answer `daemon_not_running` where
/// that says `runtime_error`, and only for a refused or missing socket: a
/// timed-out retry may still land, and a connect denied for permissions may
/// hide a live head.
fn request_error(err: &RequestError) -> anyhow::Error {
    let (code, message) = request_failure(err);
    let code = match err {
        RequestError::Connect(e) if connect_error_means_no_daemon(e) => "daemon_not_running",
        _ => code,
    };
    CliError::err(code, message)
}

/// One request to the daemon. Retry and close need it: one dispatches, the
/// other talks to herdr on the task's machine.
async fn ask(paths: &Paths, req: IpcRequest) -> anyhow::Result<IpcResponse> {
    let resp = request(&paths.socket_file(), &req)
        .await
        .map_err(|e| request_error(&e))?;
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

/// `pastor task send t-N [TEXT] [--key K]... [--no-enter] | --trust`.
pub async fn send(paths: &Paths, a: SendArgs) -> anyhow::Result<()> {
    let id = task_id(&a.task)?;
    let input = SendInput {
        enter: a.text.is_some() && !a.no_enter,
        text: a.text,
        keys: a.keys,
        trust: a.trust,
    };
    match ask(paths, IpcRequest::TaskSend { id, input }).await? {
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
/// machine, only when nothing holds the socket. A head that holds it but
/// does not answer may be busy mid-request, with actors still writing tasks,
/// so prune refuses rather than race it.
pub async fn prune(paths: &Paths, a: PruneArgs) -> anyhow::Result<()> {
    let older_than = parse_duration(&a.older_than).map_err(|e| CliError::err("usage_error", e))?;
    let states = a.states();
    let socket = paths.socket_file();
    let out = match probe_daemon(&socket).await {
        DaemonProbe::Running => {
            let req = IpcRequest::TaskPrune {
                states,
                older_than_secs: older_than.as_secs(),
            };
            match ask(paths, req).await? {
                IpcResponse::Pruned(out) => out,
                other => return Err(unexpected(other)),
            }
        }
        DaemonProbe::NotRunning => {
            paths.ensure()?;
            Store::open(&paths.db_file())?.prune(&states, older_than)?
        }
        DaemonProbe::Unresponsive => {
            return Err(CliError::err(
                "daemon_unresponsive",
                format!(
                    "pastor serve is not answering on {}; nothing was pruned, run it again once it answers",
                    socket.display()
                ),
            ));
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
            let err = request_error(&err);
            let e = err.downcast_ref::<CliError>().unwrap();
            (e.code.clone(), e.message.clone())
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

    /// A head that holds the socket but does not answer may still be running
    /// machine actors and lifecycle requests, so prune must not write the
    /// database behind its back: only a head that is not there at all lets
    /// it go offline.
    #[tokio::test]
    async fn prune_does_not_write_offline_behind_an_unresponsive_head() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        std::fs::create_dir_all(tmp.path().join("s")).unwrap();
        // Connects land in the backlog and are never answered.
        let _listener = std::os::unix::net::UnixListener::bind(paths.socket_file()).unwrap();
        let a = P::try_parse_from(["p", "--done", "--older-than", "3d"])
            .unwrap()
            .a;
        let err = prune(&paths, a).await.unwrap_err();
        let e = err.downcast_ref::<CliError>().expect("a CliError");
        assert_eq!(e.code, "daemon_unresponsive");
        assert!(e.message.contains("not answering"), "{}", e.message);
        assert!(!paths.db_file().exists(), "nothing written offline");
    }

    #[test]
    fn task_ids_are_usage_errors() {
        let err = task_id("x-1").unwrap_err();
        assert_eq!(err.downcast_ref::<CliError>().unwrap().code, "usage_error");
        assert_eq!(task_id("t-7").unwrap(), 7);
    }
}
