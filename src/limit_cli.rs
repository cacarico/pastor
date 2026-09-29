//! `pastor limit list|clear`: the accounts the head knows to be out of
//! usage (`Store::limits`), which no new task starts on before their
//! `retry_at`. With a head running they go through it
//! (`IpcRequest::LimitList`, `LimitClear`), a remote one included; with
//! none, straight to the store. There is no `limit set`: a row comes from
//! what an agent said, not from a person.
use clap::Subcommand;

use crate::cli::{CliError, age, request_error, table};
use crate::config::Paths;
use crate::ipc::{Head, IpcRequest, IpcResponse};
use crate::limit::{AccountLimit, local_time};
use crate::store::Store;

/// The code of a clear that found nothing to clear.
pub const NOT_EXHAUSTED: &str = "not_exhausted";

/// The message of `NOT_EXHAUSTED`.
pub fn not_exhausted(account: &str, model: Option<&str>) -> String {
    match model {
        Some(m) => format!("{account} has no limit for model {m}"),
        None => format!("{account} has no limit"),
    }
}

#[derive(Subcommand, Debug)]
pub enum LimitCmd {
    /// Every exhausted account: its model, when it is tried again, what ran out, who saw it
    List {
        /// Print as a JSON array
        #[arg(long)]
        json: bool,
    },
    /// Forget an account's limit, so queued tasks start on it on the next pass
    Clear {
        /// The account, as `limit list` shows it: an agent's `account`, or `<machine>/<agent>`
        account: String,
        /// Clear only this model's limit, not the whole account's
        #[arg(long)]
        model: Option<String>,
    },
}

/// Whether `cmd` changes what the head does: a clear.
pub fn changes_fleet(cmd: &LimitCmd) -> bool {
    !matches!(cmd, LimitCmd::List { .. })
}

pub async fn run(paths: &Paths, cmd: LimitCmd, head: Head) -> anyhow::Result<()> {
    if head.is_live() {
        return run_on_head(paths, cmd).await;
    }
    paths.ensure()?;
    let store = Store::open(&paths.db_file())?;
    match cmd {
        LimitCmd::List { json } => print_list(&store.limits()?, json),
        LimitCmd::Clear { account, model } => {
            let gone = store.clear_limits(&account, model.as_deref())?;
            print_cleared(&account, model.as_deref(), &gone)
        }
    }
}

async fn run_on_head(paths: &Paths, cmd: LimitCmd) -> anyhow::Result<()> {
    let (req, json) = match &cmd {
        LimitCmd::List { json } => (IpcRequest::LimitList, *json),
        LimitCmd::Clear { account, model } => (
            IpcRequest::LimitClear {
                account: account.clone(),
                model: model.clone(),
            },
            false,
        ),
    };
    match crate::ipc::request_head(paths, &req).await {
        Ok(IpcResponse::Limits(list)) => match cmd {
            LimitCmd::List { .. } => print_list(&list, json),
            LimitCmd::Clear { account, model } => print_cleared(&account, model.as_deref(), &list),
        },
        Ok(IpcResponse::Error { code, message }) => Err(CliError::err(&code, message)),
        Ok(other) => anyhow::bail!("unexpected reply to a limit request: {other:?}"),
        Err(err) => Err(request_error(&err).into()),
    }
}

fn print_cleared(account: &str, model: Option<&str>, gone: &[AccountLimit]) -> anyhow::Result<()> {
    if gone.is_empty() {
        return Err(CliError::err(NOT_EXHAUSTED, not_exhausted(account, model)));
    }
    for l in gone {
        println!("cleared {}", l.name());
    }
    Ok(())
}

fn print_list(list: &[AccountLimit], json: bool) -> anyhow::Result<()> {
    let now = chrono::Utc::now();
    if json {
        println!("{}", serde_json::to_string_pretty(list)?);
    } else if list.is_empty() {
        println!("no account is exhausted");
    } else {
        let rows: Vec<Vec<String>> = list
            .iter()
            .map(|l| {
                let seen = match (l.task_id, &l.machine) {
                    (Some(id), Some(m)) => format!("t-{id} on {m}"),
                    (Some(id), None) => format!("t-{id}"),
                    (None, Some(m)) => m.clone(),
                    (None, None) => "-".into(),
                };
                vec![
                    l.account.clone(),
                    l.model.clone().unwrap_or_else(|| "-".into()),
                    local_time(l.retry_at, now),
                    l.what(),
                    seen,
                    age(l.seen_at),
                ]
            })
            .collect();
        println!(
            "{}",
            table(
                &["ACCOUNT", "MODEL", "UNTIL", "WHAT", "SEEN BY", "SEEN"],
                &rows
            )
        );
    }
    Ok(())
}
