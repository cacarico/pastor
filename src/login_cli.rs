//! `pastor login MACHINE AGENT`, `login list|code|cancel`: a guided sign-in
//! of an agent on a machine, which the head runs and watches
//! (`login::start`, `IpcRequest::LoginStart`). It needs a head: the head
//! keeps watching the pane after the command returns.
use std::io::BufRead;

use clap::{Args, Subcommand};

use crate::cli::{CliError, age, request_error, table};
use crate::ipc::{Client, Head, IpcRequest, IpcResponse};
use crate::login::{PendingLogin, Started};

#[derive(Args, Debug)]
#[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
pub struct LoginArgs {
    /// The machine to sign the agent in on
    #[arg(required = true)]
    pub machine: Option<String>,
    /// The agent, as tasks name it (`claude`, or an `[agents.<name>]`)
    #[arg(required = true)]
    pub agent: Option<String>,
    /// Print as a JSON object: `pending`, the login started (machine, agent, pane, url, code, paste, started; null when the agent was signed in already), and `cleared`, the marks that cleared
    #[arg(long)]
    pub json: bool,
    #[command(subcommand)]
    pub cmd: Option<LoginCmd>,
}

#[derive(Subcommand, Debug)]
pub enum LoginCmd {
    /// Every login the head is watching: machine, agent, the URL and code to sign in with
    List {
        /// Print as a JSON array
        #[arg(long)]
        json: bool,
    },
    /// Type the code the browser gave back into a pending login's pane, then Enter
    Code {
        /// The machine of the pending login
        machine: String,
        /// The agent of the pending login
        agent: String,
        /// The code; read from stdin when left out, so it stays out of the shell's history
        code: Option<String>,
    },
    /// Close a pending login's pane; the agent stays signed out
    Cancel {
        /// The machine of the pending login
        machine: String,
        /// The agent of the pending login
        agent: String,
    },
}

/// Whether `args` changes what the head does: all but a list. A task's
/// agent never signs itself in.
pub fn changes_fleet(args: &LoginArgs) -> bool {
    !matches!(args.cmd, Some(LoginCmd::List { .. }))
}

pub async fn run(client: &Client, args: LoginArgs, head: Head) -> anyhow::Result<()> {
    if !head.is_live() {
        return Err(CliError::err(
            "no_head",
            "pastor login runs on the head, which watches the login, and pastor serve is not running",
        ));
    }
    let req = match &args.cmd {
        None => IpcRequest::LoginStart {
            machine: args.machine.clone().unwrap_or_default(),
            agent: args.agent.clone().unwrap_or_default(),
        },
        Some(LoginCmd::List { .. }) => IpcRequest::LoginList,
        Some(LoginCmd::Code {
            machine,
            agent,
            code,
        }) => IpcRequest::LoginCode {
            machine: machine.clone(),
            agent: agent.clone(),
            code: match code {
                Some(code) => code.clone(),
                None => read_code()?,
            },
        },
        Some(LoginCmd::Cancel { machine, agent }) => IpcRequest::LoginCancel {
            machine: machine.clone(),
            agent: agent.clone(),
        },
    };
    let reply = match crate::ipc::request_head(client, &req).await {
        Ok(IpcResponse::Error { code, message }) => return Err(CliError::err(&code, message)),
        Ok(reply) => reply,
        Err(err) => return Err(request_error(&err).into()),
    };
    match (args.cmd, reply) {
        (None, IpcResponse::Login(started)) => print_started(&started, args.json),
        (Some(LoginCmd::List { json }), IpcResponse::Logins(list)) => print_list(&list, json),
        (Some(LoginCmd::Code { .. }), IpcResponse::Logins(list)) => {
            for l in list {
                println!("typed the code into {}'s login on {}", l.agent, l.machine);
            }
            Ok(())
        }
        (Some(LoginCmd::Cancel { .. }), IpcResponse::Logins(list)) => {
            for l in list {
                println!("closed {}'s login on {}", l.agent, l.machine);
            }
            Ok(())
        }
        (_, other) => anyhow::bail!("unexpected reply to a login request: {other:?}"),
    }
}

/// One line from stdin, for `login code` with no code given.
fn read_code() -> anyhow::Result<String> {
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

fn print_started(started: &Started, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(started)?);
        return Ok(());
    }
    let Some(l) = &started.pending else {
        let cleared = started.cleared.iter().map(|l| l.name()).collect::<Vec<_>>();
        if cleared.is_empty() {
            println!("already signed in; nothing to do");
        } else {
            println!("already signed in; cleared {}", cleared.join(", "));
        }
        return Ok(());
    };
    println!("signing {} in on {} in pane {}", l.agent, l.machine, l.pane);
    match (&l.url, &l.code) {
        (Some(url), Some(code)) => println!("open {url}\nand enter the code {code}"),
        (Some(url), None) => println!("open {url}"),
        (None, Some(code)) => println!("enter the code {code}"),
        (None, None) => {
            println!("the pane shows no URL yet; see pastor login list, or attach to the pane")
        }
    }
    if l.paste {
        println!(
            "then give the code the browser shows: pastor login code {} {} <code>",
            l.machine, l.agent
        );
    }
    println!("the head clears the signed-out mark once the agent's login check passes");
    Ok(())
}

fn print_list(list: &[PendingLogin], json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(list)?);
        return Ok(());
    }
    if list.is_empty() {
        println!("no login is pending");
        return Ok(());
    }
    let rows: Vec<Vec<String>> = list
        .iter()
        .map(|l| {
            vec![
                l.machine.clone(),
                l.agent.clone(),
                l.pane.clone(),
                age(l.started),
                l.code.clone().unwrap_or_else(|| "-".into()),
                if l.paste { "yes" } else { "no" }.into(),
                l.url.clone().unwrap_or_else(|| "-".into()),
            ]
        })
        .collect();
    println!(
        "{}",
        table(
            &[
                "MACHINE", "AGENT", "PANE", "STARTED", "CODE", "PASTE", "URL"
            ],
            &rows
        )
    );
    Ok(())
}
