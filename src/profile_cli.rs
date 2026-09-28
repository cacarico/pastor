//! `pastor profile list|describe`: the permission profiles, built in and
//! from pastor.toml. Read straight from the file, so they work with the head
//! down, and change nothing.
use clap::Subcommand;

use crate::cli::{CliError, printable, table};
use crate::config::{PastorConfig, Paths};

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
}

pub fn run(paths: &Paths, cmd: ProfileCmd) -> anyhow::Result<()> {
    let config = PastorConfig::load(&paths.config_file())?;
    let profiles = &config.profiles;
    match cmd {
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
