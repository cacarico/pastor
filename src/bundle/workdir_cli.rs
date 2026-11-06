//! `pastor workdir`: a workdir set placed in, or taken out of, a directory
//! on this machine (`--local`, `bundle::workdir`), or on a push machine
//! through the head (`bundle::workdir_push`), and the head's record of them.

use std::path::{Path, PathBuf};

use clap::Subcommand;

use super::apply::{Action, Row};
use super::workdir::{self, Placed, Set, WORKDIR_INVALID, WORKDIR_UNSUPPORTED};
use super::workdir_push::{self, Record, Reported};
use super::workdir_report;
use crate::cli::{CliError, one_line};
use crate::config::{PastorConfig, Paths};
use crate::ipc::{Client, IpcRequest, IpcResponse};
use crate::profile_render::{self, OnMachine, Render};
use std::time::Duration;

#[derive(Subcommand, Debug)]
pub enum WorkdirCmd {
    /// Place a set of files in an agent's working directory before it
    /// starts, and take out what an earlier version of the set placed that
    /// this one no longer has
    Place {
        /// Place it in a directory on this machine, without the head
        #[arg(long)]
        local: bool,
        /// The machine, then the working directory on it as an absolute
        /// path; with --local, the directory alone
        #[arg(value_name = "[MACHINE] DIR", num_args = 1..=2, required = true)]
        targets: Vec<String>,
        /// The set: a JSON or TOML file, a directory with a
        /// pastor-workdir.toml, or - for stdin
        #[arg(long, required_unless_present = "bundle", conflicts_with = "bundle")]
        set: Option<String>,
        /// A stored workdir bundle in place of a set: the head builds its
        /// set for --kind from the newest version kept, with the vars of
        /// the machine's flock that names it
        #[arg(long, requires = "kind", conflicts_with = "local")]
        bundle: Option<String>,
        /// With --bundle: the kind of agent that starts in the directory
        #[arg(long, requires = "bundle")]
        kind: Option<String>,
        /// With --local: this machine's name in the flock, for a set that
        /// names a profile: settled as for a task on it, so `unrestricted`
        /// goes only where the machine's own profile is. Left out, the rules
        /// that look at the machine are not checked. It also names the
        /// machine in the report sent to a remote head; left out there,
        /// `[shepherd] machine` of pastor.toml, else the hostname
        #[arg(long, requires = "local")]
        machine: Option<String>,
        /// Say what would change, and change nothing
        #[arg(long)]
        dry_run: bool,
        /// Print as a JSON object
        #[arg(long)]
        json: bool,
    },
    /// Take out what pastor placed in a working directory, as long as it
    /// still holds what pastor wrote
    Remove {
        /// Remove it from a directory on this machine, without the head
        #[arg(long)]
        local: bool,
        /// The machine, then the working directory on it, which may be gone
        /// already; with --local, the directory alone
        #[arg(value_name = "[MACHINE] DIR", num_args = 1..=2, required = true)]
        targets: Vec<String>,
        /// Only this set; unset, every set placed there
        #[arg(long)]
        id: Option<String>,
        /// With --local: this machine's name in the flock, for the report
        /// sent to a remote head. Left out, `[shepherd] machine` of
        /// pastor.toml, else the hostname
        #[arg(long, requires = "local")]
        machine: Option<String>,
        /// Say what would change, and change nothing
        #[arg(long)]
        dry_run: bool,
        /// Print as a JSON object
        #[arg(long)]
        json: bool,
    },
    /// Say whether a directory still holds the sets the head placed in it:
    /// each read on the machine and placed again as a dry run, with its
    /// state; ok only when nothing would change
    Check {
        /// The machine the directory is on
        machine: String,
        /// The working directory on the machine, as an absolute path
        dir: String,
        /// Only this set; unset, every set the head placed there
        #[arg(long)]
        id: Option<String>,
        /// Print as a JSON array, one object per set
        #[arg(long)]
        json: bool,
    },
    /// The directories the head placed sets in, from its record
    List {
        /// Only this machine's
        machine: Option<String>,
        /// Print as a JSON array
        #[arg(long)]
        json: bool,
    },
    /// Carry out a plan the head sent: what `place` and `remove` run on a
    /// push machine
    #[command(hide = true)]
    Apply {
        /// It runs on this machine, where the head sent the plan
        #[arg(long)]
        local: bool,
        /// The run directory the head unpacked
        dir: PathBuf,
        /// Say what would change, and change nothing
        #[arg(long)]
        dry_run: bool,
        /// Print as a JSON object, the answer the head reads
        #[arg(long)]
        json: bool,
    },
}

