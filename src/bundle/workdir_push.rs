//! Workdir sets placed from the head on a push machine (ssh or local).
//!
//! The head reads and checks the set, renders the profile it names as for
//! a task on that machine, and sends both over the bundle transport
//! (`push::run_plan`). `pastor workdir apply --local` there places them
//! with the same code as `place --local` (`workdir::place_with`), so the
//! vars of the machine (`{home}`, `{workdir}`) are filled where they hold.
//! The head keeps a record of what it placed (`workdirs.json`) and each set
//! it placed, so `check` can say whether a directory still holds it.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::apply::Action;
use super::keep::Reach;
use super::push::{self, Run};
use super::workdir::{self, Placed, Set, WORKDIR_INVALID, WORKDIR_UNSUPPORTED};
use super::workdir_keep::{self, ReadEntry, ReadOut, State};
use crate::cli::CliError;
use crate::config::flock::{Flock, MachineConfig};
use crate::config::{PastorConfig, Paths};
use crate::profile_render::{self, OnMachine, Render};
use crate::provision::run::Shell;
use crate::sync::Recover;

/// The plan file in a run directory.
pub const PLAN_FILE: &str = "workdir-plan.json";

/// The code of a place or remove that never got an answer from the machine.
pub const WORKDIR_FAILED: &str = "workdir_failed";

/// The code of a `check` of a directory the head placed no set in.
pub const WORKDIR_NOT_FOUND: &str = "workdir_not_found";

/// The head's record of the sets it placed, under its state directory.
const RECORD_FILE: &str = "workdirs.json";

/// Each set the head placed, by its hash, under its state directory.
const SETS_DIR: &str = "workdir-sets";

/// The most sets one read asks a machine about, so its answer stays under
/// what a machine's answer may be.
const READ_RUN: usize = 100;

/// What the head sends a machine to do in one directory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PlanOp {
    /// Place the set (`Set::to_doc`) with the profile the head rendered.
    Place {
        dir: String,
        set: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        render: Option<Render>,
    },
    /// Take out one set, or every set with no id.
    Remove {
        dir: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    /// Say how each of `entries` stands (`workdir_keep::read_local`), its
    /// set in `sets` by hash, after dropping the ledgers of the directories
    /// in `drop` that are still gone.
    Read {
        sets: BTreeMap<String, serde_json::Value>,
        entries: Vec<ReadEntry>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        drop: Vec<String>,
        /// Answer each set's dry-run rows too.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        rows: bool,
    },
}

/// What `workdir apply --local` answers: what it placed or removed, or the
/// error it refused the plan with, so the head passes on its code.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Answer {
    Placed(Placed),
    Read(ReadOut),
    Error { code: String, message: String },
}

/// On the machine: carry out the plan in `run_dir`.
pub fn apply_local(paths: &Paths, home: &Path, run_dir: &Path, dry_run: bool) -> Answer {
    let op = (|| -> anyhow::Result<PlanOp> {
        let text = std::fs::read_to_string(run_dir.join(PLAN_FILE))?;
        serde_json::from_str(&text)
            .map_err(|e| CliError::err(WORKDIR_INVALID, format!("the plan: {e}")))
    })();
    let done = match op {
        Err(err) => Err(err),
        Ok(PlanOp::Read {
            sets,
            entries,
            drop,
            rows,
        }) => {
            return Answer::Read(workdir_keep::read_local(
                paths, home, &sets, &entries, &drop, rows,
            ));
        }
        Ok(PlanOp::Place { dir, set, render }) => Set::from_doc(&set).and_then(|set| {
            workdir::place_with(paths, home, Path::new(&dir), &set, render.as_ref(), dry_run)
        }),
        Ok(PlanOp::Remove { dir, id }) => {
            workdir::remove(paths, home, Path::new(&dir), id.as_deref(), dry_run)
        }
    };
    match done {
        Ok(placed) => Answer::Placed(placed),
        Err(err) => {
            let (code, message) = match err.downcast_ref::<CliError>() {
                Some(e) => (e.code.clone(), e.message.clone()),
                None => ("runtime_error".to_string(), format!("{err:#}")),
            };
            Answer::Error { code, message }
        }
    }
}

/// One line of the head's record: a set it placed in a directory on a
/// machine, and how the last place there ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub machine: String,
    pub dir: String,
    pub id: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// The set's hash (`Set::hash`); its document is kept by it.
    pub set_hash: String,
    /// The SHA-256 of the profile as rendered for it, when it names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render_hash: Option<String>,
    /// When the set was last placed there, RFC 3339.
    pub placed_at: String,
    /// `ok`, or `failed` with `error`.
    pub result: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// How the set stood when last read or placed; none before either.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<State>,
    /// The first few items that were not ok then.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
    /// When the head last read or placed it, RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_at: Option<String>,
}

/// What the head last read of a machine as a whole.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineRead {
    /// The directories the machine holds a ledger for.
    pub ledgers: usize,
    pub read_at: String,
}

/// What a machine the head does not push to tells it of a place or remove
/// that ran there (`pastor workdir place|remove --local`), so `list` shows
/// pull and command machines too.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Reported {
    /// A set placed in `dir`: how it ended, as the head records its own.
    Placed {
        dir: String,
        id: String,
        kind: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
        set_hash: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        render_hash: Option<String>,
        /// RFC 3339, when it happened.
        at: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Sets taken out of `dir`.
    Removed {
        dir: String,
        ids: Vec<String>,
        at: String,
    },
}

/// How many reports one request may carry.
pub const MAX_REPORTS: usize = 100;

/// The longest error a report may carry; more is cut.
const MAX_ERROR: usize = 2000;

#[derive(Debug, Default, Serialize, Deserialize)]
struct Records {
    #[serde(default)]
    workdirs: Vec<Record>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    machines: BTreeMap<String, MachineRead>,
}

/// Raises an event for a place or remove: its kind, the machine, its
/// detail.
pub type Emit<'a> = &'a (dyn Fn(&'static str, &str, serde_json::Value) + Sync);

/// What the head reads to place a set, and where its events go.
pub struct Head<'a> {
    pub paths: &'a Paths,
    pub config: &'a PastorConfig,
    pub flock: &'a Flock,
    pub reach: &'a Reach,
    pub emit: Emit<'a>,
}

/// A machine the head can push a set to, and how.
struct Target<'a> {
    machine: &'a MachineConfig,
    shell: Arc<dyn Shell>,
    pastor: String,
}

/// The profile `set` names, rendered for its kind as for a task on
/// `machine` with `config` and `flock` (`Head::render`). `None` when it
/// names none.
pub fn render_for(
    config: &PastorConfig,
    flock: &Flock,
    set: &Set,
    machine: &str,
) -> anyhow::Result<Option<Render>> {
    let Some(name) = &set.profile else {
        return Ok(None);
    };
    let profile = config
        .profiles
        .resolve(name)
        .map_err(|e| CliError::err(e.code, e.message))?;
    let own = crate::profile_cli::own_profile(config, flock, machine)?;
    let on = OnMachine {
        name: machine,
        own: own.as_deref(),
    };
    profile_render::render(config, &profile, &set.kind, Some(on)).map(Some)
}

impl<'a> Head<'a> {
    /// Whether the head can push to `machine`.
    pub fn reaches(&self, machine: &str) -> bool {
        self.target(machine).is_ok()
    }

