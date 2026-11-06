//! `pastor agent signed-in`: run an agent's login check on a machine
//! (`Agents::login_check_command`) and, when it passes, clear the mark a
//! login screen left there (`limit::signed_out_in`), so the tasks it held
//! start again. With a head running it goes through it
//! (`IpcRequest::AgentSignedIn`), which says so with `agent.signed_in`;
//! with none, the check runs from here and the mark leaves the store.
use clap::Subcommand;

use crate::cli::{CliError, request_error};
use crate::config::flock::{Flock, MachineConfig};
use crate::config::{Agents, PastorConfig};
use crate::harness_manifest::Harnesses;
use crate::herdr::transport::{Connector, Endpoint};
use crate::ipc::{Client, Head, IpcRequest, IpcResponse};
use crate::limit::AccountLimit;
use crate::store::Store;

/// The code of a login check that ran and failed: the agent is still
/// signed out there.
pub const SIGNED_OUT: &str = "signed_out";

#[derive(Subcommand, Debug)]
pub enum AgentCmd {
    /// Run an agent's login check on a machine; when it passes, clear the agent's signed-out mark there
    SignedIn {
        /// The agent, as tasks name it (`claude`, or an `[agents.<name>]`)
        agent: String,
        /// The machine to check it on
        #[arg(long)]
        machine: String,
    },
}

/// Whether `cmd` changes what the head does: a passing check clears a mark.
pub fn changes_fleet(_cmd: &AgentCmd) -> bool {
    true
}

pub async fn run(client: &Client, cmd: AgentCmd, head: Head) -> anyhow::Result<()> {
    let AgentCmd::SignedIn { agent, machine } = cmd;
    let gone = if head.is_live() {
        let req = IpcRequest::AgentSignedIn {
            machine: machine.clone(),
            agent: agent.clone(),
        };
        match crate::ipc::request_head(client, &req).await {
            Ok(IpcResponse::Limits(list)) => list,
            Ok(IpcResponse::Error { code, message }) => return Err(CliError::err(&code, message)),
            Ok(other) => anyhow::bail!("unexpected reply to agent signed-in: {other:?}"),
            Err(err) => return Err(request_error(&err).into()),
        }
    } else {
        let paths = &client.paths;
        let flock = Flock::load(&paths.flock_file())?;
        let Some(m) = flock.get(&machine) else {
            return Err(CliError::err(
                "unknown_machine",
                format!("no machine {machine} in the flock"),
            ));
        };
        let agents = PastorConfig::load(&paths.config_file())?.agents;
        let harnesses = Harnesses::load(paths)?;
        let ep = Endpoint::from_machine(m, paths);
        login_check(&ep, &agents, &harnesses, m, &agent).await?;
        paths.ensure()?;
        let store = Store::open(&paths.db_file())?;
        store.clear_signed_out(&agents.signed_out_key(&machine, &agent))?
    };
    print_signed_in(&agent, &machine, &gone);
    Ok(())
}

fn print_signed_in(agent: &str, machine: &str, gone: &[AccountLimit]) {
    if gone.is_empty() {
        println!("{agent} is signed in on {machine}; it had no signed-out mark");
    }
    for l in gone {
        println!("{agent} is signed in on {machine}; cleared {}", l.name());
    }
}

/// Run `agent`'s login check on machine `m` through `conn`
/// (`Agents::login_check_command`, with its kind's manifest from
/// `harnesses`): `Ok` when it passes, else why not, with a code
/// (`SIGNED_OUT` when it ran and failed).
pub async fn login_check(
    conn: &dyn Connector,
    agents: &Agents,
    harnesses: &Harnesses,
    m: &MachineConfig,
    agent: &str,
) -> Result<(), CliError> {
    let refuse = |code: &str, message: String| CliError {
        code: code.into(),
        message,
    };
    let machine = &m.name;
    if m.pull {
        return Err(refuse(
            "pull_machine",
            format!("{machine} is a pull machine: the head never connects to it"),
        ));
    }
    let manifest = harnesses.get(agents.kind(agent));
    let Some(command) = agents.login_check_command(agent, manifest) else {
        return Err(refuse(
            "no_login_check",
            format!("agent {agent} has no login_check; set one under [agents.{agent}]"),
        ));
    };
    match conn.command_succeeds(&command).await {
        Ok(Some(true)) => Ok(()),
        Ok(Some(false)) => Err(refuse(
            SIGNED_OUT,
            format!("{agent} is not signed in on {machine}"),
        )),
        Ok(None) => Err(refuse(
            "check_unknown",
            format!("could not run the login check of {agent} on {machine}"),
        )),
        Err(err) => Err(refuse(
            "machine_unreachable",
            format!("{machine}: {}", err.message),
        )),
    }
}