impl WorkdirCmd {
    /// Whether it may change files: what an agent pastor started may not
    /// ask for.
    pub fn writes(&self) -> bool {
        match self {
            WorkdirCmd::Place { dry_run, .. }
            | WorkdirCmd::Remove { dry_run, .. }
            | WorkdirCmd::Apply { dry_run, .. } => !*dry_run,
            WorkdirCmd::Check { .. } | WorkdirCmd::List { .. } => false,
        }
    }

    /// Whether it runs here, on this machine's own files, rather than
    /// through the head.
    pub fn is_local(&self) -> bool {
        match self {
            WorkdirCmd::Place { local, .. } | WorkdirCmd::Remove { local, .. } => *local,
            WorkdirCmd::Apply { .. } => true,
            WorkdirCmd::Check { .. } | WorkdirCmd::List { .. } => false,
        }
    }
}

/// A command with its set read. `place` reads the set once, from a file or
/// stdin, before it is known whether the rules a profile needs must be
/// fetched.
pub struct Prepared {
    cmd: WorkdirCmd,
    set: Option<Set>,
}

/// Read the set a `place --local` names.
pub fn prepare(cmd: WorkdirCmd) -> anyhow::Result<Prepared> {
    let set = match &cmd {
        WorkdirCmd::Place {
            local,
            targets,
            set,
            ..
        } => {
            need_local(*local)?;
            one_dir(targets)?;
            let Some(set) = set else {
                return Err(CliError::err(
                    WORKDIR_INVALID,
                    "with --local, give the set: a stored bundle is placed through the head",
                ));
            };
            Some(Set::load(set)?)
        }
        WorkdirCmd::Remove { local, targets, .. } => {
            need_local(*local)?;
            one_dir(targets)?;
            None
        }
        WorkdirCmd::Apply { local, .. } => {
            need_local(*local)?;
            None
        }
        WorkdirCmd::Check { .. } | WorkdirCmd::List { .. } => {
            return Err(CliError::err(
                WORKDIR_UNSUPPORTED,
                "check and list read the head's record; they have no --local",
            ));
        }
    };
    Ok(Prepared { cmd, set })
}

impl Prepared {
    /// The profile the set names.
    pub fn profile(&self) -> Option<&str> {
        self.set.as_ref()?.profile.as_deref()
    }

    /// The machine name given with `--machine`.
    pub fn machine(&self) -> Option<&str> {
        match &self.cmd {
            WorkdirCmd::Place { machine, .. } | WorkdirCmd::Remove { machine, .. } => {
                machine.as_deref()
            }
            _ => None,
        }
    }
}

/// What a set that names a profile needs: the head's pastor.toml, and the
/// profile of its own the machine `--machine` names has in the flock.
pub struct Rules {
    pub config: PastorConfig,
    pub own: Option<String>,
}

/// What `run` did: the answer to print, and the report a pull or command
/// machine owes the head when it really placed or removed something.
pub struct Done {
    placed: Placed,
    json: bool,
    /// The machine's name as `--machine` gave it.
    machine: Option<String>,
    report: Option<Reported>,
}

impl Done {
    /// The report for the head, if this run owes one.
    pub fn report(&self) -> Option<&Reported> {
        self.report.as_ref()
    }

    /// Print the answer; a problem row is an error, as ever.
    pub fn print(self) -> anyhow::Result<()> {
        print_placed(&[self.placed], self.json, false)
    }
}