    fn target(&self, machine: &str) -> anyhow::Result<Target<'a>> {
        let Some(m) = self.flock.get(machine) else {
            return Err(CliError::err(
                "unknown_machine",
                format!("no machine {machine} in the flock"),
            ));
        };
        let here = format!(
            "run `pastor workdir place --local` on {machine} itself, where the directory is"
        );
        if m.pull {
            return Err(CliError::err(
                WORKDIR_UNSUPPORTED,
                format!("{machine} is a pull machine, which the head does not reach: {here}"),
            ));
        }
        let Some((shell, pastor)) = (self.reach)(m) else {
            return Err(CliError::err(
                WORKDIR_UNSUPPORTED,
                format!(
                    "{machine} is reached through a command bridge, which runs no shell: {here}"
                ),
            ));
        };
        Ok(Target {
            machine: m,
            shell,
            pastor,
        })
    }

    /// The profile `set` names, rendered for its kind as for a task on
    /// `machine`, so `unrestricted` goes only where the machine's own
    /// profile is, and an agy set only with the machine's.
    pub fn render(&self, set: &Set, machine: &str) -> anyhow::Result<Option<Render>> {
        render_for(self.config, self.flock, set, machine)
    }

    async fn push(&self, t: &Target<'_>, op: &PlanOp, dry_run: bool) -> anyhow::Result<Placed> {
        let flags = if dry_run { " --dry-run" } else { "" };
        match self.send(t, op, flags).await? {
            Answer::Placed(mut p) => {
                p.machine = Some(t.machine.name.clone());
                Ok(p)
            }
            Answer::Read(_) => Err(CliError::err(
                WORKDIR_FAILED,
                format!("{} answered a read to a place", t.machine.name),
            )),
            Answer::Error { code, message } => Err(CliError::err(&code, message)),
        }
    }

    /// Send `op` to the machine and read its answer.
    async fn send(&self, t: &Target<'_>, op: &PlanOp, flags: &str) -> anyhow::Result<Answer> {
        let run = Run {
            machine: &t.machine.name,
            pastor: &t.pastor,
            verb: "workdir apply",
            flags,
            what: "workdir sets",
        };
        let answer: Answer = push::run_plan(self.paths, t.shell.clone(), run, |dir| {
            std::fs::write(dir.join(PLAN_FILE), serde_json::to_vec(op)?)?;
            Ok(())
        })
        .await
        .map_err(|f| CliError::err(WORKDIR_FAILED, f.why))?;
        Ok(answer)
    }

    /// Place `set` in `dir` on `machine`. A real place records the result
    /// and raises `workdir.placed`, or `workdir.failed` when it did not go
    /// through (a problem row, or no answer); a dry run does neither.
    pub async fn place(
        &self,
        machine: &str,
        dir: &str,
        set: &serde_json::Value,
        dry_run: bool,
    ) -> anyhow::Result<Placed> {
        self.place_as(machine, dir, set, dry_run, false).await
    }

    /// Place the set stored workdir bundle `id` gives an agent of `kind` in
    /// `dir` on `machine` (`workdir_stored::stored_set`), as `place` places
    /// a set: its id is the bundle's, and it names no profile. The vars are
    /// `flock`'s, else those of the machine's first flock that names the
    /// bundle in `workdir_bundles`, else of its first flock. `None` when
    /// the bundle has nothing for that kind.
    pub async fn place_bundle(
        &self,
        machine: &str,
        dir: &str,
        id: &str,
        kind: &str,
        flock: Option<&str>,
        dry_run: bool,
    ) -> anyhow::Result<Option<Placed>> {
        use super::workdir_stored;
        if !workdir_stored::places(kind) {
            return Err(CliError::err(
                WORKDIR_UNSUPPORTED,
                format!("pastor places no workdir sets for {kind} agents yet"),
            ));
        }
        let Some(m) = self.flock.get(machine) else {
            return Err(CliError::err(
                "unknown_machine",
                format!("no machine {machine} in the flock"),
            ));
        };
        let flocks: Vec<&str> = self
            .flock
            .flocks_of(m)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        let flock = match flock {
            Some(f) => f.to_string(),
            None => flocks
                .iter()
                .find(|f| {
                    self.flock
                        .entry(f)
                        .is_some_and(|e| e.workdir_bundles.iter().any(|b| b == id))
                })
                .or(flocks.first())
                .map(|f| f.to_string())
                .unwrap_or_default(),
        };
        let set = workdir_stored::stored_set(self.paths, self.flock, id, kind, machine, &flock)
            .map_err(|why| CliError::err(WORKDIR_INVALID, why))?;
        match set {
            Some(set) => self.place(machine, dir, &set, dry_run).await.map(Some),
            None => Ok(None),
        }
    }

    /// `place`; with `auto`, the head placed it on its own, and its event
    /// says so.
    pub async fn place_as(
        &self,
        machine: &str,
        dir: &str,
        set: &serde_json::Value,
        dry_run: bool,
        auto: bool,
    ) -> anyhow::Result<Placed> {
        let dir = absolute(dir)?;
        let set = Set::from_doc(set)?;
        let t = self.target(machine)?;
        let render = self.render(&set, machine)?;
        let _one = lock(machine, &dir).await;
        let op = PlanOp::Place {
            dir: dir.clone(),
            set: set.to_doc(),
            render: render.clone(),
        };
        let pushed = self.push(&t, &op, dry_run).await;
        if dry_run {
            return pushed;
        }
        let mut detail = detail(&dir, &set.id, Some(&set.kind), set.profile.as_deref());
        if auto {
            detail["auto"] = true.into();
        }
        let failed = match &pushed {
            Ok(p) if p.ok => None,
            Ok(p) => Some(problems(p)),
            Err(e) => Some(error_text(e)),
        };
        let (state, items) = match &pushed {
            Ok(p) if p.ok => (Some(State::Ok), Vec::new()),
            Ok(p) => {
                let (state, items) = workdir_keep::fold(&p.rows);
                (Some(state), items)
            }
            Err(_) => (None, Vec::new()),
        };
        let now = Utc::now().to_rfc3339();
        let record = Record {
            machine: machine.to_string(),
            dir: dir.clone(),
            id: set.id.clone(),
            kind: set.kind.clone(),
            profile: set.profile.clone(),
            set_hash: set.hash(),
            render_hash: render.as_ref().map(hash_of),
            placed_at: now.clone(),
            result: if failed.is_some() { "failed" } else { "ok" }.into(),
            error: failed.clone(),
            state,
            items,
            read_at: Some(now),
        };
        save_placed(self.paths, record, &set)?;
        match failed {
            None => (self.emit)("workdir.placed", machine, detail),
            Some(why) => {
                let mut d = detail;
                d["error"] = why.into();
                (self.emit)("workdir.failed", machine, d)
            }
        }
        pushed
    }

    /// Record what `machine` reports of its own places and removes. It must
    /// be a machine the head does not reach: on one it does, the head
    /// places sets itself and a report would only second-guess its record.
    /// A real place raises `workdir.placed` or `workdir.failed`, a remove
    /// `workdir.removed` for each set the head had a record of, with the
    /// record written first. A
    /// report older than what the record says is left out.
    pub fn report(&self, machine: &str, reports: &[Reported]) -> anyhow::Result<()> {
        let Some(m) = self.flock.get(machine) else {
            return Err(CliError::err(
                "unknown_machine",
                format!("no machine {machine} in the flock"),
            ));
        };
        if !m.pull && (self.reach)(m).is_some() {
            return Err(CliError::err(
                WORKDIR_UNSUPPORTED,
                format!(
                    "the head places sets on {machine} itself; a report is for a pull or a command machine"
                ),
            ));
        }
        if reports.len() > MAX_REPORTS {
            return Err(CliError::err(
                WORKDIR_INVALID,
                format!(
                    "{} reports in one request, at most {MAX_REPORTS}",
                    reports.len()
                ),
            ));
        }
        // All checked before any is kept: a bad one refuses the request.
        let mut checked = Vec::new();
        for r in reports {
            checked.push(check_report(r)?);
        }
        for (r, at) in reports.iter().zip(checked) {
            match r {
                Reported::Placed {
                    dir,
                    id,
                    kind,
                    profile,
                    set_hash,
                    render_hash,
                    at: _,
                    ok,
                    error,
                } => {
                    let dir = absolute(dir)?;
                    let error = error
                        .as_ref()
                        .map(|e| e.chars().take(MAX_ERROR).collect::<String>());
                    let failed = (!ok).then(|| error.clone().unwrap_or_else(|| "failed".into()));
                    let kept = upsert_unless_newer(
                        self.paths,
                        Record {
                            machine: machine.to_string(),
                            dir: dir.clone(),
                            id: id.clone(),
                            kind: kind.clone(),
                            profile: profile.clone(),
                            set_hash: set_hash.clone(),
                            render_hash: render_hash.clone(),
                            placed_at: at.to_rfc3339(),
                            result: if failed.is_some() { "failed" } else { "ok" }.into(),
                            error: failed.clone(),
                            // The head cannot read a machine it does not reach.
                            state: None,
                            items: Vec::new(),
                            read_at: None,
                        },
                        at,
                    )?;
                    if !kept {
                        continue;
                    }
                    let d = detail(&dir, id, Some(kind), profile.as_deref());
                    match failed {
                        None => (self.emit)("workdir.placed", machine, d),
                        Some(why) => {
                            let mut d = d;
                            d["error"] = why.into();
                            (self.emit)("workdir.failed", machine, d)
                        }
                    }
                }
                Reported::Removed { dir, ids, at: _ } => {
                    let dir = absolute(dir)?;
                    // A set placed again since the removal stays.
                    let dropped = drop_records_unless_newer(self.paths, machine, &dir, ids, at)?;
                    // A removal sent again, or answered late, finds no record:
                    // the first one raised the event, so this one is quiet.
                    for r in &dropped {
                        let d = detail(&dir, &r.id, Some(&r.kind), r.profile.as_deref());
                        (self.emit)("workdir.removed", machine, d);
                    }
                }
            }
        }
        Ok(())
    }

    /// Take the set `id` (every set, with none) out of `dir` on `machine`.
    /// A real remove that went through drops the head's record of it and
    /// raises `workdir.removed` for each set it took out.
    pub async fn remove(
        &self,
        machine: &str,
        dir: &str,
        id: Option<&str>,
        dry_run: bool,
    ) -> anyhow::Result<Placed> {
        let dir = absolute(dir)?;
        let t = self.target(machine)?;
        let _one = lock(machine, &dir).await;
        let op = PlanOp::Remove {
            dir: dir.clone(),
            id: id.map(str::to_string),
        };
        let p = self.push(&t, &op, dry_run).await?;
        if dry_run {
            return Ok(p);
        }
        // The machine takes out each set it can; one with a problem row
        // stays, in its ledger and in the record.
        let recorded: Vec<String> = read(self.paths)?
            .workdirs
            .into_iter()
            .filter(|r| r.machine == machine && r.dir == dir && id.is_none_or(|i| i == r.id))
            .map(|r| r.id)
            .collect();
        let out = taken_out(&p, &recorded);
        let dropped = drop_records(self.paths, machine, &dir, &out)?;
        for set in &out {
            let r = dropped.iter().find(|r| r.id == *set);
            let d = detail(
                &dir,
                set,
                r.map(|r| r.kind.as_str()),
                r.and_then(|r| r.profile.as_deref()),
            );
            (self.emit)("workdir.removed", machine, d);
        }
        Ok(p)
    }

    /// Each set the head placed in `dir` on `machine` (only `id`, when
    /// given), read on the machine: placed again as a dry run with the
    /// profile as it renders now, in one run. Each answer carries the set's
    /// state, and is `ok` only when the state is and nothing would change.
    pub async fn check(
        &self,
        machine: &str,
        dir: &str,
        id: Option<&str>,
    ) -> anyhow::Result<Vec<Placed>> {
        let dir = absolute(dir)?;
        let records: Vec<Record> = read(self.paths)?
            .workdirs
            .into_iter()
            .filter(|r| r.machine == machine && r.dir == dir && id.is_none_or(|i| i == r.id))
            .collect();
        if records.is_empty() {
            let set = id.map(|i| format!("set {i}")).unwrap_or("set".into());
            return Err(CliError::err(
                WORKDIR_NOT_FOUND,
                format!("the head placed no {set} in {dir} on {machine}"),
            ));
        }
        let out = self.read(machine, &records, &[], true).await?;
        Ok(out
            .sets
            .into_iter()
            .map(|s| {
                let mut p = s
                    .placed
                    .unwrap_or_else(|| Placed::new(Path::new(&s.dir), Some(&s.id), true, vec![]));
                p.machine = Some(machine.to_string());
                p.dry_run = true;
                p.ok = s.state == State::Ok
                    && p.rows
                        .iter()
                        .all(|row| matches!(row.action, Action::Kept | Action::Skipped));
                p.state = Some(s.state);
                p.items = s.items;
                p
            })
            .collect())
    }

    /// Read how each of `records`, all on `machine`, stands there, after
    /// dropping the ledgers of the directories in `drop` that are still
    /// gone. A set whose profile does not render now, or whose kept file
    /// is gone, goes with no document: the machine reads only whether its
    /// directory still holds it, and answers `stale` with the head's
    /// reason when it does. The sets go in runs of `READ_RUN`, so the
    /// machine's answer stays small.
    pub async fn read(
        &self,
        machine: &str,
        records: &[Record],
        drop: &[String],
        rows: bool,
    ) -> anyhow::Result<ReadOut> {
        let t = self.target(machine)?;
        let mut out = ReadOut::default();
        // Each set with its document, or with why the head has none to
        // send: its directory is still read for being gone or emptied.
        let mut asked: Vec<(ReadEntry, Option<serde_json::Value>)> = Vec::new();
        for r in records {
            let entry = |render, stale| ReadEntry {
                dir: r.dir.clone(),
                id: r.id.clone(),
                set_hash: r.set_hash.clone(),
                render,
                stale,
            };
            let set = match kept_set(self.paths, &r.set_hash) {
                Ok(set) => set,
                Err(e) => {
                    asked.push((entry(None, Some(error_text(&e))), None));
                    continue;
                }
            };
            match self.render(&set, machine) {
                Ok(render) => asked.push((entry(render, None), Some(set.to_doc()))),
                Err(e) => {
                    let why = format!(
                        "profile {}: {}",
                        set.profile.as_deref().unwrap_or_default(),
                        error_text(&e)
                    );
                    asked.push((entry(None, Some(why)), None));
                }
            }
        }
        let mut drop = drop.to_vec();
        let mut first = true;
        for run in asked.chunks(READ_RUN) {
            let mut sets = BTreeMap::new();
            for (e, doc) in run {
                if let Some(doc) = doc {
                    sets.entry(e.set_hash.clone())
                        .or_insert_with(|| doc.clone());
                }
            }
            let op = PlanOp::Read {
                sets,
                entries: run.iter().map(|(e, _)| e.clone()).collect(),
                drop: std::mem::take(&mut drop),
                rows,
            };
            first = false;
            match self.send(&t, &op, "").await? {
                Answer::Read(got) => {
                    out.sets.extend(got.sets);
                    out.ledgers = got.ledgers;
                    out.dropped.extend(got.dropped);
                }
                Answer::Error { code, message } => return Err(CliError::err(&code, message)),
                Answer::Placed(_) => {
                    return Err(CliError::err(
                        WORKDIR_FAILED,
                        format!("{machine} answered a place to a read"),
                    ));
                }
            }
        }
        if first && !drop.is_empty() {
            // Nothing to ask about, but directories to drop.
            let op = PlanOp::Read {
                sets: BTreeMap::new(),
                entries: vec![],
                drop,
                rows,
            };
            if let Answer::Read(got) = self.send(&t, &op, "").await? {
                out.ledgers = got.ledgers;
                out.dropped = got.dropped;
            }
        }
        Ok(out)
    }
}

