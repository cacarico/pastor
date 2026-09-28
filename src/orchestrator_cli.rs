//! `pastor orchestrator list|describe|run|enable|disable|note`. Orchestrators
//! run on the head, so with one running every command goes through it
//! (`IpcRequest::Orchestrator*`). With none, `list` and `describe` read the
//! files and the state the head left, `enable`, `disable` and a person's
//! `note --name` edit them, and `run` is refused: nothing would run it.
use std::io::Read;

use clap::Subcommand;

use crate::cli::{CliError, request_failure};
use crate::config::Paths;
use crate::ipc::{Head, IpcRequest, IpcResponse};
use crate::orchestrator::{OrchestratorDescription, OrchestratorStatus};
use crate::store::Store;

#[derive(Subcommand, Debug)]
pub enum OrchestratorCmd {
    /// Every orchestrator file: kind, state, schedule, last and next run, its last agent
    List {
        /// Add a DESCRIPTION column, cut to the terminal's width
        #[arg(short, long)]
        wide: bool,
        /// Print as a JSON array
        #[arg(long)]
        json: bool,
    },
    /// One orchestrator in full: its settings, note, last runs with the lines each pre script printed, and recent events
    Describe {
        /// The orchestrator: its file name without .toml
        name: String,
        /// Print as a JSON object
        #[arg(long)]
        json: bool,
    },
    /// Run a scheduled orchestrator now, ignoring its schedule and `enabled`; it starts once a run already going has finished, and is skipped while its last agent works
    Run {
        /// The orchestrator: its file name without .toml
        name: String,
    },
    /// Enable an orchestrator file
    Enable {
        /// The orchestrator: its file name without .toml
        name: String,
    },
    /// Disable an orchestrator file; an agent it started keeps running
    Disable {
        /// The orchestrator: its file name without .toml
        name: String,
    },
    /// Keep the orchestrator's handover note (4 KiB at most), which every agent it starts gets in its prompt; the last one wins, and an empty one removes it
    Note {
        /// The note; `-` reads it from stdin
        text: String,
        /// The orchestrator, for a person; its own agent and scripts may leave it out
        #[arg(long)]
        name: Option<String>,
    },
}

/// Whether `cmd` changes what the head does: anything but a read.
pub fn changes_fleet(cmd: &OrchestratorCmd) -> bool {
    !matches!(
        cmd,
        OrchestratorCmd::List { .. } | OrchestratorCmd::Describe { .. }
    )
}

/// Whether an orchestrator's agent or script may send `cmd`
/// (`IpcRequest::orchestrator_may`): its note, and only its own, which the
/// head checks.
pub fn orchestrator_may(cmd: &OrchestratorCmd) -> bool {
    matches!(cmd, OrchestratorCmd::Note { .. })
}

pub async fn run(paths: &Paths, cmd: OrchestratorCmd, head: Head) -> anyhow::Result<()> {
    let cmd = match cmd {
        OrchestratorCmd::Note { text, name } if text == "-" => {
            let mut text = String::new();
            std::io::stdin().read_to_string(&mut text)?;
            OrchestratorCmd::Note { text, name }
        }
        other => other,
    };
    if head.is_live() {
        return on_head(paths, cmd).await;
    }
    let store = Store::open_read_only(&paths.db_file()).ok();
    match cmd {
        OrchestratorCmd::List { wide, json } => {
            eprintln!("pastor serve is not running; showing the files and the last known state");
            print_list(
                &crate::orchestrator::offline_statuses(paths, store.as_ref())?,
                wide,
                json,
            )
        }
        OrchestratorCmd::Describe { name, json } => {
            let d = crate::orchestrator::offline_describe(paths, store.as_ref(), &name)
                .map_err(|(code, message)| CliError::err(&code, message))?;
            print_description(&d, json)
        }
        OrchestratorCmd::Run { name } => Err(CliError::err(
            "no_head",
            format!(
                "orchestrators run on the head, and pastor serve is not running; start it to run {name}"
            ),
        )),
        OrchestratorCmd::Enable { name } => toggle(paths, &name, true),
        OrchestratorCmd::Disable { name } => toggle(paths, &name, false),
        OrchestratorCmd::Note { text, name } => {
            // Whose note an agent or a script keeps only the head knows.
            let Some(name) = name.filter(|_| {
                crate::ipc::caller_task().is_none() && crate::ipc::caller().orchestrator.is_none()
            }) else {
                return Err(CliError::err(
                    "no_head",
                    "pastor serve is not running; with no head, a person names the orchestrator with --name",
                ));
            };
            crate::orchestrator::file_of(paths, &name)
                .map_err(|(code, message)| CliError::err(&code, message))?;
            println!("{}", crate::orchestrator::write_note(paths, &name, &text)?);
            Ok(())
        }
    }
}

fn toggle(paths: &Paths, name: &str, enabled: bool) -> anyhow::Result<()> {
    let said = crate::orchestrator::set_enabled(paths, name, enabled)
        .map_err(|(code, message)| CliError::err(&code, message))?;
    println!("{said}");
    Ok(())
}

async fn on_head(paths: &Paths, cmd: OrchestratorCmd) -> anyhow::Result<()> {
    let (req, wide, json) = match cmd {
        OrchestratorCmd::List { wide, json } => (IpcRequest::OrchestratorList, wide, json),
        OrchestratorCmd::Describe { name, json } => {
            (IpcRequest::OrchestratorDescribe { name }, false, json)
        }
        OrchestratorCmd::Run { name } => (IpcRequest::OrchestratorRun { name }, false, false),
        OrchestratorCmd::Enable { name } => (
            IpcRequest::OrchestratorSetEnabled {
                name,
                enabled: true,
            },
            false,
            false,
        ),
        OrchestratorCmd::Disable { name } => (
            IpcRequest::OrchestratorSetEnabled {
                name,
                enabled: false,
            },
            false,
            false,
        ),
        OrchestratorCmd::Note { text, name } => {
            (IpcRequest::OrchestratorNote { name, text }, false, false)
        }
    };
    match crate::ipc::request_head(paths, &req).await {
        Ok(IpcResponse::Orchestrators(list)) => print_list(&list, wide, json),
        Ok(IpcResponse::Orchestrator(d)) => print_description(&d, json),
        Ok(IpcResponse::Text(text)) => {
            println!("{text}");
            Ok(())
        }
        Ok(IpcResponse::Error { code, message }) => Err(CliError::err(&code, message)),
        Ok(other) => anyhow::bail!("unexpected reply to an orchestrator request: {other:?}"),
        Err(err) => {
            let (code, message) = request_failure(&err);
            Err(CliError::err(&code, message))
        }
    }
}

fn print_list(list: &[OrchestratorStatus], wide: bool, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(list)?);
    } else if list.is_empty() {
        println!("no orchestrators; add a file under orchestrators/ in pastor's config dir");
    } else {
        let descriptions: Vec<Option<String>> =
            list.iter().map(|o| o.description.clone()).collect();
        println!(
            "{}",
            crate::cli::list_table(
                &crate::cli::ORCHESTRATOR_HEADER,
                &crate::cli::orchestrator_rows(list),
                wide,
                &descriptions,
            )
        );
    }
    Ok(())
}

fn print_description(d: &OrchestratorDescription, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(d)?);
    } else {
        println!("{}", crate::describe::orchestrator_text(d));
    }
    Ok(())
}