pub fn run(
    paths: &Paths,
    prepared: Prepared,
    rules: Option<Rules>,
) -> anyhow::Result<Option<Done>> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|h| h.is_absolute())
        .ok_or_else(|| anyhow::anyhow!("$HOME is not an absolute path"))?;
    let Prepared { cmd, set } = prepared;
    let (placed, json, machine, report) = match cmd {
        WorkdirCmd::Place {
            targets,
            machine,
            dry_run,
            json,
            ..
        } => {
            let set = set.expect("prepare read the set");
            let render = render(&set, machine.as_deref(), rules.as_ref())?;
            let dir = absolute(Path::new(&targets[0]))?;
            let placed = workdir::place_with(paths, &home, &dir, &set, render.as_ref(), dry_run)?;
            let report = (!dry_run).then(|| workdir_report::placed(&set, render.as_ref(), &placed));
            (placed, json, machine, report)
        }
        WorkdirCmd::Remove {
            targets,
            id,
            machine,
            dry_run,
            json,
            ..
        } => {
            let dir = absolute(Path::new(&targets[0]))?;
            let placed = workdir::remove(paths, &home, &dir, id.as_deref(), dry_run)?;
            let report = if dry_run {
                None
            } else {
                workdir_report::removed(&placed)
            };
            (placed, json, machine, report)
        }
        WorkdirCmd::Apply { dir, dry_run, .. } => {
            // Always an answer the head reads, an error included, so its
            // code reaches the one who asked.
            let answer = workdir_push::apply_local(paths, &home, &dir, dry_run);
            println!("{}", serde_json::to_string(&answer)?);
            return Ok(None);
        }
        WorkdirCmd::Check { .. } | WorkdirCmd::List { .. } => {
            unreachable!("prepare refuses them")
        }
    };
    Ok(Some(Done {
        placed,
        json,
        machine,
        report,
    }))
}

/// How long a run waits for the head to take its report. The files are
/// placed already, and the queue keeps the report for a later send, so a head
/// that drops packets must not hold up a launcher that places, then starts
/// its agent.
const REPORT_WAIT: Duration = Duration::from_secs(5);

/// Tell a remote head what this run did, with whatever waits from before.
/// The head does not reach a pull or command machine, so this is how its
/// record sees it. A head that does not answer leaves the report to go
/// with the next send (a later run, or the shepherd's tick); that is said
/// on stderr, and the place itself has gone through either way.
pub async fn report_to_head(paths: &Paths, client: &Client, done: &Done) {
    let Some(report) = done.report() else {
        return;
    };
    let machine = done.machine.clone().unwrap_or_else(|| {
        PastorConfig::load(&paths.config_file())
            .unwrap_or_default()
            .shepherd
            .machine_name()
    });
    if let Err(err) = workdir_report::queue(paths, &machine, report) {
        eprintln!("workdir: the report for the head could not be kept: {err:#}");
        return;
    }
    let sent = workdir_report::deliver(paths, |req| async move {
        crate::cli::reply(crate::ipc::request_head_with_timeout(client, &req, REPORT_WAIT).await)
            .map_err(anyhow::Error::from)
    })
    .await;
    if let Some(why) = sent.waiting {
        eprintln!(
            "workdir: the head did not take the report ({}); it is sent with the next run or, on a pull machine, the next tick of `pastor serve`",
            one_line(&why)
        );
    }
    for why in sent
        .refused
        .iter()
        .filter(|w| !w.contains(WORKDIR_UNSUPPORTED))
    {
        eprintln!("workdir: the head refused the report: {}", one_line(why));
    }
}

/// The machine and directory of a `place` or `remove` through the head.
fn machine_dir(targets: &[String]) -> anyhow::Result<(String, String)> {
    match targets {
        [machine, dir] => Ok((machine.clone(), dir.clone())),
        _ => Err(CliError::err(
            WORKDIR_INVALID,
            "name the machine and the directory on it, or pass --local to place in a directory here",
        )),
    }
}

fn one_dir(targets: &[String]) -> anyhow::Result<()> {
    match targets {
        [_] => Ok(()),
        _ => Err(CliError::err(
            WORKDIR_INVALID,
            "with --local, name the directory alone: it is on this machine",
        )),
    }
}

/// How long the head may take: a push to the machine and its answer, or a
/// check, which reads every set of the directory in one run.
pub const HEAD_WAIT: Duration = Duration::from_secs(10 * 60);

