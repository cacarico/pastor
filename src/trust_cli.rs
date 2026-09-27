//! `pastor trust list|add|remove`: the (machine, repo) pairs whose
//! folder-trust prompt the head answers on its own. With a head running they
//! go through it (`IpcRequest::TrustList`, `TrustAdd`, `TrustRemove`), so a
//! CLI elsewhere sees the head's table; with none, straight to the store. The
//! head reads the table afresh each time a task blocks, so a change applies
//! at once.
use clap::Subcommand;

use crate::cli::{CliError, age, request_failure, table};
use crate::config::Paths;
use crate::ipc::{Head, IpcRequest, IpcResponse};
use crate::store::Store;

#[derive(Subcommand, Debug)]
pub enum TrustCmd {
    /// Every saved trust: machine, repo, and when it was saved
    List {
        #[arg(long)]
        json: bool,
    },
    /// Save a trust, so the repo's tasks on that machine are answered
    Add { machine: String, repo: String },
    /// Forget a saved trust; the repo's next task asks again
    Remove { machine: String, repo: String },
}

/// Whether `cmd` changes what the head does: an add or a remove.
pub fn changes_fleet(cmd: &TrustCmd) -> bool {
    !matches!(cmd, TrustCmd::List { .. })
}

pub async fn run(paths: &Paths, cmd: TrustCmd, head: Head) -> anyhow::Result<()> {
    if head.is_live() {
        return run_on_head(paths, cmd).await;
    }
    paths.ensure()?;
    let store = Store::open(&paths.db_file())?;
    match cmd {
        TrustCmd::List { json } => print_list(&store.trusted_repos()?, json),
        TrustCmd::Add { machine, repo } => {
            if store.trust_repo(&machine, &repo)? {
                println!("{repo} on {machine} is trusted");
            } else {
                println!("{repo} on {machine} was already trusted");
            }
            Ok(())
        }
        TrustCmd::Remove { machine, repo } => {
            if !store.untrust(&machine, &repo)? {
                return Err(CliError::err(
                    "not_trusted",
                    format!("{repo} on {machine} is not trusted"),
                ));
            }
            println!("{repo} on {machine} is no longer trusted");
            Ok(())
        }
    }
}

async fn run_on_head(paths: &Paths, cmd: TrustCmd) -> anyhow::Result<()> {
    let (req, json) = match cmd {
        TrustCmd::List { json } => (IpcRequest::TrustList, json),
        TrustCmd::Add { machine, repo } => (IpcRequest::TrustAdd { machine, repo }, false),
        TrustCmd::Remove { machine, repo } => (IpcRequest::TrustRemove { machine, repo }, false),
    };
    match crate::ipc::request(&paths.socket_file(), &req).await {
        Ok(IpcResponse::Trusted(list)) => print_list(&list, json),
        Ok(IpcResponse::Text(text)) => {
            println!("{text}");
            Ok(())
        }
        Ok(IpcResponse::Error { code, message }) => Err(CliError::err(&code, message)),
        Ok(other) => anyhow::bail!("unexpected reply to a trust request: {other:?}"),
        Err(err) => {
            let (code, message) = request_failure(&err);
            Err(CliError::err(code, message))
        }
    }
}

fn print_list(list: &[crate::store::TrustedRepo], json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(list)?);
    } else if list.is_empty() {
        println!("no trusted repos; `pastor task send <task> --trust` saves one");
    } else {
        let rows: Vec<Vec<String>> = list
            .iter()
            .map(|t| vec![t.machine.clone(), t.repo.clone(), age(t.trusted_at)])
            .collect();
        println!("{}", table(&["MACHINE", "REPO", "SAVED"], &rows));
    }
    Ok(())
}
