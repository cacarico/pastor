//! Applying a plan on the machine: `pastor bundle apply --local <dir>`,
//! which the head runs there with the rendered set unpacked in `dir`.
//!
//! - The versions go into the machine's bundle store,
//!   `<data>/bundles/<bundle>/<hash>/`, and the homes link into it: a new
//!   version is a new directory and a link swapped by a rename.
//! - An instruction file gets one block per bundle between
//!   `<!-- pastor:bundle:<id> -->` and `<!-- /pastor:bundle:<id> -->`. Text
//!   outside the blocks is the user's and is never changed.
//! - The ledger, `<state>/applied/ledger.json`, lists each link and block
//!   pastor wrote, with what it wrote. Pastor changes or removes only what
//!   the ledger lists, and only while it still holds what pastor wrote;
//!   anything else is a `conflict` or `changed_by_hand` row. What pastor is
//!   about to write goes in the ledger before the write, beside what was
//!   there, so a run cut short leaves nothing pastor no longer knows as its
//!   own. One apply runs at a time, under `<state>/applied/apply.lock`.
//! - Before a file is changed it is copied to `<state>/applied/backup/`,
//!   once per run: the first copy of each file for good, then the last 5.
//!   A file is written to a temp file beside it, with the old file's mode,
//!   and renamed over it, and only if it still holds what was read.
//! - Pastor never writes through a symlink: a dir from `$HOME` down, or an
//!   instruction file, that is one is a `linked` row.
//! - `bundle adopt` (`Adopt`) takes homes over: it backs up what it will
//!   touch, makes those symlinks plain copies of what they point to, and
//!   puts the bundle's in place of what the user holds (`replaced` rows).
//! - A harness's secret files (`Plan::secrets`) are never read or written.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::plan::{Item, PLAN_FILE, Plan, STORE_DIR, What};
use super::slots::BIN;
use crate::config::{Paths, create_private_dir};

/// Backups kept per file.
pub const BACKUPS: usize = 5;

/// Tries at writing a file that changed between the read and the write.
const TRIES: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Added,
    Kept,
    Updated,
    Removed,
    /// Something not pastor's is where the item goes.
    Conflict,
    /// The item's file, or a dir on its way, is a symlink.
    Linked,
    /// What pastor wrote was changed since; it is left as it is.
    ChangedByHand,
    /// The file kept changing while pastor wrote it.
    Busy,
    Failed,
    /// No home here has a place for it.
    Skipped,
    /// `bundle adopt` put the bundle's in place of what the user had, or a
    /// symlink in place of a plain copy of what it points to; `old` says
    /// what was there and `backup` where a copy of it went.
    Replaced,
}

impl Action {
    /// Whether the row leaves the machine short of its plan.
    pub fn is_problem(self) -> bool {
        matches!(
            self,
            Action::Conflict
                | Action::Linked
                | Action::ChangedByHand
                | Action::Busy
                | Action::Failed
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    pub bundle: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
    /// As `~/...` when it is under the home.
    pub path: String,
    /// The key in a JSON file, dotted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub action: Action,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// What a `replaced` row's item held before, shortened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old: Option<String>,
    /// Where `bundle adopt` copied the file it replaced, or the file or
    /// dir it is in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub machine: String,
    pub dry_run: bool,
    pub rows: Vec<Row>,
    /// The dir `bundle adopt` copied what it touches to, before it wrote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
}

impl Report {
    pub fn ok(&self) -> bool {
        !self.rows.iter().any(|r| r.action.is_problem())
    }

    /// Whether applying it changed, or would change, anything.
    pub fn changes(&self) -> bool {
        self.rows.iter().any(|r| {
            matches!(
                r.action,
                Action::Added | Action::Updated | Action::Removed | Action::Replaced
            )
        })
    }
}

/// What pastor wrote, as the ledger keeps it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Ledger {
    #[serde(flatten)]
    meta: Meta,
    entries: Vec<Entry>,
}

/// What a workdir's ledger says beside its entries: the directory, as its
/// real path, and the sets placed in it. A home's ledger has neither.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Meta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    dir: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    sets: BTreeMap<String, SetRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SetRecord {
    /// The SHA-256 of the set as it was last placed.
    hash: String,
    /// The profile the set named then, and the SHA-256 of its rendering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    profile: Option<Profiled>,
    /// When that version was first placed, RFC 3339.
    placed_at: String,
}

/// Where a run's items go.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    /// The harness homes below `Places::home`.
    Home,
    /// One working directory, as its real path.
    Workdir(PathBuf),
}

/// Which of a workdir's sets a run places or removes; what it does not
/// name stays as it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Every set in the directory: only a removal.
    All,
    /// One set by its id, with the hash of the version placed and the
    /// profile it names, or `None` when it is removed.
    Set {
        id: String,
        hash: Option<String>,
        profile: Option<Profiled>,
    },
}

/// The profile a placed set names, and the SHA-256 of what it rendered to
/// then: a changed profile is a new version of the set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profiled {
    pub name: String,
    pub hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    bundle: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    home: Option<String>,
    /// Absolute.
    path: String,
    #[serde(flatten)]
    owned: Owned,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Owned {
    /// A link pastor made, to this absolute target.
    Link { target: String },
    /// A block pastor wrote: the SHA-256 of its text, whether pastor made
    /// the file, and what pastor put between the user's text and the block,
    /// to take out with it.
    Block {
        sha: String,
        created: bool,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        sep: String,
    },
    /// A key pastor set in a JSON file: the SHA-256 of its value, the depth
    /// of the first object pastor made on its way, and whether pastor made
    /// the file.
    Key {
        key: Vec<String>,
        sha: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        made: Option<usize>,
        created: bool,
    },
    /// An entry pastor added to the list at `key`; `made` counts the list.
    Entry {
        key: Vec<String>,
        sha: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        made: Option<usize>,
        created: bool,
    },
    /// A whole file pastor wrote.
    Text { sha: String },
}

impl Owned {
    /// As `What::slot`.
    fn slot(&self) -> (Option<&[String]>, Option<String>) {
        match self {
            Owned::Key { key, .. } => (Some(key), None),
            Owned::Entry { key, sha, .. } => (Some(key), Some(sha.clone())),
            Owned::Link { .. } | Owned::Block { .. } | Owned::Text { .. } => (None, None),
        }
    }

    /// The SHA-256 of the value of a key or list entry pastor wrote.
    fn value_sha(&self) -> Option<&str> {
        match self {
            Owned::Key { sha, .. } | Owned::Entry { sha, .. } => Some(sha.as_str()),
            Owned::Link { .. } | Owned::Block { .. } | Owned::Text { .. } => None,
        }
    }
}

/// The ledger as this run keeps it on disk: what was there, and each entry
/// written before the write it covers.
struct Journal<'a> {
    places: &'a Places,
    entries: Vec<Entry>,
}

impl Journal<'_> {
    /// Record `e` before pastor writes what it says.
    fn intend(&mut self, e: &Entry) -> Result<(), String> {
        if self.entries.contains(e) {
            return Ok(());
        }
        self.entries.push(e.clone());
        save_ledger(self.places, &self.places.meta, &self.entries)
            .map_err(|e| format!("the ledger: {e:#}"))
    }
}

fn save_ledger(places: &Places, meta: &Meta, entries: &[Entry]) -> anyhow::Result<()> {
    create_private_dir(&places.applied)?;
    let ledger = Ledger {
        meta: meta.clone(),
        entries: entries.to_vec(),
    };
    write_atomic(
        &places.ledger_file(),
        serde_json::to_string_pretty(&ledger)?.as_bytes(),
        0o600,
    )?;
    Ok(())
}

/// The entries for `item` at `path`: one, or two when a run was cut short
/// between recording a write and the next ledger.
fn entries_at<'a>(entries: &'a [Entry], item: &Item, path: &Path) -> Vec<&'a Entry> {
    entries
        .iter()
        .filter(|e| e.bundle == item.bundle && Path::new(&e.path) == path)
        .filter(|e| e.owned.slot() == item.what.slot())
        .collect()
}

fn owned(entries: &[&Entry]) -> Vec<Entry> {
    entries.iter().map(|e| (*e).clone()).collect()
}

/// Whether `path` is one `pattern` names: as many parts, each the same, `*`
/// in a part of the pattern standing for any characters.
fn glob(pattern: &Path, path: &Path) -> bool {
    fn part(p: &[u8], s: &[u8]) -> bool {
        match p.split_first() {
            None => s.is_empty(),
            Some((b'*', rest)) => (0..=s.len()).any(|i| part(rest, &s[i..])),
            Some((c, rest)) => s.first() == Some(c) && part(rest, &s[1..]),
        }
    }
    use std::os::unix::ffi::OsStrExt;
    let (a, b): (Vec<_>, Vec<_>) = (pattern.components().collect(), path.components().collect());
    a.len() == b.len()
        && a.iter()
            .zip(&b)
            .all(|(x, y)| part(x.as_os_str().as_bytes(), y.as_os_str().as_bytes()))
}

/// Each secret file the plan names, as a path and, where a dir on its way
/// is a link, as the path it really is.
fn secret_globs(places: &Places, secrets: &[String]) -> Vec<(String, Vec<PathBuf>)> {
    secrets
        .iter()
        .map(|s| {
            let path = places.expand(s);
            let mut globs = vec![path.clone()];
            if let (Some(dir), Some(name)) = (path.parent(), path.file_name())
                && let Ok(real) = std::fs::canonicalize(dir)
                && real != dir
            {
                globs.push(real.join(name));
            }
            (s.clone(), globs)
        })
        .collect()
}

/// What `bundle adopt` asks of an apply: the homes it takes over, as the
/// plan names them (`~/.claude-main`); none named is every home.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Adopt {
    pub homes: Vec<String>,
}

/// An adopt as this run makes it.
struct AdoptRun {
    homes: Vec<String>,
    /// Where its backup goes: `<state>/applied/backup/adopt-<time>/`, the
    /// machine's paths below `$HOME` kept, others under `_/`. `None` on a
    /// dry run.
    backup: Option<PathBuf>,
}

/// Where the machine keeps what it applied.
struct Places {
    home: PathBuf,
    store: PathBuf,
    /// The ledger's dir: `<state>/applied`, or a workdir's.
    applied: PathBuf,
    target: Target,
    scope: Scope,
    /// What the ledger says beside its entries while this run writes;
    /// `final_meta` says what it says after.
    meta: Meta,
    /// Files backed up this run.
    backed_up: std::cell::RefCell<std::collections::BTreeSet<PathBuf>>,
    /// The secret files, as the plan names them and as paths, a parent dir
    /// on the way resolved too where it is a link.
    secrets: Vec<(String, Vec<PathBuf>)>,
    adopt: Option<AdoptRun>,
    /// A workdir set's caller held items back with problem rows of its own.
    refused: bool,
    /// Keys of a workdir's JSON file that pastor owns whole: a value there
    /// that is not what pastor wrote is replaced, not left as changed by hand.
    whole: Vec<Vec<String>>,
}

impl Places {
    /// Whether this run takes over what is in `home`: an adopt names it, or
    /// names no home. A file every home shares (`bin`) is in none.
    fn adopts(&self, home: Option<&str>) -> bool {
        match (&self.adopt, home) {
            (Some(a), Some(h)) => a.homes.is_empty() || a.homes.iter().any(|x| x == h),
            _ => false,
        }
    }

    /// Whether this run looks at what is in `home` at all: an adopt of some
    /// homes leaves the others, and the shared files, as they are.
    fn in_scope(&self, home: Option<&str>) -> bool {
        match &self.adopt {
            Some(a) if !a.homes.is_empty() => self.adopts(home),
            _ => true,
        }
    }

    /// Whether this run leaves the ledger entry `e` as it is: it is in a
    /// home an adopt does not take, or of a workdir set this run does not
    /// name.
    fn leaves(&self, e: &Entry) -> bool {
        !self.in_scope(e.home.as_deref())
            || matches!(&self.scope, Scope::Set { id, .. } if *id != e.bundle)
    }

    /// What the ledger says beside `kept` once this run is done: a set is
    /// recorded while the ledger holds something of it, or once it was
    /// placed with no problem (an empty set holds nothing), and keeps the
    /// time its version was first placed.
    fn final_meta(&self, kept: &[Entry], rows: &[Row]) -> Meta {
        let mut meta = self.meta.clone();
        if self.target == Target::Home {
            return meta;
        }
        let holds = |id: &str| kept.iter().any(|e| e.bundle == id);
        match &self.scope {
            Scope::All => meta.sets.retain(|id, _| holds(id)),
            Scope::Set { id, hash: None, .. } => {
                if !holds(id) {
                    meta.sets.remove(id);
                }
            }
            Scope::Set {
                id,
                hash: Some(hash),
                profile,
            } => {
                let clean = !self.refused && !rows.iter().any(|r| r.action.is_problem());
                if !holds(id) && !clean {
                    meta.sets.remove(id);
                } else if meta
                    .sets
                    .get(id)
                    .is_none_or(|r| r.hash != *hash || r.profile != *profile)
                {
                    meta.sets.insert(
                        id.clone(),
                        SetRecord {
                            hash: hash.clone(),
                            profile: profile.clone(),
                            placed_at: chrono::Utc::now().to_rfc3339(),
                        },
                    );
                }
            }
        }
        meta
    }

    /// The tag of `bundle`'s block in an instruction file: a home's
    /// bundle, or a workdir's set.
    fn tag(&self, bundle: &str) -> String {
        match self.target {
            Target::Home => format!("bundle:{bundle}"),
            Target::Workdir(_) => format!("workdir:{bundle}"),
        }
    }

    /// What a row calls the owner of an item: a bundle in a home, a set
    /// in a workdir.
    fn owner(&self) -> &'static str {
        match self.target {
            Target::Home => "bundle",
            Target::Workdir(_) => "set",
        }
    }

    /// Take out the dirs above `path` that are empty once pastor took a
    /// file out of them, up to `prune_stop`.
    fn prune_dirs(&self, path: &Path) {
        let Some(stop) = self.prune_stop() else {
            return;
        };
        let mut dir = path.parent();
        while let Some(d) = dir.filter(|d| d.starts_with(&stop) && *d != stop) {
            if std::fs::remove_dir(d).is_err() {
                break;
            }
            dir = d.parent();
        }
    }

    /// The dir whose empty subdirs go once pastor took the last file out of
    /// them, that dir itself kept.
    fn prune_stop(&self) -> Option<PathBuf> {
        match &self.target {
            Target::Home => self
                .expand(super::plan::MARKETPLACES)
                .parent()
                .map(Path::to_path_buf),
            Target::Workdir(dir) => Some(dir.clone()),
        }
    }

    /// Where the adopt backup keeps its copy of `p`.
    fn backup_of(&self, p: &Path) -> Option<PathBuf> {
        let dir = self.adopt.as_ref()?.backup.as_ref()?;
        Some(match p.strip_prefix(&self.home) {
            Ok(rest) => dir.join(rest),
            Err(_) => dir.join("_").join(p.strip_prefix("/").unwrap_or(p)),
        })
    }

    /// The secret file `p` is, as the plan names it: by its path, or by
    /// where it really is when a link is on its way. Only names are read.
    fn secret(&self, p: &Path) -> Option<&str> {
        let real = std::fs::canonicalize(p).ok();
        self.secrets
            .iter()
            .find(|(_, globs)| {
                globs
                    .iter()
                    .any(|g| glob(g, p) || real.as_deref().is_some_and(|r| glob(g, r)))
            })
            .map(|(name, _)| name.as_str())
    }

    fn expand(&self, p: &str) -> PathBuf {
        match p.strip_prefix("~/") {
            Some(rest) => self.home.join(rest),
            None if p == "~" => self.home.clone(),
            None => PathBuf::from(p),
        }
    }

    /// `p` as `~/...` when it is under the home; in a workdir, relative to
    /// it.
    fn show(&self, p: &Path) -> String {
        if let Target::Workdir(dir) = &self.target
            && let Ok(rest) = p.strip_prefix(dir)
        {
            return match rest.as_os_str().is_empty() {
                true => ".".to_string(),
                false => rest.display().to_string(),
            };
        }
        match p.strip_prefix(&self.home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => p.display().to_string(),
        }
    }

    fn ledger_file(&self) -> PathBuf {
        self.applied.join("ledger.json")
    }
}

/// Apply the rendered set in `dir` on this machine, whose home is `home`.
/// A dry run says what it would do and writes nothing. With `adopt`, the
/// homes it names are taken over (see `takeover`): what each item will
/// touch is copied to a backup first, and nothing is written if that
/// fails; a symlink on an item's way becomes a plain copy of what it points
/// to; and where the user holds something else, the bundle's wins.
pub fn apply_local(
    dir: &Path,
    paths: &Paths,
    home: &Path,
    dry_run: bool,
    adopt: Option<&Adopt>,
) -> anyhow::Result<Report> {
    let text = std::fs::read_to_string(dir.join(PLAN_FILE))
        .map_err(|e| anyhow::anyhow!("{}: {e}", dir.join(PLAN_FILE).display()))?;
    let plan: Plan = serde_json::from_str(&text)?;
    let applied = paths.state_dir.join("applied");
    let mut places = Places {
        home: home.to_path_buf(),
        store: paths.data_dir().join("bundles"),
        target: Target::Home,
        scope: Scope::All,
        meta: Meta::default(),
        backed_up: Default::default(),
        secrets: Vec::new(),
        adopt: adopt.map(|a| AdoptRun {
            homes: a.homes.clone(),
            backup: (!dry_run).then(|| {
                let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.6fZ");
                applied.join("backup").join(format!("adopt-{stamp}"))
            }),
        }),
        applied,
        refused: false,
        whole: Vec::new(),
    };
    places.secrets = secret_globs(&places, &plan.secrets);
    let _lock = if dry_run { None } else { Some(lock(&places)?) };
    let ledger = read_ledger(&places)?;
    let (rows, backup) = run(&plan, &places, dir, &ledger, dry_run)?;
    Ok(Report {
        machine: plan.machine,
        dry_run,
        rows,
        backup: backup.map(|b| places.show(&b)),
    })
}

fn read_ledger(places: &Places) -> anyhow::Result<Ledger> {
    match std::fs::read_to_string(places.ledger_file()) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| anyhow::anyhow!("{}: {e}", places.ledger_file().display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Ledger::default()),
        Err(e) => Err(anyhow::anyhow!("{}: {e}", places.ledger_file().display())),
    }
}

/// What a workdir's ledger says of its directory and the sets in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkdirLedger {
    /// The directory, as its real path.
    pub dir: Option<String>,
    /// Each set placed there, by id, with the hash of the version placed.
    pub sets: BTreeMap<String, String>,
}

