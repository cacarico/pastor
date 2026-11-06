//! Keeping bundles: what the head does after `bundle apply` exists.
//!
//! - A `bundles` row for each harness home in `machine check`: the home's
//!   items read against the plan (`apply --dry-run` on the machine), folded
//!   into `ok`, `stale`, `drift`, `not adopted` or `conflict`.
//! - The `config` section of `machine describe`.
//! - The keeper, a loop in the head. In `auto` mode it applies a machine's
//!   bundles when the plan changed since the last apply (a new bundle
//!   version, a `bundles`, `bundle_vars` or `bundle_apply` edit, an agent's
//!   home moving); in `manual` it holds, and the row says `stale`. In both it
//!   looks at every machine now and then and says when a home drifts or
//!   conflicts, and it reports every apply, whoever ran it, as a
//!   `bundle.applied` or `bundle.failed` event.
//! - The launch gate (`Gate`): before a task starts on a machine, `auto`
//!   applies a stale machine first and `manual` launches with what is there;
//!   a failed apply blocks the task in both.
//!
//! A person's `pastor bundle apply` and the keeper's both go through
//! `apply_recorded`, which writes `status`, so they agree on what was applied.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::apply::{Action, Report};
use super::plan::{self, Home, Plan};
use super::status::{self, Applied, Status};
use super::{Apply, Manifest, push, store};
use crate::config::Paths;
use crate::config::flock::{Flock, MachineConfig};
use crate::daemon::Fleet;
use crate::machine_check::Config;
use crate::provision::run::{Row, Shell, Status as RowStatus};

/// The error code of a task that could not start because its machine's
/// bundles are not in a state to start it.
pub const BUNDLE_STALE: &str = "bundle_stale";

/// The id of the `machine check` row.
pub const ROW: &str = "bundles";

/// How often the keeper wakes.
const TICK: Duration = Duration::from_secs(15);

/// How often it reads a machine's homes against the plan.
const CHECK_EVERY: Duration = Duration::from_secs(5 * 60);

/// How long after a failed apply of a plan it does not try that plan again
/// on its own. A new plan, or a person, tries at once.
const RETRY_AFTER: Duration = Duration::from_secs(10 * 60);

/// How long after a failed apply a task launch takes that failure as the
/// answer instead of applying again.
const FRESH_FAILURE: Duration = Duration::from_secs(60);

/// What a launch waits for an apply, at most.
const GATE_TIMEOUT: Duration = Duration::from_secs(180);

/// How a home stands against the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HomeState {
    /// Everything pastor wrote is there as it wrote it.
    Ok,
    /// The plan is ahead of the home: a newer version, changed vars, or an
    /// item to add or take away.
    Stale,
    /// Something pastor wrote was changed by hand.
    Drift,
    /// A file or directory is a symlink pastor does not write through.
    NotAdopted,
    /// Something that is not pastor's is where an item goes.
    Conflict,
}

impl HomeState {
    pub fn word(self) -> &'static str {
        match self {
            HomeState::Ok => "ok",
            HomeState::Stale => "stale",
            HomeState::Drift => "drift",
            HomeState::NotAdopted => "not adopted",
            HomeState::Conflict => "conflict",
        }
    }

    /// Whether pastor's own `bundle apply` could never settle it: the
    /// home is still not one pastor manages whole.
    pub fn adopted(self) -> bool {
        !matches!(self, HomeState::NotAdopted | HomeState::Conflict)
    }
}

/// One harness home against the plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HomeCheck {
    pub path: String,
    pub kind: String,
    pub agents: Vec<String>,
    pub state: HomeState,
    /// The items that are not ok, as `<what> <path>`, a few.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}

/// The most items a home names.
const ITEMS: usize = 5;

/// Fold a dry-run `report` into one state per home. A row with no home (a
/// version to install, a file every home shares) counts for every home.
pub fn fold(homes: &[Home], report: &Report) -> Vec<HomeCheck> {
    let mut paths: Vec<&Home> = homes.iter().collect();
    let extra: Vec<Home> = report
        .rows
        .iter()
        .filter_map(|r| r.home.as_deref())
        .filter(|h| !homes.iter().any(|x| x.path == *h))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|path| Home {
            kind: String::new(),
            path: path.to_string(),
            agents: vec![],
        })
        .collect();
    paths.extend(extra.iter());
    paths
        .into_iter()
        .map(|home| {
            let mut state = HomeState::Ok;
            let mut items = Vec::new();
            for r in report.rows.iter().filter(|r| {
                r.home.as_deref().is_none_or(|h| h == home.path) && r.action != Action::Skipped
            }) {
                let here = state_of(r.action);
                if here == HomeState::Ok {
                    continue;
                }
                state = state.max(here);
                if items.len() < ITEMS {
                    items.push(format!("{} {}", action_word(r.action), r.path));
                }
            }
            HomeCheck {
                path: home.path.clone(),
                kind: home.kind.clone(),
                agents: home.agents.clone(),
                state,
                items,
            }
        })
        .collect()
}

/// How one row of a dry run stands against what pastor wrote.
pub fn state_of(a: Action) -> HomeState {
    match a {
        Action::Conflict => HomeState::Conflict,
        Action::Linked => HomeState::NotAdopted,
        Action::ChangedByHand => HomeState::Drift,
        Action::Added | Action::Updated | Action::Removed | Action::Replaced | Action::Busy => {
            HomeState::Stale
        }
        Action::Kept | Action::Failed | Action::Skipped => HomeState::Ok,
    }
}

/// An action as a row's text says it: `changed by hand`.
pub fn action_word(a: Action) -> String {
    serde_json::to_value(a)
        .ok()
        .and_then(|v| v.as_str().map(|s| s.replace('_', " ")))
        .unwrap_or_default()
}

/// `machine check` rows for the homes in `checks`: `pass` for an `ok` home,
/// `warn` for any other, which does not fail the check.
pub fn rows(machine: &str, checks: &[HomeCheck]) -> Vec<Row> {
    checks
        .iter()
        .map(|c| {
            let who = if c.agents.is_empty() {
                c.path.clone()
            } else {
                format!("{} ({})", c.path, c.agents.join(", "))
            };
            let (status, reason) = match c.state {
                HomeState::Ok => (RowStatus::Pass, None),
                state => {
                    let fix = match state {
                        HomeState::Stale => format!("pastor bundle apply {machine}"),
                        HomeState::Drift => format!(
                            "what pastor wrote was changed; `pastor bundle apply {machine}` leaves it, so put it back or remove it by hand"
                        ),
                        _ => format!(
                            "pastor does not write through a link or over what is not its own; `pastor bundle adopt {machine}` takes the home over (after a backup), or move it aside by hand"
                        ),
                    };
                    let items = c.items.join("; ");
                    (RowStatus::Warn, Some(format!("{items}. Fix: {fix}")))
                }
            };
            Row {
                id: ROW.into(),
                agent: c.agents.first().cloned(),
                status,
                first_line: format!("{who}: {}", c.state.word()),
                reason,
            }
        })
        .collect()
}

/// How to reach a machine to apply bundles on it: a shell, and the program
/// that runs there. `None` for a machine bundles do not reach (a pull
/// machine, one behind a command bridge).
pub type Reach = Arc<dyn Fn(&MachineConfig) -> Option<(Arc<dyn Shell>, String)> + Send + Sync>;

/// The machine's real shell. A local machine runs this very binary; another,
/// the pastor on its PATH.
pub fn reach(paths: Paths) -> Reach {
    Arc::new(move |m| {
        if m.pull {
            return None;
        }
        let shell = crate::provision::run::MachineShell::new(m, &paths).ok()?;
        let pastor = if m.local {
            std::env::current_exe().ok()?.to_string_lossy().into_owned()
        } else {
            "pastor".to_string()
        };
        Some((Arc::new(shell) as Arc<dyn Shell>, pastor))
    })
}

/// The result of an apply that was recorded.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub applied: Applied,
    pub report: Option<Report>,
    /// The result reached the status file. When it did not, the next look
    /// finds the machine as stale as before.
    pub recorded: bool,
}

