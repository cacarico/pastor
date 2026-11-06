//! `pastor profile list|describe|render`: the permission profiles, built in
//! and from pastor.toml. Read straight from the file, so they work with the
//! head down, and change nothing; with a head on another machine the file is
//! the head's, fetched with `FileGet`.
use clap::Subcommand;

use crate::cli::{CliError, printable, table};
use crate::config::PastorConfig;

#[derive(Subcommand, Debug)]
pub enum ProfileCmd {
    /// Every permission profile: name, where it comes from, what it extends
    List {
        /// Print as a JSON array
        #[arg(long)]
        json: bool,
    },
    /// One profile with its extends followed: the allow and deny lists it adds up to
    Describe {
        /// The profile's name, as `profile list` shows it
        profile: String,
        /// Print as a JSON object
        #[arg(long)]
        json: bool,
    },
    /// What a profile becomes for claude or agy: its flags, env and settings, and what it cannot carry
    Render {
        /// The profile's name; with --machine, left out for the machine's own
        profile: Option<String>,
        /// The kind of agent to render for
        #[arg(long, value_parser = crate::profile_render::ALL_KINDS)]
        kind: String,
        /// Settle the profile as for a task on this machine: `unrestricted` only where the machine's own profile is, and no name means the machine's own
        #[arg(long)]
        machine: Option<String>,
        /// Print as a JSON object
        #[arg(long)]
        json: bool,
    },
}

/// What `render` needs besides pastor.toml: the flock, for `--machine`.
pub fn needs_flock(cmd: &ProfileCmd) -> bool {
    matches!(
        cmd,
        ProfileCmd::Render {
            machine: Some(_),
            ..
        }
    )
}

/// The profile `machine` has of its own in the flock (`defaults`, its flock,
/// or the machine itself), `unknown_machine` when it is in none.
pub fn own_profile(
    config: &PastorConfig,
    flock: &crate::config::flock::Flock,
    machine: &str,
) -> anyhow::Result<Option<String>> {
    let Some(m) = flock.get(machine) else {
        return Err(CliError::err(
            "unknown_machine",
            format!("no machine {machine} in the flock"),
        ));
    };
    Ok(config
        .defaults
        .own_profile(Some(m), flock.entry(flock.primary_flock(m))))
}

/// `cmd` against `config`, the head's pastor.toml.
pub fn run(
    config: &PastorConfig,
    flock: Option<&crate::config::flock::Flock>,
    cmd: ProfileCmd,
) -> anyhow::Result<()> {
    let profiles = &config.profiles;
    match cmd {
        ProfileCmd::Render {
            profile,
            kind,
            machine,
            json,
        } => {
            let own = match (&machine, flock) {
                (Some(name), Some(f)) => own_profile(config, f, name)?,
                _ => None,
            };
            let Some(name) = profile.or_else(|| own.clone().filter(|_| machine.is_some())) else {
                return Err(CliError::err(
                    "no_profile",
                    match &machine {
                        Some(m) => format!("machine {m} has no profile of its own; name one"),
                        None => "name a profile".to_string(),
                    },
                ));
            };
            let p = profiles
                .resolve(&name)
                .map_err(|e| CliError::err(e.code, e.message))?;
            let on = machine
                .as_deref()
                .map(|name| crate::profile_render::OnMachine {
                    name,
                    own: own.as_deref(),
                });
            let rendered = crate::profile_render::render(config, &p, &kind, on)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rendered)?);
            } else {
                print!("{}", rendered.text());
            }
        }
        ProfileCmd::List { json } => {
            let all = profiles
                .all()
                .into_keys()
                .map(|name| profiles.resolve(&name))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| CliError::err(e.code, e.message))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&all)?);
            } else {
                let rows: Vec<Vec<String>> = all
                    .iter()
                    .map(|p| {
                        vec![
                            p.name.clone(),
                            p.source.to_string(),
                            p.extends.clone().unwrap_or_else(|| "-".into()),
                            printable(p.description.as_deref().unwrap_or("")),
                        ]
                    })
                    .collect();
                println!(
                    "{}",
                    table(&["NAME", "SOURCE", "EXTENDS", "DESCRIPTION"], &rows)
                );
            }
        }
        ProfileCmd::Describe { profile, json } => {
            let p = profiles
                .resolve(&profile)
                .map_err(|e| CliError::err(e.code, e.message))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&p)?);
            } else {
                let list = |xs: &[String]| {
                    if xs.is_empty() {
                        "-".to_string()
                    } else {
                        xs.iter()
                            .map(|x| printable(x))
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                };
                println!("name: {}", p.name);
                println!("source: {}", p.source);
                if let Some(d) = &p.description {
                    println!("description: {}", printable(d));
                }
                println!("chain: {}", p.chain.join(" -> "));
                println!("allow: {}", list(&p.allow));
                println!("deny: {}", list(&p.deny));
            }
        }
    }
    Ok(())
}