/// What the workdir ledger in `ledger_dir` says; `None` when there is none.
pub fn workdir_sets(ledger_dir: &Path) -> anyhow::Result<Option<WorkdirLedger>> {
    let file = ledger_dir.join("ledger.json");
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow::anyhow!("{}: {e}", file.display())),
    };
    let ledger: Ledger =
        serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", file.display()))?;
    let sets = ledger
        .meta
        .sets
        .into_iter()
        .map(|(id, s)| (id, s.hash))
        .collect();
    Ok(Some(WorkdirLedger {
        dir: ledger.meta.dir,
        sets,
    }))
}

/// Place `items` in the working directory `dir`, a real path, keeping its
/// ledger in `ledger_dir`, for the sets `scope` names: what the ledger
/// lists for them and `items` no longer have comes out. `refused` says the
/// caller held items back with a problem row of its own, so the set does not
/// count as placed with no problem. `whole` names the keys of a JSON file
/// pastor owns whole (see `Places::whole`). A dry run says what it would do
/// and writes nothing.
pub fn place_workdir(
    ledger_dir: &Path,
    dir: &Path,
    scope: Scope,
    items: &[Item],
    refused: bool,
    whole: &[Vec<String>],
    dry_run: bool,
) -> anyhow::Result<Vec<Row>> {
    let mut places = Places {
        home: PathBuf::new(),
        store: PathBuf::new(),
        applied: ledger_dir.to_path_buf(),
        target: Target::Workdir(dir.to_path_buf()),
        scope,
        meta: Meta::default(),
        backed_up: Default::default(),
        secrets: Vec::new(),
        adopt: None,
        refused,
        whole: whole.to_vec(),
    };
    let _lock = if dry_run { None } else { Some(lock(&places)?) };
    let ledger = read_ledger(&places)?;
    places.meta = Meta {
        dir: Some(dir.display().to_string()),
        ..ledger.meta.clone()
    };
    if let Scope::Set {
        id, hash: Some(_), ..
    } = &places.scope
        && let Some(why) = clash(&places, &ledger, id, items)
    {
        return Err(crate::cli::CliError::err(
            super::workdir::WORKDIR_CONFLICT,
            why,
        ));
    }
    let plan = Plan {
        items: items.to_vec(),
        ..Plan::default()
    };
    let (rows, _) = run(&plan, &places, dir, &ledger, dry_run)?;
    Ok(rows)
}

/// What another set in the directory already holds that one of `items`,
/// the set `id`'s, wants too: a file, or a JSON key, or a key on the way
/// to one. Each set has its own block in an instruction file, and a list
/// entry two sets add is both of theirs, so neither clashes.
fn clash(places: &Places, ledger: &Ledger, id: &str, items: &[Item]) -> Option<String> {
    let overlap = |a: &[String], b: &[String]| a.starts_with(b) || b.starts_with(a);
    for item in items {
        let path = places.expand(&item.path);
        for e in ledger.entries.iter().filter(|e| e.bundle != id) {
            let at = Path::new(&e.path);
            // A file of one set on the way to, or under, a file of another.
            if at != path && (at.starts_with(&path) || path.starts_with(at)) {
                return Some(format!(
                    "sets {} and {id} are in each other's way: {} and {}; place one of them",
                    e.bundle,
                    places.show(at),
                    places.show(&path)
                ));
            }
            if at != path {
                continue;
            }
            let key = match (&e.owned, &item.what) {
                (Owned::Block { .. }, What::Block { .. }) => continue,
                (Owned::Entry { key: a, .. }, What::Entry { key: b, .. }) if a == b => continue,
                (
                    Owned::Key { key: a, .. } | Owned::Entry { key: a, .. },
                    What::Key { key: b, .. } | What::Entry { key: b, .. },
                ) if overlap(a, b) => Some(if a.len() < b.len() { a } else { b }),
                (
                    Owned::Key { .. } | Owned::Entry { .. },
                    What::Key { .. } | What::Entry { .. },
                ) => {
                    continue;
                }
                _ => None,
            };
            let at = places.show(&path);
            return Some(match key {
                Some(key) => format!(
                    "sets {} and {id} both set {} in {at}; place one of them, or move the key to one",
                    e.bundle,
                    key.join(".")
                ),
                None => format!(
                    "sets {} and {id} both place {at}; place one of them, or move the file to one",
                    e.bundle
                ),
            });
        }
    }
    None
}

/// Place `plan`'s items as `places` says, beside what `ledger` lists, and
/// take out what it lists that the plan no longer has: the rows, and the
/// adopt's backup dir. `dir` holds the rendered versions the plan names.
fn run(
    plan: &Plan,
    places: &Places,
    dir: &Path,
    ledger: &Ledger,
    dry_run: bool,
) -> anyhow::Result<(Vec<Row>, Option<PathBuf>)> {
    let mut journal = Journal {
        places,
        entries: ledger.entries.clone(),
    };
    let hashes: BTreeMap<&str, &str> = plan
        .versions
        .iter()
        .map(|v| (v.bundle.as_str(), v.hash.as_str()))
        .collect();
    // The vars only the machine knows go in first: what an entry's content
    // is decides which ledger entry is its. An adopt of some homes leaves
    // the rest out. A workdir set comes with its vars filled, and a home's
    // are not its.
    let items: Vec<Item> = plan
        .items
        .iter()
        .filter(|item| places.in_scope(item.home.as_deref()))
        .map(|item| {
            if places.target != Target::Home {
                return item.clone();
            }
            let bundle_dir = places
                .store
                .join(&item.bundle)
                .join(hashes.get(item.bundle.as_str()).unwrap_or(&""));
            let vars = machine_vars(places, item, &bundle_dir);
            let what = match &item.what {
                What::Block { text } => What::Block {
                    text: super::plan::fill(text, &vars),
                },
                What::Key { key, value } => What::Key {
                    key: key.clone(),
                    value: super::plan::fill_value(value.clone(), &vars),
                },
                What::Entry { key, value } => What::Entry {
                    key: key.clone(),
                    value: super::plan::fill_value(value.clone(), &vars),
                },
                other => other.clone(),
            };
            Item {
                what,
                ..item.clone()
            }
        })
        .collect();
    let mut over = takeover(places, &items)?;
    let mut rows = Vec::new();
    for v in &plan.versions {
        let dst = places.store.join(&v.bundle).join(&v.hash);
        if dst.exists() {
            continue;
        }
        if !dry_run {
            install_version(
                &dir.join(STORE_DIR).join(&v.bundle).join(&v.hash),
                &dst,
                &v.modes,
            )?;
        }
        rows.push(Row {
            bundle: v.bundle.clone(),
            home: None,
            path: places.show(&dst),
            key: None,
            action: Action::Added,
            detail: Some(format!("version {}", &v.hash[..v.hash.len().min(12)])),
            old: None,
            backup: None,
        });
    }
    rows.extend(unlink(places, &mut over, &items));
    // What an adopt, or a workdir set's run, leaves alone stays in the
    // ledger as it is.
    let mut kept: Vec<Entry> = ledger
        .entries
        .iter()
        .filter(|e| places.leaves(e))
        .cloned()
        .collect();
    for (i, item) in items.iter().enumerate() {
        let path = places.expand(&item.path);
        let old = entries_at(&ledger.entries, item, &path);
        if let Some(name) = places.secret(&path) {
            rows.push(row(
                places,
                item,
                &path,
                Action::Failed,
                Some(format!(
                    "it is a secret file of the harness ({name}); pastor never reads or writes it"
                )),
            ));
            kept.extend(owned(&old));
            continue;
        }
        if let Some(refused) = over.refused.remove(&i) {
            rows.push(refused);
            kept.extend(owned(&old));
            continue;
        }
        // Another bundle's key or hook group with this one's value: both
        // own it, so it stays while either wants it. A value that differs
        // from what the other bundle wrote is the user's, not shared.
        let shared = match &item.what {
            What::Key { value, .. } | What::Entry { value, .. } => {
                let want = super::plan::value_sha(value);
                kept.iter().chain(&ledger.entries).any(|e| {
                    e.bundle != item.bundle
                        && Path::new(&e.path) == path.as_path()
                        && e.owned.slot() == item.what.slot()
                        && e.owned.value_sha() == Some(want.as_str())
                })
            }
            _ => false,
        };
        let journal = (!dry_run).then_some(&mut journal);
        let (row, entries) = match &item.what {
            What::Link { target } => link(
                places,
                item,
                &path,
                &places.store.join(target),
                &old,
                journal,
            ),
            What::Block { text } => block(places, item, &path, text, &old, journal),
            What::Key { key, value } => {
                json_key(places, item, &path, key, value, &old, shared, journal)
            }
            What::Entry { key, value } => {
                json_entry(places, item, &path, key, value, &old, shared, journal)
            }
            What::Text { text, mode } => text_file(places, item, &path, text, *mode, &old, journal),
        };
        rows.push(row);
        kept.extend(entries);
        #[cfg(test)]
        tests::crash_point();
    }
    for s in plan
        .skipped
        .iter()
        .filter(|s| places.in_scope(Some(&s.home)))
    {
        rows.push(Row {
            bundle: s.bundle.clone(),
            home: Some(s.home.clone()),
            path: s.what.clone(),
            key: None,
            action: Action::Skipped,
            detail: Some(s.why.clone()),
            old: None,
            backup: None,
        });
    }
    let wanted = |e: &Entry| {
        places.leaves(e)
            || items.iter().any(|i| {
                i.bundle == e.bundle
                    && places.expand(&i.path) == Path::new(&e.path)
                    && i.what.slot() == e.owned.slot()
            })
    };
    let mut dropped: Vec<Vec<&Entry>> = Vec::new();
    for e in ledger.entries.iter().filter(|e| !wanted(e)) {
        match dropped.iter_mut().find(|d| {
            d[0].bundle == e.bundle && d[0].path == e.path && d[0].owned.slot() == e.owned.slot()
        }) {
            Some(d) => d.push(e),
            None => dropped.push(vec![e]),
        }
    }
    for old in &dropped {
        let (row, entries) = remove(places, old, &ledger.entries, &kept, dry_run);
        if entries.is_empty() {
            carry(&mut kept, old);
        }
        rows.push(row);
        kept.extend(entries);
    }
    let meta = places.final_meta(&kept, &rows);
    if !dry_run && (kept != journal.entries || meta != ledger.meta) {
        save_ledger(places, &meta, &kept)?;
    }
    Ok((rows, over.backup))
}

/// What an adopt does before its items: the symlinks it replaces with
/// copies, and the items it cannot take over, with why.
#[derive(Default)]
struct Takeover {
    /// Symlinks on the items' way, top down: each with the first item
    /// behind it (for its row's bundle and home).
    links: Vec<(PathBuf, usize)>,
    /// The links each item has on its way, by the item's index.
    ways: BTreeMap<usize, Vec<PathBuf>>,
    /// An item that cannot be taken over, by its index, with its row.
    refused: BTreeMap<usize, Row>,
    /// The backup dir, once something went in it.
    backup: Option<PathBuf>,
}

/// An adopt's first step: find, for each item in a home it takes over, the
/// symlinks on its way (a dir from `$HOME` down, or the item's own file when
/// it is a file pastor edits) and what it will write over, refuse what holds
/// a secret, and copy all of it to the backup dir, links resolved to what
/// they point to. A copy that fails fails the apply, before anything is
/// written. Nothing to do when this run adopts nothing.
fn takeover(places: &Places, items: &[Item]) -> anyhow::Result<Takeover> {
    let mut over = Takeover::default();
    let mut touch: Vec<PathBuf> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        if !places.adopts(item.home.as_deref()) {
            continue;
        }
        let path = places.expand(&item.path);
        if places.secret(&path).is_some() {
            continue;
        }
        let mut ways = linked_dirs(places, item, &path);
        let file = !matches!(item.what, What::Link { .. });
        if file
            && path
                .symlink_metadata()
                .is_ok_and(|m| m.file_type().is_symlink())
        {
            ways.push(path.clone());
        }
        // What the item writes over: a link item's place holding something
        // that is not its link, a file pastor edits.
        let over_what = match &item.what {
            What::Link { target } => {
                let target = places.store.join(target);
                (path.symlink_metadata().is_ok()
                    && std::fs::read_link(&path).ok().as_deref() != Some(target.as_path()))
                .then(|| path.clone())
            }
            _ => path.exists().then(|| path.clone()),
        };
        let secret = ways
            .iter()
            .chain(over_what.iter())
            .find_map(|w| secret_in(places, w).map(|s| (w, s)));
        if let Some((w, s)) = secret {
            let to = std::fs::read_link(w)
                .map(|t| format!(", a symlink to {},", t.display()))
                .unwrap_or_default();
            over.refused.insert(
                i,
                row(
                    places,
                    item,
                    &path,
                    Action::Linked,
                    Some(format!(
                        "{}{to} holds the secret file {s}, which pastor never reads; adopt does not copy it, so move it aside by hand",
                        places.show(w)
                    )),
                ),
            );
            continue;
        }
        for w in &ways {
            if !over.links.iter().any(|(l, _)| l == w) {
                over.links.push((w.clone(), i));
            }
        }
        touch.extend(ways.iter().cloned());
        touch.extend(over_what);
        over.ways.insert(i, ways);
    }
    // Top down, so a link inside a linked dir is found in its copy.
    over.links.sort_by_key(|(l, _)| l.components().count());
    let Some(dir) = places.adopt.as_ref().and_then(|a| a.backup.clone()) else {
        return Ok(over);
    };
    touch.sort();
    touch.dedup();
    if !touch.is_empty() {
        create_private_dir(&dir)
            .map_err(|e| anyhow::anyhow!("adopt: the backup dir: {e:#}; nothing was written"))?;
    }
    let mut done: Vec<&Path> = Vec::new();
    for p in &touch {
        if done.iter().any(|d| p.starts_with(d)) {
            continue;
        }
        let to = places
            .backup_of(p)
            .expect("an adopt that writes has a backup");
        copy_out(p, &to).map_err(|e| {
            anyhow::anyhow!(
                "adopt: back up {} to {}: {e}; nothing was written",
                places.show(p),
                to.display()
            )
        })?;
        over.backup = Some(dir.clone());
        done.push(p);
    }
    Ok(over)
}

/// The secret file `p` is, or holds when it is a dir, by name only.
fn secret_in(places: &Places, p: &Path) -> Option<String> {
    if let Some(s) = places.secret(p) {
        return Some(s.to_string());
    }
    if !p.is_dir() {
        return None;
    }
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).ok()?.flatten() {
            let at = e.path();
            if let Some(s) = places.secret(&at) {
                return Some(s.to_string());
            }
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(at);
            }
        }
    }
    None
}

/// Copy `from` to `to` for a person to go back to: a file's content, a
/// dir's tree; `from` itself followed when it is a link, links inside it
/// kept as links. The dirs it makes are the user's alone.
fn copy_out(from: &Path, to: &Path) -> std::io::Result<()> {
    let parent = to.parent().expect("a backup path has a parent");
    std::fs::create_dir_all(parent)?;
    match std::fs::metadata(from) {
        Ok(m) if m.is_dir() => copy_tree(from, to, Some(0o600)),
        Ok(_) => write_file(to, &std::fs::read(from)?, 0o600),
        // A link to nothing: keep the link, for where it pointed.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::os::unix::fs::symlink(std::fs::read_link(from)?, to)
        }
        Err(e) => Err(e),
    }
}

/// Copy the tree at `from` (followed when it is a link) to a new `to`,
/// links inside kept as links. With `mode`, files take it and dirs are
/// the user's alone; without, each keeps its own.
fn copy_tree(from: &Path, to: &Path, mode: Option<u32>) -> std::io::Result<()> {
    std::fs::create_dir(to)?;
    let dir_mode = match mode {
        Some(_) => 0o700,
        None => std::fs::metadata(from)?.permissions().mode() & 0o7777,
    };
    std::fs::set_permissions(to, std::fs::Permissions::from_mode(dir_mode))?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let (src, dst) = (e.path(), to.join(e.file_name()));
        let t = e.file_type()?;
        if t.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(&src)?, &dst)?;
        } else if t.is_dir() {
            copy_tree(&src, &dst, mode)?;
        } else {
            let own = e.metadata()?.permissions().mode() & 0o7777;
            write_file(&dst, &std::fs::read(&src)?, mode.unwrap_or(own))?;
        }
    }
    Ok(())
}

/// An adopt's second step: each symlink on the items' way becomes a plain
/// copy of what it points to, which is never written. A link that cannot be
/// replaced leaves the items behind it refused, as `linked`. A dry run says
/// so and changes nothing.
fn unlink(places: &Places, over: &mut Takeover, items: &[Item]) -> Vec<Row> {
    let dry_run = places.adopt.as_ref().is_none_or(|a| a.backup.is_none());
    let mut rows = Vec::new();
    let mut failed: Vec<&Path> = Vec::new();
    for (link, i) in &over.links {
        let to = std::fs::read_link(link).unwrap_or_default();
        let done = if dry_run { Ok(()) } else { replace_link(link) };
        let (action, detail) = match done {
            Ok(()) => (
                Action::Replaced,
                "now a plain copy of what the symlink points to, which is left as it is"
                    .to_string(),
            ),
            Err(e) => {
                failed.push(link);
                (
                    Action::Failed,
                    format!("replacing the symlink with a plain copy: {e}"),
                )
            }
        };
        rows.push(Row {
            key: None,
            old: Some(format!("a symlink to {}", to.display())),
            backup: places.backup_of(link).map(|b| places.show(&b)),
            ..row(places, &items[*i], link, action, Some(detail))
        });
    }
    for (i, ways) in &over.ways {
        if let Some(w) = ways.iter().find(|w| failed.contains(&w.as_path())) {
            over.refused
                .entry(*i)
                .or_insert_with(|| linked_row(places, &items[*i], w));
        }
    }
    rows
}

