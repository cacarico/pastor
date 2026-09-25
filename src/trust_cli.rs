//! `pastor trust list|remove`: the (machine, repo) pairs whose folder-trust
//! prompt the head answers on its own. Read and written straight in the
//! store, so they work with the head down; the head reads the table afresh
//! each time a task blocks, so a change applies at once.
use clap::Subcommand;

use crate::cli::{CliError, age, table};
use crate::config::Paths;
use crate::store::Store;

#[derive(Subcommand, Debug)]
pub enum TrustCmd {
    /// Every saved trust: machine, repo, and when it was saved
    List {
        #[arg(long)]
        json: bool,
    },
    /// Forget a saved trust; the repo's next task asks again
    Remove { machine: String, repo: String },
}

pub fn run(paths: &Paths, cmd: TrustCmd) -> anyhow::Result<()> {
    paths.ensure()?;
    let store = Store::open(&paths.db_file())?;
    match cmd {
        TrustCmd::List { json } => {
            let list = store.trusted_repos()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&list)?);
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
