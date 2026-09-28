use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::broadcast;

use crate::config::flock::{
    DEFAULT_FLOCK, EditError, Flock, FlockDoc, MachineConfig, TaskFlockError,
};
use crate::config::{
    AgentChoice, AgentPick, AgentRefusal, Agents, Defaults, Layer, MODEL_KIND_MISMATCH, Models,
    PastorConfig, Paths, profile::PROFILE_NOT_ALLOWED,
};
use crate::dispatch::{Claim, FlockSeat, MachineView, pick_machine, pick_machine_where};
use crate::herdr::{Connector, Endpoint};
use crate::ipc::{HeadPing, IpcRequest, IpcResponse, MODEL_PROTOCOL, check_protocol};
use crate::machine::{
    ActorStopped, MachineHandle, MachineSettings, OrphanClosed, PastorEvent, SendInput,
    SendRefused, ShutdownOutcome, spawn_machine,
};
use crate::queue::{QueueEntry, QueueSpot};
use crate::scheduler::{ConfigFingerprint, Scheduler, SchedulerHandle};
use crate::store::{MoveError, Moved, NewTask, PriorityError, RetryError, Store, TaskFilter};
use crate::task::{AgentSource, PANE_OWNING_STATES, Priority, Task, TaskRole, TaskState};

/// How a headless serve's fleet reaches the head with the items a job run
/// found (`IpcRequest::JobSubmit`): one request, its reply, or an `Err` for
/// an unreachable head or an error reply (a `CliError` with its code).
pub type HeadForward = crate::shepherd::Ask;

/// Builds a machine's transport from its flock entry. `serve` uses
/// `endpoint_factory`; tests hand out fakes by machine name.
pub type ConnectorFactory = Arc<dyn Fn(&MachineConfig) -> Arc<dyn Connector> + Send + Sync>;

/// The transports `pastor serve` uses: ssh, the local socket or a command.
pub fn endpoint_factory(paths: Paths) -> ConnectorFactory {
    Arc::new(move |m: &MachineConfig| {
        Arc::new(Endpoint::from_machine(m, &paths)) as Arc<dyn Connector>
    })
}

/// The actor timings `pastor.toml` sets. One place, so the daemon's start
/// and a reload build the same value and a reload can tell whether it changed.
pub fn machine_settings(config: &PastorConfig) -> MachineSettings {
    MachineSettings {
        settle: config.settle_duration(),
        reconcile_every: config.reconcile_duration(),
        request_timeout: config.request_timeout_duration(),
        agent_ready_timeout: config.agent_ready_timeout_duration(),
        poll_every: config.tick_duration(),
        close_done_after: config.close_done_after_duration(),
        agents: config.agents.clone(),
        head_address: config.head_address.clone(),
        ..Default::default()
    }
}

/// "text and 1 key", "2 keys": what `task send` reports it sent.
fn describe_input(input: &SendInput) -> String {
    let n = input.key_sequence().len();
    let keys = match n {
        0 => None,
        1 => Some("1 key".to_string()),
        n => Some(format!("{n} keys")),
    };
    match (input.text.is_some(), keys) {
        (true, Some(k)) => format!("text and {k}"),
        (true, None) => "text".into(),
        (false, Some(k)) => k,
        (false, None) => "nothing".into(),
    }
}

/// Open tasks (holding a pane) on machines `flock` does not have. No actor
/// reconciles them, so their state is the last one seen. They are never
/// marked done or failed and not counted toward capacity.
///
/// `states` narrows the SQL query to `PANE_OWNING_STATES` rather than
/// loading every historical task and filtering in Rust (Copilot 4103271094):
/// same idea as `Store::tasks_on_machine`, so a fleet with a long closed or
/// failed history does not have every row of it read on each pass.
pub fn tasks_on_removed_machines(store: &Store, flock: &Flock) -> anyhow::Result<Vec<Task>> {
    Ok(store
        .list_tasks(&TaskFilter {
            job: None,
            machine: None,
            states: Some(PANE_OWNING_STATES.to_vec()),
            flock: None,
        })?
        .into_iter()
        .filter(|t| t.machine.as_deref().is_some_and(|m| flock.get(m).is_none()))
        .collect())
}

/// One warning per task left on a removed machine, the first time `warned`
/// (task ids already reported) is told about it: at daemon start (a machine
/// taken out while pastor was down, `&mut HashSet::new()` so every task is
/// fresh) and after a reload took a machine out of the flock or is still
/// waiting for its actor to stop. A machine stuck `shutting_down` across
/// several reload passes keeps handing back the same task ids, so a caller
/// that keeps `warned` across passes (`Scheduler::warned_removed`) still
/// warns about each task only once. Returns the ids it warned about this
/// call, so a test can assert on that instead of captured logs.
pub fn warn_removed(store: &Store, flock: &Flock, warned: &mut HashSet<i64>) -> Vec<i64> {
    match tasks_on_removed_machines(store, flock) {
        Ok(tasks) => {
            warned.retain(|id| tasks.iter().any(|t| t.id == *id));
            let mut newly_warned = Vec::new();
            for t in tasks {
                if warned.insert(t.id) {
                    tracing::warn!(
                        task = %t.display_id(),
                        machine = t.machine.as_deref().unwrap_or("-"),
                        state = %t.state,
                        "task on a machine that is not in the flock: left as it was"
                    );
                    newly_warned.push(t.id);
                }
            }
            newly_warned
        }
        Err(err) => {
            tracing::error!(%err, "list tasks on removed machines");
            Vec::new()
        }
    }
}

/// The flock that stands for `name` according to `flock`
/// (`Flock::primary_flock`); a machine the file does not have (a fixed
/// fleet's, or one held until its actor ends) is in the default flock, the
/// only one a fixed fleet has.
fn flock_of(flock: &Flock, name: &str) -> String {
    flock
        .machine_flock(name)
        .unwrap_or(flock.default_flock())
        .to_string()
}

/// The flocks `status`'s machine is in according to `flock`, each with its
/// number (`Flock::flocks_of`) and how many of its live tasks run there, by
/// the flock stored on each task (`None`: the default). A machine the file
/// does not have is in the default flock, as in `flock_of`.
fn seats(flock: &Flock, status: &crate::machine::MachineStatus) -> Vec<FlockSeat> {
    let default = flock.default_flock();
    let flocks = flock
        .machine_flocks(&status.name)
        .unwrap_or_else(|| vec![(default, None)]);
    flocks
        .into_iter()
        .map(|(name, max)| FlockSeat {
            name: name.to_string(),
            max,
            live: status
                .live_by_flock
                .iter()
                .filter(|(f, _)| f.as_deref().unwrap_or(default) == name)
                .map(|(_, n)| n)
                .sum(),
        })
        .collect()
}

/// The part of a machine's entry its actor is built from. The flock, the
/// agent and its per-kind agents are not: they only decide which tasks the
/// machine is offered and what they run, which dispatch reads from the flock
/// last applied, so moving a machine or changing its agent keeps its
/// connection and the tasks already on it.
fn actor_config(m: &MachineConfig) -> MachineConfig {
    MachineConfig {
        flock: None,
        agent: None,
        agent_args: None,
        agents: Default::default(),
        description: None,
        ..m.clone()
    }
}

/// `settings` as the actor for `m` runs them: an agent on the head's own
/// machine (`local`) reaches the head through its socket, so it is not told
/// the head's address.
pub fn actor_settings(m: &MachineConfig, settings: &MachineSettings) -> MachineSettings {
    MachineSettings {
        head_address: settings.head_address.clone().filter(|_| !m.local),
        ..settings.clone()
    }
}

/// What `apply_flock` changed, by machine name, in flock order (`removed` in
/// the previous order).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FlockDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// Same name, different entry (target, session, capacity, tags) or
    /// different timings: the actor was replaced. Its tasks stay; the new
    /// actor reconciles them.
    pub retargeted: Vec<String>,
    /// To be removed or replaced, but the old actor has not ended yet
    /// (`ShutdownOutcome::StillRunning`). The machine stays in the fleet,
    /// out of dispatch, and the next pass tries again.
    pub shutting_down: Vec<String>,
}

impl FlockDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.retargeted.is_empty()
            && self.shutting_down.is_empty()
    }
}

/// Why `Fleet::queue_task` did not queue a task.
#[derive(Debug)]
pub enum QueueError<E = anyhow::Error> {
    /// Pinned to a machine that is not in the flock.
    UnknownMachine(String),
    /// Names a flock that does not exist, or one its pinned machine is not in.
    Flock(TaskFlockError),
    /// Its agent cannot be started as resolved: a model `[models]` lacks
    /// or of another kind (`Models::apply`), or a tool list it has no flag
    /// for (`Agents::launch_args`).
    Agent(AgentRefusal),
    /// The insert failed; for `queue_retry`, the `RetryError` that says why.
    Store(E),
}

#[derive(Clone)]
struct Member {
    handle: MachineHandle,
    /// What the actor was spawned from; `None` for handles given to
    /// `Fleet::new`, which `apply_flock` never manages.
    spawned_from: Option<(MachineConfig, MachineSettings)>,
    /// The actor was stopped but has not ended. Nothing is dispatched to it
    /// and no replacement is spawned until a later `apply_flock` sees it end.
    shutting_down: bool,
    /// A pull machine (`MachineConfig::pull`): no actor, never picked by a
    /// dispatch pass; it takes its tasks with `Fleet::claim`.
    pull: bool,
}

/// When the head last heard from a pull machine (`Fleet::claim`,
/// `Fleet::report`), and whether it has been counted lost since.
#[derive(Debug, Clone, Copy)]
struct PullSeen {
    at: tokio::time::Instant,
    lost: bool,
}

struct Spawner {
    connect: ConnectorFactory,
    events: broadcast::Sender<PastorEvent>,
}

/// The machines plus the one lock every dispatch pass takes. Shared by the
/// daemon (a `pastor task run` dispatches inline) and the scheduler (each
/// tick, and after a job run queues tasks), so two passes never read the same
/// capacity snapshot and both fill the last slot.
///
/// The set can change while the daemon runs (`apply_flock`, from a reload of
/// `flock.toml` or `pastor.toml`). Readers take a snapshot: `machines()` and
/// `get` hand out clones, never a reference into the set.
pub struct Fleet {
    members: RwLock<Vec<Member>>,
    /// The flock last passed to `apply_flock`. Differs from what `members`
    /// runs while a machine is shutting down.
    wanted: RwLock<Flock>,
    /// `[defaults]` as last applied (`set_config`): what a queued task's
    /// agent falls back to after its flock's (`resolve_agent`).
    defaults: RwLock<Defaults>,
    /// `[agents]` as last applied: whether a queued task's agent can take
    /// its tool lists (`Agents::launch_args`).
    agents: RwLock<Agents>,
    /// `[models]` as last applied: what a task's `model` names.
    models: RwLock<Models>,
    /// `[profiles]` as last applied: what a task's `profile` names.
    profiles: RwLock<crate::config::profile::Profiles>,
    /// `agents_change_fleet` as last applied: whether the head takes a
    /// fleet-changing request from an agent it started.
    agents_change_fleet: std::sync::atomic::AtomicBool,
    /// `max_orchestrators` as last applied.
    max_orchestrators: std::sync::atomic::AtomicU32,
    store: Arc<Store>,
    /// `None` for a fixed fleet (`Fleet::new`): tests and the daemon-less CLI.
    spawner: Option<Spawner>,
    /// Set for a headless serve (`Fleet::headless`): job tasks go to the
    /// head instead of this store, and the head checks their flock.
    forward: Option<HeadForward>,
    /// Pull machines by name, since each was added or last heard from.
    pull_seen: std::sync::Mutex<HashMap<String, PullSeen>>,
    dispatch_lock: tokio::sync::Mutex<()>,
}

impl Fleet {
    /// A fixed set of machines; `apply_flock` leaves it alone.
    pub fn new(machines: Vec<MachineHandle>, store: Arc<Store>) -> Fleet {
        let members = machines
            .into_iter()
            .map(|handle| Member {
                handle,
                spawned_from: None,
                shutting_down: false,
                pull: false,
            })
            .collect();
        Fleet {
            members: RwLock::new(members),
            wanted: RwLock::default(),
            defaults: RwLock::default(),
            agents: RwLock::default(),
            models: RwLock::default(),
            profiles: RwLock::default(),
            agents_change_fleet: Default::default(),
            max_orchestrators: std::sync::atomic::AtomicU32::new(1),
            store,
            spawner: None,
            forward: None,
            pull_seen: Default::default(),
            dispatch_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// A headless serve's fleet: no machines and no flock, and every job
    /// task it queues goes through `forward` to the head. `store` keeps only
    /// the jobs' state and seen keys.
    pub fn headless(store: Arc<Store>, forward: HeadForward) -> Fleet {
        Fleet {
            forward: Some(forward),
            ..Fleet::new(Vec::new(), store)
        }
    }

    /// A fixed fleet that knows `flock`'s flocks, for the daemon-less
    /// scheduler: nothing is dispatched from it, but the tasks it queues must
    /// land in the flocks flock.toml declares.
    pub fn with_flock(self, flock: Flock) -> Fleet {
        *self.wanted.write().unwrap() = flock;
        self
    }

    /// An empty fleet that `apply_flock` fills, spawning one actor per machine
    /// through `connect`.
    pub fn managed(
        store: Arc<Store>,
        events: broadcast::Sender<PastorEvent>,
        connect: ConnectorFactory,
    ) -> Fleet {
        Fleet {
            members: RwLock::new(Vec::new()),
            wanted: RwLock::default(),
            defaults: RwLock::default(),
            agents: RwLock::default(),
            models: RwLock::default(),
            profiles: RwLock::default(),
            agents_change_fleet: Default::default(),
            max_orchestrators: std::sync::atomic::AtomicU32::new(1),
            store,
            spawner: Some(Spawner { connect, events }),
            forward: None,
            pull_seen: Default::default(),
            dispatch_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Every machine in flock order, as of now.
    pub fn machines(&self) -> Vec<MachineHandle> {
        self.members
            .read()
            .unwrap()
            .iter()
            .map(|m| m.handle.clone())
            .collect()
    }

    /// Every machine's status with the flock it is in (see `flock_of`) and
    /// whether it is `shutting_down`, in flock order: what `machine list`
    /// and `machine.*` events report, and what a caller granting access by
    /// flock membership (`bridge::machine_flocks`) must check before trusting
    /// the flock it reports for a machine no longer in `wanted`.
    pub fn statuses(&self) -> Vec<crate::machine::MachineStatus> {
        let wanted = self.flock();
        self.members
            .read()
            .unwrap()
            .iter()
            .map(|m| {
                let flock = flock_of(&wanted, &m.handle.name);
                if m.pull {
                    self.count_pull(&m.handle);
                }
                let s = m.handle.snapshot();
                crate::machine::MachineStatus {
                    profile: self.own_profile(&flock, Some(&m.handle.name)),
                    flock: Some(flock),
                    flocks: seats(&wanted, &s),
                    shutting_down: m.shutting_down,
                    description: wanted
                        .get(&m.handle.name)
                        .and_then(|c| crate::config::clean_description(c.description.as_deref())),
                    ..s
                }
            })
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<MachineHandle> {
        self.members
            .read()
            .unwrap()
            .iter()
            .find(|m| m.handle.name == name)
            .map(|m| m.handle.clone())
    }

    /// The flock as last applied: what a reload falls back to when
    /// `flock.toml` does not load, and what it applies again to finish a
    /// swap a machine that was shutting down held up. Empty for a fixed
    /// fleet.
    pub fn flock(&self) -> Flock {
        self.wanted.read().unwrap().clone()
    }

    /// Take `[defaults]` and `[agents]` from `pastor.toml` as now loaded;
    /// the scheduler calls it at start, and a reload through `apply_config`.
    pub fn set_config(&self, config: &PastorConfig) {
        *self.defaults.write().unwrap() = config.defaults.clone();
        *self.agents.write().unwrap() = config.agents.clone();
        *self.models.write().unwrap() = config.models.clone();
        *self.profiles.write().unwrap() = config.profiles.clone();
        self.agents_change_fleet.store(
            config.agents_change_fleet,
            std::sync::atomic::Ordering::Relaxed,
        );
        self.max_orchestrators.store(
            config.max_orchestrators,
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// `max_orchestrators` in `pastor.toml` as last applied.
    pub fn max_orchestrators(&self) -> u32 {
        self.max_orchestrators
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// `[defaults]` as last applied.
    pub fn defaults(&self) -> Defaults {
        self.defaults.read().unwrap().clone()
    }

    /// Whether `pastor.toml` as last applied lets an agent pastor started
    /// change the fleet.
    pub fn agents_change_fleet(&self) -> bool {
        self.agents_change_fleet
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The agent a task in `flock` gets on `machine` (none before one is
    /// picked), given what its run or job asked for
    /// (`Defaults::resolve_agent_on`), from the flock and defaults as they
    /// stand now.
    pub fn resolve_agent(
        &self,
        ask: &AgentChoice,
        flock: &str,
        machine: Option<&str>,
    ) -> AgentPick {
        let wanted = self.wanted.read().unwrap();
        self.defaults.read().unwrap().resolve_agent_on(
            ask,
            machine.and_then(|m| wanted.get(m)),
            wanted.entry(flock),
        )
    }

    /// The profile a task in `flock` on `machine` runs under when it names
    /// none: the machine's, else the flock's, else `[defaults]`.
    pub fn own_profile(&self, flock: &str, machine: Option<&str>) -> Option<String> {
        self.resolve_agent(&AgentChoice::default(), flock, machine)
            .profile
            .map(|(name, _)| name)
    }

    /// `resolve_agent` written into `spec`, with the ask and where the agent
    /// and its args came from (`DispatchSpec::agent_source`), its model's
    /// args put in front (`Models::apply`) and its profile's lists added
    /// (`Profiles::apply`). `asked_by` names the ask: `task run` or `job
    /// <name>`. Refused when the model is not in `[models]` or not of the
    /// agent's kind, or the profile is unknown or `unrestricted` where the
    /// machine's own is not; `spec` is then only partly settled.
    fn settle(
        &self,
        spec: &mut crate::task::DispatchSpec,
        ask: &AgentChoice,
        flock: &str,
        machine: Option<&str>,
        asked_by: &str,
    ) -> Result<(), AgentRefusal> {
        let models = self.models.read().unwrap();
        let agents = self.agents.read().unwrap();
        let pick = {
            let wanted = self.wanted.read().unwrap();
            self.defaults.read().unwrap().resolve_agent_for(
                ask,
                machine.and_then(|m| wanted.get(m)),
                wanted.entry(flock),
                &models,
                &agents,
            )
        };
        pick.apply_to(spec);
        let label = |layer| layer_label(layer, asked_by, flock, machine);
        let mut agent_from = label(pick.agent_from);
        if pick.by_kind {
            agent_from.push_str(&format!(" agents.{}", agents.kind(&pick.agent)));
        }
        spec.agent_source = Some(Box::new(AgentSource {
            ask: ask.clone(),
            agent: agent_from,
            agent_args: pick.args_from.map(label),
            model: pick.model.as_ref().map(|(name, _)| name.clone()),
            model_from: pick.model.as_ref().map(|&(_, layer)| label(layer)),
            profile: pick.profile.as_ref().map(|(name, _)| name.clone()),
            profile_from: pick.profile.as_ref().map(|&(_, layer)| label(layer)),
        }));
        models.apply(&pick, &agents, spec)?;
        // What the machine's owner lets run there (the unrestricted rule).
        let own = self.own_profile(flock, machine);
        self.profiles
            .read()
            .unwrap()
            .apply(&pick, own.as_deref(), spec)
    }

    /// The level of a task being queued in `flock`, pinned to `pinned` if it
    /// is, and the label of the layer that set it
    /// (`Defaults::resolve_priority`), from the flock and defaults as they
    /// stand now. `asked_by` names the ask, as for `settle`.
    fn settle_priority(
        &self,
        ask: Option<Priority>,
        flock: &str,
        pinned: Option<&str>,
        asked_by: &str,
    ) -> (Priority, Option<String>) {
        let wanted = self.wanted.read().unwrap();
        let (priority, layer) = self.defaults.read().unwrap().resolve_priority(
            ask,
            pinned.and_then(|m| wanted.get(m)),
            wanted.entry(flock),
        );
        (
            priority,
            layer.map(|l| layer_label(l, asked_by, flock, pinned)),
        )
    }

    /// The label template of a task being queued in `flock`: its own
    /// (`--label`, a job's `label`), else the flock's, else `[defaults]`,
    /// as they stand now, with where it came from
    /// (`Defaults::resolve_label`). `asked_by` names the ask, as for
    /// `settle`. Dispatch renders it on the machine it picks.
    fn settle_label(&self, spec: &mut crate::task::DispatchSpec, flock: &str, asked_by: &str) {
        let wanted = self.wanted.read().unwrap();
        let picked = self
            .defaults
            .read()
            .unwrap()
            .resolve_label(spec.label.template.as_deref(), wanted.entry(flock));
        spec.label = crate::task::WorkspaceLabel {
            from: picked
                .as_ref()
                .map(|&(_, layer)| layer_label(layer, asked_by, flock, None)),
            template: picked.map(|(template, _)| template),
            ..Default::default()
        };
    }

    /// A task's `summary` setting as it is queued in `flock`
    /// (`Defaults::resolve_summary`), from the flock and defaults as they
    /// stand now.
    fn settle_summary(
        &self,
        ask: Option<crate::task::SummaryMode>,
        flock: &str,
    ) -> crate::task::SummaryMode {
        let wanted = self.wanted.read().unwrap();
        self.defaults
            .read()
            .unwrap()
            .resolve_summary(ask, wanted.entry(flock))
    }

    /// `settle` for a task being queued: on the machine it is pinned to,
    /// else with no machine yet, as dispatch settles it again on the one it
    /// picks. Refused when the agent cannot be started as resolved on any
    /// machine it may land on: a model `[models]` does not have, a tool list
    /// the agent has no flag for, or, on the machine it is pinned to or with
    /// an agent it asked for itself, a model of another kind. Checked here so
    /// the task is refused, not queued to fail at dispatch. An unpinned task
    /// whose model does not suit some machine's agent only skips that machine
    /// (`dispatch_queued`), and waits when none suits.
    fn settle_agent(
        &self,
        spec: &mut crate::task::DispatchSpec,
        ask: &AgentChoice,
        flock: &str,
        asked_by: &str,
    ) -> Result<(), AgentRefusal> {
        let pinned = spec.machine.clone();
        // Unpinned, with the agent left to the machine, its kind is known
        // only on the machine: a mismatch here is not the task's to fix. So
        // is the machine's own profile, which decides where `unrestricted`
        // may run.
        let per_machine = |e: &AgentRefusal| {
            pinned.is_none()
                && ((e.code == MODEL_KIND_MISMATCH && ask.agent.is_none())
                    || e.code == PROFILE_NOT_ALLOWED)
        };
        let mut landings = Vec::new();
        match self.settle(spec, ask, flock, pinned.as_deref(), asked_by) {
            Ok(()) => landings.push(spec.clone()),
            Err(e) if per_machine(&e) => {}
            Err(e) => return Err(e),
        }
        if pinned.is_none() {
            let wanted = self.flock();
            for m in wanted.machines.iter().filter(|m| wanted.in_flock(m, flock)) {
                let mut s = spec.clone();
                match self.settle(&mut s, ask, flock, Some(&m.name), asked_by) {
                    Ok(()) => landings.push(s),
                    Err(e) if per_machine(&e) => {}
                    Err(e) => return Err(e),
                }
            }
        }
        let agents = self.agents.read().unwrap();
        landings
            .iter()
            .try_for_each(|s| agents.launch_args(s).map(drop))
    }

    /// The store the fleet queues tasks in.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Is `name` held in the fleet only until its old actor ends?
    pub fn shutting_down(&self, name: &str) -> bool {
        self.members
            .read()
            .unwrap()
            .iter()
            .any(|m| m.handle.name == name && m.shutting_down)
    }

    /// Is `name` in the flock? For a managed fleet that is the flock last
    /// applied, so a removed machine held only until its old actor ends is
    /// not. A fixed fleet has no applied flock; its machines are the flock,
    /// plus those of the flock file it was given (`with_flock`), which is
    /// all the daemon-less scheduler has.
    pub fn in_flock(&self, name: &str) -> bool {
        let wanted = self.wanted.read().unwrap().get(name).is_some();
        if self.spawner.is_some() {
            wanted
        } else {
            wanted || self.get(name).is_some()
        }
    }

    /// The flock `job`'s tasks go to (`Flock::task_flock`), refusing a pin
    /// to a machine that is not in the flock: `task_flock` reads such a pin
    /// as none, and the task would wait in the default flock for a machine
    /// no dispatch can find. `run_job` checks this before the connector and
    /// `queue_job_task` again under the dispatch lock.
    pub fn job_task_flock(&self, job: &crate::config::job::Job) -> anyhow::Result<String> {
        // The flocks are the head's; it checks them when the task arrives.
        if self.forward.is_some() {
            return Ok(job.flock.clone().unwrap_or_default());
        }
        if let Some(m) = &job.spec.machine
            && !self.in_flock(m)
        {
            anyhow::bail!("machine {m} is not in the flock");
        }
        Ok(self
            .flock()
            .task_flock(job.flock.as_deref(), job.spec.machine.as_deref())?)
    }

    /// Swap the wanted flock under the dispatch lock, as `apply_flock` does
    /// for a managed fleet, without touching any actor.
    #[cfg(test)]
    pub async fn replace_flock(&self, flock: Flock) {
        let _pass = self.dispatch_lock.lock().await;
        *self.wanted.write().unwrap() = flock;
    }

    /// Hold the dispatch lock, as a dispatch pass does, for a test.
    #[cfg(test)]
    pub async fn hold_dispatch_lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.dispatch_lock.lock().await
    }

    /// Is any machine waiting for its old actor to end?
    pub fn any_shutting_down(&self) -> bool {
        self.members.read().unwrap().iter().any(|m| m.shutting_down)
    }

    /// Make the running set match `flock` and `settings`: spawn actors for
    /// new machines, stop the ones for machines that are gone, and replace
    /// the ones whose entry or timings changed. A machine whose entry is the
    /// same keeps its actor, connection and event stream.
    ///
    /// Takes the dispatch lock, so no pass is holding a handle that is being
    /// stopped. Stopping an actor touches nothing on its machine; tasks left
    /// there keep their last state. A replacement is spawned, or a removed
    /// machine dropped, only once its old actor has ended, so a removed
    /// machine writes no row afterwards and a retargeted one never has two
    /// actors on the same tasks. An old actor that does not end in time keeps
    /// its place, marked as shutting down and out of dispatch, and is listed
    /// in `FlockDiff::shutting_down`; calling this again (the scheduler does,
    /// on its next reload pass) waits for it again and finishes the swap.
    pub async fn apply_flock(&self, flock: &Flock, settings: &MachineSettings) -> FlockDiff {
        let Some(spawner) = &self.spawner else {
            return FlockDiff::default();
        };
        let _pass = self.dispatch_lock.lock().await;
        self.apply_flock_locked(spawner, flock, settings).await
    }

    /// A reload: `set_config` and `apply_flock` as one step under the
    /// dispatch lock, so no task is queued with the new `[defaults]` and
    /// `[agents]` and then dispatched to an actor that still runs the old
    /// ones, or the other way round. A fixed fleet takes the config only.
    pub async fn apply_config(&self, config: &PastorConfig, flock: &Flock) -> FlockDiff {
        let _pass = self.dispatch_lock.lock().await;
        self.set_config(config);
        match &self.spawner {
            Some(spawner) => {
                self.apply_flock_locked(spawner, flock, &machine_settings(config))
                    .await
            }
            None => FlockDiff::default(),
        }
    }

    /// `apply_flock`'s body; the caller holds the dispatch lock.
    async fn apply_flock_locked(
        &self,
        spawner: &Spawner,
        flock: &Flock,
        settings: &MachineSettings,
    ) -> FlockDiff {
        *self.wanted.write().unwrap() = flock.clone();
        let mut diff = FlockDiff::default();
        // A copy: readers keep seeing the old set until the new one is ready,
        // and the lock is not held across the waits below. The dispatch lock
        // keeps any other `apply_flock` out meanwhile.
        let mut old: Vec<Member> = self.members.read().unwrap().clone();
        enum Step {
            Keep(Member),
            Add,
            /// Replace this member once its actor has ended.
            Replace(Member),
        }
        let mut plan: Vec<Step> = Vec::new();
        for m in &flock.machines {
            let want = (actor_config(m), actor_settings(m, settings));
            plan.push(match old.iter().position(|o| o.handle.name == m.name) {
                Some(i) if !old[i].shutting_down && old[i].spawned_from.as_ref() == Some(&want) => {
                    Step::Keep(old.remove(i))
                }
                Some(i) => Step::Replace(old.remove(i)),
                None => Step::Add,
            });
        }
        let gone = old;
        // Published before any actor is aborted, under the lock readers take,
        // so a request that checks `shutting_down` from now on is refused
        // instead of queued on an actor that will never answer. One that got
        // past the check just before fails with `ActorStopped` once
        // `shutdown` starts.
        let stopping: Vec<String> = plan
            .iter()
            .filter_map(|s| match s {
                Step::Replace(o) => Some(o.handle.name.clone()),
                _ => None,
            })
            .chain(gone.iter().map(|o| o.handle.name.clone()))
            .collect();
        for m in self.members.write().unwrap().iter_mut() {
            if stopping.contains(&m.handle.name) {
                m.shutting_down = true;
            }
        }
        let mut members: Vec<Member> = Vec::new();
        for (step, m) in plan.into_iter().zip(&flock.machines) {
            members.push(match step {
                Step::Keep(kept) => kept,
                Step::Add => {
                    diff.added.push(m.name.clone());
                    self.spawn(spawner, m, settings)
                }
                Step::Replace(o) => match o.handle.shutdown().await {
                    ShutdownOutcome::Finished => {
                        diff.retargeted.push(m.name.clone());
                        self.spawn(spawner, m, settings)
                    }
                    ShutdownOutcome::StillRunning => {
                        diff.shutting_down.push(m.name.clone());
                        Self::hold(o)
                    }
                },
            });
        }
        for o in gone {
            match o.handle.shutdown().await {
                ShutdownOutcome::Finished => diff.removed.push(o.handle.name.clone()),
                ShutdownOutcome::StillRunning => {
                    diff.shutting_down.push(o.handle.name.clone());
                    members.push(Self::hold(o));
                }
            }
        }
        for name in &diff.shutting_down {
            tracing::warn!(
                machine = %name,
                "old actor has not stopped: machine kept out of dispatch and not \
                 replaced or removed yet; the next reload pass tries again"
            );
        }
        self.pull_seen
            .lock()
            .unwrap()
            .retain(|name, _| members.iter().any(|m| m.pull && &m.handle.name == name));
        *self.members.write().unwrap() = members;
        diff
    }

    /// Keep `m` in the fleet, out of dispatch, until its actor ends.
    fn hold(mut m: Member) -> Member {
        m.shutting_down = true;
        m.handle.status.write().unwrap().error =
            Some("shutting down: the old actor has not stopped yet".into());
        m
    }

    fn spawn(&self, spawner: &Spawner, m: &MachineConfig, settings: &MachineSettings) -> Member {
        let settings = &actor_settings(m, settings);
        if m.pull {
            // Heard from as of now: a head that starts gives the machine
            // `pull_lost_after` to claim before its tasks go stale.
            self.pull_seen.lock().unwrap().insert(
                m.name.clone(),
                PullSeen {
                    at: tokio::time::Instant::now(),
                    lost: false,
                },
            );
            return Member {
                handle: crate::machine::pull_machine(m.name.clone(), m.max_agents, m.tags.clone())
                    .with_slots(m.job_slots, m.burst),
                spawned_from: Some((actor_config(m), settings.clone())),
                shutting_down: false,
                pull: true,
            };
        }
        let handle = spawn_machine(
            m.name.clone(),
            m.max_agents,
            m.tags.clone(),
            (spawner.connect)(m),
            self.store.clone(),
            settings.clone(),
            spawner.events.clone(),
        )
        .with_slots(m.job_slots, m.burst);
        Member {
            handle,
            spawned_from: Some((actor_config(m), settings.clone())),
            shutting_down: false,
            pull: false,
        }
    }

    pub fn views(&self) -> Vec<MachineView> {
        let wanted = self.flock();
        self.members
            .read()
            .unwrap()
            .iter()
            .map(|m| {
                let s = m.handle.snapshot();
                MachineView {
                    name: m.handle.name.clone(),
                    max_agents: m.handle.max_agents,
                    job_slots: m.handle.job_slots,
                    burst: m.handle.burst,
                    tags: m.handle.tags.clone(),
                    live: s.live,
                    live_jobs: s.live_jobs,
                    // An aborted actor answers nothing, and a dispatch to
                    // it would wait for as long as it stays wedged. A pull
                    // machine takes its tasks itself (`claim`).
                    healthy: !m.pull && !m.shutting_down && s.channel.accepts_dispatch(),
                    flocks: seats(&wanted, &s),
                }
            })
            .collect()
    }

    /// Queue a one-off task (`pastor task run`) in the flock it asks for
    /// (see `Flock::task_flock`), first checking that a machine it is pinned
    /// to is in the flock file. All under the dispatch lock, so an
    /// `apply_flock` cannot drop or move the machine between the check and
    /// the insert. The check reads the wanted flock, not the running set: a
    /// removed machine held only until its old actor ends is not in it, and
    /// would never take the task. A fixed fleet has no wanted flock, so its
    /// machines are the flock, all in the default one.
    /// `ask` is what the run's flags said about the agent; the flock and
    /// `[defaults]` fill in the rest. `None`, from a client that predates
    /// it, keeps the agent `spec` already carries. `priority` is
    /// `--priority`; without it the pinned machine, the flock or
    /// `[defaults]` set the level.
    pub async fn queue_run(
        &self,
        prompt: String,
        spec: crate::task::DispatchSpec,
        flock: Option<&str>,
        ask: Option<&AgentChoice>,
        priority: Option<Priority>,
    ) -> Result<Task, QueueError> {
        self.queue_run_as(
            prompt,
            spec,
            flock,
            ask,
            priority,
            TaskRole::Agent,
            None,
            false,
            None,
        )
        .await
    }

    /// `queue_run`, for a task of `role` (`task run --role`). `summary` is
    /// `task run --summary`; without it the flock or `[defaults]` decide.
    #[allow(clippy::too_many_arguments)]
    pub async fn queue_run_as(
        &self,
        prompt: String,
        mut spec: crate::task::DispatchSpec,
        flock: Option<&str>,
        ask: Option<&AgentChoice>,
        priority: Option<Priority>,
        role: TaskRole,
        description: Option<String>,
        preempt: bool,
        summary: Option<crate::task::SummaryMode>,
    ) -> Result<Task, QueueError> {
        let _pass = self.dispatch_lock.lock().await;
        if let Some(m) = &spec.machine
            && !self.in_flock(m)
        {
            return Err(QueueError::UnknownMachine(m.clone()));
        }
        let flock = self
            .flock()
            .task_flock(flock, spec.machine.as_deref())
            .map_err(QueueError::Flock)?;
        if let Some(ask) = ask {
            self.settle_agent(&mut spec, ask, &flock, "task run")
                .map_err(QueueError::Agent)?;
        }
        self.settle_label(&mut spec, &flock, "task run");
        let (priority, from) =
            self.settle_priority(priority, &flock, spec.machine.as_deref(), "task run");
        spec.summary = self.settle_summary(summary, &flock);
        // Checked against the level it settles at: a machine or flock may
        // make it critical without `--priority`.
        if preempt && priority != Priority::Critical {
            return Err(QueueError::Agent(crate::config::AgentRefusal {
                code: crate::task::PREEMPT_NEEDS_CRITICAL,
                message: format!(
                    "--preempt needs critical: only a critical task may pause another, and this one is {priority}"
                ),
            }));
        }
        self.store
            .insert_task_preempting(
                NewTask {
                    description,
                    job: "run".into(),
                    item: serde_json::Value::Null,
                    prompt,
                    spec,
                    flock,
                },
                priority,
                from.as_deref(),
                role,
                preempt,
            )
            .map_err(QueueError::Store)
    }

    /// Queue one task of a scheduled job's run for `item`, in the job's flock
    /// (see `job_task_flock`) as the wanted flock stands now. Under the
    /// dispatch lock, like `queue_run`: a `flock remove`, or a reload that
    /// drops the machine the job is pinned to, either sees the task queued,
    /// or goes first and this fails, so the run records the item's error and
    /// holds its cursor rather than queue a task no dispatch can place.
    /// The task's agent is the job's own, else that flock's, else
    /// `[defaults]` (`resolve_agent`).
    pub async fn queue_job_task(
        &self,
        job: &crate::config::job::Job,
        item: &serde_json::Value,
        render: impl FnOnce(i64) -> Result<(String, crate::task::DispatchSpec), String>,
    ) -> anyhow::Result<Task> {
        if self.forward.is_some() {
            anyhow::bail!("a headless serve queues nothing here; its runs submit to the head");
        }
        let _pass = self.dispatch_lock.lock().await;
        let flock = self.job_task_flock(job)?;
        let mut settled = job.spec.clone();
        let ask = AgentChoice {
            model: job.model_for(item).map_err(anyhow::Error::msg)?,
            ..job.agent.clone()
        };
        let asked_by = format!("job {}", job.name);
        self.settle_agent(&mut settled, &ask, &flock, &asked_by)
            .map_err(|e| anyhow::Error::msg(e.message))?;
        self.settle_label(&mut settled, &flock, &asked_by);
        let (priority, from) = self.settle_priority(
            job.priority_for(item).map_err(anyhow::Error::msg)?,
            &flock,
            job.spec.machine.as_deref(),
            &asked_by,
        );
        let level = (priority, from.as_deref());
        // A job's tasks keep `preempt` only where they settle at critical:
        // its level may be a template, and an item below critical just
        // queues as it would without it.
        let preempt = job.preempt && priority == Priority::Critical;
        let description = job.task_description_for(item);
        let summary = self.settle_summary(job.summary, &flock);
        self.store.insert_job_task_at(
            &job.name,
            &flock,
            item,
            level,
            preempt,
            description.as_deref(),
            |id| {
                let (prompt, mut spec) = render(id)?;
                spec.agent = settled.agent;
                spec.agent_args = settled.agent_args;
                spec.allow = settled.allow;
                spec.deny = settled.deny;
                spec.agent_source = settled.agent_source;
                spec.label = settled.label;
                spec.summary = summary;
                Ok((prompt, spec))
            },
        )
    }

    /// Whether job runs send their items to a head (`Fleet::headless`)
    /// instead of queueing them here.
    pub fn submits_to_head(&self) -> bool {
        self.forward.is_some()
    }

    /// A headless serve's job run hands the head what it found, in one
    /// `JobSubmit`: the head keeps the `seen` keys, renders and queues each
    /// item as that job's task, and dispatches them. The keys it queued or
    /// had seen are marked seen here too, since this store is the one the
    /// job's next run checks. An unreachable head, or an error reply such as
    /// `job_name_taken`, is an `Err` with its code, and nothing is kept.
    pub async fn submit_to_head(
        &self,
        job: &crate::config::job::Job,
        items: Vec<serde_json::Value>,
    ) -> anyhow::Result<crate::scheduler::Submitted> {
        let Some(forward) = &self.forward else {
            anyhow::bail!("this pastor serve is the head; it queues its jobs' items itself");
        };
        // A named model rides in `dispatch`, which only a head of
        // `MODEL_PROTOCOL` or later reads; an older one would drop it and
        // start the agent on its default model without a word.
        if job.agent.model.is_some() {
            match forward(IpcRequest::Ping).await? {
                IpcResponse::Pong {
                    version, protocol, ..
                } => check_protocol(&version, protocol, MODEL_PROTOCOL, "a job naming a model")?,
                other => anyhow::bail!("the head answered a ping with {other:?}"),
            }
        }
        // A permission profile rides in `dispatch` the same way; a head
        // before `PROFILE_PROTOCOL` would drop it (serde skips the unknown
        // field) and start the agent unenforced instead of refusing.
        if job.agent.profile.is_some() {
            match forward(IpcRequest::Ping).await? {
                IpcResponse::Pong {
                    version, protocol, ..
                } => check_protocol(
                    &version,
                    protocol,
                    crate::ipc::PROFILE_PROTOCOL,
                    "a job naming a permission profile",
                )?,
                other => anyhow::bail!("the head answered a ping with {other:?}"),
            }
        }
        // A description template rides in `dispatch` the same way; the
        // head's `DispatchTable` refuses an unknown field, so an older head
        // would answer an opaque `invalid_dispatch` instead of this clear
        // refusal.
        if job.task_description.is_some() {
            match forward(IpcRequest::Ping).await? {
                IpcResponse::Pong {
                    version, protocol, ..
                } => check_protocol(
                    &version,
                    protocol,
                    crate::ipc::DESCRIPTION_PROTOCOL,
                    "a job naming a description",
                )?,
                other => anyhow::bail!("the head answered a ping with {other:?}"),
            }
        }
        // `preempt` rides in `dispatch` the same way; the head's
        // `DispatchTable` refuses an unknown field, so a head before
        // `PREEMPT_PROTOCOL` would answer an opaque `invalid_dispatch`
        // instead of this clear refusal.
        if job.preempt {
            match forward(IpcRequest::Ping).await? {
                IpcResponse::Pong {
                    version, protocol, ..
                } => check_protocol(
                    &version,
                    protocol,
                    crate::ipc::PREEMPT_PROTOCOL,
                    "a job with preempt",
                )?,
                other => anyhow::bail!("the head answered a ping with {other:?}"),
            }
        }
        // A workspace label template rides in `dispatch` the same way; a
        // head before `LABEL_PROTOCOL` would drop it (serde skips the
        // unknown field) and name the workspace by its default instead of
        // refusing.
        if job.spec.label.template.is_some() {
            match forward(IpcRequest::Ping).await? {
                IpcResponse::Pong {
                    version, protocol, ..
                } => check_protocol(
                    &version,
                    protocol,
                    crate::ipc::LABEL_PROTOCOL,
                    "a job naming a workspace label",
                )?,
                other => anyhow::bail!("the head answered a ping with {other:?}"),
            }
        }
        // `summary` rides in `dispatch` the same way; a head before
        // `SUMMARY_MODE_PROTOCOL` would answer an opaque `invalid_dispatch`.
        if job.summary.is_some() {
            match forward(IpcRequest::Ping).await? {
                IpcResponse::Pong {
                    version, protocol, ..
                } => check_protocol(
                    &version,
                    protocol,
                    crate::ipc::SUMMARY_MODE_PROTOCOL,
                    "a job with summary",
                )?,
                other => anyhow::bail!("the head answered a ping with {other:?}"),
            }
        }
        let reply = forward(IpcRequest::JobSubmit {
            job: job.name.clone(),
            dispatch: job.dispatch.clone(),
            prompt: job.prompt.clone(),
            items,
        })
        .await?;
        let (tasks, skipped, refused) = match reply {
            IpcResponse::JobSubmitted {
                tasks,
                skipped,
                refused,
            } => (tasks, skipped, refused),
            IpcResponse::Error { code, message } => {
                return Err(crate::cli::CliError::err(&code, message));
            }
            other => anyhow::bail!("the head answered a job submit with {other:?}"),
        };
        for t in &tasks {
            if let Some(key) = t.item.get("key").and_then(serde_json::Value::as_str) {
                self.store.mark_seen(&job.name, key, Some(t.id))?;
            }
        }
        for key in &skipped {
            self.store.mark_seen(&job.name, key, None)?;
        }
        Ok(crate::scheduler::Submitted {
            tasks,
            skipped,
            refused,
        })
    }

    /// `flock remove` with a head running: refuse while queued tasks name
    /// the flock (`FlockDoc::remove_flock`), else take it out of `file` and
    /// of the wanted flock. All under the dispatch lock that `queue_run`
    /// takes, so a `task run --flock` either queued first, and is seen here,
    /// or comes after and finds the flock gone; checking in the CLI and then
    /// editing let one queue in between and wait forever. The wanted flock
    /// changes here, not on the reload that follows, since `queue_run` reads
    /// it. The flock has no machines (or the edit is refused), so no actor
    /// needs to change. An `EditError` comes back inside the error.
    pub async fn remove_flock(&self, file: &std::path::Path, name: &str) -> anyhow::Result<()> {
        let _pass = self.dispatch_lock.lock().await;
        let queued: Vec<String> = self
            .store
            .queued_tasks()?
            .iter()
            .filter(|t| t.flock.as_deref() == Some(name))
            .map(|t| t.display_id())
            .collect();
        let mut doc = FlockDoc::open(file)?;
        doc.remove_flock(name, &queued)?;
        doc.save(file)?;
        self.wanted
            .write()
            .unwrap()
            .flocks
            .retain(|f| f.name != name);
        Ok(())
    }

    /// One `edit` of `file` by the head, and the wanted flock it leaves, as
    /// one step under the dispatch lock: a `task run` sees the flock from
    /// before the edit or from after it, never the file edited and the
    /// wanted flock still old. The reload that follows (the scheduler's, which
    /// takes this lock itself, so it cannot run inside this step) then only
    /// starts and stops actors. A file that does not load after the edit,
    /// or names a model `[models]` lacks, leaves the wanted flock as it was;
    /// that reload logs it and falls back to this flock, so it must still
    /// be the last valid one.
    pub async fn edit_flock_file<T>(
        &self,
        file: &std::path::Path,
        edit: impl FnOnce(&std::path::Path) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let _pass = self.dispatch_lock.lock().await;
        let done = edit(file)?;
        // A fixed fleet has no applied flock, which a reload leaves alone too.
        if self.spawner.is_some()
            && let Ok(flock) = Flock::load_existing(file)
            && flock
                .check_config(
                    &self.models.read().unwrap(),
                    &self.agents.read().unwrap(),
                    &self.profiles.read().unwrap(),
                )
                .is_ok()
        {
            *self.wanted.write().unwrap() = flock;
        }
        Ok(done)
    }

    /// `queue_task` for a retry of task `id` (`Store::insert_retry`). The
    /// copy keeps the original's pin and flock, so both get the same checks
    /// under the same lock: a failed task outlives `flock remove`, which only
    /// counts queued ones, and a copy in a removed flock would wait forever.
    /// A row that is missing or not retryable is left for `insert_retry` to
    /// name.
    pub async fn queue_retry(
        &self,
        id: i64,
        place: Option<&crate::task::Place>,
    ) -> Result<Task, QueueError<RetryError>> {
        let _pass = self.dispatch_lock.lock().await;
        if let Ok(Some(t)) = self.store.get_task(id)
            && t.state.is_retryable()
        {
            if let Some(m) = &t.spec.machine
                && !self.in_flock(m)
            {
                return Err(QueueError::UnknownMachine(m.clone()));
            }
            if let Some(f) = &t.flock
                && !self.flock().has_flock(f)
            {
                return Err(QueueError::Flock(TaskFlockError::UnknownFlock(f.clone())));
            }
            // Dispatch settles the copy's agent again from what it asked
            // for, so it is checked as a new task would be: a model since
            // dropped from `[models]` refuses it here. A copy of a task from
            // before that keeps its agent and tool lists as resolved, and
            // only an `[agents]` edit since can make them unstartable.
            match &t.spec.agent_source {
                Some(source) => {
                    let flock = t
                        .flock
                        .clone()
                        .unwrap_or_else(|| self.flock().default_flock().to_string());
                    let mut spec = t.spec.clone();
                    self.settle_agent(&mut spec, &source.ask, &flock, &asked_by(&t))
                        .map_err(QueueError::Agent)?;
                }
                None => {
                    self.agents
                        .read()
                        .unwrap()
                        .launch_args(&t.spec)
                        .map_err(QueueError::Agent)?;
                }
            }
        }
        self.store
            .insert_retry_placed(id, place)
            .map_err(QueueError::Store)
    }

    /// Set a queued task's priority (`Store::set_priority`), under the
    /// dispatch lock like `queue_run`: otherwise a dispatch pass could read
    /// the old priority, this update land on the still-queued row, and the
    /// pass then claim it by the stale ordering, reporting the change
    /// applied while it had no effect on that dispatch.
    pub async fn set_priority(
        &self,
        id: i64,
        priority: Priority,
        from: &str,
    ) -> Result<Task, PriorityError> {
        self.set_priority_preempting(id, priority, from, false)
            .await
    }

    /// `set_priority`, setting the task's `preempt` (`task priority
    /// --preempt`); the caller has checked the level is critical.
    pub async fn set_priority_preempting(
        &self,
        id: i64,
        priority: Priority,
        from: &str,
        preempt: bool,
    ) -> Result<Task, PriorityError> {
        let _pass = self.dispatch_lock.lock().await;
        self.store
            .set_priority_preempting(id, priority, from, preempt)
    }

    /// Move a queued task (`Store::move_queued`), under the dispatch lock
    /// for the reason `set_priority` is: a pass must not take the queue in
    /// the old order after the move has answered.
    pub async fn move_queued(&self, id: i64, to: QueueSpot) -> Result<Moved, MoveError> {
        let _pass = self.dispatch_lock.lock().await;
        self.store.move_queued(id, to)
    }

    /// The queue as `pastor queue` shows it: dispatch order, each task with
    /// why it waits on the machines as they are now, then filtered.
    pub fn queue(
        &self,
        flock: Option<&str>,
        machine: Option<&str>,
    ) -> anyhow::Result<Vec<QueueEntry>> {
        let queued = self.store.queued_tasks()?;
        let wanted = self.flock();
        // Same model/agent compatibility check `dispatch_queued` applies: a
        // machine whose agent cannot run the task's model does not take it.
        let accepts = |task: &Task, machine: &str| {
            let Some(source) = task.spec.agent_source.as_ref() else {
                return true;
            };
            let target = task.flock.as_deref().unwrap_or(wanted.default_flock());
            let mut spec = task.spec.clone();
            self.settle(
                &mut spec,
                &source.ask,
                target,
                Some(machine),
                &asked_by(task),
            )
            .is_ok()
        };
        let mut entries =
            crate::queue::entries(queued, &self.views(), wanted.default_flock(), &accepts);
        entries.retain(|e| e.matches(flock, machine));
        Ok(entries)
    }

    /// `task`'s spec with its agent settled for `machine` in `flock`: a
    /// machine whose agent cannot run the task's model does not take it. A
    /// task from before `agent_source` keeps the agent it was queued with
    /// (`None`). A profile it inherited (from a machine or flock, not its
    /// own ask) is pinned onto the ask here, so a re-settle that can no
    /// longer resolve it refuses instead of quietly dropping it.
    fn settled_on(
        &self,
        task: &Task,
        flock: &str,
        machine: &str,
    ) -> Option<Result<crate::task::DispatchSpec, AgentRefusal>> {
        let source = task.spec.agent_source.as_ref()?;
        let mut ask = source.ask.clone();
        if ask.profile.is_none() {
            ask.profile = source.profile.clone();
        }
        let mut spec = task.spec.clone();
        let r = self.settle(&mut spec, &ask, flock, Some(machine), &asked_by(task));
        Some(r.map(|()| spec))
    }

    /// Is `name` a pull machine (`MachineConfig::pull`) of the fleet?
    pub fn is_pull(&self, name: &str) -> bool {
        self.members
            .read()
            .unwrap()
            .iter()
            .any(|m| m.pull && m.handle.name == name)
    }

    /// The handle of pull machine `name`, or why a claim or report from it
    /// is refused.
    fn pull_handle(&self, name: &str) -> anyhow::Result<MachineHandle> {
        let members = self.members.read().unwrap();
        match members.iter().find(|m| m.handle.name == name) {
            Some(m) if m.pull => Ok(m.handle.clone()),
            Some(_) => Err(crate::cli::CliError::err(
                "not_pull_machine",
                format!("machine {name} is not a pull machine; the head reaches it itself"),
            )),
            None => Err(crate::cli::CliError::err(
                "unknown_machine",
                format!("machine {name} is not in the flock"),
            )),
        }
    }

    /// A pull machine's live counts, from the store: no actor keeps them.
    fn count_pull(&self, handle: &MachineHandle) {
        match self.store.tasks_on_machine(&handle.name) {
            Ok(tasks) => crate::machine::count_live(&mut handle.status.write().unwrap(), &tasks),
            Err(err) => {
                tracing::error!(machine = %handle.name, %err, "count a pull machine's tasks")
            }
        }
    }

    /// Emit `kind` about `machine` and `task`, as an actor would: a
    /// `task.done` or `task.failed` ends the task's round. A fixed fleet has
    /// nowhere to send it.
    fn emit(
        &self,
        kind: &str,
        machine: &str,
        task: Option<&Task>,
        detail: Option<serde_json::Value>,
    ) {
        let Some(spawner) = &self.spawner else {
            return;
        };
        let summary = task
            .filter(|_| matches!(kind, "task.done" | "task.failed"))
            .and_then(|t| match self.store.end_round(t.id, None) {
                Ok(summary) => Some(summary),
                Err(err) => {
                    tracing::error!(machine, %err, id = t.id, "save the task's summary");
                    None
                }
            });
        tracing::info!(machine, kind, task = ?task.map(|t| t.id), "event");
        let _ = spawner.events.send(PastorEvent {
            kind: kind.into(),
            task_id: task.map(|t| t.id),
            machine: Some(machine.into()),
            job: task.map(|t| t.job.clone()),
            detail,
            summary,
        });
    }

    /// Pull machine `handle` was heard from now: it is connected, and one
    /// counted lost is announced back.
    fn heard(&self, handle: &MachineHandle) {
        let was_lost = {
            let mut seen = self.pull_seen.lock().unwrap();
            let e = seen.entry(handle.name.clone()).or_insert(PullSeen {
                at: tokio::time::Instant::now(),
                lost: false,
            });
            e.at = tokio::time::Instant::now();
            std::mem::replace(&mut e.lost, false)
        };
        {
            let mut s = handle.status.write().unwrap();
            s.channel = crate::machine::ChannelState::Connected;
            s.error = None;
        }
        if was_lost {
            self.emit("machine.connected", &handle.name, None, None);
        }
    }

    /// `TaskClaim`: at most `free_slots` queued tasks for pull machine
    /// `machine` to start, each `starting` there before this returns. Those
    /// pinned to it first, then, with `flock_work`, any a dispatch pass
    /// would place there now; both only while the machine and the task's
    /// flock have room, as for any machine. Under the dispatch lock, so a
    /// pass and a claim never take the same slot.
    pub async fn claim(
        &self,
        machine: &str,
        free_slots: u32,
        flock_work: bool,
    ) -> anyhow::Result<Vec<Task>> {
        let _pass = self.dispatch_lock.lock().await;
        let handle = self.pull_handle(machine)?;
        self.heard(&handle);
        self.count_pull(&handle);
        let flock = self.flock();
        let s = handle.snapshot();
        let mut view = MachineView {
            name: machine.to_string(),
            max_agents: handle.max_agents,
            job_slots: handle.job_slots,
            burst: handle.burst,
            tags: handle.tags.clone(),
            live: s.live,
            live_jobs: s.live_jobs,
            healthy: true,
            flocks: seats(&flock, &s),
        };
        let queued = self.store.queued_tasks()?;
        let pinned = queued
            .iter()
            .filter(|t| t.spec.machine.as_deref() == Some(machine));
        let loose = queued
            .iter()
            .filter(|t| flock_work && t.spec.machine.is_none());
        let mut claimed = Vec::new();
        for task in pinned.chain(loose) {
            if claimed.len() >= free_slots as usize {
                break;
            }
            if task.state != TaskState::Queued {
                continue;
            }
            let target = task.flock.as_deref().unwrap_or(flock.default_flock());
            let settled = self.settled_on(task, target, machine);
            let claim = Claim::of(task);
            let accepts = |_: &str| settled.as_ref().is_none_or(|r| r.is_ok());
            let views = std::slice::from_ref(&view);
            if pick_machine_where(views, target, &task.spec, claim, &accepts).is_none() {
                continue;
            }
            let mut on = task.clone();
            if let Some(Ok(spec)) = settled {
                on.spec = spec;
            }
            if on
                .error
                .as_deref()
                .is_some_and(|e| e.starts_with(WAITING_FOR_MODEL))
            {
                on.error = None;
            }
            if (on.spec != task.spec || on.error != task.error)
                && let Err(err) = self.store.update_task(&mut on)
            {
                tracing::warn!(task = %task.display_id(), machine, %err, "settle agent");
                continue;
            }
            let Some(t) = self.store.claim_task(task.id, machine)? else {
                continue;
            };
            tracing::info!(task = %t.display_id(), machine, "claimed by a pull machine");
            view.take(target, claim);
            claimed.push(t);
        }
        self.count_pull(&handle);
        Ok(claimed)
    }

    /// `TaskReport`: what pull machine `machine` saw become of task `id`,
    /// written on the row with the event its own actor would have emitted.
    /// A closed or failed row stays as it is, a stale one stays stale while
    /// its agent works (as `task::next_state` keeps it), and one `task done`
    /// ended stays done until its pane closes; each answers the row as it
    /// is.
    pub async fn report(
        &self,
        machine: &str,
        id: i64,
        state: TaskState,
        pane: Option<String>,
        detail: Option<String>,
    ) -> anyhow::Result<Task> {
        let handle = self.pull_handle(machine)?;
        self.heard(&handle);
        if matches!(state, TaskState::Queued | TaskState::Paused) {
            return Err(crate::cli::CliError::err(
                "invalid_report",
                format!("a pull machine cannot report a task {state}"),
            ));
        }
        // Twice at most: once more on the fresh row after a write that lost
        // a race (a `task close` on the head, say).
        for _ in 0..2 {
            let mut task = self.store.get_task(id)?.ok_or_else(|| {
                crate::cli::CliError::err("task_not_found", format!("t-{id} not found"))
            })?;
            if task.machine.as_deref() != Some(machine) {
                return Err(crate::cli::CliError::err(
                    "not_on_machine",
                    format!("{} is not on {machine}", task.display_id()),
                ));
            }
            let from = task.state;
            let working = matches!(
                state,
                TaskState::Starting | TaskState::Running | TaskState::Blocked
            );
            // Reports carry no sequence and may arrive out of order, so a
            // failed row takes no later report, and an ended one only its
            // pane closing, which `next_state` also lets through.
            let kept = matches!(from, TaskState::Closed | TaskState::Failed)
                || from == TaskState::Done && task.ended && state != TaskState::Closed
                || from == TaskState::Stale && working;
            if kept
                || from == state
                    && task.error == detail
                    && pane
                        .as_ref()
                        .is_none_or(|p| task.pane_id.as_ref() == Some(p))
            {
                return Ok(task);
            }
            task.state = state;
            if pane.is_some() {
                task.pane_id = pane.clone();
            }
            task.error = detail.clone();
            let now = chrono::Utc::now();
            if matches!(state, TaskState::Running | TaskState::Blocked) && task.started_at.is_none()
            {
                task.started_at = Some(now);
            }
            task.finished_at = match state {
                TaskState::Closed => task.finished_at.or(Some(now)),
                TaskState::Done | TaskState::Failed if from != state => Some(now),
                TaskState::Done | TaskState::Failed => task.finished_at,
                _ => None,
            };
            match self.store.update_task(&mut task) {
                Ok(()) => {}
                Err(err) if err.downcast_ref::<crate::store::Conflict>().is_some() => continue,
                Err(err) => return Err(err),
            }
            if from != state {
                let question = (state == TaskState::Blocked)
                    .then(|| detail.as_deref()?.strip_prefix("agent asked: "))
                    .flatten()
                    .map(|q| serde_json::json!({ "question": q }));
                self.emit(&format!("task.{state}"), machine, Some(&task), question);
            }
            self.count_pull(&handle);
            return Ok(task);
        }
        Err(crate::cli::CliError::err(
            "store_error",
            format!("t-{id} kept changing under the report; try again"),
        ))
    }

    /// `task done` for a task on a pull machine, which has no actor here:
    /// the row is ended as `Actor::end_task` ends it, `done` and `ended`, so
    /// a report of the agent's last turn does not take it back to running.
    /// A required summary is held to the same rules as there (`by`).
    pub fn end_pull(
        &self,
        task: Task,
        summary: Option<String>,
        by: crate::machine::EndBy,
    ) -> anyhow::Result<Task> {
        use crate::machine::EndBy;
        let machine = task.machine.clone().unwrap_or_default();
        let summary = summary.filter(|s| !s.trim().is_empty());
        let required = task.spec.summary == crate::task::SummaryMode::Require;
        if required && summary.is_none() && by == EndBy::Agent && !task.ended {
            return Err(crate::cli::CliError::err(
                crate::task::SUMMARY_REQUIRED,
                format!(
                    "{} needs a summary: pastor task done --summary-file - <<'EOF', then a first line done, partial, blocked or nothing to do, up to five short lines, and EOF",
                    task.display_id()
                ),
            ));
        }
        let by_hand = required && summary.is_none() && by == EndBy::Hand;
        if task.ended {
            if let Some(summary) = &summary {
                self.store.replace_last_summary(task.id, summary)?;
            }
            return Ok(task);
        }
        let was_done = task.state == TaskState::Done;
        let mut t = task;
        if !was_done {
            t.state = TaskState::Done;
            t.finished_at = Some(chrono::Utc::now());
            t.activity_seen = false;
            t.prompt_pending = false;
            t.error = None;
        }
        t.ended = true;
        self.store.update_task(&mut t)?;
        if was_done {
            if let Some(summary) = &summary {
                self.store.replace_last_summary(t.id, summary)?;
            }
        } else if let Some(spawner) = &self.spawner {
            let round = if by_hand {
                self.store.end_round_by_hand(t.id)
            } else {
                self.store.end_round(t.id, summary.as_deref())
            };
            let round = match round {
                Ok(round) => Some(round),
                Err(err) => {
                    tracing::error!(%machine, %err, id = t.id, "save the task's summary");
                    None
                }
            };
            let _ = spawner.events.send(PastorEvent {
                kind: "task.done".into(),
                task_id: Some(t.id),
                machine: Some(machine),
                job: Some(t.job.clone()),
                detail: None,
                summary: round,
            });
        }
        Ok(t)
    }

    /// Count each pull machine not heard from for `after` lost, once per
    /// silence: `machine.lost`, and its starting and running tasks go
    /// stale, as a task does whose agent pastor lost sight of. Tasks pinned
    /// to it stay queued. Answers the machines it counted lost now.
    pub async fn check_pull_lost(&self, after: Duration) -> Vec<String> {
        let _pass = self.dispatch_lock.lock().await;
        let now = tokio::time::Instant::now();
        let lost: Vec<String> = self
            .pull_seen
            .lock()
            .unwrap()
            .iter_mut()
            .filter(|(_, seen)| !seen.lost && now.duration_since(seen.at) >= after)
            .map(|(name, seen)| {
                seen.lost = true;
                name.clone()
            })
            .collect();
        for name in &lost {
            let Ok(handle) = self.pull_handle(name) else {
                continue;
            };
            let why = format!(
                "pull machine {name} has not claimed or reported for {}s",
                after.as_secs()
            );
            {
                let mut s = handle.status.write().unwrap();
                s.channel = crate::machine::ChannelState::Reconnecting;
                s.error = Some(why.clone());
            }
            tracing::warn!(machine = %name, "{why}; counted lost");
            self.emit("machine.lost", name, None, None);
            let tasks = match self.store.tasks_on_machine(name) {
                Ok(tasks) => tasks,
                Err(err) => {
                    tracing::error!(machine = %name, %err, "list a lost pull machine's tasks");
                    continue;
                }
            };
            for mut t in tasks {
                if !matches!(t.state, TaskState::Starting | TaskState::Running) {
                    continue;
                }
                t.state = TaskState::Stale;
                t.error = Some(why.clone());
                match self.store.update_task(&mut t) {
                    Ok(()) => self.emit("task.stale", name, Some(&t), None),
                    Err(err) => {
                        tracing::warn!(task = %t.display_id(), %err, "mark stale")
                    }
                }
            }
            self.count_pull(&handle);
        }
        lost
    }

    /// Try to place every queued task, oldest first. Serialised: a pass sees the
    /// live counts the previous pass left behind, because a machine actor
    /// refreshes its count before it answers a dispatch (see
    /// `Actor::handle_command`) and no two passes run at once. The claim inside
    /// the actor (`Store::claim_task`) is the second line of defence: it makes a
    /// double dispatch of one task impossible even if this lock were bypassed.
    pub async fn dispatch_queued(&self) {
        let _pass = self.dispatch_lock.lock().await;
        let queued = match self.store.queued_tasks() {
            Ok(q) => q,
            Err(err) => {
                tracing::error!(%err, "list queued");
                return;
            }
        };
        let flock = self.flock();
        for task in queued {
            if task.state == TaskState::Paused {
                self.resume_paused(&task).await;
                continue;
            }
            let target = task.flock.as_deref().unwrap_or(flock.default_flock());
            let mut views = self.views();
            // The agent can depend on the machine: a machine whose agent
            // cannot run the task's model does not take it. A task from
            // before `agent_source` keeps the agent it was queued with.
            // A profile it inherited (from a machine or flock, not its own
            // ask) is pinned onto the ask here, so a re-settle that can no
            // longer resolve it refuses instead of quietly dropping it.
            let settled_on = |machine: &str| self.settled_on(&task, target, machine);
            let claim = Claim::of(&task);
            let accepts = |m: &str| settled_on(m).is_none_or(|r| r.is_ok());
            let mut picked = pick_machine_where(&views, target, &task.spec, claim, &accepts);
            // A critical task with `preempt` that finds no room pauses the
            // newest low Claude task on a machine it would fit once that one
            // is gone, then takes the slot in the same pass.
            if picked.is_none()
                && task.pause.preempt
                && task.priority == Priority::Critical
                && let Some((machine, victim)) =
                    self.pausable_for(&views, target, &task, claim, &accepts)
                && let Some(handle) = self.get(&machine)
            {
                match handle.pause(victim, task.id).await {
                    Ok(_) => {
                        tracing::info!(task = %task.display_id(), paused = %Task::agent_name_for(victim), machine = %machine, "paused a low task");
                        views = self.views();
                        picked = pick_machine_where(&views, target, &task.spec, claim, &accepts);
                    }
                    Err(err) => {
                        tracing::warn!(task = %task.display_id(), victim = %Task::agent_name_for(victim), machine = %machine, %err, "pause failed")
                    }
                }
            }
            let Some(name) = picked else {
                // Say why: no machine of the flock has an agent for its
                // model, or one would take it but for its model.
                let none_has = || {
                    let kind = self
                        .models
                        .read()
                        .unwrap()
                        .get(task.model()?)
                        .ok()?
                        .kind
                        .clone();
                    let mut members = flock
                        .machines
                        .iter()
                        .filter(|m| flock.in_flock(m, target))
                        .peekable();
                    let none = task.spec.machine.is_none()
                        && members.peek().is_some()
                        && members.all(|m| {
                            matches!(settled_on(&m.name), Some(Err(e)) if e.code == MODEL_KIND_MISMATCH)
                        });
                    none.then(|| {
                        let a = if kind.starts_with(['a', 'e', 'i', 'o', 'u']) {
                            "an"
                        } else {
                            "a"
                        };
                        format!("no machine in flock {target} has {a} {kind} agent")
                    })
                };
                // Or a machine would take it but its flock is at its
                // number there.
                let flock_full = || {
                    views
                        .iter()
                        .filter(|v| task.spec.machine.as_ref().is_none_or(|p| *p == v.name))
                        .filter(|v| {
                            v.healthy
                                && v.has_room(claim)
                                && task.spec.tags.iter().all(|t| v.tags.contains(t))
                                && settled_on(&v.name).is_none_or(|r| r.is_ok())
                        })
                        .find_map(|v| {
                            let seat = v.seat(target)?;
                            let max = seat.max.filter(|_| !seat.has_room())?;
                            Some(format!(
                                "flock {target} is at {} of {max} on {}",
                                seat.live, v.name
                            ))
                        })
                };
                let why = none_has()
                    .or_else(|| {
                        let m = pick_machine(&views, target, &task.spec, claim)?;
                        Some(settled_on(&m)?.err()?.to_string())
                    })
                    .or_else(flock_full);
                let note = why.map(|why| format!("{WAITING_FOR_MODEL}: {why}"));
                let stale = task
                    .error
                    .as_deref()
                    .is_some_and(|e| e.starts_with(WAITING_FOR_MODEL));
                if note.is_some() && task.error != note || note.is_none() && stale {
                    let mut t = task.clone();
                    t.error = note;
                    if let Err(err) = self.store.update_task(&mut t) {
                        tracing::warn!(task = %task.display_id(), %err, "note why it waits");
                    }
                }
                continue;
            };
            let Some(handle) = self.get(&name) else {
                continue;
            };
            let mut on = task.clone();
            if let Some(Ok(spec)) = settled_on(&name) {
                on.spec = spec;
            }
            if on
                .error
                .as_deref()
                .is_some_and(|e| e.starts_with(WAITING_FOR_MODEL))
            {
                on.error = None;
            }
            if (on.spec != task.spec || on.error != task.error)
                && let Err(err) = self.store.update_task(&mut on)
            {
                tracing::warn!(task = %task.display_id(), machine = %name, %err, "settle agent");
                continue;
            }
            match handle.dispatch(task.id).await {
                Ok(t) => {
                    tracing::info!(task = %t.display_id(), machine = %name, state = %t.state, "dispatched")
                }
                Err(err) => {
                    tracing::warn!(task = %task.display_id(), machine = %name, %err, "dispatch failed")
                }
            }
        }
    }

    /// Where critical task `task` could start by pausing a task: the
    /// machines it may run on (in its flock, healthy, its tags, its pin, one
    /// `accepts`) that have no room for `claim` or `flock` is at its number
    /// there now, but would have both once one pausable task
    /// (`Task::pausable`) is gone, the newest such, the machine with the
    /// fewest live tasks first. Pausing another flock's task frees a slot,
    /// not a seat in `flock`, so it only helps a machine short of slots.
    /// Answers the machine and that task's id.
    fn pausable_for(
        &self,
        views: &[MachineView],
        flock: &str,
        task: &Task,
        claim: Claim,
        accepts: &dyn Fn(&str) -> bool,
    ) -> Option<(String, i64)> {
        let agents = self.agents.read().unwrap().clone();
        let default = self.flock().default_flock().to_string();
        let now = chrono::Utc::now();
        views
            .iter()
            .filter(|m| {
                m.in_flock(flock)
                    && m.healthy
                    && !(m.has_room(claim) && m.flock_has_room(flock))
                    && task.spec.machine.as_ref().is_none_or(|p| *p == m.name)
                    && task.spec.tags.iter().all(|t| m.tags.contains(t))
                    && accepts(&m.name)
            })
            .filter_map(|m| {
                let victim = self
                    .store
                    .tasks_on_machine(&m.name)
                    .ok()?
                    .into_iter()
                    .filter(|t| t.pausable(agents.kind(&t.spec.agent), now))
                    .filter(|t| {
                        let mut after = MachineView {
                            live: m.live.saturating_sub(1),
                            live_jobs: m.live_jobs.saturating_sub(usize::from(t.from_job())),
                            ..m.clone()
                        };
                        let freed = t.flock.as_deref().unwrap_or(&default);
                        if let Some(s) = after.flocks.iter_mut().find(|s| s.name == freed) {
                            s.live = s.live.saturating_sub(1);
                        }
                        after.has_room(claim) && after.flock_has_room(flock)
                    })
                    .max_by_key(|t| t.id)?;
                Some((m.live, m.name.clone(), victim.id))
            })
            .min_by_key(|(live, _, _)| *live)
            .map(|(_, name, id)| (name, id))
    }

    /// Resume paused task `task` on the machine it was paused on, once that
    /// machine is healthy, still in the flock and has room for it, its own
    /// flock under its number there. It is not settled again: it goes back
    /// to the agent and session it had.
    async fn resume_paused(&self, task: &Task) {
        let Some(machine) = task.pinned_machine() else {
            return;
        };
        let flock = self.flock();
        let target = task.flock.as_deref().unwrap_or(flock.default_flock());
        let views = self.views();
        let fits = views
            .iter()
            .find(|m| m.name == machine)
            .is_some_and(|m| m.healthy && m.has_room(Claim::of(task)) && m.flock_has_room(target));
        if !fits || !self.in_flock(machine) {
            return;
        }
        let Some(handle) = self.get(machine) else {
            return;
        };
        match handle.resume(task.id).await {
            Ok(t) => {
                tracing::info!(task = %t.display_id(), machine = %machine, state = %t.state, "resumed")
            }
            Err(err) => {
                tracing::warn!(task = %task.display_id(), machine = %machine, %err, "resume failed")
            }
        }
    }
}

/// How `AgentSource` and `Task::priority_from` name a layer: `asked_by`
/// for the ask, else the machine, the flock or `defaults`.
fn layer_label(layer: Layer, asked_by: &str, flock: &str, machine: Option<&str>) -> String {
    match layer {
        Layer::Ask => asked_by.to_string(),
        Layer::Machine => format!("machine {}", machine.unwrap_or("-")),
        Layer::Flock => format!("flock {flock}"),
        Layer::Defaults => "defaults".to_string(),
    }
}

use crate::queue::{WAITING_FOR_MODEL, asked_by};

/// An error from code the CLI shares with the head, as a reply: its
/// `CliError` code when it has one, else `runtime_error`.
fn cli_error(err: anyhow::Error) -> IpcResponse {
    match err.downcast_ref::<crate::cli::CliError>() {
        Some(e) => IpcResponse::error(&e.code, &e.message),
        None => IpcResponse::error("runtime_error", format!("{err:#}")),
    }
}

/// The answer to `Tick`, `Reload`, `JobList` or `JobRun` from `scheduler`,
/// the same from the head and a headless serve; `None` for any other
/// request.
pub(crate) async fn jobs_answer(
    scheduler: &SchedulerHandle,
    req: IpcRequest,
) -> Option<IpcResponse> {
    Some(match req {
        IpcRequest::Tick { job, dry_run } => match scheduler.tick(job, dry_run).await {
            Ok(runs) => IpcResponse::Runs(runs),
            Err(err) => IpcResponse::error("scheduler_error", err),
        },
        IpcRequest::Reload => match scheduler.reload().await {
            Ok(jobs) => IpcResponse::Jobs(jobs),
            Err(err) => IpcResponse::error("scheduler_error", err),
        },
        IpcRequest::JobList => match scheduler.job_list().await {
            Ok(jobs) => IpcResponse::Jobs(jobs),
            Err(err) => IpcResponse::error("scheduler_error", err),
        },
        IpcRequest::JobRun { name } => match scheduler.fire(&name).await {
            Ok(Ok(msg)) => IpcResponse::Text(msg),
            Ok(Err(reason)) => IpcResponse::error("job_not_found", reason),
            Err(err) => IpcResponse::error("scheduler_error", err),
        },
        _ => return None,
    })
}

/// Why an agent pastor started, in task `task`, was refused a change to
/// the fleet. The CLI says the same for a change it makes on its own.
/// What the head says when it refuses `task`, of `role`, a fleet change.
pub fn refusal(task: &str, role: TaskRole) -> String {
    match role {
        TaskRole::Agent => agent_refusal(task),
        TaskRole::Orchestrator => format!(
            "{task} is an orchestrator, and an orchestrator may only run, retry, send to and close tasks, enable and disable jobs and keep its note besides reading; set agents_change_fleet = true in pastor.toml to allow the rest"
        ),
    }
}

/// What the head says when it refuses orchestrator `name`'s pre or post
/// script a change outside the role's table.
pub fn script_refusal(name: &str) -> String {
    format!(
        "this is orchestrator {name}'s script, and an orchestrator may only run, retry, send to and close tasks, enable and disable jobs and keep its note besides reading; set agents_change_fleet = true in pastor.toml to allow the rest"
    )
}

pub fn agent_refusal(task: &str) -> String {
    format!(
        "{task} is an agent pastor started, and agents may not change the fleet (run, send to, attach to, retry, reprioritize, move in the queue, close or prune tasks, tick (dry runs too), run or reload jobs, install, link, uninstall or unlink connectors, edit machines, flocks, jobs or pastor.toml, serve or set up a head, open herdr's UI; `pastor task done` may end only its own task); set agents_change_fleet = true in pastor.toml to allow it"
    )
}

pub struct Daemon {
    paths: Paths,
    store: Arc<Store>,
    fleet: Arc<Fleet>,
    scheduler: SchedulerHandle,
    events: broadcast::Sender<PastorEvent>,
    orchestrators: Arc<crate::orchestrator::Runner>,
}

/// The longest IPC request line, newline excluded. The largest real one is a
/// `task run` prompt, far below this.
const MAX_IPC_REQUEST: usize = 1024 * 1024;

/// How long a client gets to send its request line. The CLI sends it at once;
/// a connection that stays silent is holding a file descriptor for nothing.
const IPC_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// The pause after a failed `accept`, so EMFILE does not spin the loop.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

#[derive(Debug)]
enum RequestReadError {
    TooLarge,
    TimedOut,
    NotUtf8,
    Io,
}

/// One request line from `r`, newline included when there is one, read
/// within `time` and refused past `max` bytes.
async fn read_request<R: tokio::io::AsyncRead + Unpin>(
    r: R,
    max: usize,
    time: Duration,
) -> Result<String, RequestReadError> {
    let mut buf = Vec::new();
    let mut r = BufReader::new(r.take(max as u64 + 1));
    match tokio::time::timeout(time, r.read_until(b'\n', &mut buf)).await {
        Err(_) => return Err(RequestReadError::TimedOut),
        Ok(Err(_)) => return Err(RequestReadError::Io),
        Ok(Ok(_)) => {}
    }
    let content = buf.len() - usize::from(buf.last() == Some(&b'\n'));
    if content > max {
        return Err(RequestReadError::TooLarge);
    }
    // Strict, never lossy: U+FFFD in place of bad bytes could turn a
    // malformed line into a valid request with different parameters.
    String::from_utf8(buf).map_err(|_| RequestReadError::NotUtf8)
}

/// SIGTERM and SIGHUP, alongside ctrl_c's SIGINT, so `run_with_listener` can
/// select over all three without an attribute on a `tokio::select!` branch
/// (the macro does not support `#[cfg(...)]` there). Unix-only underneath,
/// like the rest of this file's `tokio::net::UnixListener`; on any other
/// platform `recv` simply never resolves, leaving ctrl_c as the only way in.
#[cfg(unix)]
struct ExtraSignals {
    sigterm: tokio::signal::unix::Signal,
    sighup: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl ExtraSignals {
    fn new() -> std::io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(ExtraSignals {
            sigterm: signal(SignalKind::terminate())?,
            sighup: signal(SignalKind::hangup())?,
        })
    }

    /// The log line for whichever signal arrived first.
    async fn recv(&mut self) -> &'static str {
        tokio::select! {
            _ = self.sigterm.recv() => "shutting down on SIGTERM; agents keep running",
            _ = self.sighup.recv() => "shutting down on SIGHUP; agents keep running",
        }
    }
}

#[cfg(not(unix))]
struct ExtraSignals;

#[cfg(not(unix))]
impl ExtraSignals {
    fn new() -> std::io::Result<Self> {
        Ok(ExtraSignals)
    }

    async fn recv(&mut self) -> &'static str {
        std::future::pending().await
    }
}

/// Refuse a socket that something answers on, or that something holds but
/// does not answer: only a refused (or absent) connect leaves it free to
/// replace. `Daemon::bind_socket` checks this before it binds, and a
/// background `pastor serve` before it starts the head that would.
pub async fn refuse_live_socket(socket: &std::path::Path) -> anyhow::Result<()> {
    // Staleness is a property of the connect, not of the reply: a live
    // daemon mid-request (e.g. `dispatch_queued` against a slow or wedged
    // herdr) can go a while without answering a ping, and a busy daemon
    // looks exactly like a wedged one from the outside. Only a refused (or
    // absent) connect means nothing is actually listening; anything else
    // must be left alone rather than unlinked and stolen.
    match crate::ipc::ping_head(socket).await {
        HeadPing::Pong { role: Some(r), .. } if r == crate::ipc::SHEPHERD_ROLE => {
            Err(crate::cli::CliError::err(
                "shepherd_running",
                format!(
                    "a headless pastor serve is already running on {}; stop it first (`pastor serve stop`)",
                    socket.display()
                ),
            ))
        }
        HeadPing::Pong { .. } => Err(crate::cli::CliError::err(
            "head_running",
            format!(
                "another pastor daemon is already running on {}, as this machine's head; stop it first (`pastor serve stop`)",
                socket.display()
            ),
        )),
        HeadPing::Unresponsive => anyhow::bail!(
            "a daemon is listening on {} but did not respond within 2s; \
             remove the socket file by hand only if that daemon is dead",
            socket.display()
        ),
        HeadPing::NotRunning => Ok(()),
    }
}

/// What answers a request line on the socket: the head (`Daemon`) or a
/// headless serve (`shepherd::Shepherd`).
pub(crate) trait Answer: Send + Sync + 'static {
    fn answer(
        &self,
        req: IpcRequest,
        from: crate::ipc::Caller,
    ) -> impl std::future::Future<Output = IpcResponse> + Send;
}

impl Answer for Daemon {
    async fn answer(&self, req: IpcRequest, from: crate::ipc::Caller) -> IpcResponse {
        self.handle_as(req, &from).await
    }
}

/// The accept loop on a socket `daemon` already owns, until a signal. See
/// `Daemon::run_with_listener`.
pub(crate) async fn answer_on<A: Answer>(
    daemon: Arc<A>,
    listener: tokio::net::UnixListener,
    socket: PathBuf,
) -> anyhow::Result<()> {
    let mut extra_signals = ExtraSignals::new()?;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let stream = match accepted {
                    Ok((stream, _)) => stream,
                    // Out of file descriptors, or a client that hung up
                    // mid-accept: the listener is fine, and exiting would
                    // hand every other client a restart loop. Back off so
                    // EMFILE does not spin, then keep serving.
                    Err(e) => {
                        tracing::warn!(error = %e, "accept on the socket failed");
                        tokio::time::sleep(ACCEPT_BACKOFF).await;
                        continue;
                    }
                };
                let d = daemon.clone();
                tokio::spawn(async move {
                    let (r, mut w) = stream.into_split();
                    let resp = match read_request(r, MAX_IPC_REQUEST, IPC_READ_TIMEOUT).await {
                        Ok(line) => match crate::ipc::parse_request_line(line.trim()) {
                            Ok((req, from)) => d.answer(req, from).await,
                            Err(err) => IpcResponse::error("invalid_request", err),
                        },
                        Err(RequestReadError::TooLarge) => IpcResponse::error(
                            "request_too_large",
                            format!("a request is at most {MAX_IPC_REQUEST} bytes"),
                        ),
                        Err(RequestReadError::NotUtf8) => {
                            IpcResponse::error("invalid_request", "a request must be UTF-8")
                        }
                        Err(_) => return,
                    };
                    let mut out = serde_json::to_string(&resp).unwrap_or_else(|e| format!("{{\"kind\":\"error\",\"code\":\"internal\",\"message\":\"{e}\"}}"));
                    out.push('\n');
                    let _ = w.write_all(out.as_bytes()).await;
                });
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutting down on SIGINT; agents keep running");
                let _ = std::fs::remove_file(&socket);
                return Ok(());
            }
            msg = extra_signals.recv() => {
                tracing::info!("{msg}");
                let _ = std::fs::remove_file(&socket);
                return Ok(());
            }
        }
    }
}

impl Daemon {
    /// `on_disk` is `ConfigFingerprint::sample` taken before `config` and
    /// `flock` were read (or built): the scheduler keeps them until either
    /// file changes from that, then reloads from disk.
    pub async fn start(
        paths: Paths,
        config: PastorConfig,
        flock: Flock,
        on_disk: ConfigFingerprint,
        connect: Option<ConnectorFactory>,
    ) -> anyhow::Result<Daemon> {
        paths.ensure()?;
        let store = Arc::new(Store::open(&paths.db_file())?);
        // Before any actor or the scheduler reads a row, so none of them
        // sees a task from before flocks without one.
        store.adopt_default_flock(flock.default_flock())?;
        let (events, log_rx) = broadcast::channel(1024);
        let connect = connect.unwrap_or_else(|| endpoint_factory(paths.clone()));
        let fleet = Arc::new(Fleet::managed(store.clone(), events.clone(), connect));
        fleet.apply_flock(&flock, &machine_settings(&config)).await;
        // Fresh set: nothing has been warned about yet, so every task on a
        // machine `flock` does not have is reported.
        warn_removed(&store, &flock, &mut HashSet::new());
        // Subscribed in `start`, before any actor runs, so the log sees the
        // first events too. The log holds the fleet weakly (see `spawn_log`),
        // so dropping the daemon still winds the tasks down.
        // Connector event hooks get each record from the log task once it is
        // written, so they see the same sequence number.
        let lookup: Arc<dyn crate::events::MachineLookup> = fleet.clone();
        let (to_hooks, hooks_rx) = tokio::sync::mpsc::channel(crate::events::HOOK_QUEUE_CAPACITY);
        crate::events::spawn_log(
            paths.events_file(),
            crate::events::DEFAULT_MAX_BYTES,
            store.clone(),
            Some(Arc::downgrade(&lookup)),
            log_rx,
            Some(to_hooks),
        );
        crate::hooks::spawn(paths.clone(), store.clone(), events.downgrade(), hooks_rx);
        let scheduler = Scheduler::new(
            paths.clone(),
            &config,
            store.clone(),
            fleet.clone(),
            events.clone(),
        )
        .with_connectors()
        .with_config_baseline(on_disk)
        .spawn();
        let orchestrators = crate::orchestrator::Runner::new(
            paths.clone(),
            store.clone(),
            fleet.clone(),
            events.clone(),
        );
        orchestrators.spawn(config.tick_duration());
        Ok(Daemon {
            paths,
            store,
            fleet,
            scheduler,
            events,
            orchestrators,
        })
    }

    pub fn socket_path(&self) -> PathBuf {
        self.paths.socket_file()
    }
    pub fn store(&self) -> Arc<Store> {
        self.store.clone()
    }
    pub fn subscribe(&self) -> broadcast::Receiver<PastorEvent> {
        self.events.subscribe()
    }
    pub fn fleet(&self) -> Arc<Fleet> {
        self.fleet.clone()
    }
    pub fn scheduler(&self) -> SchedulerHandle {
        self.scheduler.clone()
    }

    /// Take ownership of the daemon socket: refuse to steal it from a live or
    /// merely unresponsive daemon, replace it if nothing answers, bind and lock
    /// down its permissions. Split out of `run` so a second `pastor serve` can
    /// be refused here, before `start` spawns a single machine actor or touches
    /// the shared database — not after, which is what let a second daemon
    /// reconcile and mutate the store for up to the probe timeout before it
    /// finally bailed.
    pub(crate) async fn bind_socket(
        socket: &std::path::Path,
    ) -> anyhow::Result<tokio::net::UnixListener> {
        if socket.exists() {
            refuse_live_socket(socket).await?;
            std::fs::remove_file(socket)?;
        }
        let listener = tokio::net::UnixListener::bind(socket)?;
        std::fs::set_permissions(
            socket,
            <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o600),
        )?;
        Ok(listener)
    }

    /// Own the socket, then build the daemon: machine actors are only spawned
    /// once the socket is ours, so a second `pastor serve` bails on a live
    /// daemon before it ever reconciles or dispatches against the shared
    /// database. Used by both `serve` and anything that needs the same
    /// ordering under test.
    pub async fn bind_and_start(
        paths: Paths,
        config: PastorConfig,
        flock: Flock,
        on_disk: ConfigFingerprint,
        connect: Option<ConnectorFactory>,
    ) -> anyhow::Result<(Daemon, tokio::net::UnixListener)> {
        paths.ensure()?;
        let listener = Daemon::bind_socket(&paths.socket_file()).await?;
        let daemon = Daemon::start(paths, config, flock, on_disk, connect).await?;
        Ok((daemon, listener))
    }

    pub async fn run(self) -> anyhow::Result<()> {
        let listener = Daemon::bind_socket(&self.socket_path()).await?;
        self.run_with_listener(listener).await
    }

    /// The accept/tick/event loop, given a socket this daemon already owns.
    ///
    /// systemd counts SIGTERM, SIGHUP and SIGINT as a clean exit and will not
    /// restart a `Restart=on-failure` unit after any of them. Before this,
    /// `pastor serve` only handled SIGINT (ctrl-c), so a stray SIGTERM or
    /// SIGHUP from outside a terminal killed the head silently: no log line
    /// past the start message, the socket file left behind, and the unit
    /// stayed down. All three now take the same shutdown path.
    pub async fn run_with_listener(self, listener: tokio::net::UnixListener) -> anyhow::Result<()> {
        let socket = self.socket_path();
        answer_on(Arc::new(self), listener, socket).await
    }

    /// `handle`, for a caller that says it runs in a task's pane
    /// (`ipc::TASK_ENV`): unless `agents_change_fleet` is on, such a caller
    /// may read but not change the fleet, save to end its own task, and an
    /// orchestrator (`TaskRole::Orchestrator`) may make the changes
    /// `IpcRequest::orchestrator_may` lists. No caller in a task may make an
    /// orchestrator, `agents_change_fleet` or not: only a person does.
    pub async fn handle_from(&self, req: IpcRequest, from_task: Option<&str>) -> IpcResponse {
        self.handle_as(req, &crate::ipc::Caller::task(from_task))
            .await
    }

    /// `handle` for `caller`: a task's agent as `handle_from` says, an
    /// orchestrator's pre or post script (`orchestrator::ORCHESTRATOR_ENV`)
    /// under the orchestrator role's table, which must name an orchestrator
    /// the head has a file for, or a person. A caller that names both a
    /// task and an orchestrator is the task: an agent cannot widen its
    /// rights by setting the other.
    pub async fn handle_as(&self, req: IpcRequest, caller: &crate::ipc::Caller) -> IpcResponse {
        if let Some(task) = caller.task.as_deref() {
            if let Some(why) = self.makes_orchestrator(&req) {
                return IpcResponse::error("role_refused", format!("{task} is a task, and {why}"));
            }
            if req.changes_fleet() && !req.ends_own_task(task) && !self.fleet.agents_change_fleet()
            {
                let role = self.caller_role(task);
                if !(role == TaskRole::Orchestrator && req.orchestrator_may()) {
                    return IpcResponse::error("agent_refused", refusal(task, role));
                }
            }
            if let IpcRequest::OrchestratorNote { name, text } = req {
                return self.note_from_task(task, name, &text);
            }
            // The agent ending its own task: one that must say what it did is
            // held to it (`SummaryMode::Require`).
            if req.ends_own_task(task)
                && let IpcRequest::TaskDone { id, summary } = req
            {
                return self.end(id, summary, crate::machine::EndBy::Agent).await;
            }
            return self.handle(req).await;
        }
        if let Some(o) = caller.orchestrator.as_deref() {
            if let Some(why) = self.makes_orchestrator(&req) {
                return IpcResponse::error(
                    "role_refused",
                    format!("orchestrator {o}'s script is not a person, and {why}"),
                );
            }
            if req.changes_fleet() {
                if !self.orchestrators.knows(o) {
                    return IpcResponse::error(
                        "agent_refused",
                        format!(
                            "{} names {o:?}, but the head has no orchestrator of that name",
                            crate::orchestrator::ORCHESTRATOR_ENV
                        ),
                    );
                }
                if !req.orchestrator_may() && !self.fleet.agents_change_fleet() {
                    return IpcResponse::error("agent_refused", script_refusal(o));
                }
            }
            if let IpcRequest::OrchestratorNote { name, text } = req {
                if name.as_deref().is_some_and(|n| n != o) {
                    return IpcResponse::error(
                        "agent_refused",
                        format!("orchestrator {o}'s script may keep only its own note"),
                    );
                }
                return self.note(o, &text);
            }
        }
        self.handle(req).await
    }

    /// `orchestrator note` from task `task`'s agent: the note of the
    /// orchestrator that started it, and no other.
    fn note_from_task(&self, task: &str, name: Option<String>, text: &str) -> IpcResponse {
        let own = crate::task::parse_task_id(task).and_then(|id| self.orchestrators.of_task(id));
        match (own, name) {
            (Some(own), Some(name)) if name != own => IpcResponse::error(
                "agent_refused",
                format!("{task} is orchestrator {own}'s agent, and may keep only its note"),
            ),
            (Some(own), _) => self.note(&own, text),
            (None, _) => IpcResponse::error(
                "not_an_orchestrator",
                format!(
                    "{task} was not started by an orchestrator file, so it has no note to keep"
                ),
            ),
        }
    }

    fn note(&self, name: &str, text: &str) -> IpcResponse {
        if let Err((code, message)) = crate::orchestrator::file_of(&self.paths, name) {
            return IpcResponse::error(&code, message);
        }
        match crate::orchestrator::write_note(&self.paths, name, text) {
            Ok(said) => IpcResponse::Text(said),
            Err(err) => IpcResponse::error("runtime_error", format!("{err:#}")),
        }
    }

    /// Why `req` would make an orchestrator, which only a person may do:
    /// `task run --role orchestrator`, or a retry of an orchestrator task,
    /// whose copy keeps the role.
    fn makes_orchestrator(&self, req: &IpcRequest) -> Option<String> {
        match req {
            IpcRequest::Run {
                role: TaskRole::Orchestrator,
                ..
            } => Some("only a person may run a task with --role orchestrator".into()),
            IpcRequest::TaskRetry { id, .. }
                if self
                    .store
                    .get_task(*id)
                    .ok()
                    .flatten()
                    .is_some_and(|t| t.role == TaskRole::Orchestrator) =>
            {
                Some(format!(
                    "t-{id} is an orchestrator, whose retry would be one too; only a person may retry it"
                ))
            }
            _ => None,
        }
    }

    /// The role of `task`, the task a caller says it runs in. One the store
    /// does not know, or cannot read, is a plain agent: the refusal is the
    /// safe side.
    fn caller_role(&self, task: &str) -> TaskRole {
        crate::task::parse_task_id(task)
            .and_then(|id| self.store.get_task(id).ok().flatten())
            .map_or(TaskRole::Agent, |t| t.role)
    }

    /// One `fleet_edit` edit of flock.toml for a CLI, then the reload that
    /// applies it: what the CLI would do with no head, done here so the
    /// head's file is the one changed. The edit and the wanted flock it
    /// leaves change together under the dispatch lock (`Fleet::edit_flock_file`),
    /// so a `task run` sees the flock from before or after it, never a mix.
    /// Answers `Text`: what the edit did, `sep`, and how the reload went.
    async fn edit_flock_file(
        &self,
        sep: &str,
        edit: impl FnOnce(&std::path::Path) -> anyhow::Result<String>,
    ) -> IpcResponse {
        let done = self
            .fleet
            .edit_flock_file(&self.paths.flock_file(), edit)
            .await;
        let done = match done {
            Ok(done) => done,
            Err(err) => {
                return match err.downcast_ref::<EditError>() {
                    Some(e) => IpcResponse::error(e.code(), e),
                    None => IpcResponse::error("runtime_error", format!("{err:#}")),
                };
            }
        };
        match self.scheduler.reload().await {
            Ok(_) => IpcResponse::Text(format!("{done}{sep}the running pastor serve picked it up")),
            Err(err) => IpcResponse::Text(format!(
                "{done}{sep}the reload after it failed ({err}); run `pastor job reload`"
            )),
        }
    }

    pub async fn handle(&self, req: IpcRequest) -> IpcResponse {
        match req {
            IpcRequest::Ping => IpcResponse::Pong {
                version: env!("CARGO_PKG_VERSION").into(),
                protocol: crate::ipc::IPC_PROTOCOL,
                role: None,
            },
            IpcRequest::Run {
                prompt,
                spec,
                flock,
                agent,
                priority,
                role,
                description,
                preempt,
                summary,
            } => {
                // clap refuses this too; checked here as well so no other
                // client can queue a task dispatch can only fail.
                if spec.worktree && spec.repo.is_none() {
                    return IpcResponse::error(
                        "worktree_needs_repo",
                        "a worktree task needs a repo to branch from",
                    );
                }
                // pastor.toml and flock.toml as they read now, not as of the
                // last tick: `task run` has always taken a `[defaults]` edit
                // at once. Through the scheduler, so the `[agents]` the task
                // is checked against here are the ones its machine's actor
                // dispatches with. A file that does not load leaves the last
                // good one in use.
                if agent.is_some()
                    && let Err(err) = self.scheduler.sync_config().await
                {
                    tracing::warn!(%err, "config not re-read before task run");
                }
                let task = match self
                    .fleet
                    .queue_run_as(
                        prompt,
                        spec,
                        flock.as_deref(),
                        agent.as_ref(),
                        priority,
                        role,
                        crate::config::clean_description(description.as_deref()),
                        preempt,
                        summary,
                    )
                    .await
                {
                    Ok(t) => t,
                    Err(QueueError::Flock(err @ TaskFlockError::UnknownFlock(_))) => {
                        return IpcResponse::error("unknown_flock", err);
                    }
                    Err(QueueError::Flock(err @ TaskFlockError::MachineElsewhere { .. })) => {
                        return IpcResponse::error("flock_mismatch", err);
                    }
                    Err(QueueError::UnknownMachine(m)) => {
                        return IpcResponse::error(
                            "unknown_machine",
                            format!("machine {m} is not in the flock"),
                        );
                    }
                    Err(QueueError::Agent(err)) => {
                        return IpcResponse::error(err.code, err.message);
                    }
                    Err(QueueError::Store(err)) => {
                        return IpcResponse::error("store_error", err);
                    }
                };
                let _ = self.events.send(PastorEvent {
                    detail: None,
                    kind: "task.queued".into(),
                    task_id: Some(task.id),
                    machine: None,
                    job: Some(task.job.clone()),
                    summary: None,
                });
                self.fleet.dispatch_queued().await;
                match self.store.get_task(task.id) {
                    Ok(Some(t)) => IpcResponse::Task(t),
                    Ok(None) => IpcResponse::error("task_not_found", task.id),
                    Err(err) => IpcResponse::error("store_error", err),
                }
            }
            IpcRequest::List { filter } => match self.store.list_tasks(&filter) {
                Ok(ts) => IpcResponse::Tasks(ts),
                Err(err) => IpcResponse::error("store_error", err),
            },
            IpcRequest::TaskPriority {
                id,
                priority,
                preempt,
            } => {
                if preempt && priority != Priority::Critical {
                    return IpcResponse::error(
                        crate::task::PREEMPT_NEEDS_CRITICAL,
                        format!(
                            "--preempt needs critical: only a critical task may pause another, not a {priority} one"
                        ),
                    );
                }
                match self
                    .fleet
                    .set_priority_preempting(id, priority, "task priority", preempt)
                    .await
                {
                    Ok(t) => IpcResponse::Task(t),
                    Err(err @ PriorityError::NotFound(_)) => {
                        IpcResponse::error("task_not_found", err)
                    }
                    Err(err @ PriorityError::NotQueued { .. }) => {
                        IpcResponse::error("not_queued", err)
                    }
                    Err(PriorityError::Store(err)) => {
                        IpcResponse::error("store_error", format!("{err:#}"))
                    }
                }
            }
            IpcRequest::Queue { flock, machine } => {
                match self.fleet.queue(flock.as_deref(), machine.as_deref()) {
                    Ok(entries) => IpcResponse::Queue(entries),
                    Err(err) => IpcResponse::error("store_error", format!("{err:#}")),
                }
            }
            IpcRequest::QueueMove { id, to } => match self.fleet.move_queued(id, to).await {
                Ok(moved) => IpcResponse::Moved(moved),
                Err(err @ MoveError::NotFound(_)) => IpcResponse::error("task_not_found", err),
                Err(err @ MoveError::NotQueued { .. }) => IpcResponse::error("not_queued", err),
                Err(MoveError::Store(err)) => IpcResponse::error("store_error", format!("{err:#}")),
            },
            IpcRequest::TaskShow { id } => match self.store.get_task(id) {
                Ok(Some(t)) => IpcResponse::Task(t),
                Ok(None) => IpcResponse::error("task_not_found", format!("t-{id}")),
                Err(err) => IpcResponse::error("store_error", err),
            },
            IpcRequest::TaskSummaries { id } => match self.store.get_task(id) {
                Ok(Some(_)) => match self.store.summaries(id) {
                    Ok(all) => IpcResponse::Summaries(all),
                    Err(err) => IpcResponse::error("store_error", format!("{err:#}")),
                },
                Ok(None) => IpcResponse::error("task_not_found", format!("t-{id}")),
                Err(err) => IpcResponse::error("store_error", err),
            },
            IpcRequest::TaskRead { id, lines } => {
                let task = match self.store.get_task(id) {
                    Ok(Some(t)) => t,
                    Ok(None) => return IpcResponse::error("task_not_found", format!("t-{id}")),
                    Err(err) => return IpcResponse::error("store_error", err),
                };
                let Some(handle) = task.machine.as_ref().and_then(|m| self.fleet.get(m)) else {
                    return IpcResponse::error(
                        "no_machine",
                        format!("t-{id} is not on any machine"),
                    );
                };
                if self.fleet.is_pull(&handle.name) {
                    return pull_machine_task(&task, &handle.name);
                }
                // Its aborted actor answers nothing; the read would wait for
                // as long as it stays wedged.
                if self.fleet.shutting_down(&handle.name) {
                    return IpcResponse::error(
                        "machine_shutting_down",
                        format!("machine {} is shutting down; try again later", handle.name),
                    );
                }
                match handle.read(id, lines).await {
                    Ok(text) => IpcResponse::Text(text),
                    Err(err) if err.downcast_ref::<ActorStopped>().is_some() => {
                        IpcResponse::error("machine_shutting_down", err)
                    }
                    Err(err) => IpcResponse::error("read_failed", err),
                }
            }
            IpcRequest::FlockList => IpcResponse::Machines(self.fleet.statuses()),
            IpcRequest::FlockRemove { name } => {
                if let Err(err) = self
                    .fleet
                    .remove_flock(&self.paths.flock_file(), &name)
                    .await
                {
                    return match err.downcast_ref::<EditError>() {
                        Some(e) => IpcResponse::error(e.code(), e),
                        None => IpcResponse::error("runtime_error", format!("{err:#}")),
                    };
                }
                // The file changed on disk; the reload brings the scheduler's
                // view of it up to date. The flock is already out of dispatch.
                match self.scheduler.reload().await {
                    Ok(_) => IpcResponse::Text(format!(
                        "removed flock {name}; the running pastor serve picked it up"
                    )),
                    Err(err) => IpcResponse::Text(format!(
                        "removed flock {name}; the reload after it failed ({err}); run `pastor job reload`"
                    )),
                }
            }
            IpcRequest::FlockAdd {
                name,
                default,
                description,
                machines,
            } => {
                let store = &self.store;
                self.edit_flock_file("; ", |file| {
                    crate::fleet_edit::add_flock(
                        file,
                        &name,
                        default,
                        description.as_deref(),
                        &machines,
                        || {
                            Ok(store
                                .queued_tasks()?
                                .iter()
                                .filter(|t| t.flock.as_deref() == Some(DEFAULT_FLOCK))
                                .map(|t| t.display_id())
                                .collect())
                        },
                    )
                })
                .await
            }
            IpcRequest::FlockJoin {
                flock,
                machine,
                max,
            } => {
                self.edit_flock_file("; ", |file| {
                    crate::fleet_edit::join_flock(file, &flock, &machine, max)
                })
                .await
            }
            IpcRequest::FlockLeave { flock, machine } => {
                let store = &self.store;
                self.edit_flock_file("; ", |file| {
                    crate::fleet_edit::leave_flock(file, &flock, &machine, || store.queued_tasks())
                })
                .await
            }
            IpcRequest::FlockSetDefault { name } => {
                self.edit_flock_file("; ", |file| crate::fleet_edit::set_default(file, &name))
                    .await
            }
            // The CLI prints its herdr lines after this one, so the reload
            // gets a line of its own, as without a head.
            IpcRequest::MachineAdd { machine } => {
                self.edit_flock_file("\n", |file| crate::fleet_edit::add_machine(file, &machine))
                    .await
            }
            IpcRequest::MachineRemove { name } => {
                self.edit_flock_file("; ", |file| crate::fleet_edit::remove_machine(file, &name))
                    .await
            }
            IpcRequest::MachineMove { name, flock } => {
                self.edit_flock_file("; ", |file| {
                    crate::fleet_edit::move_machine(file, &name, &flock)
                })
                .await
            }
            req @ (IpcRequest::Tick { .. }
            | IpcRequest::Reload
            | IpcRequest::JobList
            | IpcRequest::JobRun { .. }) => jobs_answer(&self.scheduler, req)
                .await
                .expect("a job request"),
            IpcRequest::JobSubmit {
                job,
                dispatch,
                prompt,
                items,
            } => self.submit(job, dispatch, prompt, items).await,
            IpcRequest::TaskRetry { id, place } => self.retry(id, place).await,
            IpcRequest::TaskClose {
                id,
                remove_worktree,
            } => self.close(id, remove_worktree).await,
            IpcRequest::TaskSend { id, input } => self.send(id, input).await,
            IpcRequest::TaskDone { id, summary } => {
                self.end(id, summary, crate::machine::EndBy::Hand).await
            }
            IpcRequest::TaskPrune {
                states,
                older_than_secs,
            } => {
                if let Some(s) = states.iter().find(|s| !s.is_prunable()) {
                    return IpcResponse::error(
                        "not_prunable",
                        format!("{s} tasks cannot be pruned; only done, failed and closed"),
                    );
                }
                match self
                    .store
                    .prune(&states, std::time::Duration::from_secs(older_than_secs))
                {
                    Ok(out) => IpcResponse::Pruned(out),
                    Err(err) => IpcResponse::error("store_error", err),
                }
            }
            IpcRequest::EventsSince { after, limit, task } => {
                // Up to two log generations of file reading: off the runtime.
                let path = self.paths.events_file();
                match tokio::task::spawn_blocking(move || {
                    crate::events::since(&path, after, limit, task)
                })
                .await
                {
                    Ok(Ok(page)) => IpcResponse::Events(page),
                    Ok(Err(err)) => IpcResponse::error("events_read_failed", err),
                    Err(err) => IpcResponse::error("events_read_failed", err),
                }
            }
            IpcRequest::FileGet { file } => self.file_get(&file).unwrap_or_else(cli_error),
            IpcRequest::FilePut {
                file,
                text,
                base_hash,
            } => match self.file_put(&file, &text, &base_hash) {
                Ok(path) => IpcResponse::Text(format!(
                    "saved {}; {}",
                    path.display(),
                    self.reload_after_edit().await
                )),
                Err(err) => cli_error(err),
            },
            IpcRequest::JobDescribe { name } => {
                let statuses = match self.scheduler.job_list().await {
                    Ok(jobs) => jobs,
                    Err(err) => return IpcResponse::error("scheduler_error", err),
                };
                match crate::describe::job(&self.paths, &name, statuses, &self.store) {
                    Ok(d) => IpcResponse::Job(d),
                    Err(err) => cli_error(err),
                }
            }
            IpcRequest::JobTask {
                job,
                flock,
                agent,
                prompt,
                spec,
                item,
                description,
            } => {
                let job = crate::config::job::Job {
                    task_description: description,
                    description: None,
                    name: job,
                    // A headless serve schedules the job; the head only
                    // queues what it found, so these are never read.
                    schedule: crate::schedule::Schedule::Every(Duration::from_secs(3600)),
                    enabled: true,
                    connector: String::new(),
                    connector_config: serde_json::Value::Null,
                    prompt,
                    max_tasks_per_run: 1,
                    backfill: Duration::ZERO,
                    spec,
                    agent,
                    flock,
                    // A headless serve resolves priority itself before
                    // sending the item; the head does not re-render it.
                    priority: None,
                    preempt: false,
                    // `JobTask` carries none: the flock's or `[defaults]`.
                    summary: None,
                    dispatch: serde_json::Value::Null,
                };
                self.job_task(job, item).await
            }
            IpcRequest::JobSetEnabled { name, enabled } => {
                let set = crate::edit::ConfigFile::Job(name.clone())
                    .path(&self.paths)
                    .and_then(|path| crate::config::job::set_enabled(&path, enabled));
                if let Err(err) = set {
                    return cli_error(err);
                }
                let verb = if enabled { "enabled" } else { "disabled" };
                match self.scheduler.reload().await {
                    Ok(_) => IpcResponse::Text(format!("{verb} {name}")),
                    Err(err) => IpcResponse::Text(format!(
                        "{verb} {name}; the reload after it failed ({err}); run `pastor job reload`"
                    )),
                }
            }
            IpcRequest::TrustList => match self.store.trusted_repos() {
                Ok(list) => IpcResponse::Trusted(list),
                Err(err) => IpcResponse::error("store_error", err),
            },
            IpcRequest::TrustAdd { machine, repo } => {
                match self.store.trust_repo(&machine, &repo) {
                    Ok(true) => IpcResponse::Text(format!("{repo} on {machine} is trusted")),
                    Ok(false) => {
                        IpcResponse::Text(format!("{repo} on {machine} was already trusted"))
                    }
                    Err(err) => IpcResponse::error("store_error", err),
                }
            }
            IpcRequest::TrustRemove { machine, repo } => {
                match self.store.untrust(&machine, &repo) {
                    Ok(true) => {
                        IpcResponse::Text(format!("{repo} on {machine} is no longer trusted"))
                    }
                    Ok(false) => IpcResponse::error(
                        "not_trusted",
                        format!("{repo} on {machine} is not trusted"),
                    ),
                    Err(err) => IpcResponse::error("store_error", err),
                }
            }
            IpcRequest::FlockDescribe { name } => self.describe_flock(&name),
            IpcRequest::MachineDescribe { name } => self.describe_machine(&name),
            IpcRequest::TaskClaim {
                machine,
                free_slots,
                flock_work,
            } => match self.fleet.claim(&machine, free_slots, flock_work).await {
                Ok(tasks) => IpcResponse::Tasks(tasks),
                Err(err) => cli_error(err),
            },
            IpcRequest::TaskReport {
                machine,
                id,
                state,
                pane,
                detail,
            } => match self.fleet.report(&machine, id, state, pane, detail).await {
                Ok(task) => IpcResponse::Task(task),
                Err(err) => cli_error(err),
            },
            IpcRequest::OrchestratorList => {
                IpcResponse::Orchestrators(self.orchestrators.statuses(chrono::Utc::now()))
            }
            IpcRequest::OrchestratorDescribe { name } => {
                match self.orchestrators.describe(&name, chrono::Utc::now()) {
                    Ok(d) => IpcResponse::Orchestrator(d),
                    Err((code, message)) => IpcResponse::error(&code, message),
                }
            }
            IpcRequest::OrchestratorRun { name } => match self.orchestrators.fire(&name) {
                Ok(said) => IpcResponse::Text(said),
                Err((code, message)) => IpcResponse::error(&code, message),
            },
            IpcRequest::OrchestratorSetEnabled { name, enabled } => {
                match crate::orchestrator::set_enabled(&self.paths, &name, enabled) {
                    Ok(said) => IpcResponse::Text(said),
                    Err((code, message)) => IpcResponse::error(&code, message),
                }
            }
            IpcRequest::OrchestratorNote { name, text } => match name {
                Some(name) => self.note(&name, &text),
                None => IpcResponse::error(
                    "orchestrator_not_found",
                    "name the orchestrator whose note this is (--name)",
                ),
            },
        }
    }

    /// `FileGet`: the head's own copy of the file, and its hash.
    fn file_get(&self, file: &str) -> anyhow::Result<IpcResponse> {
        let path = file.parse::<crate::edit::ConfigFile>()?.path(&self.paths)?;
        let (text, hash) = crate::edit::get(&path)?;
        Ok(IpcResponse::File(crate::ipc::FileText {
            path: path.display().to_string(),
            text,
            hash,
        }))
    }

    /// `FilePut`: checked and written by `edit::put`, as an edit with no
    /// head is. Answers where it wrote.
    fn file_put(&self, file: &str, text: &str, base_hash: &str) -> anyhow::Result<PathBuf> {
        let file = file.parse::<crate::edit::ConfigFile>()?;
        let path = file.path(&self.paths)?;
        let check = file.checker(&self.paths)?;
        crate::edit::put(&path, text, base_hash, &check)?;
        Ok(path)
    }

    /// What a saved edit tells the user about the reload that follows it.
    async fn reload_after_edit(&self) -> String {
        match self.scheduler.reload().await {
            Ok(_) => "the running pastor serve picked it up".into(),
            Err(err) => format!("the reload after it failed ({err}); run `pastor job reload`"),
        }
    }

    /// `JobSubmit`: the scheduler builds the job, so the job files and
    /// `[defaults]` it checks against are the ones it runs with; the items
    /// are queued here, off its loop, and dispatched like a run's.
    async fn submit(
        &self,
        name: String,
        dispatch: serde_json::Value,
        prompt: String,
        items: Vec<serde_json::Value>,
    ) -> IpcResponse {
        let job = match self.scheduler.submitted(name, dispatch, prompt).await {
            Ok(Ok(job)) => job,
            Ok(Err((code, message))) => return IpcResponse::error(&code, message),
            Err(err) => return IpcResponse::error("scheduler_error", err),
        };
        let out =
            crate::scheduler::submit_items(&self.fleet, &self.store, &self.events, &job, &items)
                .await;
        // The name is reserved from the moment the scheduler builds the
        // `Job`, above, so a concurrent job-file reload or scheduled run
        // cannot mix its tasks and `seen` keys with this job's until now.
        self.scheduler.released(job.name.clone()).await;
        if !out.tasks.is_empty() {
            self.fleet.dispatch_queued().await;
        }
        // As queued: dispatch may have moved them on since.
        let tasks = out
            .tasks
            .into_iter()
            .map(|t| self.store.get_task(t.id).ok().flatten().unwrap_or(t))
            .collect();
        IpcResponse::JobSubmitted {
            tasks,
            skipped: out.skipped,
            refused: out.refused,
        }
    }

    /// `FlockDescribe`: from the flock last applied and the live agents,
    /// not from flock.toml as it reads now.
    fn describe_flock(&self, name: &str) -> IpcResponse {
        let flock = self.fleet.flock();
        if !flock.has_flock(name) {
            return IpcResponse::error("unknown_flock", format!("no flock {name}"));
        }
        let tasks = match self.store.list_tasks(&crate::describe::flock_tasks(name)) {
            Ok(ts) => ts,
            Err(err) => return IpcResponse::error("store_error", err),
        };
        let statuses = self.fleet.statuses();
        match crate::describe::flock_description(&flock, name, Some(&statuses), tasks) {
            Some(d) => IpcResponse::FlockDescription(d),
            None => IpcResponse::error("unknown_flock", format!("no flock {name}")),
        }
    }

    /// `MachineDescribe`: one of the head's machines, as its actor sees it.
    fn describe_machine(&self, name: &str) -> IpcResponse {
        let unknown =
            || IpcResponse::error("unknown_machine", format!("no machine {name} in the flock"));
        let flock = self.fleet.flock();
        let Some(m) = flock.get(name) else {
            return unknown();
        };
        let Some(status) = self.fleet.statuses().into_iter().find(|s| s.name == name) else {
            return unknown();
        };
        let tasks = match self.store.list_tasks(&crate::describe::machine_tasks(name)) {
            Ok(ts) => ts,
            Err(err) => return IpcResponse::error("store_error", err),
        };
        // A description is still worth sending without the log.
        let events = crate::events::read(&self.paths.events_file(), None).unwrap_or_default();
        IpcResponse::MachineDescription(crate::describe::MachineDescription {
            row: crate::cli::MachineRow::from(&status),
            session: m.session.clone(),
            model: m.model.clone(),
            agents_by_kind: m.agents.clone(),
            tasks,
            recent_errors: crate::describe::machine_errors(events, name),
        })
    }

    /// `TaskSend`: through the actor of the task's machine, which checks the
    /// row again and types into the pane. A task on no machine, or one whose
    /// machine has left the flock, has no agent to type into.
    async fn send(&self, id: i64, input: SendInput) -> IpcResponse {
        if input.is_empty() {
            return IpcResponse::error("nothing_to_send", "give text, --key or --trust");
        }
        if input.trust && (input.text.is_some() || !input.keys.is_empty()) {
            return IpcResponse::error(
                "usage_error",
                "--trust sends the agent's own keys; give no text or --key with it",
            );
        }
        let task = match self.store.get_task(id) {
            Ok(Some(t)) => t,
            Ok(None) => return IpcResponse::error("task_not_found", format!("t-{id}")),
            Err(err) => return IpcResponse::error("store_error", err),
        };
        let handle = task
            .machine
            .as_deref()
            .filter(|_| task.state.occupies_pane())
            .and_then(|m| self.fleet.get(m).filter(|_| self.fleet.in_flock(m)));
        let Some(handle) = handle else {
            return IpcResponse::error(
                "task_not_live",
                format!(
                    "{} is {} with no live agent to send to",
                    task.display_id(),
                    task.state
                ),
            );
        };
        if self.fleet.is_pull(&handle.name) {
            return pull_machine_task(&task, &handle.name);
        }
        if self.fleet.shutting_down(&handle.name) {
            return IpcResponse::error(
                "machine_shutting_down",
                format!("machine {} is shutting down; try again later", handle.name),
            );
        }
        let what = describe_input(&input);
        let trust = input.trust;
        match handle.send(id, input).await {
            Ok(t) if trust => IpcResponse::Text(match &t.spec.repo {
                Some(repo) => format!(
                    "sent the trust keys to {}; {repo} on {} is trusted from now on",
                    t.display_id(),
                    handle.name
                ),
                None => format!(
                    "sent the trust keys to {}; it has no --repo, so nothing was saved",
                    t.display_id()
                ),
            }),
            Ok(t) => IpcResponse::Text(format!("sent {what} to {}", t.display_id())),
            Err(err) => match err.downcast_ref::<SendRefused>() {
                Some(r) => IpcResponse::error(r.code, r),
                None => stopped_or(err, "send_failed"),
            },
        }
    }

    /// `TaskDone`: through the actor of the task's machine, which checks the
    /// row again and marks it done and ended. A task on no machine, or one
    /// whose machine has left the flock, has no pane to end.
    /// `by` is who asks (`EndBy`): the task's own agent (`handle_from`), or
    /// anyone else.
    async fn end(
        &self,
        id: i64,
        summary: Option<String>,
        by: crate::machine::EndBy,
    ) -> IpcResponse {
        let task = match self.store.get_task(id) {
            Ok(Some(t)) => t,
            Ok(None) => return IpcResponse::error("task_not_found", format!("t-{id}")),
            Err(err) => return IpcResponse::error("store_error", err),
        };
        let handle = task
            .machine
            .as_deref()
            .filter(|_| task.state.occupies_pane())
            .and_then(|m| self.fleet.get(m).filter(|_| self.fleet.in_flock(m)));
        let Some(handle) = handle else {
            return IpcResponse::error(
                "task_not_live",
                format!(
                    "{} is {} with no pane to end",
                    task.display_id(),
                    task.state
                ),
            );
        };
        if self.fleet.is_pull(&handle.name) {
            return match self.fleet.end_pull(task, summary, by) {
                Ok(t) => IpcResponse::Task(t),
                Err(err) => cli_error(err),
            };
        }
        match handle.end_by(id, summary, by).await {
            Ok(t) => IpcResponse::Task(t),
            Err(err) => match err.downcast_ref::<SendRefused>() {
                Some(r) => IpcResponse::error(r.code, r),
                None => stopped_or(err, "done_failed"),
            },
        }
    }

    /// `TaskRetry`: a new queued row copying `id` (see `Store::insert_retry`),
    /// dispatched at once like a `Run`. Answers the new row as it stands after
    /// the dispatch pass.
    /// `JobTask`: one item a headless serve's job found, queued here as
    /// that job's task, with the checks and rendering the head's own jobs
    /// get (`run_job`), then dispatched.
    async fn job_task(&self, job: crate::config::job::Job, item: serde_json::Value) -> IpcResponse {
        if let Err(err) = crate::config::job::check_name(&job.name) {
            return IpcResponse::error("invalid_request", format!("job name: {err}"));
        }
        if item
            .get("key")
            .and_then(serde_json::Value::as_str)
            .is_none()
        {
            return IpcResponse::error("invalid_request", "the item has no string key");
        }
        if job.spec.worktree && job.spec.repo.is_none() {
            return IpcResponse::error(
                "worktree_needs_repo",
                "a worktree task needs a repo to branch from",
            );
        }
        if let Err(why) = crate::scheduler::check_item_paths(&job, &item) {
            return IpcResponse::error("item_rejected", why);
        }
        let key = item
            .get("key")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if let Some(resp) = self.seen_job_task(&job.name, key) {
            return resp;
        }
        // As for `Run`: the agent resolves against the config as it reads now.
        if let Err(err) = self.scheduler.sync_config().await {
            tracing::warn!(%err, "config not re-read before a job task");
        }
        let task = match self
            .fleet
            .queue_job_task(&job, &item, |id| {
                crate::scheduler::render_task(&job, &item, id)
            })
            .await
        {
            Ok(t) => t,
            // Another request queued the key first.
            Err(err) => {
                return self
                    .seen_job_task(&job.name, key)
                    .unwrap_or_else(|| IpcResponse::error("job_task_refused", format!("{err:#}")));
            }
        };
        tracing::info!(job = %job.name, task = %task.display_id(), "task queued for a headless serve");
        let _ = self.events.send(PastorEvent {
            detail: None,
            kind: "task.queued".into(),
            task_id: Some(task.id),
            machine: None,
            job: Some(task.job.clone()),
            summary: None,
        });
        self.fleet.dispatch_queued().await;
        match self.store.get_task(task.id) {
            Ok(Some(t)) => IpcResponse::Task(t),
            Ok(None) => IpcResponse::error("task_not_found", task.id),
            Err(err) => IpcResponse::error("store_error", err),
        }
    }

    /// The answer to a `JobTask` whose key this head has seen: the task it
    /// queued then, so a serve that lost the first reply can mark the key
    /// seen and move its cursor. `already_seen` when that row is gone.
    /// `None` for an unseen key.
    fn seen_job_task(&self, job: &str, key: &str) -> Option<IpcResponse> {
        let id = match self.store.seen_task(job, key) {
            Ok(Some(id)) => id,
            Ok(None) => return None,
            Err(err) => return Some(IpcResponse::error("store_error", err)),
        };
        let gone = || {
            IpcResponse::error(
                crate::ipc::ALREADY_SEEN,
                format!("job {job} queued item {key} already, and its task is gone"),
            )
        };
        let Some(id) = id else { return Some(gone()) };
        Some(match self.store.get_task(id) {
            Ok(Some(t)) => IpcResponse::Task(t),
            Ok(None) => gone(),
            Err(err) => IpcResponse::error("store_error", err),
        })
    }

    async fn retry(&self, id: i64, place: Option<crate::task::Place>) -> IpcResponse {
        // The store checks the state and copies in one statement; its error
        // says which check failed, so a row pruned by a concurrent request is
        // `task_not_found` and a storage failure is `store_error`.
        let task = match self.fleet.queue_retry(id, place.as_ref()).await {
            Ok(t) => t,
            Err(QueueError::UnknownMachine(m)) => {
                return IpcResponse::error(
                    "unknown_machine",
                    format!("t-{id} is pinned to machine {m}, which is not in the flock"),
                );
            }
            Err(QueueError::Agent(err)) => {
                return IpcResponse::error(err.code, err.message);
            }
            // A retry keeps the flock of the task it copies; nothing chooses one.
            Err(QueueError::Flock(err)) => {
                return IpcResponse::error("unknown_flock", err);
            }
            Err(QueueError::Store(err @ RetryError::NotFound(_))) => {
                return IpcResponse::error("task_not_found", err);
            }
            Err(QueueError::Store(err @ RetryError::NotRetryable { .. })) => {
                return IpcResponse::error("not_retryable", err);
            }
            Err(QueueError::Store(RetryError::Store(err))) => {
                return IpcResponse::error("store_error", format!("{err:#}"));
            }
        };
        let _ = self.events.send(PastorEvent {
            detail: None,
            kind: "task.queued".into(),
            task_id: Some(task.id),
            machine: None,
            job: Some(task.job.clone()),
            summary: None,
        });
        self.fleet.dispatch_queued().await;
        match self.store.get_task(task.id) {
            Ok(Some(t)) => IpcResponse::Task(t),
            Ok(None) => IpcResponse::error("task_not_found", task.display_id()),
            Err(err) => IpcResponse::error("store_error", err),
        }
    }

    /// `TaskClose`: through the actor of the task's machine, which closes the
    /// pane (or worktree) before the row. A task that never reached a machine,
    /// or whose machine has left the flock, only has its row closed, and a closed one is answered as it is. With
    /// no row, the machines are asked for an orphaned agent `t-<id>` (as
    /// their last reconcile found them).
    async fn close(&self, id: i64, remove_worktree: bool) -> IpcResponse {
        let row = match self.store.get_task(id) {
            Ok(r) => r,
            Err(err) => return IpcResponse::error("store_error", err),
        };
        let Some(t) = row else {
            let name = crate::task::Task::agent_name_for(id);
            // A machine shutting down has an aborted actor that answers
            // nothing, so its last reconcile does not count.
            let Some(handle) = self.fleet.machines().into_iter().find(|m| {
                !self.fleet.shutting_down(&m.name) && m.snapshot().orphans.contains(&name)
            }) else {
                return IpcResponse::error(
                    "task_not_found",
                    format!("{name}: no task row, and no machine reports an agent by that name"),
                );
            };
            return match handle.close(id, remove_worktree).await {
                Err(err) if err.downcast_ref::<OrphanClosed>().is_some() => {
                    IpcResponse::Text(err.to_string())
                }
                Ok(t) => IpcResponse::Task(t),
                Err(err) => stopped_or(err, "close_failed"),
            };
        };
        self.close_row(t, remove_worktree).await
    }

    /// The rest of `close`, for the row `t` as it was read. A queued task
    /// can be claimed by a dispatch pass at any moment after that read, so
    /// its row is closed only while it is still queued (`close_queued`); on
    /// losing to a claim the row is read again and the close goes to the
    /// machine that took it, which closes the agent too.
    async fn close_row(&self, mut t: Task, remove_worktree: bool) -> IpcResponse {
        let id = t.id;
        // Runs twice at most: a row leaves `queued` only once, and a claim
        // sets `machine`, so the second pass routes to it.
        let machine = loop {
            if remove_worktree && !t.spec.worktree {
                return IpcResponse::error(
                    "no_worktree",
                    format!("{} has no worktree to remove", t.display_id()),
                );
            }
            // A closed task has no pane left to close, so repeating the close
            // answers the row without its machine, which may be gone or down.
            // A worktree removal still routes: the checkout may remain.
            if t.state == TaskState::Closed && !remove_worktree {
                return IpcResponse::Task(t);
            }
            // A paused task holds no pane: a plain close is its row alone,
            // unless a resume claims it first. Its checkout is on its
            // machine, so `--remove-worktree` goes there.
            let paused = t.state == TaskState::Paused && !remove_worktree;
            if !paused && let Some(m) = t.machine.clone() {
                break m;
            }
            let was = t.state;
            let closed = if was == TaskState::Queued || paused {
                match self.store.close_queued(id) {
                    Ok(Some(c)) => c,
                    // Claimed (or closed) since the read: read it again.
                    Ok(None) => match self.store.get_task(id) {
                        Ok(Some(fresh)) => {
                            t = fresh;
                            continue;
                        }
                        Ok(None) => return IpcResponse::error("task_not_found", t.display_id()),
                        Err(err) => return IpcResponse::error("store_error", err),
                    },
                    Err(err) => return IpcResponse::error("store_error", err),
                }
            } else {
                // Not queued and never on a machine: nothing can claim it.
                match self.store.close_task(id) {
                    Ok(c) => c,
                    Err(err) => return IpcResponse::error("store_error", err),
                }
            };
            if was != TaskState::Closed {
                let _ = self.events.send(PastorEvent {
                    detail: None,
                    kind: "task.closed".into(),
                    task_id: Some(id),
                    machine: None,
                    job: Some(closed.job.clone()),
                    summary: None,
                });
            }
            return IpcResponse::Task(closed);
        };
        // A removed machine held only until its old actor ends counts as
        // gone: that actor answers nothing and no replacement will come.
        // A pull machine has no actor either: the head closes the row, and
        // its own pastor serve closes the pane when it next reports on it.
        let pull = self.fleet.is_pull(&machine);
        if pull && remove_worktree {
            return pull_machine_task(&t, &machine);
        }
        let handle = self
            .fleet
            .get(&machine)
            .filter(|_| self.fleet.in_flock(&machine) && !pull);
        let Some(handle) = handle else {
            // Its machine left the flock, so no actor owns the row and no
            // herdr can be asked: a plain close is only the row, but the
            // checkout lives on that machine and cannot be removed from here.
            if remove_worktree {
                return IpcResponse::error(
                    "unknown_machine",
                    format!(
                        "{} is on machine {machine}, which is not in the flock, so its worktree cannot be reached; close it without --remove-worktree",
                        t.display_id()
                    ),
                );
            }
            let closed = match self.store.close_task(id) {
                Ok(c) => c,
                Err(err) => return IpcResponse::error("store_error", err),
            };
            let _ = self.events.send(PastorEvent {
                detail: None,
                kind: "task.closed".into(),
                task_id: Some(id),
                machine: Some(machine),
                job: Some(closed.job.clone()),
                summary: None,
            });
            return IpcResponse::Task(closed);
        };
        // Still in the flock but waiting for its old actor to end before the
        // replacement starts. That actor answers nothing, and the row belongs
        // to the replacement, so the close waits for it.
        if self.fleet.shutting_down(&machine) {
            return IpcResponse::error(
                "machine_shutting_down",
                format!("machine {machine} is shutting down; try again later"),
            );
        }
        match handle.close(id, remove_worktree).await {
            Ok(t) => IpcResponse::Task(t),
            Err(err) => stopped_or(err, "close_failed"),
        }
    }
}

/// The answer to a request about `task` that only its pull machine
/// `machine` can carry out, being the one that reaches its pane.
fn pull_machine_task(task: &Task, machine: &str) -> IpcResponse {
    IpcResponse::error(
        "pull_machine_task",
        format!(
            "{} runs on pull machine {machine}, which the head never reaches; read, send to or attach to it from {machine} itself",
            task.display_id()
        ),
    )
}

/// A request that lost the race with a reload stopping its machine's actor
/// answers like one refused up front; any other failure keeps `code`.
fn stopped_or(err: anyhow::Error, code: &str) -> IpcResponse {
    if let Some(stopped) = err.downcast_ref::<ActorStopped>() {
        return IpcResponse::error("machine_shutting_down", stopped);
    }
    IpcResponse::error(code, format!("{err:#}"))
}

pub async fn serve(paths: Paths) -> anyhow::Result<()> {
    // Before the loads: an edit that lands after them must still read as a
    // change on the scheduler's first pass.
    let on_disk = ConfigFingerprint::sample(&paths);
    let config = PastorConfig::load(&paths.config_file())?;
    let flock = Flock::load(&paths.flock_file())?;
    flock.check_config(&config.models, &config.agents, &config.profiles)?;
    anyhow::ensure!(
        !flock.machines.is_empty(),
        "flock is empty; add a machine with `pastor machine add`"
    );
    let (daemon, listener) = Daemon::bind_and_start(paths, config, flock, on_disk, None).await?;
    tracing::info!(socket = %daemon.socket_path().display(), machines = daemon.fleet.machines().len(), "pastor serve");
    daemon.run_with_listener(listener).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::flock::MachineConfig;
    use crate::herdr::ConnectorExt;
    use crate::herdr::fake::FakeHerdr;
    use crate::scheduler::RunOutcome;
    use crate::store::NewTask;
    use crate::store::TaskFilter;
    use crate::task::DispatchSpec;
    use std::time::{Duration, Instant};

    fn machine(name: &str, max: u32) -> MachineConfig {
        MachineConfig {
            pull: false,
            description: None,
            name: name.into(),
            local: false,
            ssh: None,
            command: Some(vec!["fake".into()]),
            session: "default".into(),
            max_agents: max,
            // No job slots or burst: tests of max_agents alone.
            job_slots: 0,
            burst: 0,
            tags: vec![],
            flock: None,
            agent: None,
            agent_args: None,
            model: None,
            priority: None,
            agents: Default::default(),
            profile: None,
        }
    }

    fn spec() -> DispatchSpec {
        DispatchSpec {
            agent: "claude".into(),
            agent_args: vec![],
            allow: vec![],
            deny: vec![],
            repo: None,
            worktree: false,
            branch: None,
            machine: None,
            tags: vec![],
            timeout_secs: 60,
            checkout: None,
            reopen: None,
            agent_source: None,
            place: Default::default(),
            session_id: None,
            label: Default::default(),
            summary: Default::default(),
        }
    }

    /// The config every `daemon()` in these tests runs with; a test that
    /// applies a flock by hand passes `machine_settings(&test_config())` so
    /// it does not look like a timing change.
    fn test_config() -> PastorConfig {
        PastorConfig {
            settle: "1s".into(),
            ..Default::default()
        }
    }

    /// Hands out the fake registered under a machine's name, or a fresh one.
    fn factory(fakes: &[(&str, FakeHerdr)]) -> ConnectorFactory {
        let by_name: std::collections::HashMap<String, FakeHerdr> = fakes
            .iter()
            .map(|(n, f)| (n.to_string(), f.clone()))
            .collect();
        Arc::new(move |m: &MachineConfig| {
            Arc::new(by_name.get(&m.name).cloned().unwrap_or_else(FakeHerdr::new))
                as Arc<dyn Connector>
        })
    }

    fn fast() -> MachineSettings {
        MachineSettings {
            settle: Duration::from_millis(100),
            reconcile_every: Duration::from_millis(200),
            initial_backoff: Duration::from_millis(50),
            max_backoff: Duration::from_millis(200),
            request_timeout: Duration::from_secs(5),
            agent_ready_timeout: Duration::from_millis(500),
            poll_every: Duration::from_millis(200),
            close_done_after: None,
            agents: Default::default(),
            ..Default::default()
        }
    }

    fn managed(fakes: &[(&str, FakeHerdr)]) -> (Arc<Fleet>, Arc<Store>) {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (events, _) = broadcast::channel(64);
        let fleet = Arc::new(Fleet::managed(store.clone(), events, factory(fakes)));
        (fleet, store)
    }

    fn flock_of(machines: &[(&str, u32)]) -> Flock {
        Flock {
            flocks: vec![],
            machines: machines.iter().map(|(n, max)| machine(n, *max)).collect(),
        }
    }

    async fn healthy(fleet: &Fleet, name: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !fleet.views().iter().any(|v| v.name == name && v.healthy) {
            assert!(Instant::now() < deadline, "{name} never connected");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn apply_flock_adds_removes_and_retargets() {
        let (fleet, _store) = managed(&[]);
        let d = fleet
            .apply_flock(&flock_of(&[("a", 2), ("b", 2)]), &fast())
            .await;
        assert_eq!(
            d,
            FlockDiff {
                added: vec!["a".into(), "b".into()],
                ..Default::default()
            }
        );
        let a = fleet.get("a").unwrap();
        let b = fleet.get("b").unwrap();

        let d = fleet.apply_flock(&flock_of(&[("a", 3)]), &fast()).await;
        // The old actors have ended by the time `apply_flock` returns: a
        // removed machine cannot touch its rows afterwards, and a retargeted
        // one never has two actors at once.
        assert!(a.actor_finished() && b.actor_finished());
        assert!(a.tx.is_closed() && b.tx.is_closed());
        assert!(!fleet.get("a").unwrap().actor_finished());
        assert_eq!(d.removed, vec!["b".to_string()]);
        assert_eq!(d.retargeted, vec!["a".to_string()], "max_agents changed");
        assert!(d.added.is_empty());
        assert!(fleet.get("b").is_none());
        assert_eq!(fleet.get("a").unwrap().max_agents, 3);
        assert_eq!(fleet.flock(), flock_of(&[("a", 3)]));
    }

    /// Review Focus 2: an editor re-save, or `machine add other`, must not
    /// restart the actors of machines whose entry did not change.
    #[tokio::test]
    async fn unchanged_machines_keep_their_actor() {
        let (fleet, _store) = managed(&[]);
        fleet
            .apply_flock(&flock_of(&[("a", 2), ("b", 2)]), &fast())
            .await;
        let a = fleet.get("a").unwrap();
        let d = fleet
            .apply_flock(&flock_of(&[("a", 2), ("b", 2)]), &fast())
            .await;
        assert!(d.is_empty(), "{d:?}");
        let d = fleet
            .apply_flock(&flock_of(&[("a", 2), ("b", 2), ("c", 1)]), &fast())
            .await;
        assert_eq!(d.added, vec!["c".to_string()]);
        assert!(d.removed.is_empty() && d.retargeted.is_empty(), "{d:?}");
        assert!(
            a.tx.same_channel(&fleet.get("a").unwrap().tx),
            "a kept its actor"
        );
        assert!(!a.tx.is_closed());
    }

    #[tokio::test]
    async fn a_timing_change_replaces_every_actor() {
        let (fleet, _store) = managed(&[]);
        fleet.apply_flock(&flock_of(&[("a", 2)]), &fast()).await;
        let slower = MachineSettings {
            settle: Duration::from_millis(300),
            ..fast()
        };
        let d = fleet.apply_flock(&flock_of(&[("a", 2)]), &slower).await;
        assert_eq!(d.retargeted, vec!["a".to_string()]);
    }

    /// Review Focus 4: a machine removed and added back (or retargeted to a
    /// new address for the same host) gets its old tasks back: the new actor
    /// finds their panes, keeps them running and counts them again.
    #[tokio::test]
    async fn readding_a_machine_reconciles_its_old_tasks() {
        let fake = FakeHerdr::new();
        let (fleet, store) = managed(&[("a", fake.clone())]);
        fleet.apply_flock(&flock_of(&[("a", 2)]), &fast()).await;
        healthy(&fleet, "a").await;
        let t = store
            .insert_task(NewTask {
                description: None,
                job: "run".into(),
                item: serde_json::Value::Null,
                prompt: "p".into(),
                spec: spec(),
                flock: "default".into(),
            })
            .unwrap();
        fleet.dispatch_queued().await;
        assert_eq!(
            store.get_task(t.id).unwrap().unwrap().state,
            TaskState::Running
        );

        let d = fleet.apply_flock(&Flock::default(), &fast()).await;
        assert_eq!(d.removed, vec!["a".to_string()]);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            store.get_task(t.id).unwrap().unwrap().state,
            TaskState::Running,
            "nothing watches a removed machine's tasks, so nothing changes them"
        );

        let d = fleet.apply_flock(&flock_of(&[("a", 2)]), &fast()).await;
        assert_eq!(d.added, vec!["a".to_string()]);
        healthy(&fleet, "a").await;
        tokio::time::sleep(Duration::from_millis(500)).await; // two reconciles
        assert_eq!(
            store.get_task(t.id).unwrap().unwrap().state,
            TaskState::Running
        );
        assert_eq!(fleet.get("a").unwrap().snapshot().live, 1);
    }

    /// Through the fleet: a task on another machine starts with
    /// `PASTOR_HEAD`, one on the head's own machine without it.
    #[tokio::test]
    async fn a_task_off_the_head_machine_is_told_where_the_head_is() {
        let (here, there) = (FakeHerdr::new(), FakeHerdr::new());
        let (fleet, store) = managed(&[("here", here.clone()), ("there", there.clone())]);
        let mut flock = flock_of(&[("here", 1), ("there", 1)]);
        flock.machines[0].local = true;
        flock.machines[0].command = None;
        let settings = MachineSettings {
            head_address: Some("user@head.example".into()),
            ..fast()
        };
        fleet.apply_flock(&flock, &settings).await;
        healthy(&fleet, "here").await;
        healthy(&fleet, "there").await;
        let mut ids = vec![];
        for _ in 0..2 {
            let t = store
                .insert_task(NewTask {
                    description: None,
                    job: "run".into(),
                    item: serde_json::Value::Null,
                    prompt: "p".into(),
                    spec: spec(),
                    flock: "default".into(),
                })
                .unwrap();
            ids.push(t.id);
        }
        fleet.dispatch_queued().await;
        for id in ids {
            let t = store.get_task(id).unwrap().unwrap();
            let fake = match t.machine.as_deref() {
                Some("here") => &here,
                Some("there") => &there,
                other => panic!("t-{id} on {other:?}"),
            };
            let env = fake.pane_env(t.pane_id.as_deref().unwrap());
            match t.machine.as_deref() {
                Some("there") => assert_eq!(env["PASTOR_HEAD"], "user@head.example"),
                _ => assert!(env.get("PASTOR_HEAD").is_none(), "{env}"),
            }
        }
    }

    /// Copilot 4103070196: an old actor that does not end within the
    /// shutdown wait must not get a replacement next to it, or two actors
    /// race on the same tasks. The machine stays, marked as shutting down and
    /// out of dispatch, and the next pass finishes the swap once it has ended.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_replacement_waits_for_an_actor_that_does_not_stop() {
        let fake = FakeHerdr::new();
        let spawns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (events, _) = broadcast::channel(64);
        let connect: ConnectorFactory = {
            let (fake, spawns) = (fake.clone(), spawns.clone());
            Arc::new(move |_m: &MachineConfig| {
                spawns.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Arc::new(fake.clone()) as Arc<dyn Connector>
            })
        };
        let fleet = Fleet::managed(store.clone(), events, connect);
        let spawned = || spawns.load(std::sync::atomic::Ordering::SeqCst);

        fake.wedge_connects(true);
        fleet.apply_flock(&flock_of(&[("a", 2)]), &fast()).await;
        let deadline = Instant::now() + Duration::from_secs(5);
        while fake.wedged() == 0 {
            assert!(Instant::now() < deadline, "actor never reached connect");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let old = fleet.get("a").unwrap();

        let d = fleet.apply_flock(&flock_of(&[("a", 3)]), &fast()).await;
        assert_eq!(d.shutting_down, vec!["a".to_string()], "{d:?}");
        assert!(d.retargeted.is_empty(), "not replaced yet: {d:?}");
        assert_eq!(spawned(), 1, "no second actor next to the live one");
        assert!(old.tx.same_channel(&fleet.get("a").unwrap().tx));
        assert!(fleet.shutting_down("a"));
        assert!(
            !fleet.views().iter().any(|v| v.name == "a" && v.healthy),
            "nothing is dispatched to it"
        );
        assert_eq!(fleet.flock(), flock_of(&[("a", 3)]), "what was asked for");

        fake.wedge_connects(false);
        let d = fleet.apply_flock(&flock_of(&[("a", 3)]), &fast()).await;
        assert_eq!(d.retargeted, vec!["a".to_string()], "{d:?}");
        assert!(d.shutting_down.is_empty(), "{d:?}");
        assert!(old.actor_finished());
        assert_eq!(spawned(), 2);
        assert!(!fleet.shutting_down("a"));
        assert_eq!(fleet.get("a").unwrap().max_agents, 3);
        healthy(&fleet, "a").await;
    }

    /// The same for a removed machine: it stays listed until its actor ends.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_removed_machine_stays_until_its_actor_stops() {
        let fake = FakeHerdr::new();
        let (fleet, _store) = managed(&[("a", fake.clone())]);
        fake.wedge_connects(true);
        fleet.apply_flock(&flock_of(&[("a", 2)]), &fast()).await;
        let deadline = Instant::now() + Duration::from_secs(5);
        while fake.wedged() == 0 {
            assert!(Instant::now() < deadline, "actor never reached connect");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let d = fleet.apply_flock(&Flock::default(), &fast()).await;
        assert_eq!(d.shutting_down, vec!["a".to_string()], "{d:?}");
        assert!(d.removed.is_empty(), "{d:?}");
        assert!(fleet.get("a").is_some() && fleet.shutting_down("a"));

        fake.wedge_connects(false);
        let d = fleet.apply_flock(&Flock::default(), &fast()).await;
        assert_eq!(d.removed, vec!["a".to_string()], "{d:?}");
        assert!(fleet.machines().is_empty());
    }

    /// Review Focus 3: which rows a removed machine leaves behind. Only rows
    /// that still hold a pane count; a closed one is finished business.
    #[test]
    fn tasks_on_removed_machines_lists_open_tasks_only() {
        let store = Store::open_in_memory().unwrap();
        let put = |machine: &str, state: TaskState| {
            let mut t = store
                .insert_task(NewTask {
                    description: None,
                    job: "run".into(),
                    item: serde_json::Value::Null,
                    prompt: "p".into(),
                    spec: spec(),
                    flock: "default".into(),
                })
                .unwrap();
            t.machine = Some(machine.into());
            t.state = state;
            store.update_task(&mut t).unwrap();
            t.id
        };
        let running_gone = put("gone", TaskState::Running);
        let blocked_gone = put("gone", TaskState::Blocked);
        put("gone", TaskState::Closed);
        put("a", TaskState::Running);
        let mut ids: Vec<i64> = tasks_on_removed_machines(&store, &flock_of(&[("a", 2)]))
            .unwrap()
            .iter()
            .map(|t| t.id)
            .collect();
        ids.sort();
        assert_eq!(ids, vec![running_gone, blocked_gone]);
    }

    /// Copilot 4103271094: the SQL query, not a Rust filter after the fact,
    /// keeps a long closed/failed history off this path. A store with many
    /// finished rows on removed machines and one open one still returns just
    /// the open one.
    #[test]
    fn tasks_on_removed_machines_filters_pane_owning_states_in_sql() {
        let store = Store::open_in_memory().unwrap();
        let put = |machine: &str, state: TaskState| {
            let mut t = store
                .insert_task(NewTask {
                    description: None,
                    job: "run".into(),
                    item: serde_json::Value::Null,
                    prompt: "p".into(),
                    spec: spec(),
                    flock: "default".into(),
                })
                .unwrap();
            t.machine = Some(machine.into());
            t.state = state;
            store.update_task(&mut t).unwrap();
            t.id
        };
        for _ in 0..50 {
            put("gone", TaskState::Closed);
            put("gone", TaskState::Failed);
        }
        let open = put("gone", TaskState::Blocked);
        put("a", TaskState::Running);

        let ids: Vec<i64> = tasks_on_removed_machines(&store, &flock_of(&[("a", 2)]))
            .unwrap()
            .iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ids, vec![open]);
    }

    #[tokio::test]
    async fn a_fixed_fleet_ignores_apply_flock() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let fleet = Fleet::new(vec![], store);
        assert!(
            fleet
                .apply_flock(&flock_of(&[("a", 2)]), &fast())
                .await
                .is_empty()
        );
        assert!(fleet.machines().is_empty());
    }

    async fn daemon(fakes: &[(&str, u32, FakeHerdr)]) -> (Daemon, tempfile::TempDir) {
        let flock = Flock {
            flocks: vec![],
            machines: fakes.iter().map(|(n, max, _)| machine(n, *max)).collect(),
        };
        daemon_with_flock(flock, fakes).await
    }

    /// `flock` names the machines; `fakes` gives each its herdr.
    async fn daemon_with_flock(
        flock: Flock,
        fakes: &[(&str, u32, FakeHerdr)],
    ) -> (Daemon, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        // On disk too, as `serve` would have found them: a `pastor job reload`
        // re-reads both, and a missing file would read as an empty flock and
        // default timings.
        flock.save(&paths.flock_file()).unwrap();
        std::fs::write(
            paths.config_file(),
            toml::to_string(&test_config()).unwrap(),
        )
        .unwrap();
        let named: Vec<(&str, FakeHerdr)> = fakes.iter().map(|(n, _, f)| (*n, f.clone())).collect();
        let on_disk = ConfigFingerprint::sample(&paths);
        let d = Daemon::start(paths, test_config(), flock, on_disk, Some(factory(&named)))
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while d
            .fleet()
            .views()
            .iter()
            .any(|v| !v.healthy && !d.fleet().is_pull(&v.name))
        {
            assert!(Instant::now() < deadline, "machines never connected");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        (d, tmp)
    }

    /// `a` (a fake herdr, one slot) and `laptop`, a pull machine with
    /// `slots`.
    async fn pull_daemon(slots: u32) -> (Daemon, tempfile::TempDir) {
        let laptop = MachineConfig {
            pull: true,
            command: None,
            ..machine("laptop", slots)
        };
        let flock = Flock {
            flocks: vec![],
            machines: vec![machine("a", 1), laptop],
        };
        daemon_with_flock(flock, &[("a", 1, FakeHerdr::new())]).await
    }

    fn run_on(prompt: &str, machine: Option<&str>) -> IpcRequest {
        IpcRequest::Run {
            preempt: false,
            prompt: prompt.into(),
            spec: DispatchSpec {
                machine: machine.map(str::to_string),
                ..spec()
            },
            flock: None,
            agent: None,
            priority: None,
            role: TaskRole::Agent,
            description: None,
            summary: None,
        }
    }

    async fn queued(d: &Daemon, prompt: &str, machine: Option<&str>) -> Task {
        match d.handle(run_on(prompt, machine)).await {
            IpcResponse::Task(t) => t,
            other => panic!("{other:?}"),
        }
    }

    async fn claim(d: &Daemon, free_slots: u32, flock_work: bool) -> Vec<String> {
        let req = IpcRequest::TaskClaim {
            machine: "laptop".into(),
            free_slots,
            flock_work,
        };
        match d.handle(req).await {
            IpcResponse::Tasks(ts) => ts
                .into_iter()
                .map(|t| {
                    assert_eq!(t.state, TaskState::Starting, "{t:?}");
                    assert_eq!(t.machine.as_deref(), Some("laptop"));
                    t.prompt
                })
                .collect(),
            other => panic!("{other:?}"),
        }
    }

    fn report(machine: &str, t: &Task, state: TaskState, detail: Option<&str>) -> IpcRequest {
        IpcRequest::TaskReport {
            machine: machine.into(),
            id: t.id,
            state,
            pane: Some("p-9".into()),
            detail: detail.map(str::to_string),
        }
    }

    /// (kind, task id, machine) of each event sent so far.
    fn events_of(rx: &mut broadcast::Receiver<PastorEvent>) -> Vec<(String, Option<i64>, String)> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .map(|e| (e.kind, e.task_id, e.machine.unwrap_or_default()))
            .collect()
    }

    fn state_of(d: &Daemon, t: &Task) -> TaskState {
        d.store().get_task(t.id).unwrap().unwrap().state
    }

    /// The head runs no actor for a pull machine and never dispatches to
    /// it: tasks pinned there wait until it claims them, pinned ones first
    /// and flock work only when asked, within its free slots and its own
    /// number. Its reports land on the rows with the events an actor
    /// would send; one about another machine's task, or from a machine the
    /// head reaches itself, is refused.
    #[tokio::test]
    async fn a_pull_machine_claims_its_tasks_and_reports_them() {
        let (d, _tmp) = pull_daemon(3).await;
        let laptop = || {
            d.fleet()
                .statuses()
                .into_iter()
                .find(|s| s.name == "laptop")
                .unwrap()
        };
        assert_eq!(laptop().endpoint, crate::machine::PULL_ENDPOINT);
        assert_eq!(laptop().channel, crate::machine::ChannelState::Connecting);
        let p1 = queued(&d, "p1", Some("laptop")).await;
        let p2 = queued(&d, "p2", Some("laptop")).await;
        let on_a = queued(&d, "u1", None).await;
        let loose = queued(&d, "u2", None).await;
        assert_eq!(p1.state, TaskState::Queued);
        assert_eq!(on_a.machine.as_deref(), Some("a"));
        assert_eq!(loose.state, TaskState::Queued, "a is full");
        d.fleet().dispatch_queued().await;
        assert_eq!(state_of(&d, &p1), TaskState::Queued, "never dispatched");

        let mut rx = d.subscribe();
        assert_eq!(claim(&d, 1, false).await, ["p1"]);
        assert_eq!(laptop().channel, crate::machine::ChannelState::Connected);
        assert_eq!(claim(&d, 5, false).await, ["p2"], "no flock work unasked");
        assert_eq!(claim(&d, 5, true).await, ["u2"]);
        assert!(claim(&d, 5, true).await.is_empty());
        assert_eq!(laptop().live, 3);

        let resp = d
            .handle(report("laptop", &p1, TaskState::Running, None))
            .await;
        let IpcResponse::Task(t) = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(t.state, TaskState::Running);
        assert_eq!(t.pane_id.as_deref(), Some("p-9"));
        assert!(t.started_at.is_some());
        // The same report again changes nothing and says nothing.
        d.handle(report("laptop", &p1, TaskState::Running, None))
            .await;
        d.handle(report("laptop", &p1, TaskState::Done, None)).await;
        d.handle(report(
            "laptop",
            &p2,
            TaskState::Blocked,
            Some("agent asked: go on?"),
        ))
        .await;
        let blocked = d.store().get_task(p2.id).unwrap().unwrap();
        assert_eq!(blocked.error.as_deref(), Some("agent asked: go on?"));
        let events = events_of(&mut rx);
        let lap = |kind: &str, t: &Task| (kind.to_string(), Some(t.id), "laptop".to_string());
        assert_eq!(
            events,
            [
                lap("task.running", &p1),
                lap("task.done", &p1),
                lap("task.blocked", &p2)
            ]
        );
        assert_eq!(state_of(&d, &p1), TaskState::Done);

        assert_eq!(
            error_code(d.handle(report("a", &on_a, TaskState::Done, None)).await),
            "not_pull_machine"
        );
        assert_eq!(
            error_code(
                d.handle(report("laptop", &on_a, TaskState::Done, None))
                    .await
            ),
            "not_on_machine"
        );
        assert_eq!(
            error_code(
                d.handle(report("laptop", &p1, TaskState::Queued, None))
                    .await
            ),
            "invalid_report"
        );
        assert_eq!(
            error_code(
                d.handle(IpcRequest::TaskClaim {
                    machine: "nope".into(),
                    free_slots: 1,
                    flock_work: false,
                })
                .await
            ),
            "unknown_machine"
        );
    }

    /// Reading, typing into or removing the worktree of a pull machine's
    /// task needs that machine; `task done` and a plain close are the
    /// head's rows alone, and a report after `task done` leaves it done.
    #[tokio::test]
    async fn a_pull_machine_s_task_is_read_and_sent_to_on_that_machine() {
        let (d, _tmp) = pull_daemon(2).await;
        let t = queued(&d, "p", Some("laptop")).await;
        claim(&d, 1, false).await;
        d.handle(report("laptop", &t, TaskState::Running, None))
            .await;
        assert_eq!(
            error_code(d.handle(IpcRequest::TaskRead { id: t.id, lines: 5 }).await),
            "pull_machine_task"
        );
        let send = IpcRequest::TaskSend {
            id: t.id,
            input: SendInput {
                text: Some("hi".into()),
                ..Default::default()
            },
        };
        assert_eq!(error_code(d.handle(send).await), "pull_machine_task");
        let mut rx = d.subscribe();
        let done = IpcRequest::TaskDone {
            id: t.id,
            summary: Some("did it".into()),
        };
        let IpcResponse::Task(ended) = d.handle(done).await else {
            panic!()
        };
        assert_eq!(ended.state, TaskState::Done);
        assert!(ended.ended);
        d.handle(report("laptop", &t, TaskState::Running, None))
            .await;
        assert_eq!(state_of(&d, &t), TaskState::Done, "ended stays done");
        assert_eq!(
            events_of(&mut rx),
            [("task.done".to_string(), Some(t.id), "laptop".to_string())]
        );
        let close = |remove_worktree| IpcRequest::TaskClose {
            id: t.id,
            remove_worktree,
        };
        let mut wt = d.store().get_task(t.id).unwrap().unwrap();
        wt.spec.worktree = true;
        wt.spec.repo = Some("~/r".into());
        d.store().update_task(&mut wt).unwrap();
        assert_eq!(error_code(d.handle(close(true)).await), "pull_machine_task");
        let IpcResponse::Task(closed) = d.handle(close(false)).await else {
            panic!()
        };
        assert_eq!(closed.state, TaskState::Closed);
        d.handle(report("laptop", &t, TaskState::Done, None)).await;
        assert_eq!(state_of(&d, &t), TaskState::Closed, "closed stays closed");
    }

    /// A task on a pull machine that requires a summary is held to it as
    /// one on a herdr machine: its agent's bare `task done` is refused, and
    /// a person's ends it by hand.
    #[tokio::test]
    async fn a_pull_task_holds_its_agent_to_a_required_summary() {
        use crate::machine::EndBy;
        let (d, _tmp) = pull_daemon(2).await;
        let t = queued(&d, "p", Some("laptop")).await;
        claim(&d, 1, false).await;
        let mut row = d.store().get_task(t.id).unwrap().unwrap();
        row.spec.summary = crate::task::SummaryMode::Require;
        d.store().update_task(&mut row).unwrap();
        let err = d
            .fleet()
            .end_pull(row.clone(), None, EndBy::Agent)
            .unwrap_err();
        let err = err.downcast_ref::<crate::cli::CliError>().unwrap();
        assert_eq!(err.code, crate::task::SUMMARY_REQUIRED);
        assert!(!d.store().get_task(t.id).unwrap().unwrap().ended);
        let ended = d.fleet().end_pull(row, None, EndBy::Hand).unwrap();
        assert!(ended.ended);
        let round = d.store().get_task(t.id).unwrap().unwrap().summary.unwrap();
        assert_eq!(round.text, crate::task::ENDED_BY_HAND);
    }

    /// A pull machine that neither claims nor reports for `after` is lost
    /// once: its starting and running tasks go stale, the ones pinned to it
    /// stay queued, and its next claim brings it back. A report of work on
    /// a stale task leaves it stale; one of its end does not.
    #[tokio::test]
    async fn a_silent_pull_machine_is_lost_and_its_tasks_go_stale() {
        let (d, _tmp) = pull_daemon(2).await;
        let fleet = d.fleet();
        let after = Duration::from_millis(300);
        let running = queued(&d, "r", Some("laptop")).await;
        let starting = queued(&d, "s", Some("laptop")).await;
        claim(&d, 2, false).await;
        d.handle(report("laptop", &running, TaskState::Running, None))
            .await;
        let waiting = queued(&d, "w", Some("laptop")).await;
        let mut rx = d.subscribe();
        assert!(fleet.check_pull_lost(after).await.is_empty(), "just heard");
        tokio::time::sleep(after).await;
        assert_eq!(fleet.check_pull_lost(after).await, ["laptop"]);
        assert!(fleet.check_pull_lost(after).await.is_empty(), "lost once");
        assert_eq!(state_of(&d, &running), TaskState::Stale);
        assert_eq!(state_of(&d, &starting), TaskState::Stale);
        assert_eq!(state_of(&d, &waiting), TaskState::Queued);
        let status = fleet
            .statuses()
            .into_iter()
            .find(|s| s.name == "laptop")
            .unwrap();
        assert_eq!(status.channel, crate::machine::ChannelState::Reconnecting);
        assert!(status.error.unwrap().contains("has not claimed"));
        let mut events = events_of(&mut rx);
        events[1..].sort();
        let lap = |kind: &str, id: Option<i64>| (kind.to_string(), id, "laptop".to_string());
        assert_eq!(
            events,
            [
                lap("machine.lost", None),
                lap("task.stale", Some(running.id)),
                lap("task.stale", Some(starting.id)),
            ]
        );

        // Two slots, both stale tasks still hold theirs.
        assert!(claim(&d, 2, false).await.is_empty());
        assert_eq!(events_of(&mut rx), [lap("machine.connected", None)]);
        d.handle(report("laptop", &running, TaskState::Running, None))
            .await;
        assert_eq!(state_of(&d, &running), TaskState::Stale);
        d.handle(report("laptop", &running, TaskState::Done, None))
            .await;
        assert_eq!(state_of(&d, &running), TaskState::Done);
    }

    /// Reports carry no sequence, so a late one must not reopen a failed
    /// task or move one `task done` ended anywhere but closed.
    #[tokio::test]
    async fn a_late_report_leaves_failed_and_ended_tasks_as_they_are() {
        let (d, _tmp) = pull_daemon(2).await;
        let failed = queued(&d, "f", Some("laptop")).await;
        let ended = queued(&d, "e", Some("laptop")).await;
        claim(&d, 2, false).await;
        d.handle(report("laptop", &failed, TaskState::Failed, Some("boom")))
            .await;
        for late in [TaskState::Running, TaskState::Blocked, TaskState::Done] {
            d.handle(report("laptop", &failed, late, None)).await;
            assert_eq!(state_of(&d, &failed), TaskState::Failed, "{late}");
        }
        d.handle(IpcRequest::TaskDone {
            id: ended.id,
            summary: None,
        })
        .await;
        for late in [TaskState::Failed, TaskState::Stale, TaskState::Running] {
            d.handle(report("laptop", &ended, late, None)).await;
            assert_eq!(state_of(&d, &ended), TaskState::Done, "{late}");
        }
        d.handle(report("laptop", &ended, TaskState::Closed, None))
            .await;
        assert_eq!(state_of(&d, &ended), TaskState::Closed);
    }

    /// `pull = true` is a way to reach a machine, like `ssh`, `local` and
    /// `command`: exactly one of them.
    #[test]
    fn a_pull_machine_has_no_other_way_in() {
        let f: Flock = toml::from_str("[[machine]]\nname = \"laptop\"\npull = true\n").unwrap();
        f.validate().unwrap();
        assert!(f.machines[0].pull);
        let f: Flock =
            toml::from_str("[[machine]]\nname = \"laptop\"\npull = true\nssh = \"user@pi-1\"\n")
                .unwrap();
        assert_eq!(
            f.validate().unwrap_err(),
            "machine laptop: set exactly one of local, ssh, command, pull"
        );
    }

    /// `JobSubmit` queues another machine's items as this head's own job
    /// would: rendered the same, deduplicated by `seen`, capped by
    /// `max_tasks_per_run`, and refused for a name a job file owns or a
    /// `[dispatch]` that does not validate.
    #[tokio::test]
    async fn job_submit_queues_items_like_a_local_job() {
        let (d, _tmp) = daemon(&[("a", 1, FakeHerdr::new())]).await;
        let dispatch = serde_json::json!({
            "repo": "~/work/{{ item.repo }}",
            "branch": "pastor/{{ item.key }}",
            "max_tasks_per_run": 2,
        });
        let prompt = "{{ job.name }} {{ task.id }}: {{ item.title }}";
        let submit = |job: &str, dispatch: &serde_json::Value, items: Vec<serde_json::Value>| {
            IpcRequest::JobSubmit {
                job: job.into(),
                dispatch: dispatch.clone(),
                prompt: prompt.into(),
                items,
            }
        };
        let item = |key: &str| serde_json::json!({"key": key, "repo": "r", "title": "fix it"});
        let resp = d
            .handle(submit(
                "vault",
                &dispatch,
                vec![
                    item("c1"),
                    item("c1"),
                    serde_json::json!({"key": "bad", "repo": "..", "title": "x"}),
                    item("c2"),
                    item("c3"),
                ],
            ))
            .await;
        let IpcResponse::JobSubmitted {
            tasks,
            skipped,
            refused,
        } = resp
        else {
            panic!("{resp:?}")
        };
        assert_eq!(tasks.len(), 2, "{tasks:?}");
        assert_eq!(skipped, vec!["c1"]);
        assert_eq!(refused.len(), 2, "{refused:?}");
        assert_eq!(refused[0].0, "bad");
        assert!(refused[0].1.contains("repo"), "{refused:?}");
        assert_eq!(
            refused[1],
            ("c3".to_string(), "max_tasks_per_run".to_string())
        );

        // Rendered exactly as a job file with the same [dispatch] would be.
        let file = format!(
            "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nrepo = \"~/work/{{{{ item.repo }}}}\"\nbranch = \"pastor/{{{{ item.key }}}}\"\nprompt = \"{prompt}\"\n"
        );
        let local = crate::config::job::Job::parse(
            &file,
            "vault",
            &test_config().defaults,
            &crate::connector::Builtins,
        )
        .unwrap();
        let t = &tasks[0];
        assert_eq!(t.job, "vault");
        let (want_prompt, want_spec) =
            crate::scheduler::render_task(&local, &item("c1"), t.id).unwrap();
        assert_eq!(t.prompt, want_prompt);
        assert_eq!(t.prompt, format!("vault t-{}: fix it", t.id));
        assert_eq!(t.spec.repo, want_spec.repo);
        assert_eq!(t.spec.branch.as_deref(), Some("pastor/c1"));
        assert!(d.store.is_seen("vault", "c1").unwrap());

        // A second submitter of the same job shares its seen keys.
        let resp = d
            .handle(submit("vault", &dispatch, vec![item("c1"), item("c3")]))
            .await;
        let IpcResponse::JobSubmitted {
            tasks,
            skipped,
            refused,
        } = resp
        else {
            panic!("{resp:?}")
        };
        assert_eq!(skipped, vec!["c1"]);
        assert!(refused.is_empty(), "{refused:?}");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].item["key"], "c3");

        // A job file owns its name, even an invalid one.
        let jobs = d.paths.jobs_dir();
        std::fs::create_dir_all(&jobs).unwrap();
        std::fs::write(jobs.join("mine.toml"), &file).unwrap();
        std::fs::write(jobs.join("broken.toml"), "not toml [").unwrap();
        for name in ["mine", "broken"] {
            let resp = d.handle(submit(name, &dispatch, vec![item("x")])).await;
            let IpcResponse::Error { code, .. } = resp else {
                panic!("{resp:?}")
            };
            assert_eq!(code, "job_name_taken", "{name}");
        }
        assert!(!d.store.is_seen("mine", "x").unwrap());

        // A [dispatch] the job file would refuse, with the job file's error.
        let resp = d
            .handle(submit(
                "other",
                &serde_json::json!({"worktree": true}),
                vec![item("x")],
            ))
            .await;
        let IpcResponse::Error { code, message } = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(code, "invalid_dispatch");
        assert_eq!(message, "dispatch.worktree = true needs dispatch.repo");
        assert!(!d.store.is_seen("other", "x").unwrap());
    }

    #[tokio::test]
    async fn trust_is_listed_added_and_removed_on_the_head() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let add = || IpcRequest::TrustAdd {
            machine: "a".into(),
            repo: "/r".into(),
        };
        let resp = d.handle(add()).await;
        assert!(
            matches!(&resp, IpcResponse::Text(t) if t == "/r on a is trusted"),
            "{resp:?}"
        );
        let resp = d.handle(add()).await;
        assert!(
            matches!(&resp, IpcResponse::Text(t) if t.contains("already")),
            "{resp:?}"
        );
        let IpcResponse::Trusted(list) = d.handle(IpcRequest::TrustList).await else {
            panic!()
        };
        assert_eq!(list.len(), 1);
        assert!(d.store.is_trusted("a", "/r").unwrap());
        let remove = || IpcRequest::TrustRemove {
            machine: "a".into(),
            repo: "/r".into(),
        };
        assert!(matches!(d.handle(remove()).await, IpcResponse::Text(_)));
        let resp = d.handle(remove()).await;
        assert!(
            matches!(&resp, IpcResponse::Error { code, .. } if code == "not_trusted"),
            "{resp:?}"
        );
        let IpcResponse::Trusted(list) = d.handle(IpcRequest::TrustList).await else {
            panic!()
        };
        assert!(list.is_empty());
    }

    /// The head describes from the flock it applied, not from flock.toml as
    /// it reads now: a file that does not load leaves the last one in use.
    #[tokio::test]
    async fn describe_answers_from_the_applied_flock() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        std::fs::write(d.paths.flock_file(), "not [[ toml").unwrap();
        let resp = d
            .handle(IpcRequest::MachineDescribe { name: "a".into() })
            .await;
        let IpcResponse::MachineDescription(m) = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(m.row.name, "a");
        assert_eq!(m.row.channel, "connected");
        assert_eq!(m.row.live, Some(0));
        let resp = d
            .handle(IpcRequest::MachineDescribe { name: "b".into() })
            .await;
        assert!(
            matches!(&resp, IpcResponse::Error { code, .. } if code == "unknown_machine"),
            "{resp:?}"
        );
        let resp = d
            .handle(IpcRequest::FlockDescribe {
                name: "default".into(),
            })
            .await;
        let IpcResponse::FlockDescription(f) = resp else {
            panic!("{resp:?}")
        };
        assert!(f.default);
        assert_eq!(f.machines, ["a"]);
        assert_eq!(f.agents, Some(0));
        let resp = d
            .handle(IpcRequest::FlockDescribe {
                name: "nope".into(),
            })
            .await;
        assert!(
            matches!(&resp, IpcResponse::Error { code, .. } if code == "unknown_flock"),
            "{resp:?}"
        );
    }

    #[tokio::test]
    async fn run_dispatches_immediately() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let mut events = d.subscribe();
        let resp = d
            .handle(IpcRequest::Run {
                preempt: false,
                summary: None,
                role: Default::default(),
                description: None,
                prompt: "hi".into(),
                spec: spec(),
                flock: None,
                agent: None,
                priority: None,
            })
            .await;
        let IpcResponse::Task(t) = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(t.state, TaskState::Running);
        // Queued before dispatch picked it up.
        let ev = events.try_recv().expect("task.queued emitted");
        assert_eq!(ev.kind, "task.queued");
        assert_eq!(ev.task_id, Some(t.id));
        assert_eq!(ev.job.as_deref(), Some("run"));
        assert_eq!(events.try_recv().unwrap().kind, "task.running");
        assert_eq!(t.machine.as_deref(), Some("a"));
        let IpcResponse::Tasks(list) = d
            .handle(IpcRequest::List {
                filter: TaskFilter::default(),
            })
            .await
        else {
            panic!()
        };
        assert_eq!(list.len(), 1);
        let IpcResponse::Text(text) = d.handle(IpcRequest::TaskRead { id: t.id, lines: 5 }).await
        else {
            panic!()
        };
        assert!(text.contains("fake output"));
        let IpcResponse::Machines(ms) = d.handle(IpcRequest::FlockList).await else {
            panic!()
        };
        assert_eq!(ms[0].live, 1);
        assert_eq!(
            ms[0].pastor_version.as_deref(),
            Some("fake"),
            "the flock list carries what the machine answered on connect"
        );
    }

    #[tokio::test]
    async fn queued_task_dispatches_when_capacity_frees() {
        let fake = FakeHerdr::new();
        let (d, _tmp) = daemon(&[("a", 1, fake.clone())]).await;
        let IpcResponse::Task(first) = d
            .handle(IpcRequest::Run {
                preempt: false,
                summary: None,
                role: Default::default(),
                description: None,
                prompt: "1".into(),
                spec: spec(),
                flock: None,
                agent: None,
                priority: None,
            })
            .await
        else {
            panic!()
        };
        assert_eq!(first.state, TaskState::Running);
        let IpcResponse::Task(second) = d
            .handle(IpcRequest::Run {
                preempt: false,
                summary: None,
                role: Default::default(),
                description: None,
                prompt: "2".into(),
                spec: spec(),
                flock: None,
                agent: None,
                priority: None,
            })
            .await
        else {
            panic!()
        };
        assert_eq!(second.state, TaskState::Queued);
        d.fleet().dispatch_queued().await;
        assert_eq!(
            d.store.get_task(second.id).unwrap().unwrap().state,
            TaskState::Queued,
            "still no room"
        );
        fake.close_pane(first.pane_id.as_deref().unwrap());
        let deadline = Instant::now() + Duration::from_secs(5);
        while d.store.get_task(first.id).unwrap().unwrap().state != TaskState::Closed {
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        d.fleet().dispatch_queued().await;
        assert_eq!(
            d.store.get_task(second.id).unwrap().unwrap().state,
            TaskState::Running
        );
    }

    fn run_at(prompt: &str, priority: Option<Priority>) -> IpcRequest {
        IpcRequest::Run {
            preempt: false,
            summary: None,
            prompt: prompt.into(),
            spec: spec(),
            flock: None,
            agent: None,
            priority,
            role: TaskRole::Agent,
            description: None,
        }
    }

    /// When a slot frees, the queued task of the highest level takes it,
    /// whatever its age; `task priority` moves a queued task, and refuses
    /// one that left the queue, one that is not there and an agent pastor
    /// started.
    #[tokio::test]
    async fn the_highest_level_takes_a_freed_slot() {
        let fake = FakeHerdr::new();
        let (d, _tmp) = daemon(&[("a", 1, fake.clone())]).await;
        let task = |resp: IpcResponse| match resp {
            IpcResponse::Task(t) => t,
            other => panic!("{other:?}"),
        };
        let first = task(d.handle(run_at("1", None)).await);
        assert_eq!(first.state, TaskState::Running);
        let normal = task(d.handle(run_at("2", None)).await);
        let high = task(d.handle(run_at("3", Some(Priority::High))).await);
        let low = task(d.handle(run_at("4", Some(Priority::Low))).await);
        assert_eq!(normal.priority, Priority::Normal);
        assert_eq!(normal.priority_from, None);
        assert_eq!(high.priority, Priority::High);
        assert_eq!(high.priority_from.as_deref(), Some("task run"));

        let set = |id: i64, priority: Priority| IpcRequest::TaskPriority {
            preempt: false,
            id,
            priority,
        };
        let raised = task(d.handle(set(low.id, Priority::Critical)).await);
        assert_eq!(raised.priority, Priority::Critical);
        assert_eq!(raised.priority_from.as_deref(), Some("task priority"));
        assert_eq!(
            error_code(d.handle(set(first.id, Priority::Low)).await),
            "not_queued"
        );
        assert_eq!(
            error_code(d.handle(set(999, Priority::Low)).await),
            "task_not_found"
        );
        assert_eq!(
            error_code(
                d.handle_from(set(normal.id, Priority::Low), Some("t-1"))
                    .await
            ),
            "agent_refused"
        );

        fake.close_pane(first.pane_id.as_deref().unwrap());
        let deadline = Instant::now() + Duration::from_secs(5);
        while d.store.get_task(first.id).unwrap().unwrap().state != TaskState::Closed {
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        d.fleet().dispatch_queued().await;
        let state = |id: i64| d.store.get_task(id).unwrap().unwrap().state;
        assert_eq!(state(low.id), TaskState::Running);
        assert_eq!(state(high.id), TaskState::Queued);
        assert_eq!(state(normal.id), TaskState::Queued);
    }

    /// A task's level comes from `--priority`, else the machine it is
    /// pinned to, else its flock, else `[defaults]`; an unpinned task never
    /// takes a machine's. A job's comes from its template, and an empty
    /// value falls through; a value that is not a level is the item's error.
    #[tokio::test]
    async fn a_tasks_priority_comes_from_its_layers() {
        use crate::config::flock::FlockEntry;
        let mut urgent = machine("a", 4);
        urgent.flock = Some("work".into());
        urgent.priority = Some(Priority::Critical);
        let mut plain = machine("b", 4);
        plain.flock = Some("work".into());
        let flock = Flock {
            flocks: vec![FlockEntry {
                name: "work".into(),
                default: true,
                priority: Some(Priority::High),
                ..Default::default()
            }],
            machines: vec![urgent, plain],
        };
        let (d, _tmp) = daemon_with_flock(
            flock,
            &[("a", 4, FakeHerdr::new()), ("b", 4, FakeHerdr::new())],
        )
        .await;
        let run = |machine: Option<&str>, priority: Option<Priority>| IpcRequest::Run {
            preempt: false,
            summary: None,
            prompt: "x".into(),
            spec: DispatchSpec {
                machine: machine.map(Into::into),
                ..spec()
            },
            flock: None,
            agent: None,
            priority,
            role: TaskRole::Agent,
            description: None,
        };
        let level = |resp: IpcResponse| match resp {
            IpcResponse::Task(t) => (t.priority, t.priority_from.unwrap_or_default()),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            level(d.handle(run(Some("a"), None)).await),
            (Priority::Critical, "machine a".into())
        );
        assert_eq!(
            level(d.handle(run(Some("a"), Some(Priority::Low))).await),
            (Priority::Low, "task run".into())
        );
        assert_eq!(
            level(d.handle(run(Some("b"), None)).await),
            (Priority::High, "flock work".into())
        );
        assert_eq!(
            level(d.handle(run(None, None)).await),
            (Priority::High, "flock work".into())
        );

        let config = test_config();
        let text = "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\npriority = \"{{ item.priority }}\"\nprompt = \"p\"\n";
        let job = crate::config::job::Job::parse(
            text,
            "j",
            &config.defaults,
            &crate::connector::Builtins,
        )
        .unwrap();
        let queue = |item: serde_json::Value| {
            let job = job.clone();
            let fleet = d.fleet().clone();
            async move {
                fleet
                    .queue_job_task(&job, &item, |_| Ok(("p".into(), job.spec.clone())))
                    .await
            }
        };
        let t = queue(serde_json::json!({"key": "a", "priority": "low"}))
            .await
            .unwrap();
        assert_eq!(t.priority, Priority::Low);
        assert_eq!(t.priority_from.as_deref(), Some("job j"));
        let t = queue(serde_json::json!({"key": "b", "priority": ""}))
            .await
            .unwrap();
        assert_eq!(t.priority, Priority::High);
        assert_eq!(t.priority_from.as_deref(), Some("flock work"));
        let err = queue(serde_json::json!({"key": "c", "priority": "urgent"}))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("unknown_priority"), "{err:#}");
    }

    /// A task's workspace label comes from `--label`, else its job's, else
    /// its flock's, else `[defaults]`, settled when it is queued; dispatch
    /// renders it on the machine, and the agent stays `t-N`. A retry keeps
    /// the template and where it came from.
    #[tokio::test]
    async fn a_tasks_label_comes_from_its_layers() {
        use crate::config::flock::FlockEntry;
        let mut a = machine("a", 4);
        a.flock = Some("work".into());
        let mut b = machine("b", 4);
        b.flock = Some("bare".into());
        let flock = Flock {
            flocks: vec![
                FlockEntry {
                    name: "work".into(),
                    default: true,
                    label: Some("w/{{ task.id }}".into()),
                    ..Default::default()
                },
                FlockEntry {
                    name: "bare".into(),
                    ..Default::default()
                },
            ],
            machines: vec![a, b],
        };
        let fake_a = FakeHerdr::new();
        let (d, _tmp) = daemon_with_flock(
            flock,
            &[("a", 4, fake_a.clone()), ("b", 4, FakeHerdr::new())],
        )
        .await;
        let mut config = test_config();
        config.defaults.label = Some("{{ machine }}/{{ task.id }}".into());
        d.fleet().set_config(&config);
        let run = |label: Option<&str>, flock: &str| IpcRequest::Run {
            prompt: "x".into(),
            spec: DispatchSpec {
                label: crate::task::WorkspaceLabel {
                    template: label.map(Into::into),
                    ..Default::default()
                },
                ..spec()
            },
            flock: Some(flock.into()),
            agent: None,
            priority: None,
            role: TaskRole::Agent,
            description: None,
            preempt: false,
            summary: None,
        };
        let task = |resp: IpcResponse| match resp {
            IpcResponse::Task(t) => t,
            other => panic!("{other:?}"),
        };
        let label = |t: &Task| {
            (
                t.spec.label.template.clone().unwrap_or_default(),
                t.spec.label.from.clone().unwrap_or_default(),
                t.spec.label.name.clone().unwrap_or_default(),
            )
        };
        let asked = task(d.handle(run(Some("mine-{{ task.id }}"), "work")).await);
        let id = asked.display_id();
        assert_eq!(
            label(&asked),
            (
                "mine-{{ task.id }}".into(),
                "task run".into(),
                format!("mine-{id}")
            )
        );
        assert_eq!(asked.agent_name.as_deref(), Some(id.as_str()));
        let from_flock = task(d.handle(run(None, "work")).await);
        assert_eq!(
            label(&from_flock),
            (
                "w/{{ task.id }}".into(),
                "flock work".into(),
                format!("w/{}", from_flock.display_id())
            )
        );
        let from_defaults = task(d.handle(run(None, "bare")).await);
        assert_eq!(
            label(&from_defaults),
            (
                "{{ machine }}/{{ task.id }}".into(),
                "defaults".into(),
                format!("b/{}", from_defaults.display_id())
            )
        );
        let created: Vec<String> = fake_a
            .requests()
            .into_iter()
            .filter(|r| r.method == "workspace.create")
            .map(|r| r.params["label"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            created,
            vec![
                format!("mine-{id}"),
                format!("w/{}", from_flock.display_id())
            ]
        );

        let text = |extra: &str| {
            format!(
                "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"p\"\n{extra}"
            )
        };
        let parse = |extra: &str| {
            crate::config::job::Job::parse(
                &text(extra),
                "j",
                &config.defaults,
                &crate::connector::Builtins,
            )
            .unwrap()
        };
        for (job, want) in [
            (
                parse("label = \"{{ job }}/{{ item.key }}\"\n"),
                ("{{ job }}/{{ item.key }}", "job j"),
            ),
            (parse(""), ("w/{{ task.id }}", "flock work")),
        ] {
            let item = serde_json::json!({"key": want.1.replace(' ', "-")});
            let t = d
                .fleet()
                .queue_job_task(&job, &item, |_| Ok(("p".into(), job.spec.clone())))
                .await
                .unwrap();
            assert_eq!(t.spec.label.template.as_deref(), Some(want.0));
            assert_eq!(t.spec.label.from.as_deref(), Some(want.1));
        }

        let mut failed = asked.clone();
        failed.state = TaskState::Failed;
        failed.finished_at = Some(chrono::Utc::now());
        d.store.update_task(&mut failed).unwrap();
        let copy = task(
            d.handle(IpcRequest::TaskRetry {
                id: failed.id,
                place: None,
            })
            .await,
        );
        assert_eq!(
            copy.spec.label.template.as_deref(),
            Some("mine-{{ task.id }}")
        );
        assert_eq!(copy.spec.label.from.as_deref(), Some("task run"));
        assert_eq!(
            copy.spec.label.name,
            Some(format!("mine-{}", copy.display_id())),
            "rendered again for the copy"
        );
    }

    /// A retry keeps the level of the task it copies, and where it came
    /// from.
    #[tokio::test]
    async fn a_retry_keeps_its_priority() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let IpcResponse::Task(mut t) = d.handle(run_at("x", Some(Priority::High))).await else {
            panic!()
        };
        t.state = TaskState::Failed;
        t.finished_at = Some(chrono::Utc::now());
        d.store.update_task(&mut t).unwrap();
        let IpcResponse::Task(copy) = d
            .handle(IpcRequest::TaskRetry {
                id: t.id,
                place: None,
            })
            .await
        else {
            panic!()
        };
        assert_eq!(copy.retry_of, Some(t.id));
        assert_eq!(copy.priority, Priority::High);
        assert_eq!(copy.priority_from.as_deref(), Some("task run"));
    }

    #[tokio::test]
    async fn pinned_unknown_machine_and_bad_ids_are_errors() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let resp = d
            .handle(IpcRequest::Run {
                preempt: false,
                summary: None,
                role: Default::default(),
                description: None,
                prompt: "x".into(),
                spec: DispatchSpec {
                    machine: Some("zzz".into()),
                    ..spec()
                },
                flock: None,
                agent: None,
                priority: None,
            })
            .await;
        let IpcResponse::Error { code, .. } = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(code, "unknown_machine");
        let IpcResponse::Error { code, .. } = d.handle(IpcRequest::TaskShow { id: 99 }).await
        else {
            panic!()
        };
        assert_eq!(code, "task_not_found");
    }

    /// A valid pin still queues, and dispatches to that machine.
    #[tokio::test]
    async fn run_pinned_to_a_known_machine_queues() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new()), ("b", 2, FakeHerdr::new())]).await;
        let resp = d
            .handle(IpcRequest::Run {
                preempt: false,
                summary: None,
                role: Default::default(),
                description: None,
                prompt: "x".into(),
                spec: DispatchSpec {
                    machine: Some("b".into()),
                    ..spec()
                },
                flock: None,
                agent: None,
                priority: None,
            })
            .await;
        let IpcResponse::Task(t) = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(t.machine.as_deref(), Some("b"));
        assert_eq!(t.state, TaskState::Running);
    }

    /// `home` (the default) holds `h`, `work` holds `w`.
    fn home_and_work() -> Flock {
        use crate::config::flock::FlockEntry;
        Flock {
            flocks: vec![
                FlockEntry {
                    name: "home".into(),
                    default: true,
                    ..Default::default()
                },
                FlockEntry {
                    name: "work".into(),
                    default: false,
                    ..Default::default()
                },
            ],
            machines: vec![
                machine("h", 2),
                MachineConfig {
                    flock: Some("work".into()),
                    ..machine("w", 2)
                },
            ],
        }
    }

    async fn flocked_daemon() -> (Daemon, tempfile::TempDir) {
        daemon_with_flock(
            home_and_work(),
            &[("h", 2, FakeHerdr::new()), ("w", 2, FakeHerdr::new())],
        )
        .await
    }

    fn run_in(flock: Option<&str>, machine: Option<&str>) -> IpcRequest {
        IpcRequest::Run {
            preempt: false,
            summary: None,
            role: Default::default(),
            description: None,
            prompt: "x".into(),
            spec: DispatchSpec {
                machine: machine.map(Into::into),
                ..spec()
            },
            flock: flock.map(Into::into),
            agent: None,
            priority: None,
        }
    }

    /// A run or job task that names no agent takes its flock's, then
    /// `[defaults]`; one that names its own keeps it; a client from before
    /// flock agents (`agent: None`) keeps the spec it sent.
    #[tokio::test]
    async fn queued_tasks_take_their_flocks_agent_unless_they_name_one() {
        let mut flock = home_and_work();
        flock.flocks[1].agent = Some("codex".into());
        flock.flocks[1].agent_args = Some(vec!["--model".into(), "gpt-x".into()]);
        let (d, _tmp) = daemon_with_flock(
            flock,
            &[("h", 2, FakeHerdr::new()), ("w", 2, FakeHerdr::new())],
        )
        .await;
        let run = |flock: &str, agent: Option<AgentChoice>| IpcRequest::Run {
            preempt: false,
            summary: None,
            role: Default::default(),
            description: None,
            prompt: "x".into(),
            spec: spec(),
            flock: Some(flock.into()),
            agent,
            priority: None,
        };
        let queued = |resp: IpcResponse| match resp {
            IpcResponse::Task(t) => (t.spec.agent, t.spec.agent_args.join(" ")),
            other => panic!("{other:?}"),
        };

        let none = Some(AgentChoice::default());
        assert_eq!(
            queued(d.handle(run("work", none.clone())).await),
            ("codex".into(), "--model gpt-x".into())
        );
        assert_eq!(
            queued(d.handle(run("home", none)).await),
            ("claude".into(), String::new())
        );
        let own = Some(AgentChoice {
            agent: Some("aider".into()),
            agent_args: None,
            allow: vec![],
            deny: vec![],
            model: None,
            profile: None,
        });
        assert_eq!(
            queued(d.handle(run("work", own)).await),
            ("aider".into(), String::new())
        );
        assert_eq!(
            queued(d.handle(run("work", None)).await),
            ("claude".into(), String::new())
        );

        let job = |extra: &str| {
            let text = format!(
                "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nflock = \"work\"\nprompt = \"p\"\n{extra}"
            );
            crate::config::job::Job::parse(
                &text,
                "j",
                &test_config().defaults,
                &crate::connector::Builtins,
            )
            .unwrap()
        };
        let item = serde_json::json!({"key": "k"});
        let bare = job("");
        let t = d
            .fleet()
            .queue_job_task(&bare, &item, |_| Ok(("p".into(), bare.spec.clone())))
            .await
            .unwrap();
        assert_eq!(
            (t.spec.agent.as_str(), t.spec.agent_args.len()),
            ("codex", 2)
        );
        let own = job("agent = \"claude\"\nagent_args = []\n");
        let t = d
            .fleet()
            .queue_job_task(&own, &serde_json::json!({"key": "k2"}), |_| {
                Ok(("p".into(), own.spec.clone()))
            })
            .await
            .unwrap();
        assert_eq!(
            (t.spec.agent.as_str(), t.spec.agent_args.len()),
            ("claude", 0)
        );
    }

    /// A flock's deny list reaches its tasks on top of `[defaults]`, and a
    /// task whose agent has no flag for its tool lists is refused, run or
    /// job, rather than queued to start without them.
    #[tokio::test]
    async fn tool_lists_reach_the_task_or_refuse_it() {
        let mut flock = home_and_work();
        flock.flocks[1].deny = vec!["WebFetch".into()];
        let (d, _tmp) = daemon_with_flock(
            flock,
            &[("h", 2, FakeHerdr::new()), ("w", 2, FakeHerdr::new())],
        )
        .await;
        let run = |agent: Option<&str>| IpcRequest::Run {
            preempt: false,
            summary: None,
            role: Default::default(),
            description: None,
            prompt: "x".into(),
            spec: spec(),
            flock: Some("work".into()),
            agent: Some(AgentChoice {
                agent: agent.map(Into::into),
                allow: vec!["Edit".into()],
                ..Default::default()
            }),
            priority: None,
        };
        let IpcResponse::Task(t) = d.handle(run(None)).await else {
            panic!()
        };
        assert_eq!(
            (t.spec.allow.clone(), t.spec.deny.clone()),
            (vec!["Edit".into()], vec!["WebFetch".into()])
        );
        match d.handle(run(Some("codex"))).await {
            IpcResponse::Error { code, message } => {
                assert_eq!(code, "agent_tools_unsupported", "{message}");
                assert!(message.contains("[agents.codex] allow_flag"), "{message}");
            }
            other => panic!("{other:?}"),
        }
        let text = "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nflock = \"work\"\nagent = \"codex\"\nprompt = \"p\"\n";
        let job = crate::config::job::Job::parse(
            text,
            "j",
            &test_config().defaults,
            &crate::connector::Builtins,
        )
        .unwrap();
        let err = d
            .fleet()
            .queue_job_task(&job, &serde_json::json!({"key": "k"}), |_| {
                Ok(("p".into(), job.spec.clone()))
            })
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("deny_flag"), "{err:#}");
    }

    /// A flock's default agent can name an `[agents]` definition: its tasks
    /// keep that name, and Claude's tool flags follow its kind, so a deny
    /// list does not refuse it.
    #[tokio::test]
    async fn a_flock_can_run_an_agent_definition() {
        let mut flock = home_and_work();
        flock.flocks[1].agent = Some("claude-personal".into());
        flock.flocks[1].deny = vec!["WebFetch".into()];
        let (d, _tmp) = daemon_with_flock(
            flock,
            &[("h", 2, FakeHerdr::new()), ("w", 2, FakeHerdr::new())],
        )
        .await;
        let mut config = test_config();
        config.agents.0.insert(
            "claude-personal".into(),
            crate::config::AgentDef {
                kind: Some("claude".into()),
                ..Default::default()
            },
        );
        // `task run` reads pastor.toml afresh, as the CLI's edit would land.
        std::fs::write(d.paths.config_file(), toml::to_string(&config).unwrap()).unwrap();
        let resp = d
            .handle(IpcRequest::Run {
                preempt: false,
                summary: None,
                role: Default::default(),
                description: None,
                prompt: "x".into(),
                spec: spec(),
                flock: Some("work".into()),
                agent: Some(AgentChoice::default()),
                priority: None,
            })
            .await;
        let IpcResponse::Task(t) = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(t.spec.agent, "claude-personal");
        assert_eq!(t.spec.deny, vec!["WebFetch"]);
    }

    /// pastor.toml with `sonnet` and `opus` for claude and `gpt` for codex,
    /// and `claude-personal`, a claude.
    fn models_config() -> PastorConfig {
        let mut c = test_config();
        c.agents.0.insert(
            "claude-personal".into(),
            crate::config::AgentDef {
                kind: Some("claude".into()),
                ..Default::default()
            },
        );
        for (name, kind, arg) in [
            ("sonnet", "claude", "claude-sonnet-5"),
            ("opus", "claude", "claude-opus-5-5"),
            ("gpt", "codex", "gpt-x"),
        ] {
            c.models.0.insert(
                name.into(),
                crate::config::ModelDef {
                    kind: kind.into(),
                    args: vec!["--model".into(), arg.into()],
                },
            );
        }
        c
    }

    /// A daemon over `machines` in the flock `personal`, which runs
    /// `claude-personal` with `-v` and the model `flock_model`, and
    /// `models_config` on disk: `task run` re-reads both files.
    async fn models_daemon(
        flock_model: Option<&str>,
        machines: Vec<MachineConfig>,
        fakes: &[(&str, u32, FakeHerdr)],
    ) -> (Daemon, tempfile::TempDir) {
        use crate::config::flock::FlockEntry;
        let flock = Flock {
            flocks: vec![FlockEntry {
                name: "personal".into(),
                default: true,
                agent: Some("claude-personal".into()),
                agent_args: Some(vec!["-v".into()]),
                ..Default::default()
            }],
            machines,
        };
        let (d, tmp) = daemon_with_flock(flock.clone(), fakes).await;
        std::fs::write(
            d.paths.config_file(),
            toml::to_string(&models_config()).unwrap(),
        )
        .unwrap();
        let mut flock = flock;
        flock.flocks[0].model = flock_model.map(Into::into);
        flock.save(&d.paths.flock_file()).unwrap();
        (d, tmp)
    }

    fn run_model(model: Option<&str>, agent: Option<&str>, machine: Option<&str>) -> IpcRequest {
        IpcRequest::Run {
            preempt: false,
            summary: None,
            role: Default::default(),
            description: None,
            prompt: "x".into(),
            spec: DispatchSpec {
                machine: machine.map(Into::into),
                ..spec()
            },
            flock: None,
            agent: Some(AgentChoice {
                agent: agent.map(Into::into),
                model: model.map(Into::into),
                ..Default::default()
            }),
            priority: None,
        }
    }

    /// `--model` puts the model's args before the agent's own, herdr starts
    /// the agent with them, and the task keeps the name and where it came
    /// from.
    #[tokio::test]
    async fn a_task_runs_the_model_it_names() {
        let fake = FakeHerdr::new();
        let (d, _tmp) =
            models_daemon(None, vec![machine("pi", 2)], &[("pi", 2, fake.clone())]).await;
        let resp = d.handle(run_model(Some("sonnet"), None, None)).await;
        let IpcResponse::Task(t) = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(t.spec.agent, "claude-personal");
        assert_eq!(t.spec.agent_args, vec!["--model", "claude-sonnet-5", "-v"]);
        assert_eq!(t.model(), Some("sonnet"));
        let source = t.spec.agent_source.clone().unwrap();
        assert_eq!(source.model_from.as_deref(), Some("task run"));
        let reqs = fake.requests();
        let start = reqs.iter().find(|r| r.method == "agent.start").unwrap();
        assert_eq!(start.params["kind"], "claude");
        let args = start.params["args"].as_array().unwrap();
        assert_eq!(
            args[..3],
            serde_json::json!(["--model", "claude-sonnet-5", "-v"])
                .as_array()
                .unwrap()[..]
        );
        assert_eq!(args[3], "--session-id");
        let text = crate::cli::task_detail(&t);
        assert!(
            text.contains("model:      sonnet (from task run)"),
            "{text}"
        );
    }

    /// A flock's model reaches its tasks that name none; `--model` wins.
    #[tokio::test]
    async fn a_flocks_model_runs_unless_the_task_names_one() {
        let (d, _tmp) = models_daemon(
            Some("sonnet"),
            vec![machine("pi", 2)],
            &[("pi", 2, FakeHerdr::new())],
        )
        .await;
        let IpcResponse::Task(t) = d.handle(run_model(None, None, None)).await else {
            panic!()
        };
        assert_eq!(t.model(), Some("sonnet"));
        assert_eq!(
            t.spec.agent_source.as_ref().unwrap().model_from.as_deref(),
            Some("flock personal")
        );
        assert_eq!(t.spec.agent_args[..2], ["--model", "claude-sonnet-5"]);
        let IpcResponse::Task(t) = d.handle(run_model(Some("opus"), None, None)).await else {
            panic!()
        };
        assert_eq!(t.model(), Some("opus"));
        assert_eq!(t.spec.agent_args[..2], ["--model", "claude-opus-5-5"]);
    }

    /// A name `[models]` lacks, and a model of another kind than the agent
    /// the task asked for, or than every agent the machine it is pinned to
    /// can run, are refused.
    #[tokio::test]
    async fn an_unknown_or_mismatched_model_is_refused() {
        let cx = MachineConfig {
            agent: Some("codex".into()),
            ..machine("cx", 1)
        };
        let (d, _tmp) = models_daemon(
            None,
            vec![machine("pi", 1), cx],
            &[("pi", 1, FakeHerdr::new()), ("cx", 1, FakeHerdr::new())],
        )
        .await;
        let resp = d.handle(run_model(Some("haiku"), None, None)).await;
        match resp {
            IpcResponse::Error { code, message } => {
                assert_eq!(code, "unknown_model", "{message}");
                assert!(message.contains("gpt, opus, sonnet"), "{message}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            error_code(
                d.handle(run_model(Some("sonnet"), Some("codex"), None))
                    .await
            ),
            "model_kind_mismatch"
        );
        assert_eq!(
            error_code(d.handle(run_model(Some("gpt"), None, Some("pi"))).await),
            "model_kind_mismatch"
        );
        assert!(d.store.queued_tasks().unwrap().is_empty());
        // cx's own agent is codex, but its flock's is a claude.
        let IpcResponse::Task(t) = d.handle(run_model(Some("sonnet"), None, Some("cx"))).await
        else {
            panic!()
        };
        assert_eq!(t.spec.agent, "claude-personal");
    }

    /// An unpinned task goes only to a machine whose agent has its model's
    /// kind; with none, it waits, and says why.
    #[tokio::test]
    async fn a_model_skips_machines_of_another_kind() {
        let cx = MachineConfig {
            agent: Some("codex".into()),
            ..machine("cx", 1)
        };
        let (d, _tmp) = models_daemon(
            None,
            vec![machine("pi", 1), cx],
            &[("pi", 1, FakeHerdr::new()), ("cx", 1, FakeHerdr::new())],
        )
        .await;
        let IpcResponse::Task(t) = d.handle(run_model(Some("gpt"), None, None)).await else {
            panic!()
        };
        assert_eq!(
            (t.machine.as_deref(), t.spec.agent.as_str()),
            (Some("cx"), "codex")
        );
        assert_eq!(t.spec.agent_args, vec!["--model", "gpt-x"]);
        // cx is full, and pi's agent is a claude.
        let IpcResponse::Task(t) = d.handle(run_model(Some("gpt"), None, None)).await else {
            panic!()
        };
        assert_eq!(t.state, TaskState::Queued);
        let err = t.error.unwrap_or_default();
        assert!(err.starts_with("waiting for a machine"), "{err}");
        assert!(err.contains("model gpt runs on codex agents"), "{err}");
    }

    /// `models_daemon` with `gpt5`, a model for opencode agents, in
    /// `[models]` as well.
    async fn opencode_daemon(
        machines: Vec<MachineConfig>,
        fakes: &[(&str, u32, FakeHerdr)],
    ) -> (Daemon, tempfile::TempDir) {
        let (d, tmp) = models_daemon(None, machines, fakes).await;
        let mut config = models_config();
        config.models.0.insert(
            "gpt5".into(),
            crate::config::ModelDef {
                kind: "opencode".into(),
                args: vec!["--model".into(), "openai/gpt-5.5".into()],
            },
        );
        std::fs::write(d.paths.config_file(), toml::to_string(&config).unwrap()).unwrap();
        d.fleet().set_config(&config);
        (d, tmp)
    }

    /// A model of another kind than the flock's agent runs on the machine
    /// whose `agents` names one of its kind, as that agent, without the
    /// flock's claude args; describe says where the agent came from.
    #[tokio::test]
    async fn a_model_of_another_kind_runs_on_the_agent_named_for_it() {
        let desk = MachineConfig {
            agents: [("opencode".to_string(), "opencode".to_string())].into(),
            ..machine("desk", 1)
        };
        let fake = FakeHerdr::new();
        let (d, _tmp) = opencode_daemon(
            vec![machine("pi", 1), desk],
            &[("pi", 1, FakeHerdr::new()), ("desk", 1, fake.clone())],
        )
        .await;
        let IpcResponse::Task(t) = d.handle(run_model(Some("gpt5"), None, None)).await else {
            panic!()
        };
        assert_eq!(
            (t.machine.as_deref(), t.spec.agent.as_str()),
            (Some("desk"), "opencode")
        );
        assert_eq!(t.spec.agent_args, vec!["--model", "openai/gpt-5.5"]);
        let start = fake
            .requests()
            .into_iter()
            .find(|r| r.method == "agent.start")
            .unwrap();
        assert_eq!(start.params["kind"], "opencode");
        assert_eq!(
            start.params["args"],
            serde_json::json!(["--model", "openai/gpt-5.5"])
        );
        let text = crate::cli::task_detail(&t);
        assert!(
            text.contains("agent:      opencode (from machine desk agents.opencode)\n"),
            "{text}"
        );
        // A claude model still runs on the flock's claude agent there.
        let IpcResponse::Task(t) = d.handle(run_model(Some("sonnet"), None, Some("pi"))).await
        else {
            panic!()
        };
        assert_eq!(t.spec.agent, "claude-personal");
    }

    /// With no machine of the flock naming an agent of the model's kind,
    /// the task waits and says so; pinned to such a machine, it is refused.
    #[tokio::test]
    async fn a_model_no_machine_has_an_agent_for_waits_or_is_refused() {
        let (d, _tmp) = opencode_daemon(
            vec![machine("pi", 1), machine("pi-2", 1)],
            &[("pi", 1, FakeHerdr::new()), ("pi-2", 1, FakeHerdr::new())],
        )
        .await;
        let IpcResponse::Task(t) = d.handle(run_model(Some("gpt5"), None, None)).await else {
            panic!()
        };
        assert_eq!(t.state, TaskState::Queued);
        let err = t.error.unwrap_or_default();
        assert!(
            err.contains("no machine in flock personal has an opencode agent"),
            "{err}"
        );
        let resp = d.handle(run_model(Some("gpt5"), None, Some("pi-2"))).await;
        match resp {
            IpcResponse::Error { code, message } => {
                assert_eq!(code, "model_kind_mismatch", "{message}");
                assert!(message.contains("no layer's agents.opencode"), "{message}");
            }
            other => panic!("{other:?}"),
        }
    }

    /// A retry settles its model again, so one since dropped from
    /// `[models]` is refused.
    #[tokio::test]
    async fn a_retry_of_a_dropped_model_is_unknown() {
        let (d, _tmp) =
            models_daemon(None, vec![machine("pi", 2)], &[("pi", 2, FakeHerdr::new())]).await;
        let IpcResponse::Task(mut t) = d.handle(run_model(Some("sonnet"), None, None)).await else {
            panic!()
        };
        t.state = TaskState::Failed;
        t.finished_at = Some(chrono::Utc::now());
        d.store.update_task(&mut t).unwrap();
        let mut config = models_config();
        config.models.0.remove("sonnet");
        d.fleet().set_config(&config);
        let retry = IpcRequest::TaskRetry {
            id: t.id,
            place: None,
        };
        assert_eq!(error_code(d.handle(retry.clone()).await), "unknown_model");
        d.fleet().set_config(&models_config());
        let IpcResponse::Task(copy) = d.handle(retry).await else {
            panic!()
        };
        assert_eq!(copy.model(), Some("sonnet"));
    }

    /// `models_config` with `[profiles.ci]`, develop plus make.
    fn profiles_config() -> PastorConfig {
        let mut c = models_config();
        c.profiles.0.insert(
            "ci".into(),
            crate::config::profile::ProfileDef {
                extends: Some("develop".into()),
                allow: vec!["Bash(make:*)".into()],
                ..Default::default()
            },
        );
        c
    }

    /// `models_daemon` with `profiles_config` on disk and applied.
    async fn profiles_daemon(
        machines: Vec<MachineConfig>,
        fakes: &[(&str, u32, FakeHerdr)],
    ) -> (Daemon, tempfile::TempDir) {
        let (d, tmp) = models_daemon(None, machines, fakes).await;
        apply_config(&d, &profiles_config()).await;
        (d, tmp)
    }

    /// Write `config` to pastor.toml and have the head reload it, as an
    /// edit and `pastor job reload` would: the fleet and its actors both
    /// take it.
    async fn apply_config(d: &Daemon, config: &PastorConfig) {
        std::fs::write(d.paths.config_file(), toml::to_string(config).unwrap()).unwrap();
        let resp = d.handle(IpcRequest::Reload).await;
        assert!(matches!(resp, IpcResponse::Jobs(_)), "{resp:?}");
    }

    fn run_profile(profile: Option<&str>, machine: Option<&str>) -> IpcRequest {
        let IpcRequest::Run {
            preempt: false,
            summary: _,
            prompt,
            spec,
            flock,
            agent,
            priority,
            role,
            description,
        } = run_model(None, None, machine)
        else {
            unreachable!()
        };
        IpcRequest::Run {
            preempt: false,
            summary: None,
            prompt,
            spec,
            flock,
            description,
            agent: agent.map(|a| AgentChoice {
                profile: profile.map(Into::into),
                ..a
            }),
            priority,
            role,
        }
    }

    /// `--profile` adds the profile's lists to the task's, the task keeps
    /// its name and where it came from, and herdr starts Claude with
    /// `--permission-mode dontAsk` and the lists as its flags.
    #[tokio::test]
    async fn a_task_runs_under_the_profile_it_names() {
        let fake = FakeHerdr::new();
        let (d, _tmp) = profiles_daemon(vec![machine("pi", 2)], &[("pi", 2, fake.clone())]).await;
        let IpcResponse::Task(t) = d.handle(run_profile(Some("ci"), None)).await else {
            panic!()
        };
        assert_eq!(t.profile(), Some("ci"));
        let source = t.spec.agent_source.clone().unwrap();
        assert_eq!(source.profile_from.as_deref(), Some("task run"));
        assert!(t.spec.allow.contains(&"Edit".to_string()));
        assert_eq!(
            t.spec.allow.last().map(String::as_str),
            Some("Bash(make:*)")
        );
        assert!(t.spec.deny.contains(&"Bash(sudo:*)".to_string()));
        assert_eq!(t.to_json()["profile"], "ci");
        let reqs = fake.requests();
        let start = reqs.iter().find(|r| r.method == "agent.start").unwrap();
        let args: Vec<&str> = start.params["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        assert_eq!(args[..3], ["-v", "--permission-mode", "dontAsk"]);
        assert!(
            args.windows(2)
                .any(|w| w == ["--disallowedTools", "Bash(sudo:*)"]),
            "{args:?}"
        );
        let text = crate::cli::task_detail(&t);
        assert!(text.contains("profile:    ci (from task run)"), "{text}");

        // With no profile named anywhere, nothing changes.
        let IpcResponse::Task(t) = d.handle(run_profile(None, None)).await else {
            panic!()
        };
        assert_eq!(t.profile(), None);
        assert!(t.spec.allow.is_empty());
    }

    /// A machine's profile reaches its tasks, before its flock's; a name
    /// that is no profile is refused, and so are agent args that pick a
    /// permission mode while a profile applies.
    #[tokio::test]
    async fn a_machines_profile_applies_and_bad_ones_are_refused() {
        let pi = MachineConfig {
            profile: Some("review".into()),
            ..machine("pi", 2)
        };
        let (d, _tmp) = profiles_daemon(vec![pi], &[("pi", 2, FakeHerdr::new())]).await;
        let IpcResponse::Task(t) = d.handle(run_profile(None, None)).await else {
            panic!()
        };
        assert_eq!(t.profile(), Some("review"));
        assert_eq!(
            t.spec.agent_source.unwrap().profile_from.as_deref(),
            Some("machine pi")
        );
        assert!(t.spec.deny.contains(&"Edit".to_string()));

        assert_eq!(
            error_code(d.handle(run_profile(Some("nope"), None)).await),
            "unknown_profile"
        );
        let IpcRequest::Run {
            preempt: false,
            summary: _,
            prompt,
            spec,
            flock,
            agent,
            priority,
            role,
            description,
        } = run_profile(Some("ci"), None)
        else {
            unreachable!()
        };
        let conflict = IpcRequest::Run {
            preempt: false,
            summary: None,
            prompt,
            spec,
            flock,
            description,
            agent: agent.map(|a| AgentChoice {
                agent_args: Some(vec!["--permission-mode".into(), "bypassPermissions".into()]),
                ..a
            }),
            priority,
            role,
        };
        assert_eq!(
            error_code(d.handle(conflict).await),
            crate::config::PROFILE_ARGS_CONFLICT
        );
    }

    /// The unrestricted rule: a task may ask for `unrestricted` only where
    /// the machine's own profile is `unrestricted`. Pinned elsewhere it is
    /// refused; unpinned it goes to such a machine, or waits and says why.
    #[tokio::test]
    async fn unrestricted_runs_only_on_a_machine_that_is_unrestricted() {
        let box1 = MachineConfig {
            profile: Some("unrestricted".into()),
            ..machine("box", 1)
        };
        let (d, _tmp) = profiles_daemon(
            vec![machine("pi", 2), box1],
            &[("pi", 2, FakeHerdr::new()), ("box", 1, FakeHerdr::new())],
        )
        .await;
        assert_eq!(
            error_code(
                d.handle(run_profile(Some("unrestricted"), Some("pi")))
                    .await
            ),
            PROFILE_NOT_ALLOWED
        );
        let IpcResponse::Task(t) = d.handle(run_profile(Some("unrestricted"), None)).await else {
            panic!()
        };
        assert_eq!(t.machine.as_deref(), Some("box"));
        assert_eq!(t.profile(), Some("unrestricted"));
        // box is full, and pi is not unrestricted.
        let IpcResponse::Task(t) = d.handle(run_profile(Some("unrestricted"), None)).await else {
            panic!()
        };
        assert_eq!(t.state, TaskState::Queued);
        let err = t.error.unwrap_or_default();
        assert!(err.starts_with("waiting for a machine"), "{err}");
        assert!(err.contains("runs only where"), "{err}");
        // On pi, a task that names none runs without one.
        let IpcResponse::Task(t) = d.handle(run_profile(None, Some("pi"))).await else {
            panic!()
        };
        assert_eq!(t.profile(), None);
        // `machine list` and `machine describe` show each machine's own.
        let IpcResponse::Machines(ms) = d.handle(IpcRequest::FlockList).await else {
            panic!()
        };
        let own: Vec<_> = ms
            .iter()
            .map(|m| (m.name.as_str(), m.profile.as_deref()))
            .collect();
        assert_eq!(own, [("pi", None), ("box", Some("unrestricted"))]);
        let IpcResponse::MachineDescription(m) = d
            .handle(IpcRequest::MachineDescribe { name: "box".into() })
            .await
        else {
            panic!()
        };
        let text = crate::describe::machine_text(&m);
        assert!(text.contains("profile:     unrestricted"), "{text}");
    }

    /// A profile dropped from pastor.toml while a task waits for a machine
    /// keeps it waiting, with the reason in its error, rather than start
    /// it without the profile; put back, the task runs under it.
    #[tokio::test]
    async fn a_profile_removed_while_queued_holds_the_task() {
        let fake = FakeHerdr::new();
        let (d, _tmp) = profiles_daemon(vec![machine("pi", 1)], &[("pi", 1, fake.clone())]).await;
        let ask = AgentChoice {
            profile: Some("ci".into()),
            ..Default::default()
        };
        let t = d
            .fleet()
            .queue_run("x".into(), spec(), None, Some(&ask), None)
            .await
            .unwrap();
        assert_eq!(t.state, TaskState::Queued);
        apply_config(&d, &models_config()).await;
        d.fleet().dispatch_queued().await;
        let t = d.store.get_task(t.id).unwrap().unwrap();
        assert_eq!(t.state, TaskState::Queued);
        let err = t.error.clone().unwrap_or_default();
        assert!(err.contains("profile ci is not built in"), "{err}");
        assert!(!fake.requests().iter().any(|r| r.method == "agent.start"));

        apply_config(&d, &profiles_config()).await;
        d.fleet().dispatch_queued().await;
        let t = d.store.get_task(t.id).unwrap().unwrap();
        assert_ne!(t.state, TaskState::Queued);
        assert_eq!(t.error, None);
        let reqs = fake.requests();
        let start = reqs.iter().find(|r| r.method == "agent.start").unwrap();
        assert!(
            start.params["args"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("dontAsk"))
        );
    }

    /// A task that inherited its profile from `[defaults]` (not its own
    /// ask) while queued for a busy machine keeps that profile pinned: once
    /// `[defaults].profile` moves on and the profile itself is gone too, the
    /// re-settle at placement holds the task rather than start it with none
    /// of the profile's lists, silently, because the ask itself never named
    /// one.
    #[tokio::test]
    async fn a_profile_inherited_from_defaults_is_pinned_while_queued() {
        let fake = FakeHerdr::new();
        let (d, _tmp) = profiles_daemon(vec![machine("pi", 1)], &[("pi", 1, fake.clone())]).await;
        let mut with_default_profile = profiles_config();
        with_default_profile.defaults.profile = Some("ci".into());
        apply_config(&d, &with_default_profile).await;

        // Occupy pi's one slot so the new task must wait.
        let IpcResponse::Task(busy) = d
            .handle(IpcRequest::Run {
                preempt: false,
                summary: None,
                prompt: "busy".into(),
                spec: spec(),
                flock: None,
                agent: None,
                priority: None,
                role: Default::default(),
                description: None,
            })
            .await
        else {
            panic!()
        };
        assert_eq!(busy.state, TaskState::Running);

        let mut pinned = spec();
        pinned.machine = Some("pi".into());
        let t = d
            .fleet()
            .queue_run(
                "x".into(),
                pinned,
                None,
                Some(&AgentChoice::default()),
                None,
            )
            .await
            .unwrap();
        let source = t.spec.agent_source.clone().unwrap();
        assert_eq!(source.profile.as_deref(), Some("ci"));
        assert_eq!(source.profile_from.as_deref(), Some("defaults"));
        assert!(source.ask.profile.is_none(), "the ask itself named none");

        d.fleet().dispatch_queued().await;
        let t = d.store.get_task(t.id).unwrap().unwrap();
        assert_eq!(t.state, TaskState::Queued, "pi is still busy");

        // `[defaults]` moves on, and the profile it named is dropped too.
        let mut without = profiles_config();
        without.profiles.0.remove("ci");
        apply_config(&d, &without).await;

        // Free pi's slot.
        fake.close_pane(busy.pane_id.as_deref().unwrap());
        let deadline = Instant::now() + Duration::from_secs(5);
        while d.store.get_task(busy.id).unwrap().unwrap().state != TaskState::Closed {
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        d.fleet().dispatch_queued().await;
        let t = d.store.get_task(t.id).unwrap().unwrap();
        assert_eq!(t.state, TaskState::Queued, "held rather than started bare");
        let err = t.error.clone().unwrap_or_default();
        assert!(err.contains("profile ci is not built in"), "{err}");
        assert_eq!(
            fake.requests()
                .iter()
                .filter(|r| r.method == "agent.start")
                .count(),
            1,
            "only the busy task ever started"
        );
    }

    /// A retry settles its profile again, so one since dropped is refused.
    #[tokio::test]
    async fn a_retry_of_a_dropped_profile_is_unknown() {
        let (d, _tmp) =
            profiles_daemon(vec![machine("pi", 2)], &[("pi", 2, FakeHerdr::new())]).await;
        let IpcResponse::Task(mut t) = d.handle(run_profile(Some("ci"), None)).await else {
            panic!()
        };
        t.state = TaskState::Failed;
        t.finished_at = Some(chrono::Utc::now());
        d.store.update_task(&mut t).unwrap();
        d.fleet().set_config(&models_config());
        let retry = IpcRequest::TaskRetry {
            id: t.id,
            place: None,
        };
        assert_eq!(error_code(d.handle(retry).await), "unknown_profile");
    }

    /// Claude resolves `~` in a pattern itself, on its own machine, so a
    /// profile's `~` is passed as written: a machine that cannot report a
    /// home still runs it.
    #[tokio::test]
    async fn a_profiles_tilde_is_passed_as_written_without_a_home() {
        let fake = FakeHerdr::new();
        fake.set_home(None);
        let (d, _tmp) = profiles_daemon(vec![machine("pi", 1)], &[("pi", 1, fake.clone())]).await;
        let mut config = profiles_config();
        config.profiles.0.get_mut("ci").unwrap().deny = vec!["Read(~/.ssh/**)".into()];
        apply_config(&d, &config).await;
        let ask = AgentChoice {
            profile: Some("ci".into()),
            ..Default::default()
        };
        let t = d
            .fleet()
            .queue_run("x".into(), spec(), None, Some(&ask), None)
            .await
            .unwrap();
        d.fleet().dispatch_queued().await;
        let t = d.store.get_task(t.id).unwrap().unwrap();
        assert_ne!(t.state, TaskState::Failed, "{:?}", t.error);
        let reqs = fake.requests();
        let start = reqs.iter().find(|r| r.method == "agent.start").unwrap();
        assert!(
            start.params["args"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("Read(~/.ssh/**)"))
        );
    }

    /// A job's model is a template: the item's model when it has one, the
    /// flock's when it renders empty, and an item error when `[models]`
    /// lacks it.
    #[tokio::test]
    async fn a_jobs_model_comes_from_its_item() {
        let (d, _tmp) = models_daemon(
            Some("sonnet"),
            vec![machine("pi", 4)],
            &[("pi", 4, FakeHerdr::new())],
        )
        .await;
        let config = models_config();
        let _ = d.scheduler.sync_config().await;
        let text = "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nmodel = \"{{ item.model }}\"\nprompt = \"p\"\n";
        let job = crate::config::job::Job::parse(
            text,
            "j",
            &config.defaults,
            &crate::connector::Builtins,
        )
        .unwrap();
        let queue = |item: serde_json::Value| {
            let job = job.clone();
            let fleet = d.fleet().clone();
            async move {
                fleet
                    .queue_job_task(&job, &item, |_| Ok(("p".into(), job.spec.clone())))
                    .await
            }
        };
        let t = queue(serde_json::json!({"key": "a", "model": "opus"}))
            .await
            .unwrap();
        assert_eq!(t.model(), Some("opus"));
        assert_eq!(
            t.spec.agent_source.as_ref().unwrap().model_from.as_deref(),
            Some("job j")
        );
        let t = queue(serde_json::json!({"key": "b"})).await.unwrap();
        assert_eq!(t.model(), Some("sonnet"));
        let err = queue(serde_json::json!({"key": "c", "model": "haiku"}))
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("model haiku is not in [models]"),
            "{err:#}"
        );
    }

    /// `personal` holds `own`, which runs `claude-personal`, and `plain`,
    /// which runs `[defaults]`' `claude`. The flock's `--model` names no
    /// agent, so both take it.
    fn personal_flock() -> Flock {
        use crate::config::flock::FlockEntry;
        Flock {
            flocks: vec![FlockEntry {
                name: "personal".into(),
                default: true,
                agent_args: Some(vec!["--model".into(), "claude-opus-5-5".into()]),
                ..Default::default()
            }],
            machines: vec![
                MachineConfig {
                    agent: Some("claude-personal".into()),
                    ..machine("own", 1)
                },
                machine("plain", 1),
            ],
        }
    }

    /// Two machines of one flock, each with its own agent: every task runs
    /// the agent of the machine it lands on, and herdr is asked for it.
    #[tokio::test]
    async fn each_task_runs_the_agent_of_the_machine_it_lands_on() {
        let (own, plain) = (FakeHerdr::new(), FakeHerdr::new());
        let (d, _tmp) = daemon_with_flock(
            personal_flock(),
            &[("own", 1, own.clone()), ("plain", 1, plain.clone())],
        )
        .await;
        let run = || IpcRequest::Run {
            preempt: false,
            summary: None,
            role: Default::default(),
            description: None,
            prompt: "x".into(),
            spec: spec(),
            flock: None,
            agent: Some(AgentChoice::default()),
            priority: None,
        };
        let mut seen = vec![];
        for _ in 0..2 {
            let IpcResponse::Task(t) = d.handle(run()).await else {
                panic!()
            };
            let from = t.spec.agent_source.clone().unwrap();
            seen.push((
                t.machine.clone().unwrap(),
                t.spec.agent.clone(),
                t.spec.agent_args.join(" "),
                from.agent,
                from.agent_args,
            ));
        }
        seen.sort();
        assert_eq!(
            seen,
            vec![
                (
                    "own".into(),
                    "claude-personal".into(),
                    "--model claude-opus-5-5".into(),
                    "machine own".into(),
                    Some("flock personal".into())
                ),
                (
                    "plain".into(),
                    "claude".into(),
                    "--model claude-opus-5-5".into(),
                    "defaults".into(),
                    Some("flock personal".into())
                ),
            ]
        );
        let kind = |fake: &FakeHerdr| {
            let reqs = fake.requests();
            let start = reqs.iter().find(|r| r.method == "agent.start").unwrap();
            start.params["kind"].as_str().unwrap().to_string()
        };
        assert_eq!(kind(&own), "claude-personal");
        assert_eq!(kind(&plain), "claude");
    }

    /// A task pinned to a machine shows that machine's agent from the moment
    /// it is queued, and `--agent` wins over the machine's.
    #[tokio::test]
    async fn a_pinned_task_takes_its_machines_agent_unless_it_names_one() {
        let (d, _tmp) = daemon_with_flock(
            personal_flock(),
            &[("own", 1, FakeHerdr::new()), ("plain", 1, FakeHerdr::new())],
        )
        .await;
        let pinned = DispatchSpec {
            machine: Some("own".into()),
            ..spec()
        };
        let t = d
            .fleet()
            .queue_run(
                "x".into(),
                pinned.clone(),
                None,
                Some(&AgentChoice::default()),
                None,
            )
            .await
            .unwrap();
        assert_eq!(t.state, TaskState::Queued);
        assert_eq!(t.spec.agent, "claude-personal");
        assert_eq!(t.spec.agent_source.unwrap().agent, "machine own");

        let asked = AgentChoice {
            agent: Some("aider".into()),
            ..Default::default()
        };
        let t = d
            .fleet()
            .queue_run("x".into(), pinned, None, Some(&asked), None)
            .await
            .unwrap();
        assert_eq!(t.spec.agent, "aider");
        assert_eq!(t.spec.agent_source.unwrap().agent, "task run");
    }

    /// `home_and_work` plus `spare`, a flock with no machine.
    async fn spare_daemon() -> (Daemon, tempfile::TempDir) {
        let mut flock = home_and_work();
        flock.flocks.push(crate::config::flock::FlockEntry {
            name: "spare".into(),
            default: false,
            ..Default::default()
        });
        daemon_with_flock(
            flock,
            &[("h", 2, FakeHerdr::new()), ("w", 2, FakeHerdr::new())],
        )
        .await
    }

    /// The head removes a flock: refused while a queued task names it, and
    /// once it is gone a `task run` in it is refused at once, before any
    /// reload, and the file no longer has it.
    #[tokio::test]
    async fn flock_remove_goes_through_the_head() {
        let (d, tmp) = spare_daemon().await;
        let IpcResponse::Task(t) = d.handle(run_in(Some("spare"), None)).await else {
            panic!()
        };
        assert_eq!(t.state, TaskState::Queued);
        let resp = d
            .handle(IpcRequest::FlockRemove {
                name: "spare".into(),
            })
            .await;
        let IpcResponse::Error { code, message } = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(code, "flock_has_tasks");
        assert!(message.contains(&t.display_id()), "{message}");
        d.handle(IpcRequest::TaskClose {
            id: t.id,
            remove_worktree: false,
        })
        .await;
        let resp = d
            .handle(IpcRequest::FlockRemove {
                name: "spare".into(),
            })
            .await;
        assert!(matches!(resp, IpcResponse::Text(_)), "{resp:?}");
        let resp = d.handle(run_in(Some("spare"), None)).await;
        assert!(
            matches!(&resp, IpcResponse::Error { code, .. } if code == "unknown_flock"),
            "{resp:?}"
        );
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        assert!(!Flock::load(&paths.flock_file()).unwrap().has_flock("spare"));
        let resp = d
            .handle(IpcRequest::FlockRemove {
                name: "home".into(),
            })
            .await;
        assert!(
            matches!(&resp, IpcResponse::Error { code, .. } if code == "flock_is_default"),
            "{resp:?}"
        );
    }

    /// The flock of `name` as the head's fleet reports it, `None` once it
    /// is gone from the fleet.
    fn fleet_flock_of(d: &Daemon, name: &str) -> Option<String> {
        d.fleet()
            .statuses()
            .into_iter()
            .find(|s| s.name == name)
            .and_then(|s| s.flock)
    }

    /// `flock add`, `flock default`, `machine add|move|remove` go through the
    /// head: it edits flock.toml with the CLI's own code, reloads, answers
    /// `Text`, and its fleet matches the file afterwards.
    #[tokio::test]
    async fn fleet_edits_go_through_the_head() {
        let (d, tmp) = daemon_with_flock(
            home_and_work(),
            &[
                ("h", 2, FakeHerdr::new()),
                ("w", 2, FakeHerdr::new()),
                ("n", 2, FakeHerdr::new()),
            ],
        )
        .await;
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        let on_disk = || Flock::load(&paths.flock_file()).unwrap();
        let text = |resp: IpcResponse| match resp {
            IpcResponse::Text(t) => t,
            other => panic!("expected text, got {other:?}"),
        };

        let said = text(
            d.handle(IpcRequest::FlockAdd {
                description: None,
                name: "spare".into(),
                default: false,
                machines: vec![],
            })
            .await,
        );
        assert!(said.starts_with("added flock spare; "), "{said}");
        assert!(said.contains("picked it up"), "{said}");
        assert!(on_disk().has_flock("spare"));
        assert!(d.fleet().flock().has_flock("spare"));

        let said = text(
            d.handle(IpcRequest::FlockSetDefault {
                name: "work".into(),
            })
            .await,
        );
        assert!(said.starts_with("work is the default flock"), "{said}");
        assert_eq!(on_disk().default_flock(), "work");
        assert_eq!(d.fleet().flock().default_flock(), "work");

        let said = text(
            d.handle(IpcRequest::MachineAdd {
                machine: MachineConfig {
                    flock: Some("spare".into()),
                    ..machine("n", 2)
                },
            })
            .await,
        );
        assert!(said.starts_with("added n to flock spare in "), "{said}");
        assert!(on_disk().get("n").is_some());
        wait_until("n in the fleet", || {
            fleet_flock_of(&d, "n").as_deref() == Some("spare")
        })
        .await;

        let said = text(
            d.handle(IpcRequest::MachineMove {
                name: "h".into(),
                flock: "spare".into(),
            })
            .await,
        );
        assert!(said.starts_with("moved h to flock spare"), "{said}");
        assert_eq!(on_disk().machine_flock("h"), Some("spare"));
        assert_eq!(fleet_flock_of(&d, "h").as_deref(), Some("spare"));

        let said = text(
            d.handle(IpcRequest::FlockJoin {
                flock: "work".into(),
                machine: "h".into(),
                max: Some(1),
            })
            .await,
        );
        assert!(
            said.starts_with("h is in flock work with 1; its flocks: work:1,spare:2; "),
            "{said}"
        );
        assert!(said.contains("picked it up"), "{said}");
        assert_eq!(
            d.fleet().flock().machine_flocks("h").unwrap(),
            [("work", Some(1)), ("spare", Some(2))]
        );
        let said = text(
            d.handle(IpcRequest::FlockLeave {
                flock: "work".into(),
                machine: "h".into(),
            })
            .await,
        );
        assert!(
            said.starts_with("h left flock work; its flocks: spare:2"),
            "{said}"
        );
        assert_eq!(on_disk().machine_flocks("h").unwrap(), [("spare", Some(2))]);

        let said = text(
            d.handle(IpcRequest::MachineRemove { name: "w".into() })
                .await,
        );
        assert!(said.starts_with("removed w; "), "{said}");
        assert!(on_disk().get("w").is_none());
        wait_until("w out of the fleet", || fleet_flock_of(&d, "w").is_none()).await;

        // A refused edit keeps its code and leaves the file alone.
        let before = std::fs::read_to_string(paths.flock_file()).unwrap();
        for (req, code) in [
            (
                IpcRequest::FlockAdd {
                    description: None,
                    name: "spare".into(),
                    default: false,
                    machines: vec![],
                },
                "flock_exists",
            ),
            (
                IpcRequest::FlockJoin {
                    flock: "work".into(),
                    machine: "nope".into(),
                    max: None,
                },
                "unknown_machine",
            ),
            (
                IpcRequest::FlockLeave {
                    flock: "work".into(),
                    machine: "h".into(),
                },
                "not_in_flock",
            ),
            (
                IpcRequest::FlockSetDefault {
                    name: "nope".into(),
                },
                "unknown_flock",
            ),
            (
                IpcRequest::MachineAdd {
                    machine: machine("h", 2),
                },
                "machine_exists",
            ),
            (
                IpcRequest::MachineMove {
                    name: "h".into(),
                    flock: "nope".into(),
                },
                "unknown_flock",
            ),
            (
                IpcRequest::MachineRemove {
                    name: "nope".into(),
                },
                "unknown_machine",
            ),
        ] {
            assert_eq!(error_code(d.handle(req).await), code);
        }
        assert_eq!(std::fs::read_to_string(paths.flock_file()).unwrap(), before);
    }

    /// `flock add --default` on a file with only the implicit flock keeps its
    /// machines there while the head has tasks queued in it.
    #[tokio::test]
    async fn flock_add_default_through_the_head_sees_queued_tasks() {
        let (d, _tmp) = daemon_with_flock(
            Flock {
                flocks: vec![],
                machines: vec![machine("a", 1)],
            },
            &[("a", 1, FakeHerdr::new())],
        )
        .await;
        let t = d
            .store
            .insert_task(NewTask {
                description: None,
                job: "run".into(),
                item: serde_json::Value::Null,
                prompt: "p".into(),
                // Pinned to a machine the fleet lacks, so it stays queued.
                spec: DispatchSpec {
                    machine: Some("gone".into()),
                    ..spec()
                },
                flock: crate::config::flock::DEFAULT_FLOCK.into(),
            })
            .unwrap();
        let IpcResponse::Text(said) = d
            .handle(IpcRequest::FlockAdd {
                description: None,
                name: "work".into(),
                default: true,
                machines: vec![],
            })
            .await
        else {
            panic!()
        };
        assert!(said.contains(&t.display_id()), "{said}");
        assert_eq!(fleet_flock_of(&d, "a").as_deref(), Some("default"));
    }

    /// A retry copies the flock of the task it retries. A failed task keeps
    /// its flock after `flock remove` (only queued tasks block that), so its
    /// retry is refused with `unknown_flock` rather than queued in a flock
    /// dispatch never serves.
    #[tokio::test]
    async fn retry_in_a_removed_flock_is_unknown() {
        let (d, _tmp) = spare_daemon().await;
        let IpcResponse::Task(mut t) = d.handle(run_in(Some("spare"), None)).await else {
            panic!()
        };
        t.state = TaskState::Failed;
        t.finished_at = Some(chrono::Utc::now());
        d.store.update_task(&mut t).unwrap();
        let resp = d
            .handle(IpcRequest::FlockRemove {
                name: "spare".into(),
            })
            .await;
        assert!(matches!(resp, IpcResponse::Text(_)), "{resp:?}");
        assert_eq!(
            error_code(
                d.handle(IpcRequest::TaskRetry {
                    id: t.id,
                    place: None
                })
                .await
            ),
            "unknown_flock"
        );
        assert!(
            d.store.queued_tasks().unwrap().is_empty(),
            "no copy left queued"
        );
    }

    /// A `flock remove` and a `task run` in that flock that race each other
    /// are ordered by the dispatch lock (tokio's mutex is fair, so the one
    /// that waited first goes first): whichever loses sees what the winner
    /// did. Before, the CLI checked for queued tasks and then edited the
    /// file, and a run landing in between waited forever in a removed flock.
    #[tokio::test]
    async fn flock_remove_and_run_are_ordered_by_the_dispatch_lock() {
        for remove_first in [true, false] {
            let (d, tmp) = spare_daemon().await;
            let fleet = d.fleet();
            let file = Paths::new(tmp.path().join("c"), tmp.path().join("s")).flock_file();
            let held = fleet.dispatch_lock.lock().await;
            let remove = {
                let fleet = fleet.clone();
                async move { fleet.remove_flock(&file, "spare").await }
            };
            let run = {
                let fleet = fleet.clone();
                async move {
                    fleet
                        .queue_run("x".into(), spec(), Some("spare"), None, None)
                        .await
                }
            };
            // Each waits on the held lock before the next is started.
            let (remove, run) = if remove_first {
                let remove = tokio::spawn(remove);
                tokio::task::yield_now().await;
                (remove, tokio::spawn(run))
            } else {
                let run = tokio::spawn(run);
                tokio::task::yield_now().await;
                (tokio::spawn(remove), run)
            };
            tokio::task::yield_now().await;
            drop(held);
            let (remove, run) = (remove.await.unwrap(), run.await.unwrap());
            if remove_first {
                remove.unwrap();
                assert!(
                    matches!(run, Err(QueueError::Flock(TaskFlockError::UnknownFlock(_)))),
                    "{run:?}"
                );
            } else {
                assert_eq!(run.unwrap().flock.as_deref(), Some("spare"));
                let err = remove.unwrap_err();
                assert_eq!(
                    err.downcast_ref::<EditError>().map(EditError::code),
                    Some("flock_has_tasks"),
                    "{err:#}"
                );
            }
        }
    }

    /// A machine edit changes the file and the wanted flock in one step: a
    /// `task run` that comes right after, before any reload, is refused for
    /// the removed machine, and one queued for a moved machine lands in its
    /// new flock. Before, the lock was released between the file edit and
    /// the reload, and such a run queued against the old flock.
    #[tokio::test]
    async fn machine_edit_updates_the_wanted_flock_under_the_lock() {
        let (d, tmp) = flocked_daemon().await;
        let fleet = d.fleet();
        let file = Paths::new(tmp.path().join("c"), tmp.path().join("s")).flock_file();
        fleet
            .edit_flock_file(&file, |f| crate::fleet_edit::move_machine(f, "w", "home"))
            .await
            .unwrap();
        let t = fleet
            .queue_run("x".into(), spec_on("w"), None, None, None)
            .await
            .unwrap();
        assert_eq!(t.flock.as_deref(), Some("home"), "w moved to home");
        fleet
            .edit_flock_file(&file, |f| crate::fleet_edit::remove_machine(f, "w"))
            .await
            .unwrap();
        let run = fleet
            .queue_run("x".into(), spec_on("w"), None, None, None)
            .await;
        assert!(matches!(run, Err(QueueError::UnknownMachine(_))), "{run:?}");
    }

    /// An edit that leaves flock.toml naming a model `[models]` lacks keeps
    /// the wanted flock as it was. The reload after it falls back to the
    /// wanted flock, so publishing the edit would have kept the unknown model
    /// in use instead of the previous flock.
    #[tokio::test]
    async fn flock_edit_with_an_unknown_model_keeps_the_wanted_flock() {
        let (d, tmp) = flocked_daemon().await;
        let fleet = d.fleet();
        let file = Paths::new(tmp.path().join("c"), tmp.path().join("s")).flock_file();
        let before = fleet.flock();
        fleet
            .edit_flock_file(&file, |f| {
                let mut flock = Flock::load_existing(f)?;
                flock.machines[0].model = Some("nope".into());
                flock.save(f)
            })
            .await
            .unwrap();
        assert_eq!(fleet.flock(), before);
        let err = d.scheduler.reload().await;
        assert!(err.is_ok(), "{err:?}");
        assert_eq!(fleet.flock(), before, "the reload kept the previous flock");
    }

    fn spec_on(machine: &str) -> DispatchSpec {
        DispatchSpec {
            machine: Some(machine.into()),
            ..spec()
        }
    }

    /// Only the task's flock takes it: the default one when it names none,
    /// the flock of the machine it is pinned to when it names that.
    #[tokio::test]
    async fn run_lands_in_its_flock() {
        let (d, _tmp) = flocked_daemon().await;
        for (flock, machine, want_flock, want_machine) in [
            (None, None, "home", "h"),
            (Some("work"), None, "work", "w"),
            (None, Some("w"), "work", "w"),
            (Some("home"), Some("h"), "home", "h"),
        ] {
            let resp = d.handle(run_in(flock, machine)).await;
            let IpcResponse::Task(t) = resp else {
                panic!("{resp:?}")
            };
            assert_eq!(
                t.flock.as_deref(),
                Some(want_flock),
                "{flock:?} {machine:?}"
            );
            assert_eq!(
                t.machine.as_deref(),
                Some(want_machine),
                "{flock:?} {machine:?}"
            );
        }
    }

    #[tokio::test]
    async fn run_refuses_a_machine_outside_its_flock_and_an_unknown_flock() {
        let (d, _tmp) = flocked_daemon().await;
        let resp = d.handle(run_in(Some("home"), Some("w"))).await;
        let IpcResponse::Error { code, message } = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(code, "flock_mismatch");
        assert_eq!(message, "machine w is in flock work, not home");
        assert_eq!(
            error_code(d.handle(run_in(Some("play"), None)).await),
            "unknown_flock"
        );
        assert!(
            d.store()
                .list_tasks(&TaskFilter::default())
                .unwrap()
                .is_empty(),
            "nothing queued"
        );
    }

    /// A pinned machine moved to another flock after the task was queued
    /// leaves it queued: it keeps the flock it was made for.
    #[tokio::test]
    async fn a_pin_that_moved_flock_leaves_the_task_queued() {
        let (d, _tmp) = flocked_daemon().await;
        let t = d
            .store()
            .insert_task(NewTask {
                description: None,
                job: "run".into(),
                item: serde_json::Value::Null,
                prompt: "p".into(),
                spec: DispatchSpec {
                    machine: Some("w".into()),
                    ..spec()
                },
                flock: "home".into(),
            })
            .unwrap();
        d.fleet().dispatch_queued().await;
        let t = d.store().get_task(t.id).unwrap().unwrap();
        assert_eq!(t.state, TaskState::Queued);
        assert_eq!(t.machine, None);
    }

    /// Moving a machine changes where new tasks go, not its actor: its
    /// channel, and the tasks already on it, carry on.
    #[tokio::test]
    async fn moving_a_machine_keeps_its_actor() {
        let (d, _tmp) = flocked_daemon().await;
        let mut moved = home_and_work();
        moved.machines[0].flock = Some("work".into());
        let diff = d
            .fleet()
            .apply_flock(&moved, &machine_settings(&test_config()))
            .await;
        assert!(diff.is_empty(), "{diff:?}");
        let views = d.fleet().views();
        let h = views.iter().find(|v| v.name == "h").unwrap();
        assert!(h.in_flock("work") && !h.in_flock("home"), "{h:?}");
        let resp = d.handle(run_in(Some("home"), None)).await;
        let IpcResponse::Task(t) = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(t.state, TaskState::Queued, "home has no machine left");
    }

    /// A machine's agent is read by dispatch from the flock last applied,
    /// like its flock: changing it keeps the actor, and the next task on
    /// the machine runs the new agent.
    #[tokio::test]
    async fn changing_a_machines_agent_keeps_its_actor() {
        let (d, _tmp) = flocked_daemon().await;
        let mut changed = home_and_work();
        changed.machines[0].agent = Some("claude-personal".into());
        let diff = d
            .fleet()
            .apply_flock(&changed, &machine_settings(&test_config()))
            .await;
        assert!(diff.is_empty(), "{diff:?}");
        let resp = d
            .handle(IpcRequest::Run {
                preempt: false,
                summary: None,
                role: Default::default(),
                description: None,
                prompt: "x".into(),
                spec: spec(),
                flock: Some("home".into()),
                agent: Some(AgentChoice::default()),
                priority: None,
            })
            .await;
        let IpcResponse::Task(t) = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(t.machine.as_deref(), Some("h"));
        assert_eq!(t.spec.agent, "claude-personal");
    }

    /// Copilot 4103544623: a machine a reload removed but whose old actor
    /// has not ended is still in the fleet, out of dispatch. A run pinned to
    /// it must be refused, not queued for a machine that will never take it.
    /// The pin is checked against the wanted flock, under the lock
    /// `apply_flock` takes.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_pinned_to_a_removed_machine_shutting_down_is_unknown() {
        let (d, _tmp, _unwedge) = daemon_with_b_shutting_down(false).await;
        let resp = d
            .handle(IpcRequest::Run {
                preempt: false,
                summary: None,
                role: Default::default(),
                description: None,
                prompt: "x".into(),
                spec: DispatchSpec {
                    machine: Some("b".into()),
                    ..spec()
                },
                flock: None,
                agent: None,
                priority: None,
            })
            .await;
        let IpcResponse::Error { code, .. } = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(code, "unknown_machine");
        assert!(
            d.store.queued_tasks().unwrap().is_empty(),
            "no row left queued"
        );
    }

    /// A retry copies the pin, so it is checked like a run's: against the
    /// wanted flock, under the dispatch lock. A removed machine held only
    /// while its old actor ends would never take the copy.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retry_pinned_to_a_removed_machine_shutting_down_is_unknown() {
        let (d, _tmp, _unwedge) = daemon_with_b_shutting_down(false).await;
        let mut failed = insert(&d, TaskState::Failed);
        failed.spec.machine = Some("b".into());
        d.store.update_task(&mut failed).unwrap();
        assert_eq!(
            error_code(
                d.handle(IpcRequest::TaskRetry {
                    id: failed.id,
                    place: None
                })
                .await
            ),
            "unknown_machine"
        );
        assert!(
            d.store.queued_tasks().unwrap().is_empty(),
            "no copy left queued"
        );
        // The state is still checked first.
        let mut running = insert(&d, TaskState::Running);
        running.spec.machine = Some("b".into());
        d.store.update_task(&mut running).unwrap();
        assert_eq!(
            error_code(
                d.handle(IpcRequest::TaskRetry {
                    id: running.id,
                    place: None
                })
                .await
            ),
            "not_retryable"
        );
    }

    #[tokio::test]
    async fn socket_round_trip() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let socket = d.socket_path();
        tokio::spawn(d.run());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !crate::ipc::daemon_running(&socket).await {
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let resp = crate::ipc::request(
            &socket,
            &IpcRequest::Run {
                preempt: false,
                summary: None,
                role: Default::default(),
                description: None,
                prompt: "hi".into(),
                spec: spec(),
                flock: None,
                agent: None,
                priority: None,
            },
        )
        .await
        .unwrap();
        assert!(matches!(resp, IpcResponse::Task(_)), "{resp:?}");
        // garbage in, error out, connection survives for the next client
        let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
        let (r, mut w) = stream.into_split();
        w.write_all(b"not json\n").await.unwrap();
        let mut line = String::new();
        BufReader::new(r).read_line(&mut line).await.unwrap();
        assert!(line.contains("invalid_request"), "{line}");
        assert!(crate::ipc::daemon_running(&socket).await);
    }

    /// One raw line to the head's socket, as a client in task `t-9`'s pane
    /// sends it, and the reply.
    async fn ask_from_task(socket: &std::path::Path, req: &IpcRequest) -> IpcResponse {
        ask_as(socket, req, "t-9").await
    }

    /// `ask_from_task`, from the pane of `task`.
    async fn ask_as(socket: &std::path::Path, req: &IpcRequest, task: &str) -> IpcResponse {
        let stream = tokio::net::UnixStream::connect(socket).await.unwrap();
        let (r, mut w) = stream.into_split();
        let line = crate::ipc::request_line(req, Some(task)).unwrap();
        w.write_all(line.as_bytes()).await.unwrap();
        let mut reply = String::new();
        BufReader::new(r).read_line(&mut reply).await.unwrap();
        serde_json::from_str(reply.trim()).unwrap()
    }

    async fn serving(d: Daemon) -> std::path::PathBuf {
        let socket = d.socket_path();
        tokio::spawn(d.run());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !crate::ipc::daemon_running(&socket).await {
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        socket
    }

    fn run_hi() -> IpcRequest {
        run_hi_as(TaskRole::Agent)
    }

    fn run_hi_as(role: TaskRole) -> IpcRequest {
        IpcRequest::Run {
            preempt: false,
            summary: None,
            role,
            description: None,
            prompt: "hi".into(),
            spec: spec(),
            flock: None,
            agent: None,
            priority: None,
        }
    }

    /// An agent pastor started (its pane has `PASTOR_TASK`) gets a clear
    /// refusal for `pastor task run`, and nothing is queued; it may still
    /// read.
    #[tokio::test]
    async fn an_agent_pastor_started_is_refused_a_task_run() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let store = d.store.clone();
        let socket = serving(d).await;
        let IpcResponse::Error { code, message } = ask_from_task(&socket, &run_hi()).await else {
            panic!("an agent's task run was not refused")
        };
        assert_eq!(code, "agent_refused");
        assert!(message.contains("t-9"), "{message}");
        assert!(message.contains("agents_change_fleet"), "{message}");
        assert!(store.list_tasks(&TaskFilter::default()).unwrap().is_empty());
        let send = IpcRequest::TaskSend {
            id: 1,
            input: crate::machine::SendInput::default(),
        };
        assert!(matches!(
            ask_from_task(&socket, &send).await,
            IpcResponse::Error { code, .. } if code == "agent_refused"
        ));
        let list = IpcRequest::List {
            filter: TaskFilter::default(),
        };
        assert!(matches!(
            ask_from_task(&socket, &list).await,
            IpcResponse::Tasks(_)
        ));
    }

    /// The one change an agent may make on its own: ending its own task,
    /// which leaves it done and ended. Another task's is refused.
    #[tokio::test]
    async fn an_agent_may_end_its_own_task_and_nobody_else_s() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let IpcResponse::Task(mine) = d.handle(run_hi()).await else {
            panic!("run failed")
        };
        let IpcResponse::Task(theirs) = d.handle(run_hi()).await else {
            panic!("run failed")
        };
        let store = d.store.clone();
        let socket = serving(d).await;
        let own = mine.display_id();
        let resp = ask_as(
            &socket,
            &IpcRequest::TaskDone {
                id: theirs.id,
                summary: None,
            },
            &own,
        )
        .await;
        assert!(
            matches!(&resp, IpcResponse::Error { code, .. } if code == "agent_refused"),
            "{resp:?}"
        );
        assert!(!store.get_task(theirs.id).unwrap().unwrap().ended);
        let resp = ask_as(
            &socket,
            &IpcRequest::TaskDone {
                id: mine.id,
                summary: None,
            },
            &own,
        )
        .await;
        let IpcResponse::Task(t) = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(t.state, TaskState::Done);
        assert!(t.ended);
        assert!(t.finished_at.is_some());
        let row = store.get_task(mine.id).unwrap().unwrap();
        assert_eq!(row.state, TaskState::Done);
        assert!(row.ended);
        assert_eq!(
            store.get_task(theirs.id).unwrap().unwrap().state,
            theirs.state
        );
    }

    /// `req`, a `Run`, with `task run --summary mode`.
    fn with_summary(mut req: IpcRequest, mode: crate::task::SummaryMode) -> IpcRequest {
        if let IpcRequest::Run { summary, .. } = &mut req {
            *summary = Some(mode);
        }
        req
    }

    /// A task that requires a summary refuses its own agent's bare `task
    /// done`, and takes a person's, whose round says it was ended by hand.
    #[tokio::test]
    async fn a_required_summary_holds_the_agent_not_a_person() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let run = || with_summary(run_hi(), crate::task::SummaryMode::Require);
        let IpcResponse::Task(mine) = d.handle(run()).await else {
            panic!("run failed")
        };
        let IpcResponse::Task(other) = d.handle(run()).await else {
            panic!("run failed")
        };
        assert_eq!(mine.spec.summary, crate::task::SummaryMode::Require);
        let store = d.store.clone();
        let socket = serving(d).await;
        let bare = |id| IpcRequest::TaskDone { id, summary: None };
        let resp = ask_as(&socket, &bare(mine.id), &mine.display_id()).await;
        assert_eq!(
            code_of(&resp),
            Some(crate::task::SUMMARY_REQUIRED),
            "{resp:?}"
        );
        assert!(!store.get_task(mine.id).unwrap().unwrap().ended);
        let with_one = IpcRequest::TaskDone {
            id: mine.id,
            summary: Some("done\npushed".into()),
        };
        let resp = ask_as(&socket, &with_one, &mine.display_id()).await;
        assert!(
            matches!(resp, IpcResponse::Task(ref t) if t.ended),
            "{resp:?}"
        );
        // A person, with no task of their own.
        let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
        let (r, mut w) = stream.into_split();
        let line = crate::ipc::request_line(&bare(other.id), None).unwrap();
        w.write_all(line.as_bytes()).await.unwrap();
        let mut reply = String::new();
        BufReader::new(r).read_line(&mut reply).await.unwrap();
        let resp: IpcResponse = serde_json::from_str(reply.trim()).unwrap();
        let IpcResponse::Task(t) = resp else {
            panic!("{resp:?}")
        };
        assert!(t.ended);
        assert_eq!(t.summary.unwrap().text, crate::task::ENDED_BY_HAND);
    }

    /// A task's `summary` comes from `task run --summary`, else its job's,
    /// else its flock's, else `[defaults]` (unset here: `ask`), and is
    /// stored on the task.
    #[tokio::test]
    async fn the_summary_setting_is_settled_when_a_task_is_queued() {
        use crate::task::SummaryMode;
        let mut flock = home_and_work();
        flock.flocks.push(crate::config::flock::FlockEntry {
            name: "quiet".into(),
            summary: Some(SummaryMode::Off),
            ..Default::default()
        });
        let (d, _tmp) = daemon_with_flock(
            flock,
            &[("h", 2, FakeHerdr::new()), ("w", 2, FakeHerdr::new())],
        )
        .await;
        let settled = |resp: IpcResponse| match resp {
            IpcResponse::Task(t) => t.spec.summary,
            other => panic!("{other:?}"),
        };
        assert_eq!(
            settled(d.handle(run_in(None, None)).await),
            SummaryMode::Ask
        );
        assert_eq!(
            settled(d.handle(run_in(Some("quiet"), None)).await),
            SummaryMode::Off
        );
        let asked = with_summary(run_in(Some("quiet"), None), SummaryMode::Require);
        assert_eq!(settled(d.handle(asked).await), SummaryMode::Require);
        let submit = |job: &str, dispatch: serde_json::Value| IpcRequest::JobSubmit {
            job: job.into(),
            dispatch,
            prompt: "p".into(),
            items: vec![serde_json::json!({"key": "k"})],
        };
        let job_task = |resp: IpcResponse| match resp {
            IpcResponse::JobSubmitted { tasks, .. } => tasks[0].spec.summary,
            other => panic!("{other:?}"),
        };
        let from_flock = d
            .handle(submit("j1", serde_json::json!({"flock": "quiet"})))
            .await;
        assert_eq!(job_task(from_flock), SummaryMode::Off);
        let from_job = d
            .handle(submit(
                "j2",
                serde_json::json!({"flock": "quiet", "summary": "require"}),
            ))
            .await;
        assert_eq!(job_task(from_job), SummaryMode::Require);
    }

    /// A task with no pane has nothing to end.
    #[tokio::test]
    async fn ending_a_task_with_no_pane_is_refused() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let resp = d
            .handle(IpcRequest::TaskDone {
                id: 42,
                summary: None,
            })
            .await;
        assert!(
            matches!(&resp, IpcResponse::Error { code, .. } if code == "task_not_found"),
            "{resp:?}"
        );
        let IpcResponse::Task(t) = d.handle(run_hi()).await else {
            panic!("run failed")
        };
        d.handle(IpcRequest::TaskClose {
            id: t.id,
            remove_worktree: false,
        })
        .await;
        let resp = d
            .handle(IpcRequest::TaskDone {
                id: t.id,
                summary: None,
            })
            .await;
        assert!(
            matches!(&resp, IpcResponse::Error { code, .. } if code == "task_not_live"),
            "{resp:?}"
        );
    }

    /// An orchestrator task, queued as a person would, and its agent name.
    async fn orchestrator(d: &Daemon) -> String {
        let IpcResponse::Task(t) = d.handle(run_hi_as(TaskRole::Orchestrator)).await else {
            panic!("run failed")
        };
        assert_eq!(t.role, TaskRole::Orchestrator);
        t.display_id()
    }

    fn code_of(resp: &IpcResponse) -> Option<&str> {
        match resp {
            IpcResponse::Error { code, .. } => Some(code),
            _ => None,
        }
    }

    /// A job file `name` in the head's jobs directory, enabled.
    fn write_job(tmp: &tempfile::TempDir, name: &str) -> std::path::PathBuf {
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        std::fs::create_dir_all(paths.jobs_dir()).unwrap();
        let file = paths.jobs_dir().join(format!("{name}.toml"));
        std::fs::write(
            &file,
            "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"p {{ task.id }}\"\n",
        )
        .unwrap();
        file
    }

    /// An orchestrator may run, retry, send to and close tasks and enable
    /// and disable a job, all from its own pane with `agents_change_fleet`
    /// off.
    #[tokio::test]
    async fn an_orchestrator_may_run_retry_send_close_enable_and_disable() {
        let (d, tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        assert!(!d.fleet().agents_change_fleet());
        let me = orchestrator(&d).await;
        let own = me.as_str();

        let resp = d.handle_from(run_hi(), Some(own)).await;
        let IpcResponse::Task(worker) = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(worker.role, TaskRole::Agent);

        let send = IpcRequest::TaskSend {
            id: worker.id,
            input: crate::machine::SendInput {
                text: Some("go on".into()),
                enter: true,
                ..Default::default()
            },
        };
        let resp = d.handle_from(send, Some(own)).await;
        assert_ne!(code_of(&resp), Some("agent_refused"), "{resp:?}");

        let mut failed = d.store.get_task(worker.id).unwrap().unwrap();
        failed.state = TaskState::Failed;
        d.store.update_task(&mut failed).unwrap();
        let resp = d
            .handle_from(
                IpcRequest::TaskRetry {
                    id: worker.id,
                    place: None,
                },
                Some(own),
            )
            .await;
        let IpcResponse::Task(retry) = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(retry.retry_of, Some(worker.id));

        let file = write_job(&tmp, "clock");
        let resp = d
            .handle_from(
                IpcRequest::JobSetEnabled {
                    name: "clock".into(),
                    enabled: false,
                },
                Some(own),
            )
            .await;
        assert!(matches!(resp, IpcResponse::Text(_)), "{resp:?}");
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .contains("enabled = false")
        );
        let resp = d
            .handle_from(
                IpcRequest::JobSetEnabled {
                    name: "clock".into(),
                    enabled: true,
                },
                Some(own),
            )
            .await;
        assert!(matches!(resp, IpcResponse::Text(_)), "{resp:?}");
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .contains("enabled = true")
        );

        let resp = d
            .handle_from(
                IpcRequest::TaskClose {
                    id: retry.id,
                    remove_worktree: false,
                },
                Some(own),
            )
            .await;
        let IpcResponse::Task(closed) = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(closed.state, TaskState::Closed);
    }

    /// An orchestrator file `name` in the head's orchestrators directory.
    fn write_orchestrator(tmp: &tempfile::TempDir, name: &str) -> Paths {
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        std::fs::create_dir_all(paths.orchestrators_dir()).unwrap();
        std::fs::write(
            paths.orchestrators_dir().join(format!("{name}.toml")),
            "kind = \"scheduled\"\nevery = \"1h\"\npre = [\"./pre.sh\"]\nprompt = \"p\"\n",
        )
        .unwrap();
        paths
    }

    fn script(name: &str) -> crate::ipc::Caller {
        crate::ipc::Caller {
            task: None,
            orchestrator: Some(name.into()),
        }
    }

    /// A pre or post script (`PASTOR_ORCHESTRATOR`) gets the orchestrator
    /// role's table: `task run` passes, `machine add` and making an
    /// orchestrator do not, and a name the head has no file for is refused
    /// every change.
    #[tokio::test]
    async fn an_orchestrators_script_gets_the_roles_table() {
        let (d, tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        write_orchestrator(&tmp, "merge");
        let resp = d.handle_as(run_hi(), &script("merge")).await;
        let IpcResponse::Task(t) = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(t.role, TaskRole::Agent);
        let add = IpcRequest::MachineAdd {
            machine: machine("x", 1),
        };
        let resp = d.handle_as(add, &script("merge")).await;
        let IpcResponse::Error { code, message } = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(code, "agent_refused");
        assert!(message.contains("orchestrator merge"), "{message}");
        assert!(d.fleet().flock().get("x").is_none());
        let resp = d
            .handle_as(run_hi_as(TaskRole::Orchestrator), &script("merge"))
            .await;
        assert_eq!(code_of(&resp), Some("role_refused"), "{resp:?}");
        let resp = d.handle_as(run_hi(), &script("ghost")).await;
        let IpcResponse::Error { code, message } = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(code, "agent_refused");
        assert!(message.contains("ghost"), "{message}");
        let list = IpcRequest::List {
            filter: TaskFilter::default(),
        };
        assert!(matches!(
            d.handle_as(list, &script("ghost")).await,
            IpcResponse::Tasks(_)
        ));
    }

    /// A request that names both a task and an orchestrator is the task's:
    /// a plain agent that sets `PASTOR_ORCHESTRATOR` gains nothing.
    #[tokio::test]
    async fn a_caller_with_both_variables_gets_the_tasks_rights() {
        let (d, tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        write_orchestrator(&tmp, "merge");
        let IpcResponse::Task(agent) = d.handle(run_hi()).await else {
            panic!("run failed")
        };
        let both = crate::ipc::Caller {
            task: Some(agent.display_id()),
            orchestrator: Some("merge".into()),
        };
        let resp = d.handle_as(run_hi(), &both).await;
        let IpcResponse::Error { code, message } = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(code, "agent_refused");
        assert!(message.contains("is an agent pastor started"), "{message}");
        let note = IpcRequest::OrchestratorNote {
            name: Some("merge".into()),
            text: "x".into(),
        };
        assert_eq!(
            code_of(&d.handle_as(note, &both).await),
            Some("agent_refused")
        );
    }

    /// The handover note: a script keeps its own orchestrator's only, the
    /// agent an orchestrator file started keeps that one's, a hand-started
    /// orchestrator has none, and a person names it.
    #[tokio::test]
    async fn each_orchestrator_keeps_only_its_own_note() {
        let (d, tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let paths = write_orchestrator(&tmp, "merge");
        write_orchestrator(&tmp, "night");
        let note = |name: Option<&str>, text: &str| IpcRequest::OrchestratorNote {
            name: name.map(str::to_string),
            text: text.into(),
        };
        let resp = d.handle_as(note(None, "from pre"), &script("merge")).await;
        assert!(matches!(resp, IpcResponse::Text(_)), "{resp:?}");
        assert_eq!(
            crate::orchestrator::read_note(&paths, "merge").as_deref(),
            Some("from pre")
        );
        let resp = d
            .handle_as(note(Some("night"), "x"), &script("merge"))
            .await;
        assert_eq!(code_of(&resp), Some("agent_refused"), "{resp:?}");

        let me = orchestrator(&d).await;
        let resp = d.handle_from(note(None, "x"), Some(&me)).await;
        assert_eq!(code_of(&resp), Some("not_an_orchestrator"), "{resp:?}");
        // As if merge's last run had started it.
        let id = crate::task::parse_task_id(&me).unwrap();
        std::fs::create_dir_all(paths.orchestrator_state_dir("merge")).unwrap();
        std::fs::write(
            paths.orchestrator_state_dir("merge").join("state.json"),
            serde_json::json!({ "task": id }).to_string(),
        )
        .unwrap();
        d.handle(IpcRequest::OrchestratorList).await;
        let resp = d.handle_from(note(None, "from the agent"), Some(&me)).await;
        assert!(matches!(resp, IpcResponse::Text(_)), "{resp:?}");
        assert_eq!(
            crate::orchestrator::read_note(&paths, "merge").as_deref(),
            Some("from the agent")
        );
        let resp = d.handle_from(note(Some("night"), "x"), Some(&me)).await;
        assert_eq!(code_of(&resp), Some("agent_refused"), "{resp:?}");

        let resp = d.handle(note(None, "x")).await;
        assert!(code_of(&resp).is_some(), "{resp:?}");
        let resp = d.handle(note(Some("night"), "by hand")).await;
        assert!(matches!(resp, IpcResponse::Text(_)), "{resp:?}");
        assert_eq!(
            crate::orchestrator::read_note(&paths, "night").as_deref(),
            Some("by hand")
        );
    }

    /// Everything else that changes the fleet is refused an orchestrator,
    /// with a message that names the role; a plain agent is still refused
    /// what an orchestrator may do.
    #[tokio::test]
    async fn an_orchestrator_is_refused_every_other_fleet_change() {
        let (d, tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let me = orchestrator(&d).await;
        let IpcResponse::Task(worker) = d.handle(run_hi()).await else {
            panic!("run failed")
        };
        let file = write_job(&tmp, "clock");
        let refused = [
            IpcRequest::FilePut {
                file: "job:clock".into(),
                text: String::new(),
                base_hash: String::new(),
            },
            IpcRequest::MachineAdd {
                machine: crate::config::flock::MachineConfig {
                    pull: false,
                    name: "m".into(),
                    local: true,
                    ssh: None,
                    command: None,
                    session: "default".into(),
                    max_agents: 1,
                    job_slots: 1,
                    burst: 1,
                    tags: vec![],
                    flock: None,
                    agent: None,
                    agent_args: None,
                    model: None,
                    priority: None,
                    agents: Default::default(),
                    profile: None,
                    description: None,
                },
            },
            IpcRequest::TaskPrune {
                states: vec![TaskState::Done],
                older_than_secs: 0,
            },
            IpcRequest::JobRun {
                name: "clock".into(),
            },
            IpcRequest::Reload,
        ];
        for req in refused {
            let resp = d.handle_from(req.clone(), Some(&me)).await;
            let IpcResponse::Error { code, message } = resp else {
                panic!("{req:?} was not refused: {resp:?}")
            };
            assert_eq!(code, "agent_refused", "{req:?}");
            assert!(message.contains("orchestrator"), "{message}");
            assert!(message.contains(&me), "{message}");
        }
        assert_eq!(
            d.store.get_task(worker.id).unwrap().unwrap().state,
            worker.state
        );
        assert!(!std::fs::read_to_string(&file).unwrap().contains("enabled"));

        let agent = worker.display_id();
        let resp = d.handle_from(run_hi(), Some(&agent)).await;
        assert_eq!(code_of(&resp), Some("agent_refused"), "{resp:?}");
        for enabled in [false, true] {
            let resp = d
                .handle_from(
                    IpcRequest::JobSetEnabled {
                        name: "clock".into(),
                        enabled,
                    },
                    Some(&agent),
                )
                .await;
            assert_eq!(code_of(&resp), Some("agent_refused"), "{resp:?}");
        }
        let resp = d
            .handle_from(
                IpcRequest::TaskClose {
                    id: worker.id,
                    remove_worktree: false,
                },
                Some(&agent),
            )
            .await;
        assert_eq!(code_of(&resp), Some("agent_refused"), "{resp:?}");
        assert_eq!(
            d.store.get_task(worker.id).unwrap().unwrap().state,
            worker.state
        );
        // A task the head does not know is a plain agent.
        let resp = d.handle_from(run_hi(), Some("t-999")).await;
        assert_eq!(code_of(&resp), Some("agent_refused"), "{resp:?}");
    }

    /// No task makes an orchestrator, not an orchestrator and not with
    /// `agents_change_fleet` on: neither with `--role orchestrator` nor by
    /// retrying one. A person may.
    #[tokio::test]
    async fn only_a_person_makes_an_orchestrator() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        d.fleet().set_config(&PastorConfig {
            agents_change_fleet: true,
            ..test_config()
        });
        let me = orchestrator(&d).await;
        let IpcResponse::Task(worker) = d.handle(run_hi()).await else {
            panic!("run failed")
        };
        let run_orch = run_hi_as(TaskRole::Orchestrator);
        for caller in [me.as_str(), &worker.display_id()] {
            let resp = d.handle_from(run_orch.clone(), Some(caller)).await;
            let IpcResponse::Error { code, message } = resp else {
                panic!("{resp:?}")
            };
            assert_eq!(code, "role_refused");
            assert!(message.contains("orchestrator"), "{message}");
        }
        let id = crate::task::parse_task_id(&me).unwrap();
        let mut failed = d.store.get_task(id).unwrap().unwrap();
        failed.state = TaskState::Failed;
        d.store.update_task(&mut failed).unwrap();
        let retry = IpcRequest::TaskRetry { id, place: None };
        let resp = d
            .handle_from(retry.clone(), Some(&worker.display_id()))
            .await;
        assert_eq!(code_of(&resp), Some("role_refused"), "{resp:?}");
        let before = d.store.list_tasks(&TaskFilter::default()).unwrap().len();
        assert_eq!(before, 2);
        let IpcResponse::Task(again) = d.handle_from(retry, None).await else {
            panic!("a person's retry failed")
        };
        assert_eq!(again.role, TaskRole::Orchestrator);
        // With agents_change_fleet on, a plain agent may still close tasks.
        let resp = d
            .handle_from(
                IpcRequest::TaskClose {
                    id: worker.id,
                    remove_worktree: false,
                },
                Some(&worker.display_id()),
            )
            .await;
        assert_ne!(code_of(&resp), Some("agent_refused"), "{resp:?}");
    }

    /// The refusal names every operation it covers, so its advice holds for
    /// whichever one was refused.
    #[test]
    fn the_agent_refusal_names_every_refused_operation() {
        let message = agent_refusal("t-9");
        for op in [
            "run",
            "send",
            "attach",
            "retry",
            "close",
            "prune",
            "tick",
            "dry",
            "reload",
            "install",
            "link",
            "uninstall",
            "unlink",
            "connectors",
            "machines",
            "flocks",
            "serve",
            "set up",
            "open",
            "pastor.toml",
            "jobs",
        ] {
            assert!(message.contains(op), "{op}: {message}");
        }
    }

    /// `agents_change_fleet = true` in pastor.toml turns the guard off.
    #[tokio::test]
    async fn agents_change_fleet_lets_an_agent_run_a_task() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        d.fleet().set_config(&PastorConfig {
            agents_change_fleet: true,
            ..test_config()
        });
        let socket = serving(d).await;
        let resp = ask_from_task(&socket, &run_hi()).await;
        assert!(matches!(resp, IpcResponse::Task(_)), "{resp:?}");
    }

    /// A request is one line. A client that sends more than the cap, or
    /// nothing at all, must not hold memory or a file descriptor forever:
    /// the first is answered with an error, the second is hung up on.
    #[tokio::test]
    async fn ipc_requests_are_bounded_in_size_and_time() {
        let read = |data: &'static [u8], max| read_request(data, max, Duration::from_secs(5));
        assert_eq!(read(b"{\"a\":1}\nrest", 16).await.unwrap(), "{\"a\":1}\n");
        assert_eq!(read(b"no newline", 16).await.unwrap(), "no newline");
        assert_eq!(
            read(b"0123456789abcdef\n", 16).await.unwrap(),
            "0123456789abcdef\n"
        );
        assert!(matches!(
            read(b"0123456789abcdefg\n", 16).await,
            Err(RequestReadError::TooLarge)
        ));
        // Malformed UTF-8 is refused, never patched into U+FFFD and parsed.
        assert!(matches!(
            read(b"{\"a\":\"\xff\"}\n", 16).await,
            Err(RequestReadError::NotUtf8)
        ));
        let (_client, server) = tokio::io::duplex(64);
        let started = Instant::now();
        let got = read_request(server, 16, Duration::from_millis(50)).await;
        assert!(matches!(got, Err(RequestReadError::TimedOut)), "{got:?}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn an_oversized_ipc_request_is_refused_and_the_daemon_lives_on() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let socket = d.socket_path();
        tokio::spawn(d.run());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !crate::ipc::daemon_running(&socket).await {
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
        let (r, mut w) = stream.into_split();
        let big = vec![b'x'; MAX_IPC_REQUEST + 1];
        // The daemon may hang up before taking it all; that is fine.
        let _ = w.write_all(&big).await;
        let mut line = String::new();
        BufReader::new(r).read_line(&mut line).await.unwrap();
        assert!(line.contains("request_too_large"), "{line}");
        assert!(crate::ipc::daemon_running(&socket).await);
    }

    /// `run` must replace a stale socket file left behind by an unclean shutdown
    /// (nothing is listening on it) instead of refusing to start. Shutdown-time
    /// removal of the socket is not exercised here: `run` only exits on
    /// ctrl-c/SIGTERM/SIGHUP, and sending one of those to this process would
    /// affect the whole test process. `tests/cli.rs` covers the SIGTERM path
    /// against a real `pastor serve` child instead.
    #[tokio::test]
    async fn run_replaces_a_stale_socket_file() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let socket = d.socket_path();
        std::fs::write(&socket, b"not a socket").unwrap();
        tokio::spawn(d.run());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !crate::ipc::daemon_running(&socket).await {
            assert!(Instant::now() < deadline, "daemon never started");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// A daemon that is mid-request and not answering pings (or a listener that
    /// never reads at all) must not have its socket unlinked by a second `pastor
    /// serve`: staleness is decided by whether the connect is refused, not by
    /// whether anything replies within the probe window.
    #[tokio::test]
    async fn run_refuses_to_replace_an_unresponsive_listener() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let socket = d.socket_path();
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            // Accept connections and never read or reply: unresponsive, not dead.
            let mut kept = Vec::new();
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => kept.push(stream),
                    Err(_) => return,
                }
            }
        });

        let err = tokio::time::timeout(Duration::from_secs(10), d.run())
            .await
            .expect("run must not hang waiting on the other daemon")
            .unwrap_err();
        assert!(err.to_string().contains("did not respond"), "{err}");
        assert!(
            socket.exists(),
            "an unresponsive daemon's socket file must not be removed"
        );
    }

    /// A second `pastor serve` must own the socket before it spawns a single
    /// machine actor: reconciling and mutating the shared database for up to
    /// the probe timeout before bailing (the old order) is the bug. With a
    /// live, responding daemon already on the socket, `bind_and_start` must
    /// fail before its own connectors are ever contacted.
    #[tokio::test]
    async fn bind_and_start_bails_before_spawning_actors_when_a_daemon_is_already_running() {
        let (first, tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let socket = first.socket_path();
        tokio::spawn(first.run());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !crate::ipc::daemon_running(&socket).await {
            assert!(Instant::now() < deadline, "first daemon never started");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        let flock = Flock {
            flocks: vec![],
            machines: vec![machine("a", 2)],
        };
        let fake = FakeHerdr::new();
        let connect = factory(&[("a", fake.clone())]);
        let on_disk = ConfigFingerprint::sample(&paths);
        let err = match Daemon::bind_and_start(
            paths,
            PastorConfig::default(),
            flock,
            on_disk,
            Some(connect),
        )
        .await
        {
            Ok(_) => panic!("expected bind_and_start to bail on a live socket"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("already running"), "{err}");
        assert!(
            fake.requests().is_empty(),
            "the second daemon's machine actor must never have contacted its connector"
        );
    }

    /// CI run 36111067337: a daemon started with a flock that is not on disk
    /// (no flock.toml at all) must keep that flock until the files change.
    /// The scheduler's first pass used to read the missing file as an empty
    /// flock and stop every machine the caller had just applied, so a task
    /// queued right after start never dispatched. After a `Tick` reply the
    /// scheduler has reloaded config at least once, so no timing is involved.
    #[tokio::test]
    async fn the_first_pass_keeps_the_flock_the_daemon_started_with() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        let flock = Flock {
            flocks: vec![],
            machines: vec![machine("a", 1)],
        };
        let on_disk = ConfigFingerprint::sample(&paths);
        let d = Daemon::start(paths, test_config(), flock, on_disk, Some(factory(&[])))
            .await
            .unwrap();
        d.scheduler().tick(None, true).await.unwrap();
        assert!(
            d.fleet().get("a").is_some(),
            "unchanged (absent) config files must not replace the flock the daemon started with"
        );
    }

    /// Rows from before flocks join the default flock of the flock file the
    /// head starts with, whatever it is named.
    #[tokio::test]
    async fn the_head_puts_flockless_rows_in_the_default_flock() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        paths.ensure().unwrap();
        {
            let store = Store::open(&paths.db_file()).unwrap();
            store
                .insert_task(NewTask {
                    description: None,
                    job: "run".into(),
                    item: serde_json::Value::Null,
                    prompt: "p".into(),
                    spec: spec(),
                    flock: "x".into(),
                })
                .unwrap();
            store.execute_raw("UPDATE tasks SET flock = NULL");
        }
        let flock = Flock {
            flocks: vec![crate::config::flock::FlockEntry {
                name: "personal".into(),
                default: true,
                ..Default::default()
            }],
            machines: vec![machine("a", 1)],
        };
        let on_disk = ConfigFingerprint::sample(&paths);
        let d = Daemon::start(paths, test_config(), flock, on_disk, Some(factory(&[])))
            .await
            .unwrap();
        let t = d.store().get_task(1).unwrap().unwrap();
        assert_eq!(t.flock.as_deref(), Some("personal"));
    }

    /// Two passes at once (a tick and a `pastor task run`) against one machine with
    /// one free slot: the lock makes the second wait and see the first's task.
    #[tokio::test]
    async fn concurrent_dispatch_passes_do_not_over_dispatch() {
        let fake = FakeHerdr::new();
        fake.set_ready_after(Duration::from_millis(200));
        let (d, _tmp) = daemon(&[("a", 1, fake.clone())]).await;
        for p in ["1", "2"] {
            d.store()
                .insert_task(NewTask {
                    description: None,
                    job: "run".into(),
                    item: serde_json::Value::Null,
                    prompt: p.into(),
                    spec: spec(),
                    flock: "default".into(),
                })
                .unwrap();
        }
        let fleet = d.fleet();
        tokio::join!(fleet.dispatch_queued(), fleet.dispatch_queued());
        let states: Vec<TaskState> = d
            .store()
            .list_tasks(&TaskFilter::default())
            .unwrap()
            .iter()
            .map(|t| t.state)
            .collect();
        assert_eq!(
            states.iter().filter(|s| **s == TaskState::Running).count(),
            1,
            "{states:?}"
        );
        assert_eq!(
            states.iter().filter(|s| **s == TaskState::Queued).count(),
            1,
            "{states:?}"
        );
        assert_eq!(
            fake.agents().len(),
            1,
            "one agent on a max_agents = 1 machine"
        );
    }

    /// A machine with max_agents = 1, one job slot and one burst: a second
    /// `task run` task waits, a job task takes the job slot, and a critical
    /// task goes past the full shared slot on burst.
    #[tokio::test]
    async fn dispatch_uses_job_slots_and_burst() {
        let fake = FakeHerdr::new();
        let flock = Flock {
            flocks: vec![],
            machines: vec![MachineConfig {
                job_slots: 1,
                burst: 1,
                ..machine("a", 1)
            }],
        };
        let (d, _tmp) = daemon_with_flock(flock, &[("a", 1, fake.clone())]).await;
        let insert = |job: &str, p: &str| {
            d.store()
                .insert_task(NewTask {
                    job: job.into(),
                    item: serde_json::Value::Null,
                    prompt: p.into(),
                    spec: spec(),
                    flock: "default".into(),
                    description: None,
                })
                .unwrap()
        };
        let first = insert("run", "1");
        let second = insert("run", "2");
        let job = insert("nightly", "3");
        let fleet = d.fleet();
        fleet.dispatch_queued().await;
        let state = |id: i64| d.store().get_task(id).unwrap().unwrap().state;
        assert_eq!(state(first.id), TaskState::Running);
        assert_eq!(state(second.id), TaskState::Queued, "held at max_agents");
        assert_eq!(state(job.id), TaskState::Running, "in the job slot");

        let critical = insert("run", "4");
        d.store()
            .set_priority(critical.id, Priority::Critical, "test")
            .unwrap();
        fleet.dispatch_queued().await;
        assert_eq!(state(critical.id), TaskState::Running, "on burst");
        assert_eq!(state(second.id), TaskState::Queued);
        assert_eq!(fake.agents().len(), 3);
    }

    /// One machine in two flocks: a flock at its number there waits with a
    /// note saying so, the task behind it from the other flock goes, and
    /// the machine's status carries both flocks with their live tasks.
    #[tokio::test]
    async fn dispatch_keeps_a_flock_to_its_number_on_a_machine() {
        let fake = FakeHerdr::new();
        let flock: Flock = toml::from_str(
            "[[flock]]\nname = \"home\"\ndefault = true\nmachines = { desk = 3 }\n\n\
             [[flock]]\nname = \"work\"\nmachines = { desk = 1 }\n\n\
             [[machine]]\nname = \"desk\"\nlocal = true\nmax_agents = 3\njob_slots = 1\nburst = 1\n",
        )
        .unwrap();
        flock.validate().unwrap();
        let (d, _tmp) = daemon_with_flock(flock, &[("desk", 3, fake.clone())]).await;
        let insert = |job: &str, flock: &str| {
            d.store()
                .insert_task(NewTask {
                    job: job.into(),
                    item: serde_json::Value::Null,
                    prompt: "p".into(),
                    spec: spec(),
                    flock: flock.into(),
                    description: None,
                })
                .unwrap()
        };
        let first = insert("run", "work");
        let second = insert("nightly", "work");
        let home = insert("run", "home");
        let fleet = d.fleet();
        fleet.dispatch_queued().await;
        let task = |id: i64| d.store().get_task(id).unwrap().unwrap();
        assert_eq!(task(first.id).state, TaskState::Running);
        assert_eq!(
            task(second.id).state,
            TaskState::Queued,
            "no job slot past the number"
        );
        assert_eq!(
            task(second.id).error.as_deref(),
            Some("waiting for a machine: flock work is at 1 of 1 on desk")
        );
        assert_eq!(
            task(home.id).state,
            TaskState::Running,
            "the next task goes"
        );
        let status = fleet
            .statuses()
            .into_iter()
            .find(|s| s.name == "desk")
            .unwrap();
        assert_eq!(status.flock.as_deref(), Some("home"));
        let seats: Vec<(String, Option<u32>, usize)> = status
            .flocks
            .iter()
            .map(|f| (f.name.clone(), f.max, f.live))
            .collect();
        assert_eq!(
            seats,
            [("home".into(), Some(3), 1), ("work".into(), Some(1), 1)]
        );

        // The flock's task ends; the waiting one goes and its note clears.
        fleet
            .get("desk")
            .unwrap()
            .close(first.id, false)
            .await
            .unwrap();
        fleet.dispatch_queued().await;
        assert_eq!(task(second.id).state, TaskState::Running);
        assert_eq!(task(second.id).error, None);
    }

    /// `FileGet` and `FilePut` act on the head's own files, named only as
    /// `flock`, `config` or `job:<name>`, and write only a valid edit made
    /// from the file as it is now.
    #[tokio::test]
    async fn file_requests_edit_the_heads_files() {
        let (d, tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        std::fs::create_dir_all(paths.jobs_dir()).unwrap();
        let job = "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"p {{ task.id }}\"\n";
        let file = paths.jobs_dir().join("clock.toml");
        std::fs::write(&file, job).unwrap();
        let code = |r: IpcResponse| match r {
            IpcResponse::Error { code, .. } => code,
            other => panic!("{other:?}"),
        };
        for bad in ["/etc/passwd", "job:../pastor", "jobs"] {
            let r = d.handle(IpcRequest::FileGet { file: bad.into() }).await;
            assert!(
                ["invalid_file", "job_not_found"].contains(&code(r).as_str()),
                "{bad}"
            );
        }
        let r = d
            .handle(IpcRequest::FileGet {
                file: "job:ghost".into(),
            })
            .await;
        assert_eq!(code(r), "job_not_found");

        let IpcResponse::File(got) = d
            .handle(IpcRequest::FileGet {
                file: "job:clock".into(),
            })
            .await
        else {
            panic!()
        };
        assert_eq!(got.text, job);
        assert_eq!(got.hash, crate::edit::hash(job));
        let put = |text: &str, base_hash: &str| IpcRequest::FilePut {
            file: "job:clock".into(),
            text: text.into(),
            base_hash: base_hash.into(),
        };
        let r = d.handle(put(&job.replace("1h", "soon"), &got.hash)).await;
        assert_eq!(code(r), "invalid_edit");
        let r = d
            .handle(put(&job.replace("1h", "2h"), &crate::edit::hash("old")))
            .await;
        assert_eq!(code(r), "edit_conflict");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), job);
        let IpcResponse::Text(msg) = d.handle(put(&job.replace("1h", "2h"), &got.hash)).await
        else {
            panic!()
        };
        assert!(msg.starts_with("saved "), "{msg}");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            job.replace("1h", "2h")
        );
    }

    /// `JobTask`, from a headless serve: rendered with this head's id,
    /// queued as that job's task and dispatched, its key seen here too; an
    /// item seen already answers its task again, or `already_seen` once
    /// that is gone; one that would climb out of its branch is refused.
    #[tokio::test]
    async fn a_job_task_is_rendered_queued_and_dispatched_here() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let req = |key: &str, branch: Option<&str>| IpcRequest::JobTask {
            description: None,
            job: "sweep".into(),
            flock: None,
            agent: AgentChoice::default(),
            prompt: "sweep {{ item.key }} for {{ job.name }} as {{ task.id }}".into(),
            spec: crate::task::DispatchSpec {
                repo: Some("/tmp".into()),
                branch: branch.map(Into::into),
                ..spec()
            },
            item: serde_json::json!({ "key": key }),
        };
        let IpcResponse::Task(t) = d.handle(req("k1", None)).await else {
            panic!()
        };
        assert_eq!(t.job, "sweep");
        assert_eq!(t.prompt, format!("sweep k1 for sweep as t-{}", t.id));
        assert_eq!(t.state, TaskState::Running);
        assert!(d.store().is_seen("sweep", "k1").unwrap());
        // A resubmitted key, as after a lost reply, gets the same task back.
        let IpcResponse::Task(again) = d.handle(req("k1", None)).await else {
            panic!()
        };
        assert_eq!(again.id, t.id);
        assert_eq!(
            d.store().list_tasks(&TaskFilter::default()).unwrap().len(),
            1
        );
        d.store().mark_seen("sweep", "gone", None).unwrap();
        assert_eq!(
            error_code(d.handle(req("gone", None)).await),
            crate::ipc::ALREADY_SEEN
        );
        assert_eq!(
            error_code(d.handle(req("..", Some("b/{{ item.key }}"))).await),
            "item_rejected"
        );
        let IpcRequest::JobTask { spec, .. } = req("k2", None) else {
            panic!()
        };
        let keyless = IpcRequest::JobTask {
            description: None,
            job: "sweep".into(),
            flock: None,
            agent: AgentChoice::default(),
            prompt: "p".into(),
            spec,
            item: serde_json::json!({}),
        };
        assert_eq!(error_code(d.handle(keyless).await), "invalid_request");
    }

    #[tokio::test]
    async fn scheduler_ipc_ticks_lists_and_reloads() {
        let (d, tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        std::fs::create_dir_all(paths.jobs_dir()).unwrap();
        std::fs::write(
            paths.jobs_dir().join("clock.toml"),
            "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"tick {{ item.key }} {{ task.id }}\"\n",
        )
        .unwrap();
        let IpcResponse::Jobs(jobs) = d.handle(IpcRequest::Reload).await else {
            panic!()
        };
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "clock");
        let IpcResponse::Runs(runs) = d
            .handle(IpcRequest::Tick {
                job: Some("clock".into()),
                dry_run: true,
            })
            .await
        else {
            panic!()
        };
        assert_eq!(runs[0].outcome, RunOutcome::DryRun);
        assert_eq!(runs[0].created.len(), 1);
        let IpcResponse::Runs(runs) = d
            .handle(IpcRequest::Tick {
                job: Some("clock".into()),
                dry_run: false,
            })
            .await
        else {
            panic!()
        };
        assert_eq!(runs[0].outcome, RunOutcome::Ran);
        // The task was created and, the fleet having room, dispatched.
        let IpcResponse::Tasks(list) = d
            .handle(IpcRequest::List {
                filter: TaskFilter {
                    job: Some("clock".into()),
                    ..Default::default()
                },
            })
            .await
        else {
            panic!()
        };
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].state, TaskState::Running);
        assert!(
            list[0]
                .prompt
                .ends_with(&format!(" {}", list[0].display_id()))
        );
        let IpcResponse::Jobs(jobs) = d.handle(IpcRequest::JobList).await else {
            panic!()
        };
        assert!(jobs[0].last_result.as_deref().unwrap().starts_with("ok:"));
        let IpcResponse::Text(msg) = d
            .handle(IpcRequest::JobRun {
                name: "clock".into(),
            })
            .await
        else {
            panic!()
        };
        assert!(msg.contains("started"), "{msg}");
        let IpcResponse::Error { code, .. } = d
            .handle(IpcRequest::JobRun {
                name: "ghost".into(),
            })
            .await
        else {
            panic!()
        };
        assert_eq!(code, "job_not_found");
    }

    fn error_code(resp: IpcResponse) -> String {
        match resp {
            IpcResponse::Error { code, .. } => code,
            other => panic!("expected an error, got {other:?}"),
        }
    }

    async fn wait_until(what: &str, f: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !f() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn insert(d: &Daemon, state: TaskState) -> crate::task::Task {
        let mut t = d
            .store
            .insert_task(NewTask {
                description: None,
                job: "run".into(),
                item: serde_json::Value::Null,
                prompt: "p".into(),
                spec: spec(),
                flock: "default".into(),
            })
            .unwrap();
        if state != TaskState::Queued {
            t.state = state;
            t.machine = Some("a".into());
            t.finished_at = Some(chrono::Utc::now() - chrono::Duration::days(5));
            d.store.update_task(&mut t).unwrap();
        }
        t
    }

    /// An `[agents]` edit reaches the actors like a timing change does, so
    /// a reload restarts them with the new trust keys.
    #[test]
    fn machine_settings_carry_the_agents_trust_keys() {
        let mut config = test_config();
        assert_eq!(
            machine_settings(&config).agents.trust_keys("claude"),
            Some(vec!["Down".into(), "Enter".into()])
        );
        config.agents.0.insert(
            "codex".into(),
            crate::config::AgentDef {
                trust_keys: Some(vec!["Enter".into()]),
                ..Default::default()
            },
        );
        let settings = machine_settings(&config);
        assert_eq!(
            settings.agents.trust_keys("codex"),
            Some(vec!["Enter".into()])
        );
        assert_ne!(settings, machine_settings(&test_config()));
    }

    /// Agents on other machines are told where the head is; those on the
    /// head's own machine use its socket, and with no `head_address` nobody
    /// is told.
    #[test]
    fn only_agents_off_the_head_machine_get_the_head_address() {
        let machine = |text: &str| -> MachineConfig {
            toml::from_str(&format!("name = \"m\"\n{text}")).unwrap()
        };
        let local = machine("local = true");
        let remote = machine("ssh = \"pi\"");
        let config = PastorConfig {
            head_address: Some("user@head.example".into()),
            ..test_config()
        };
        let settings = machine_settings(&config);
        assert_eq!(
            actor_settings(&remote, &settings).head_address.as_deref(),
            Some("user@head.example")
        );
        assert_eq!(actor_settings(&local, &settings).head_address, None);
        let unset = machine_settings(&test_config());
        assert_eq!(actor_settings(&remote, &unset).head_address, None);
        assert_eq!(actor_settings(&local, &unset).head_address, None);
    }

    #[tokio::test]
    async fn task_send_reaches_a_live_task_and_refuses_the_rest() {
        let fake = FakeHerdr::new();
        let (d, _tmp) = daemon(&[("a", 2, fake.clone())]).await;
        let IpcResponse::Task(t) = d
            .handle(IpcRequest::Run {
                preempt: false,
                summary: None,
                role: Default::default(),
                description: None,
                prompt: "hi".into(),
                spec: spec(),
                flock: None,
                agent: None,
                priority: None,
            })
            .await
        else {
            panic!()
        };
        let keys = crate::machine::SendInput {
            keys: vec!["Enter".into()],
            ..Default::default()
        };
        let resp = d
            .handle(IpcRequest::TaskSend {
                id: t.id,
                input: keys.clone(),
            })
            .await;
        assert!(matches!(resp, IpcResponse::Text(_)), "{resp:?}");
        assert_eq!(
            fake.pane_input(t.pane_id.as_deref().unwrap()),
            [crate::herdr::fake::PaneInput::Keys(vec!["Enter".into()])]
        );
        let queued = insert(&d, TaskState::Queued);
        let done = insert(&d, TaskState::Done);
        for id in [queued.id, done.id] {
            let resp = d
                .handle(IpcRequest::TaskSend {
                    id,
                    input: keys.clone(),
                })
                .await;
            assert_eq!(error_code(resp), "task_not_live");
        }
        let resp = d
            .handle(IpcRequest::TaskSend {
                id: 99,
                input: keys.clone(),
            })
            .await;
        assert_eq!(error_code(resp), "task_not_found");
        let resp = d
            .handle(IpcRequest::TaskSend {
                id: t.id,
                input: crate::machine::SendInput::default(),
            })
            .await;
        assert_eq!(error_code(resp), "nothing_to_send");
        let resp = d
            .handle(IpcRequest::TaskSend {
                id: t.id,
                input: crate::machine::SendInput {
                    trust: true,
                    ..keys
                },
            })
            .await;
        assert_eq!(error_code(resp), "usage_error");
    }

    #[tokio::test]
    async fn retry_queues_a_copy_and_dispatches_it() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let failed = insert(&d, TaskState::Failed);
        let mut events = d.subscribe();
        let resp = d
            .handle(IpcRequest::TaskRetry {
                id: failed.id,
                place: None,
            })
            .await;
        let IpcResponse::Task(t) = resp else {
            panic!("{resp:?}")
        };
        assert_ne!(t.id, failed.id);
        assert_eq!(t.retry_of, Some(failed.id));
        assert_eq!(t.state, TaskState::Running, "dispatched right away");
        let ev = events.try_recv().unwrap();
        assert_eq!((ev.kind.as_str(), ev.task_id), ("task.queued", Some(t.id)));

        assert_eq!(
            error_code(
                d.handle(IpcRequest::TaskRetry {
                    id: t.id,
                    place: None
                })
                .await
            ),
            "not_retryable"
        );
        assert_eq!(
            error_code(
                d.handle(IpcRequest::TaskRetry {
                    id: 99,
                    place: None
                })
                .await
            ),
            "task_not_found"
        );
    }

    #[tokio::test]
    async fn retry_answers_a_storage_failure_as_store_error() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let failed = insert(&d, TaskState::Failed);
        // Reads work, only the insert fails: the way a full disk looks.
        d.store.execute_raw(
            "CREATE TRIGGER no_insert BEFORE INSERT ON tasks BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
        );
        assert_eq!(
            error_code(
                d.handle(IpcRequest::TaskRetry {
                    id: failed.id,
                    place: None
                })
                .await
            ),
            "store_error"
        );
    }

    /// The close read the task queued, then a dispatch claimed and started
    /// it before the row was written. Closing the row alone would leave the
    /// agent running behind a closed task; the close must lose to the claim
    /// and go through the machine that took it.
    #[tokio::test]
    async fn closing_a_queued_task_that_a_dispatch_just_claimed_routes_to_its_machine() {
        let fake = FakeHerdr::new();
        let (d, _tmp) = daemon(&[("a", 2, fake.clone())]).await;
        let IpcResponse::Task(t) = d
            .handle(IpcRequest::Run {
                preempt: false,
                summary: None,
                role: Default::default(),
                description: None,
                prompt: "x".into(),
                spec: spec(),
                flock: None,
                agent: None,
                priority: None,
            })
            .await
        else {
            panic!()
        };
        assert_eq!(t.machine.as_deref(), Some("a"));
        assert_eq!(fake.agents().len(), 1);
        // What the close read before the claim landed.
        let mut seen = t.clone();
        seen.state = TaskState::Queued;
        seen.machine = None;
        seen.pane_id = None;
        seen.workspace_id = None;
        let resp = d.close_row(seen, false).await;
        let IpcResponse::Task(closed) = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(closed.state, TaskState::Closed);
        assert!(fake.agents().is_empty(), "the agent went with the task");
    }

    #[tokio::test]
    async fn close_goes_through_the_task_s_machine() {
        let fake = FakeHerdr::new();
        let (d, _tmp) = daemon(&[("a", 2, fake.clone())]).await;
        let IpcResponse::Task(t) = d
            .handle(IpcRequest::Run {
                preempt: false,
                summary: None,
                role: Default::default(),
                description: None,
                prompt: "x".into(),
                spec: spec(),
                flock: None,
                agent: None,
                priority: None,
            })
            .await
        else {
            panic!()
        };
        assert_eq!(
            error_code(
                d.handle(IpcRequest::TaskClose {
                    id: t.id,
                    remove_worktree: true
                })
                .await
            ),
            "no_worktree"
        );
        let resp = d
            .handle(IpcRequest::TaskClose {
                id: t.id,
                remove_worktree: false,
            })
            .await;
        let IpcResponse::Task(closed) = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(closed.state, TaskState::Closed);
        assert!(fake.agents().is_empty());

        // Never dispatched: only the row changes.
        let queued = insert(&d, TaskState::Queued);
        let IpcResponse::Task(c) = d
            .handle(IpcRequest::TaskClose {
                id: queued.id,
                remove_worktree: false,
            })
            .await
        else {
            panic!()
        };
        assert_eq!(c.state, TaskState::Closed);

        assert_eq!(
            error_code(
                d.handle(IpcRequest::TaskClose {
                    id: 99,
                    remove_worktree: false
                })
                .await
            ),
            "task_not_found"
        );
    }

    #[tokio::test]
    async fn closing_a_closed_task_again_needs_no_machine() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let mut t = insert(&d, TaskState::Closed);
        t.machine = Some("zzz".into());
        d.store.update_task(&mut t).unwrap();
        let t = d.store.get_task(t.id).unwrap().unwrap();
        let mut events = d.subscribe();
        let resp = d
            .handle(IpcRequest::TaskClose {
                id: t.id,
                remove_worktree: false,
            })
            .await;
        let IpcResponse::Task(again) = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(again.state, TaskState::Closed);
        assert_eq!(again.updated_at, t.updated_at, "the row is unchanged");
        assert!(events.try_recv().is_err(), "no second task.closed");

        // Its checkout may still be there, so removing it still needs the machine.
        let mut wt = insert(&d, TaskState::Closed);
        wt.spec.worktree = true;
        wt.machine = Some("zzz".into());
        d.store.update_task(&mut wt).unwrap();
        assert_eq!(
            error_code(
                d.handle(IpcRequest::TaskClose {
                    id: wt.id,
                    remove_worktree: true
                })
                .await
            ),
            "unknown_machine"
        );
    }

    #[tokio::test]
    async fn closing_a_task_on_a_removed_machine_closes_the_row_locally() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let mut gone = insert(&d, TaskState::Running);
        gone.machine = Some("zzz".into());
        d.store.update_task(&mut gone).unwrap();
        let finished = d.store.get_task(gone.id).unwrap().unwrap().finished_at;
        let mut events = d.subscribe();
        let resp = d
            .handle(IpcRequest::TaskClose {
                id: gone.id,
                remove_worktree: false,
            })
            .await;
        let IpcResponse::Task(closed) = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(closed.state, TaskState::Closed);
        assert_eq!(closed.finished_at, finished, "finished_at is kept");
        let stored = d.store.get_task(gone.id).unwrap().unwrap();
        assert_eq!(stored.state, TaskState::Closed);
        let ev = events.try_recv().expect("task.closed emitted");
        assert_eq!(ev.kind, "task.closed");
        assert_eq!(ev.task_id, Some(gone.id));
        assert_eq!(ev.job.as_deref(), Some("run"));
        assert!(events.try_recv().is_err(), "one task.closed only");

        // Its checkout is on a machine pastor cannot reach any more.
        let mut wt = insert(&d, TaskState::Running);
        wt.spec.worktree = true;
        wt.machine = Some("zzz".into());
        d.store.update_task(&mut wt).unwrap();
        let resp = d
            .handle(IpcRequest::TaskClose {
                id: wt.id,
                remove_worktree: true,
            })
            .await;
        let IpcResponse::Error { code, message } = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(code, "unknown_machine");
        assert!(
            message.contains("zzz") && message.contains("not in the flock"),
            "{message}"
        );
        assert!(message.contains("worktree"), "{message}");
        let stored = d.store.get_task(wt.id).unwrap().unwrap();
        assert_eq!(stored.state, TaskState::Running, "the row stays open");
    }

    /// Wedges `FakeHerdr::connect` until dropped, even when an assert fails:
    /// a wedged connect blocks a runtime thread, and the runtime would never
    /// shut down.
    struct Unwedge(FakeHerdr);
    impl Drop for Unwedge {
        fn drop(&mut self) {
            self.0.wedge_connects(false);
        }
    }

    /// A daemon running `a`, and `b` whose actor is stuck in connect and was
    /// then taken out of the flock (`keep_b` false) or retargeted (true), so
    /// `b` is shutting down. Needs a multi-thread runtime.
    async fn daemon_with_b_shutting_down(keep_b: bool) -> (Daemon, tempfile::TempDir, Unwedge) {
        let (d, tmp, unwedge) = daemon_with_b_wedged().await;
        let next = if keep_b {
            flock_of(&[("a", 2), ("b", 3)])
        } else {
            flock_of(&[("a", 2)])
        };
        let diff = d
            .fleet()
            .apply_flock(&next, &machine_settings(&test_config()))
            .await;
        assert_eq!(diff.shutting_down, vec!["b".to_string()], "{diff:?}");
        assert!(d.fleet().get("b").is_some(), "still held while it stops");
        (d, tmp, unwedge)
    }

    /// A daemon running `a`, and `b` whose actor is stuck in connect. Needs a
    /// multi-thread runtime.
    async fn daemon_with_b_wedged() -> (Daemon, tempfile::TempDir, Unwedge) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        let flock = flock_of(&[("a", 2)]);
        flock.save(&paths.flock_file()).unwrap();
        std::fs::write(
            paths.config_file(),
            toml::to_string(&test_config()).unwrap(),
        )
        .unwrap();
        let b = FakeHerdr::new();
        b.wedge_connects(true);
        let unwedge = Unwedge(b.clone());
        let on_disk = ConfigFingerprint::sample(&paths);
        let d = Daemon::start(
            paths,
            test_config(),
            flock.clone(),
            on_disk,
            Some(factory(&[("a", FakeHerdr::new()), ("b", b.clone())])),
        )
        .await
        .unwrap();
        let settings = machine_settings(&test_config());
        d.fleet()
            .apply_flock(&flock_of(&[("a", 2), ("b", 2)]), &settings)
            .await;
        let deadline = Instant::now() + Duration::from_secs(5);
        while b.wedged() == 0 {
            assert!(Instant::now() < deadline, "actor never reached connect");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        (d, tmp, unwedge)
    }

    /// While a reload is still waiting for a removed machine's actor to end,
    /// a read of a task on that machine is refused at once: the actor was
    /// aborted and will never answer, so the read must not wait for the IPC
    /// timeout.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reading_a_task_on_a_machine_being_removed_fails_at_once() {
        let (d, _tmp, _unwedge) = daemon_with_b_wedged().await;
        let mut t = insert(&d, TaskState::Running);
        t.machine = Some("b".into());
        d.store.update_task(&mut t).unwrap();
        let fleet = d.fleet();
        let settings = machine_settings(&test_config());
        let reload =
            tokio::spawn(async move { fleet.apply_flock(&flock_of(&[("a", 2)]), &settings).await });
        // `apply_flock` waits up to `SHUTDOWN_WAIT` (2s) for b's actor; read
        // well inside that window.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!reload.is_finished(), "still waiting for b's actor");
        let resp = tokio::time::timeout(
            Duration::from_secs(1),
            d.handle(IpcRequest::TaskRead {
                id: t.id,
                lines: 10,
            }),
        )
        .await
        .expect("the read does not wait on an aborted actor");
        assert_eq!(error_code(resp), "machine_shutting_down");
        let diff = reload.await.unwrap();
        assert_eq!(diff.shutting_down, vec!["b".to_string()], "{diff:?}");
    }

    /// A task on a removed machine held only while its old actor ends is
    /// closed like one on any removed machine: that actor would never answer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closing_a_task_on_a_removed_machine_shutting_down_closes_the_row() {
        let (d, _tmp, _unwedge) = daemon_with_b_shutting_down(false).await;
        let mut t = insert(&d, TaskState::Running);
        t.machine = Some("b".into());
        d.store.update_task(&mut t).unwrap();
        let resp = d
            .handle(IpcRequest::TaskClose {
                id: t.id,
                remove_worktree: false,
            })
            .await;
        let IpcResponse::Task(closed) = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(closed.state, TaskState::Closed);

        let mut wt = insert(&d, TaskState::Running);
        wt.spec.worktree = true;
        wt.machine = Some("b".into());
        d.store.update_task(&mut wt).unwrap();
        assert_eq!(
            error_code(
                d.handle(IpcRequest::TaskClose {
                    id: wt.id,
                    remove_worktree: true
                })
                .await
            ),
            "unknown_machine"
        );
    }

    /// A retargeted machine keeps its tasks for the replacement, so a close
    /// is refused until the old actor has ended, not left to hang on it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn closing_a_task_on_a_machine_being_replaced_waits() {
        let (d, _tmp, _unwedge) = daemon_with_b_shutting_down(true).await;
        let mut t = insert(&d, TaskState::Running);
        t.machine = Some("b".into());
        d.store.update_task(&mut t).unwrap();
        assert_eq!(
            error_code(
                d.handle(IpcRequest::TaskClose {
                    id: t.id,
                    remove_worktree: false
                })
                .await
            ),
            "machine_shutting_down"
        );
        let stored = d.store.get_task(t.id).unwrap().unwrap();
        assert_eq!(stored.state, TaskState::Running, "the row stays open");
    }

    #[tokio::test]
    async fn close_finds_an_orphan_with_no_row() {
        let fake = FakeHerdr::new();
        let ws = fake
            .workspace_create(None, "t-42", &Default::default())
            .await
            .unwrap();
        fake.agent_start("t-42", "claude", &ws.root_pane.pane_id, &[])
            .await
            .unwrap();
        let (d, _tmp) = daemon(&[("a", 2, fake.clone())]).await;
        let fleet = d.fleet();
        wait_until("orphan", || {
            fleet.get("a").unwrap().snapshot().orphans == vec!["t-42".to_string()]
        })
        .await;
        let resp = d
            .handle(IpcRequest::TaskClose {
                id: 42,
                remove_worktree: false,
            })
            .await;
        let IpcResponse::Text(msg) = resp else {
            panic!("{resp:?}")
        };
        assert!(msg.contains("t-42") && msg.contains("orphan"), "{msg}");
        assert!(fake.agents().is_empty());
    }

    #[tokio::test]
    async fn prune_answers_the_count() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let old_done = insert(&d, TaskState::Done);
        let old_failed = insert(&d, TaskState::Failed);
        let resp = d
            .handle(IpcRequest::TaskPrune {
                states: vec![TaskState::Done],
                older_than_secs: 3 * 86400,
            })
            .await;
        let IpcResponse::Pruned(out) = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(out.pruned, 1);
        assert!(out.kept_worktrees.is_empty());
        assert!(d.store.get_task(old_done.id).unwrap().is_none());
        assert!(d.store.get_task(old_failed.id).unwrap().is_some());
        assert_eq!(
            error_code(
                d.handle(IpcRequest::TaskPrune {
                    states: vec![TaskState::Running],
                    older_than_secs: 1
                })
                .await
            ),
            "not_prunable"
        );
    }

    #[tokio::test]
    async fn run_refuses_a_worktree_without_a_repo() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let resp = d
            .handle(IpcRequest::Run {
                preempt: false,
                summary: None,
                role: Default::default(),
                description: None,
                prompt: "x".into(),
                spec: DispatchSpec {
                    worktree: true,
                    ..spec()
                },
                flock: None,
                agent: None,
                priority: None,
            })
            .await;
        assert_eq!(error_code(resp), "worktree_needs_repo");
        assert!(
            d.store
                .list_tasks(&TaskFilter::default())
                .unwrap()
                .is_empty(),
            "no row for a refused run"
        );
    }

    /// Pausing a low task for a critical one (`Task::pause`), against the
    /// fake herdr on a one-slot machine with no burst.
    mod pausing {
        use super::*;
        use crate::herdr::ConnectorExt;
        use serde_json::Value;

        /// A worktree task, so the checkout kept at pause can be followed.
        fn wt() -> DispatchSpec {
            DispatchSpec {
                repo: Some("/srv/app".into()),
                worktree: true,
                ..spec()
            }
        }

        fn run(prompt: &str, spec: DispatchSpec, priority: Priority, preempt: bool) -> IpcRequest {
            IpcRequest::Run {
                prompt: prompt.into(),
                spec,
                flock: None,
                agent: None,
                priority: Some(priority),
                role: Default::default(),
                description: None,
                preempt,
                summary: None,
            }
        }

        async fn start(d: &Daemon, req: IpcRequest) -> Task {
            match d.handle(req).await {
                IpcResponse::Task(t) => t,
                other => panic!("{other:?}"),
            }
        }

        fn get(d: &Daemon, id: i64) -> Task {
            d.store.get_task(id).unwrap().unwrap()
        }

        fn started_args(fake: &FakeHerdr, agent: &str) -> Vec<Value> {
            fake.requests()
                .iter()
                .rev()
                .find(|r| r.method == "agent.start" && r.params["name"] == agent)
                .map(|r| r.params["args"].as_array().cloned().unwrap_or_default())
                .unwrap_or_default()
        }

        /// A critical task with `--preempt` on a full machine pauses the
        /// low task there and starts in the same pass. The paused task's
        /// agent is interrupted and its pane closed; its worktree stays.
        #[tokio::test]
        async fn a_preempting_critical_task_pauses_the_newest_low_task() {
            let fake = FakeHerdr::new();
            let (d, _tmp) = daemon(&[("a", 2, fake.clone())]).await;
            let older = start(&d, run("older", wt(), Priority::Low, false)).await;
            let low = start(&d, run("low", wt(), Priority::Low, false)).await;
            assert_eq!(get(&d, low.id).state, TaskState::Running);
            let checkout = get(&d, low.id).spec.checkout.clone().expect("a checkout");
            let pane = get(&d, low.id).pane_id.clone().unwrap();

            let crit = start(&d, run("fix prod", spec(), Priority::Critical, true)).await;
            assert_eq!(crit.state, TaskState::Running, "started in the same pass");
            assert!(crit.pause.preempt);
            let paused = get(&d, low.id);
            assert_eq!(paused.state, TaskState::Paused, "the newest low task");
            assert_eq!(paused.pause.paused_for, Some(crit.id));
            assert!(paused.pause.paused_at.is_some());
            assert_eq!(
                paused.machine.as_deref(),
                Some("a"),
                "pinned to its machine"
            );
            assert_eq!((paused.pane_id, paused.workspace_id), (None, None));
            assert_eq!(get(&d, older.id).state, TaskState::Running);
            assert!(
                fake.pane_input(&pane)
                    .contains(&crate::herdr::fake::PaneInput::Keys(vec!["esc".into()])),
                "interrupted before its pane closed"
            );
            assert!(
                !fake
                    .agents()
                    .iter()
                    .any(|a| a.name.as_deref() == Some("t-2"))
            );
            let reqs = fake.requests();
            assert!(!reqs.iter().any(|r| r.method == "worktree.remove"));
            let kept = fake.worktree_list("/srv/app").await.unwrap();
            assert!(
                kept.iter().any(|w| w.path == checkout.path),
                "the worktree is kept: {kept:?}"
            );
        }

        /// A critical task whose flock is at its number pauses a task of
        /// its own flock, even when another flock's task there is newer:
        /// pausing that one frees a slot, not a seat in the flock.
        #[tokio::test]
        async fn a_preempting_task_pauses_in_its_own_flock_at_its_number() {
            let fake = FakeHerdr::new();
            let flock: Flock = toml::from_str(
                "[[flock]]\nname = \"home\"\ndefault = true\nmachines = { desk = 3 }\n\n\
                 [[flock]]\nname = \"work\"\nmachines = { desk = 1 }\n\n\
                 [[machine]]\nname = \"desk\"\nlocal = true\nmax_agents = 3\n",
            )
            .unwrap();
            flock.validate().unwrap();
            let (d, _tmp) = daemon_with_flock(flock, &[("desk", 3, fake.clone())]).await;
            let in_flock = |prompt: &str, name: &str, priority: Priority, preempt: bool| {
                let mut req = run(prompt, spec(), priority, preempt);
                if let IpcRequest::Run { flock, .. } = &mut req {
                    *flock = Some(name.into());
                }
                req
            };
            let work = start(&d, in_flock("work", "work", Priority::Low, false)).await;
            let home = start(&d, in_flock("home", "home", Priority::Low, false)).await;
            assert_eq!(get(&d, work.id).state, TaskState::Running);
            assert_eq!(get(&d, home.id).state, TaskState::Running);

            let crit = start(&d, in_flock("fix", "work", Priority::Critical, true)).await;
            assert_eq!(crit.state, TaskState::Running, "started in the same pass");
            assert_eq!(get(&d, work.id).state, TaskState::Paused);
            assert_eq!(
                get(&d, home.id).state,
                TaskState::Running,
                "not the newer one"
            );
        }

        /// Without `--preempt` a critical task still waits on a full
        /// machine (burst 0), and nothing is paused.
        #[tokio::test]
        async fn a_critical_task_without_preempt_waits() {
            let fake = FakeHerdr::new();
            let (d, _tmp) = daemon(&[("a", 1, fake.clone())]).await;
            let low = start(&d, run("low", spec(), Priority::Low, false)).await;
            let crit = start(&d, run("crit", spec(), Priority::Critical, false)).await;
            assert_eq!(crit.state, TaskState::Queued);
            assert_eq!(get(&d, low.id).state, TaskState::Running);
        }

        /// `--preempt` is for critical tasks only, from `task run` and
        /// `task priority` alike, and nothing is queued or changed.
        #[tokio::test]
        async fn preempt_is_refused_below_critical() {
            let (d, _tmp) = daemon(&[("a", 1, FakeHerdr::new())]).await;
            let resp = d.handle(run("x", spec(), Priority::High, true)).await;
            assert_eq!(error_code(resp), crate::task::PREEMPT_NEEDS_CRITICAL);
            assert!(
                d.store
                    .list_tasks(&TaskFilter::default())
                    .unwrap()
                    .is_empty()
            );

            start(&d, run("busy", spec(), Priority::Normal, false)).await;
            let queued = start(&d, run("q", spec(), Priority::Normal, false)).await;
            let resp = d
                .handle(IpcRequest::TaskPriority {
                    id: queued.id,
                    priority: Priority::High,
                    preempt: true,
                })
                .await;
            assert_eq!(error_code(resp), crate::task::PREEMPT_NEEDS_CRITICAL);
            let IpcResponse::Task(t) = d
                .handle(IpcRequest::TaskPriority {
                    id: queued.id,
                    priority: Priority::Critical,
                    preempt: true,
                })
                .await
            else {
                panic!()
            };
            assert!(t.pause.preempt);
            let IpcResponse::Task(t) = d
                .handle(IpcRequest::TaskPriority {
                    id: queued.id,
                    priority: Priority::Critical,
                    preempt: false,
                })
                .await
            else {
                panic!()
            };
            assert!(!t.pause.preempt, "task priority without --preempt drops it");
        }

        /// No task to pause: a normal one, an opencode one, one that
        /// resumed a moment ago, a done one. The critical task waits.
        #[tokio::test]
        async fn only_a_running_low_claude_task_can_be_paused() {
            for case in ["normal", "opencode", "resumed", "done"] {
                let fake = FakeHerdr::new();
                let (d, _tmp) = daemon(&[("a", 1, fake.clone())]).await;
                let (level, agent) = match case {
                    "normal" => (Priority::Normal, "claude"),
                    "opencode" => (Priority::Low, "opencode"),
                    _ => (Priority::Low, "claude"),
                };
                let busy = start(
                    &d,
                    run(
                        "busy",
                        DispatchSpec {
                            agent: agent.into(),
                            ..spec()
                        },
                        level,
                        false,
                    ),
                )
                .await;
                let mut t = get(&d, busy.id);
                match case {
                    "resumed" => t.pause.resumed_at = Some(chrono::Utc::now()),
                    "done" => t.state = TaskState::Done,
                    _ => {}
                }
                d.store.update_task(&mut t).unwrap();
                let crit = start(&d, run("crit", spec(), Priority::Critical, true)).await;
                assert_eq!(crit.state, TaskState::Queued, "{case}");
                assert_ne!(get(&d, busy.id).state, TaskState::Paused, "{case}");
            }
        }

        /// A paused task goes first among the low tasks, and resumes its
        /// own session in its own worktree once a slot frees: `claude
        /// --resume <session>`, told to carry on.
        #[tokio::test]
        async fn a_paused_task_resumes_first_among_low_tasks_when_a_slot_frees() {
            let fake = FakeHerdr::new();
            let (d, _tmp) = daemon(&[("a", 1, fake.clone())]).await;
            let low = start(&d, run("low", wt(), Priority::Low, false)).await;
            let session = get(&d, low.id).spec.session_id.clone().expect("a session");
            let branch = get(&d, low.id).spec.checkout.clone().unwrap().branch;
            let crit = start(&d, run("crit", spec(), Priority::Critical, true)).await;
            assert_eq!(crit.state, TaskState::Running);
            let later = start(&d, run("later low", spec(), Priority::Low, false)).await;
            assert_eq!(later.state, TaskState::Queued);
            let queue: Vec<i64> = d
                .store
                .queued_tasks()
                .unwrap()
                .iter()
                .map(|t| t.id)
                .collect();
            assert_eq!(queue, vec![low.id, later.id], "the paused task goes first");
            let entries = d.fleet().queue(None, None).unwrap();
            assert_eq!(entries[0].task.id, low.id);

            // Still full: it stays paused.
            d.fleet().dispatch_queued().await;
            assert_eq!(get(&d, low.id).state, TaskState::Paused);

            let IpcResponse::Task(_) = d
                .handle(IpcRequest::TaskClose {
                    id: crit.id,
                    remove_worktree: false,
                })
                .await
            else {
                panic!()
            };
            d.fleet().dispatch_queued().await;
            let resumed = get(&d, low.id);
            assert_eq!(resumed.state, TaskState::Running);
            assert!(resumed.pause.resumed_at.is_some());
            assert_eq!(resumed.spec.session_id.as_deref(), Some(session.as_str()));
            assert_eq!(get(&d, later.id).state, TaskState::Queued);
            let args = started_args(&fake, "t-1");
            assert_eq!(
                args[args.len() - 2..],
                [Value::from("--resume"), Value::from(session.as_str())]
            );
            let reqs = fake.requests();
            let open = reqs
                .iter()
                .rev()
                .find(|r| r.method == "worktree.open")
                .expect("its own checkout opened again");
            assert_eq!(open.params["branch"], branch.as_str());
            let prompt = reqs
                .iter()
                .rev()
                .find(|r| r.method == "agent.prompt" && r.params["target"] == "t-1")
                .unwrap();
            assert_eq!(prompt.params["text"], crate::task::RESUME_PROMPT);
            assert_eq!(get(&d, low.id).prompt, "low", "its own prompt is kept");

            // Resumed a moment ago: another critical task does not pause it.
            let again = start(&d, run("crit 2", spec(), Priority::Critical, true)).await;
            assert_eq!(again.state, TaskState::Queued);
            assert_eq!(get(&d, low.id).state, TaskState::Running);
        }

        /// Two low tasks paused for two critical ones, both pinned to the
        /// same machine: closing only one critical frees a single slot, and
        /// `dispatch_queued` resumes exactly one paused task, not both. Each
        /// `resume_paused` call reads a fresh `views()` after the previous
        /// one's actor has replied (`refresh_live` runs before the reply),
        /// so the second sees the slot the first just took.
        #[tokio::test]
        async fn only_one_of_two_paused_tasks_resumes_into_one_freed_slot() {
            let fake = FakeHerdr::new();
            let (d, _tmp) = daemon(&[("a", 2, fake.clone())]).await;
            let low1 = start(&d, run("low1", spec(), Priority::Low, false)).await;
            let low2 = start(&d, run("low2", spec(), Priority::Low, false)).await;
            assert_eq!(low1.state, TaskState::Running);
            assert_eq!(low2.state, TaskState::Running);
            let crit1 = start(&d, run("crit1", spec(), Priority::Critical, true)).await;
            let crit2 = start(&d, run("crit2", spec(), Priority::Critical, true)).await;
            assert_eq!(crit1.state, TaskState::Running);
            assert_eq!(crit2.state, TaskState::Running);
            assert_eq!(get(&d, low1.id).state, TaskState::Paused);
            assert_eq!(get(&d, low2.id).state, TaskState::Paused);

            // Free exactly one slot.
            d.handle(IpcRequest::TaskClose {
                id: crit1.id,
                remove_worktree: false,
            })
            .await;
            d.fleet().dispatch_queued().await;

            let states = [get(&d, low1.id).state, get(&d, low2.id).state];
            let running = states.iter().filter(|s| **s == TaskState::Running).count();
            let paused = states.iter().filter(|s| **s == TaskState::Paused).count();
            assert_eq!(running, 1, "only one freed slot, only one may resume");
            assert_eq!(paused, 1, "the other stays paused");
        }

        /// A resume whose agent does not come up fails the task, as any
        /// dispatch that fails does.
        #[tokio::test]
        async fn a_failed_resume_fails_the_task() {
            let fake = FakeHerdr::new();
            let (d, _tmp) = daemon(&[("a", 1, fake.clone())]).await;
            let low = start(&d, run("low", spec(), Priority::Low, false)).await;
            let crit = start(&d, run("crit", spec(), Priority::Critical, true)).await;
            assert_eq!(get(&d, low.id).state, TaskState::Paused);
            d.handle(IpcRequest::TaskClose {
                id: crit.id,
                remove_worktree: false,
            })
            .await;
            fake.exit_agents_on_start(true);
            d.fleet().dispatch_queued().await;
            let failed = get(&d, low.id);
            assert_eq!(failed.state, TaskState::Failed);
            assert!(failed.error.is_some());
        }

        /// A paused task has no agent: `send` has nothing to type into,
        /// and `close` closes its row, with nothing asked of the machine.
        #[tokio::test]
        async fn send_and_close_on_a_paused_task() {
            let fake = FakeHerdr::new();
            let (d, _tmp) = daemon(&[("a", 1, fake.clone())]).await;
            let low = start(&d, run("low", spec(), Priority::Low, false)).await;
            start(&d, run("crit", spec(), Priority::Critical, true)).await;
            assert_eq!(get(&d, low.id).state, TaskState::Paused);
            let resp = d
                .handle(IpcRequest::TaskSend {
                    id: low.id,
                    input: SendInput {
                        text: Some("hi".into()),
                        enter: true,
                        ..Default::default()
                    },
                })
                .await;
            assert_eq!(error_code(resp), "task_not_live");
            let before = fake.requests().len();
            let IpcResponse::Task(t) = d
                .handle(IpcRequest::TaskClose {
                    id: low.id,
                    remove_worktree: false,
                })
                .await
            else {
                panic!()
            };
            assert_eq!(t.state, TaskState::Closed);
            assert_eq!(fake.requests().len(), before, "the row alone");
            assert!(d.store.queued_tasks().unwrap().is_empty());
        }

        /// `close --remove-worktree` on a paused task opens its kept
        /// checkout and removes it through that workspace.
        #[tokio::test]
        async fn close_remove_worktree_on_a_paused_task_removes_its_checkout() {
            let fake = FakeHerdr::new();
            let (d, _tmp) = daemon(&[("a", 1, fake.clone())]).await;
            let low = start(&d, run("low", wt(), Priority::Low, false)).await;
            let checkout = get(&d, low.id).spec.checkout.clone().unwrap();
            start(&d, run("crit", spec(), Priority::Critical, true)).await;
            assert_eq!(get(&d, low.id).state, TaskState::Paused);
            let IpcResponse::Task(t) = d
                .handle(IpcRequest::TaskClose {
                    id: low.id,
                    remove_worktree: true,
                })
                .await
            else {
                panic!()
            };
            assert_eq!(t.state, TaskState::Closed);
            let left = fake.worktree_list("/srv/app").await.unwrap();
            assert!(!left.iter().any(|w| w.path == checkout.path), "{left:?}");
        }
    }
}
