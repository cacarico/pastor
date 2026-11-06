//! Bootstrapping a machine: `pastor bootstrap` runs the setup steps on the
//! machine itself, as the login user (`steps`), and prints what it found as
//! one `Outcome`. The head keeps that report as the machine's state
//! (`Store::put_machine_state`), shows it in `machine describe` and reads it
//! before it places a task there.
//!
//! Every command a step runs goes through the `Runner`, and what needs
//! pastor's own machinery on the machine (herdr and the harnesses at their
//! pins, `machine check`) through `Local`, so the tests record and script
//! both instead of touching the host. No step calls sudo or installs an OS
//! package: what the machine lacks is reported, for its owner to add.

mod facts;
mod outcome;
pub mod pull;
pub mod push;
mod steps;

use std::path::PathBuf;
use std::sync::Arc;

pub use facts::{Facts, Init, facts};
pub use outcome::{Isolation, Outcome, StepResult, StepStatus};
pub use steps::{PULL_STEPS, STEPS};

use crate::cli::CliError;
use crate::config::Paths;
use crate::setup::Runner;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// The machine's name, as the head's flock.toml has it: what its own
    /// flock.toml and `machine check` know it by.
    pub name: String,
    /// The head's ssh destination, for client.toml.
    pub head: Option<String>,
    /// The head's public key, for this user's authorized_keys.
    pub head_key: Option<String>,
    /// The pastor binary to put in `~/.local/bin`.
    pub pastor_from: PathBuf,
    /// This user's config and state: where the keys and client.toml go.
    pub paths: Paths,
    /// `[artifacts] herdr` in this machine's pastor.toml.
    pub herdr_pin: Option<String>,
}

/// Where a pull machine's outcome waits until the head has it: `pastor join`
/// removes the file once its own report is answered, else the shepherd sends
/// it and removes it then.
pub fn report_file(state_dir: &std::path::Path) -> PathBuf {
    state_dir.join("bootstrap.json")
}

/// What the harnesses step found to do.
pub enum Harnesses {
    /// This machine's flock.toml does not name it: its agents and pins are
    /// the head's.
    NotHere,
    /// Nothing to install, and why.
    NonePinned(String),
    /// What stopped it before any install.
    Failed(String),
    Report(crate::harness_install::Report),
}

pub type LocalFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + 'a>>;

/// The steps that run pastor's own machinery on this machine.
pub trait Local {
    /// herdr at `pin`, as `pastor machine install --kind herdr` puts it.
    fn herdr<'a>(&'a self, pin: &'a str) -> LocalFuture<'a, crate::harness_install::Row>;
    /// The harnesses of the agents that can land here, at their pins.
    fn harnesses(&self) -> LocalFuture<'_, Harnesses>;
    /// `pastor machine check` of this machine.
    fn check(&self) -> LocalFuture<'_, Result<crate::machine_check::Report, String>>;
}

/// Run the steps in order, stopping at the first that fails.
pub async fn run(r: &dyn Runner, o: &Options, local: &dyn Local) -> Outcome {
    run_steps(r, o, local, &STEPS).await
}

/// Run the steps called `names`, in that order, stopping at the first that
/// fails.
pub async fn run_steps(r: &dyn Runner, o: &Options, local: &dyn Local, names: &[&str]) -> Outcome {
    let ctx = steps::Ctx {
        o,
        facts: facts::facts(r),
        found: Default::default(),
    };
    let mut results = Vec::new();
    for name in names {
        let result = steps::run(name, r, &ctx, local).await;
        let ends = steps::ends_run(&result);
        results.push(result);
        if ends {
            break;
        }
    }
    let found = ctx.found.into_inner();
    Outcome {
        ok: results.iter().all(|s| s.status != StepStatus::Failed),
        isolation: Isolation::None,
        steps: results,
        machine_key: found.machine_key,
        agent_key: found.agent_key,
        host_key: found.host_key,
        herdr_version: found.herdr_version,
        pastor_version: found.pastor_version,
        check: found.check,
    }
}

/// The code of a run that stopped at a step for want of a supported
/// platform.
pub const BOOTSTRAP_UNSUPPORTED_PLATFORM: &str = "bootstrap_unsupported_platform";

