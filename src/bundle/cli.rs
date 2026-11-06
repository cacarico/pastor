//! `pastor bundle ...`. Everything here works from files on the head, with
//! or without a daemon running: the bundles dir and the versions kept, and
//! flock.toml for who names a bundle. `apply` reaches the machines itself,
//! as `machine check` does without a head.

use std::path::PathBuf;

use clap::{ArgGroup, Subcommand};
use serde::Serialize;

use super::apply::{Action, Row as ApplyRow};
use super::store::{self, Origin, Source, StoreError, Version};
use super::{BUNDLE_IN_USE, Catalog, Manifest, UNKNOWN_BUNDLE};
use crate::cli::{CliError, one_line};
use crate::config::Paths;
use crate::config::flock::{BundleUsers, Flock, MachineConfig};

#[derive(Subcommand, Debug)]
pub enum BundleCmd {
    /// Add a bundle from a directory, or from a git ref: validate it, refuse
    /// secrets, and keep it as a new version when its content changed
    #[command(group(ArgGroup::new("source").required(true).args(["dir", "git"])))]
    Add {
        /// The bundle's directory, with its pastor-bundle.toml
        dir: Option<PathBuf>,
        /// A git repository to fetch the bundle from, with --ref
        #[arg(long, requires = "git_ref")]
        git: Option<String>,
        /// The branch, tag or commit to fetch; the commit it names is recorded
        #[arg(long = "ref", requires = "git")]
        git_ref: Option<String>,
        /// The bundle's directory inside the repository
        #[arg(long, requires = "git")]
        path: Option<String>,
        /// Print as a JSON object
        #[arg(long)]
        json: bool,
    },
    /// List bundles: version, content hash, versions kept, the flocks naming each
    List {
        /// Print as a JSON array
        #[arg(long)]
        json: bool,
    },
    /// One bundle in full: what it carries, who names it, the versions kept
    Show {
        /// The bundle's id, as `bundle list` shows it
        id: String,
        /// Print as a JSON object
        #[arg(long)]
        json: bool,
    },
    /// Remove a bundle and its versions; refused while flock.toml or a job names it
    Remove {
        /// The bundle's id, as `bundle list` shows it
        id: String,
        /// Print as a JSON object
        #[arg(long)]
        json: bool,
    },
    /// Place a machine's bundles in its agents' homes: skills, hooks, bin
    /// files and instruction blocks, and take out what it no longer gets
    Apply {
        /// The machine; unset, every machine reached by ssh or locally
        #[arg(conflicts_with = "local")]
        machine: Option<String>,
        /// Say what would change, and change nothing
        #[arg(long)]
        dry_run: bool,
        /// Print as a JSON array, one object per machine
        #[arg(long)]
        json: bool,
        /// Apply the rendered set in this directory to this machine; what
        /// the head runs on the machine
        #[arg(long, hide = true)]
        local: Option<PathBuf>,
    },
    /// Take over the homes of a machine's agents in one step: back up what
    /// it touches, make linked files and dirs plain copies, and apply its
    /// bundles with the bundle's value winning over the user's
    Adopt {
        /// The machine
        #[arg(required_unless_present = "local", conflicts_with = "local")]
        machine: Option<String>,
        /// Only the home of this agent; unset, every home on the machine
        #[arg(long, conflicts_with = "local")]
        agent: Option<String>,
        /// List what would be replaced, and change nothing
        #[arg(long)]
        dry_run: bool,
        /// Print as a JSON object
        #[arg(long)]
        json: bool,
        /// Adopt with the rendered set in this directory on this machine;
        /// what the head runs on the machine
        #[arg(long, hide = true)]
        local: Option<PathBuf>,
        /// With --local: a home to adopt, as the plan names it; unset, every home
        #[arg(long = "home", hide = true, requires = "local")]
        homes: Vec<String>,
    },
}