/// Put a plain copy of what the symlink at `link` points to in its place:
/// a dir's tree (links inside kept), a file's content and mode; a link to
/// nothing just goes.
fn replace_link(link: &Path) -> std::io::Result<()> {
    let tmp = beside(link, "copy");
    match std::fs::metadata(link) {
        Ok(m) if m.is_dir() => {
            copy_tree(link, &tmp, None).inspect_err(|_| {
                let _ = std::fs::remove_dir_all(&tmp);
            })?;
            // A dir cannot be renamed over a link: the link goes first.
            std::fs::remove_file(link)?;
            std::fs::rename(&tmp, link)
        }
        Ok(m) => {
            write_file(&tmp, &std::fs::read(link)?, m.permissions().mode() & 0o7777)?;
            std::fs::rename(&tmp, link).inspect_err(|_| {
                let _ = std::fs::remove_file(&tmp);
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::remove_file(link),
        Err(e) => Err(e),
    }
}

/// How long an apply waits for another to finish.
const LOCK_WAIT: std::time::Duration = if cfg!(test) {
    std::time::Duration::from_millis(200)
} else {
    std::time::Duration::from_secs(60)
};

/// Hold `<state>/applied/apply.lock`, so two applies do not both start from
/// the same ledger.
fn lock(places: &Places) -> anyhow::Result<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    create_private_dir(&places.applied)?;
    let path = places.applied.join("apply.lock");
    let f = crate::edit::open_lock(&path)?;
    let deadline = std::time::Instant::now() + LOCK_WAIT;
    loop {
        // SAFETY: flock on a descriptor this function owns.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(f);
        }
        let e = std::io::Error::last_os_error();
        match e.kind() {
            std::io::ErrorKind::Interrupted => continue,
            std::io::ErrorKind::WouldBlock if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            std::io::ErrorKind::WouldBlock => anyhow::bail!(
                "another bundle apply held {} for {LOCK_WAIT:?}; try again",
                path.display()
            ),
            _ => return Err(anyhow::Error::from(e).context(format!("lock {}", path.display()))),
        }
    }
}

/// `{home}`, `{config_dir}`, `{bundle_dir}` and `{bin}`, which only the
/// machine knows.
fn machine_vars(places: &Places, item: &Item, bundle_dir: &Path) -> BTreeMap<String, String> {
    let mut vars = BTreeMap::new();
    vars.insert("home".to_string(), places.home.display().to_string());
    if let Some(h) = &item.home {
        vars.insert(
            "config_dir".to_string(),
            places.expand(h).display().to_string(),
        );
    }
    vars.insert("bundle_dir".to_string(), bundle_dir.display().to_string());
    vars.insert("bin".to_string(), places.expand(BIN).display().to_string());
    vars
}

fn row(places: &Places, item: &Item, path: &Path, action: Action, detail: Option<String>) -> Row {
    Row {
        bundle: item.bundle.clone(),
        home: item.home.clone(),
        path: places.show(path),
        key: item.what.slot().0.map(|k| k.join(".")),
        action,
        detail,
        old: None,
        backup: None,
    }
}

/// A `replaced` row: the bundle's put in place of `old`, which the adopt
/// backup holds.
fn replaced(places: &Places, item: &Item, path: &Path, old: String) -> Row {
    Row {
        old: Some(old),
        backup: places.backup_of(path).map(|b| places.show(&b)),
        ..row(
            places,
            item,
            path,
            Action::Replaced,
            Some("adopt put the bundle's in place of what was there".into()),
        )
    }
}

/// A user's text or value as a row shows it, at most 80 characters.
fn shorten(text: &str) -> String {
    match text.char_indices().nth(80) {
        Some((i, _)) => format!("{}...", &text[..i]),
        None => text.to_string(),
    }
}

/// The first symlink among `path`'s parent dirs, from `$HOME` down (the
/// home itself is the machine's own business); for a path outside `$HOME`,
/// from the item's home (or the bin dir) down, that dir included.
fn linked_dir(places: &Places, item: &Item, path: &Path) -> Option<PathBuf> {
    linked_dirs(places, item, path).into_iter().next()
}

/// Every symlink among `path`'s parent dirs, as `linked_dir` looks, top
/// down.
fn linked_dirs(places: &Places, item: &Item, path: &Path) -> Vec<PathBuf> {
    let Some(parent) = path.parent() else {
        return Vec::new();
    };
    let mut dirs: Vec<&Path> = if let Target::Workdir(dir) = &places.target {
        // The workdir itself is a real path, so its own parents are
        // resolved already.
        parent
            .ancestors()
            .take_while(|a| a.starts_with(dir) && a != dir)
            .collect()
    } else if parent.starts_with(&places.home) {
        parent
            .ancestors()
            .take_while(|a| *a != places.home)
            .collect()
    } else {
        let base = places.expand(item.home.as_deref().unwrap_or(BIN));
        if !parent.starts_with(&base) {
            return Vec::new();
        }
        parent
            .ancestors()
            .take_while(|a| a.starts_with(&base))
            .collect()
    };
    dirs.reverse();
    dirs.into_iter()
        .filter(|d| {
            d.symlink_metadata()
                .is_ok_and(|m| m.file_type().is_symlink())
        })
        .map(Path::to_path_buf)
        .collect()
}

/// The `linked` row of a file pastor edits at `path` when it, or a dir on
/// its way, is a symlink; never for an adopt, which has made them plain or
/// would have.
fn linked(places: &Places, item: &Item, path: &Path, adopt: bool) -> Option<Row> {
    if adopt {
        return None;
    }
    if let Some(at) = linked_dir(places, item, path) {
        return Some(linked_row(places, item, &at));
    }
    path.symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
        .then(|| linked_row(places, item, path))
}

fn linked_row(places: &Places, item: &Item, at: &Path) -> Row {
    let to = std::fs::read_link(at)
        .map(|t| format!(" to {}", t.display()))
        .unwrap_or_default();
    row(
        places,
        item,
        at,
        Action::Linked,
        Some(format!(
            "{} is a symlink{to}; pastor does not write through it",
            places.show(at)
        )),
    )
}

/// Place the link at `path` to `target`. `journal` is `None` on a dry run.
fn link(
    places: &Places,
    item: &Item,
    path: &Path,
    target: &Path,
    old: &[&Entry],
    journal: Option<&mut Journal>,
) -> (Row, Vec<Entry>) {
    let entry = Entry {
        bundle: item.bundle.clone(),
        home: item.home.clone(),
        path: path.display().to_string(),
        owned: Owned::Link {
            target: target.display().to_string(),
        },
    };
    // An adopt has made the dirs on the way plain, or would have.
    let adopt = places.adopts(item.home.as_deref());
    if !adopt && let Some(at) = linked_dir(places, item, path) {
        return (linked_row(places, item, &at), owned(old));
    }
    let failed = |e: String| (row(places, item, path, Action::Failed, Some(e)), owned(old));
    let write = |journal: Option<&mut Journal>, how: &dyn Fn() -> std::io::Result<()>| {
        let Some(journal) = journal else {
            return Ok(());
        };
        journal.intend(&entry)?;
        how().map_err(|e| e.to_string())
    };
    let action = match path.symlink_metadata() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let made = write(journal, &|| {
                path.parent()
                    .map_or(Ok(()), std::fs::create_dir_all)
                    .and_then(|()| std::os::unix::fs::symlink(target, path))
            });
            if let Err(e) = made {
                return failed(e);
            }
            Action::Added
        }
        Err(e) => return failed(e.to_string()),
        Ok(meta) if meta.file_type().is_symlink() => {
            let now = std::fs::read_link(path).unwrap_or_default();
            if now == target {
                Action::Kept
            } else {
                let ours = old
                    .iter()
                    .any(|e| matches!(&e.owned, Owned::Link { target: t } if Path::new(t) == now));
                if !ours && adopt {
                    if let Err(e) = write(journal, &|| swap_link(path, target)) {
                        return failed(e);
                    }
                    let was = format!("a symlink to {}", now.display());
                    return (replaced(places, item, path, was), vec![entry]);
                }
                if !ours {
                    return (
                        row(
                            places,
                            item,
                            path,
                            Action::Conflict,
                            Some(format!("it is a link to {}, not pastor's", now.display())),
                        ),
                        Vec::new(),
                    );
                }
                if let Err(e) = write(journal, &|| swap_link(path, target)) {
                    return failed(e);
                }
                Action::Updated
            }
        }
        Ok(meta) if adopt => {
            let dir = meta.is_dir();
            let put = write(journal, &|| {
                match dir {
                    true => std::fs::remove_dir_all(path)?,
                    false => std::fs::remove_file(path)?,
                }
                std::os::unix::fs::symlink(target, path)
            });
            if let Err(e) = put {
                return failed(e);
            }
            let was = if dir { "a directory" } else { "a file" };
            return (replaced(places, item, path, was.into()), vec![entry]);
        }
        Ok(_) => {
            return (
                row(
                    places,
                    item,
                    path,
                    Action::Conflict,
                    Some("something that is not pastor's is there".into()),
                ),
                Vec::new(),
            );
        }
    };
    (row(places, item, path, action, None), vec![entry])
}

/// Point the link at `path` to `target`, by a new link renamed over it.
fn swap_link(path: &Path, target: &Path) -> std::io::Result<()> {
    let tmp = beside(path, "link");
    std::os::unix::fs::symlink(target, &tmp)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// A hidden temp path next to `path`.
fn beside(path: &Path, what: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(
        ".{name}.pastor-{what}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}

/// A block's markers, by its tag: `bundle:<name>` in a home,
/// `workdir:<id>` in a working directory (`Places::tag`).
fn markers(tag: &str) -> (String, String) {
    (
        format!("<!-- pastor:{tag} -->"),
        format!("<!-- /pastor:{tag} -->"),
    )
}

/// The block tagged `tag` in `content`: the byte range of its lines, the
/// closing marker's newline included, and the text between the markers.
fn find_block<'a>(content: &'a str, tag: &str) -> Option<(std::ops::Range<usize>, &'a str)> {
    let (open, close) = markers(tag);
    let start = line_at(content, &open)?;
    let body_start = start + open.len() + 1;
    let close_at = body_start + line_at(content.get(body_start..)?, &close)?;
    let mut end = close_at + close.len();
    if content[end..].starts_with('\n') {
        end += 1;
    }
    let body = content[body_start..close_at]
        .strip_suffix('\n')
        .unwrap_or("");
    Some((start..end, body))
}

/// Where a line that is exactly `line` starts in `content`.
fn line_at(content: &str, line: &str) -> Option<usize> {
    content.match_indices(line).map(|(i, _)| i).find(|&i| {
        (i == 0 || content.as_bytes()[i - 1] == b'\n')
            && matches!(content.as_bytes().get(i + line.len()), None | Some(b'\n'))
    })
}

/// `content` with the block tagged `tag` holding `body`, and what was put between
/// the user's text and the block: the block in place (nothing put), or
/// appended after the user's text with a blank line between.
fn put_block(content: &str, tag: &str, body: &str) -> (String, String) {
    let (open, close) = markers(tag);
    let block = format!("{open}\n{body}\n{close}\n");
    if let Some((range, _)) = find_block(content, tag) {
        let put = format!(
            "{}{block}{}",
            &content[..range.start],
            &content[range.end..]
        );
        return (put, String::new());
    }
    let sep = if content.is_empty() || content.ends_with("\n\n") {
        ""
    } else if content.ends_with('\n') {
        "\n"
    } else {
        "\n\n"
    };
    (format!("{content}{sep}{block}"), sep.to_string())
}

/// `content` without the block tagged `tag`, and without the `sep` `put_block`
/// put before it when it is still there: the user's text as it was.
fn drop_block(content: &str, tag: &str, sep: &str) -> String {
    let Some((range, _)) = find_block(content, tag) else {
        return content.to_string();
    };
    let before = &content[..range.start];
    let before = before.strip_suffix(sep).unwrap_or(before);
    format!("{before}{}", &content[range.end..])
}

fn sha(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

const EDITED: &str = "the block was changed since pastor wrote it; it is left as it is";

/// Place `bundle`'s block holding `text` in the file at `path`. `journal`
/// is `None` on a dry run.
fn block(
    places: &Places,
    item: &Item,
    path: &Path,
    text: &str,
    old: &[&Entry],
    journal: Option<&mut Journal>,
) -> (Row, Vec<Entry>) {
    use std::cell::{Cell, RefCell};
    let adopt = places.adopts(item.home.as_deref());
    if let Some(r) = linked(places, item, path, adopt) {
        return (r, owned(old));
    }
    let body = text.trim_end();
    let mut was: Option<String> = None;
    let mut shas = Vec::new();
    let mut was_created = false;
    let mut sep_was = None;
    for e in old {
        if let Owned::Block { sha, created, sep } = &e.owned {
            shas.push(sha.as_str());
            was_created |= *created;
            sep_was.get_or_insert(sep.clone());
        }
    }
    let created = Cell::new(was_created);
    let sep = RefCell::new(sep_was.unwrap_or_default());
    let tag = places.tag(&item.bundle);
    let mut action = Action::Kept;
    let entry = || Entry {
        bundle: item.bundle.clone(),
        home: item.home.clone(),
        path: path.display().to_string(),
        owned: Owned::Block {
            sha: sha(body),
            created: created.get(),
            sep: sep.borrow().clone(),
        },
    };
    let mut journal = journal;
    let dry_run = journal.is_none();
    let result = update_file(
        places,
        path,
        dry_run,
        |content| {
            let Some(content) = content else {
                created.set(true);
                action = Action::Added;
                let (put, s) = put_block("", &tag, body);
                sep.replace(s);
                return Ok(Change::Write(put));
            };
            match find_block(content, &tag) {
                Some((_, now)) if now == body => Ok(Change::Leave),
                Some((_, now)) if shas.contains(&sha(now).as_str()) => {
                    action = Action::Updated;
                    Ok(Change::Write(put_block(content, &tag, body).0))
                }
                Some((_, now)) if adopt => {
                    action = Action::Replaced;
                    was = Some(shorten(now));
                    Ok(Change::Write(put_block(content, &tag, body).0))
                }
                Some(_) => Err(EDITED.to_string()),
                None => {
                    action = Action::Added;
                    let (put, s) = put_block(content, &tag, body);
                    sep.replace(s);
                    Ok(Change::Write(put))
                }
            }
        },
        || match journal.as_deref_mut() {
            Some(j) => j.intend(&entry()),
            None => Ok(()),
        },
    );
    match result {
        Ok(()) => match was {
            Some(was) => (replaced(places, item, path, was), vec![entry()]),
            None => (row(places, item, path, action, None), vec![entry()]),
        },
        Err(Failure::Refused(why)) => (
            row(places, item, path, Action::ChangedByHand, Some(why)),
            owned(old),
        ),
        Err(Failure::Busy) => (row(places, item, path, Action::Busy, None), owned(old)),
        Err(Failure::Io(e)) => (row(places, item, path, Action::Failed, Some(e)), owned(old)),
    }
}

/// Take out what the entries for one place the plan no longer has say
/// pastor put there, if it still holds what pastor wrote and no entry in
/// `kept` (another bundle's) still holds it.
fn remove(
    places: &Places,
    old: &[&Entry],
    all: &[Entry],
    kept: &[Entry],
    dry_run: bool,
) -> (Row, Vec<Entry>) {
    let e = old[0];
    let path = PathBuf::from(&e.path);
    let what = match &e.owned {
        Owned::Key { key, .. } => What::Key {
            key: key.clone(),
            value: serde_json::Value::Null,
        },
        Owned::Entry { key, .. } => What::Entry {
            key: key.clone(),
            value: serde_json::Value::Null,
        },
        Owned::Text { .. } => What::Text {
            text: String::new(),
            mode: None,
        },
        Owned::Link { .. } | Owned::Block { .. } => What::Block {
            text: String::new(),
        },
    };
    let item = Item {
        bundle: e.bundle.clone(),
        home: e.home.clone(),
        path: e.path.clone(),
        what,
    };
    let gone = |action, detail| (row(places, &item, &path, action, detail), Vec::new());
    let left = |action, detail| (row(places, &item, &path, action, detail), owned(old));
    if matches!(e.owned, Owned::Key { .. } | Owned::Entry { .. })
        && let Some(k) = kept
            .iter()
            .find(|k| k.path == e.path && k.owned.slot() == e.owned.slot())
    {
        return gone(
            Action::Removed,
            Some(format!(
                "{} {} sets it too, so it stays",
                places.owner(),
                k.bundle
            )),
        );
    }
    if let Some(at) = linked_dir(places, &item, &path) {
        return (linked_row(places, &item, &at), owned(old));
    }
    if let Owned::Link { .. } = e.owned {
        let ours = |now: &Path| {
            old.iter()
                .any(|e| matches!(&e.owned, Owned::Link { target } if Path::new(target) == now))
        };
        return match std::fs::read_link(&path) {
            Ok(now) if ours(&now) => {
                if !dry_run && let Err(err) = std::fs::remove_file(&path) {
                    return left(Action::Failed, Some(err.to_string()));
                }
                gone(Action::Removed, None)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                gone(Action::Removed, Some("already gone".into()))
            }
            _ => gone(
                Action::ChangedByHand,
                Some("it no longer links where pastor pointed it; it is left as it is".into()),
            ),
        };
    }
    if path
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
    {
        return (linked_row(places, &item, &path), owned(old));
    }
    match &e.owned {
        Owned::Key { .. } | Owned::Entry { .. } => {
            return remove_json(places, &item, &path, old, all, dry_run);
        }
        Owned::Text { .. } => return remove_text(places, &item, &path, old, dry_run),
        Owned::Link { .. } | Owned::Block { .. } => {}
    }
    let mut detail = None;
    let tag = places.tag(&e.bundle);
    // Pastor made the file if any block's entry for it says so: another
    // set's leaving in this same run has not carried it here yet.
    let made_file = all.iter().any(|e| {
        Path::new(&e.path) == path && matches!(e.owned, Owned::Block { created: true, .. })
    });
    let result = update_file(
        places,
        &path,
        dry_run,
        |content| {
            let Some(content) = content else {
                detail = Some("already gone".to_string());
                return Ok(Change::Leave);
            };
            let Some((_, now)) = find_block(content, &tag) else {
                detail = Some("already gone".to_string());
                return Ok(Change::Leave);
            };
            let now = sha(now);
            let Some((sep, created)) = old.iter().find_map(|e| match &e.owned {
                Owned::Block { sha, created, sep } if *sha == now => Some((sep, *created)),
                _ => None,
            }) else {
                return Err(EDITED.to_string());
            };
            let rest = drop_block(content, &tag, sep);
            // A file pastor made and nothing else wrote to goes too.
            Ok(if (created || made_file) && rest.trim().is_empty() {
                Change::Remove
            } else {
                Change::Write(rest)
            })
        },
        || Ok(()),
    );
    match result {
        Ok(()) => {
            if !dry_run && !path.exists() {
                places.prune_dirs(&path);
            }
            gone(Action::Removed, detail)
        }
        Err(Failure::Refused(why)) => gone(Action::ChangedByHand, Some(why)),
        Err(Failure::Busy) => left(Action::Busy, None),
        Err(Failure::Io(err)) => left(Action::Failed, Some(err)),
    }
}

type Object = serde_json::Map<String, serde_json::Value>;

/// The JSON object a settings file holds; no file, or an empty one, holds
/// an empty object.
fn parse_object(content: Option<&str>) -> Result<Object, String> {
    match content {
        None => Ok(Object::new()),
        Some(c) if c.trim().is_empty() => Ok(Object::new()),
        Some(c) => match serde_json::from_str(c) {
            Ok(serde_json::Value::Object(o)) => Ok(o),
            Ok(_) => Err("it does not hold a JSON object; it is left as it is".into()),
            Err(e) => Err(format!("it is not JSON ({e}); it is left as it is")),
        },
    }
}

/// As pastor writes a JSON file: two-space indents, a newline at the end.
fn object_text(o: Object) -> String {
    format!("{:#}\n", serde_json::Value::Object(o))
}

/// The value at `key`, or the first thing on its way that is no object.
fn json_get<'a>(root: &'a Object, key: &[String]) -> Result<Option<&'a serde_json::Value>, String> {
    let mut at = root;
    for (i, k) in key[..key.len() - 1].iter().enumerate() {
        match at.get(k) {
            None => return Ok(None),
            Some(serde_json::Value::Object(o)) => at = o,
            Some(_) => {
                return Err(format!(
                    "{} is not an object; it is left as it is",
                    key[..=i].join(".")
                ));
            }
        }
    }
    Ok(at.get(&key[key.len() - 1]))
}

/// The object `key` sits in, made where missing; with the depth of the
/// first object made. `json_get` has checked nothing on the way is another
/// kind of value.
fn json_parent<'a>(root: &'a mut Object, key: &[String]) -> (&'a mut Object, Option<usize>) {
    let mut made = None;
    let mut at = root;
    for (i, k) in key[..key.len() - 1].iter().enumerate() {
        if !at.contains_key(k) {
            at.insert(k.clone(), serde_json::Value::Object(Object::new()));
            made.get_or_insert(i);
        }
        at = at
            .get_mut(k)
            .and_then(serde_json::Value::as_object_mut)
            .expect("checked by json_get");
    }
    (at, made)
}

