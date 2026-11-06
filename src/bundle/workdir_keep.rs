//! Keeping the workdir sets the head placed: how each stands on its
//! machine.
//!
//! The head reads a push machine in one run (`PlanOp::Read`): for each set
//! it recorded there, the machine places it again as a dry run with the
//! profile as the head renders it now, and folds the rows into one state by
//! the table home bundles use (`keep::state_of`). A directory that is gone,
//! or a set its ledger no longer names, says so without a dry run. The same
//! run drops the ledgers of the directories the head names in `drop`, the
//! ones an earlier read found gone, when they are still gone.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::Apply;
use super::apply::{self, Row};
use super::keep::Reach;
use super::keep::{self, HomeState};
use super::workdir::{self, Placed, Set};
use super::workdir_push::{self, Head, Record};
use crate::config::{PastorConfig, Paths};
use crate::daemon::Fleet;
use crate::profile_render::Render;
use crate::provision::run::{Row as CheckRow, Status as RowStatus};
use crate::sync::Recover;

/// How a set the head placed stands in its directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// What pastor placed is there as it placed it.
    Ok,
    /// The directory's ledger does not name the set any more: it was
    /// removed on the machine.
    Missing,
    /// The directory is not there.
    Gone,
    /// Placing the set now would change something: the profile renders
    /// otherwise, a file is missing, or another version was placed since.
    Stale,
    /// Something pastor placed was changed by hand.
    Drift,
    /// A symlink is on the way to something pastor placed.
    NotAdopted,
    /// Something that is not pastor's is where the set puts a file, or the
    /// directory cannot be read.
    Conflict,
}

impl State {
    pub fn word(self) -> &'static str {
        match self {
            State::Ok => "ok",
            State::Missing => "missing",
            State::Gone => "gone",
            State::Stale => "stale",
            State::Drift => "drift",
            State::NotAdopted => "not adopted",
            State::Conflict => "conflict",
        }
    }
}

impl From<HomeState> for State {
    fn from(h: HomeState) -> State {
        match h {
            HomeState::Ok => State::Ok,
            HomeState::Stale => State::Stale,
            HomeState::Drift => State::Drift,
            HomeState::NotAdopted => State::NotAdopted,
            HomeState::Conflict => State::Conflict,
        }
    }
}

/// The item of a set placed on the machine in another version than the
/// head's.
pub const ANOTHER_VERSION: &str = "another version placed on the machine";

/// The most items a set's reading names.
const ITEMS: usize = 3;

/// One set the head asks about: where, which, the hash of the version it
/// placed, and its profile as rendered now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadEntry {
    pub dir: String,
    pub id: String,
    pub set_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render: Option<Render>,
    /// Why the head sends no set for it (its kept file is gone, its profile
    /// does not render): the machine reads only whether the directory and
    /// the set are still there, and answers `stale` with this when they
    /// are.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale: Option<String>,
}

/// How one set stands, and the first few items that are not ok.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetRead {
    pub dir: String,
    pub id: String,
    pub state: State,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
    /// The dry run's answer, when the read asked for rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placed: Option<Placed>,
}

/// What a read answers: each set asked about, how many directories the
/// machine holds a ledger for, and the directories whose ledger it dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadOut {
    pub sets: Vec<SetRead>,
    pub ledgers: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropped: Vec<String>,
}

/// On the machine: drop the ledger of each directory in `drop` that is
/// still gone, then read each of `entries`, whose sets are in `sets` by
/// hash; with `rows`, each answers its dry run too.
pub fn read_local(
    paths: &Paths,
    home: &Path,
    sets: &BTreeMap<String, serde_json::Value>,
    entries: &[ReadEntry],
    drop: &[String],
    rows: bool,
) -> ReadOut {
    let dropped = drop
        .iter()
        .filter(|dir| drop_ledger(paths, home, Path::new(dir)))
        .cloned()
        .collect();
    let sets = entries
        .iter()
        .map(|e| {
            let (state, items, placed) = if gone(Path::new(&e.dir)) {
                (State::Gone, Vec::new(), None)
            } else {
                read_one(paths, home, sets, e).unwrap_or_else(|err| {
                    let why = match err.downcast_ref::<crate::cli::CliError>() {
                        Some(c) => c.message.clone(),
                        None => format!("{err:#}"),
                    };
                    (State::Conflict, vec![why], None)
                })
            };
            SetRead {
                dir: e.dir.clone(),
                id: e.id.clone(),
                state,
                items,
                placed: placed.filter(|_| rows),
            }
        })
        .collect();
    ReadOut {
        sets,
        ledgers: ledgers(paths).len(),
        dropped,
    }
}