pub async fn run(paths: &Paths, cmd: BundleCmd) -> anyhow::Result<()> {
    match cmd {
        BundleCmd::Add {
            dir,
            git,
            git_ref,
            path,
            json,
        } => {
            let source = match (dir, git, git_ref) {
                (Some(dir), None, None) => Source::Dir(dir),
                (None, Some(url), Some(git_ref)) => Source::Git { url, git_ref, path },
                _ => unreachable!("clap takes a directory or --git with --ref"),
            };
            add(paths, &source, json)
        }
        BundleCmd::List { json } => list(paths, json),
        BundleCmd::Show { id, json } => show(paths, &id, json),
        BundleCmd::Remove { id, json } => remove(paths, &id, json),
        BundleCmd::Apply {
            local: Some(dir),
            dry_run,
            json,
            ..
        } => run_here(paths, &dir, dry_run, json, None),
        BundleCmd::Apply {
            machine,
            dry_run,
            json,
            local: None,
        } => apply(paths, machine.as_deref(), dry_run, json).await,
        BundleCmd::Adopt {
            local: Some(dir),
            dry_run,
            json,
            homes,
            ..
        } => {
            let adopt = super::apply::Adopt { homes };
            run_here(paths, &dir, dry_run, json, Some(&adopt))
        }
        BundleCmd::Adopt {
            machine: Some(machine),
            agent,
            dry_run,
            json,
            local: None,
            ..
        } => adopt(paths, &machine, agent.as_deref(), dry_run, json).await,
        BundleCmd::Adopt { .. } => unreachable!("clap takes a machine or --local"),
    }
}

/// One machine's apply, as `bundle apply --json` prints it.
#[derive(Serialize)]
struct Applied {
    machine: String,
    ok: bool,
    /// Why nothing was applied there.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// Why the machine was left out.
    #[serde(skip_serializing_if = "Option::is_none")]
    skipped: Option<String>,
    /// Where `bundle adopt` copied what it touched, on the machine.
    #[serde(skip_serializing_if = "Option::is_none")]
    backup: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    rows: Vec<ApplyRow>,
}

/// The settings `bundle apply` and `bundle adopt` read on the head.
struct Head {
    flock: Flock,
    config: crate::config::PastorConfig,
    harnesses: crate::harness_manifest::Harnesses,
}

impl Head {
    fn load(paths: &Paths) -> anyhow::Result<Head> {
        let flock = Flock::load(&paths.flock_file())?;
        let config = crate::config::PastorConfig::load(&paths.config_file())?;
        flock.check_bundles(&Catalog::load(paths)?)?;
        let harnesses = crate::harness_manifest::Harnesses::load(paths)?;
        Ok(Head {
            flock,
            config,
            harnesses,
        })
    }

    fn parts(&self) -> crate::machine_check::Config<'_> {
        crate::machine_check::Config {
            defaults: &self.config.defaults,
            agents: &self.config.agents,
            models: &self.config.models,
            profiles: &self.config.profiles,
            harnesses: &self.harnesses,
            herdr_pin: None,
            head_address: None,
        }
    }

    fn machine(&self, name: &str) -> anyhow::Result<&MachineConfig> {
        self.flock.get(name).ok_or_else(|| {
            CliError::err("unknown_machine", format!("no machine {name} in the flock"))
        })
    }
}

/// Why bundles cannot reach `m`, if they cannot: a pull machine, or one
/// behind a command bridge.
fn unreachable(paths: &Paths, m: &MachineConfig) -> Option<String> {
    if m.pull {
        return Some(format!(
            "{} is a pull machine, which bundles do not reach yet",
            m.name
        ));
    }
    match crate::provision::run::MachineShell::new(m, paths) {
        Ok(_) => None,
        Err(_) => Some(format!(
            "{} is reached through a command bridge, which runs no shell to apply bundles in",
            m.name
        )),
    }
}

/// The program that runs pastor on `m`: this very binary on a local
/// machine, the pastor on its PATH on another.
fn pastor_on(m: &MachineConfig) -> anyhow::Result<String> {
    Ok(if m.local {
        std::env::current_exe()?.to_string_lossy().into_owned()
    } else {
        "pastor".to_string()
    })
}