/// `place`, `remove`, `check` and `list` through the head, which reaches
/// push machines.
pub async fn run_head(client: &Client, cmd: WorkdirCmd) -> anyhow::Result<()> {
    let (req, json, many) = match cmd {
        WorkdirCmd::Place {
            targets,
            set,
            bundle,
            kind,
            dry_run,
            json,
            ..
        } => {
            let (machine, dir) = machine_dir(&targets)?;
            let set = match set {
                Some(set) => Some(Set::load(&set)?.to_doc()),
                None => None,
            };
            let what = bundle.clone().zip(kind.clone());
            let req = IpcRequest::WorkdirPlace {
                machine,
                dir,
                set,
                bundle,
                kind,
                flock: None,
                dry_run,
            };
            if let Some((bundle, kind)) = what {
                return place_bundle(client, &req, &bundle, &kind, json).await;
            }
            (req, json, false)
        }
        WorkdirCmd::Remove {
            targets,
            id,
            dry_run,
            json,
            ..
        } => {
            let (machine, dir) = machine_dir(&targets)?;
            let req = IpcRequest::WorkdirRemove {
                machine,
                dir,
                id,
                dry_run,
            };
            (req, json, false)
        }
        WorkdirCmd::Check {
            machine,
            dir,
            id,
            json,
        } => (IpcRequest::WorkdirCheck { machine, dir, id }, json, true),
        WorkdirCmd::List { machine, json } => {
            let resp = crate::cli::reply(
                crate::ipc::request_head(client, &IpcRequest::WorkdirList { machine }).await,
            )?;
            let IpcResponse::Workdirs(records) = resp else {
                return Err(crate::cli::unexpected(resp));
            };
            if json {
                println!("{}", serde_json::to_string(&records)?);
            } else {
                print!("{}", list_text(&records));
            }
            return Ok(());
        }
        WorkdirCmd::Apply { .. } => unreachable!("runs here"),
    };
    let got = crate::ipc::request_head_with_timeout(client, &req, HEAD_WAIT).await;
    match crate::cli::reply(got)? {
        IpcResponse::Workdir(placed) => print_placed(&placed, json, many),
        resp => Err(crate::cli::unexpected(resp)),
    }
}

/// `place --bundle`: the head's answer, or a line saying the bundle has
/// nothing for that kind of agent (`[]` with `--json`), which is no error.
async fn place_bundle(
    client: &Client,
    req: &IpcRequest,
    bundle: &str,
    kind: &str,
    json: bool,
) -> anyhow::Result<()> {
    let got = crate::ipc::request_head_with_timeout(client, req, HEAD_WAIT).await;
    match crate::cli::reply(got)? {
        IpcResponse::Workdir(placed) if placed.is_empty() => {
            if json {
                println!("[]");
            } else {
                println!("bundle {bundle} has nothing for {kind} agents; nothing placed");
            }
            Ok(())
        }
        IpcResponse::Workdir(placed) => print_placed(&placed, json, false),
        resp => Err(crate::cli::unexpected(resp)),
    }
}

/// Print each answer, a JSON object (an array for `many`) or text, and exit
/// 1 when one did not go through.
fn print_placed(placed: &[Placed], json: bool, many: bool) -> anyhow::Result<()> {
    if json {
        match (many, placed) {
            (false, [one]) => println!("{}", serde_json::to_string(one)?),
            _ => println!("{}", serde_json::to_string(placed)?),
        }
    } else {
        for p in placed {
            print!("{}", text(p));
        }
    }
    if placed.iter().any(|p| !p.ok) {
        std::process::exit(1);
    }
    Ok(())
}

fn list_text(records: &[Record]) -> String {
    if records.is_empty() {
        return "the head has placed no workdir set\n".to_string();
    }
    let rows: Vec<Vec<String>> = records
        .iter()
        .map(|r| {
            vec![
                r.machine.clone(),
                r.dir.clone(),
                r.id.clone(),
                r.kind.clone(),
                r.profile.clone().unwrap_or("-".into()),
                match &r.error {
                    Some(e) => format!("{}: {}", r.result, one_line(e)),
                    None => r.result.clone(),
                },
                r.placed_at.clone(),
                r.state.map_or("-".into(), |s| s.word().to_string()),
                r.read_at.clone().unwrap_or("-".into()),
            ]
        })
        .collect();
    crate::cli::table(
        &[
            "MACHINE", "DIR", "ID", "KIND", "PROFILE", "RESULT", "PLACED", "STATE", "READ",
        ],
        &rows,
    )
}

