//! `pastor plugin ...`. Everything here works from files, with or without a
//! daemon: plugins change only through these commands, and `run` dispatches
//! nothing.

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, bail};
use clap::Subcommand;
use serde::Serialize;

use super::install::{self, InstallSource};
use super::manifest::Manifest;
use super::{Discovered, Plugin, PluginCatalog, discover};
use crate::config::job::{self, Loaded};
use crate::config::{PastorConfig, Paths, parse_duration};
use crate::connector::{self, RunInput};

#[derive(Subcommand, Debug)]
pub enum PluginCmd {
    /// Install a plugin from GitHub: owner/repo, or owner/repo/subdir
    Install {
        source: String,
        /// Branch, tag or commit to check out
        #[arg(long = "ref")]
        git_ref: Option<String>,
        /// Do not ask for confirmation
        #[arg(long)]
        yes: bool,
    },
    /// Use a plugin from a local directory, in place (for developing one)
    Link { path: PathBuf },
    /// Remove an installed plugin (its .env and state are kept)
    Uninstall { id: String },
    /// Remove a linked plugin; the directory itself is left alone
    Unlink { id: String },
    /// List plugins: version, connector, hooks, missing secrets
    List {
        #[arg(long)]
        json: bool,
    },
    /// Run a plugin's connector once for a job and print its items; creates
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

pub async fn run(paths: &Paths, cmd: PluginCmd) -> anyhow::Result<()> {
    match cmd {
        PluginCmd::Install {
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
            after_change(paths, &p).await;
            Ok(())
        }
        PluginCmd::Link { path } => {
            let p = install::link(paths, &path)?;
            println!(
                "linked {} {} from {}",
                p.id,
                p.manifest.version,
                p.dir.display()
            );
            after_change(paths, &p).await;
            Ok(())
        }
        PluginCmd::Uninstall { id } => {
            install::uninstall(paths, &id)?;
            println!("uninstalled {id}; jobs using it are invalid until it is back");
            reload_daemon(paths).await;
            Ok(())
        }
        PluginCmd::Unlink { id } => {
            install::unlink(paths, &id)?;
            println!("unlinked {id}; jobs using it are invalid until it is back");
            reload_daemon(paths).await;
            Ok(())
        }
        PluginCmd::List { json } => {
            let rows = list_rows(paths)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                println!("{}", table(&rows));
            }
            Ok(())
        }
        PluginCmd::Run { id, job, since } => run_once(paths, &id, &job, since.as_deref()).await,
    }
}