/// The rows of a report that left the machine short for want of a write.
fn failures(report: &Report) -> Vec<String> {
    report
        .rows
        .iter()
        .filter(|r| matches!(r.action, Action::Failed | Action::Busy))
        .map(|r| {
            format!(
                "{} {}{}",
                action_word(r.action),
                r.path,
                r.detail
                    .as_deref()
                    .map(|d| format!(": {d}"))
                    .unwrap_or_default()
            )
        })
        .collect()
}

/// Apply `plan` on the machine behind `shell` and record the result in
/// `status`. It went through when the machine answered and no item failed;
/// conflicts and links are rows the plan could not place, not a failed apply.
/// One that never reached pastor on the machine placed nothing; one cut
/// short after it did (a timeout, an answer that is no report) may have, and
/// is recorded as an answered failure, so what it may have put is still
/// taken off when its bundles leave.
pub async fn apply_recorded(
    paths: &Paths,
    shell: Arc<dyn Shell>,
    plan: &Plan,
    pastor: &str,
) -> Outcome {
    run_recorded(paths, shell, plan, pastor, None).await
}

/// As `apply_recorded`, for an apply that adopts the homes `adopt` names:
/// it applies the whole plan, so it is recorded the same way.
pub async fn run_recorded(
    paths: &Paths,
    shell: Arc<dyn Shell>,
    plan: &Plan,
    pastor: &str,
    adopt: Option<&super::apply::Adopt>,
) -> Outcome {
    let (report, ok, error, answered) =
        match push::apply(paths, shell, plan, pastor, false, adopt).await {
            Ok(report) => {
                let failed = failures(&report);
                if failed.is_empty() {
                    (Some(report), true, None, true)
                } else {
                    (Some(report), false, Some(failed.join("; ")), true)
                }
            }
            Err(f) => (None, false, Some(f.why), f.started),
        };
    let (applied, recorded) = match status::record(
        paths,
        &plan.machine,
        plan,
        ok,
        answered,
        error.clone(),
    ) {
        Ok(a) => (a, true),
        Err(e) => {
            tracing::warn!(machine = %plan.machine, err = %format!("{e:#}"), "record a bundle apply");
            let a = Applied {
                at: Utc::now(),
                fingerprint: status::fingerprint(plan),
                bundles: BTreeMap::new(),
                ok,
                error,
            };
            (a, false)
        }
    };
    Outcome {
        applied,
        report,
        recorded,
    }
}

/// Read the homes of `plan`'s machine against it.
pub async fn check(
    paths: &Paths,
    shell: Arc<dyn Shell>,
    plan: &Plan,
    pastor: &str,
    homes: &[Home],
) -> Result<Vec<HomeCheck>, String> {
    let report = push::apply(paths, shell, plan, pastor, true, None)
        .await
        .map_err(|f| f.why)?;
    Ok(fold(homes, &report))
}

/// The `bundles` rows `machine check` adds for `m`: none when no bundle is
/// meant for it and none is left on it; a `fail` row when the last apply did
/// not go through; a `warn` row when the homes cannot be read.
pub async fn check_rows(
    paths: &Paths,
    flock: &Flock,
    config: &Config<'_>,
    m: &MachineConfig,
    shell: Arc<dyn Shell>,
) -> Vec<Row> {
    let plan = match plan::render(paths, flock, config, m) {
        Ok(plan) => plan,
        Err(why) => return vec![warn_row(format!("the bundles cannot be rendered: {why}"))],
    };
    if nothing_to_keep(paths, &plan) {
        return vec![];
    }
    let pastor = if m.local {
        std::env::current_exe()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "pastor".into())
    } else {
        "pastor".to_string()
    };
    let homes = plan::homes_of(flock, config, m);
    // A failed apply is said first: it is why a task here does not start,
    // and a removal that failed is the only sign bundles taken off the
    // machine are still on it.
    let mut out: Vec<Row> = last_apply_failed_row(paths, &m.name).into_iter().collect();
    match check(paths, shell, &plan, &pastor, &homes).await {
        Ok(checks) => out.extend(rows(&m.name, &checks)),
        Err(why) => out.push(warn_row(format!(
            "the bundles on {} could not be read: {why}",
            m.name
        ))),
    }
    out
}

/// A `fail` row when the last apply to `machine` did not go through.
fn last_apply_failed_row(paths: &Paths, machine: &str) -> Option<Row> {
    let last = Status::load(paths).machines.get(machine).cloned()?;
    if last.ok {
        return None;
    }
    Some(Row {
        id: ROW.into(),
        agent: None,
        status: RowStatus::Fail,
        first_line: format!(
            "the last bundle apply failed: {}",
            last.error.unwrap_or_default()
        ),
        reason: Some(format!(
            "tasks do not start here until it is fixed; then `pastor bundle apply {machine}`"
        )),
    })
}

fn warn_row(first_line: String) -> Row {
    Row {
        id: ROW.into(),
        agent: None,
        status: RowStatus::Warn,
        first_line,
        reason: None,
    }
}

/// Whether `plan` places nothing and nothing was placed before. Such a
/// machine is not asked about, whatever its last apply said.
pub fn nothing_to_keep(paths: &Paths, plan: &Plan) -> bool {
    plan.items.is_empty()
        && plan.versions.is_empty()
        && !Status::load(paths).stale(&plan.machine, plan)
}

/// What `machine describe` says of a machine's bundles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigDescription {
    /// `auto` or `manual`: how the machine takes a new plan.
    pub apply_mode: Apply,
    /// The bundles the machine is meant to have, at the version the head
    /// would apply.
    pub bundles: Vec<WantedBundle>,
    /// The plan is not what the machine last took.
    pub stale: bool,
    /// The last apply to the machine, by anyone; none if never.
    #[serde(default)]
    pub last_apply: Option<LastApply>,
    /// Each harness home as the head last read it.
    pub homes: Vec<HomeConfig>,
    /// When the homes were last read; none before the head has.
    #[serde(default)]
    pub checked_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WantedBundle {
    pub id: String,
    /// The author's version.
    pub version: String,
    pub hash: String,
    /// The hash last applied to the machine, if it is another.
    #[serde(default)]
    pub applied_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastApply {
    pub at: DateTime<Utc>,
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HomeConfig {
    pub path: String,
    pub kind: String,
    pub agents: Vec<String>,
    /// As the head last read it; none before it has.
    #[serde(default)]
    pub state: Option<HomeState>,
    /// Whether pastor manages the home whole: no link it does not write
    /// through and no key it would not overwrite. None before the head has
    /// read the home.
    #[serde(default)]
    pub adopted: Option<bool>,
}

/// `m`'s `config` from the files, the status and (when the head has them)
/// its last readings of the homes.
pub fn describe(
    paths: &Paths,
    flock: &Flock,
    config: &Config<'_>,
    m: &MachineConfig,
    checks: Option<&(DateTime<Utc>, Vec<HomeCheck>)>,
) -> Option<ConfigDescription> {
    let plan = plan::render(paths, flock, config, m).ok()?;
    let status = Status::load(paths);
    let homes = plan::homes_of(flock, config, m);
    if plan.items.is_empty() && plan.versions.is_empty() && !status.machines.contains_key(&m.name) {
        return None;
    }
    let applied = status.machines.get(&m.name);
    let bundles = plan
        .versions
        .iter()
        .map(|v| {
            let version = store::read_versions(paths, &v.bundle)
                .ok()
                .and_then(|all| all.into_iter().find(|x| x.hash == v.hash))
                .map(|x| x.version)
                .or_else(|| {
                    Manifest::load(&paths.bundle_versions_dir(&v.bundle).join(&v.hash))
                        .ok()
                        .map(|m| m.version)
                })
                .unwrap_or_default();
            let was = applied.and_then(|a| a.bundles.get(&v.bundle));
            WantedBundle {
                id: v.bundle.clone(),
                version,
                hash: v.hash.clone(),
                applied_hash: was.filter(|h| **h != v.hash).cloned(),
            }
        })
        .collect();
    let homes = homes
        .iter()
        .map(|h| {
            let read = checks.and_then(|(_, c)| c.iter().find(|c| c.path == h.path));
            HomeConfig {
                path: h.path.clone(),
                kind: h.kind.clone(),
                agents: h.agents.clone(),
                state: read.map(|c| c.state),
                adopted: read.map(|c| c.state.adopted()),
            }
        })
        .collect();
    Some(ConfigDescription {
        apply_mode: flock.bundle_apply(m),
        bundles,
        stale: status.stale(&m.name, &plan),
        last_apply: applied.map(|a| LastApply {
            at: a.at,
            ok: a.ok,
            error: a.error.clone(),
        }),
        homes,
        checked_at: checks.map(|(at, _)| *at),
    })
}

/// What stands between a task and its machine's bundles.
pub trait Gate: Send + Sync {
    /// Called before a task starts on `machine`. `Err` says why it must not:
    /// the last apply failed, or `auto` just tried and failed again.
    fn before_launch<'a>(
        &'a self,
        machine: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
}

/// A machine actor's gate; none for a fleet with no keeper.
#[derive(Clone, Default)]
pub struct GateHandle(pub Option<Arc<dyn Gate>>);

impl std::fmt::Debug for GateHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() { "Gate" } else { "no gate" })
    }
}