/// The code of a run that stopped at what the machine lacks; the step's
/// `missing` lists it.
pub const BOOTSTRAP_MISSING: &str = "bootstrap_missing";

/// The code of any other failed step.
pub const BOOTSTRAP_FAILED: &str = "bootstrap_failed";

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
        message.push_str("; fix: ");
        message.push_str(fix);
    }
    let code = if !failed.missing.is_empty() {
        BOOTSTRAP_MISSING
    } else if failed.step == "platform" {
        BOOTSTRAP_UNSUPPORTED_PLATFORM
    } else {
        BOOTSTRAP_FAILED
    };
    Some(CliError::err(code, message))
}

/// One line per step, then what this mode costs.
pub fn human(o: &Outcome) -> String {
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

/// `pastor bootstrap`.
#[derive(clap::Args, Debug)]
pub struct Args {
    /// The machine's name in the head's flock.toml (the hostname when unset)
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,
    /// The head's public key line, for this user's authorized_keys
    #[arg(long, value_name = "PUBKEY")]
    pub head_key: Option<String>,
    /// Print as a JSON object: isolation, each step, the public keys and versions, the check
    #[arg(long)]
    pub json: bool,
}

/// Entry point for `pastor bootstrap`; `head` is the global `--head`.
pub async fn cli(paths: &Paths, args: &Args, head: Option<&str>) -> anyhow::Result<()> {
    let head = head.map(str::trim).filter(|h| !h.is_empty());
    if let Some(dest) = head
        && (dest.starts_with('-') || dest.chars().any(char::is_whitespace))
    {
        return Err(CliError::err(
            "usage_error",
            format!("{dest:?} is not an ssh destination"),
        ));
    }
    let name = args.name.clone().unwrap_or_else(crate::config::hostname);
    if !crate::authkeys::valid_name(&name) {
        return Err(CliError::err(
            "invalid_machine",
            format!("{name:?} cannot be a machine name"),
        ));
    }
    let config = crate::config::PastorConfig::load(&paths.config_file())?;
    let o = Options {
        name: name.clone(),
        head: head.map(str::to_string),
        head_key: args.head_key.clone(),
        pastor_from: std::env::current_exe()?,
        paths: paths.clone(),
        herdr_pin: config
            .artifacts
            .herdr
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string),
    };
    let local = OnThisMachine {
        paths,
        name,
        config,
    };
    let outcome = run(&crate::setup::SystemRunner, &o, &local).await;
    crate::provision::run::finish_cleanups().await;
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

/// `Local` on the machine bootstrap runs on, from its own config: what
/// `pastor machine install` and `pastor machine check` run, through a local
/// shell.
struct OnThisMachine<'a> {
    paths: &'a Paths,
    name: String,
    config: crate::config::PastorConfig,
}

impl OnThisMachine<'_> {
    /// This machine's flock.toml, and its entry there as a `local` machine:
    /// the one it has, or one that names only it when it has none. `Err`
    /// says why the file does not load.
    fn machine(&self) -> Result<(crate::config::flock::Flock, bool), String> {
        use crate::config::flock::Flock;
        let file = self.paths.flock_file();
        let mut flock = if file.exists() {
            Flock::load(&file).map_err(|e| format!("{e:#}"))?
        } else {
            Flock::default()
        };
        let named = match flock.machines.iter_mut().find(|m| m.name == self.name) {
            Some(m) => {
                // Reached here, not over ssh or a command: this is it.
                m.local = true;
                m.ssh = None;
                m.command = None;
                m.pull = false;
                true
            }
            None => {
                let only = format!("[[machine]]\nname = \"{}\"\nlocal = true\n", self.name);
                let alone = Flock::parse(std::path::Path::new("flock.toml"), &only)
                    .map_err(|e| format!("{e:#}"))?;
                flock.machines.extend(alone.machines);
                false
            }
        };
        Ok((flock, named))
    }

    fn parts<'b>(
        &'b self,
        harnesses: &'b crate::harness_manifest::Harnesses,
    ) -> crate::machine_check::Config<'b> {
        crate::machine_check::Config {
            defaults: &self.config.defaults,
            agents: &self.config.agents,
            models: &self.config.models,
            profiles: &self.config.profiles,
            harnesses,
            herdr_pin: self
                .config
                .artifacts
                .herdr
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty()),
            head_address: None,
        }
    }
}