/// Drop the ledger of `dir` when it is gone: take out what the ledger
/// lists, as `remove` does, which also takes its lines out of the repo's
/// exclude file, then the ledger's own directory. Whether it did.
pub fn drop_ledger(paths: &Paths, home: &Path, dir: &Path) -> bool {
    if !gone(dir) {
        return false;
    }
    match workdir::remove(paths, home, dir, None, false) {
        Ok(p) if p.ok => {
            let ledger = workdir::ledger_dir(paths, &workdir::gone(dir));
            std::fs::remove_dir_all(&ledger).is_ok() || !ledger.exists()
        }
        Ok(_) | Err(_) => false,
    }
}

/// Whether nothing is at `dir`, not even a dangling link.
fn gone(dir: &Path) -> bool {
    dir.symlink_metadata()
        .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}

fn read_one(
    paths: &Paths,
    home: &Path,
    sets: &BTreeMap<String, serde_json::Value>,
    e: &ReadEntry,
) -> anyhow::Result<(State, Vec<String>, Option<Placed>)> {
    let dir = Path::new(&e.dir);
    // The ledger is keyed by the real path, as `place` keys it.
    let real = dir.canonicalize()?;
    let placed = apply::workdir_sets(&workdir::ledger_dir(paths, &real))?;
    let Some(hash) = placed.as_ref().and_then(|l| l.sets.get(&e.id)) else {
        return Ok((State::Missing, Vec::new(), None));
    };
    if *hash != e.set_hash {
        return Ok((State::Stale, vec![ANOTHER_VERSION.to_string()], None));
    }
    if let Some(why) = &e.stale {
        return Ok((State::Stale, vec![why.clone()], None));
    }
    let doc = sets
        .get(&e.set_hash)
        .ok_or_else(|| anyhow::anyhow!("the head sent no set {}", e.set_hash))?;
    let set = Set::from_doc(doc)?;
    let p = workdir::place_with(paths, home, dir, &set, e.render.as_ref(), true)?;
    let (state, items) = fold(&p.rows);
    Ok((state, items, Some(p)))
}

/// The rows of a dry run as one state, the worst of them, and the first
/// few that are not ok.
pub fn fold(rows: &[Row]) -> (State, Vec<String>) {
    let mut state = State::Ok;
    let mut items = Vec::new();
    for r in rows {
        let here = State::from(keep::state_of(r.action));
        if here == State::Ok {
            continue;
        }
        state = state.max(here);
        if items.len() < ITEMS {
            items.push(format!("{} {}", keep::action_word(r.action), r.path));
        }
    }
    (state, items)
}

/// Past `limit` ledgers on this machine, drop the ledgers of directories
/// that are gone, the least recently written first, down to `limit`. A
/// directory still there keeps its ledger however many there are. How many
/// it dropped.
pub fn cap(paths: &Paths, home: &Path, limit: usize) -> usize {
    let all = ledgers(paths);
    if all.len() <= limit {
        return 0;
    }
    let mut gone_dirs: Vec<(std::time::SystemTime, std::path::PathBuf)> = all
        .iter()
        .filter_map(|ledger| {
            let ledger_of = apply::workdir_sets(ledger).ok()??;
            let dir = std::path::PathBuf::from(ledger_of.dir?);
            if !gone(&dir) {
                return None;
            }
            let at = ledger
                .join("ledger.json")
                .metadata()
                .ok()?
                .modified()
                .ok()?;
            Some((at, dir))
        })
        .collect();
    gone_dirs.sort();
    let mut left = all.len();
    let mut dropped = 0;
    for (_, dir) in gone_dirs {
        if left <= limit {
            break;
        }
        if drop_ledger(paths, home, &dir) {
            left -= 1;
            dropped += 1;
        }
    }
    dropped
}