/// What the plugin will run, shown before install asks.
fn describe(m: &Manifest) -> String {
    let mut out = format!("{} {} ({})\n", m.id, m.version, m.name);
    if let Some(d) = &m.description {
        out.push_str(&format!("  {d}\n"));
    }
    if let Some(c) = &m.connector {
        out.push_str(&format!(
            "  connector ({}): {}\n",
            c.mode,
            c.command.join(" ")
        ));
    }
    for h in &m.events {
        out.push_str(&format!(
            "  hook on {}: {}\n",
            h.on.join(", "),
            h.command.join(" ")
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
async fn after_change(paths: &Paths, p: &Plugin) {
    let env = p.env(paths).unwrap_or_default();
    let missing = p.missing_secrets(&env);
    if !missing.is_empty() {
        eprintln!(
            "set {} in {}",
            missing.join(", "),
            paths.plugin_env_file(&p.id).display()
        );
    }
    reload_daemon(paths).await;
}

/// A running daemon re-reads its plugins only on `Reload`; send it so the
/// change takes effect now. The change itself is already on disk, so a
/// failed reload is a warning, not an error.
async fn reload_daemon(paths: &Paths) {
    if let Some(note) = reload_note(&paths.socket_file()).await {
        eprintln!("{note}");
    }
}

/// What to tell the user about the reload: nothing when no daemon runs, a
/// warning when one is there but did not answer or did not reload.
async fn reload_note(socket: &std::path::Path) -> Option<String> {
    use crate::ipc::{DaemonProbe, IpcRequest, IpcResponse};
    match crate::ipc::probe_daemon(socket).await {
        DaemonProbe::NotRunning => return None,
        DaemonProbe::Unresponsive => {
            return Some(
                "pastor serve is not responding, so it did not reload; run `pastor reload` once it answers"
                    .into(),
            );
        }
        DaemonProbe::Running => {}
    }
    Some(
        match crate::ipc::request(socket, &IpcRequest::Reload).await {
            Ok(IpcResponse::Error { message, .. }) => {
                format!("pastor serve did not reload ({message}); run `pastor reload`")
            }
            Ok(_) => "pastor serve reloaded its plugins and jobs".into(),
            Err(e) => format!("pastor serve did not reload ({e:#}); run `pastor reload`"),
        },
    )
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PluginRow {
    pub id: String,
    pub version: Option<String>,
    /// `poll`, `stream`, or none.
    pub connector: Option<String>,
    pub hooks: usize,
    pub linked: bool,
    pub dir: PathBuf,
    pub missing_secrets: Vec<String>,
    /// Why the plugin is unusable: a bad manifest, or an unreadable `.env`.
    pub error: Option<String>,
}

pub fn list_rows(paths: &Paths) -> anyhow::Result<Vec<PluginRow>> {
    Ok(discover(paths)?
        .into_iter()
        .map(|d| match d {
            Discovered::Valid(p) => {
                let (missing_secrets, error) = match p.env(paths) {
                    Ok(env) => (p.missing_secrets(&env), None),
                    Err(e) => (Vec::new(), Some(format!("{e:#}"))),
                };
                PluginRow {
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
            } => PluginRow {
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

pub fn table(rows: &[PluginRow]) -> String {
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

/// `plugin run`: the job's config if its file exists (and names this plugin),
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
    let catalog = PluginCatalog::load(paths)?;
    let Some(plugin) = catalog.plugin(id).cloned() else {
        // Let the catalog explain: invalid, hooks only, or absent.
        let why = connector::Catalog::check(&catalog, id, &serde_json::json!({}))
            .err()
            .unwrap_or_else(|| format!("{id:?} is built in, not a plugin"));
        bail!("{why}");
    };
    if plugin.manifest.connector.is_none() {
        bail!("plugin {id:?} has no connector (it only provides event hooks)");
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
        connector::process::source(Arc::clone(&plugin), paths.clone(), Some(job_name.into()));
    let spec = plugin.manifest.connector.as_ref().expect("checked above");
    let out = match spec.mode {
        // A stream never ends on its own: collect for its timeout, then stop
        // (dropping the source kills it).
        super::manifest::Mode::Stream => {
            eprintln!(
                "stream connector: collecting for {}s",
                spec.timeout.as_secs()
            );
            let mut out = source.run(input.clone()).await.unwrap_or_default();
            tokio::time::sleep(spec.timeout).await;
            match source.run(input).await {
                Ok(more) => {
                    out.items.extend(more.items);
                    out.cursor = more.cursor.or(out.cursor);
                    out.logs.extend(more.logs);
                }
                Err(e) if out.items.is_empty() => bail!("{e}"),
                Err(e) => out.logs.push(format!("warn: {e}")),
            }
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

    #[tokio::test]
    async fn reload_is_quiet_without_a_daemon_and_warns_when_it_does_not_answer() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("pastor.sock");
        assert_eq!(
            reload_note(&socket).await,
            None,
            "no daemon, nothing to say"
        );

        // Something accepts on the socket and never answers.
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((s, _)) = listener.accept().await {
                held.push(s);
            }
        });
        let note = reload_note(&socket).await.expect("a warning");
        assert!(
            note.contains("not responding") && note.contains("pastor reload"),
            "{note}"
        );
    }
}
