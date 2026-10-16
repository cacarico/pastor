//! `pastor bootstrap`: the setup steps run on a machine itself, as the
//! login user. Push runs it over ssh and pull runs it from `pastor join`;
//! both read the one JSON [`Outcome`] it prints.
//!
//! Every command that changes the machine (useradd, loginctl, systemctl,
//! ssh-keygen, the copies) goes through the [`Runner`], so the tests record
//! them instead of touching the host.

pub mod facts;
pub mod outcome;
mod steps;

use std::path::PathBuf;

pub use facts::{Facts, Init, Pkg, facts, target_for};
pub use outcome::{Isolation, Mode, Outcome, StepResult, StepStatus};

use crate::cli::CliError;
use crate::setup::Runner;

/// The user herdr and every agent pane run as, in root mode.
pub const AGENT_USER: &str = "pastor-agent";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Use `sudo -n` for root work when not uid 0.
    pub sudo: bool,
    /// The user that holds the machine key; the login user when unset.
    pub service_user: Option<String>,
    /// The head's ssh destination, written to each user's client.toml.
    pub head: String,
    /// The head's public key, for the agent user's authorized_keys.
    pub head_key: Option<String>,
    /// A herdr binary to install; without one, herdr must be there already.
    pub herdr_from: Option<PathBuf>,
    /// The pastor binary to install for each user.
    pub pastor_from: PathBuf,
}

/// Run the steps in order, stopping at the first that fails.
pub fn run(r: &dyn Runner, o: &Options) -> Outcome {
    let facts = facts::facts(r);
    // Root work only when asked for or already root: an ssh user with sudo
    // still gets user mode unless --sudo says otherwise.
    let mode = if facts.uid == 0 || o.sudo {
        Mode::Root
    } else {
        Mode::User
    };
    let login = facts::stdout(r, "id", &["-un"]).unwrap_or_default();
    let user_bus = mode == Mode::Root
        || facts.os != "linux"
        || facts::stdout(r, "printenv", &["XDG_RUNTIME_DIR"]).is_some_and(|d| !d.is_empty());
    let ctx = steps::Ctx {
        o,
        facts,
        mode,
        login,
        user_bus,
        found: Default::default(),
        known: Default::default(),
        stage: Default::default(),
        staged: Default::default(),
    };
    let mut results = Vec::new();
    for (name, step) in steps::STEPS {
        let mut result = step(r, &ctx);
        result.step = name.to_string();
        let failed = result.status == StepStatus::Failed;
        results.push(result);
        if failed {
            break;
        }
    }
    ctx.unstage(r);
    let found = ctx.found.into_inner();
    Outcome {
        ok: results.iter().all(|s| s.status != StepStatus::Failed),
        mode,
        isolation: match mode {
            Mode::Root => Isolation::Users,
            Mode::User => Isolation::None,
        },
        steps: results,
        machine_key: found.machine_key,
        agent_key: found.agent_key,
        host_key: found.host_key,
        herdr_version: found.herdr_version,
        pastor_version: found.pastor_version,
    }
}

/// The CLI error for a failed outcome, `None` when it is ok.
pub fn error(outcome: &Outcome) -> Option<anyhow::Error> {
    let failed = outcome
        .steps
        .iter()
        .find(|s| s.status == StepStatus::Failed)?;
    let mut message = format!(
        "bootstrap step {} failed: {}",
        failed.step,
        failed.why.as_deref().unwrap_or("no reason given")
    );
    if let Some(fix) = &failed.fix {
        message.push_str("; ");
        message.push_str(fix);
    }
    // The facts step fails only on --sudo without sudo.
    let code = match failed.step.as_str() {
        "facts" => "bootstrap_needs_sudo",
        "platform" => "bootstrap_unsupported_platform",
        _ if !failed.missing.is_empty() => "bootstrap_missing",
        _ => "runtime_error",
    };
    Some(CliError::err(code, message))
}