/// Each ledger dir on this machine.
fn ledgers(paths: &Paths) -> Vec<std::path::PathBuf> {
    let Ok(all) = std::fs::read_dir(paths.state_dir.join(workdir::LEDGERS)) else {
        return Vec::new();
    };
    all.filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.join("ledger.json").is_file())
        .collect()
}

/// The id of the `machine check` row.
pub const ROW: &str = "workdirs";

/// The ledgers a machine keeps before the oldest of gone directories go.
pub const CAP: usize = 2000;

/// The most directories a summary names.
const WORST: usize = 5;

/// One directory at its worst.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worst {
    pub dir: String,
    pub id: String,
    pub state: State,
}

/// What `machine describe` says of the workdir sets the head placed on a
/// machine, from its last reading.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkdirsDescription {
    /// The directories with a set the head placed.
    pub count: usize,
    /// How many of them stand at each state, by the worst of their sets;
    /// `unread` before the first reading.
    pub states: BTreeMap<String, usize>,
    /// When the machine was last read as a whole.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_at: Option<String>,
    /// The directories the machine holds a ledger for, at that read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ledgers: Option<usize>,
    /// The directories that are not ok, worst first, a few.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub worst: Vec<Worst>,
}

/// Each directory of `sets` at the worst of its sets (`None`: not read),
/// worst first.
fn by_dir(sets: &[(String, String, Option<State>)]) -> Vec<(String, String, Option<State>)> {
    let mut dirs: BTreeMap<&str, (String, Option<State>)> = BTreeMap::new();
    for (dir, id, state) in sets {
        let at = dirs.entry(dir).or_insert((id.clone(), *state));
        if *state > at.1 {
            *at = (id.clone(), *state);
        }
    }
    let mut out: Vec<(String, String, Option<State>)> = dirs
        .into_iter()
        .map(|(dir, (id, state))| (dir.to_string(), id, state))
        .collect();
    out.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    out
}

fn word(state: Option<State>) -> &'static str {
    state.map_or("unread", State::word)
}

/// `12 dirs: 11 ok, 1 drift`, worst last.
fn summary(dirs: &[(String, String, Option<State>)]) -> String {
    let mut counts: BTreeMap<Option<State>, usize> = BTreeMap::new();
    for (_, _, state) in dirs {
        *counts.entry(*state).or_default() += 1;
    }
    let parts: Vec<String> = counts
        .iter()
        .map(|(state, n)| format!("{n} {}", word(*state)))
        .collect();
    let n = dirs.len();
    format!(
        "{n} {}: {}",
        if n == 1 { "dir" } else { "dirs" },
        parts.join(", ")
    )
}

/// `machine describe`'s `workdirs` for `machine`, from the head's record:
/// none when the head placed no set there. A machine the head does not
/// `reach` is never read, so its sets stand as `reported`, not `unread`.
pub fn describe(paths: &Paths, machine: &str, reached: bool) -> Option<WorkdirsDescription> {
    let records = workdir_push::list(paths, Some(machine)).ok()?;
    if records.is_empty() {
        return None;
    }
    let sets: Vec<(String, String, Option<State>)> = records
        .into_iter()
        .map(|r| (r.dir, r.id, r.state))
        .collect();
    let dirs = by_dir(&sets);
    let mut states: BTreeMap<String, usize> = BTreeMap::new();
    for (_, _, state) in &dirs {
        let word = match state {
            None if !reached => "reported",
            state => word(*state),
        };
        *states.entry(word.to_string()).or_default() += 1;
    }
    let read = workdir_push::machine_read(paths, machine);
    Some(WorkdirsDescription {
        count: dirs.len(),
        states,
        read_at: read.as_ref().map(|r| r.read_at.clone()),
        ledgers: read.map(|r| r.ledgers),
        worst: dirs
            .iter()
            .filter_map(|(dir, id, state)| {
                state.filter(|s| *s != State::Ok).map(|state| Worst {
                    dir: dir.clone(),
                    id: id.clone(),
                    state,
                })
            })
            .take(WORST)
            .collect(),
    })
}