impl PartialEq for GateHandle {
    /// The same keeper, so a settings change that is not the gate does not
    /// look like one, and a new keeper replaces the actors.
    fn eq(&self, other: &GateHandle) -> bool {
        match (&self.0, &other.0) {
            (Some(a), Some(b)) => std::ptr::addr_eq(Arc::as_ptr(a), Arc::as_ptr(b)),
            (None, None) => true,
            _ => false,
        }
    }
}

/// What the keeper knows of one machine between ticks.
#[derive(Default)]
struct Watch {
    /// The `at` of the last apply it has told about; `None` before it has
    /// seen one, which it does not tell about (it may be a restart).
    reported: Option<DateTime<Utc>>,
    /// The plan it last failed to apply on its own, and when.
    failed: Option<(String, Instant)>,
    /// The last reading of the homes.
    read: Option<(DateTime<Utc>, Instant, Vec<HomeCheck>)>,
}

/// The loop that keeps the machines' bundles, and the launch gate.
pub struct Keeper {
    paths: Paths,
    fleet: Weak<Fleet>,
    reach: Reach,
    watch: Mutex<BTreeMap<String, Watch>>,
    /// One apply on a machine at a time.
    busy: Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

/// The parts of the head's configuration a plan is rendered from.
struct Snapshot {
    flock: Flock,
    defaults: crate::config::Defaults,
    agents: crate::config::Agents,
    models: crate::config::Models,
    profiles: crate::config::profile::Profiles,
    harnesses: Arc<crate::harness_manifest::Harnesses>,
}

impl Snapshot {
    fn of(fleet: &Fleet) -> Snapshot {
        Snapshot {
            flock: fleet.flock(),
            defaults: fleet.defaults(),
            agents: fleet.agents(),
            models: fleet.models(),
            profiles: fleet.profiles(),
            harnesses: fleet.harnesses(),
        }
    }

    fn parts(&self) -> Config<'_> {
        Config {
            defaults: &self.defaults,
            agents: &self.agents,
            models: &self.models,
            profiles: &self.profiles,
            harnesses: &self.harnesses,
            herdr_pin: None,
            head_address: None,
        }
    }
}

impl Keeper {
    pub fn new(paths: Paths, fleet: Weak<Fleet>, reach: Reach) -> Arc<Keeper> {
        Arc::new(Keeper {
            paths,
            fleet,
            reach,
            watch: Mutex::default(),
            busy: Mutex::default(),
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

    fn lock_of(&self, machine: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.busy
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(machine.to_string())
            .or_default()
            .clone()
    }

    fn with_watch<T>(&self, machine: &str, f: impl FnOnce(&mut Watch) -> T) -> T {
        let mut all = self.watch.lock().unwrap_or_else(|p| p.into_inner());
        f(all.entry(machine.to_string()).or_default())
    }

    /// The last reading of `machine`'s homes, for `machine describe`.
    pub fn last_read(&self, machine: &str) -> Option<(DateTime<Utc>, Vec<HomeCheck>)> {
        self.with_watch(machine, |w| {
            w.read.as_ref().map(|(at, _, c)| (*at, c.clone()))
        })
    }

    /// One pass over the machines.
    pub async fn tick(&self) {
        let Some(fleet) = self.fleet.upgrade() else {
            return;
        };
        let snap = Snapshot::of(&fleet);
        for m in &snap.flock.machines {
            self.keep(&fleet, &snap, m).await;
        }
    }

    async fn keep(&self, fleet: &Fleet, snap: &Snapshot, m: &MachineConfig) {
        let Some((shell, pastor)) = (self.reach)(m) else {
            return;
        };
        let plan = match plan::render(&self.paths, &snap.flock, &snap.parts(), m) {
            Ok(plan) => plan,
            Err(why) => {
                tracing::debug!(machine = %m.name, %why, "bundles do not render");
                return;
            }
        };
        let status = Status::load(&self.paths);
        self.observe(fleet, &m.name, status.machines.get(&m.name));
        let stale = status.stale(&m.name, &plan);
        if stale
            && snap.flock.bundle_apply(m) == Apply::Auto
            && self.may_retry(&m.name, &status::fingerprint(&plan))
        {
            let lock = self.lock_of(&m.name);
            let _held = lock.lock().await;
            // A launch may have applied it while this waited for the lock.
            if Status::load(&self.paths).stale(&m.name, &plan) {
                self.apply_now(fleet, shell.clone(), &plan, &pastor).await;
            }
        }
        let due = self.with_watch(&m.name, |w| {
            w.read
                .as_ref()
                .is_none_or(|(_, at, _)| at.elapsed() >= CHECK_EVERY)
        });
        if due && !nothing_to_keep(&self.paths, &plan) {
            let homes = plan::homes_of(&snap.flock, &snap.parts(), m);
            match check(&self.paths, shell, &plan, &pastor, &homes).await {
                Ok(checks) => self.note(fleet, &m.name, checks),
                Err(why) => {
                    tracing::warn!(machine = %m.name, %why, "read the bundles on a machine");
                    // Not again until the next interval.
                    self.with_watch(&m.name, |w| {
                        if let Some((_, at, _)) = &mut w.read {
                            *at = Instant::now();
                        }
                    });
                }
            }
        }
    }

    /// Whether the keeper may try `fingerprint` on `machine` on its own:
    /// not within `RETRY_AFTER` of failing it.
    fn may_retry(&self, machine: &str, fingerprint: &str) -> bool {
        self.with_watch(machine, |w| match &w.failed {
            Some((fp, at)) => fp != fingerprint || at.elapsed() >= RETRY_AFTER,
            None => true,
        })
    }

    /// Tell about an apply someone else recorded since the last look.
    fn observe(&self, fleet: &Fleet, machine: &str, applied: Option<&Applied>) {
        let Some(a) = applied else { return };
        let first = self.with_watch(machine, |w| match w.reported {
            Some(at) if at == a.at => None,
            Some(_) => Some(false),
            None => Some(true),
        });
        let Some(restart) = first else { return };
        self.with_watch(machine, |w| w.reported = Some(a.at));
        if !restart {
            announce(fleet, machine, a, None);
        }
    }

    /// Apply `plan` now, record and tell about it. The caller holds the
    /// machine's lock.
    async fn apply_now(
        &self,
        fleet: &Fleet,
        shell: Arc<dyn Shell>,
        plan: &Plan,
        pastor: &str,
    ) -> Outcome {
        let out = apply_recorded(&self.paths, shell, plan, pastor).await;
        self.with_watch(&plan.machine, |w| {
            w.reported = Some(out.applied.at);
            // A result that could not be written counts as a failure here,
            // or the next tick would find the machine stale and apply again.
            w.failed = (!out.applied.ok || !out.recorded)
                .then(|| (status::fingerprint(plan), Instant::now()));
            // What was read before is not what is there now.
            w.read = None;
        });
        announce(fleet, &plan.machine, &out.applied, out.report.as_ref());
        out
    }

    /// Keep a new reading of `machine`'s homes, and say when a home that was
    /// `ok` is not any more.
    fn note(&self, fleet: &Fleet, machine: &str, checks: Vec<HomeCheck>) {
        let before: BTreeMap<String, HomeState> = self.with_watch(machine, |w| {
            w.read
                .as_ref()
                .map(|(_, _, c)| c.iter().map(|c| (c.path.clone(), c.state)).collect())
                .unwrap_or_default()
        });
        for c in &checks {
            let was = before.get(&c.path).copied();
            let kind = match (was, c.state) {
                (Some(HomeState::Ok | HomeState::Stale), HomeState::Drift) => "bundle.drift",
                (Some(w), HomeState::Conflict) if w != HomeState::Conflict => "bundle.conflict",
                _ => continue,
            };
            fleet.emit_machine(
                kind,
                machine,
                Some(serde_json::json!({
                    "home": c.path,
                    "agents": c.agents,
                    "items": c.items,
                })),
            );
        }
        self.with_watch(machine, |w| {
            w.read = Some((Utc::now(), Instant::now(), checks))
        });
    }

    async fn gate(&self, machine: &str) -> Result<(), String> {
        let Some(fleet) = self.fleet.upgrade() else {
            return Ok(());
        };
        let snap = Snapshot::of(&fleet);
        let Some(m) = snap.flock.get(machine) else {
            return Ok(());
        };
        let Some((shell, pastor)) = (self.reach)(m) else {
            return Ok(());
        };
        let plan = match plan::render(&self.paths, &snap.flock, &snap.parts(), m) {
            Ok(plan) => plan,
            Err(why) => {
                // A broken bundle setup is `machine check`'s to say; it
                // does not stop work that never asked for it.
                tracing::warn!(machine, %why, "bundles do not render; launching anyway");
                return Ok(());
            }
        };
        // Wait out an apply under way, then read what it left.
        let lock = self.lock_of(machine);
        let _held = lock.lock().await;
        let status = Status::load(&self.paths);
        let stale = status.stale(machine, &plan);
        if plan.items.is_empty() && plan.versions.is_empty() && !stale {
            // Nothing is meant for it and nothing is left to take off: a
            // failed apply that placed nothing is not the task's to answer for.
            return Ok(());
        }
        let last_failed = status
            .machines
            .get(machine)
            .filter(|a| !a.ok)
            .map(|a| a.error.clone().unwrap_or_default());
        let blocked = |why: String| {
            format!(
                "the last bundle apply to {machine} failed: {why}; fix it, then `pastor bundle apply {machine}`"
            )
        };
        if snap.flock.bundle_apply(m) == Apply::Auto && stale {
            // A failure a moment ago is the answer; trying again on every
            // launch would only make each wait for it.
            let fresh = self.with_watch(machine, |w| match &w.failed {
                Some((fp, at)) => *fp == status::fingerprint(&plan) && at.elapsed() < FRESH_FAILURE,
                None => false,
            });
            if fresh && let Some(why) = last_failed {
                return Err(blocked(why));
            }
            let out = self.apply_now(&fleet, shell, &plan, &pastor).await;
            return match out.applied.error {
                Some(why) if !out.applied.ok => Err(blocked(why)),
                _ => Ok(()),
            };
        }
        match last_failed {
            Some(why) => Err(blocked(why)),
            None => Ok(()),
        }
    }
}

impl Gate for Keeper {
    fn before_launch<'a>(
        &'a self,
        machine: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            match tokio::time::timeout(GATE_TIMEOUT, self.gate(machine)).await {
                Ok(answer) => answer,
                Err(_) => Err(format!(
                    "applying bundles to {machine} took longer than {GATE_TIMEOUT:?}"
                )),
            }
        })
    }
}