/// A report checked: its time, parsed. A directory is made absolute where
/// the report is kept.
fn check_report(r: &Reported) -> anyhow::Result<chrono::DateTime<Utc>> {
    let bad = |what: String| CliError::err(WORKDIR_INVALID, format!("a report: {what}"));
    let (dir, at) = match r {
        Reported::Placed {
            dir,
            id,
            kind,
            profile,
            set_hash,
            at,
            ..
        } => {
            if !workdir::is_name(id) {
                return Err(bad(format!("id {id:?} is not a set id")));
            }
            if !workdir::KINDS.contains(&kind.as_str()) {
                return Err(bad(format!("kind {kind:?} is not a harness kind")));
            }
            if profile
                .as_ref()
                .is_some_and(|p| p.is_empty() || p.len() > 200 || p.chars().any(char::is_control))
            {
                return Err(bad("profile: a profile name".into()));
            }
            if set_hash.len() != 64 || !set_hash.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(bad("set_hash: a SHA-256".into()));
            }
            (dir, at)
        }
        Reported::Removed { dir, ids, at } => {
            if let Some(id) = ids.iter().find(|id| !workdir::is_name(id)) {
                return Err(bad(format!("id {id:?} is not a set id")));
            }
            if ids.len() > 1000 {
                return Err(bad("too many ids".into()));
            }
            (dir, at)
        }
    };
    absolute(dir)?;
    chrono::DateTime::parse_from_rfc3339(at)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| bad(format!("at {at:?}: {e}")))
}

/// Whether the record time `placed_at` is later than `than`.
fn newer(placed_at: &str, than: chrono::DateTime<Utc>) -> bool {
    chrono::DateTime::parse_from_rfc3339(placed_at).is_ok_and(|t| t > than)
}

/// Whether the record time `placed_at` is earlier than `than`. One that
/// does not read is, so a report replaces it.
fn older(placed_at: &str, than: chrono::DateTime<Utc>) -> bool {
    match chrono::DateTime::parse_from_rfc3339(placed_at) {
        Ok(t) => t < than,
        Err(_) => true,
    }
}

/// What the head placed, on `machine` only when given, as its record says.
pub fn list(paths: &Paths, machine: Option<&str>) -> anyhow::Result<Vec<Record>> {
    Ok(read(paths)?
        .workdirs
        .into_iter()
        .filter(|r| machine.is_none_or(|m| m == r.machine))
        .collect())
}

/// A directory as the machine takes it: absolute, since the head cannot
/// tell what a relative one would be relative to there. It is written one
/// way, with no trailing `/`, `.` or repeated `/`, so the record has one
/// line for it however it was typed. A `..` stays: only the machine knows
/// what it leads to.
fn absolute(dir: &str) -> anyhow::Result<String> {
    let path = Path::new(dir);
    if path.is_absolute() {
        Ok(path.components().collect::<PathBuf>().display().to_string())
    } else {
        Err(CliError::err(
            WORKDIR_INVALID,
            format!("{dir:?}: give the directory as an absolute path on the machine"),
        ))
    }
}

/// The sets a remove took out of the directory: of those the head recorded
/// there and those the answer names, each with no problem row. A recorded
/// set with no row was not on the machine any more. A row of no set (the
/// exclude file's) counts for none.
pub fn taken_out(p: &Placed, recorded: &[String]) -> Vec<String> {
    let mut sets: Vec<String> = p
        .rows
        .iter()
        .filter(|r| !r.bundle.is_empty())
        .map(|r| r.bundle.clone())
        .chain(recorded.iter().cloned())
        .collect();
    sets.sort();
    sets.dedup();
    sets.retain(|s| {
        !p.rows
            .iter()
            .any(|r| r.bundle == *s && r.action.is_problem())
    });
    sets
}

fn detail(dir: &str, id: &str, kind: Option<&str>, profile: Option<&str>) -> serde_json::Value {
    serde_json::json!({"dir": dir, "id": id, "kind": kind, "profile": profile})
}