impl Local for OnThisMachine<'_> {
    fn herdr<'a>(&'a self, pin: &'a str) -> LocalFuture<'a, crate::harness_install::Row> {
        Box::pin(async move {
            let failed = |detail: String| crate::harness_install::Row {
                kind: crate::harness_install::HERDR.into(),
                pin: pin.to_string(),
                before: None,
                after: None,
                source: None,
                action: crate::harness_install::Action::Failed,
                steps: Vec::new(),
                detail: Some(detail),
            };
            let (flock, _) = match self.machine() {
                Ok(m) => m,
                Err(why) => return failed(why),
            };
            let m = flock.get(&self.name).expect("added above");
            let shell = match crate::provision::run::MachineShell::new(m, self.paths) {
                Ok(s) => Arc::new(s),
                Err(e) => return failed(e.to_string()),
            };
            let at_work = crate::harness_install::ListsAgents(Arc::new(
                crate::herdr::Endpoint::from_machine(m, self.paths),
            ));
            let want = crate::harness_install::HerdrWant {
                pin,
                artifacts: &self.config.artifacts,
                live: &at_work,
            };
            let report = crate::harness_install::install(
                self.paths,
                shell,
                &self.name,
                &[],
                Some(&want),
                &crate::artifacts::CurlFetch,
                false,
            )
            .await;
            report
                .rows
                .into_iter()
                .next()
                .unwrap_or_else(|| failed("the install gave no row".into()))
        })
    }

    fn harnesses(&self) -> LocalFuture<'_, Harnesses> {
        Box::pin(async move {
            let (flock, named) = match self.machine() {
                Ok(m) => m,
                Err(why) => return Harnesses::Failed(why),
            };
            if !named {
                return Harnesses::NotHere;
            }
            let m = flock.get(&self.name).expect("named");
            let manifests = match crate::harness_manifest::Harnesses::load(self.paths) {
                Ok(h) => h,
                Err(e) => return Harnesses::Failed(format!("{e:#}")),
            };
            let parts = self.parts(&manifests);
            let agents = crate::machine_check::agents_on(m, &flock, &parts);
            let pins = flock.pins_for(m);
            let wants = match crate::harness_install::wanted(
                &agents,
                &pins,
                &manifests,
                &crate::harness_install::Only::All,
            ) {
                Ok(wants) => wants,
                Err(why) => return Harnesses::NonePinned(why),
            };
            let shell = match crate::provision::run::MachineShell::new(m, self.paths) {
                Ok(s) => Arc::new(s),
                Err(e) => return Harnesses::Failed(e.to_string()),
            };
            Harnesses::Report(
                crate::harness_install::install(
                    self.paths,
                    shell,
                    &self.name,
                    &wants,
                    None,
                    &crate::artifacts::CurlFetch,
                    false,
                )
                .await,
            )
        })
    }

    fn check(&self) -> LocalFuture<'_, Result<crate::machine_check::Report, String>> {
        Box::pin(async move {
            let (flock, _) = self.machine()?;
            let m = flock.get(&self.name).expect("added above");
            let catalog =
                crate::provision::Catalog::load(self.paths).map_err(|e| format!("{e:#}"))?;
            let manifests = crate::harness_manifest::Harnesses::load(self.paths)
                .map_err(|e| format!("{e:#}"))?;
            let shell = Arc::new(
                crate::provision::run::MachineShell::new(m, self.paths)
                    .map_err(|e| e.to_string())?,
            );
            let running = crate::machine_check::running_herdr(m, self.paths).await;
            crate::machine_check::check(
                shell,
                m,
                &flock,
                &self.parts(&manifests),
                &catalog,
                None,
                running.as_deref(),
                Some(Arc::new(crate::herdr::transport::Endpoint::from_machine(
                    m, self.paths,
                ))),
            )
            .await
            .map_err(|e| format!("{e:#}"))
        })
    }
}

#[cfg(test)]
mod tests;