/// `bundle.applied` or `bundle.failed` for an apply, and `bundle.conflict`
/// for the rows an apply could not place for want of room.
fn announce(fleet: &Fleet, machine: &str, a: &Applied, report: Option<&Report>) {
    if a.ok {
        let count =
            |want: Action| report.map_or(0, |r| r.rows.iter().filter(|x| x.action == want).count());
        fleet.emit_machine(
            "bundle.applied",
            machine,
            Some(serde_json::json!({
                "bundles": a.bundles,
                "added": count(Action::Added),
                "updated": count(Action::Updated),
                "removed": count(Action::Removed),
            })),
        );
    } else {
        fleet.emit_machine(
            "bundle.failed",
            machine,
            Some(serde_json::json!({ "error": a.error })),
        );
    }
    let Some(report) = report else { return };
    let conflicts: Vec<String> = report
        .rows
        .iter()
        .filter(|r| matches!(r.action, Action::Conflict | Action::Linked))
        .map(|r| format!("{} {}", action_word(r.action), r.path))
        .collect();
    if !conflicts.is_empty() {
        fleet.emit_machine(
            "bundle.conflict",
            machine,
            Some(serde_json::json!({ "items": conflicts })),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::apply::Row as ApplyRow;
    use crate::bundle::plan::tests::{FLOCK, PASTOR_TOML, add_team_rules};
    use crate::config::PastorConfig;
    use crate::provision::run::testing::Fake;
    use std::path::Path;

    fn row(home: Option<&str>, path: &str, action: Action) -> ApplyRow {
        ApplyRow {
            bundle: "b".into(),
            home: home.map(str::to_string),
            path: path.into(),
            key: None,
            action,
            detail: None,
            old: None,
            backup: None,
        }
    }

    fn report(rows: Vec<ApplyRow>) -> Report {
        Report {
            machine: "m".into(),
            dry_run: true,
            rows,
            backup: None,
        }
    }

    fn homes() -> Vec<Home> {
        vec![
            Home {
                kind: "claude".into(),
                path: "~/.claude".into(),
                agents: vec!["claude".into()],
            },
            Home {
                kind: "claude".into(),
                path: "~/.claude-main".into(),
                agents: vec!["claude-main".into(), "claude-base".into()],
            },
        ]
    }

    fn state_of(checks: &[HomeCheck], home: &str) -> HomeState {
        checks.iter().find(|c| c.path == home).unwrap().state
    }

    /// Each row of the dry run puts its home in one of the five states, the
    /// worst one wins, and a row with no home counts for every home.
    #[test]
    fn a_home_is_ok_stale_drift_not_adopted_or_conflict() {
        for (action, want) in [
            (Action::Kept, HomeState::Ok),
            (Action::Added, HomeState::Stale),
            (Action::Updated, HomeState::Stale),
            (Action::Removed, HomeState::Stale),
            (Action::ChangedByHand, HomeState::Drift),
            (Action::Linked, HomeState::NotAdopted),
            (Action::Conflict, HomeState::Conflict),
        ] {
            let checks = fold(
                &homes(),
                &report(vec![row(
                    Some("~/.claude-main"),
                    "~/.claude-main/x",
                    action,
                )]),
            );
            assert_eq!(state_of(&checks, "~/.claude-main"), want, "{action:?}");
            assert_eq!(state_of(&checks, "~/.claude"), HomeState::Ok, "{action:?}");
        }
        // The worst wins: conflict over a link over drift over stale.
        let all = [
            Action::Updated,
            Action::ChangedByHand,
            Action::Linked,
            Action::Conflict,
        ];
        for n in 1..=all.len() {
            let rows = all[..n]
                .iter()
                .map(|a| row(Some("~/.claude"), "~/.claude/x", *a))
                .collect();
            let checks = fold(&homes(), &report(rows));
            let want = [
                HomeState::Stale,
                HomeState::Drift,
                HomeState::NotAdopted,
                HomeState::Conflict,
            ][n - 1];
            assert_eq!(state_of(&checks, "~/.claude"), want);
        }
        // A new version to install touches no home in particular: all are stale.
        let checks = fold(
            &homes(),
            &report(vec![row(None, "~/store/b/ab", Action::Added)]),
        );
        assert!(checks.iter().all(|c| c.state == HomeState::Stale));
        // A home pastor wrote to that is no agent's now is still read.
        let checks = fold(
            &homes(),
            &report(vec![row(
                Some("~/.gone"),
                "~/.gone/CLAUDE.md",
                Action::Removed,
            )]),
        );
        assert_eq!(checks.len(), 3);
        assert_eq!(state_of(&checks, "~/.gone"), HomeState::Stale);
        // Skipped items are not a state; items name what is not ok.
        let checks = fold(
            &homes(),
            &report(vec![
                row(Some("~/.claude"), "~/.claude/a", Action::Skipped),
                row(Some("~/.claude"), "~/.claude/b", Action::ChangedByHand),
            ]),
        );
        assert_eq!(checks[0].items, ["changed by hand ~/.claude/b"]);
    }

    #[test]
    fn a_home_that_is_not_ok_is_a_warning_with_its_fix() {
        let checks = fold(
            &homes(),
            &report(vec![
                row(
                    Some("~/.claude-main"),
                    "~/.claude-main/CLAUDE.md",
                    Action::Updated,
                ),
                row(Some("~/.claude"), "~/.claude/skills/x", Action::Conflict),
            ]),
        );
        let rows = rows("build-01", &checks);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.id == ROW));
        let work = rows
            .iter()
            .find(|r| r.agent.as_deref() == Some("claude-main"))
            .unwrap();
        assert_eq!(work.status, RowStatus::Warn);
        assert_eq!(
            work.first_line,
            "~/.claude-main (claude-main, claude-base): stale"
        );
        assert!(
            work.reason
                .as_deref()
                .unwrap()
                .contains("pastor bundle apply build-01"),
            "{work:?}"
        );
        let plain = rows
            .iter()
            .find(|r| r.agent.as_deref() == Some("claude"))
            .unwrap();
        assert_eq!(plain.first_line, "~/.claude (claude): conflict");
        let ok = rows_of_ok();
        assert_eq!(ok[0].status, RowStatus::Pass);
        assert_eq!(ok[0].reason, None);
    }

    fn rows_of_ok() -> Vec<Row> {
        rows("m", &fold(&homes(), &report(vec![])))
    }

    // The tests that start a head use the fake herdr.
    #[cfg(feature = "fake-herdr")]
    mod head {
        use super::*;
        use crate::store::Store;
        use tokio::sync::broadcast;

        /// Everything a keeper test needs: a head with a local machine that
        /// has the team-rules bundle, a fake machine whose `pastor` answers what
        /// a file says, and the events the fleet sends.
        struct Head {
            tmp: tempfile::TempDir,
            paths: Paths,
            fleet: Arc<Fleet>,
            keeper: Arc<Keeper>,
            events: broadcast::Receiver<crate::machine::PastorEvent>,
            script: std::path::PathBuf,
        }

        impl Head {
            async fn new(mode: &str) -> Head {
                let tmp = tempfile::tempdir().unwrap();
                let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
                add_team_rules(&paths, tmp.path());
                let home = tmp.path().join("home");
                std::fs::create_dir_all(&home).unwrap();
                let script = tmp.path().join("fake-pastor");
                std::fs::write(
                    &script,
                    "#!/bin/sh\nd=$(dirname \"$0\")\nif echo \"$*\" | grep -q dry-run; then k=dry; else k=real; fi\necho \"$k\" >> \"$d/calls\"\ncat \"$d/answer-$k.json\"\n",
                )
                .unwrap();
                std::fs::set_permissions(
                    &script,
                    std::os::unix::fs::PermissionsExt::from_mode(0o755),
                )
                .unwrap();
                let store = Arc::new(Store::open_in_memory().unwrap());
                let (events_tx, events) = broadcast::channel(64);
                let connect: crate::daemon::ConnectorFactory = Arc::new(|_m| {
                    Arc::new(crate::herdr::fake::FakeHerdr::new())
                        as Arc<dyn crate::herdr::Connector>
                });
                let fleet = Arc::new(Fleet::managed(store, events_tx, connect));
                let config = PastorConfig::parse(Path::new("pastor.toml"), PASTOR_TOML).unwrap();
                fleet.set_config(&config);
                let reach: Reach = {
                    let (script, home) = (script.clone(), home.clone());
                    Arc::new(move |_m| {
                        Some((
                            Arc::new(Fake::new(&home, Some("/usr/bin:/bin".into())))
                                as Arc<dyn Shell>,
                            script.to_string_lossy().into_owned(),
                        ))
                    })
                };
                let keeper = Keeper::new(paths.clone(), Arc::downgrade(&fleet), reach);
                fleet.set_keeper(keeper.clone());
                let head = Head {
                    tmp,
                    paths,
                    fleet,
                    keeper,
                    events,
                    script,
                };
                head.flock(mode).await;
                head.answer("real", &[]);
                head.answer("dry", &[]);
                head
            }

            /// flock.toml with `bundle_apply` as `mode` says ("" for none).
            async fn flock(&self, mode: &str) {
                let text = if mode.is_empty() {
                    FLOCK.to_string()
                } else {
                    FLOCK.replace(
                        "bundles = [\"team-rules\"]",
                        &format!("bundles = [\"team-rules\"]\nbundle_apply = \"{mode}\""),
                    )
                };
                let flock = Flock::parse(Path::new("flock.toml"), &text).unwrap();
                let config = PastorConfig::parse(Path::new("pastor.toml"), PASTOR_TOML).unwrap();
                self.fleet
                    .apply_flock(&flock, &crate::daemon::machine_settings(&config))
                    .await;
            }

            /// flock.toml with no bundle for the machine, which takes a change
            /// as `mode` says.
            async fn take_bundles_off(&self, mode: &str) {
                let text = format!(
                    "[[flock]]\nname = \"ops\"\ndefault = true\nmachines = {{ build-01 = 1 }}\n[[machine]]\nname = \"build-01\"\nlocal = true\nbundle_apply = \"{mode}\"\n"
                );
                self.fleet
                    .apply_flock(
                        &Flock::parse(Path::new("flock.toml"), &text).unwrap(),
                        &crate::daemon::machine_settings(&PastorConfig::default()),
                    )
                    .await;
            }

            /// `ten_minutes_pass` when the keeper has a failed plan on record,
            /// else nothing.
            fn ten_minutes_pass_if_failed(&self) {
                let failed = self.keeper.with_watch("build-01", |w| w.failed.is_some());
                if failed {
                    self.ten_minutes_pass();
                }
            }

            /// The keeper's backoff for a plan it failed has run out.
            fn ten_minutes_pass(&self) {
                self.keeper.with_watch("build-01", |w| {
                    let (fp, at) = w.failed.take().unwrap();
                    let past = at.checked_sub(RETRY_AFTER + Duration::from_secs(1));
                    w.failed = Some((fp, past.expect("a clock that has run long enough")));
                });
            }

            /// What the machine's `pastor` answers to `real` or `dry` runs.
            fn answer(&self, kind: &str, rows: &[ApplyRow]) {
                let body = serde_json::json!({
                    "machine": "build-01",
                    "dry_run": kind == "dry",
                    "rows": rows,
                });
                std::fs::write(
                    self.tmp.path().join(format!("answer-{kind}.json")),
                    body.to_string(),
                )
                .unwrap();
            }

            /// The machine has no `pastor`: the apply answers `nopastor`, and
            /// never starts.
            fn pastor_gone(&self) {
                std::fs::remove_file(&self.script).unwrap();
            }

            /// What the machine's `pastor` prints in place of a report: it
            /// answered nothing the head can read.
            fn answer_text(&self, kind: &str, text: &str) {
                std::fs::write(self.tmp.path().join(format!("answer-{kind}.json")), text).unwrap();
            }

            /// The runs the machine saw so far, `real` and `dry`.
            fn calls(&self) -> Vec<String> {
                std::fs::read_to_string(self.tmp.path().join("calls"))
                    .unwrap_or_default()
                    .lines()
                    .map(str::to_string)
                    .collect()
            }

            fn kinds(&mut self) -> Vec<String> {
                let mut out = Vec::new();
                while let Ok(ev) = self.events.try_recv() {
                    if ev.kind.starts_with("bundle.") {
                        out.push(ev.kind);
                    }
                }
                out
            }

            fn status(&self) -> Option<Applied> {
                Status::load(&self.paths).machines.get("build-01").cloned()
            }

            /// A new version of the team-rules bundle: the plan changes.
            fn new_version(&self) {
                let dir = self.tmp.path().join("src/team-rules");
                std::fs::write(
                    dir.join("skills/reviewing-a-pr/SKILL.md"),
                    "Read the diff twice.\n",
                )
                .unwrap();
                crate::bundle::store::add(&self.paths, &crate::bundle::store::Source::Dir(dir))
                    .unwrap();
            }
        }

        fn failed_row() -> ApplyRow {
            row(
                Some("~/.claude-main"),
                "~/.claude-main/CLAUDE.md",
                Action::Failed,
            )
        }

        /// In `auto` the keeper applies a machine whose plan is ahead of what it
        /// took, once, tells the fleet, and applies again when a bundle changes;
        /// it does not apply what is already there.
        #[tokio::test]
        async fn auto_applies_on_the_next_tick_after_a_change() {
            let mut head = Head::new("auto").await;
            head.keeper.tick().await;
            let first = head.status().expect("applied on the first tick");
            assert!(first.ok);
            assert_eq!(head.calls().iter().filter(|c| *c == "real").count(), 1);
            assert_eq!(head.kinds(), ["bundle.applied"]);
            // Nothing changed: no second apply, only a reading of the homes.
            head.keeper.tick().await;
            assert_eq!(head.calls().iter().filter(|c| *c == "real").count(), 1);
            assert!(head.kinds().is_empty());
            // A new version of the bundle is ahead of the machine.
            head.new_version();
            head.keeper.tick().await;
            assert_eq!(head.calls().iter().filter(|c| *c == "real").count(), 2);
            assert_eq!(head.kinds(), ["bundle.applied"]);
            assert_ne!(head.status().unwrap().fingerprint, first.fingerprint);
            // A vars edit is a change too.
            head.flock("auto").await;
            let text = FLOCK.replace("https://tickets.example.com", "https://other.example.com");
            let flock = Flock::parse(
                Path::new("flock.toml"),
                &text.replace(
                    "bundles = [\"team-rules\"]",
                    "bundles = [\"team-rules\"]\nbundle_apply = \"auto\"",
                ),
            )
            .unwrap();
            let config = PastorConfig::parse(Path::new("pastor.toml"), PASTOR_TOML).unwrap();
            head.fleet
                .apply_flock(&flock, &crate::daemon::machine_settings(&config))
                .await;
            head.keeper.tick().await;
            assert_eq!(head.calls().iter().filter(|c| *c == "real").count(), 3);
        }

        /// In `manual` (also the default, with no `bundle_apply`) the keeper
        /// holds: the machine is stale and nothing is written until a person
        /// applies. What they apply is told to the fleet at the next tick.
        #[tokio::test]
        async fn manual_holds_and_the_row_says_stale() {
            for mode in ["manual", ""] {
                let mut head = Head::new(mode).await;
                head.answer(
                    "dry",
                    &[row(
                        Some("~/.claude-main"),
                        "~/.claude-main/CLAUDE.md",
                        Action::Added,
                    )],
                );
                head.keeper.tick().await;
                head.keeper.tick().await;
                assert!(head.status().is_none(), "{mode}: nothing applied");
                assert!(!head.calls().contains(&"real".to_string()), "{mode}");
                assert!(head.kinds().is_empty());
                // The homes were read, and `describe` says stale.
                let read = head.keeper.last_read("build-01").expect("read");
                assert_eq!(state_of(&read.1, "~/.claude-main"), HomeState::Stale);
                let snap = Snapshot::of(&head.fleet);
                let desc = describe(
                    &head.paths,
                    &snap.flock,
                    &snap.parts(),
                    snap.flock.get("build-01").unwrap(),
                    Some(&read),
                )
                .unwrap();
                assert_eq!(desc.apply_mode, Apply::Manual);
                assert!(desc.stale);
                assert_eq!(desc.bundles[0].id, "team-rules");
                assert_eq!(desc.bundles[0].version, "1");
                assert!(desc.last_apply.is_none());
                assert_eq!(desc.homes[0].state, Some(HomeState::Stale));
                assert_eq!(desc.homes[0].adopted, Some(true));
            }
        }

        /// A person's `bundle apply` is recorded like the keeper's: the next
        /// tick tells the fleet about it (but not about one made before the head
        /// started), and the machine is not stale any more.
        #[tokio::test]
        async fn a_hand_apply_is_told_at_the_next_tick() {
            let mut head = Head::new("manual").await;
            let snap = Snapshot::of(&head.fleet);
            let m = snap.flock.get("build-01").unwrap();
            let plan = plan::render(&head.paths, &snap.flock, &snap.parts(), m).unwrap();
            let shell = (head.keeper.reach)(m).unwrap();
            // Before the keeper's first look: a restart, so no event.
            apply_recorded(&head.paths, shell.0.clone(), &plan, &shell.1).await;
            head.keeper.tick().await;
            assert!(head.kinds().is_empty());
            // After it: told.
            head.new_version();
            let plan = plan::render(&head.paths, &snap.flock, &snap.parts(), m).unwrap();
            assert!(Status::load(&head.paths).stale("build-01", &plan));
            head.answer("real", &[failed_row()]);
            apply_recorded(&head.paths, shell.0.clone(), &plan, &shell.1).await;
            head.keeper.tick().await;
            assert_eq!(head.kinds(), ["bundle.failed"]);
            head.answer("real", &[]);
            apply_recorded(&head.paths, shell.0, &plan, &shell.1).await;
            head.keeper.tick().await;
            assert_eq!(head.kinds(), ["bundle.applied"]);
        }

        /// A home that was ok and has drifted, and one that came to conflict, are
        /// told once, when they turn.
        #[tokio::test]
        async fn drift_and_conflict_are_told_when_a_home_turns() {
            let mut head = Head::new("auto").await;
            head.keeper.tick().await;
            assert_eq!(head.kinds(), ["bundle.applied"]);
            head.keeper.tick().await; // the first reading: all ok
            let after_check = |head: &Head| {
                // Make the next tick read the homes again.
                head.keeper.with_watch("build-01", |w| {
                    if let Some((_, at, _)) = &mut w.read {
                        *at = Instant::now() - CHECK_EVERY - Duration::from_secs(1);
                    }
                });
            };
            head.answer(
                "dry",
                &[row(
                    Some("~/.claude-main"),
                    "~/.claude-main/CLAUDE.md",
                    Action::ChangedByHand,
                )],
            );
            after_check(&head);
            head.keeper.tick().await;
            assert_eq!(head.kinds(), ["bundle.drift"]);
            // Still drifted: not told again.
            after_check(&head);
            head.keeper.tick().await;
            assert!(head.kinds().is_empty());
            head.answer(
                "dry",
                &[row(
                    Some("~/.claude-main"),
                    "~/.claude-main/skills/x",
                    Action::Conflict,
                )],
            );
            after_check(&head);
            head.keeper.tick().await;
            assert_eq!(head.kinds(), ["bundle.conflict"]);
            // Back to ok, then drifting again is told again.
            head.answer("dry", &[]);
            after_check(&head);
            head.keeper.tick().await;
            head.answer(
                "dry",
                &[row(
                    Some("~/.claude-main"),
                    "~/.claude-main/CLAUDE.md",
                    Action::ChangedByHand,
                )],
            );
            after_check(&head);
            head.keeper.tick().await;
            assert_eq!(head.kinds(), ["bundle.drift"]);
        }

        /// Before a task starts: `auto` applies a stale machine first and
        /// launches; `manual` launches with what is there and applies nothing.
        #[tokio::test]
        async fn the_launch_rule_follows_the_apply_mode() {
            let head = Head::new("auto").await;
            head.keeper.gate("build-01").await.unwrap();
            assert_eq!(head.calls(), ["real"], "auto applied first");
            assert!(head.status().unwrap().ok);
            // Up to date: nothing more.
            head.keeper.gate("build-01").await.unwrap();
            assert_eq!(head.calls(), ["real"]);

            let manual = Head::new("manual").await;
            manual.keeper.gate("build-01").await.unwrap();
            assert!(
                manual.calls().is_empty(),
                "manual launches with what is there"
            );
            assert!(manual.status().is_none());
            // A machine no bundle is meant for is not asked about.
            let none = Head::new("auto").await;
            none.fleet
                .apply_flock(
                    &Flock::parse(
                        Path::new("flock.toml"),
                        "[[flock]]\nname = \"ops\"\ndefault = true\nmachines = { build-01 = 1 }\n[[machine]]\nname = \"build-01\"\nlocal = true\n",
                    )
                    .unwrap(),
                    &crate::daemon::machine_settings(&PastorConfig::default()),
                )
                .await;
            none.keeper.gate("build-01").await.unwrap();
            assert!(none.calls().is_empty());
        }

        /// A failed apply blocks the task in both modes, with the reason: `auto`
        /// tried it just now, `manual` was told by the apply a person ran. A
        /// later good apply lets tasks through.
        #[tokio::test]
        async fn a_failed_apply_blocks_the_task_in_both_modes() {
            let auto = Head::new("auto").await;
            auto.answer("real", &[failed_row()]);
            let why = auto.keeper.gate("build-01").await.unwrap_err();
            assert!(
                why.contains("the last bundle apply to build-01 failed"),
                "{why}"
            );
            assert!(why.contains("failed ~/.claude-main/CLAUDE.md"), "{why}");
            assert!(why.contains("pastor bundle apply build-01"), "{why}");
            assert!(!auto.status().unwrap().ok);
            // Straight after, the failure is the answer: the machine is not asked again.
            auto.keeper.gate("build-01").await.unwrap_err();
            assert_eq!(auto.calls(), ["real"]);
            // Fixed, and the stale-failure window passed: applies and lets it through.
            auto.answer("real", &[]);
            auto.keeper.with_watch("build-01", |w| w.failed = None);
            auto.keeper.gate("build-01").await.unwrap();
            assert!(auto.status().unwrap().ok);

            let manual = Head::new("manual").await;
            let snap = Snapshot::of(&manual.fleet);
            let m = snap.flock.get("build-01").unwrap();
            let plan = plan::render(&manual.paths, &snap.flock, &snap.parts(), m).unwrap();
            // Stale, never applied: launches (nothing has failed).
            manual.keeper.gate("build-01").await.unwrap();
            manual.answer("real", &[failed_row()]);
            let shell = (manual.keeper.reach)(m).unwrap();
            apply_recorded(&manual.paths, shell.0.clone(), &plan, &shell.1).await;
            let why = manual.keeper.gate("build-01").await.unwrap_err();
            assert!(why.contains("failed"), "{why}");
            // The machine is unreachable the same way.
            manual.answer("real", &[]);
            apply_recorded(&manual.paths, shell.0, &plan, &shell.1).await;
            manual.keeper.gate("build-01").await.unwrap();
            let _ = &manual.script;
        }

        /// The machine's plan and what reaches it to apply it.
        fn plan_and_shell(head: &Head) -> (Plan, (Arc<dyn Shell>, String)) {
            let snap = Snapshot::of(&head.fleet);
            let m = snap.flock.get("build-01").unwrap();
            let plan = plan::render(&head.paths, &snap.flock, &snap.parts(), m).unwrap();
            (plan, (head.keeper.reach)(m).unwrap())
        }

        fn real_runs(head: &Head) -> usize {
            head.calls().iter().filter(|c| *c == "real").count()
        }

        /// An apply that never reached pastor on the machine (it is not on its
        /// PATH) placed nothing, so a machine it never reached is not recorded as
        /// holding the bundles: take them off the machine in the flock and it is
        /// not held, nothing is applied, its tasks start and the check says
        /// nothing, whether it was `auto` or `manual`. What an earlier apply
        /// placed stays recorded through it.
        #[tokio::test]
        async fn a_machine_the_apply_never_reached_is_not_held_once_its_bundles_come_off() {
            for mode in ["auto", "manual"] {
                let mut head = Head::new(mode).await;
                head.pastor_gone();
                if mode == "auto" {
                    head.keeper.tick().await;
                } else {
                    let (plan, (shell, pastor)) = plan_and_shell(&head);
                    apply_recorded(&head.paths, shell, &plan, &pastor).await;
                }
                let last = head.status().unwrap();
                assert!(!last.ok, "{mode}");
                assert!(last.bundles.is_empty(), "{mode}: {last:?}");
                assert!(head.kinds().iter().all(|k| k != "bundle.applied"));

                head.take_bundles_off(mode).await;
                let (plan, _) = plan_and_shell(&head);
                assert!(plan.items.is_empty() && plan.versions.is_empty());
                assert!(nothing_to_keep(&head.paths, &plan), "{mode}");
                let before = real_runs(&head);
                head.ten_minutes_pass_if_failed();
                for _ in 0..3 {
                    head.keeper.tick().await;
                }
                assert_eq!(real_runs(&head), before, "{mode}: nothing to apply");
                head.keeper.gate("build-01").await.unwrap();
            }

            // What an earlier apply placed is still on it through an apply that
            // was not answered: it stays held until taken off for real.
            let head = Head::new("auto").await;
            let (plan, (shell, pastor)) = plan_and_shell(&head);
            apply_recorded(&head.paths, shell.clone(), &plan, &pastor).await;
            assert!(head.status().unwrap().ok);
            head.pastor_gone();
            apply_recorded(&head.paths, shell, &plan, &pastor).await;
            let last = head.status().unwrap();
            assert!(!last.ok && !last.bundles.is_empty(), "{last:?}");
            head.take_bundles_off("auto").await;
            let (plan, _) = plan_and_shell(&head);
            assert!(!nothing_to_keep(&head.paths, &plan));
        }

        /// A shell whose `bundle apply` runs on the machine and is then cut
        /// short, as by a timeout: the head gets no output to read.
        struct CutShort(Arc<dyn Shell>);

        impl Shell for CutShort {
            fn run(
                &self,
                script: String,
                input: Vec<u8>,
                time: Duration,
            ) -> crate::provision::run::ShellFuture<'_> {
                let apply = script.contains("bundle apply --local");
                Box::pin(async move {
                    let ran = self.0.run(script, input, time).await;
                    if apply {
                        return Err("timed out after 120s".into());
                    }
                    ran
                })
            }
        }

        /// An apply cut short after pastor on the machine started it may have
        /// written: a timeout, or an answer that is no report. It is recorded
        /// as holding the plan's bundles, so once they are taken off the machine
        /// stays held and in view until a removal goes through, and `auto` tries
        /// that removal.
        #[tokio::test]
        async fn an_apply_cut_short_after_it_started_still_counts_as_placed() {
            for how in ["timeout", "an answer that is no report"] {
                let head = Head::new("manual").await;
                let (plan, (shell, pastor)) = plan_and_shell(&head);
                let shell: Arc<dyn Shell> = if how == "timeout" {
                    Arc::new(CutShort(shell))
                } else {
                    head.answer_text("real", "{\"machine\": \"build-01\", \"dry");
                    shell
                };
                let done = apply_recorded(&head.paths, shell, &plan, &pastor).await;
                assert!(done.report.is_none(), "{how}");
                assert_eq!(real_runs(&head), 1, "{how}: the machine ran it");
                let last = head.status().unwrap();
                assert!(!last.ok, "{how}");
                assert!(last.bundles.contains_key("team-rules"), "{how}: {last:?}");

                head.answer("real", &[]);
                head.take_bundles_off("manual").await;
                let (plan, _) = plan_and_shell(&head);
                assert!(plan.items.is_empty() && plan.versions.is_empty());
                assert!(!nothing_to_keep(&head.paths, &plan), "{how}");
                assert!(failed_row_of(&head).is_some(), "{how}: shown");
                head.keeper.gate("build-01").await.unwrap_err();

                head.take_bundles_off("auto").await;
                head.ten_minutes_pass_if_failed();
                head.keeper.tick().await;
                assert_eq!(real_runs(&head), 2, "{how}: the removal was tried");
                let last = head.status().unwrap();
                assert!(last.ok && last.bundles.is_empty(), "{how}: {last:?}");
                head.keeper.gate("build-01").await.unwrap();
            }
        }

        /// A machine no bundle is meant for is not held to its last apply: one
        /// that failed (it was down when a fleet-wide apply went by) neither
        /// fails its tasks nor is applied again, and the check says nothing.
        #[tokio::test]
        async fn a_machine_with_no_bundles_is_not_held_to_a_failed_apply() {
            let head = Head::new("auto").await;
            head.take_bundles_off("auto").await;
            let (plan, (shell, pastor)) = plan_and_shell(&head);
            assert!(plan.items.is_empty() && plan.versions.is_empty());
            head.answer("real", &[failed_row()]);
            apply_recorded(&head.paths, shell, &plan, &pastor).await;
            assert!(!head.status().unwrap().ok);
            let before = real_runs(&head);
            for _ in 0..3 {
                head.keeper.tick().await;
            }
            assert_eq!(real_runs(&head), before, "nothing to apply");
            head.keeper.gate("build-01").await.unwrap();
            assert_eq!(real_runs(&head), before);
            assert!(nothing_to_keep(&head.paths, &plan));
        }

        /// Bundles taken off a machine whose removal apply failed are still on
        /// it, and that stays in view: a failed row in `machine check`, a
        /// `bundle.failed` event, tasks held, and in `auto` the removal tried
        /// again once the backoff is up. A good removal clears all of it.
        #[tokio::test]
        async fn a_removal_that_failed_stays_visible_and_auto_retries_it() {
            let mut head = Head::new("auto").await;
            head.keeper.tick().await;
            assert!(head.status().unwrap().ok);
            assert_eq!(head.kinds(), ["bundle.applied"]);
            assert!(!head.status().unwrap().bundles.is_empty());

            head.take_bundles_off("auto").await;
            head.answer("real", &[failed_row()]);
            head.keeper.tick().await;
            assert_eq!(real_runs(&head), 2, "the removal was tried");
            assert_eq!(head.kinds(), ["bundle.failed"]);
            let last = head.status().unwrap();
            assert!(!last.ok);
            assert!(!last.bundles.is_empty(), "they are still on the machine");
            // Visible: a failed row, and the tasks held.
            let row = failed_row_of(&head).expect("a row");
            assert_eq!(row.status, RowStatus::Fail);
            assert!(
                row.first_line.contains("failed ~/.claude-main/CLAUDE.md"),
                "{row:?}"
            );
            head.keeper.gate("build-01").await.unwrap_err();
            // Not again within the backoff.
            for _ in 0..3 {
                head.keeper.tick().await;
            }
            assert_eq!(real_runs(&head), 2);

            // Fixed, and the backoff over: the removal goes through.
            head.answer("real", &[]);
            head.ten_minutes_pass();
            head.keeper.tick().await;
            assert_eq!(real_runs(&head), 3);
            assert_eq!(head.kinds(), ["bundle.applied"]);
            let last = head.status().unwrap();
            assert!(last.ok && last.bundles.is_empty());
            assert!(failed_row_of(&head).is_none());
            head.keeper.gate("build-01").await.unwrap();

            // In `manual` nothing retries it, but it is just as visible.
            let manual = Head::new("manual").await;
            let (plan, (shell, pastor)) = plan_and_shell(&manual);
            apply_recorded(&manual.paths, shell.clone(), &plan, &pastor).await;
            manual.take_bundles_off("manual").await;
            manual.answer("real", &[failed_row()]);
            let (plan, _) = plan_and_shell(&manual);
            apply_recorded(&manual.paths, shell, &plan, &pastor).await;
            let before = real_runs(&manual);
            manual.keeper.tick().await;
            assert_eq!(real_runs(&manual), before);
            assert!(failed_row_of(&manual).is_some());
            manual.keeper.gate("build-01").await.unwrap_err();
        }

        fn failed_row_of(head: &Head) -> Option<Row> {
            last_apply_failed_row(&head.paths, "build-01")
        }

        /// `auto` does not try a plan again within ten minutes of failing it, on
        /// any number of ticks, and tries it once the time is up.
        #[tokio::test]
        async fn auto_does_not_retry_a_failed_plan_for_ten_minutes() {
            let mut head = Head::new("auto").await;
            head.answer("real", &[failed_row()]);
            for _ in 0..4 {
                head.keeper.tick().await;
            }
            assert_eq!(real_runs(&head), 1);
            assert_eq!(head.kinds(), ["bundle.failed"]);
            // Ten minutes on.
            head.ten_minutes_pass();
            head.keeper.tick().await;
            assert_eq!(real_runs(&head), 2);
            // A new plan is tried at once.
            head.new_version();
            head.keeper.tick().await;
            assert_eq!(real_runs(&head), 3);
        }

        /// A launch that applied while the keeper waited for the lock is not
        /// followed by the keeper applying the same plan again.
        #[tokio::test]
        async fn the_keeper_looks_again_once_it_holds_the_lock() {
            let mut head = Head::new("auto").await;
            let (plan, (shell, pastor)) = plan_and_shell(&head);
            let lock = head.keeper.lock_of("build-01");
            let held = lock.lock().await;
            let keeper = head.keeper.clone();
            let tick = tokio::spawn(async move { keeper.tick().await });
            // The tick runs as far as the lock, where it waits.
            tokio::task::yield_now().await;
            // The apply that holds the lock finishes, and is recorded.
            apply_recorded(&head.paths, shell, &plan, &pastor).await;
            drop(held);
            tick.await.unwrap();
            assert_eq!(real_runs(&head), 1, "applied once");
            assert!(head.kinds().is_empty(), "no second apply to tell about");
        }

        /// When the result cannot be written, `auto` does not apply on every
        /// tick: it counts as a failure and waits.
        #[tokio::test]
        async fn an_apply_that_cannot_be_recorded_is_not_repeated_every_tick() {
            let head = Head::new("auto").await;
            // The status file is where a directory is: nothing can be written.
            std::fs::create_dir_all(status::file(&head.paths)).unwrap();
            for _ in 0..3 {
                head.keeper.tick().await;
            }
            assert_eq!(real_runs(&head), 1);
        }
    }

    /// `machine check` has no `bundles` row for a machine no bundle is meant
    /// for, and a warning (not a failure) for one whose homes cannot be read
    /// because pastor is not there.
    #[test]
    fn the_check_has_a_row_only_where_there_are_bundles() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        add_team_rules(&paths, tmp.path());
        let config = PastorConfig::parse(Path::new("pastor.toml"), PASTOR_TOML).unwrap();
        let harnesses = crate::harness_manifest::Harnesses::builtin();
        let parts = Config {
            defaults: &config.defaults,
            agents: &config.agents,
            models: &config.models,
            profiles: &config.profiles,
            harnesses,
            herdr_pin: None,
            head_address: None,
        };
        let home = tmp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let shell: Arc<dyn Shell> = Arc::new(Fake::new(&home, Some("/usr/bin:/bin".into())));
        let ssh = FLOCK.replace("local = true", "ssh = \"user@build-01\"");
        let flock = Flock::parse(Path::new("flock.toml"), &ssh).unwrap();
        let m = flock.get("build-01").unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let rows = rt.block_on(check_rows(&paths, &flock, &parts, m, shell.clone()));
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].id, ROW);
        assert_eq!(rows[0].status, RowStatus::Warn);
        assert!(rows[0].first_line.contains("could not be read"), "{rows:?}");
        assert!(rows[0].first_line.contains("not on its PATH"), "{rows:?}");
        // No bundle named: no row, and the machine is not asked.
        let bare = ssh.replace("bundles = [\"team-rules\"]\n", "");
        let bare = Flock::parse(Path::new("flock.toml"), &bare).unwrap();
        let m = bare.get("build-01").unwrap();
        assert!(
            rt.block_on(check_rows(&paths, &bare, &parts, m, shell.clone()))
                .is_empty()
        );
        // Its bundles taken off and the removal failed: they are still on
        // it, and the failed apply is the first row.
        let plan = plan::render(&paths, &flock, &parts, flock.get("build-01").unwrap()).unwrap();
        status::record(&paths, "build-01", &plan, true, true, None).unwrap();
        let off = Plan {
            machine: "build-01".into(),
            ..Default::default()
        };
        status::record(&paths, "build-01", &off, false, true, Some("down".into())).unwrap();
        let rows = rt.block_on(check_rows(&paths, &bare, &parts, m, shell));
        assert_eq!(rows.len(), 2, "{rows:?}");
        assert_eq!(rows[0].status, RowStatus::Fail);
        assert!(rows[0].first_line.contains("failed: down"), "{rows:?}");
        assert_eq!(rows[1].status, RowStatus::Warn);
    }
}