/// Put an empty object in place of the first value on `key`'s way that is
/// no object, for an adopt: its dotted key, its depth and what it held.
fn json_clear(root: &mut Object, key: &[String]) -> (String, usize, serde_json::Value) {
    let mut at = root;
    for (i, k) in key[..key.len() - 1].iter().enumerate() {
        let gone = match at.get(k) {
            Some(serde_json::Value::Object(_)) | None => None,
            Some(_) => at.insert(k.clone(), serde_json::Value::Object(Object::new())),
        };
        if let Some(gone) = gone {
            return (key[..=i].join("."), i, gone);
        }
        let Some(serde_json::Value::Object(next)) = at.get_mut(k) else {
            unreachable!("json_get found something on the way that is no object");
        };
        at = next;
    }
    unreachable!("json_get found something on the way that is no object")
}

/// Take out each object or list on `key`'s path, from the deepest up,
/// that is empty and that pastor made: `made(depth)` says whether it made
/// the one at `key[..=depth]`.
fn json_prune(at: &mut Object, key: &[String], depth: usize, made: &dyn Fn(usize) -> bool) {
    if key.len() > 1
        && let Some(serde_json::Value::Object(o)) = at.get_mut(&key[0])
    {
        json_prune(o, &key[1..], depth + 1, made);
    }
    let empty = match at.get(&key[0]) {
        Some(serde_json::Value::Object(o)) => o.is_empty(),
        Some(serde_json::Value::Array(a)) => a.is_empty(),
        _ => false,
    };
    if empty && made(depth) {
        at.remove(&key[0]);
    }
}

fn short_json(v: &serde_json::Value) -> String {
    let text = v.to_string();
    match text.char_indices().nth(80) {
        Some((i, _)) => format!("{}...", &text[..i]),
        None => text,
    }
}

/// The ledger's facts about a key or entry: its SHA-256s, the depth of
/// the first object made, and whether pastor made the file.
fn json_owned(old: &[&Entry]) -> (Vec<String>, Option<usize>, bool) {
    let mut shas = Vec::new();
    let mut made = None;
    let mut created = false;
    for e in old {
        if let Owned::Key {
            sha,
            made: m,
            created: c,
            ..
        }
        | Owned::Entry {
            sha,
            made: m,
            created: c,
            ..
        } = &e.owned
        {
            shas.push(sha.clone());
            made = made.or(*m);
            created |= *c;
        }
    }
    (shas, made, created)
}

/// How many objects, or for an entry objects and its list, a key or entry
/// pastor set sits in.
fn containers(owned: &Owned) -> usize {
    match owned {
        Owned::Key { key, .. } => key.len() - 1,
        Owned::Entry { key, .. } => key.len(),
        Owned::Link { .. } | Owned::Block { .. } | Owned::Text { .. } => 0,
    }
}

/// Hand what the entries leaving the ledger say pastor made (the file, and
/// the objects or list on their way) to the entries left for that file
/// that sit in those, so taking those out later takes them out too.
fn carry(kept: &mut [Entry], gone: &[&Entry]) {
    for g in gone {
        // A file pastor made for a block goes with the last block in it.
        if let Owned::Block { created: true, .. } = &g.owned {
            for k in kept.iter_mut().filter(|k| k.path == g.path) {
                if let Owned::Block { created, .. } = &mut k.owned {
                    *created = true;
                }
            }
            continue;
        }
        let (Owned::Key {
            key: gkey,
            made: gmade,
            created: gcreated,
            ..
        }
        | Owned::Entry {
            key: gkey,
            made: gmade,
            created: gcreated,
            ..
        }) = &g.owned
        else {
            continue;
        };
        let gn = containers(&g.owned);
        for k in kept.iter_mut().filter(|k| k.path == g.path) {
            let n = containers(&k.owned);
            let (Owned::Key {
                key, made, created, ..
            }
            | Owned::Entry {
                key, made, created, ..
            }) = &mut k.owned
            else {
                continue;
            };
            *created |= *gcreated;
            // `k` holds that pastor made its containers from `made` down;
            // the ones above that it does not claim yet must all be on
            // `g`'s way, from where `g` made them.
            if let Some(d) = *gmade
                && let Some(t) = made.unwrap_or(n).checked_sub(1)
                && d <= t
                && t < gn
                && key[..=t] == gkey[..=t]
            {
                *made = Some(d);
            }
        }
    }
}

/// Set `key` in the JSON file at `path` to `value`. A key the user holds
/// with another value is theirs: a `conflict`, left as it is. `journal` is
/// `None` on a dry run.
#[allow(clippy::too_many_arguments)]
fn json_key(
    places: &Places,
    item: &Item,
    path: &Path,
    key: &[String],
    value: &serde_json::Value,
    old: &[&Entry],
    shared: bool,
    journal: Option<&mut Journal>,
) -> (Row, Vec<Entry>) {
    json_edit(places, item, path, key, value, old, shared, journal, false)
}

/// Add `value` to the list at `key` in the JSON file at `path`, unless it
/// is there; one the user had is not recorded, so it never comes out.
/// `shared`: another bundle's ledger entry holds it, so it is pastor's.
#[allow(clippy::too_many_arguments)]
fn json_entry(
    places: &Places,
    item: &Item,
    path: &Path,
    key: &[String],
    value: &serde_json::Value,
    old: &[&Entry],
    shared: bool,
    journal: Option<&mut Journal>,
) -> (Row, Vec<Entry>) {
    json_edit(places, item, path, key, value, old, shared, journal, true)
}

#[allow(clippy::too_many_arguments)]
fn json_edit(
    places: &Places,
    item: &Item,
    path: &Path,
    key: &[String],
    value: &serde_json::Value,
    old: &[&Entry],
    shared: bool,
    journal: Option<&mut Journal>,
    entry: bool,
) -> (Row, Vec<Entry>) {
    use std::cell::{Cell, RefCell};
    let adopt = places.adopts(item.home.as_deref());
    if let Some(r) = linked(places, item, path, adopt) {
        return (r, owned(old));
    }
    let (shas, made_was, created_was) = json_owned(old);
    let was: RefCell<Option<String>> = RefCell::new(None);
    // The depth of an object an adopt put in place of another value.
    let cleared: Cell<Option<usize>> = Cell::new(None);
    let lowest = |a: Option<usize>, b: Option<usize>| match (a, b) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let want = super::plan::value_sha(value);
    let whole = !entry && places.whole.iter().any(|w| w == key);
    // What a whole-owned key had that is not in the value: someone else's.
    let dropped = Cell::new(0usize);
    let action = Cell::new(Action::Kept);
    let record = Cell::new(false);
    let made = Cell::new(made_was);
    let created = Cell::new(created_was);
    let make = || {
        let (key, sha, made, created) = (key.to_vec(), want.clone(), made.get(), created.get());
        Entry {
            bundle: item.bundle.clone(),
            home: item.home.clone(),
            path: path.display().to_string(),
            owned: if entry {
                Owned::Entry {
                    key,
                    sha,
                    made,
                    created,
                }
            } else {
                Owned::Key {
                    key,
                    sha,
                    made,
                    created,
                }
            },
        }
    };
    let mut journal = journal;
    let dry_run = journal.is_none();
    let refuse = |a: Action, why: String| {
        action.set(a);
        why
    };
    let result = update_file(
        places,
        path,
        dry_run,
        |content| {
            let mut root = parse_object(content).map_err(|why| refuse(Action::Failed, why))?;
            created.set(created_was || content.is_none());
            was.replace(None);
            cleared.set(None);
            // An adopt makes room where something on the way is no object.
            if adopt && json_get(&root, key).is_err() {
                let (at, depth, gone) = json_clear(&mut root, key);
                cleared.set(Some(depth));
                was.replace(Some(format!("{at}: {}", short_json(&gone))));
            }
            let now = json_get(&root, key).map_err(|why| refuse(Action::Conflict, why))?;
            if entry {
                match now {
                    Some(serde_json::Value::Array(list)) if list.contains(value) => {
                        action.set(Action::Kept);
                        record.set(!old.is_empty() || shared || adopt);
                        return Ok(Change::Leave);
                    }
                    Some(serde_json::Value::Array(_)) | None => {}
                    Some(now) if adopt => {
                        was.replace(Some(short_json(now)));
                        json_parent(&mut root, key)
                            .0
                            .insert(key[key.len() - 1].clone(), serde_json::json!([value]));
                        made.set(made.get().or(Some(key.len() - 1)));
                        record.set(true);
                        return Ok(Change::Write(object_text(root)));
                    }
                    Some(_) => {
                        return Err(refuse(
                            Action::Conflict,
                            format!("{} is not a list; it is left as it is", key.join(".")),
                        ));
                    }
                }
                let (parent, m) = json_parent(&mut root, key);
                let last = &key[key.len() - 1];
                if !parent.contains_key(last) {
                    parent.insert(last.clone(), serde_json::Value::Array(Vec::new()));
                    made.set(lowest(m.or(Some(key.len() - 1)), cleared.get()));
                }
                parent[last.as_str()]
                    .as_array_mut()
                    .expect("a list")
                    .push(value.clone());
                action.set(Action::Added);
                record.set(true);
                return Ok(Change::Write(object_text(root)));
            }
            match now {
                Some(now) if now == value => {
                    action.set(Action::Kept);
                    record.set(!old.is_empty() || shared || adopt);
                    Ok(Change::Leave)
                }
                Some(now) if adopt && !shas.contains(&super::plan::value_sha(now)) => {
                    was.replace(Some(short_json(now)));
                    record.set(true);
                    json_parent(&mut root, key)
                        .0
                        .insert(key[key.len() - 1].clone(), value.clone());
                    Ok(Change::Write(object_text(root)))
                }
                Some(now) if shas.contains(&super::plan::value_sha(now)) => {
                    action.set(Action::Updated);
                    record.set(true);
                    json_parent(&mut root, key)
                        .0
                        .insert(key[key.len() - 1].clone(), value.clone());
                    Ok(Change::Write(object_text(root)))
                }
                Some(now) if whole && !old.is_empty() => {
                    dropped.set(dropped_from(now, value));
                    action.set(Action::Updated);
                    record.set(true);
                    json_parent(&mut root, key)
                        .0
                        .insert(key[key.len() - 1].clone(), value.clone());
                    Ok(Change::Write(object_text(root)))
                }
                Some(_) if !old.is_empty() => Err(refuse(
                    Action::ChangedByHand,
                    "it was changed since pastor set it; it is left as it is".into(),
                )),
                Some(_) => Err(refuse(
                    Action::Conflict,
                    format!(
                        "it holds another value, which stays; the {} wants {}",
                        places.owner(),
                        short_json(value)
                    ),
                )),
                None => {
                    let (parent, m) = json_parent(&mut root, key);
                    parent.insert(key[key.len() - 1].clone(), value.clone());
                    made.set(lowest(m, cleared.get()));
                    action.set(Action::Added);
                    record.set(true);
                    Ok(Change::Write(object_text(root)))
                }
            }
        },
        || match journal.as_deref_mut() {
            Some(j) => j.intend(&make()),
            None => Ok(()),
        },
    );
    let recorded = || {
        if record.get() {
            vec![make()]
        } else {
            Vec::new()
        }
    };
    match result {
        Ok(()) => match was.take() {
            Some(was) => (replaced(places, item, path, was), recorded()),
            None => {
                let said = (dropped.get() > 0).then(|| format!("{DROPPED}: {}", dropped.get()));
                (row(places, item, path, action.get(), said), recorded())
            }
        },
        Err(Failure::Refused(why)) => {
            let kept = if action.get() == Action::Conflict {
                Vec::new()
            } else {
                owned(old)
            };
            (row(places, item, path, action.get(), Some(why)), kept)
        }
        Err(Failure::Busy) => (row(places, item, path, Action::Busy, None), owned(old)),
        Err(Failure::Io(e)) => (row(places, item, path, Action::Failed, Some(e)), owned(old)),
    }
}

/// What a row says of rules in a whole-owned list that the agent added since
/// and the new value does not have: replaced with the rest.
pub const DROPPED: &str = "agent_rules_dropped";

/// How many entries of the list `now` are not in the list `value`.
fn dropped_from(now: &serde_json::Value, value: &serde_json::Value) -> usize {
    match (now, value) {
        (serde_json::Value::Array(now), serde_json::Value::Array(value)) => {
            now.iter().filter(|v| !value.contains(v)).count()
        }
        _ => 0,
    }
}

/// Write the whole file at `path`, pastor's own, as `text`.
fn text_file(
    places: &Places,
    item: &Item,
    path: &Path,
    text: &str,
    mode: Option<u32>,
    old: &[&Entry],
    journal: Option<&mut Journal>,
) -> (Row, Vec<Entry>) {
    let adopt = places.adopts(item.home.as_deref());
    if !adopt && let Some(at) = linked_dir(places, item, path) {
        return (linked_row(places, item, &at), owned(old));
    }
    // A workdir's file is never one an agent can point elsewhere.
    if places.target != Target::Home
        && let Some(row) = linked(places, item, path, false)
    {
        return (row, owned(old));
    }
    let shas: Vec<&str> = old
        .iter()
        .filter_map(|e| match &e.owned {
            Owned::Text { sha } => Some(sha.as_str()),
            _ => None,
        })
        .collect();
    let mut was = None;
    let entry = Entry {
        bundle: item.bundle.clone(),
        home: item.home.clone(),
        path: path.display().to_string(),
        owned: Owned::Text { sha: sha(text) },
    };
    let mut action = Action::Kept;
    let mut journal = journal;
    let dry_run = journal.is_none();
    // The mode the file has, when it is not the one asked for.
    let wrong_mode = |path: &Path| {
        let want = mode?;
        let has = path.metadata().ok()?.permissions().mode() & 0o777;
        (has != want).then_some((has, want))
    };
    let mut chmod = None;
    let result = update_file_mode(
        places,
        path,
        dry_run,
        mode,
        |content| match content {
            None => {
                action = Action::Added;
                Ok(Change::Write(text.to_string()))
            }
            Some(now) if now == text => match wrong_mode(path) {
                Some((_, want)) if shas.contains(&sha(now).as_str()) => {
                    action = Action::Updated;
                    chmod = Some(want);
                    Ok(Change::Leave)
                }
                // Not pastor's to change: as it is, a hook may not run.
                Some((has, want)) if shas.is_empty() && places.target != Target::Home => {
                    Err(format!(
                        "it is there already as the set has it, but with mode {has:04o}, not {want:04o}; pastor changes only what it wrote"
                    ))
                }
                _ => Ok(Change::Leave),
            },
            Some(now) if shas.contains(&sha(now).as_str()) => {
                action = Action::Updated;
                Ok(Change::Write(text.to_string()))
            }
            Some(now) if adopt => {
                was = Some(shorten(now));
                Ok(Change::Write(text.to_string()))
            }
            Some(_) if !shas.is_empty() => Err(CHANGED.into()),
            Some(_) => Err("something that is not pastor's is there".into()),
        },
        || match journal.as_deref_mut() {
            Some(j) => j.intend(&entry),
            None => Ok(()),
        },
    );
    if let (Ok(()), Some(want), false) = (&result, chmod, dry_run)
        && let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(want))
    {
        return (
            row(places, item, path, Action::Failed, Some(e.to_string())),
            owned(old),
        );
    }
    // The same file there already, in a workdir, is not pastor's: it may
    // be the repository's own, and pastor takes out only what it wrote.
    let found = action == Action::Kept && shas.is_empty() && places.target != Target::Home;
    match result {
        Ok(()) if found => (
            row(
                places,
                item,
                path,
                Action::Kept,
                Some(
                    "it was there already, as the set has it; it stays when the set drops it"
                        .into(),
                ),
            ),
            Vec::new(),
        ),
        Ok(()) => match was {
            Some(was) => (replaced(places, item, path, was), vec![entry]),
            None => (row(places, item, path, action, None), vec![entry]),
        },
        // Pastor wrote it once: it stays pastor's to take out, once it is
        // back to what pastor wrote.
        Err(Failure::Refused(why)) if why == CHANGED => (
            row(places, item, path, Action::ChangedByHand, Some(why)),
            owned(old),
        ),
        Err(Failure::Refused(why)) => (
            row(places, item, path, Action::Conflict, Some(why)),
            Vec::new(),
        ),
        Err(Failure::Busy) => (row(places, item, path, Action::Busy, None), owned(old)),
        Err(Failure::Io(e)) => (row(places, item, path, Action::Failed, Some(e)), owned(old)),
    }
}

/// Why a whole file pastor wrote is left as it is.
const CHANGED: &str = "it was changed since pastor wrote it; it is left as it is";