/// The lines of `machine describe`'s `workdirs` section.
pub fn describe_lines(d: &WorkdirsDescription) -> Vec<String> {
    // Best to worst, as the check row says them.
    const ORDER: [&str; 9] = [
        "unread",
        "reported",
        "ok",
        "missing",
        "gone",
        "stale",
        "drift",
        "not adopted",
        "conflict",
    ];
    let mut states: Vec<(&String, &usize)> = d.states.iter().collect();
    states.sort_by_key(|(state, _)| ORDER.iter().position(|o| o == state));
    let parts: Vec<String> = states
        .iter()
        .map(|(state, n)| format!("{n} {state}"))
        .collect();
    let mut first = format!(
        "{} {}: {}",
        d.count,
        if d.count == 1 { "dir" } else { "dirs" },
        parts.join(", ")
    );
    if let Some(at) = &d.read_at {
        first.push_str(&format!(" (read {at})"));
    }
    let mut out = vec![first];
    for w in &d.worst {
        out.push(format!("{} {}: {}", w.dir, w.id, w.state.word()));
    }
    out
}

/// The `workdirs` row `machine check` adds for `machine`, from a live read
/// that writes nothing: none when the head placed no set there or cannot
/// reach it; `warn` when a directory is not ok, or the machine holds more
/// ledgers than `CAP`, or it cannot be read.
pub async fn check_rows(head: &Head<'_>, machine: &str) -> Vec<CheckRow> {
    let Ok(records) = workdir_push::list(head.paths, Some(machine)) else {
        return vec![];
    };
    if records.is_empty() || !head.reaches(machine) {
        return vec![];
    }
    let out = match head.read(machine, &records, &[], false).await {
        Ok(out) => out,
        Err(e) => {
            let why = match e.downcast_ref::<crate::cli::CliError>() {
                Some(c) => c.message.clone(),
                None => format!("{e:#}"),
            };
            return vec![check_row(
                RowStatus::Warn,
                format!("the workdir sets on {machine} could not be read: {why}"),
                None,
            )];
        }
    };
    let sets: Vec<(String, String, Option<State>)> = out
        .sets
        .into_iter()
        .map(|s| (s.dir, s.id, Some(s.state)))
        .collect();
    let dirs = by_dir(&sets);
    let line = summary(&dirs);
    let bad: Vec<String> = dirs
        .iter()
        .filter(|(_, _, s)| *s != Some(State::Ok))
        .take(3)
        .map(|(dir, id, s)| format!("{dir} ({id}): {}", word(*s)))
        .collect();
    let over = out.ledgers > CAP;
    if bad.is_empty() && !over {
        return vec![check_row(RowStatus::Pass, line, None)];
    }
    let mut why = Vec::new();
    if !bad.is_empty() {
        why.push(format!(
            "{}; `pastor workdir check {machine} <dir>` names the files, and `pastor workdir place` again settles a stale set",
            bad.join("; ")
        ));
    }
    if over {
        why.push(format!(
            "{} ledgers on the machine, past {CAP}: `pastor workdir remove` takes out the sets of directories no longer used",
            out.ledgers
        ));
    }
    vec![check_row(RowStatus::Warn, line, Some(why.join("; ")))]
}

fn check_row(status: RowStatus, first_line: String, reason: Option<String>) -> CheckRow {
    CheckRow {
        id: ROW.into(),
        agent: None,
        status,
        first_line,
        reason,
    }
}

/// How often the keeper wakes, as the home bundles' keeper does.
const TICK: Duration = Duration::from_secs(15);