async fn apply(
    paths: &Paths,
    machine: Option<&str>,
    dry_run: bool,
    json: bool,
) -> anyhow::Result<()> {
    let head = Head::load(paths)?;
    let flock = &head.flock;
    let machines: Vec<&MachineConfig> = match machine {
        Some(name) => vec![head.machine(name)?],
        None => flock.machines.iter().collect(),
    };
    let parts = head.parts();
    let mut out = Vec::new();
    for m in machines {
        let one = |error: Option<String>, skipped: Option<String>, rows: Vec<ApplyRow>| Applied {
            machine: m.name.clone(),
            ok: error.is_none() && !rows.iter().any(|r| r.action.is_problem()),
            error,
            skipped,
            backup: None,
            rows,
        };
        if let Some(why) = unreachable(paths, m) {
            // Asked for by name, it is an error; among all, it is left out.
            out.push(match machine {
                Some(_) => one(Some(why), None, vec![]),
                None => one(None, Some(why), vec![]),
            });
            continue;
        }
        let shell = crate::provision::run::MachineShell::new(m, paths).expect("checked above");
        let plan = match super::plan::render(paths, flock, &parts, m) {
            Ok(plan) => plan,
            Err(why) => {
                out.push(one(Some(why), None, vec![]));
                continue;
            }
        };
        // Among all, a machine with nothing to place and nothing to take off
        // is left alone: reaching it could only fail, and be recorded.
        if machine.is_none() && super::keep::nothing_to_keep(paths, &plan) {
            out.push(one(
                None,
                Some(format!("no bundle is meant for {}", m.name)),
                vec![],
            ));
            continue;
        }
        let pastor = pastor_on(m)?;
        let shell = std::sync::Arc::new(shell);
        // A real apply is recorded, so the head and `machine describe` know
        // what was last applied however it was run.
        let applied = if dry_run {
            super::push::apply(paths, shell, &plan, &pastor, true, None)
                .await
                .map_err(|f| f.why)
        } else {
            let done = super::keep::apply_recorded(paths, shell, &plan, &pastor).await;
            match done.report {
                Some(report) => Ok(report),
                None => Err(done.applied.error.unwrap_or_default()),
            }
        };
        match applied {
            Ok(report) => out.push(one(None, None, report.rows)),
            Err(why) => out.push(one(Some(why), None, vec![])),
        }
    }
    crate::provision::run::finish_cleanups().await;
    let ok = out.iter().all(|a| a.ok);
    if json {
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        for a in &out {
            print!("{}", applied_text(a, dry_run));
        }
    }
    if !ok {
        std::process::exit(1);
    }
    Ok(())
}