/// The profile a set names, rendered for its kind as for a task on `machine`.
fn render(
    set: &Set,
    machine: Option<&str>,
    rules: Option<&Rules>,
) -> anyhow::Result<Option<Render>> {
    let Some(name) = &set.profile else {
        return Ok(None);
    };
    let Some(rules) = rules else {
        return Err(CliError::err(
            workdir::WORKDIR_INVALID,
            format!("profile {name}: pastor.toml was not read"),
        ));
    };
    let profile = rules
        .config
        .profiles
        .resolve(name)
        .map_err(|e| CliError::err(e.code, e.message))?;
    let on = machine.map(|name| OnMachine {
        name,
        own: rules.own.as_deref(),
    });
    profile_render::render(&rules.config, &profile, &set.kind, on).map(Some)
}

/// What runs here is only what names `--local` (or the head's `apply`);
/// the rest goes through the head (`run_head`).
fn need_local(local: bool) -> anyhow::Result<()> {
    if local {
        return Ok(());
    }
    Err(CliError::err(
        WORKDIR_UNSUPPORTED,
        "pass --local and run it on the machine that has the directory",
    ))
}

fn absolute(dir: &Path) -> anyhow::Result<PathBuf> {
    Ok(match dir.is_absolute() {
        true => dir.to_path_buf(),
        false => std::env::current_dir()?.join(dir),
    })
}

fn text(p: &Placed) -> String {
    let when = if p.dry_run {
        " (dry run, nothing written)"
    } else {
        ""
    };
    let id = p.id.as_deref().map(|i| format!(" {i}")).unwrap_or_default();
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for r in &p.rows {
        let name = word(r.action);
        match counts.iter_mut().find(|(n, _)| *n == name) {
            Some((_, c)) => *c += 1,
            None => counts.push((name, 1)),
        }
    }
    let said = if p.rows.is_empty() {
        "nothing to do".to_string()
    } else if p.rows.iter().all(|r| r.action == Action::Kept) {
        format!("{} kept, nothing to change", p.rows.len())
    } else {
        counts
            .iter()
            .map(|(n, c)| format!("{c} {n}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut out = format!("{}{id}{when}: {said}\n", p.dir);
    if let Some(state) = p.state {
        out.push_str(&format!("state: {}\n", state.word()));
        for item in &p.items {
            out.push_str(&format!("  {item}\n"));
        }
    }
    for r in p.rows.iter().filter(|r| r.action != Action::Kept) {
        out.push_str(&line(r));
    }
    if let Some(profile) = &p.profile {
        // What the launch adds to the agent's start.
        let flags: Vec<String> = profile
            .flags
            .iter()
            .map(|f| crate::herdr::transport::shell_quote(f))
            .collect();
        out.push_str(&format!(
            "profile {}: start the agent with {}\n",
            profile.name,
            if flags.is_empty() {
                "no extra args".to_string()
            } else {
                flags.join(" ")
            }
        ));
        for (k, v) in &profile.env {
            out.push_str(&format!("  env {k}={}\n", crate::cli::printable(v)));
        }
    }
    out
}

fn line(r: &Row) -> String {
    let detail = r
        .detail
        .as_deref()
        .map(|d| format!(": {}", one_line(d)))
        .unwrap_or_default();
    let key = r
        .key
        .as_deref()
        .map(|k| format!(" {k}"))
        .unwrap_or_default();
    format!("  {} {}{key}{detail}\n", word(r.action), r.path)
}

fn word(a: Action) -> &'static str {
    match a {
        Action::Added => "added",
        Action::Kept => "kept",
        Action::Updated => "updated",
        Action::Removed => "removed",
        Action::Conflict => "conflict",
        Action::Linked => "linked",
        Action::ChangedByHand => "changed by hand",
        Action::Busy => "busy",
        Action::Failed => "failed",
        Action::Skipped => "skipped",
        Action::Replaced => "replaced",
    }
}