/// The rows that kept a place from going through, as one line.
pub fn problems(p: &Placed) -> String {
    p.rows
        .iter()
        .filter(|r| r.action.is_problem())
        .map(|r| {
            let action = serde_json::to_value(r.action)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default();
            format!("{action} {}", r.path)
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn error_text(e: &anyhow::Error) -> String {
    match e.downcast_ref::<CliError>() {
        Some(c) => c.message.clone(),
        None => format!("{e:#}"),
    }
}

/// The SHA-256 of a profile as rendered, as the record keeps it.
pub fn hash_of(render: &Render) -> String {
    hex(&Sha256::digest(
        serde_json::to_string(render)
            .expect("a render serializes")
            .as_bytes(),
    ))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// One place or remove at a time in a directory; others go on beside it.
async fn lock(machine: &str, dir: &str) -> tokio::sync::OwnedMutexGuard<()> {
    static LOCKS: Mutex<Option<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = Mutex::new(None);
    let one = LOCKS
        .lock()
        .recover()
        .get_or_insert_with(HashMap::new)
        .entry(format!("{machine}\0{dir}"))
        .or_default()
        .clone();
    one.lock_owned().await
}

/// The record file is read and written by one request at a time.
static RECORD: Mutex<()> = Mutex::new(());

fn record_file(paths: &Paths) -> PathBuf {
    paths.state_dir.join(RECORD_FILE)
}

fn read(paths: &Paths) -> anyhow::Result<Records> {
    match std::fs::read_to_string(record_file(paths)) {
        Ok(text) => Ok(serde_json::from_str(&text)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Records::default()),
        Err(e) => Err(e.into()),
    }
}

fn write(paths: &Paths, records: &Records) -> anyhow::Result<()> {
    let file = record_file(paths);
    if let Some(dir) = file.parent() {
        crate::config::create_private_dir(dir)?;
    }
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(records)?)?;
    std::fs::rename(&tmp, &file)?;
    Ok(())
}

/// Put `record` in place of the one for its machine, directory and set.
/// The caller holds `RECORD`.
fn upsert_locked(paths: &Paths, record: Record) -> anyhow::Result<()> {
    let mut all = read(paths)?;
    all.workdirs
        .retain(|r| !(r.machine == record.machine && r.dir == record.dir && r.id == record.id));
    all.workdirs.push(record);
    write(paths, &all)
}

/// Take out the records of the sets `ids` in `dir` on `machine`, and answer
/// them.
fn drop_records(
    paths: &Paths,
    machine: &str,
    dir: &str,
    ids: &[String],
) -> anyhow::Result<Vec<Record>> {
    let _one = RECORD.lock().recover();
    drop_locked(paths, machine, dir, ids)
}

/// `drop_records`, with the record file already locked.
fn drop_locked(
    paths: &Paths,
    machine: &str,
    dir: &str,
    ids: &[String],
) -> anyhow::Result<Vec<Record>> {
    let mut all = read(paths)?;
    let (gone, kept): (Vec<Record>, Vec<Record>) = all
        .workdirs
        .into_iter()
        .partition(|r| r.machine == machine && r.dir == dir && ids.contains(&r.id));
    all.workdirs = kept;
    if !gone.is_empty() {
        write(paths, &all)?;
    }
    Ok(gone)
}

/// `upsert`, unless the record there is as new as `at` or newer: a report
/// delivered twice, or late, changes nothing. The read and the write are
/// one step under the record's lock. Answers whether it wrote.
fn upsert_unless_newer(
    paths: &Paths,
    record: Record,
    at: chrono::DateTime<Utc>,
) -> anyhow::Result<bool> {
    let _one = RECORD.lock().recover();
    let mut all = read(paths)?;
    let has_newer = all.workdirs.iter().any(|r| {
        r.machine == record.machine
            && r.dir == record.dir
            && r.id == record.id
            && !older(&r.placed_at, at)
    });
    if has_newer {
        return Ok(false);
    }
    all.workdirs
        .retain(|r| !(r.machine == record.machine && r.dir == record.dir && r.id == record.id));
    all.workdirs.push(record);
    write(paths, &all)?;
    Ok(true)
}

/// Take out the records of the sets `ids` that a removal at `at` is not
/// older than, in one step under the record's lock. Answers the records
/// taken out.
fn drop_records_unless_newer(
    paths: &Paths,
    machine: &str,
    dir: &str,
    ids: &[String],
    at: chrono::DateTime<Utc>,
) -> anyhow::Result<Vec<Record>> {
    let _one = RECORD.lock().recover();
    let all = read(paths)?;
    let out: Vec<String> = ids
        .iter()
        .filter(|id| {
            !all.workdirs.iter().any(|r| {
                r.machine == machine && r.dir == dir && r.id == **id && newer(&r.placed_at, at)
            })
        })
        .cloned()
        .collect();
    drop_locked(paths, machine, dir, &out)
}

fn set_file(paths: &Paths, hash: &str) -> PathBuf {
    paths.state_dir.join(SETS_DIR).join(format!("{hash}.json"))
}

/// Keep the set of `record` and put `record` in place of the one for its
/// machine, directory and set, under the record's lock: two first places of
/// one set do not race on its file, and a prune never takes a set between
/// the two writes.
fn save_placed(paths: &Paths, record: Record, set: &Set) -> anyhow::Result<()> {
    let _one = RECORD.lock().recover();
    keep_set(paths, &record.set_hash, set)?;
    upsert_locked(paths, record)
}

/// Keep `set` by its hash, once. The caller holds `RECORD`.
fn keep_set(paths: &Paths, hash: &str, set: &Set) -> anyhow::Result<()> {
    let file = set_file(paths, hash);
    if file.exists() {
        return Ok(());
    }
    crate::config::create_private_dir(file.parent().expect("under the state dir"))?;
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&set.to_doc())?)?;
    std::fs::rename(&tmp, &file)?;
    Ok(())
}

/// The set the head kept by `hash`.
pub fn kept_set(paths: &Paths, hash: &str) -> anyhow::Result<Set> {
    let text = std::fs::read_to_string(set_file(paths, hash)).map_err(|e| {
        CliError::err(
            WORKDIR_NOT_FOUND,
            format!("the set the head placed ({hash}) is not kept any more: {e}; place it again"),
        )
    })?;
    let doc: serde_json::Value = serde_json::from_str(&text)?;
    Set::from_doc(&doc)
}

/// Say a recorded set is `stale` with `item`, unless the record says so
/// already or is of another version now. A set read worse than stale
/// (`drift`, `not adopted`, `conflict`) keeps its state and gets the item
/// beside it: a lower state would hide what the read found, and the next
/// read would raise `workdir.drift` for it again. Whether it wrote.
pub fn note_stale(paths: &Paths, of: &Record, item: &str) -> anyhow::Result<bool> {
    let _one = RECORD.lock().recover();
    let mut all = read(paths)?;
    let Some(r) = all.workdirs.iter_mut().find(|r| same(r, of)) else {
        return Ok(false);
    };
    let at_least = r.state.is_some_and(|s| s >= State::Stale);
    if at_least && r.items.iter().any(|i| i == item) {
        return Ok(false);
    }
    // What a read found stays beside it: another version placed on the
    // machine is why the keeper does not place it again.
    if !at_least {
        r.items.clear();
        r.state = Some(State::Stale);
    }
    r.items.push(item.to_string());
    write(paths, &all)?;
    Ok(true)
}

/// Whether `r` is the record `of` was: one machine, directory and set, in
/// the version placed then.
fn same(r: &Record, of: &Record) -> bool {
    r.machine == of.machine && r.dir == of.dir && r.id == of.id && r.set_hash == of.set_hash
}

/// An event to raise once the record is written: its kind and detail.
pub type Raise = (&'static str, serde_json::Value);

/// Write what a read of `machine` found for the records `asked`:
/// - each set's state, items and when;
/// - a set in a directory the machine dropped, or one its ledger no longer
///   names, leaves the record (`workdir.removed`, with the reason);
/// - a set that was `ok` or `stale` and now is `drift`, `conflict` or `not
///   adopted` raises `workdir.drift`;
/// - the machine's ledger count;
/// - the kept sets no record names any more go.
///
/// A record placed again since it was asked about is left as it is.
pub fn note_read(
    paths: &Paths,
    machine: &str,
    asked: &[Record],
    out: &ReadOut,
) -> anyhow::Result<Vec<Raise>> {
    let _one = RECORD.lock().recover();
    let mut all = read(paths)?;
    let now = Utc::now().to_rfc3339();
    let mut raise = Vec::new();
    let mut gone: Vec<(String, String)> = Vec::new();
    for s in &out.sets {
        let Some(of) = asked.iter().find(|r| r.dir == s.dir && r.id == s.id) else {
            continue;
        };
        let Some(r) = all.workdirs.iter_mut().find(|r| same(r, of)) else {
            continue;
        };
        let reason = match s.state {
            State::Gone if out.dropped.contains(&s.dir) => Some("gone"),
            State::Missing => Some("removed on the machine"),
            _ => None,
        };
        if let Some(reason) = reason {
            let mut d = detail(&r.dir, &r.id, Some(&r.kind), r.profile.as_deref());
            d["reason"] = reason.into();
            raise.push(("workdir.removed", d));
            gone.push((r.dir.clone(), r.id.clone()));
            continue;
        }
        let was = r.state;
        r.state = Some(s.state);
        r.items = s.items.clone();
        r.read_at = Some(now.clone());
        if matches!(was, Some(State::Ok | State::Stale))
            && matches!(s.state, State::Drift | State::Conflict | State::NotAdopted)
        {
            let mut d = detail(&r.dir, &r.id, Some(&r.kind), r.profile.as_deref());
            d["state"] = s.state.word().into();
            d["items"] = s.items.clone().into();
            raise.push(("workdir.drift", d));
        }
    }
    // A directory dropped with no set asked about in it this time.
    for r in all.workdirs.iter() {
        if r.machine == machine
            && out.dropped.contains(&r.dir)
            && !gone.iter().any(|(d, i)| *d == r.dir && *i == r.id)
        {
            let mut d = detail(&r.dir, &r.id, Some(&r.kind), r.profile.as_deref());
            d["reason"] = "gone".into();
            raise.push(("workdir.removed", d));
            gone.push((r.dir.clone(), r.id.clone()));
        }
    }
    all.workdirs
        .retain(|r| !(r.machine == machine && gone.iter().any(|(d, i)| *d == r.dir && *i == r.id)));
    all.machines.insert(
        machine.to_string(),
        MachineRead {
            ledgers: out.ledgers,
            read_at: now,
        },
    );
    write(paths, &all)?;
    prune_sets(paths, &all);
    Ok(raise)
}

/// What the head last read of `machine` as a whole.
pub fn machine_read(paths: &Paths, machine: &str) -> Option<MachineRead> {
    read(paths).ok()?.machines.remove(machine)
}

/// Take out the kept sets no record names. The caller holds `RECORD`.
fn prune_sets(paths: &Paths, all: &Records) {
    let Ok(files) = std::fs::read_dir(paths.state_dir.join(SETS_DIR)) else {
        return;
    };
    for f in files.filter_map(Result::ok) {
        let path = f.path();
        let Some(hash) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".json"))
        else {
            continue;
        };
        if !all.workdirs.iter().any(|r| r.set_hash == hash) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::provision::run::testing::Fake;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    pub(crate) const SET: &str = r#"{
        "id": "reviewer",
        "kind": "claude",
        "files": [{"path": ".claude/agents/checker.md", "content": "Check.\n"}]
    }"#;

    const FLOCK: &str = "[[machine]]\nname = \"m\"\nlocal = true\n\n\
        [[machine]]\nname = \"p\"\npull = true\n\n\
        [[machine]]\nname = \"c\"\ncommand = [\"true\"]\n";

    /// A head whose push machine `m` runs a script in place of pastor: it
    /// keeps each plan it is sent and its args, and answers with
    /// `answer-real.json`, or `answer-dry.json` for a dry run.
    pub(crate) struct Fixture {
        pub(crate) tmp: tempfile::TempDir,
        pub(crate) paths: Paths,
        config: PastorConfig,
        pub(crate) flock: Flock,
        pub(crate) reach: Reach,
        events: StdMutex<Vec<(String, String, serde_json::Value)>>,
        /// How many records there were as each event was raised.
        records_at: StdMutex<Vec<usize>>,
    }

    impl Fixture {
        pub(crate) fn new() -> Fixture {
            let tmp = tempfile::tempdir().unwrap();
            let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
            let home = tmp.path().join("h");
            std::fs::create_dir_all(&home).unwrap();
            let script = tmp.path().join("pastor");
            std::fs::write(
                &script,
                "#!/bin/sh\nd=$(dirname \"$0\")\necho \"$*\" >> \"$d/calls\"\ncat \"$4/workdir-plan.json\" > \"$d/plan.json\"\nif [ -f \"$d/delay\" ]; then sleep \"$(cat \"$d/delay\")\"; fi\nif grep -q '\"op\":\"read\"' \"$d/plan.json\"; then cat \"$d/answer-read.json\"; elif echo \"$*\" | grep -q dry-run; then cat \"$d/answer-dry.json\"; else cat \"$d/answer-real.json\"; fi\n",
            )
            .unwrap();
            std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .unwrap();
            let reach: Reach = Arc::new(move |m: &MachineConfig| {
                (m.name != "c").then(|| {
                    (
                        Arc::new(Fake::new(&home, Some("/usr/bin:/bin".into()))) as Arc<dyn Shell>,
                        script.to_string_lossy().into_owned(),
                    )
                })
            });
            let f = Fixture {
                paths,
                config: PastorConfig::default(),
                flock: Flock::parse(Path::new("flock.toml"), FLOCK).unwrap(),
                reach,
                events: StdMutex::new(vec![]),
                records_at: StdMutex::new(vec![]),
                tmp,
            };
            f.answer("real", &[("added", ".claude/agents/checker.md")]);
            f.answer("dry", &[("kept", ".claude/agents/checker.md")]);
            f.answer_read(&[("/w", "reviewer", "ok")], &[]);
            f
        }

        /// What the machine answers next, as rows of action and path.
        pub(crate) fn answer(&self, which: &str, rows: &[(&str, &str)]) {
            let rows: Vec<(&str, &str, &str)> =
                rows.iter().map(|(a, p)| ("reviewer", *a, *p)).collect();
            self.answer_sets(which, &rows);
        }

        /// What the machine answers next, as rows of set, action and path.
        pub(crate) fn answer_sets(&self, which: &str, rows: &[(&str, &str, &str)]) {
            let rows: Vec<serde_json::Value> = rows
                .iter()
                .map(|(id, action, path)| serde_json::json!({"bundle": id, "path": path, "action": action}))
                .collect();
            let problem = rows.iter().any(|r| {
                !["added", "kept", "updated", "removed", "skipped"]
                    .contains(&r["action"].as_str().unwrap())
            });
            let placed = serde_json::json!({"placed": {
                "target": "workdir", "dir": "/w", "id": "reviewer",
                "dry_run": which == "dry", "ok": !problem, "rows": rows,
            }});
            std::fs::write(
                self.tmp.path().join(format!("answer-{which}.json")),
                placed.to_string(),
            )
            .unwrap();
        }

        /// What the machine answers a read next: each set by directory,
        /// id and state, with a `kept` row; and the directories it dropped.
        fn answer_read(&self, sets: &[(&str, &str, &str)], dropped: &[&str]) {
            let sets: Vec<(&str, &str, &str, &[&str])> =
                sets.iter().map(|(d, i, s)| (*d, *i, *s, &[][..])).collect();
            self.answer_read_items(&sets, dropped);
        }

        /// `answer_read`, with each set's items.
        fn answer_read_items(&self, sets: &[(&str, &str, &str, &[&str])], dropped: &[&str]) {
            let sets: Vec<serde_json::Value> = sets
                .iter()
                .map(|(dir, id, state, items)| {
                    serde_json::json!({"dir": dir, "id": id, "state": state, "items": items,
                        "placed": {"target": "workdir", "dir": dir, "id": id, "dry_run": true,
                            "ok": true, "rows": [{"bundle": id, "path": ".claude/agents/checker.md", "action": "kept"}]}})
                })
                .collect();
            let out = serde_json::json!({"read": {"sets": sets, "ledgers": 1, "dropped": dropped}});
            std::fs::write(self.tmp.path().join("answer-read.json"), out.to_string()).unwrap();
        }

        /// How long the machine takes over each run from now on, in
        /// seconds; `None` answers at once.
        pub(crate) fn delay(&self, secs: Option<&str>) {
            let path = self.tmp.path().join("delay");
            match secs {
                Some(secs) => std::fs::write(path, secs).unwrap(),
                None => std::fs::remove_file(path).unwrap(),
            }
        }

        pub(crate) fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.tmp.path().join("calls"))
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }

        fn plan(&self) -> serde_json::Value {
            serde_json::from_str(
                &std::fs::read_to_string(self.tmp.path().join("plan.json")).unwrap(),
            )
            .unwrap()
        }

        fn kinds(&self) -> Vec<String> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .map(|e| e.0.clone())
                .collect()
        }

        fn run<T>(&self, f: impl AsyncFnOnce(&Head<'_>) -> T) -> T {
            let emit = |kind: &'static str, machine: &str, detail: serde_json::Value| {
                self.records_at
                    .lock()
                    .unwrap()
                    .push(list(&self.paths, None).map_or(0, |r| r.len()));
                self.events
                    .lock()
                    .unwrap()
                    .push((kind.to_string(), machine.to_string(), detail));
            };
            let head = Head {
                paths: &self.paths,
                config: &self.config,
                flock: &self.flock,
                reach: &self.reach,
                emit: &emit,
            };
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(f(&head))
        }
    }

    fn set() -> serde_json::Value {
        serde_json::from_str(SET).unwrap()
    }

    fn code(e: anyhow::Error) -> String {
        e.downcast::<CliError>().map(|c| c.code).unwrap_or_default()
    }

    /// A stored workdir bundle is placed as the set it gives the kind: the
    /// head builds it from the newest version kept, with the vars of the
    /// machine's flock that names it (or the flock asked for), records it
    /// under the bundle's id and raises `workdir.placed`. A kind it has
    /// nothing for places nothing; one pastor places no sets for, a bundle
    /// with no version and a flock short of a var are refused.
    #[test]
    fn a_stored_bundle_is_placed_as_its_set() {
        let mut f = Fixture::new();
        f.flock = Flock::parse(
            Path::new("flock.toml"),
            "[[flock]]\nname = \"ops\"\ndefault = true\nmachines = { m = 1 }\n\
             [[flock]]\nname = \"docs\"\nmachines = { m = 1 }\nworkdir_bundles = [\"repo-notes\"]\n\
             bundle_vars = { ticket_url = \"https://t.example.com\" }\n\
             [[machine]]\nname = \"m\"\nlocal = true\n",
        )
        .unwrap();
        let src = f.tmp.path().join("src");
        crate::bundle::manifest::tests::write_workdir_example(&src);
        crate::bundle::store::add(&f.paths, &crate::bundle::store::Source::Dir(src)).unwrap();
        f.answer_sets(
            "real",
            &[("repo-notes", "added", ".claude/agents/checker.md")],
        );

        let p = f
            .run(async |h| {
                h.place_bundle("m", "/w", "repo-notes", "claude", None, false)
                    .await
            })
            .unwrap()
            .unwrap();
        assert!(p.ok);
        let plan = f.plan();
        assert_eq!(plan["set"]["id"], "repo-notes");
        assert_eq!(
            plan["set"]["vars"]["flock"], "docs",
            "the flock that names it"
        );
        assert_eq!(plan["set"]["vars"]["ticket_url"], "https://t.example.com");
        assert!(plan["set"].get("profile").is_none());
        let records = list(&f.paths, Some("m")).unwrap();
        assert_eq!(records[0].id, "repo-notes");
        assert_eq!(f.kinds(), ["workdir.placed"]);

        let code_of = |kind: &str, flock: Option<&str>, id: &str| {
            let (kind, flock, id) = (kind.to_string(), flock.map(str::to_string), id.to_string());
            f.run(async move |h| {
                h.place_bundle("m", "/w", &id, &kind, flock.as_deref(), true)
                    .await
            })
            .map(|p| p.map(|_| "placed".to_string()).unwrap_or("nothing".into()))
            .unwrap_or_else(code)
        };
        assert_eq!(code_of("codex", None, "repo-notes"), WORKDIR_UNSUPPORTED);
        assert_eq!(
            code_of("claude", Some("ops"), "repo-notes"),
            WORKDIR_INVALID
        );
        assert_eq!(code_of("claude", None, "absent"), WORKDIR_INVALID);
        assert_eq!(code_of("agy", None, "repo-notes"), "placed");
    }

    /// A place sends the machine the set and runs `workdir apply` there; a
    /// real one records what it placed, keeps the set, and raises
    /// `workdir.placed`. A dry run records and raises nothing.
    #[test]
    fn a_place_goes_to_the_machine_and_a_real_one_is_recorded() {
        let f = Fixture::new();
        let dry = f
            .run(async |h| h.place("m", "/w", &set(), true).await)
            .unwrap();
        assert_eq!(dry.machine.as_deref(), Some("m"));
        assert!(list(&f.paths, None).unwrap().is_empty());
        assert!(f.kinds().is_empty());
        assert!(
            f.calls()[0].starts_with("workdir apply --local "),
            "{:?}",
            f.calls()
        );
        assert!(
            f.calls()[0].ends_with(" --json --dry-run"),
            "{:?}",
            f.calls()
        );
        let p = f
            .run(async |h| h.place("m", "/w", &set(), false).await)
            .unwrap();
        assert!(p.ok);
        let plan = f.plan();
        assert_eq!(plan["op"], "place");
        assert_eq!(plan["dir"], "/w");
        let sent = Set::parse(&set().to_string(), Some("json")).unwrap();
        assert_eq!(plan["set"], sent.to_doc());
        assert!(plan.get("render").is_none(), "no profile, no render");
        let records = list(&f.paths, Some("m")).unwrap();
        assert_eq!(records.len(), 1);
        let r = &records[0];
        assert_eq!(
            (r.dir.as_str(), r.id.as_str(), r.kind.as_str()),
            ("/w", "reviewer", "claude")
        );
        assert_eq!((r.result.as_str(), r.error.as_deref()), ("ok", None));
        assert_eq!(r.set_hash, sent.hash());
        assert_eq!(kept_set(&f.paths, &r.set_hash).unwrap(), sent);
        assert_eq!(f.kinds(), ["workdir.placed"]);
        let e = f.events.lock().unwrap()[0].clone();
        assert_eq!(e.1, "m");
        assert_eq!(
            e.2,
            serde_json::json!({"dir": "/w", "id": "reviewer", "kind": "claude", "profile": null})
        );
        // Placed again: one record still.
        f.run(async |h| h.place("m", "/w", &set(), false).await)
            .unwrap();
        assert_eq!(list(&f.paths, None).unwrap().len(), 1);
    }

    /// A place with a problem row, or one the machine gave no answer to, is
    /// recorded as failed and raises `workdir.failed`; the second is an
    /// error, `workdir_failed`.
    #[test]
    fn a_place_that_does_not_go_through_is_recorded_as_failed() {
        let f = Fixture::new();
        f.answer("real", &[("conflict", ".claude/agents/checker.md")]);
        let p = f
            .run(async |h| h.place("m", "/w", &set(), false).await)
            .unwrap();
        assert!(!p.ok);
        let r = list(&f.paths, None).unwrap().remove(0);
        assert_eq!(r.result, "failed");
        assert_eq!(
            r.error.as_deref(),
            Some("conflict .claude/agents/checker.md")
        );
        assert_eq!(f.kinds(), ["workdir.failed"]);
        std::fs::write(f.tmp.path().join("answer-real.json"), "").unwrap();
        let err = f
            .run(async |h| h.place("m", "/w", &set(), false).await)
            .unwrap_err();
        assert_eq!(code(err), WORKDIR_FAILED);
        assert_eq!(f.kinds(), ["workdir.failed", "workdir.failed"]);
        assert_eq!(list(&f.paths, None).unwrap()[0].result, "failed");
    }

    /// The machine's own refusal comes back with its code.
    #[test]
    fn the_machine_s_refusal_keeps_its_code() {
        let f = Fixture::new();
        std::fs::write(
            f.tmp.path().join("answer-real.json"),
            r#"{"error": {"code": "workdir_invalid", "message": "files: no"}}"#,
        )
        .unwrap();
        let err = f
            .run(async |h| h.place("m", "/w", &set(), false).await)
            .unwrap_err();
        assert_eq!(code(err), WORKDIR_INVALID);
    }

    /// Nothing is pushed for a machine the head cannot reach with a shell, a
    /// relative directory or a set that does not check.
    #[test]
    fn what_the_head_cannot_push_is_refused_before_it_tries() {
        let f = Fixture::new();
        let place = |machine: &'static str, dir: &'static str, set: serde_json::Value| {
            f.run(async move |h| h.place(machine, dir, &set, false).await)
                .unwrap_err()
        };
        assert_eq!(code(place("z", "/w", set())), "unknown_machine");
        assert_eq!(code(place("p", "/w", set())), WORKDIR_UNSUPPORTED);
        assert_eq!(code(place("c", "/w", set())), WORKDIR_UNSUPPORTED);
        assert_eq!(code(place("m", "w", set())), WORKDIR_INVALID);
        assert_eq!(
            code(place(
                "m",
                "/w",
                serde_json::json!({"id": "x", "kind": "nope"})
            )),
            WORKDIR_INVALID
        );
        assert!(f.calls().is_empty());
        assert!(f.kinds().is_empty());
    }

    /// A set's profile is rendered on the head for the machine, and the
    /// render goes with the set; one the machine may not run is refused.
    #[test]
    fn a_profile_is_rendered_for_the_machine() {
        let f = Fixture::new();
        let mut with = set();
        with["profile"] = "review".into();
        f.run(async |h| h.place("m", "/w", &with, false).await)
            .unwrap();
        let render = &f.plan()["render"];
        assert_eq!(render["profile"], "review");
        assert_eq!(render["kind"], "claude");
        assert!(list(&f.paths, None).unwrap()[0].render_hash.is_some());
        with["profile"] = "unrestricted".into();
        let err = f
            .run(async |h| h.place("m", "/w", &with, false).await)
            .unwrap_err();
        assert_eq!(code(err), crate::config::profile::PROFILE_NOT_ALLOWED);
        with["profile"] = "nope".into();
        let err = f
            .run(async |h| h.place("m", "/w", &with, false).await)
            .unwrap_err();
        assert_eq!(code(err), crate::config::profile::UNKNOWN_PROFILE);
    }

    /// A remove that went through drops the record and raises
    /// `workdir.removed`; a dry run keeps both.
    #[test]
    fn a_remove_drops_the_record() {
        let f = Fixture::new();
        f.run(async |h| h.place("m", "/w", &set(), false).await)
            .unwrap();
        f.answer("dry", &[("removed", ".claude/agents/checker.md")]);
        f.run(async |h| h.remove("m", "/w", None, true).await)
            .unwrap();
        assert_eq!(f.plan(), serde_json::json!({"op": "remove", "dir": "/w"}));
        assert_eq!(list(&f.paths, None).unwrap().len(), 1);
        f.answer("real", &[("removed", ".claude/agents/checker.md")]);
        f.run(async |h| h.remove("m", "/w", Some("reviewer"), false).await)
            .unwrap();
        assert_eq!(f.plan()["id"], "reviewer");
        assert!(list(&f.paths, None).unwrap().is_empty());
        assert_eq!(f.kinds(), ["workdir.placed", "workdir.removed"]);
    }

    /// `check` reads what the head recorded on the machine, in one run with
    /// the rows: ok only when the set's state is. A directory the head
    /// placed nothing in is `workdir_not_found`.
    #[test]
    fn check_reads_the_set_the_head_kept() {
        let f = Fixture::new();
        let err = f.run(async |h| h.check("m", "/w", None).await).unwrap_err();
        assert_eq!(code(err), WORKDIR_NOT_FOUND);
        f.run(async |h| h.place("m", "/w", &set(), false).await)
            .unwrap();
        f.answer_read(&[("/w", "reviewer", "ok")], &[]);
        let checked = f.run(async |h| h.check("m", "/w", None).await).unwrap();
        assert_eq!(checked.len(), 1);
        assert!(checked[0].ok);
        assert_eq!(checked[0].state, Some(State::Ok));
        assert_eq!(checked[0].rows.len(), 1);
        let plan = f.plan();
        assert_eq!(plan["op"], "read");
        assert_eq!(plan["rows"], true);
        assert_eq!(plan["entries"][0]["dir"], "/w");
        let hash = plan["entries"][0]["set_hash"].as_str().unwrap().to_string();
        assert!(plan["sets"][&hash].is_object(), "the set goes with it");
        f.answer_read(&[("/w", "reviewer", "drift")], &[]);
        let checked = f.run(async |h| h.check("m", "/w", None).await).unwrap();
        assert!(!checked[0].ok, "a set that drifted is not ok");
        assert_eq!(checked[0].state, Some(State::Drift));
        f.answer_read(&[("/w", "reviewer", "gone")], &[]);
        let checked = f.run(async |h| h.check("m", "/w", None).await).unwrap();
        assert_eq!(checked[0].state, Some(State::Gone));
        let err = f
            .run(async |h| h.check("m", "/w", Some("other")).await)
            .unwrap_err();
        assert_eq!(code(err), WORKDIR_NOT_FOUND);
        assert_eq!(f.kinds(), ["workdir.placed"], "a check raises nothing");
        assert!(
            list(&f.paths, None).unwrap()[0].state == Some(State::Ok),
            "a check writes nothing"
        );
    }

    #[test]
    fn the_event_comes_after_the_record() {
        let f = Fixture::new();
        f.run(async |h| h.place("m", "/w", &set(), false).await)
            .unwrap();
        assert_eq!(f.kinds(), ["workdir.placed"]);
        assert_eq!(*f.records_at.lock().unwrap(), [1]);
    }

    /// The head measures a set by what it places, as `place --local` does:
    /// a set under the cap whose JSON text is over it is placed, kept and
    /// checked.
    #[test]
    fn a_set_under_the_cap_is_not_refused_for_its_text() {
        let f = Fixture::new();
        let big = serde_json::json!({
            "id": "reviewer",
            "kind": "claude",
            "files": [{"path": ".claude/agents/checker.md", "content": "\n".repeat(workdir::MAX_SET - 64)}],
        });
        assert!(big.to_string().len() > workdir::MAX_SET);
        let p = f
            .run(async |h| h.place("m", "/w", &big, false).await)
            .unwrap();
        assert!(p.ok);
        let checked = f.run(async |h| h.check("m", "/w", None).await).unwrap();
        assert!(checked[0].ok);
        // And the machine reads it from the plan the same way.
        let home = f.tmp.path().join("h");
        let dir = home.join("src");
        std::fs::create_dir_all(&dir).unwrap();
        let run = f.tmp.path().join("run");
        std::fs::create_dir_all(&run).unwrap();
        let op = PlanOp::Place {
            dir: dir.display().to_string(),
            set: big,
            render: None,
        };
        std::fs::write(run.join(PLAN_FILE), serde_json::to_vec(&op).unwrap()).unwrap();
        match apply_local(&f.paths, &home, &run, true) {
            Answer::Placed(p) => assert!(p.ok, "{p:?}"),
            other => panic!("{other:?}"),
        }
    }

    /// Two first places of one set at once both keep it: neither fails for
    /// the other's write.
    #[test]
    fn keeping_one_set_twice_at_once_holds() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        for round in 0..20 {
            let mut doc = set();
            doc["id"] = format!("set-{round}").into();
            let one = Set::from_doc(&doc).unwrap();
            let hash = one.hash();
            let start = std::sync::Barrier::new(8);
            std::thread::scope(|s| {
                let all: Vec<_> = (0..8)
                    .map(|_| {
                        s.spawn(|| {
                            start.wait();
                            let record = Record {
                                machine: "m".into(),
                                dir: "/w".into(),
                                id: one.id.clone(),
                                kind: "claude".into(),
                                profile: None,
                                set_hash: hash.clone(),
                                render_hash: None,
                                placed_at: String::new(),
                                result: "ok".into(),
                                error: None,
                                state: None,
                                items: vec![],
                                read_at: None,
                            };
                            save_placed(&paths, record, &one)
                        })
                    })
                    .collect();
                for t in all {
                    t.join().unwrap().unwrap();
                }
            });
            assert_eq!(kept_set(&paths, &hash).unwrap(), one);
        }
    }

    /// A remove that took out some sets and not others drops the records of
    /// those it took out, and raises `workdir.removed` for each, also for a
    /// set the head did not place; the others stay.
    #[test]
    fn a_partial_remove_drops_what_it_took_out() {
        let f = Fixture::new();
        let mut other = set();
        other["id"] = "other".into();
        f.run(async |h| h.place("m", "/w", &set(), false).await)
            .unwrap();
        f.run(async |h| h.place("m", "/w", &other, false).await)
            .unwrap();
        f.answer_sets(
            "real",
            &[
                ("reviewer", "removed", ".claude/agents/checker.md"),
                ("other", "changed_by_hand", ".claude/agents/other.md"),
                ("local", "removed", ".claude/agents/local.md"),
                ("", "removed", "/r/.git/info/exclude"),
            ],
        );
        let p = f
            .run(async |h| h.remove("m", "/w", None, false).await)
            .unwrap();
        assert!(!p.ok);
        let left: Vec<String> = list(&f.paths, None)
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(left, ["other"]);
        let removed: Vec<(String, serde_json::Value)> = f
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.0 == "workdir.removed")
            .map(|e| (e.2["id"].as_str().unwrap().to_string(), e.2["kind"].clone()))
            .collect();
        assert_eq!(
            removed,
            [
                ("local".to_string(), serde_json::Value::Null),
                ("reviewer".to_string(), "claude".into()),
            ]
        );
    }

    /// Change the record of the one set with `f`.
    fn edit_record(f: &Fixture, edit: impl FnOnce(&mut serde_json::Value)) {
        let file = f.paths.state_dir.join(RECORD_FILE);
        let mut all: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        edit(&mut all["workdirs"][0]);
        std::fs::write(&file, all.to_string()).unwrap();
    }

    fn record(f: &Fixture) -> Record {
        list(&f.paths, None).unwrap().remove(0)
    }

    const AUTO: &str = "[[machine]]\nname = \"m\"\nlocal = true\nbundle_apply = \"auto\"\n";

    /// A profile that renders otherwise than when its set was placed makes
    /// the set `stale`. In `manual` that is all; in `auto` the keeper places
    /// it again, and the event says so, once.
    #[test]
    fn a_changed_profile_is_stale_and_auto_places_it_again() {
        let mut f = Fixture::new();
        let mut with = set();
        with["profile"] = "review".into();
        f.run(async |h| h.place("m", "/w", &with, false).await)
            .unwrap();
        let rendered = record(&f).render_hash.clone();
        edit_record(&f, |r| r["render_hash"] = "before".into());
        f.answer_read(&[("/w", "reviewer", "stale")], &[]);
        let watch = workdir_keep::Watch::new(workdir_keep::READ_EVERY);
        f.run(async |h| watch.pass(h).await);
        let r = record(&f);
        assert_eq!(
            r.render_hash.as_deref(),
            Some("before"),
            "manual places nothing"
        );
        assert_eq!(r.state, Some(State::Stale));
        assert_eq!(f.kinds(), ["workdir.placed"]);
        f.flock = Flock::parse(Path::new("flock.toml"), AUTO).unwrap();
        let watch = workdir_keep::Watch::new(workdir_keep::READ_EVERY);
        f.run(async |h| watch.pass(h).await);
        assert_eq!(record(&f).render_hash, rendered);
        assert_eq!(f.kinds(), ["workdir.placed", "workdir.placed"]);
        assert_eq!(f.events.lock().unwrap()[1].2["auto"], true);
        f.run(async |h| watch.pass(h).await);
        assert_eq!(f.kinds().len(), 2, "placed once");
    }

    /// `auto` never places a set the machine holds in another version, nor
    /// tries again at once a set it failed to place.
    #[test]
    fn auto_holds_back_where_it_should() {
        let mut f = Fixture::new();
        f.flock = Flock::parse(Path::new("flock.toml"), AUTO).unwrap();
        let mut with = set();
        with["profile"] = "review".into();
        f.run(async |h| h.place("m", "/w", &with, false).await)
            .unwrap();
        edit_record(&f, |r| {
            r["render_hash"] = "before".into();
            r["state"] = "stale".into();
            r["items"] = serde_json::json!([workdir_keep::ANOTHER_VERSION]);
        });
        f.answer_read_items(
            &[(
                "/w",
                "reviewer",
                "stale",
                &[workdir_keep::ANOTHER_VERSION][..],
            )],
            &[],
        );
        let watch = workdir_keep::Watch::new(Duration::from_secs(3600));
        f.run(async |h| watch.pass(h).await);
        assert_eq!(f.kinds(), ["workdir.placed"], "another version is left");
        // Now one it fails to place.
        edit_record(&f, |r| {
            r["render_hash"] = "before".into();
            r["items"] = serde_json::json!([]);
        });
        f.answer("real", &[("conflict", ".claude/agents/checker.md")]);
        f.run(async |h| watch.pass(h).await);
        assert_eq!(f.kinds(), ["workdir.placed", "workdir.failed"]);
        f.run(async |h| watch.pass(h).await);
        assert_eq!(
            f.kinds(),
            ["workdir.placed", "workdir.failed"],
            "not again within the wait"
        );
    }

    /// The read writes each set's state; one that was ok and drifts raises
    /// `workdir.drift` once. A record never read nor placed with a state
    /// raises nothing on its first read.
    #[test]
    fn the_read_writes_states_and_raises_drift_once() {
        let f = Fixture::new();
        f.run(async |h| h.place("m", "/w", &set(), false).await)
            .unwrap();
        assert_eq!(record(&f).state, Some(State::Ok), "a place says ok");
        let watch = workdir_keep::Watch::new(Duration::ZERO);
        f.answer_read(&[("/w", "reviewer", "drift")], &[]);
        f.run(async |h| watch.pass(h).await);
        let r = record(&f);
        assert_eq!(r.state, Some(State::Drift));
        assert!(r.read_at.is_some());
        assert_eq!(f.kinds(), ["workdir.placed", "workdir.drift"]);
        let drift = f.events.lock().unwrap()[1].2.clone();
        assert_eq!(drift["state"], "drift");
        assert_eq!(drift["dir"], "/w");
        f.run(async |h| watch.pass(h).await);
        assert_eq!(f.kinds().len(), 2, "once");
        assert_eq!(machine_read(&f.paths, "m").unwrap().ledgers, 1);
        edit_record(&f, |r| {
            r.as_object_mut().unwrap().remove("state");
        });
        f.answer_read(&[("/w", "reviewer", "conflict")], &[]);
        f.run(async |h| watch.pass(h).await);
        assert_eq!(f.kinds().len(), 2, "no state before: no event");
        assert_eq!(record(&f).state, Some(State::Conflict));
    }

    /// A set that drifted keeps its state when its profile changes too:
    /// `stale` would hide the drift, and the next read would raise
    /// `workdir.drift` again, every interval.
    #[test]
    fn a_changed_profile_does_not_hide_a_drift() {
        let f = Fixture::new();
        let mut with = set();
        with["profile"] = "review".into();
        f.run(async |h| h.place("m", "/w", &with, false).await)
            .unwrap();
        let watch = workdir_keep::Watch::new(Duration::ZERO);
        f.answer_read(&[("/w", "reviewer", "drift")], &[]);
        f.run(async |h| watch.pass(h).await);
        assert_eq!(f.kinds(), ["workdir.placed", "workdir.drift"]);
        edit_record(&f, |r| r["render_hash"] = "before".into());
        for _ in 0..3 {
            f.run(async |h| watch.pass(h).await);
        }
        assert_eq!(f.kinds().len(), 2, "{:?}", f.kinds());
        assert_eq!(record(&f).state, Some(State::Drift));
    }

    /// Change the record of the set in `dir` with `edit`.
    fn edit_record_in(f: &Fixture, dir: &str, edit: impl FnOnce(&mut serde_json::Value)) {
        let file = f.paths.state_dir.join(RECORD_FILE);
        let mut all: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        let r = all["workdirs"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|r| r["dir"] == dir)
            .unwrap();
        edit(r);
        std::fs::write(&file, all.to_string()).unwrap();
    }

    /// A directory found gone is dropped only a whole read interval after
    /// that read, not on the next read whenever it comes: after a head
    /// restart (a new watch reads at once), nor on a read an `auto` place
    /// brought forward.
    #[test]
    fn a_gone_directory_waits_a_read_interval() {
        let f = Fixture::new();
        f.run(async |h| h.place("m", "/w", &set(), false).await)
            .unwrap();
        f.answer_read(&[("/w", "reviewer", "gone")], &[]);
        let watch = workdir_keep::Watch::new(workdir_keep::READ_EVERY);
        f.run(async |h| watch.pass(h).await);
        assert_eq!(record(&f).state, Some(State::Gone));
        // A restart: a new watch reads at once.
        let watch = workdir_keep::Watch::new(workdir_keep::READ_EVERY);
        f.run(async |h| watch.pass(h).await);
        assert!(f.plan().get("drop").is_none(), "{}", f.plan());
        // A read interval after the read that found it gone: dropped.
        let old = (Utc::now() - chrono::Duration::from_std(workdir_keep::READ_EVERY).unwrap())
            .to_rfc3339();
        edit_record(&f, |r| r["read_at"] = old.clone().into());
        let watch = workdir_keep::Watch::new(workdir_keep::READ_EVERY);
        f.run(async |h| watch.pass(h).await);
        assert_eq!(f.plan()["drop"], serde_json::json!(["/w"]));
    }

    /// The `auto` variant: another set's place brings the read forward.
    #[test]
    fn an_auto_place_does_not_drop_a_gone_directory_early() {
        let mut f = Fixture::new();
        f.flock = Flock::parse(Path::new("flock.toml"), AUTO).unwrap();
        f.run(async |h| h.place("m", "/w", &set(), false).await)
            .unwrap();
        let mut with = set();
        with["profile"] = "review".into();
        f.run(async |h| h.place("m", "/v", &with, false).await)
            .unwrap();
        f.answer_read(&[("/w", "reviewer", "gone"), ("/v", "reviewer", "ok")], &[]);
        let watch = workdir_keep::Watch::new(workdir_keep::READ_EVERY);
        f.run(async |h| watch.pass(h).await);
        edit_record_in(&f, "/v", |r| r["render_hash"] = "before".into());
        f.run(async |h| watch.pass(h).await);
        assert_eq!(
            f.kinds().iter().filter(|k| *k == "workdir.placed").count(),
            3,
            "auto placed /v again: {:?}",
            f.kinds()
        );
        assert!(f.plan().get("drop").is_none(), "{}", f.plan());
    }

    /// A set whose kept file is gone still has its directory read: gone
    /// there, it goes like any other.
    #[test]
    fn a_set_the_head_cannot_send_is_still_read_for_its_directory() {
        let f = Fixture::new();
        f.run(async |h| h.place("m", "/w", &set(), false).await)
            .unwrap();
        std::fs::remove_file(set_file(&f.paths, &record(&f).set_hash)).unwrap();
        f.answer_read(&[("/w", "reviewer", "gone")], &[]);
        let watch = workdir_keep::Watch::new(Duration::ZERO);
        f.run(async |h| watch.pass(h).await);
        let plan = f.plan();
        assert_eq!(plan["entries"][0]["dir"], "/w", "{plan}");
        assert!(plan["entries"][0]["stale"].is_string(), "{plan}");
        assert_eq!(plan["sets"], serde_json::json!({}), "{plan}");
        assert_eq!(record(&f).state, Some(State::Gone));
    }

    /// A gone directory stays on the first read; the second read in a row
    /// asks the machine to drop its ledger, and once it did the record
    /// goes, with `workdir.removed` and the kept set. One that comes back
    /// stays. A set removed on the machine goes at once.
    #[test]
    fn a_gone_directory_goes_after_two_reads() {
        let f = Fixture::new();
        f.run(async |h| h.place("m", "/w", &set(), false).await)
            .unwrap();
        let watch = workdir_keep::Watch::new(Duration::ZERO);
        f.answer_read(&[("/w", "reviewer", "gone")], &[]);
        f.run(async |h| watch.pass(h).await);
        assert!(f.plan().get("drop").is_none(), "nothing to drop yet");
        assert_eq!(record(&f).state, Some(State::Gone));
        // Back by the next read: it stays.
        f.answer_read(&[("/w", "reviewer", "ok")], &[]);
        f.run(async |h| watch.pass(h).await);
        assert_eq!(f.plan()["drop"], serde_json::json!(["/w"]));
        assert_eq!(record(&f).state, Some(State::Ok));
        // Gone twice in a row: dropped.
        f.answer_read(&[("/w", "reviewer", "gone")], &[]);
        f.run(async |h| watch.pass(h).await);
        let hash = record(&f).set_hash;
        f.answer_read(&[("/w", "reviewer", "gone")], &["/w"]);
        f.run(async |h| watch.pass(h).await);
        assert_eq!(f.plan()["drop"], serde_json::json!(["/w"]));
        assert!(list(&f.paths, None).unwrap().is_empty());
        let removed = f.events.lock().unwrap().last().unwrap().clone();
        assert_eq!(removed.0, "workdir.removed");
        assert_eq!(removed.2["reason"], "gone");
        assert!(!set_file(&f.paths, &hash).exists(), "the kept set goes");
        // Removed on the machine: at once.
        f.run(async |h| h.place("m", "/w", &set(), false).await)
            .unwrap();
        f.answer_read(&[("/w", "reviewer", "missing")], &[]);
        f.run(async |h| watch.pass(h).await);
        assert!(list(&f.paths, None).unwrap().is_empty());
        let removed = f.events.lock().unwrap().last().unwrap().clone();
        assert_eq!(removed.2["reason"], "removed on the machine");
    }

    /// `machine check`'s `workdirs` row: none before the head placed a set
    /// on the machine; one summary line, `warn` naming the worst and the
    /// fix when a directory is not ok or the machine holds too many
    /// ledgers. It writes nothing.
    #[test]
    fn the_check_row_sums_up_the_machine() {
        use crate::provision::run::Status as RowStatus;
        let f = Fixture::new();
        let rows = f.run(async |h| workdir_keep::check_rows(h, "m").await);
        assert!(rows.is_empty(), "nothing placed: no row");
        f.run(async |h| h.place("m", "/w", &set(), false).await)
            .unwrap();
        let rows = f.run(async |h| workdir_keep::check_rows(h, "m").await);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, workdir_keep::ROW);
        assert_eq!(rows[0].status, RowStatus::Pass);
        assert_eq!(rows[0].first_line, "1 dir: 1 ok");
        f.answer_read(
            &[("/w", "reviewer", "drift"), ("/v", "reviewer", "ok")],
            &[],
        );
        let rows = f.run(async |h| workdir_keep::check_rows(h, "m").await);
        assert_eq!(rows[0].status, RowStatus::Warn);
        assert_eq!(rows[0].first_line, "2 dirs: 1 ok, 1 drift");
        let why = rows[0].reason.clone().unwrap();
        assert!(why.starts_with("/w (reviewer): drift; "), "{why}");
        assert!(why.contains("pastor workdir check m <dir>"), "{why}");
        assert_eq!(record(&f).state, Some(State::Ok), "a check writes nothing");
        let file = f.tmp.path().join("answer-read.json");
        let text = std::fs::read_to_string(&file)
            .unwrap()
            .replace("\"ledgers\":1", "\"ledgers\":2001");
        std::fs::write(&file, text).unwrap();
        let rows = f.run(async |h| workdir_keep::check_rows(h, "m").await);
        assert!(
            rows[0].reason.as_deref().unwrap().contains("2001 ledgers"),
            "{rows:?}"
        );
    }

    /// `machine describe`'s `workdirs`: the directories by their worst set,
    /// as the last read left them, and the machine's ledger count.
    #[test]
    fn describe_says_how_the_directories_stand() {
        let f = Fixture::new();
        assert!(workdir_keep::describe(&f.paths, "m", true).is_none());
        f.run(async |h| h.place("m", "/w", &set(), false).await)
            .unwrap();
        let mut other = set();
        other["id"] = "other".into();
        f.run(async |h| h.place("m", "/w", &other, false).await)
            .unwrap();
        f.run(async |h| h.place("m", "/v", &set(), false).await)
            .unwrap();
        f.answer_read(
            &[
                ("/w", "reviewer", "ok"),
                ("/w", "other", "drift"),
                ("/v", "reviewer", "ok"),
            ],
            &[],
        );
        let watch = workdir_keep::Watch::new(Duration::ZERO);
        f.run(async |h| watch.pass(h).await);
        let d = workdir_keep::describe(&f.paths, "m", true).unwrap();
        assert_eq!(d.count, 2);
        assert_eq!(
            d.states,
            [("drift".to_string(), 1), ("ok".to_string(), 1)].into()
        );
        assert_eq!(d.ledgers, Some(1));
        assert!(d.read_at.is_some());
        assert_eq!(
            d.worst,
            [workdir_keep::Worst {
                dir: "/w".into(),
                id: "other".into(),
                state: State::Drift,
            }]
        );
        let lines = workdir_keep::describe_lines(&d);
        assert!(
            lines[0].starts_with("2 dirs: 1 ok, 1 drift (read "),
            "{lines:?}"
        );
        assert_eq!(lines[1], "/w other: drift");
    }

    /// The head keys a directory as the machine does, written any way: a
    /// trailing `/`, `.` and repeated `/` aside.
    #[test]
    fn a_directory_is_one_however_it_is_written() {
        let f = Fixture::new();
        f.run(async |h| h.place("m", "/w/", &set(), false).await)
            .unwrap();
        assert_eq!(f.plan()["dir"], "/w");
        assert_eq!(list(&f.paths, None).unwrap()[0].dir, "/w");
        f.run(async |h| h.place("m", "//w/.", &set(), false).await)
            .unwrap();
        assert_eq!(list(&f.paths, None).unwrap().len(), 1);
        let checked = f.run(async |h| h.check("m", "/w", None).await).unwrap();
        assert_eq!(checked.len(), 1);
        f.answer("real", &[("removed", ".claude/agents/checker.md")]);
        f.run(async |h| h.remove("m", "/w//", None, false).await)
            .unwrap();
        assert!(list(&f.paths, None).unwrap().is_empty());
    }

    /// On the machine, a plan places the set, or removes it; a plan that
    /// does not read is an error answer with its code.
    #[test]
    fn apply_local_carries_out_the_plan() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        let home = tmp.path().canonicalize().unwrap().join("h");
        let dir = home.join("src/app");
        std::fs::create_dir_all(&dir).unwrap();
        let run = tmp.path().join("run");
        std::fs::create_dir_all(&run).unwrap();
        let plan = |op: PlanOp| {
            std::fs::write(run.join(PLAN_FILE), serde_json::to_vec(&op).unwrap()).unwrap();
        };
        let doc = Set::parse(SET, Some("json")).unwrap().to_doc();
        plan(PlanOp::Place {
            dir: dir.display().to_string(),
            set: doc,
            render: None,
        });
        let Answer::Placed(dry) = apply_local(&paths, &home, &run, true) else {
            panic!()
        };
        assert!(dry.dry_run);
        assert!(!dir.join(".claude/agents/checker.md").exists());
        let Answer::Placed(p) = apply_local(&paths, &home, &run, false) else {
            panic!()
        };
        assert!(p.ok, "{p:?}");
        assert_eq!(
            std::fs::read_to_string(dir.join(".claude/agents/checker.md")).unwrap(),
            "Check.\n"
        );
        plan(PlanOp::Remove {
            dir: dir.display().to_string(),
            id: None,
        });
        let Answer::Placed(p) = apply_local(&paths, &home, &run, false) else {
            panic!()
        };
        assert!(p.ok, "{p:?}");
        assert!(!dir.join(".claude/agents/checker.md").exists());
        std::fs::write(run.join(PLAN_FILE), "{\"op\": \"nope\"}").unwrap();
        let Answer::Error { code, .. } = apply_local(&paths, &home, &run, false) else {
            panic!()
        };
        assert_eq!(code, WORKDIR_INVALID);
    }

    fn placed(dir: &str, id: &str, at: &str, ok: bool) -> Reported {
        Reported::Placed {
            dir: dir.into(),
            id: id.into(),
            kind: "claude".into(),
            profile: Some("review".into()),
            set_hash: "a".repeat(64),
            render_hash: Some("b".repeat(64)),
            at: at.into(),
            ok,
            error: (!ok).then(|| "conflict .claude/agents/checker.md".into()),
        }
    }

    /// A pull machine and a command machine tell the head what ran on them:
    /// a place is recorded as the head records its own and raises the
    /// event, after the record is written; a failed one raises
    /// `workdir.failed`; a remove drops the record.
    #[test]
    fn a_pull_or_command_machine_reports_what_it_placed() {
        let f = Fixture::new();
        for machine in ["p", "c"] {
            f.run(async |head| {
                head.report(
                    machine,
                    &[placed("/w", "reviewer", "2026-11-02T10:00:00Z", true)],
                )
                .unwrap();
            });
        }
        let rows = list(&f.paths, None).unwrap();
        assert_eq!(rows.len(), 2, "{rows:?}");
        let p = rows.iter().find(|r| r.machine == "p").unwrap();
        assert_eq!(
            (
                p.dir.as_str(),
                p.id.as_str(),
                p.result.as_str(),
                p.profile.as_deref()
            ),
            ("/w", "reviewer", "ok", Some("review"))
        );
        assert_eq!(p.placed_at, "2026-11-02T10:00:00+00:00");
        assert_eq!(f.kinds(), ["workdir.placed", "workdir.placed"]);
        assert_eq!(
            *f.records_at.lock().unwrap(),
            [1, 2],
            "the record comes first"
        );
        // Nothing was pushed to either.
        assert!(f.calls().is_empty());

        // A failed place is recorded as failed, with the error.
        f.run(async |head| {
            head.report(
                "p",
                &[placed("/w", "reviewer", "2026-11-02T11:00:00Z", false)],
            )
            .unwrap();
        });
        let p = list(&f.paths, Some("p")).unwrap().remove(0);
        assert_eq!(p.result, "failed");
        assert_eq!(
            p.error.as_deref(),
            Some("conflict .claude/agents/checker.md")
        );
        assert_eq!(f.kinds().last().map(String::as_str), Some("workdir.failed"));

        // A remove drops it and says so.
        f.run(async |head| {
            head.report(
                "p",
                &[Reported::Removed {
                    dir: "/w".into(),
                    ids: vec!["reviewer".into()],
                    at: "2026-11-02T12:00:00Z".into(),
                }],
            )
            .unwrap();
        });
        assert!(list(&f.paths, Some("p")).unwrap().is_empty());
        let (kind, machine, d) = f.events.lock().unwrap().last().unwrap().clone();
        assert_eq!((kind.as_str(), machine.as_str()), ("workdir.removed", "p"));
        assert_eq!(d["id"], "reviewer");
        assert_eq!(d["kind"], "claude");

        // The same removal sent again, for a set with no record any more,
        // raises nothing.
        let before = f.kinds().len();
        f.run(async |head| {
            head.report(
                "p",
                &[Reported::Removed {
                    dir: "/w".into(),
                    ids: vec!["reviewer".into()],
                    at: "2026-11-02T12:00:00Z".into(),
                }],
            )
            .unwrap();
        });
        assert_eq!(f.kinds().len(), before, "{:?}", f.kinds());
    }

    /// `machine describe` on a machine the head does not reach says its sets
    /// were reported, not unread: the head never reads them.
    #[test]
    fn describe_says_reported_for_a_machine_the_head_does_not_read() {
        let f = Fixture::new();
        f.run(async |head| {
            head.report(
                "p",
                &[placed("/w", "reviewer", "2026-11-02T10:00:00Z", true)],
            )
            .unwrap();
        });
        let d = workdir_keep::describe(&f.paths, "p", false).unwrap();
        assert_eq!(d.states, [("reported".to_string(), 1)].into());
        assert_eq!(workdir_keep::describe_lines(&d)[0], "1 dir: 1 reported");
        let d = workdir_keep::describe(&f.paths, "p", true).unwrap();
        assert_eq!(d.states, [("unread".to_string(), 1)].into());
    }

    /// A report that arrives after a later one is left out, so a delayed
    /// delivery cannot undo what happened since.
    #[test]
    fn an_older_report_does_not_undo_a_newer_one() {
        let f = Fixture::new();
        f.run(async |head| {
            head.report(
                "p",
                &[placed("/w", "reviewer", "2026-11-02T10:00:00Z", true)],
            )
            .unwrap();
            head.report(
                "p",
                &[placed("/w", "reviewer", "2026-11-02T09:00:00Z", false)],
            )
            .unwrap();
            head.report(
                "p",
                &[Reported::Removed {
                    dir: "/w".into(),
                    ids: vec!["reviewer".into()],
                    at: "2026-11-02T09:30:00Z".into(),
                }],
            )
            .unwrap();
        });
        let rows = list(&f.paths, None).unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].result, "ok");
        assert_eq!(
            f.kinds(),
            ["workdir.placed"],
            "the late ones raised nothing"
        );
    }

    /// A place delivered twice is recorded once and raises one event: a
    /// report as old as the record changes nothing.
    #[test]
    fn a_place_delivered_twice_changes_nothing_the_second_time() {
        let f = Fixture::new();
        let place = placed("/w", "reviewer", "2026-11-02T10:00:00Z", true);
        f.run(async |head| {
            head.report("p", std::slice::from_ref(&place)).unwrap();
            head.report("p", std::slice::from_ref(&place)).unwrap();
        });
        assert_eq!(f.kinds(), ["workdir.placed"]);
        assert_eq!(list(&f.paths, None).unwrap().len(), 1);
    }

    /// The head places sets on a machine it reaches itself, and does not
    /// take a report from it; a machine it does not have is unknown; a bad
    /// report refuses the request and records nothing.
    #[test]
    fn a_report_is_refused_from_a_push_machine_an_unknown_one_or_when_bad() {
        let f = Fixture::new();
        let ok = placed("/w", "reviewer", "2026-11-02T10:00:00Z", true);
        let code_of = |machine: &str, reports: &[Reported]| {
            f.run(async |head| {
                head.report(machine, reports)
                    .unwrap_err()
                    .downcast_ref::<CliError>()
                    .unwrap()
                    .code
                    .clone()
            })
        };
        assert_eq!(code_of("m", std::slice::from_ref(&ok)), WORKDIR_UNSUPPORTED);
        assert_eq!(
            code_of("nope", std::slice::from_ref(&ok)),
            "unknown_machine"
        );
        let mut relative = ok.clone();
        if let Reported::Placed { dir, .. } = &mut relative {
            *dir = "w".into();
        }
        let mut bad_id = ok.clone();
        if let Reported::Placed { id, .. } = &mut bad_id {
            *id = "A b".into();
        }
        let mut bad_at = ok.clone();
        if let Reported::Placed { at, .. } = &mut bad_at {
            *at = "yesterday".into();
        }
        for bad in [relative, bad_id, bad_at] {
            assert_eq!(code_of("p", &[ok.clone(), bad]), WORKDIR_INVALID);
        }
        let many = vec![ok.clone(); MAX_REPORTS + 1];
        assert_eq!(code_of("p", &many), WORKDIR_INVALID);
        assert!(list(&f.paths, None).unwrap().is_empty(), "nothing recorded");
        assert!(f.kinds().is_empty());
    }
}