fn applied_text(a: &Applied, dry_run: bool) -> String {
    let when = if dry_run {
        " (dry run, nothing written)"
    } else {
        ""
    };
    if let Some(e) = &a.error {
        return format!("{}{when}: {}\n", a.machine, one_line(e));
    }
    if let Some(s) = &a.skipped {
        return format!("{}: skipped: {}\n", a.machine, one_line(s));
    }
    let mut counts: Vec<(String, usize)> = Vec::new();
    for r in &a.rows {
        let name = action_word(r.action);
        match counts.iter_mut().find(|(n, _)| *n == name) {
            Some((_, c)) => *c += 1,
            None => counts.push((name, 1)),
        }
    }
    let said = if counts.is_empty() {
        "nothing to place".to_string()
    } else if !a.rows.iter().any(|r| r.action != Action::Kept) {
        format!("{} kept, nothing to change", a.rows.len())
    } else {
        counts
            .iter()
            .map(|(n, c)| format!("{c} {n}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut out = format!("{}{when}: {said}\n", a.machine);
    if let Some(b) = &a.backup {
        out.push_str(&format!("  backup: {}\n", one_line(b)));
    }
    for r in a.rows.iter().filter(|r| r.action != Action::Kept) {
        let detail = match (&r.old, &r.backup) {
            (Some(old), Some(b)) => format!(": was {}; backup {}", one_line(old), one_line(b)),
            (Some(old), None) => format!(": was {}", one_line(old)),
            _ => r
                .detail
                .as_deref()
                .map(|d| format!(": {}", one_line(d)))
                .unwrap_or_default(),
        };
        let key = r
            .key
            .as_deref()
            .map(|k| format!(" {k}"))
            .unwrap_or_default();
        out.push_str(&format!(
            "  {:<15} {}{key}  {}{detail}\n",
            action_word(r.action),
            one_line(&r.path),
            r.bundle
        ));
    }
    out
}

fn action_word(a: Action) -> String {
    serde_json::to_value(a)
        .ok()
        .and_then(|v| v.as_str().map(|s| s.replace('_', " ")))
        .unwrap_or_default()
}

/// `--local`: apply a rendered set on this machine, or adopt with it.
fn run_here(
    paths: &Paths,
    dir: &std::path::Path,
    dry_run: bool,
    json: bool,
    adopt: Option<&super::apply::Adopt>,
) -> anyhow::Result<()> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|h| h.is_absolute())
        .ok_or_else(|| anyhow::anyhow!("$HOME is not an absolute path"))?;
    let report = super::apply::apply_local(dir, paths, &home, dry_run, adopt)?;
    if json {
        println!("{}", serde_json::to_string(&report)?);
    } else {
        let a = Applied {
            machine: report.machine.clone(),
            ok: report.ok(),
            error: None,
            skipped: None,
            backup: report.backup,
            rows: report.rows,
        };
        print!("{}", applied_text(&a, dry_run));
    }
    Ok(())
}

/// `bundle adopt`: take over the homes of `machine`'s agents, or `agent`'s
/// alone. Adopting every home applies the whole plan, so it is recorded as
/// an apply; one agent's home leaves the rest as they were, so it is not.
async fn adopt(
    paths: &Paths,
    machine: &str,
    agent: Option<&str>,
    dry_run: bool,
    json: bool,
) -> anyhow::Result<()> {
    let head = Head::load(paths)?;
    let m = head.machine(machine)?;
    if let Some(why) = unreachable(paths, m) {
        return Err(CliError::err("bundle_unreachable", why));
    }
    let parts = head.parts();
    let plan = super::plan::render(paths, &head.flock, &parts, m)
        .map_err(|why| CliError::err(super::BUNDLE_INVALID, why))?;
    let homes = match agent {
        None => Vec::new(),
        Some(agent) => {
            let homes = super::plan::homes_of(&head.flock, &parts, m);
            let Some(home) = homes.iter().find(|h| h.agents.iter().any(|a| a == agent)) else {
                return Err(CliError::err(
                    "unknown_agent",
                    format!(
                        "no task on {machine} runs agent {agent}, so it has no home there; its homes are those of {}",
                        homes
                            .iter()
                            .flat_map(|h| h.agents.iter().map(String::as_str))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ));
            };
            vec![home.path.clone()]
        }
    };
    let whole = homes.is_empty();
    let adopt = super::apply::Adopt { homes };
    let shell = std::sync::Arc::new(
        crate::provision::run::MachineShell::new(m, paths).expect("reachable, checked above"),
    );
    let pastor = pastor_on(m)?;
    let done = if dry_run || !whole {
        super::push::apply(paths, shell, &plan, &pastor, dry_run, Some(&adopt))
            .await
            .map_err(|f| f.why)
    } else {
        let done = super::keep::run_recorded(paths, shell, &plan, &pastor, Some(&adopt)).await;
        match done.report {
            Some(report) => Ok(report),
            None => Err(done.applied.error.unwrap_or_default()),
        }
    };
    crate::provision::run::finish_cleanups().await;
    let a = match done {
        Ok(report) => Applied {
            machine: m.name.clone(),
            ok: report.ok(),
            error: None,
            skipped: None,
            backup: report.backup,
            rows: report.rows,
        },
        Err(why) => Applied {
            machine: m.name.clone(),
            ok: false,
            error: Some(why),
            skipped: None,
            backup: None,
            rows: Vec::new(),
        },
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&a)?);
    } else {
        print!("{}", applied_text(&a, dry_run));
    }
    if !a.ok {
        std::process::exit(1);
    }
    Ok(())
}