/// Take a key or a list entry pastor set out of a JSON file, if it still
/// holds what pastor wrote, with the objects pastor made for it once they
/// are empty, and the file once it is empty if pastor made it.
fn remove_json(
    places: &Places,
    item: &Item,
    path: &Path,
    old: &[&Entry],
    all: &[Entry],
    dry_run: bool,
) -> (Row, Vec<Entry>) {
    let (shas, _, _) = json_owned(old);
    // An object or list on the way, or the file, may have been made for
    // another key pastor set there: any entry for this file counts.
    let file: Vec<(&[String], Option<usize>, bool)> = all
        .iter()
        .filter(|e| Path::new(&e.path) == path)
        .filter_map(|e| match &e.owned {
            Owned::Key {
                key, made, created, ..
            }
            | Owned::Entry {
                key, made, created, ..
            } => Some((key.as_slice(), *made, *created)),
            _ => None,
        })
        .collect();
    let created = file.iter().any(|(_, _, c)| *c);
    let (key, entry) = match &old[0].owned {
        Owned::Key { key, .. } => (key.as_slice(), false),
        Owned::Entry { key, .. } => (key.as_slice(), true),
        _ => unreachable!("remove_json takes keys and entries"),
    };
    // A whole-owned key goes with what the agent added to it.
    let whole = !entry && places.whole.iter().any(|w| w == key);
    let mut action = Action::Removed;
    let mut detail = None;
    let result = update_file(
        places,
        path,
        dry_run,
        |content| {
            let Some(content) = content else {
                detail = Some("already gone".to_string());
                return Ok(Change::Leave);
            };
            let mut root = parse_object(Some(content)).inspect_err(|_| action = Action::Failed)?;
            let ours = |v: &serde_json::Value| shas.contains(&super::plan::value_sha(v));
            match json_get(&root, key) {
                Ok(Some(serde_json::Value::Array(list))) if entry && list.iter().any(ours) => {
                    let (parent, _) = json_parent(&mut root, key);
                    let list = parent
                        .get_mut(&key[key.len() - 1])
                        .and_then(serde_json::Value::as_array_mut)
                        .expect("a list");
                    let at = list.iter().rposition(ours).expect("found above");
                    list.remove(at);
                }
                Ok(Some(now)) if !entry && (ours(now) || whole) => {
                    json_parent(&mut root, key).0.remove(&key[key.len() - 1]);
                }
                Ok(Some(_)) if !entry => {
                    action = Action::ChangedByHand;
                    return Err("it was changed since pastor set it; it is left as it is".into());
                }
                _ => {
                    detail = Some(if entry {
                        "no longer there as pastor wrote it".to_string()
                    } else {
                        "already gone".to_string()
                    });
                    return Ok(Change::Leave);
                }
            }
            let prune = if entry { key } else { &key[..key.len() - 1] };
            let made = |depth: usize| {
                file.iter().any(|(k, m, _)| {
                    m.is_some_and(|m| m <= depth)
                        && k.len() > depth
                        && k[..=depth] == prune[..=depth]
                })
            };
            if !prune.is_empty() {
                json_prune(&mut root, prune, 0, &made);
            }
            Ok(if created && root.is_empty() {
                Change::Remove
            } else {
                Change::Write(object_text(root))
            })
        },
        || Ok(()),
    );
    match result {
        Ok(()) => {
            if !dry_run && !path.exists() {
                places.prune_dirs(path);
            }
            (row(places, item, path, Action::Removed, detail), Vec::new())
        }
        Err(Failure::Refused(why)) if action == Action::ChangedByHand => (
            row(places, item, path, Action::ChangedByHand, Some(why)),
            Vec::new(),
        ),
        Err(Failure::Refused(why)) => (
            row(places, item, path, Action::Failed, Some(why)),
            owned(old),
        ),
        Err(Failure::Busy) => (row(places, item, path, Action::Busy, None), owned(old)),
        Err(Failure::Io(e)) => (row(places, item, path, Action::Failed, Some(e)), owned(old)),
    }
}

/// Take out a whole file pastor wrote, if it still holds what pastor wrote,
/// and the dirs it leaves empty, up to the marketplaces dir's parent.
fn remove_text(
    places: &Places,
    item: &Item,
    path: &Path,
    old: &[&Entry],
    dry_run: bool,
) -> (Row, Vec<Entry>) {
    let shas: Vec<&str> = old
        .iter()
        .filter_map(|e| match &e.owned {
            Owned::Text { sha } => Some(sha.as_str()),
            _ => None,
        })
        .collect();
    let mut detail = None;
    let result = update_file(
        places,
        path,
        dry_run,
        |content| match content {
            None => {
                detail = Some("already gone".to_string());
                Ok(Change::Leave)
            }
            Some(now) if shas.contains(&sha(now).as_str()) => Ok(Change::Remove),
            Some(_) => Err(CHANGED.into()),
        },
        || Ok(()),
    );
    match result {
        Ok(()) => {
            if !dry_run {
                places.prune_dirs(path);
            }
            (row(places, item, path, Action::Removed, detail), Vec::new())
        }
        Err(Failure::Refused(why)) => (
            row(places, item, path, Action::ChangedByHand, Some(why)),
            Vec::new(),
        ),
        Err(Failure::Busy) => (row(places, item, path, Action::Busy, None), owned(old)),
        Err(Failure::Io(e)) => (row(places, item, path, Action::Failed, Some(e)), owned(old)),
    }
}

/// What `update_file`'s edit decides.
enum Change {
    Leave,
    Write(String),
    Remove,
}

enum Failure {
    /// `edit` said no, with why.
    Refused(String),
    Busy,
    Io(String),
}

/// Change the file at `path` by `edit`, which gets its content (`None`
/// when there is none) and decides, or says why not. Backed up first (once
/// a run), `before_write` called, then written beside it and renamed over
/// it, only if it still holds what `edit` saw. A dry run reads and decides
/// only.
fn update_file(
    places: &Places,
    path: &Path,
    dry_run: bool,
    edit: impl FnMut(Option<&str>) -> Result<Change, String>,
    before_write: impl FnMut() -> Result<(), String>,
) -> Result<(), Failure> {
    update_file_mode(places, path, dry_run, None, edit, before_write)
}

/// `update_file`, writing the file with `mode` when it is given.
fn update_file_mode(
    places: &Places,
    path: &Path,
    dry_run: bool,
    mode: Option<u32>,
    mut edit: impl FnMut(Option<&str>) -> Result<Change, String>,
    mut before_write: impl FnMut() -> Result<(), String>,
) -> Result<(), Failure> {
    let io = |e: std::io::Error| Failure::Io(e.to_string());
    for _ in 0..TRIES {
        let before = read(path).map_err(io)?;
        let change = edit(before.as_deref()).map_err(Failure::Refused)?;
        let after = match change {
            Change::Leave => return Ok(()),
            Change::Remove if before.is_none() => return Ok(()),
            Change::Remove => None,
            Change::Write(after) => Some(after),
        };
        if dry_run {
            return Ok(());
        }
        if let Some(old) = &before {
            backup(places, path, old).map_err(|e| Failure::Io(format!("backup: {e:#}")))?;
        }
        before_write().map_err(Failure::Io)?;
        let mode = mode.unwrap_or_else(|| {
            path.metadata()
                .map(|m| m.permissions().mode() & 0o777)
                .unwrap_or(0o644)
        });
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(io)?;
        }
        let tmp = beside(path, "tmp");
        if let Some(after) = &after {
            write_file(&tmp, after.as_bytes(), mode).map_err(io)?;
        }
        // Compare and swap: a harness may have saved the file meanwhile.
        if read(path).map_err(io)? != before {
            let _ = std::fs::remove_file(&tmp);
            continue;
        }
        let done = match after {
            Some(_) => std::fs::rename(&tmp, path),
            None => std::fs::remove_file(path),
        };
        if let Err(e) = done {
            let _ = std::fs::remove_file(&tmp);
            return Err(io(e));
        }
        return Ok(());
    }
    Err(Failure::Busy)
}

fn read(path: &Path) -> std::io::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Write a new file with `mode`, whatever the umask, synced to disk.
fn write_file(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?;
    f.set_permissions(std::fs::Permissions::from_mode(mode))?;
    f.write_all(bytes)?;
    f.sync_all()
}

fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    let tmp = beside(path, "tmp");
    write_file(&tmp, bytes, mode)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Copy what `path` holds to the backup dir, once a run: the first copy
/// ever as `<path with / as %>.first`, kept for good, as the file was
/// before pastor changed it; later ones as `<...>.<time>`, the last
/// `BACKUPS` kept.
fn backup(places: &Places, path: &Path, content: &str) -> anyhow::Result<()> {
    if !places.backed_up.borrow_mut().insert(path.to_path_buf()) {
        return Ok(());
    }
    let dir = places.applied.join("backup");
    create_private_dir(&dir)?;
    let stem = path.display().to_string().replace('/', "%");
    let first = dir.join(format!("{stem}.first"));
    if !first.exists() {
        write_file(&first, content.as_bytes(), 0o600)?;
        return Ok(());
    }
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.6fZ");
    write_file(
        &dir.join(format!("{stem}.{stamp}")),
        content.as_bytes(),
        0o600,
    )?;
    let mut olds: Vec<String> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| {
            n.strip_prefix(&stem)
                .and_then(|rest| rest.strip_prefix('.'))
                .is_some_and(|stamp| stamp.starts_with(|c: char| c.is_ascii_digit()))
        })
        .collect();
    olds.sort();
    let extra = olds.len().saturating_sub(BACKUPS);
    for name in &olds[..extra] {
        let _ = std::fs::remove_file(dir.join(name));
    }
    Ok(())
}