/// How often it reads a machine's sets.
pub const READ_EVERY: Duration = Duration::from_secs(5 * 60);

/// How long after the keeper failed to place a set on its own it does not
/// try that rendering again. A person's `place`, or a new rendering, tries
/// at once.
const RETRY_AFTER: Duration = Duration::from_secs(10 * 60);

/// The item of a set whose profile renders otherwise than when it was
/// placed.
pub const PROFILE_CHANGED: &str = "the profile renders otherwise than when it was placed";

/// A set by machine, directory and id.
type SetKey = (String, String, String);

/// The rendering the keeper tried to place a set with, and when.
type Tried = (Option<String>, Instant);

/// What the keeper knows between passes.
pub struct Watch {
    /// How often a machine is read: `READ_EVERY`.
    every: Duration,
    /// When each machine was last read.
    read: Mutex<BTreeMap<String, Instant>>,
    /// The sets the keeper failed to place on its own, by machine,
    /// directory and id: the rendering it tried, and when.
    failed: Mutex<BTreeMap<SetKey, Tried>>,
}

impl Watch {
    pub fn new(every: Duration) -> Watch {
        Watch {
            every,
            read: Mutex::default(),
            failed: Mutex::default(),
        }
    }

    /// One pass over the push machines the head placed sets on:
    /// - a set whose profile renders otherwise now is `stale`, and in `auto`
    ///   it is placed again (not one placed on the machine in another
    ///   version since, nor one in a directory that was gone);
    /// - every `every`, each machine is read in one run, and the directories
    ///   the last read found gone, and that still are, lose their ledgers
    ///   and records.
    pub async fn pass(&self, head: &Head<'_>) {
        let Ok(records) = workdir_push::list(head.paths, None) else {
            return;
        };
        let mut by: BTreeMap<String, Vec<Record>> = BTreeMap::new();
        for r in records {
            by.entry(r.machine.clone()).or_default().push(r);
        }
        for (machine, records) in by {
            let Some(m) = head.flock.get(&machine) else {
                continue;
            };
            if !head.reaches(&machine) {
                continue;
            }
            let auto = head.flock.bundle_apply(m) == Apply::Auto;
            for r in &records {
                self.keep(head, r, auto).await;
            }
            self.read_due(head, &machine).await;
        }
    }

    async fn keep(&self, head: &Head<'_>, r: &Record, auto: bool) {
        if r.state == Some(State::Gone) {
            return;
        }
        let Ok(set) = workdir_push::kept_set(head.paths, &r.set_hash) else {
            return;
        };
        let now = match head.render(&set, &r.machine) {
            Ok(render) => render.as_ref().map(workdir_push::hash_of),
            Err(e) => {
                let why = match e.downcast_ref::<crate::cli::CliError>() {
                    Some(c) => c.message.clone(),
                    None => format!("{e:#}"),
                };
                let item = format!(
                    "profile {}: {why}",
                    set.profile.as_deref().unwrap_or_default()
                );
                let _ = workdir_push::note_stale(head.paths, r, &item);
                return;
            }
        };
        let key = (r.machine.clone(), r.dir.clone(), r.id.clone());
        // A place that went through since, the keeper's or a person's,
        // settles what the keeper failed before.
        if r.result != "failed" {
            self.failed.lock().recover().remove(&key);
        }
        let failed = self.failed.lock().recover().get(&key).cloned();
        let retry = failed
            .as_ref()
            .is_some_and(|(tried, at)| *tried != now || at.elapsed() >= RETRY_AFTER);
        if now == r.render_hash && !retry {
            return;
        }
        let another = r.items.iter().any(|i| i == ANOTHER_VERSION);
        if !auto || another {
            if now != r.render_hash {
                let _ = workdir_push::note_stale(head.paths, r, PROFILE_CHANGED);
            }
            return;
        }
        if failed.is_some() && !retry {
            return;
        }
        let placed = head
            .place_as(&r.machine, &r.dir, &set.to_doc(), false, true)
            .await;
        let mut all = self.failed.lock().recover();
        match placed {
            Ok(p) if p.ok => {
                all.remove(&key);
            }
            Ok(_) | Err(_) => {
                all.insert(key, (now, Instant::now()));
            }
        }
        drop(all);
        // What was read before is not what is there now.
        self.read.lock().recover().remove(&r.machine);
    }