/// `pastor bootstrap`. Hidden from the short help: push and pull run it,
/// not people.
#[derive(clap::Args, Debug)]
pub struct Args {
    /// Do root work (packages, users, lingering) through `sudo -n`
    #[arg(long)]
    pub sudo: bool,
    /// The user that holds the machine key and runs the shepherd; created if missing (root mode)
    #[arg(long, value_name = "NAME")]
    pub service_user: Option<String>,
    /// The head's public key, for the agent user's authorized_keys
    #[arg(long, value_name = "PUBKEY")]
    pub head_key: Option<String>,
    /// A herdr binary to install for the agent user
    #[arg(long, value_name = "PATH")]
    pub herdr_from: Option<PathBuf>,
    /// Print as a JSON object: mode, isolation, each step, the public keys and versions
    #[arg(long)]
    pub json: bool,
    /// Write client.toml's [head] for this user and print `written` or `unchanged`; the client step runs it as each user
    #[arg(
        long,
        value_name = "DEST",
        hide = true,
        conflicts_with_all = ["sudo", "service_user", "head_key", "herdr_from", "json"]
    )]
    pub write_client: Option<String>,
    /// Print this user's pastor state dir; the steps run it as each user to find where its keys go
    #[arg(
        long,
        hide = true,
        conflicts_with_all = ["sudo", "service_user", "head_key", "herdr_from", "json", "write_client"]
    )]
    pub print_state_dir: bool,
}

/// Entry point for `pastor bootstrap`; `head` is the global `--head`.
pub fn cli(args: &Args, head: Option<&str>) -> anyhow::Result<()> {
    if let Some(dest) = &args.write_client {
        let paths = crate::config::Paths::from_env()?;
        let wrote = write_client(&crate::head::client_file(&paths), dest)?;
        println!("{}", if wrote { "written" } else { "unchanged" });
        return Ok(());
    }
    if args.print_state_dir {
        let paths = crate::config::Paths::from_env()?;
        println!("{}", paths.state_dir.display());
        return Ok(());
    }
    if let Some(user) = &args.service_user {
        check_user(user)?;
    }
    let Some(head) = head.filter(|h| !h.trim().is_empty()) else {
        return Err(CliError::err(
            "usage_error",
            "bootstrap needs --head <DEST>, the head's ssh destination",
        ));
    };
    check_dest(head)?;
    let o = Options {
        sudo: args.sudo,
        service_user: args.service_user.clone(),
        head: head.to_string(),
        head_key: args.head_key.clone(),
        herdr_from: args.herdr_from.clone(),
        pastor_from: std::env::current_exe()?,
    };
    let outcome = run(&crate::setup::SystemRunner, &o);
    if args.json {
        println!("{}", serde_json::to_string(&outcome)?);
    } else {
        print!("{}", human(&outcome));
    }
    match error(&outcome) {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// A destination ssh reads as an option, or that cannot go in a TOML
/// string on one line, is refused.
fn check_dest(dest: &str) -> anyhow::Result<()> {
    if dest.starts_with('-') || dest.chars().any(char::is_whitespace) {
        return Err(CliError::err(
            "usage_error",
            format!("{dest:?} is not an ssh destination"),
        ));
    }
    Ok(())
}

/// A user name as useradd takes it (`^[a-z_][a-z0-9_-]*$`, at most 32
/// characters), so none reads as an option or splits into two words.
fn check_user(name: &str) -> anyhow::Result<()> {
    let mut chars = name.chars();
    let ok = name.len() <= 32
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if !ok {
        return Err(CliError::err(
            "invalid_user",
            format!(
                "--service-user {name:?} is not a user name (lowercase letters, digits, _ and -, at most 32)"
            ),
        ));
    }
    Ok(())
}

/// Set client.toml's `[head]` to `dest`, unless it names it already.
/// Returns whether it wrote.
fn write_client(file: &std::path::Path, dest: &str) -> anyhow::Result<bool> {
    check_dest(dest)?;
    if crate::head::load(file)?.is_some_and(|h| h.ssh == dest) {
        return Ok(false);
    }
    let setting = crate::head::HeadSetting {
        ssh: dest.to_string(),
        pastor: None,
    };
    crate::head::save(file, Some(&setting))?;
    Ok(true)
}

/// One line per step, then what user mode costs.
fn human(o: &Outcome) -> String {
    let mut out = String::new();
    for s in &o.steps {
        let status = match s.status {
            StepStatus::Done => "done",
            StepStatus::AlreadyThere => "already there",
            StepStatus::Failed => "failed",
            StepStatus::Skipped => "skipped",
        };
        out.push_str(&format!("{}: {status}", s.step));
        if let Some(why) = &s.why {
            out.push_str(&format!(" ({why})"));
        }
        out.push('\n');
        if let Some(fix) = &s.fix {
            out.push_str(&format!("  fix: {fix}\n"));
        }
    }
    if o.isolation == Isolation::None {
        out.push_str("isolation: none; agents run as this user and can read the machine key\n");
    }
    out
}

#[cfg(test)]
mod tests;