fn store_err(e: StoreError) -> anyhow::Error {
    match e {
        StoreError::Io(e) => e,
        e => CliError::err(e.code(), e),
    }
}

/// The first 12 hex digits, enough to tell versions apart on screen.
fn short(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

/// Who names `id` in flock.toml; a flock.toml that does not load is a
/// warning, not a reason to refuse.
fn users(paths: &Paths, id: &str, warnings: &mut Vec<String>) -> BundleUsers {
    match Flock::load(&paths.flock_file()) {
        Ok(flock) => flock.bundle_users(id),
        Err(e) => {
            warnings.push(format!(
                "flock.toml does not load, so who uses the bundle is unknown: {e:#}"
            ));
            BundleUsers::default()
        }
    }
}

/// Refuse a version that would leave flock.toml short of what it needs:
/// a var it declares that a flock naming it does not give. What flock.toml
/// lacked already is not this version's doing and does not refuse it; a
/// flock.toml that does not load is a warning `add` gives later.
fn fits_flock(paths: &Paths, bundle: &super::Bundle) -> Result<(), String> {
    let Ok(flock) = Flock::load(&paths.flock_file()) else {
        return Ok(());
    };
    let now = Catalog::load(paths).map_err(|e| format!("{e:#}"))?;
    let before = flock.bundle_problems(&now);
    let new: Vec<String> = flock
        .bundle_problems(&now.with(bundle.clone()))
        .into_iter()
        .filter(|p| !before.contains(p))
        .collect();
    if new.is_empty() {
        return Ok(());
    }
    Err(format!(
        "bundle {} {} does not fit flock.toml, so it is not added: {}; set what it needs in flock.toml, then add it again",
        bundle.id,
        bundle.manifest.version,
        new.join("; ")
    ))
}

/// What flock.toml already lacks for its bundles, once a version is in:
/// `fits_flock` refused anything the version brought, so this was there
/// before it, and a head will not load flock.toml until it is set.
fn gaps(paths: &Paths) -> Option<String> {
    let flock = Flock::load(&paths.flock_file()).ok()?;
    let catalog = Catalog::load(paths).ok()?;
    let problems = flock.bundle_problems(&catalog);
    if problems.is_empty() {
        return None;
    }
    Some(format!(
        "flock.toml already lacks what its bundles need, so a head will not load it until it is set: {}",
        problems.join("; ")
    ))
}

#[derive(Serialize)]
struct AddOut {
    id: String,
    version: String,
    hash: String,
    previous_hash: Option<String>,
    flocks: Vec<String>,
    machines_affected: Vec<String>,
    warnings: Vec<String>,
}

fn add(paths: &Paths, source: &Source, json: bool) -> anyhow::Result<()> {
    let added = store::add_checked(paths, source, &|b| fits_flock(paths, b)).map_err(store_err)?;
    let mut warnings = Vec::new();
    let who = users(paths, &added.id, &mut warnings);
    warnings.extend(gaps(paths));
    if added.unchanged() {
        warnings.push("its content is already the newest version; nothing changed".into());
    }
    if who.is_empty() {
        warnings.push(format!(
            "no flock or machine names it yet; add \"{}\" to `bundles` in flock.toml",
            added.id
        ));
    }
    if json {
        let out = AddOut {
            id: added.id,
            version: added.version,
            hash: added.hash,
            previous_hash: added.previous_hash,
            flocks: who.flocks,
            machines_affected: who.affected,
            warnings,
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    if !added.unchanged() {
        let replacing = match &added.previous_hash {
            Some(p) => format!(", replacing {}", short(p)),
            None => String::new(),
        };
        println!(
            "added {} {} ({}){replacing}",
            added.id,
            one_line(&added.version),
            short(&added.hash)
        );
    }
    if !who.affected.is_empty() {
        println!(
            "machines that get it: {}; nothing is placed on them yet",
            who.affected.join(", ")
        );
    }
    for w in warnings {
        eprintln!("warning: {w}");
    }
    Ok(())
}

#[derive(Serialize)]
struct Row {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    /// Where it goes, `home` or `workdir`; `None` for one that does not
    /// load.
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<super::manifest::Target>,
    /// The content hash of the bundle as it is now.
    #[serde(skip_serializing_if = "Option::is_none")]
    hash: Option<String>,
    versions: usize,
    flocks: Vec<String>,
    machines: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn list(paths: &Paths, json: bool) -> anyhow::Result<()> {
    let catalog = Catalog::load(paths)?;
    let flock = Flock::load(&paths.flock_file()).ok();
    let who = |id: &str| {
        flock
            .as_ref()
            .map(|f| f.bundle_users(id))
            .unwrap_or_default()
    };
    let mut rows = Vec::new();
    for b in catalog.bundles() {
        let users = who(&b.id);
        let (hash, error) = match store::inspect(&b.dir) {
            Ok((_, _, hash)) => (Some(hash), None),
            Err(e) => (None, Some(e.to_string())),
        };
        rows.push(Row {
            id: b.id.clone(),
            version: Some(b.manifest.version.clone()),
            description: b.manifest.description.clone(),
            target: Some(b.manifest.target()),
            hash,
            versions: store::read_versions(paths, &b.id)?.len(),
            flocks: users.flocks,
            machines: users.machines,
            error,
        });
    }
    for (id, why) in catalog.invalid() {
        let users = who(id);
        rows.push(Row {
            id: id.clone(),
            version: None,
            description: None,
            target: None,
            hash: None,
            versions: store::read_versions(paths, id)?.len(),
            flocks: users.flocks,
            machines: users.machines,
            error: Some(why.clone()),
        });
    }
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    if json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!(
            "no bundles in {}; add one with `pastor bundle add <dir>`",
            paths.bundles_dir().display()
        );
        return Ok(());
    }
    let cells: Vec<[String; 7]> = rows
        .iter()
        .map(|r| {
            let named: Vec<String> = r
                .flocks
                .iter()
                .cloned()
                .chain(r.machines.iter().map(|m| format!("machine {m}")))
                .collect();
            let last = match &r.error {
                Some(e) => format!("invalid: {}", one_line(e)),
                None => r.description.as_deref().map(one_line).unwrap_or_default(),
            };
            [
                r.id.clone(),
                r.version
                    .as_deref()
                    .map(one_line)
                    .unwrap_or_else(|| "-".into()),
                r.target.map_or("-", |t| t.as_str()).to_string(),
                r.hash.as_deref().map(short).unwrap_or("-").to_string(),
                r.versions.to_string(),
                if named.is_empty() {
                    "-".into()
                } else {
                    named.join(",")
                },
                last,
            ]
        })
        .collect();
    let header = [
        "ID",
        "VERSION",
        "TARGET",
        "HASH",
        "KEPT",
        "NAMED BY",
        "DESCRIPTION",
    ]
    .map(String::from);
    let mut width = [0usize; 6];
    for row in std::iter::once(&header).chain(&cells) {
        for (w, c) in width.iter_mut().zip(row) {
            *w = (*w).max(c.chars().count());
        }
    }
    for row in std::iter::once(&header).chain(&cells) {
        let mut line = String::new();
        for (i, c) in row.iter().enumerate() {
            match width.get(i) {
                Some(w) => line.push_str(&format!("{c:<w$}  ")),
                None => line.push_str(c),
            }
        }
        println!("{}", line.trim_end());
    }
    Ok(())
}

#[derive(Serialize)]
struct ShowOut<'a> {
    id: &'a str,
    dir: String,
    /// The content hash of the bundle as it is now.
    hash: String,
    /// Whether the bundle changed since its newest version was kept.
    changed_since_add: bool,
    flocks: Vec<String>,
    machines: Vec<String>,
    machines_affected: Vec<String>,
    versions: Vec<Version>,
    manifest: &'a Manifest,
}

fn show(paths: &Paths, id: &str, json: bool) -> anyhow::Result<()> {
    let catalog = Catalog::load(paths)?;
    let bundle = match catalog.check(id) {
        Ok(b) => b,
        Err(why) if catalog.get(id).is_none() && catalog.invalid().any(|(i, _)| i == id) => {
            return Err(CliError::err(super::BUNDLE_INVALID, why));
        }
        Err(why) => return Err(CliError::err(UNKNOWN_BUNDLE, why)),
    };
    let (_, _, hash) = store::inspect(&bundle.dir).map_err(store_err)?;
    let versions = store::read_versions(paths, id)?;
    let changed = versions.first().is_none_or(|v| v.hash != hash);
    let mut warnings = Vec::new();
    let who = users(paths, id, &mut warnings);
    if json {
        let out = ShowOut {
            id,
            dir: bundle.dir.to_string_lossy().into_owned(),
            hash,
            changed_since_add: changed,
            flocks: who.flocks,
            machines: who.machines,
            machines_affected: who.affected,
            versions,
            manifest: &bundle.manifest,
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    print!("{}", show_text(bundle, &hash, changed, &who, &versions));
    for w in warnings {
        eprintln!("warning: {w}");
    }
    Ok(())
}

/// `show` for a person. The manifest is its author's text, so every string
/// of it is escaped (`one_line`).
fn show_text(
    bundle: &super::Bundle,
    hash: &str,
    changed: bool,
    who: &BundleUsers,
    versions: &[Version],
) -> String {
    let m = &bundle.manifest;
    let kinds = |k: &Option<Vec<String>>| match k {
        Some(k) => format!(" ({})", k.join(", ")),
        None => String::new(),
    };
    let mut out = format!("{} {} ({})\n", m.id, one_line(&m.version), short(hash));
    let mut line = |label: &str, value: String| {
        if !value.is_empty() {
            out.push_str(&format!("  {label}: {value}\n"));
        }
    };
    line(
        "description",
        m.description.as_deref().map(one_line).unwrap_or_default(),
    );
    line(
        "author",
        m.author.as_deref().map(one_line).unwrap_or_default(),
    );
    line("dir", bundle.dir.display().to_string());
    line("target", m.target().to_string());
    line(
        "vars",
        m.vars.keys().cloned().collect::<Vec<_>>().join(", "),
    );
    line(
        "skills",
        m.skills
            .iter()
            .map(|s| format!("{}{}", one_line(&s.src), kinds(&s.kinds)))
            .collect::<Vec<_>>()
            .join(", "),
    );
    line(
        "files",
        m.files
            .iter()
            .map(|f| match &f.path {
                Some(path) => format!(
                    "{} at {}{}",
                    one_line(&f.src),
                    one_line(path),
                    kinds(&f.kinds)
                ),
                None => {
                    let slot = serde_json::to_value(f.slot)
                        .ok()
                        .and_then(|v| v.as_str().map(String::from))
                        .unwrap_or_default();
                    format!("{} in {slot}", one_line(&f.src))
                }
            })
            .collect::<Vec<_>>()
            .join(", "),
    );
    if let Some(i) = &m.instructions {
        let per = match &i.per_machine {
            Some(p) => format!(", and {} per machine", one_line(p)),
            None => String::new(),
        };
        line(
            "instructions",
            format!("{}{per}{}", one_line(&i.src), kinds(&i.kinds)),
        );
    }
    line(
        "settings",
        m.settings.keys().cloned().collect::<Vec<_>>().join(", "),
    );
    line(
        "mcp",
        m.mcp
            .iter()
            .map(|s| {
                let env = if s.env_from.is_empty() {
                    String::new()
                } else {
                    format!(" (env {})", s.env_from.join(", "))
                };
                format!("{}{env}", s.name)
            })
            .collect::<Vec<_>>()
            .join(", "),
    );
    line(
        "plugins",
        m.plugins
            .iter()
            .map(|p| {
                let at = p
                    .git_ref
                    .as_deref()
                    .map(|r| format!(" {}", one_line(r)))
                    .unwrap_or_default();
                format!("{}@{}{at}", one_line(&p.name), one_line(&p.marketplace))
            })
            .collect::<Vec<_>>()
            .join(", "),
    );
    line(
        "requires",
        m.requires
            .iter()
            .map(|r| r.id.clone())
            .collect::<Vec<_>>()
            .join(", "),
    );
    let named: Vec<String> = who
        .flocks
        .iter()
        .map(|f| format!("flock {f}"))
        .chain(who.machines.iter().map(|m| format!("machine {m}")))
        .collect();
    line(
        "named by",
        if named.is_empty() {
            "nothing yet".into()
        } else {
            named.join(", ")
        },
    );
    if changed {
        out.push_str(
            "  changed since it was last added; `pastor bundle add` keeps it as a version\n",
        );
    }
    if !versions.is_empty() {
        out.push_str("  versions:\n");
        for v in versions {
            let from = match &v.origin {
                Origin::Dir { path } => one_line(path),
                Origin::Git {
                    url,
                    git_ref,
                    commit,
                    path,
                } => {
                    let sub = path
                        .as_deref()
                        .map(|p| format!(" {}", one_line(p)))
                        .unwrap_or_default();
                    format!(
                        "{} {} ({}){sub}",
                        one_line(url),
                        one_line(git_ref),
                        short(commit)
                    )
                }
            };
            out.push_str(&format!(
                "    {}  {}  {}  {from}\n",
                short(&v.hash),
                one_line(&v.version),
                v.added_at.format("%Y-%m-%d %H:%M")
            ));
        }
    }
    out
}

#[derive(Serialize)]
struct RemoveOut<'a> {
    id: &'a str,
    versions_removed: usize,
}

fn remove(paths: &Paths, id: &str, json: bool) -> anyhow::Result<()> {
    let known = paths.bundles_dir().join(id).symlink_metadata().is_ok()
        || paths.bundle_versions_dir(id).exists();
    if crate::connector::manifest::check_id(id).is_err() || !known {
        return Err(CliError::err(
            UNKNOWN_BUNDLE,
            format!("no bundle {id:?} in {}", paths.bundles_dir().display()),
        ));
    }
    let flock = Flock::load(&paths.flock_file())?;
    // Each user with the key that names it, so the refusal says what to edit.
    let has = |list: &[String]| list.iter().any(|b| b == id);
    let mut named: Vec<String> = Vec::new();
    for f in &flock.flocks {
        for (key, list) in [
            ("bundles", &f.bundles),
            ("workdir_bundles", &f.workdir_bundles),
        ] {
            if has(list) {
                named.push(format!("flock {} ({key})", f.name));
            }
        }
    }
    for m in flock.machines.iter().filter(|m| has(&m.bundles)) {
        named.push(format!("machine {} (bundles)", m.name));
    }
    let jobs = crate::config::job::naming_workdir_bundle(&paths.jobs_dir(), id);
    named.extend(
        jobs.iter()
            .map(|j| format!("job {j} ([dispatch] workdir_bundles)")),
    );
    if !named.is_empty() {
        let files = match (named.len() > jobs.len(), !jobs.is_empty()) {
            (true, true) => "flock.toml and the job files",
            (true, false) => "flock.toml",
            _ => "the job files",
        };
        return Err(CliError::err(
            BUNDLE_IN_USE,
            format!(
                "bundle {id} is named by {}; take it out of those lists in {files} first",
                named.join(", ")
            ),
        ));
    }
    let versions_removed = store::remove(paths, id)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&RemoveOut {
                id,
                versions_removed
            })?
        );
    } else {
        println!("removed {id} and the {versions_removed} versions kept of it");
    }
    Ok(())
}