    async fn read_due(&self, head: &Head<'_>, machine: &str) {
        let due = self
            .read
            .lock()
            .recover()
            .get(machine)
            .is_none_or(|at| at.elapsed() >= self.every);
        if !due {
            return;
        }
        // As the record is now, after any place this pass made.
        let Ok(records) = workdir_push::list(head.paths, Some(machine)) else {
            return;
        };
        // Gone for a whole interval, by the saved time of the read that
        // found it so: the in-memory interval starts again on a restart,
        // and an `auto` place brings the next read forward.
        let mut drop: Vec<String> = records
            .iter()
            .filter(|r| r.state == Some(State::Gone) && read_before(r, self.every))
            .map(|r| r.dir.clone())
            .collect();
        drop.sort();
        drop.dedup();
        match head.read(machine, &records, &drop, false).await {
            Ok(out) => match workdir_push::note_read(head.paths, machine, &records, &out) {
                Ok(raise) => {
                    for (kind, detail) in raise {
                        (head.emit)(kind, machine, detail);
                    }
                }
                Err(e) => {
                    tracing::warn!(%machine, error = %format!("{e:#}"), "record a workdir read")
                }
            },
            Err(e) => {
                tracing::warn!(%machine, error = %format!("{e:#}"), "read the workdir sets on a machine")
            }
        }
        // Not again until the next interval, read or not.
        self.read
            .lock()
            .recover()
            .insert(machine.to_string(), Instant::now());
    }
}

/// Whether `r` was last read at least `every` ago. A time that does not
/// parse counts as not: nothing is dropped on it.
fn read_before(r: &Record, every: Duration) -> bool {
    let Some(at) = r
        .read_at
        .as_deref()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
    else {
        return false;
    };
    (chrono::Utc::now() - at.with_timezone(&chrono::Utc))
        .to_std()
        .is_ok_and(|age| age >= every)
}

/// The keeper of workdir sets: a loop in the head.
pub struct Keeper {
    paths: Paths,
    fleet: Weak<Fleet>,
    reach: Reach,
    watch: Watch,
}

impl Keeper {
    pub fn new(paths: Paths, fleet: Weak<Fleet>, reach: Reach) -> Arc<Keeper> {
        Arc::new(Keeper {
            paths,
            fleet,
            reach,
            watch: Watch::new(READ_EVERY),
        })
    }