/// Copy a version from the rendered set into the store, by a rename, and
/// set the modes the bundle gives.
fn install_version(from: &Path, to: &Path, modes: &BTreeMap<String, u32>) -> anyhow::Result<()> {
    let parent = to.parent().expect("a version dir has a parent");
    create_private_dir(parent)?;
    let tmp = beside(to, "new");
    let files = super::store::files(from).map_err(|e| anyhow::anyhow!("{e}"))?;
    for rel in &files {
        let dst = tmp.join(rel);
        std::fs::create_dir_all(dst.parent().expect("a file has a parent"))?;
        std::fs::copy(from.join(rel), &dst)?;
        if let Some(mode) = modes.get(rel) {
            std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(*mode))?;
        }
    }
    std::fs::rename(&tmp, to).inspect_err(|_| {
        let _ = std::fs::remove_dir_all(&tmp);
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::plan::tests::{FLOCK, PASTOR_TOML, add_base, add_team_rules};
    use std::cell::{Cell, RefCell};

    thread_local! {
        /// Items left before `apply_local` stops as a killed run would.
        static CRASH_AFTER: Cell<Option<usize>> = const { Cell::new(None) };
        /// Called after each item, to look at the machine mid-run.
        static AFTER_ITEM: RefCell<Option<Box<dyn Fn()>>> = const { RefCell::new(None) };
    }

    pub(super) fn crash_point() {
        AFTER_ITEM.with(|f| {
            if let Some(f) = &*f.borrow() {
                f()
            }
        });
        CRASH_AFTER.with(|c| match c.get() {
            Some(0) => {
                c.set(None);
                panic!("the run was cut short");
            }
            Some(n) => c.set(Some(n - 1)),
            None => {}
        });
    }
    use crate::config::PastorConfig;
    use crate::config::flock::Flock;

    /// A head and a machine in one temp dir: the head's config and state,
    /// and the machine's home; pastor's dirs are shared, as on a local
    /// machine.
    struct Machine {
        tmp: tempfile::TempDir,
        paths: Paths,
        home: PathBuf,
    }

    impl Machine {
        fn new() -> Machine {
            let tmp = tempfile::tempdir().unwrap();
            let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
            let home = tmp.path().join("h");
            std::fs::create_dir_all(&home).unwrap();
            add_team_rules(&paths, tmp.path());
            Machine { tmp, paths, home }
        }

        /// Render build-01's plan from `flock` and apply it.
        fn apply(&self, flock: &str, dry_run: bool) -> Report {
            self.apply_with(PASTOR_TOML, flock, dry_run)
        }

        fn apply_with(&self, pastor_toml: &str, flock: &str, dry_run: bool) -> Report {
            self.try_apply(pastor_toml, flock, dry_run).unwrap()
        }

        fn render(&self, pastor_toml: &str, flock: &str) -> Result<Plan, String> {
            let config = PastorConfig::parse(Path::new("pastor.toml"), pastor_toml).unwrap();
            let flock = Flock::parse(Path::new("flock.toml"), flock).unwrap();
            let parts = crate::machine_check::Config {
                defaults: &config.defaults,
                agents: &config.agents,
                models: &config.models,
                profiles: &config.profiles,
                harnesses: crate::harness_manifest::Harnesses::builtin(),
                herdr_pin: None,
                head_address: None,
            };
            crate::bundle::plan::render(&self.paths, &flock, &parts, flock.get("build-01").unwrap())
        }

        fn try_apply(
            &self,
            pastor_toml: &str,
            flock: &str,
            dry_run: bool,
        ) -> anyhow::Result<Report> {
            self.try_apply_as(pastor_toml, flock, dry_run, None)
        }

        fn try_apply_as(
            &self,
            pastor_toml: &str,
            flock: &str,
            dry_run: bool,
            adopt: Option<&Adopt>,
        ) -> anyhow::Result<Report> {
            let config = PastorConfig::parse(Path::new("pastor.toml"), pastor_toml).unwrap();
            let flock = Flock::parse(Path::new("flock.toml"), flock).unwrap();
            let parts = crate::machine_check::Config {
                defaults: &config.defaults,
                agents: &config.agents,
                models: &config.models,
                profiles: &config.profiles,
                harnesses: crate::harness_manifest::Harnesses::builtin(),
                herdr_pin: None,
                head_address: None,
            };
            let plan = crate::bundle::plan::render(
                &self.paths,
                &flock,
                &parts,
                flock.get("build-01").unwrap(),
            )
            .unwrap();
            let set = tempfile::tempdir_in(self.tmp.path()).unwrap();
            crate::bundle::push::write_set(&self.paths, set.path(), &plan).unwrap();
            apply_local(set.path(), &self.paths, &self.home, dry_run, adopt)
        }

        fn at(&self, rel: &str) -> PathBuf {
            self.home.join(rel)
        }

        fn actions(report: &Report) -> Vec<(String, Action)> {
            report
                .rows
                .iter()
                .map(|r| (r.path.clone(), r.action))
                .collect()
        }

        fn ledger(&self) -> Option<String> {
            std::fs::read_to_string(self.paths.state_dir.join("applied/ledger.json")).ok()
        }

        fn backups(&self) -> usize {
            std::fs::read_dir(self.paths.state_dir.join("applied/backup")).map_or(0, |d| d.count())
        }

        fn backup(&self, name: &str) -> Option<String> {
            let stem = self.at(CLAUDE_MD).display().to_string().replace('/', "%");
            std::fs::read_to_string(
                self.paths
                    .state_dir
                    .join("applied/backup")
                    .join(format!("{stem}.{name}")),
            )
            .ok()
        }

        /// Add the bundle again with these instructions: a new version.
        fn new_instructions(&self, text: &str) {
            let src = self.tmp.path().join("src/team-rules");
            std::fs::write(src.join("instructions/team.md"), text).unwrap();
            crate::bundle::store::add(&self.paths, &crate::bundle::store::Source::Dir(src))
                .unwrap();
        }

        fn row<'a>(report: &'a Report, path_end: &str) -> &'a Row {
            report
                .rows
                .iter()
                .find(|r| r.path.ends_with(path_end))
                .unwrap_or_else(|| panic!("no row for {path_end}: {report:?}"))
        }

        /// Add the bundle again with this skill text: a new version.
        fn new_version(&self, skill: &str) {
            let src = self.tmp.path().join("src/team-rules");
            std::fs::write(src.join("skills/reviewing-a-pr/SKILL.md"), skill).unwrap();
            crate::bundle::store::add(&self.paths, &crate::bundle::store::Source::Dir(src))
                .unwrap();
        }
    }

    const CLAUDE_MD: &str = ".claude-main/CLAUDE.md";
    const USER_TEXT: &str = "# Mine\n\nKeep this.\n";

    /// The first apply places everything and keeps the user's text; the
    /// second finds it all in place and writes nothing.
    #[test]
    fn apply_then_apply_again_changes_nothing() {
        let m = Machine::new();
        std::fs::create_dir_all(m.at(".claude-main")).unwrap();
        std::fs::write(m.at(CLAUDE_MD), USER_TEXT).unwrap();
        let first = m.apply(FLOCK, false);
        assert!(first.ok(), "{first:?}");
        let hash = &crate::bundle::store::read_versions(&m.paths, "team-rules").unwrap()[0].hash;
        let store = m.paths.data_dir().join("bundles/team-rules").join(hash);
        let actions = Machine::actions(&first);
        assert_eq!(actions.len(), 5, "{actions:?}");
        assert!(
            actions.iter().all(|(_, a)| *a == Action::Added),
            "{actions:?}"
        );
        assert_eq!(
            std::fs::read_link(m.at(".claude-main/skills/reviewing-a-pr")).unwrap(),
            store.join("skills/reviewing-a-pr")
        );
        assert_eq!(
            std::fs::read_to_string(m.at(".claude-main/skills/reviewing-a-pr/SKILL.md")).unwrap(),
            "Read the diff.\n"
        );
        let hook = std::fs::metadata(m.at(".claude-main/hooks/notify.sh")).unwrap();
        assert_eq!(hook.permissions().mode() & 0o777, 0o755);
        assert!(
            m.at(".local/bin/team-tool")
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let md = std::fs::read_to_string(m.at(CLAUDE_MD)).unwrap();
        let config_dir = m.at(".claude-main");
        assert_eq!(
            md,
            format!(
                "{USER_TEXT}\n<!-- pastor:bundle:team-rules -->\nTickets: https://tickets.example.com. You are claude-main on build-01 in ops; your home is {}. {{not_a_var}}\n\nThis one has the GPUs.\n<!-- /pastor:bundle:team-rules -->\n",
                config_dir.display()
            )
        );
        let ledger = m.ledger().unwrap();
        assert_eq!(
            serde_json::from_str::<Ledger>(&ledger)
                .unwrap()
                .entries
                .len(),
            4
        );
        assert_eq!(m.backups(), 1, "the user's CLAUDE.md, before the block");

        let again = m.apply(FLOCK, false);
        assert!(!again.changes(), "{again:?}");
        assert!(
            again.rows.iter().all(|r| r.action == Action::Kept),
            "{again:?}"
        );
        assert_eq!(m.ledger().unwrap(), ledger);
        assert_eq!(m.backups(), 1);
    }

    /// A dry run says what it would place and writes nothing at all.
    #[test]
    fn a_dry_run_writes_nothing() {
        let m = Machine::new();
        let report = m.apply(FLOCK, true);
        assert!(report.dry_run);
        assert_eq!(
            report
                .rows
                .iter()
                .filter(|r| r.action == Action::Added)
                .count(),
            5
        );
        assert!(!m.at(".claude-main").exists());
        assert!(!m.at(".local").exists());
        assert!(!m.paths.data_dir().join("bundles").exists());
        assert!(m.ledger().is_none());
    }

    /// A home dir or an instruction file that is a symlink is not written
    /// through: its rows say so, and its target stays as it was.
    #[test]
    fn a_symlink_is_not_written_through() {
        let m = Machine::new();
        let dotfiles = m.tmp.path().join("dotfiles");
        std::fs::create_dir_all(dotfiles.join("skills")).unwrap();
        std::fs::write(dotfiles.join("CLAUDE.md"), "dotfiles\n").unwrap();
        std::fs::create_dir_all(m.at(".claude-main")).unwrap();
        std::os::unix::fs::symlink(dotfiles.join("skills"), m.at(".claude-main/skills")).unwrap();
        std::os::unix::fs::symlink(dotfiles.join("CLAUDE.md"), m.at(CLAUDE_MD)).unwrap();
        let report = m.apply(FLOCK, false);
        assert!(!report.ok());
        let linked: Vec<&Row> = report
            .rows
            .iter()
            .filter(|r| r.action == Action::Linked)
            .collect();
        assert_eq!(linked.len(), 2, "{report:?}");
        assert_eq!(linked[0].path, "~/.claude-main/skills");
        assert!(
            linked[0]
                .detail
                .as_deref()
                .unwrap()
                .contains("is a symlink to"),
            "{linked:?}"
        );
        assert_eq!(linked[1].path, format!("~/{CLAUDE_MD}"));
        assert_eq!(
            std::fs::read_dir(dotfiles.join("skills")).unwrap().count(),
            0
        );
        assert_eq!(
            std::fs::read_to_string(dotfiles.join("CLAUDE.md")).unwrap(),
            "dotfiles\n"
        );
        // The hook, in a dir that is no link, went in.
        assert!(m.at(".claude-main/hooks/notify.sh").exists());
    }

    /// A new version swaps the links and the block; a block the user
    /// edited stays; what the flock no longer names comes out, down to the
    /// user's own text.
    #[test]
    fn a_new_version_updates_and_a_dropped_bundle_comes_out() {
        let m = Machine::new();
        std::fs::create_dir_all(m.at(".claude-main")).unwrap();
        std::fs::write(m.at(CLAUDE_MD), USER_TEXT).unwrap();
        m.apply(FLOCK, false);
        m.new_version("Read the diff twice.\n");
        let updated = m.apply(FLOCK, false);
        let skill = updated
            .rows
            .iter()
            .find(|r| r.path.ends_with("reviewing-a-pr"))
            .unwrap();
        assert_eq!(skill.action, Action::Updated);
        assert_eq!(
            std::fs::read_to_string(m.at(".claude-main/skills/reviewing-a-pr/SKILL.md")).unwrap(),
            "Read the diff twice.\n"
        );

        let none = FLOCK.replace("bundles = [\"team-rules\"]\nbundle_vars = { ticket_url = \"https://tickets.example.com\" }\n", "");
        let removed = m.apply(&none, false);
        let actions = Machine::actions(&removed);
        assert_eq!(actions.len(), 4, "{actions:?}");
        assert!(
            actions.iter().all(|(_, a)| *a == Action::Removed),
            "{actions:?}"
        );
        assert!(!m.at(".claude-main/skills/reviewing-a-pr").exists());
        assert!(!m.at(".local/bin/team-tool").exists());
        assert_eq!(std::fs::read_to_string(m.at(CLAUDE_MD)).unwrap(), USER_TEXT);
        assert_eq!(
            serde_json::from_str::<Ledger>(&m.ledger().unwrap())
                .unwrap()
                .entries
                .len(),
            0
        );
    }

    #[test]
    fn a_block_edited_by_hand_is_left_alone() {
        let m = Machine::new();
        m.apply(FLOCK, false);
        let md = std::fs::read_to_string(m.at(CLAUDE_MD)).unwrap();
        let edited = md.replace("This one has the GPUs.", "This one has no GPUs left.");
        std::fs::write(m.at(CLAUDE_MD), &edited).unwrap();
        let src = m.tmp.path().join("src/team-rules/instructions/team.md");
        std::fs::write(&src, "New rules.\n").unwrap();
        m.new_version("v2\n");
        let report = m.apply(FLOCK, false);
        let row = report
            .rows
            .iter()
            .find(|r| r.path.ends_with("CLAUDE.md"))
            .unwrap();
        assert_eq!(row.action, Action::ChangedByHand, "{row:?}");
        assert_eq!(std::fs::read_to_string(m.at(CLAUDE_MD)).unwrap(), edited);
        // The rest of the new version went in.
        let skill = report
            .rows
            .iter()
            .find(|r| r.path.ends_with("reviewing-a-pr"))
            .unwrap();
        assert_eq!(skill.action, Action::Updated);
    }

    /// Something not pastor's where an item goes is a conflict, left as it
    /// is: a dir, or a link pastor did not make.
    #[test]
    fn something_else_in_an_items_place_is_a_conflict() {
        let m = Machine::new();
        std::fs::create_dir_all(m.at(".claude-main/skills/reviewing-a-pr")).unwrap();
        std::fs::create_dir_all(m.at(".claude-main/hooks")).unwrap();
        std::os::unix::fs::symlink("/elsewhere", m.at(".claude-main/hooks/notify.sh")).unwrap();
        let report = m.apply(FLOCK, false);
        let row = report
            .rows
            .iter()
            .find(|r| r.path.ends_with("reviewing-a-pr"))
            .unwrap();
        assert_eq!(row.action, Action::Conflict);
        assert!(
            row.detail.as_deref().unwrap().contains("not pastor's"),
            "{row:?}"
        );
        let hook = report
            .rows
            .iter()
            .find(|r| r.path.ends_with("notify.sh"))
            .unwrap();
        assert_eq!(hook.action, Action::Conflict);
        assert!(
            hook.detail
                .as_deref()
                .unwrap()
                .contains("a link to /elsewhere"),
            "{hook:?}"
        );
        assert_eq!(
            std::fs::read_link(m.at(".claude-main/hooks/notify.sh")).unwrap(),
            Path::new("/elsewhere")
        );
        assert!(!report.ok());
    }

    /// A block goes in after the user's text and comes out again leaving
    /// that text byte for byte, whatever it ends with.
    #[test]
    fn blocks_go_in_after_the_users_text_and_come_out_again() {
        let user = "# My rules\n\nBe brief.\n";
        let (one, sep) = put_block(user, "bundle:team-rules", "Use the ticket tracker.");
        assert_eq!(
            one,
            "# My rules\n\nBe brief.\n\n<!-- pastor:bundle:team-rules -->\nUse the ticket tracker.\n<!-- /pastor:bundle:team-rules -->\n"
        );
        let (two, base_sep) = put_block(&one, "bundle:base", "Run the tests.");
        let (again, none) = put_block(&two, "bundle:team-rules", "Use the new tracker.");
        assert_eq!(none, "");
        assert!(again.starts_with(user), "{again}");
        assert_eq!(
            find_block(&again, "bundle:team-rules").unwrap().1,
            "Use the new tracker."
        );
        assert_eq!(
            find_block(&again, "bundle:base").unwrap().1,
            "Run the tests."
        );
        assert_eq!(
            drop_block(
                &drop_block(&again, "bundle:base", &base_sep),
                "bundle:team-rules",
                &sep
            ),
            user
        );
        assert_eq!(
            put_block("", "bundle:b", "x"),
            (
                "<!-- pastor:bundle:b -->\nx\n<!-- /pastor:bundle:b -->\n".to_string(),
                String::new()
            )
        );
        for user in ["no newline", "two newlines\n\n", "\n", "  \n\t", ""] {
            let (put, sep) = put_block(user, "bundle:b", "x");
            assert!(put.starts_with(user), "{put:?}");
            assert_eq!(drop_block(&put, "bundle:b", &sep), user, "{put:?}");
        }
        // A marker inside a line is the user's text, not a block.
        assert!(find_block("see <!-- pastor:bundle:b --> here\n", "bundle:b").is_none());
    }

    /// flock.toml with no bundle named.
    fn no_bundles() -> String {
        FLOCK.replace(
            "bundles = [\"team-rules\"]\nbundle_vars = { ticket_url = \"https://tickets.example.com\" }\n",
            "",
        )
    }

    /// Two bundles' blocks share one instruction file, in flock order, with
    /// one backup of it for the run; dropping one leaves the other.
    #[test]
    fn two_bundles_each_get_a_block_in_one_file() {
        let m = Machine::new();
        add_base(&m.paths, m.tmp.path());
        std::fs::create_dir_all(m.at(".claude-main")).unwrap();
        std::fs::write(m.at(CLAUDE_MD), USER_TEXT).unwrap();
        let both = FLOCK.replace("[\"team-rules\"]", "[\"team-rules\", \"base\"]");
        let report = m.apply(&both, false);
        assert!(report.ok(), "{report:?}");
        let md = std::fs::read_to_string(m.at(CLAUDE_MD)).unwrap();
        let team = md.find("<!-- pastor:bundle:team-rules -->").unwrap();
        let base = md.find("<!-- pastor:bundle:base -->").unwrap();
        assert!(md.starts_with(USER_TEXT) && team < base, "{md}");
        assert_eq!(m.backups(), 1, "one backup a run, not one a block");

        let report = m.apply(&FLOCK.replace("[\"team-rules\"]", "[\"base\"]"), false);
        assert!(report.ok(), "{report:?}");
        let md = std::fs::read_to_string(m.at(CLAUDE_MD)).unwrap();
        assert!(!md.contains("pastor:bundle:team-rules"), "{md}");
        assert!(md.contains("Run the tests."), "{md}");
        let report = m.apply(&no_bundles(), false);
        assert!(report.ok(), "{report:?}");
        assert_eq!(std::fs::read_to_string(m.at(CLAUDE_MD)).unwrap(), USER_TEXT);
    }

    /// A run cut short after some writes leaves them in the ledger, so the
    /// next run takes them as pastor's: no conflict, no changed by hand.
    #[test]
    fn a_run_cut_short_leaves_its_writes_in_the_ledger() {
        let m = Machine::new();
        // The skill, the hook and the block go in; then the run dies.
        CRASH_AFTER.with(|c| c.set(Some(2)));
        let cut = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| m.apply(FLOCK, false)));
        assert!(cut.is_err());
        assert!(m.at(CLAUDE_MD).exists());
        assert!(!m.at(".local/bin/team-tool").exists());

        m.new_version("Read the diff twice.\n");
        m.new_instructions("New rules.\n");
        let report = m.apply(FLOCK, false);
        assert!(report.ok(), "{report:?}");
        assert_eq!(
            Machine::row(&report, "reviewing-a-pr").action,
            Action::Updated
        );
        assert_eq!(Machine::row(&report, "CLAUDE.md").action, Action::Updated);
        assert_eq!(Machine::row(&report, "team-tool").action, Action::Added);
        let ledger: Ledger = serde_json::from_str(&m.ledger().unwrap()).unwrap();
        assert_eq!(ledger.entries.len(), 4, "{ledger:?}");
        // Then the bundle leaves, and all of it comes out.
        let report = m.apply(&no_bundles(), false);
        assert!(report.ok(), "{report:?}");
        assert!(
            report.rows.iter().all(|r| r.action == Action::Removed),
            "{report:?}"
        );
        assert!(!m.at(CLAUDE_MD).exists());
    }

    /// A home under a symlinked dir is not written through either.
    #[test]
    fn a_home_under_a_symlinked_dir_is_not_written_through() {
        let m = Machine::new();
        let dotfiles = m.tmp.path().join("dotfiles");
        std::fs::create_dir_all(dotfiles.join("claude")).unwrap();
        std::os::unix::fs::symlink(&dotfiles, m.at("dot")).unwrap();
        let pastor_toml = PASTOR_TOML.replace("~/.claude-main", "~/dot/claude");
        let report = m.apply_with(&pastor_toml, FLOCK, false);
        let linked: Vec<&str> = report
            .rows
            .iter()
            .filter(|r| r.action == Action::Linked)
            .map(|r| r.path.as_str())
            .collect();
        assert_eq!(linked, ["~/dot", "~/dot", "~/dot"], "{report:?}");
        assert_eq!(
            std::fs::read_dir(dotfiles.join("claude")).unwrap().count(),
            0
        );
        // The bin file is not under it.
        assert_eq!(Machine::row(&report, "team-tool").action, Action::Added);
    }

    fn places(m: &Machine) -> Places {
        Places {
            home: m.home.clone(),
            store: m.paths.data_dir().join("bundles"),
            applied: m.paths.state_dir.join("applied"),
            target: Target::Home,
            scope: Scope::All,
            meta: Meta::default(),
            backed_up: Default::default(),
            secrets: Vec::new(),
            adopt: None,
            refused: false,
            whole: Vec::new(),
        }
    }

    /// A file saved between pastor's read and its write is read again and
    /// the edit made on what was saved; one that never stops changing is
    /// left to the harness, `busy`.
    #[test]
    fn a_file_saved_meanwhile_is_read_again_or_left_busy() {
        let m = Machine::new();
        let path = m.at("CLAUDE.md");
        std::fs::write(&path, "v0\n").unwrap();
        let places = places(&m);
        let mut saves = 0;
        let once = update_file(
            &places,
            &path,
            false,
            |content| {
                let content = content.unwrap().to_string();
                if saves == 0 {
                    saves += 1;
                    std::fs::write(&path, "v1\n").unwrap();
                }
                Ok(Change::Write(format!("{content}pastor\n")))
            },
            || Ok(()),
        );
        assert!(once.is_ok());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "v1\npastor\n");

        let mut n = 0;
        let always = update_file(
            &places,
            &path,
            false,
            |_| {
                n += 1;
                std::fs::write(&path, format!("harness {n}\n")).unwrap();
                Ok(Change::Write("pastor\n".into()))
            },
            || Ok(()),
        );
        assert!(matches!(always, Err(Failure::Busy)));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("harness {TRIES}\n")
        );
    }

    /// When its bundle leaves, a link the user pointed elsewhere and a
    /// block the user edited stay as the user left them.
    #[test]
    fn a_dropped_bundle_leaves_what_the_user_changed() {
        let m = Machine::new();
        m.apply(FLOCK, false);
        let skill = m.at(".claude-main/skills/reviewing-a-pr");
        std::fs::remove_file(&skill).unwrap();
        std::os::unix::fs::symlink("/mine", &skill).unwrap();
        let md = std::fs::read_to_string(m.at(CLAUDE_MD)).unwrap();
        let edited = md.replace("This one has the GPUs.", "Mine now.");
        std::fs::write(m.at(CLAUDE_MD), &edited).unwrap();

        let report = m.apply(&no_bundles(), false);
        assert_eq!(
            Machine::row(&report, "reviewing-a-pr").action,
            Action::ChangedByHand
        );
        assert_eq!(
            Machine::row(&report, "CLAUDE.md").action,
            Action::ChangedByHand
        );
        assert_eq!(std::fs::read_link(&skill).unwrap(), Path::new("/mine"));
        assert_eq!(std::fs::read_to_string(m.at(CLAUDE_MD)).unwrap(), edited);
        assert_eq!(Machine::row(&report, "team-tool").action, Action::Removed);
    }

    /// Taking a block out gives the user's file back as it was, down to
    /// its last byte, and keeps a file the user made even when empty.
    #[test]
    fn a_dropped_block_gives_the_users_file_back() {
        for user in ["user text", "user text\n\n", "", "  \n"] {
            let m = Machine::new();
            std::fs::create_dir_all(m.at(".claude-main")).unwrap();
            std::fs::write(m.at(CLAUDE_MD), user).unwrap();
            m.apply(FLOCK, false);
            assert!(
                std::fs::read_to_string(m.at(CLAUDE_MD))
                    .unwrap()
                    .starts_with(user)
            );
            let report = m.apply(&no_bundles(), false);
            assert!(report.ok(), "{report:?}");
            assert_eq!(
                std::fs::read_to_string(m.at(CLAUDE_MD)).unwrap(),
                user,
                "{user:?}"
            );
        }
    }

    /// The file as it was before pastor changed it stays among the
    /// backups however many runs come after.
    #[test]
    fn the_first_backup_is_kept_for_good() {
        let m = Machine::new();
        std::fs::create_dir_all(m.at(".claude-main")).unwrap();
        std::fs::write(m.at(CLAUDE_MD), USER_TEXT).unwrap();
        for n in 0..BACKUPS + 3 {
            m.new_instructions(&format!("Rules {n}.\n"));
            let report = m.apply(FLOCK, false);
            assert!(report.ok(), "{report:?}");
        }
        assert_eq!(m.backup("first").as_deref(), Some(USER_TEXT));
        assert_eq!(m.backups(), 1 + BACKUPS);
    }

    /// A changed file keeps its mode, whatever the umask.
    #[test]
    fn a_changed_file_keeps_its_mode() {
        let m = Machine::new();
        std::fs::create_dir_all(m.at(".claude-main")).unwrap();
        std::fs::write(m.at(CLAUDE_MD), USER_TEXT).unwrap();
        std::fs::set_permissions(m.at(CLAUDE_MD), std::fs::Permissions::from_mode(0o646)).unwrap();
        m.apply(FLOCK, false);
        let mode = std::fs::metadata(m.at(CLAUDE_MD))
            .unwrap()
            .permissions()
            .mode();
        // Other-write is masked by any usual umask, 002 or 022.
        assert_eq!(mode & 0o777, 0o646);
    }

    /// Whether this process has a descriptor open on `file`.
    fn holds_open(file: &Path) -> bool {
        use std::os::unix::fs::MetadataExt;
        let want = std::fs::metadata(file).unwrap();
        std::fs::read_dir("/dev/fd").unwrap().flatten().any(|fd| {
            std::fs::metadata(fd.path())
                .is_ok_and(|m| m.dev() == want.dev() && m.ino() == want.ino())
        })
    }

    /// Whether another apply could take the lock now.
    fn lock_is_free(file: &Path) -> bool {
        use std::os::unix::io::AsRawFd;
        let f = crate::edit::open_lock(file).unwrap();
        // SAFETY: flock on a descriptor this function owns.
        unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
    }

    /// One apply at a time: one that finds the lock taken waits, then gives
    /// up having written nothing; one that runs holds it to the end.
    #[test]
    fn one_apply_runs_at_a_time() {
        use std::os::unix::io::AsRawFd;
        let m = Machine::new();
        let file = m.paths.state_dir.join("applied/apply.lock");
        create_private_dir(file.parent().unwrap()).unwrap();
        let other = crate::edit::open_lock(&file).unwrap();
        // SAFETY: flock on a descriptor this test owns.
        assert_eq!(
            unsafe { libc::flock(other.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        let err = m.try_apply(PASTOR_TOML, FLOCK, false).unwrap_err();
        assert!(
            err.to_string().contains("another bundle apply held"),
            "{err}"
        );
        assert!(!m.at(".claude-main").exists());
        assert!(m.ledger().is_none());
        drop(other);

        let seen = std::rc::Rc::new(RefCell::new(Vec::new()));
        let record = seen.clone();
        let at = file.clone();
        AFTER_ITEM.with(|f| {
            *f.borrow_mut() = Some(Box::new(move || {
                record.borrow_mut().push(lock_is_free(&at))
            }))
        });
        let report = m.apply(FLOCK, false);
        AFTER_ITEM.with(|f| *f.borrow_mut() = None);
        assert!(report.ok(), "{report:?}");
        assert_eq!(*seen.borrow(), [false; 4], "held after every item");
        // The apply closed its descriptor. Not "the lock is free": a child
        // that another test forks holds a copy of every descriptor this
        // process has until it execs, and with it the lock, for a moment
        // after the apply let go. That the lock never outlives the exec is
        // `the_apply_lock_closes_on_exec`.
        assert!(!holds_open(&file), "let go after the run");
    }

    /// The lock's descriptor closes when a command the apply runs, or any
    /// other this process starts, execs, so none of them holds the lock
    /// past its start.
    #[test]
    fn the_apply_lock_closes_on_exec() {
        use std::os::unix::io::AsRawFd;
        let m = Machine::new();
        let f = lock(&places(&m)).unwrap();
        // SAFETY: fcntl on a descriptor this test owns.
        let flags = unsafe { libc::fcntl(f.as_raw_fd(), libc::F_GETFD) };
        assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0, "{flags}");
    }

    /// A tools bundle: Claude settings (two env keys, a hook group, a model)
    /// and agy settings, and two MCP servers, one needing env by name.
    fn add_tools(m: &Machine, model: &str) {
        let dir = m.tmp.path().join("src/tools");
        let put = |rel: &str, text: &str| {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        };
        put(
            "pastor-bundle.toml",
            "id = \"tools\"\nversion = \"1\"\n\
             [settings.claude]\nsrc = \"claude.json\"\n\
             [settings.agy]\nsrc = \"agy.json\"\n\
             [[mcp]]\nname = \"tickets\"\ncommand = \"tickets-mcp\"\nargs = [\"--home\", \"{home}\"]\nenv_from = [\"TICKETS_TOKEN\"]\n\
             [[mcp]]\nname = \"docs\"\nurl = \"https://docs.example.com/mcp\"\n",
        );
        put(
            "claude.json",
            &serde_json::json!({
                "env": {"TEAM": "ops", "TEAM_HOME": "{config_dir}"},
                "hooks": {"PreToolUse": [
                    {"matcher": "Bash", "hooks": [{"type": "command", "command": "{config_dir}/hooks/check.sh"}]}
                ]},
                "model": model,
            })
            .to_string(),
        );
        put("agy.json", "{\"theme\": \"dark\"}\n");
        crate::bundle::store::add(&m.paths, &crate::bundle::store::Source::Dir(dir)).unwrap();
    }

    const TOOLS: &str = "bundles = [\"team-rules\", \"tools\"]";

    fn with_tools() -> String {
        FLOCK.replace("bundles = [\"team-rules\"]", TOOLS)
    }

    fn json_at(m: &Machine, rel: &str) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(m.at(rel)).unwrap()).unwrap()
    }

    fn key_row<'a>(report: &'a Report, key: &str) -> &'a Row {
        report
            .rows
            .iter()
            .find(|r| r.key.as_deref() == Some(key))
            .unwrap_or_else(|| panic!("no row for {key}: {report:?}"))
    }

    const SETTINGS: &str = ".claude-main/settings.json";

    /// A bundle's settings go in key by key beside the user's; a key the
    /// user holds is a conflict and keeps the user's value; applying again
    /// changes nothing; taking the bundle out leaves the user's file as it
    /// was.
    #[test]
    fn settings_merge_beside_the_users_and_come_out_again() {
        let m = Machine::new();
        add_tools(&m, "opus");
        std::fs::create_dir_all(m.at(".claude-main")).unwrap();
        let user = serde_json::json!({
            "env": {"MINE": "1"},
            "model": "sonnet",
            "hooks": {"PreToolUse": [{"matcher": "Edit", "hooks": []}]},
        });
        std::fs::write(m.at(SETTINGS), format!("{user:#}\n")).unwrap();

        let report = m.apply(&with_tools(), false);
        assert_eq!(key_row(&report, "env.TEAM").action, Action::Added);
        let model = key_row(&report, "model");
        assert_eq!(model.action, Action::Conflict, "{model:?}");
        assert!(
            model.detail.as_deref().unwrap().contains("\"opus\"")
                && !model.detail.as_deref().unwrap().contains("sonnet"),
            "the bundle's value, never the user's: {model:?}"
        );
        let got = json_at(&m, SETTINGS);
        let home = m.at(".claude-main").display().to_string();
        assert_eq!(got["env"]["MINE"], "1");
        assert_eq!(got["env"]["TEAM"], "ops");
        assert_eq!(got["env"]["TEAM_HOME"], home);
        assert_eq!(got["model"], "sonnet");
        let groups = got["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0]["matcher"], "Edit", "the user's group first");
        assert_eq!(
            groups[1]["hooks"][0]["command"],
            format!("{home}/hooks/check.sh")
        );

        let again = m.apply(&with_tools(), false);
        assert!(!again.changes(), "{again:?}");
        assert_eq!(key_row(&again, "model").action, Action::Conflict);

        let report = m.apply(&FLOCK.replace(TOOLS, "bundles = [\"team-rules\"]"), false);
        assert!(report.ok(), "{report:?}");
        assert_eq!(json_at(&m, SETTINGS), user);
    }

    /// With no settings file and no `env` to start with, the bundle's keys
    /// make them, and taking the bundle out takes them away again, the
    /// file too; the plugin that carries Claude's MCP servers goes with it.
    #[test]
    fn what_pastor_made_for_settings_and_mcp_goes_when_the_bundle_does() {
        let m = Machine::new();
        add_tools(&m, "opus");
        let report = m.apply(&with_tools(), false);
        assert!(report.ok(), "{report:?}");
        let market = ".local/share/pastor/claude-marketplace/.claude-main";
        let mcp = json_at(&m, &format!("{market}/plugins/pastor-mcp/.mcp.json"));
        let home = m.home.display().to_string();
        assert_eq!(
            mcp["mcpServers"]["tickets"],
            serde_json::json!({
                "command": "tickets-mcp",
                "args": ["--home", home],
                "env": {"TICKETS_TOKEN": "${TICKETS_TOKEN}"},
            })
        );
        assert_eq!(
            mcp["mcpServers"]["docs"],
            serde_json::json!({"type": "http", "url": "https://docs.example.com/mcp"})
        );
        assert_eq!(
            json_at(&m, &format!("{market}/.claude-plugin/marketplace.json"))["plugins"][0]["name"],
            "pastor-mcp"
        );
        let settings = json_at(&m, SETTINGS);
        assert_eq!(
            settings["extraKnownMarketplaces"]["pastor"]["source"],
            serde_json::json!({"source": "directory", "path": m.at(market).display().to_string()})
        );
        assert_eq!(settings["enabledPlugins"]["pastor-mcp@pastor"], true);

        let again = m.apply(&with_tools(), false);
        assert!(!again.changes(), "{again:?}");

        let report = m.apply(&FLOCK.replace(TOOLS, "bundles = [\"team-rules\"]"), false);
        assert!(report.ok(), "{report:?}");
        assert!(!m.at(SETTINGS).exists(), "pastor made it, and it is empty");
        assert!(!m.at(".local/share/pastor/claude-marketplace").exists());
        assert!(m.at(".local/share/pastor").is_dir());
    }

    /// A key pastor set that the user changed is left as the user has it,
    /// both when a new version would change it and when the bundle leaves.
    #[test]
    fn a_settings_key_changed_by_hand_is_left_alone() {
        let m = Machine::new();
        add_tools(&m, "opus");
        m.apply(&with_tools(), false);
        let mut got = json_at(&m, SETTINGS);
        got["model"] = "haiku".into();
        std::fs::write(m.at(SETTINGS), format!("{got:#}\n")).unwrap();
        add_tools(&m, "opus-2");
        let report = m.apply(&with_tools(), false);
        assert_eq!(key_row(&report, "model").action, Action::ChangedByHand);
        assert_eq!(json_at(&m, SETTINGS)["model"], "haiku");

        let report = m.apply(&FLOCK.replace(TOOLS, "bundles = [\"team-rules\"]"), false);
        assert_eq!(key_row(&report, "model").action, Action::ChangedByHand);
        assert_eq!(json_at(&m, SETTINGS), serde_json::json!({"model": "haiku"}));
    }

    /// A new version's value replaces the old one pastor set.
    #[test]
    fn a_new_version_updates_a_settings_key() {
        let m = Machine::new();
        add_tools(&m, "opus");
        m.apply(&with_tools(), false);
        add_tools(&m, "opus-2");
        let report = m.apply(&with_tools(), false);
        assert_eq!(key_row(&report, "model").action, Action::Updated);
        assert_eq!(json_at(&m, SETTINGS)["model"], "opus-2");
    }

    /// A settings file that is not JSON is not touched: a `failed` row.
    #[test]
    fn a_settings_file_that_is_not_json_is_left_alone() {
        let m = Machine::new();
        add_tools(&m, "opus");
        std::fs::create_dir_all(m.at(".claude-main")).unwrap();
        std::fs::write(m.at(SETTINGS), "{ // a comment\n}\n").unwrap();
        let report = m.apply(&with_tools(), false);
        let row = key_row(&report, "env.TEAM");
        assert_eq!(row.action, Action::Failed);
        assert!(
            row.detail.as_deref().unwrap().contains("not JSON"),
            "{row:?}"
        );
        assert_eq!(
            std::fs::read_to_string(m.at(SETTINGS)).unwrap(),
            "{ // a comment\n}\n"
        );
    }

    /// agy gets its settings key and the url server in its own MCP file;
    /// a server that needs env by name is skipped there, since agy takes
    /// env values only.
    #[test]
    fn agy_gets_settings_and_mcp_servers_it_can_take() {
        let m = Machine::new();
        add_tools(&m, "opus");
        let pastor_toml = format!("{PASTOR_TOML}[agents.agy]\nkind = \"agy\"\n");
        let flock = with_tools().replace("agent = \"claude-main\"", "agent = \"agy\"");
        let report = m.apply_with(&pastor_toml, &flock, false);
        assert!(report.ok(), "{report:?}");
        assert_eq!(
            json_at(&m, ".gemini/antigravity-cli/settings.json"),
            serde_json::json!({"theme": "dark"})
        );
        assert_eq!(
            json_at(&m, ".gemini/config/mcp_config.json"),
            serde_json::json!({"mcpServers": {"docs": {"url": "https://docs.example.com/mcp"}}})
        );
        let skipped = report
            .rows
            .iter()
            .find(|r| r.action == Action::Skipped && r.path == "mcp server tickets")
            .unwrap_or_else(|| panic!("{report:?}"));
        assert!(
            skipped
                .detail
                .as_deref()
                .unwrap()
                .contains("env values, not names"),
            "{skipped:?}"
        );
    }

    /// A dry run reports the settings rows, the conflict too, and writes
    /// nothing.
    #[test]
    fn a_dry_run_reports_settings_and_writes_nothing() {
        let m = Machine::new();
        add_tools(&m, "opus");
        std::fs::create_dir_all(m.at(".claude-main")).unwrap();
        std::fs::write(m.at(SETTINGS), "{\"model\": \"sonnet\"}\n").unwrap();
        let report = m.apply(&with_tools(), true);
        assert_eq!(key_row(&report, "model").action, Action::Conflict);
        assert_eq!(key_row(&report, "env.TEAM").action, Action::Added);
        assert_eq!(
            std::fs::read_to_string(m.at(SETTINGS)).unwrap(),
            "{\"model\": \"sonnet\"}\n"
        );
        assert!(!m.at(".local/share/pastor/claude-marketplace").exists());
    }

    /// Two bundles may not set one settings key, nor name one MCP server,
    /// in a home: the plan says which and where.
    #[test]
    fn two_bundles_setting_one_key_are_refused() {
        let m = Machine::new();
        add_tools(&m, "opus");
        let dir = m.tmp.path().join("src/more");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("pastor-bundle.toml"),
            "id = \"more\"\nversion = \"1\"\n[settings.claude]\nsrc = \"c.json\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("c.json"), "{\"env\": {\"TEAM\": \"dev\"}}").unwrap();
        crate::bundle::store::add(&m.paths, &crate::bundle::store::Source::Dir(dir.clone()))
            .unwrap();
        let both = FLOCK.replace("[\"team-rules\"]", "[\"tools\", \"more\"]");
        assert_eq!(
            m.render(PASTOR_TOML, &both).unwrap_err(),
            "bundles tools and more both set env.TEAM in ~/.claude-main/settings.json"
        );
        std::fs::write(
            dir.join("pastor-bundle.toml"),
            "id = \"more\"\nversion = \"2\"\n[[mcp]]\nname = \"docs\"\nurl = \"https://elsewhere.example/mcp\"\n",
        )
        .unwrap();
        crate::bundle::store::add(&m.paths, &crate::bundle::store::Source::Dir(dir)).unwrap();
        let err = m.render(PASTOR_TOML, &both).unwrap_err();
        assert!(
            err.starts_with("bundles tools and more both set mcpServers.docs in ~/.local/share/pastor/claude-marketplace/"),
            "{err}"
        );
    }

    /// What the user had before the bundle (a key with the bundle's value,
    /// the bundle's hook group, an empty `env`, an empty file) is theirs:
    /// it stays when the bundle leaves.
    #[test]
    fn what_the_user_had_stays_when_the_bundle_leaves() {
        let group = serde_json::json!({"matcher": "Bash", "hooks": [{"type": "command", "command": "{config_dir}/hooks/check.sh"}]});
        let cases = [
            serde_json::json!({"env": {"TEAM": "ops"}, "hooks": {"PreToolUse": [group]}}),
            serde_json::json!({"env": {}}),
            serde_json::json!({}),
        ];
        for user in cases {
            let m = Machine::new();
            add_tools(&m, "opus");
            std::fs::create_dir_all(m.at(".claude-main")).unwrap();
            // The user's copy of the group has the home filled in, as pastor's does.
            let text = user
                .to_string()
                .replace("{config_dir}", &m.at(".claude-main").display().to_string());
            std::fs::write(m.at(SETTINGS), &text).unwrap();
            let before: serde_json::Value = serde_json::from_str(&text).unwrap();
            let report = m.apply(&with_tools(), false);
            assert!(report.ok(), "{report:?}");
            let report = m.apply(&FLOCK.replace(TOOLS, "bundles = [\"team-rules\"]"), false);
            assert!(report.ok(), "{report:?}");
            assert_eq!(json_at(&m, SETTINGS), before, "{user}");
        }
    }

    /// A bundle `id` with only a Claude settings fragment.
    fn add_settings(m: &Machine, id: &str, claude: serde_json::Value) {
        let dir = m.tmp.path().join("src").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("pastor-bundle.toml"),
            format!("id = \"{id}\"\nversion = \"1\"\n[settings.claude]\nsrc = \"c.json\"\n"),
        )
        .unwrap();
        std::fs::write(dir.join("c.json"), claude.to_string()).unwrap();
        crate::bundle::store::add(&m.paths, &crate::bundle::store::Source::Dir(dir)).unwrap();
    }

    fn with_bundles(ids: &str) -> String {
        FLOCK.replace("[\"team-rules\"]", &format!("[\"team-rules\", {ids}]"))
    }

    /// A hook group two bundles add is theirs together: it stays while
    /// either wants it, and goes, with the file pastor made for it, when
    /// the last one leaves.
    #[test]
    fn a_hook_group_two_bundles_add_stays_until_both_leave() {
        let m = Machine::new();
        let group = serde_json::json!({"hooks": [{"type": "command", "command": "guard"}]});
        let hooks = serde_json::json!({"hooks": {"Stop": [group]}});
        add_settings(&m, "p", hooks.clone());
        add_settings(&m, "q", hooks.clone());
        let report = m.apply(&with_bundles("\"p\", \"q\""), false);
        assert!(report.ok(), "{report:?}");
        assert_eq!(json_at(&m, SETTINGS), hooks);

        let report = m.apply(&with_bundles("\"q\""), false);
        assert!(report.ok(), "{report:?}");
        let stop: Vec<(&str, Action)> = report
            .rows
            .iter()
            .filter(|r| r.key.as_deref() == Some("hooks.Stop"))
            .map(|r| (r.bundle.as_str(), r.action))
            .collect();
        assert_eq!(stop, [("q", Action::Kept), ("p", Action::Removed)]);
        assert_eq!(json_at(&m, SETTINGS), hooks, "q still has it");

        let report = m.apply(FLOCK, false);
        assert!(report.ok(), "{report:?}");
        assert!(!m.at(SETTINGS).exists(), "pastor made it, and it is empty");
    }

    /// A new version that sets other keys and hook groups than the old one
    /// keeps knowing what pastor made for the old: when the bundle leaves,
    /// nothing is left of it.
    #[test]
    fn a_new_version_with_other_keys_leaves_nothing_behind() {
        let m = Machine::new();
        let group = |c: &str| serde_json::json!({"hooks": [{"type": "command", "command": c}]});
        add_settings(
            &m,
            "p",
            serde_json::json!({"env": {"A": "1"}, "hooks": {"Stop": [group("a")]}}),
        );
        let report = m.apply(&with_bundles("\"p\""), false);
        assert!(report.ok(), "{report:?}");
        let v2 = serde_json::json!({"env": {"B": "1"}, "hooks": {"Stop": [group("b")]}});
        add_settings(&m, "p", v2.clone());
        let report = m.apply(&with_bundles("\"p\""), false);
        assert!(report.ok(), "{report:?}");
        assert_eq!(json_at(&m, SETTINGS), v2);

        let report = m.apply(FLOCK, false);
        assert!(report.ok(), "{report:?}");
        assert!(!m.at(SETTINGS).exists(), "pastor made it, and it is empty");
    }

    /// An MCP server for every kind gets a `skipped` row on a home whose
    /// kind has no place for MCP servers.
    #[test]
    fn mcp_servers_for_every_kind_are_skipped_where_there_is_no_place() {
        let m = Machine::new();
        add_tools(&m, "opus");
        let pastor_toml = format!("{PASTOR_TOML}[agents.cx]\nkind = \"codex\"\n");
        let flock = with_tools().replace("agent = \"claude-main\"", "agent = \"cx\"");
        let report = m.apply_with(&pastor_toml, &flock, false);
        assert!(report.ok(), "{report:?}");
        for server in ["mcp server tickets", "mcp server docs"] {
            let row = report
                .rows
                .iter()
                .find(|r| r.bundle == "tools" && r.path == server)
                .unwrap_or_else(|| panic!("no row for {server}: {report:?}"));
            assert_eq!(row.action, Action::Skipped);
            assert_eq!(
                row.detail.as_deref(),
                Some("pastor does not place codex MCP servers yet")
            );
        }
    }

    /// A value the user changed by hand stays theirs when another bundle
    /// that wants that value comes in as the one that set it leaves: the
    /// new bundle does not take it over, so it never takes it out.
    #[test]
    fn a_value_changed_by_hand_is_not_taken_over_by_another_bundle() {
        let m = Machine::new();
        add_settings(&m, "p", serde_json::json!({"env": {"A": "1"}}));
        add_settings(&m, "q", serde_json::json!({"env": {"A": "2"}}));
        std::fs::create_dir_all(m.at(".claude-main")).unwrap();
        std::fs::write(m.at(SETTINGS), "{\"theme\": \"dark\"}\n").unwrap();
        let report = m.apply(&with_bundles("\"p\""), false);
        assert!(report.ok(), "{report:?}");
        let mut got = json_at(&m, SETTINGS);
        got["env"]["A"] = "2".into();
        std::fs::write(m.at(SETTINGS), format!("{got:#}\n")).unwrap();

        let report = m.apply(&with_bundles("\"q\""), false);
        let rows: Vec<(&str, Action)> = report
            .rows
            .iter()
            .filter(|r| r.key.as_deref() == Some("env.A"))
            .map(|r| (r.bundle.as_str(), r.action))
            .collect();
        assert_eq!(rows, [("q", Action::Kept), ("p", Action::ChangedByHand)]);

        m.apply(FLOCK, false);
        assert_eq!(
            json_at(&m, SETTINGS),
            serde_json::json!({"env": {"A": "2"}, "theme": "dark"})
        );
    }

    impl Machine {
        /// `bundle adopt` of the homes in `homes` (none: every home).
        fn adopt(&self, flock: &str, dry_run: bool, homes: &[&str]) -> Report {
            self.try_adopt(PASTOR_TOML, flock, dry_run, homes).unwrap()
        }

        fn try_adopt(
            &self,
            pastor_toml: &str,
            flock: &str,
            dry_run: bool,
            homes: &[&str],
        ) -> anyhow::Result<Report> {
            let adopt = Adopt {
                homes: homes.iter().map(|h| h.to_string()).collect(),
            };
            self.try_apply_as(pastor_toml, flock, dry_run, Some(&adopt))
        }

        /// What the adopt in `report` backed up of `rel`, below the home.
        fn backed_up(&self, report: &Report, rel: &str) -> PathBuf {
            let dir = report.backup.as_deref().expect("a backup dir");
            match dir.strip_prefix("~/") {
                Some(below) => self.at(below).join(rel),
                None => Path::new(dir).join(rel),
            }
        }
    }

    /// The user's own home before an adopt: settings with a model the
    /// bundle sets otherwise, a key of their own, a hook group of their own
    /// and a hook event that is no list; `CLAUDE.md` a symlink into a
    /// dotfiles dir; and a skill of their own where the bundle's goes.
    fn users_home(m: &Machine) -> (serde_json::Value, PathBuf) {
        add_tools(m, "opus");
        std::fs::create_dir_all(m.at(".claude-main/skills/reviewing-a-pr")).unwrap();
        std::fs::write(
            m.at(".claude-main/skills/reviewing-a-pr/SKILL.md"),
            "mine\n",
        )
        .unwrap();
        let user = serde_json::json!({
            "env": {"MINE": "1"},
            "model": "sonnet",
            "hooks": {
                "Stop": [{"hooks": [{"type": "command", "command": "mine"}]}],
                "PreToolUse": {"matcher": "not a list"},
            },
        });
        std::fs::write(m.at(SETTINGS), format!("{user:#}\n")).unwrap();
        let dotfiles = m.tmp.path().join("dotfiles");
        std::fs::create_dir_all(&dotfiles).unwrap();
        std::fs::write(dotfiles.join("CLAUDE.md"), USER_TEXT).unwrap();
        std::os::unix::fs::symlink(dotfiles.join("CLAUDE.md"), m.at(CLAUDE_MD)).unwrap();
        (user, dotfiles)
    }

    /// A dry run lists every replacement, old value and all, and changes
    /// nothing: no file, link, backup or ledger.
    #[test]
    fn an_adopt_dry_run_lists_what_it_replaces_and_changes_nothing() {
        let m = Machine::new();
        let (user, _) = users_home(&m);
        let report = m.adopt(&with_tools(), true, &[]);
        let model = key_row(&report, "model");
        assert_eq!(
            (model.action, model.old.as_deref()),
            (Action::Replaced, Some("\"sonnet\"")),
            "{model:?}"
        );
        assert_eq!(model.backup, None, "a dry run backs nothing up");
        let hooks = key_row(&report, "hooks.PreToolUse");
        assert_eq!(hooks.action, Action::Replaced, "{hooks:?}");
        let md = report
            .rows
            .iter()
            .find(|r| r.path == format!("~/{CLAUDE_MD}") && r.old.is_some())
            .unwrap_or_else(|| panic!("no replaced link: {report:?}"));
        assert_eq!(md.action, Action::Replaced);
        assert!(
            md.old.as_deref().unwrap().starts_with("a symlink to"),
            "{md:?}"
        );
        let skill = Machine::row(&report, "skills/reviewing-a-pr");
        assert_eq!(
            (skill.action, skill.old.as_deref()),
            (Action::Replaced, Some("a directory"))
        );
        assert_eq!(report.backup, None);
        assert_eq!(json_at(&m, SETTINGS), user);
        assert!(
            m.at(CLAUDE_MD)
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            m.at(".claude-main/skills/reviewing-a-pr/SKILL.md")
                .is_file()
        );
        assert_eq!(m.ledger(), None);
        assert_eq!(m.backups(), 0);
    }

    /// An adopt backs up what it touches, makes the linked instruction file
    /// a plain copy (its target unchanged), puts the bundle's value in
    /// place of the user's and its skill in place of theirs, and keeps the
    /// user's other keys, hook groups and text; the ledger has all of it,
    /// so a plain apply after finds nothing to do, and a later hand edit is
    /// the user's again.
    #[test]
    fn an_adopt_backs_up_then_the_bundle_wins() {
        let m = Machine::new();
        let (user, dotfiles) = users_home(&m);
        let report = m.adopt(&with_tools(), false, &[]);
        assert!(report.ok(), "{report:?}");

        // The backup holds each touched file as it was, the link's content.
        assert_eq!(
            std::fs::read_to_string(m.backed_up(&report, SETTINGS)).unwrap(),
            format!("{user:#}\n")
        );
        assert_eq!(
            std::fs::read_to_string(m.backed_up(&report, CLAUDE_MD)).unwrap(),
            USER_TEXT
        );
        assert_eq!(
            std::fs::read_to_string(
                m.backed_up(&report, ".claude-main/skills/reviewing-a-pr/SKILL.md")
            )
            .unwrap(),
            "mine\n"
        );
        let model = key_row(&report, "model");
        assert_eq!(
            model.backup.as_deref(),
            Some(format!("{}/{SETTINGS}", report.backup.as_deref().unwrap()).as_str())
        );

        let got = json_at(&m, SETTINGS);
        assert_eq!(got["model"], "opus");
        assert_eq!(got["env"]["MINE"], "1");
        assert_eq!(got["env"]["TEAM"], "ops");
        assert_eq!(got["hooks"]["Stop"], user["hooks"]["Stop"]);
        assert_eq!(got["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
        assert!(
            !m.at(CLAUDE_MD)
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let md = std::fs::read_to_string(m.at(CLAUDE_MD)).unwrap();
        assert!(
            md.starts_with(USER_TEXT) && md.contains("pastor:bundle:team-rules"),
            "{md}"
        );
        assert_eq!(
            std::fs::read_to_string(dotfiles.join("CLAUDE.md")).unwrap(),
            USER_TEXT,
            "the link's target is never written"
        );
        assert!(
            std::fs::read_link(m.at(".claude-main/skills/reviewing-a-pr"))
                .unwrap()
                .starts_with(m.paths.data_dir())
        );

        let again = m.apply(&with_tools(), false);
        assert!(again.ok() && !again.changes(), "{again:?}");

        let mut edited = json_at(&m, SETTINGS);
        edited["model"] = "haiku".into();
        std::fs::write(m.at(SETTINGS), format!("{edited:#}\n")).unwrap();
        let after = m.apply(&with_tools(), false);
        assert_eq!(key_row(&after, "model").action, Action::ChangedByHand);
        assert_eq!(json_at(&m, SETTINGS)["model"], "haiku");
    }

    /// A value the user had that equals the bundle's is pastor's after an
    /// adopt: when the bundle changes it, the change goes in instead of a
    /// conflict, and when the bundle leaves, it goes.
    #[test]
    fn an_adopt_owns_what_the_user_had_as_the_bundle_has_it() {
        let m = Machine::new();
        add_settings(&m, "p", serde_json::json!({"env": {"A": "1"}}));
        std::fs::create_dir_all(m.at(".claude-main")).unwrap();
        std::fs::write(m.at(SETTINGS), "{\"env\": {\"A\": \"1\", \"B\": \"2\"}}\n").unwrap();
        let report = m.adopt(&with_bundles("\"p\""), false, &[]);
        assert_eq!(key_row(&report, "env.A").action, Action::Kept);
        add_settings(&m, "p", serde_json::json!({"env": {"A": "3"}}));
        let report = m.apply(&with_bundles("\"p\""), false);
        assert_eq!(
            key_row(&report, "env.A").action,
            Action::Updated,
            "{report:?}"
        );
        m.apply(FLOCK, false);
        assert_eq!(
            json_at(&m, SETTINGS),
            serde_json::json!({"env": {"B": "2"}})
        );
    }

    /// A block the user edited is the bundle's again after an adopt, with
    /// the start of what the user had written in the row and all of it in
    /// the backup.
    #[test]
    fn an_adopt_puts_back_a_block_edited_by_hand() {
        let m = Machine::new();
        m.apply(FLOCK, false);
        let md = std::fs::read_to_string(m.at(CLAUDE_MD)).unwrap();
        std::fs::write(m.at(CLAUDE_MD), md.replace("GPUs", "TPUs")).unwrap();
        let report = m.adopt(FLOCK, false, &[]);
        let row = Machine::row(&report, "CLAUDE.md");
        assert_eq!(row.action, Action::Replaced, "{row:?}");
        assert!(
            row.old.as_deref().unwrap().starts_with("Tickets:"),
            "{row:?}"
        );
        assert!(
            std::fs::read_to_string(m.backed_up(&report, CLAUDE_MD))
                .unwrap()
                .contains("TPUs")
        );
        assert_eq!(std::fs::read_to_string(m.at(CLAUDE_MD)).unwrap(), md);
    }

    /// A linked dir on the way becomes a plain dir holding a copy of what
    /// it pointed to, the user's other skills in it; the target is never
    /// written.
    #[test]
    fn an_adopt_makes_a_linked_dir_a_plain_copy() {
        let m = Machine::new();
        let dotfiles = m.tmp.path().join("dotfiles");
        std::fs::create_dir_all(dotfiles.join("skills/mine")).unwrap();
        std::fs::write(dotfiles.join("skills/mine/SKILL.md"), "mine\n").unwrap();
        std::os::unix::fs::symlink("/elsewhere", dotfiles.join("skills/linked")).unwrap();
        std::fs::create_dir_all(m.at(".claude-main")).unwrap();
        std::os::unix::fs::symlink(dotfiles.join("skills"), m.at(".claude-main/skills")).unwrap();
        let report = m.adopt(FLOCK, false, &[]);
        assert!(report.ok(), "{report:?}");
        let skills = m.at(".claude-main/skills");
        assert!(skills.symlink_metadata().unwrap().is_dir());
        assert_eq!(
            std::fs::read_to_string(skills.join("mine/SKILL.md")).unwrap(),
            "mine\n"
        );
        assert_eq!(
            std::fs::read_link(skills.join("linked")).unwrap(),
            Path::new("/elsewhere")
        );
        assert!(
            skills
                .join("reviewing-a-pr")
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let names: Vec<_> = std::fs::read_dir(dotfiles.join("skills"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 2, "the target is left as it was: {names:?}");
        assert_eq!(
            std::fs::read_to_string(m.backed_up(&report, ".claude-main/skills/mine/SKILL.md"))
                .unwrap(),
            "mine\n"
        );
    }

    /// A backup that cannot be made fails the adopt before anything is
    /// written.
    #[test]
    fn an_adopt_whose_backup_fails_writes_nothing() {
        let m = Machine::new();
        let (user, _) = users_home(&m);
        std::fs::create_dir_all(m.paths.state_dir.join("applied")).unwrap();
        std::fs::write(m.paths.state_dir.join("applied/backup"), "no dir\n").unwrap();
        let err = m
            .try_adopt(PASTOR_TOML, &with_tools(), false, &[])
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("nothing was written"),
            "{err:#}"
        );
        assert_eq!(json_at(&m, SETTINGS), user);
        assert!(
            m.at(CLAUDE_MD)
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            m.at(".claude-main/skills/reviewing-a-pr/SKILL.md")
                .is_file()
        );
        assert_eq!(m.ledger(), None);
        assert!(
            !m.paths.data_dir().join("bundles").exists(),
            "not even the store"
        );
    }

    /// The harness manifest's secret files are refused, by apply and adopt
    /// alike: never read, copied or written, and a linked dir holding one
    /// is not copied.
    #[test]
    fn secret_files_are_refused_and_never_copied() {
        let m = Machine::new();
        add_tools(&m, "opus");
        // A manifest whose secret is the settings file itself.
        let dir = m.paths.harnesses_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let claude = crate::harness_manifest::BUILTIN
            .iter()
            .find(|(id, _)| *id == "claude")
            .unwrap()
            .1
            .replace(
                "secrets = [\".credentials.json\"]",
                "secrets = [\"settings.*\"]",
            );
        std::fs::write(dir.join("claude.toml"), claude).unwrap();
        let harnesses = crate::harness_manifest::Harnesses::load(&m.paths).unwrap();
        let config = PastorConfig::parse(Path::new("pastor.toml"), PASTOR_TOML).unwrap();
        let flock = Flock::parse(Path::new("flock.toml"), &with_tools()).unwrap();
        let parts = crate::machine_check::Config {
            defaults: &config.defaults,
            agents: &config.agents,
            models: &config.models,
            profiles: &config.profiles,
            harnesses: &harnesses,
            herdr_pin: None,
            head_address: None,
        };
        let plan =
            crate::bundle::plan::render(&m.paths, &flock, &parts, flock.get("build-01").unwrap())
                .unwrap();
        assert!(
            plan.secrets
                .contains(&"~/.claude-main/settings.*".to_string()),
            "{plan:?}"
        );
        std::fs::create_dir_all(m.at(".claude-main")).unwrap();
        std::fs::write(m.at(SETTINGS), "{\"model\": \"sonnet\"}\n").unwrap();
        for adopt in [None, Some(Adopt::default())] {
            let set = tempfile::tempdir_in(m.tmp.path()).unwrap();
            crate::bundle::push::write_set(&m.paths, set.path(), &plan).unwrap();
            let report = apply_local(set.path(), &m.paths, &m.home, false, adopt.as_ref()).unwrap();
            let model = key_row(&report, "model");
            assert_eq!(model.action, Action::Failed, "{model:?}");
            assert!(
                model.detail.as_deref().unwrap().contains("secret"),
                "{model:?}"
            );
            assert_eq!(
                std::fs::read_to_string(m.at(SETTINGS)).unwrap(),
                "{\"model\": \"sonnet\"}\n"
            );
        }
        assert!(
            walk(&m.paths.state_dir.join("applied/backup"))
                .iter()
                .all(|p| !p.to_string_lossy().contains("settings.json")),
            "a secret is never backed up"
        );

        // With the built-in manifest: a linked home dir that holds the
        // login is not copied, and nothing behind it is written.
        let m = Machine::new();
        let dotfiles = m.tmp.path().join("dotfiles/claude-main");
        std::fs::create_dir_all(&dotfiles).unwrap();
        std::fs::write(dotfiles.join(".credentials.json"), "{}\n").unwrap();
        std::os::unix::fs::symlink(&dotfiles, m.at(".claude-main")).unwrap();
        let report = m.adopt(FLOCK, false, &[]);
        let md = Machine::row(&report, "CLAUDE.md");
        assert_eq!(md.action, Action::Linked, "{md:?}");
        assert!(
            md.detail.as_deref().unwrap().contains(".credentials.json"),
            "{md:?}"
        );
        assert!(
            m.at(".claude-main")
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_dir(&dotfiles).unwrap().count(), 1);
        assert!(
            walk(&m.paths.state_dir.join("applied/backup"))
                .iter()
                .all(|p| !p.to_string_lossy().contains("credentials")),
            "a secret is never backed up"
        );
    }

    /// Every file under `dir`, none when it is not there.
    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&d) else {
                continue;
            };
            for e in entries.flatten() {
                match e.file_type().unwrap().is_dir() {
                    true => stack.push(e.path()),
                    false => out.push(e.path()),
                }
            }
        }
        out
    }

    /// An adopt of one agent's home takes over that home only: the other
    /// home's conflict stays, nothing there is written or left out of the
    /// ledger, and shared files are left alone.
    #[test]
    fn an_adopt_of_one_home_leaves_the_others() {
        let m = Machine::new();
        let pastor_toml = format!(
            "{PASTOR_TOML}[agents.codex-1]\nkind = \"codex\"\n[models.c1]\nkind = \"codex\"\nargs = []\n"
        );
        let flock = FLOCK.replace(
            "agent = \"claude-main\"",
            "agent = \"claude-main\"\nagents = { codex = \"codex-1\" }",
        );
        for home in [".claude-main", ".codex"] {
            std::fs::create_dir_all(m.at(&format!("{home}/skills/reviewing-a-pr"))).unwrap();
        }
        let first = m.apply_with(&pastor_toml, &flock, false);
        assert_eq!(
            Machine::row(&first, ".codex/AGENTS.md").action,
            Action::Added
        );
        let ledger = m.ledger().unwrap();

        let report = m
            .try_adopt(&pastor_toml, &flock, false, &["~/.claude-main"])
            .unwrap();
        assert!(report.ok(), "{report:?}");
        assert!(
            report
                .rows
                .iter()
                .all(|r| !r.path.contains(".codex") && !r.path.contains(".local/bin")),
            "{report:?}"
        );
        assert!(
            m.at(".codex/skills/reviewing-a-pr")
                .symlink_metadata()
                .unwrap()
                .is_dir()
        );
        assert!(
            m.at(".claude-main/skills/reviewing-a-pr")
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let after = m.ledger().unwrap();
        for line in ledger
            .lines()
            .filter(|l| l.contains(".codex") || l.contains("team-tool"))
        {
            assert!(after.contains(line), "{line} left the ledger");
        }
    }
}