    /// Wake every `TICK` until the fleet is gone.
    pub fn spawn(self: &Arc<Self>) {
        let keeper = self.clone();
        tokio::spawn(async move {
            let mut every = tokio::time::interval(TICK);
            every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                every.tick().await;
                if keeper.fleet.upgrade().is_none() {
                    return;
                }
                keeper.tick().await;
            }
        });
    }

    async fn tick(&self) {
        let Some(fleet) = self.fleet.upgrade() else {
            return;
        };
        // Nothing placed: pastor.toml is not read.
        if workdir_push::list(&self.paths, None).is_ok_and(|r| r.is_empty()) {
            return;
        }
        let config = match PastorConfig::load(&self.paths.config_file()) {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(error = %format!("{e:#}"), "workdir keeper: pastor.toml does not load");
                return;
            }
        };
        let flock = fleet.flock();
        let emit = |kind: &'static str, machine: &str, detail: serde_json::Value| {
            fleet.emit_machine(kind, machine, Some(detail));
        };
        let head = Head {
            paths: &self.paths,
            config: &config,
            flock: &flock,
            reach: &self.reach,
            emit: &emit,
        };
        self.watch.pass(&head).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SET: &str = r#"{
        "id": "reviewer",
        "kind": "claude",
        "files": [{"path": ".claude/agents/checker.md", "content": "Check.\n"}]
    }"#;

    /// A machine with a home and a working directory below it.
    struct Machine {
        _tmp: tempfile::TempDir,
        paths: Paths,
        home: std::path::PathBuf,
        dir: std::path::PathBuf,
    }

    impl Machine {
        fn new() -> Machine {
            let tmp = tempfile::tempdir().unwrap();
            let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
            let home = tmp.path().canonicalize().unwrap().join("h");
            let dir = home.join("src/app");
            std::fs::create_dir_all(&dir).unwrap();
            Machine {
                _tmp: tmp,
                paths,
                home,
                dir,
            }
        }

        fn place(&self, text: &str) -> Set {
            let set = Set::parse(text, Some("json")).unwrap();
            let p = workdir::place(&self.paths, &self.home, &self.dir, &set, false).unwrap();
            assert!(p.ok, "{p:?}");
            set
        }

        fn read(&self, set: &Set, drop: &[String]) -> ReadOut {
            let sets = BTreeMap::from([(set.hash(), set.to_doc())]);
            let entry = ReadEntry {
                dir: self.dir.display().to_string(),
                id: set.id.clone(),
                set_hash: set.hash(),
                render: None,
                stale: None,
            };
            read_local(&self.paths, &self.home, &sets, &[entry], drop, false)
        }

        /// A read of `set` with no document, as for a set the head cannot
        /// send.
        fn probe(&self, set: &Set) -> SetRead {
            let entry = ReadEntry {
                dir: self.dir.display().to_string(),
                id: set.id.clone(),
                set_hash: set.hash(),
                render: None,
                stale: Some("profile gone: unknown".into()),
            };
            let out = read_local(
                &self.paths,
                &self.home,
                &BTreeMap::new(),
                &[entry],
                &[],
                false,
            );
            out.sets.into_iter().next().unwrap()
        }

        fn state(&self, set: &Set) -> (State, Vec<String>) {
            let out = self.read(set, &[]);
            assert_eq!(out.sets.len(), 1);
            (out.sets[0].state, out.sets[0].items.clone())
        }
    }

    /// A set the head sends no document for is `stale` with the head's
    /// reason while its directory holds it, and `missing` or `gone` as any
    /// other set once it does not.
    #[test]
    fn a_set_with_no_document_is_read_for_its_directory() {
        let m = Machine::new();
        let set = m.place(SET);
        let got = m.probe(&set);
        assert_eq!(got.state, State::Stale);
        assert_eq!(got.items, ["profile gone: unknown"]);
        workdir::remove(&m.paths, &m.home, &m.dir, None, false).unwrap();
        assert_eq!(m.probe(&set).state, State::Missing);
        std::fs::remove_dir_all(&m.dir).unwrap();
        assert_eq!(m.probe(&set).state, State::Gone);
    }

    /// A set as pastor placed it is ok; edited by hand it is drift; taken
    /// out by hand it is stale; behind a symlink it is not adopted.
    #[test]
    fn a_set_reads_as_it_stands() {
        let m = Machine::new();
        let set = m.place(SET);
        let out = m.read(&set, &[]);
        assert_eq!(out.ledgers, 1);
        assert_eq!(out.sets[0].state, State::Ok);
        let file = m.dir.join(".claude/agents/checker.md");
        std::fs::write(&file, "Edited.\n").unwrap();
        assert_eq!(
            m.state(&set),
            (
                State::Drift,
                vec!["changed by hand .claude/agents/checker.md".to_string()]
            )
        );
        std::fs::remove_file(&file).unwrap();
        assert_eq!(m.state(&set).0, State::Stale);
        let agents = m.dir.join(".claude/agents");
        std::fs::remove_dir(&agents).unwrap();
        let elsewhere = m.home.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &agents).unwrap();
        assert_eq!(m.state(&set).0, State::NotAdopted);
    }

    /// A set removed on the machine is missing; another version placed
    /// there is stale, and its files are left as they are.
    #[test]
    fn a_set_changed_on_the_machine_says_so() {
        let m = Machine::new();
        let set = m.place(SET);
        let other = m.place(&SET.replace("Check.", "Check again."));
        assert_eq!(
            m.state(&set),
            (State::Stale, vec![ANOTHER_VERSION.to_string()])
        );
        assert_eq!(
            std::fs::read_to_string(m.dir.join(".claude/agents/checker.md")).unwrap(),
            "Check again.\n"
        );
        assert_eq!(m.state(&other).0, State::Ok);
        workdir::remove(&m.paths, &m.home, &m.dir, None, false).unwrap();
        assert_eq!(m.state(&set).0, State::Missing);
    }

    /// A gone directory reads as gone and keeps its ledger; named in
    /// `drop`, its ledger goes. One in `drop` that is back is kept.
    #[test]
    fn a_gone_directory_is_dropped_only_when_asked() {
        let m = Machine::new();
        let set = m.place(SET);
        let ledger = workdir::ledger_dir(&m.paths, &m.dir);
        let dir = m.dir.display().to_string();
        let out = m.read(&set, std::slice::from_ref(&dir));
        assert!(out.dropped.is_empty(), "it is there");
        assert!(ledger.join("ledger.json").exists());
        std::fs::remove_dir_all(&m.dir).unwrap();
        let out = m.read(&set, &[]);
        assert_eq!(out.sets[0].state, State::Gone);
        assert!(ledger.join("ledger.json").exists(), "one read keeps it");
        let out = m.read(&set, std::slice::from_ref(&dir));
        assert_eq!(out.dropped, [dir]);
        assert_eq!(out.sets[0].state, State::Gone);
        assert!(!ledger.join("ledger.json").exists());
        assert_eq!(out.ledgers, 0);
    }

    /// Past the cap, the ledgers of gone directories go, the oldest first;
    /// a directory still there keeps its ledger.
    #[test]
    fn the_cap_drops_the_oldest_gone_ledgers() {
        let m = Machine::new();
        let mut dirs = Vec::new();
        for (i, name) in ["a", "b", "c"].iter().enumerate() {
            let dir = m.home.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let set = Set::parse(SET, Some("json")).unwrap();
            workdir::place(&m.paths, &m.home, &dir, &set, false).unwrap();
            // a is the oldest, c the newest.
            let ledger = workdir::ledger_dir(&m.paths, &dir).join("ledger.json");
            let at = std::time::SystemTime::now() - Duration::from_secs(3600 * (3 - i as u64));
            std::fs::File::options()
                .write(true)
                .open(&ledger)
                .unwrap()
                .set_modified(at)
                .unwrap();
            dirs.push(dir);
        }
        assert_eq!(cap(&m.paths, &m.home, 3), 0, "not past the cap");
        std::fs::remove_dir_all(&dirs[0]).unwrap();
        std::fs::remove_dir_all(&dirs[1]).unwrap();
        assert_eq!(cap(&m.paths, &m.home, 2), 1);
        assert!(
            !workdir::ledger_dir(&m.paths, &dirs[0]).exists(),
            "the oldest"
        );
        assert!(workdir::ledger_dir(&m.paths, &dirs[1]).exists());
        assert_eq!(cap(&m.paths, &m.home, 0), 1, "the live one stays");
        assert_eq!(ledgers(&m.paths).len(), 1);
        assert!(workdir::ledger_dir(&m.paths, &dirs[2]).exists());
    }

    /// The worst row decides, and only those that are not ok are named.
    #[test]
    fn the_worst_row_decides() {
        let row = |path: &str, action| Row {
            bundle: "s".into(),
            home: None,
            path: path.into(),
            key: None,
            action,
            detail: None,
            old: None,
            backup: None,
        };
        use apply::Action;
        let (state, items) = fold(&[
            row("a", Action::Kept),
            row("b", Action::Added),
            row("c", Action::ChangedByHand),
            row("d", Action::Skipped),
        ]);
        assert_eq!(state, State::Drift);
        assert_eq!(items, ["added b", "changed by hand c"]);
        assert_eq!(fold(&[row("a", Action::Kept)]).0, State::Ok);
    }
}
