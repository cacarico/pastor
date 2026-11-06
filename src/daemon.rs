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
    AGENT_KIND_MISSING, AgentChoice, AgentPick, AgentRefusal, Agents, Defaults, Layer,
    LimitsConfig, MODEL_KIND_MISMATCH, Models, PastorConfig, Paths, profile::PROFILE_NOT_ALLOWED,
};
use crate::dispatch::{
    Claim, FlockSeat, FloorView, MachineView, flock_held, mark_waiting_under_share, now_machine,
    pick_machine, pick_machine_where,
};
use crate::herdr::{Connector, Endpoint};
use crate::ipc::{HeadPing, IpcRequest, IpcResponse};
use crate::limit::AccountLimit;
use crate::machine::{
    ActorStopped, MachineHandle, MachineSettings, OrphanClosed, PastorEvent, SendInput,
    SendRefused, ShutdownOutcome, spawn_machine,
};
use crate::queue::{QueueEntry, QueueSpot};
use crate::scheduler::{ConfigFingerprint, Scheduler, SchedulerHandle};
use crate::store::{MoveError, Moved, NewTask, PriorityError, RetryError, Store, TaskFilter};
use crate::sync::Recover;
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
        version_every: config.version_every_duration(),
        request_timeout: config.request_timeout_duration(),
        agent_ready_timeout: config.agent_ready_timeout_duration(),
        poll_every: config.tick_duration(),
        close_done_after: config.close_done_after_duration(),
        close_failed_after: config.close_failed_after_duration(),
        agents: config.agents.clone(),
        head_address: config.head_address.clone(),
        limits: Some(config.limits.clone()),
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
        .map(|(name, number)| {
            let live = status
                .live_by_flock
                .iter()
                .filter(|(f, _)| f.as_deref().unwrap_or(default) == name)
                .map(|(_, n)| n)
                .sum();
            FlockSeat::new(name, number, live)
        })
        .collect()
}

/// The part of a machine's entry its actor is built from. The flock, the
/// agent, its per-kind agents, its loads, its room floor and its provisioner
/// are not: they only decide which tasks the machine is offered and what
/// they run, or have no runtime behavior yet, which dispatch reads from the
/// flock last applied, so moving a machine, changing its agent, its loads, its
/// floor or its provisioner keeps its connection and the tasks already on it.
fn actor_config(m: &MachineConfig) -> MachineConfig {
    MachineConfig {
        flock: None,
        agent: None,
        agent_args: None,
        agents: Default::default(),
        loads: Vec::new(),
        min_disk: None,
        min_memory: None,
        unknown_room: None,
        reserved: 0,
        description: None,
        provisioner: None,
        bundles: Vec::new(),
        bundle_apply: None,
        ..m.clone()
    }
}

/// `settings` as the actor for `m` runs them: an agent on the head's own
/// machine (`local`) reaches the head through its socket, so it is not told
/// the head's address, and an `ssh` machine with no address to tell is cut
/// off (`is_cut_off`).
pub fn actor_settings(m: &MachineConfig, settings: &MachineSettings) -> MachineSettings {
    MachineSettings {
        head_address: settings.head_address.clone().filter(|_| !m.local),
        cut_off_from_head: is_cut_off(settings.head_address.as_deref(), m),
        ..settings.clone()
    }
}

/// The `ssh` machines whose agents cannot reach the head: with no
/// `head_address` their panes get no `PASTOR_HEAD`, so `pastor task done`
/// there finds no head and every `summary = "require"` task fails. A local
/// machine uses the socket and a pull machine's own serve carries requests.
pub fn cut_off_from_head(config: &PastorConfig, machines: &[MachineConfig]) -> Vec<String> {
    machines
        .iter()
        .filter(|m| is_cut_off(config.head_address.as_deref(), m))
        .map(|m| m.name.clone())
        .collect()
}

/// Whether `m`'s agents cannot reach a head whose `head_address` is
/// `head_address` (`cut_off_from_head`).
pub fn is_cut_off(head_address: Option<&str>, m: &MachineConfig) -> bool {
    head_address.is_none() && m.ssh.is_some()
}

/// Logs `cut_off_from_head` as a warning, at start and after a reload.
pub fn warn_cut_off_from_head(config: &PastorConfig, machines: &[MachineConfig]) {
    let names = cut_off_from_head(config, machines);
    if !names.is_empty() {
        tracing::warn!(
            machines = ?names,
            "agents on these ssh machines cannot reach the head, so `pastor task done` fails there; set head_address in pastor.toml"
        );
    }
}

/// How many `request_timeout`s the head waits for a machine's actor to
/// answer one request. A dispatch is a claim, `agent.start`, the readiness
/// wait and the prompt under one `request_timeout`, then maybe the trust
/// keys under another, and the actor may be inside a reconcile (a third)
/// when the request arrives.
const REPLY_WAIT_FACTOR: u32 = 3;

fn reply_wait_ms(config: &PastorConfig) -> std::sync::atomic::AtomicU64 {
    let wait = config.request_timeout_duration() * REPLY_WAIT_FACTOR;
    std::sync::atomic::AtomicU64::new(u64::try_from(wait.as_millis()).unwrap_or(u64::MAX))
}

/// The most a `task run --now` waits for its pull machine's claim: half the
/// client's bound on a request (`ipc::DEFAULT_REQUEST_TIMEOUT`), so the
/// run's own answer, `now_not_started` included, reaches the client.
pub const NOW_CLAIM_WAIT: Duration = Duration::from_secs(60);

/// How often a run waiting for a claim reads its row again, for a change no
/// claim makes (a `task close` meanwhile).
const NOW_CLAIM_RECHECK: Duration = Duration::from_secs(1);

/// A machine's actor did not answer within the head's bound
/// (`Fleet::bounded`). The request may still be carried out: it stays in the
/// actor's queue, and `Store::claim_task` keeps a dispatch from happening
/// twice.
#[derive(Debug, thiserror::Error)]
#[error("machine {machine} did not answer within {after:?}")]
pub struct NoReply {
    pub machine: String,
    pub after: Duration,
}

/// How long the head waits for one machine's sweep: it asks the machine
/// several things for each worktree it finds.
const SWEEP_WAIT: Duration = Duration::from_secs(600);

/// How long the head waits for a push bootstrap.
const PUSH_WAIT: Duration = Duration::from_secs(30 * 60);

async fn within<T>(
    machine: &str,
    after: Duration,
    request: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    match tokio::time::timeout(after, request).await {
        Ok(res) => res,
        Err(_) => Err(NoReply {
            machine: machine.to_string(),
            after,
        }
        .into()),
    }
}

/// A task a dispatch pass placed on a machine under the dispatch lock and
/// is sending outside it. Until the send ends it counts on that machine in
/// `Fleet::views`, as the actor's own count will once it has claimed the
/// task, and later passes leave the task alone.
#[derive(Debug, Clone)]
struct Placed {
    task_id: i64,
    machine: String,
    flock: Option<String>,
    from_job: bool,
}

/// The `Placed` tasks of every pass, and a signal each time one's send ends.
#[derive(Default)]
struct InFlight {
    placed: std::sync::Mutex<Vec<Placed>>,
    sent: tokio::sync::Notify,
}

impl InFlight {
    fn placed(&self) -> Vec<Placed> {
        self.placed.lock().unwrap().clone()
    }

    fn holds(&self, task_id: i64) -> bool {
        self.placed
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.task_id == task_id)
    }
}

/// Holds a `Placed` in `Fleet::in_flight` until dropped: when the send
/// ends, fails, times out or is abandoned with its pass.
struct Reservation {
    in_flight: Arc<InFlight>,
    task_id: i64,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.in_flight
            .placed
            .lock()
            .unwrap()
            .retain(|p| p.task_id != self.task_id);
        self.in_flight.sent.notify_waiters();
    }
}

/// What a dispatch pass sends for one placed task, once the lock is let go.
enum Outbound {
    Dispatch,
    /// A `--now` task: dispatch it on the actor's urgent channel
    /// (`MachineHandle::dispatch_now`), so it starts even while the actor
    /// is still starting an earlier task.
    Now,
    /// Pause this task on the machine first (`preempt`), then dispatch.
    PauseFor(i64),
    Resume,
}

/// One send a pass decided under the lock.
struct Placement {
    task: Task,
    handle: MachineHandle,
    send: Outbound,
    _held: Reservation,
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
    /// Its last claim said its serve places workdir sets itself
    /// (`TaskClaim::workdir_sets`).
    places_sets: bool,
}

struct Spawner {
    connect: ConnectorFactory,
    events: broadcast::Sender<PastorEvent>,
}

/// The start of the error `check_pull_lost` puts on the tasks it marks stale.
fn pull_lost_note(machine: &str) -> String {
    format!("pull machine {machine} has not claimed or reported")
}

/// Whether `task` is stale because `check_pull_lost` counted `machine` lost,
/// rather than because the machine reported it so.
fn stale_from_loss(task: &Task, machine: &str) -> bool {
    task.state == TaskState::Stale
        && task
            .error
            .as_deref()
            .is_some_and(|e| e.starts_with(&pull_lost_note(machine)))
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
    /// `[repos]` as last applied: a queued task's load when its run and
    /// agent name none (`config::resolve_load`).
    repos: RwLock<crate::config::Repos>,
    /// The harness manifests as last loaded (`set_harnesses`): how each
    /// machine's actor asks for a pinned harness's version.
    harnesses: RwLock<Arc<crate::harness_manifest::Harnesses>>,
    /// The keeper of the machines' bundles, once the daemon has made one:
    /// the gate each actor asks before it starts a task (`set_keeper`).
    keeper: RwLock<Option<Arc<crate::bundle::keep::Keeper>>>,
    /// What places a task's workdir set before its agent starts, once the
    /// daemon has made one (`set_workdirs`).
    workdirs: RwLock<Option<Arc<crate::bundle::workdir_task::HeadGate>>>,
    /// `[profiles]` as last applied: what a task's `profile` names.
    profiles: RwLock<crate::config::profile::Profiles>,
    /// `head_address` in pastor.toml as last applied, for `statuses` to
    /// mark the machines cut off from the head.
    head_address: RwLock<Option<String>>,
    /// `max_orchestrators` as last applied.
    max_orchestrators: std::sync::atomic::AtomicU32,
    /// `[limits]` as last applied: how long a limit holds.
    limits: RwLock<LimitsConfig>,
    /// `[artifacts] herdr` as last applied: the herdr version `machine
    /// check` holds each machine to; `None` when none is pinned.
    herdr_pin: RwLock<Option<String>>,
    /// The limits table as last read (`live_limits`): a pass settles each
    /// queued task against each machine, and the later tasks again for
    /// each one, so reading the store every time adds up. Dropped whenever
    /// this fleet writes the table; a row another process writes (the CLI
    /// with no head) shows within `LIMITS_FRESH`.
    limits_seen: std::sync::Mutex<Option<(std::time::Instant, Vec<AccountLimit>)>>,
    store: Arc<Store>,
    /// `None` for a fixed fleet (`Fleet::new`): tests and the daemon-less CLI.
    spawner: Option<Spawner>,
    /// Set for a headless serve (`Fleet::headless`): job tasks go to the
    /// head instead of this store, and the head checks their flock.
    forward: Option<HeadForward>,
    /// Pull machines by name, since each was added or last heard from.
    pull_seen: std::sync::Mutex<HashMap<String, PullSeen>>,
    /// Woken by each claim that took a task, for a `--now` run waiting for
    /// its pull machine to take it (`wait_claimed`).
    claimed: tokio::sync::Notify,
    dispatch_lock: tokio::sync::Mutex<()>,
    /// What dispatch passes placed under the lock and are sending outside
    /// it (`Fleet::dispatch_queued`).
    in_flight: Arc<InFlight>,
    /// How long the head waits for a machine's actor to answer a request
    /// (`Fleet::bounded`), in milliseconds: `REPLY_WAIT_FACTOR` times
    /// `request_timeout` as last applied.
    reply_wait_ms: std::sync::atomic::AtomicU64,
}

/// `Fleet::queue_run_as`'s arguments, grouped: the `IpcRequest::Run` fields
/// that reach it (`now` is settled into `spec` before this, so it carries
/// none).
pub struct RunAsk<'a> {
    pub prompt: String,
    pub spec: crate::task::DispatchSpec,
    /// `None`: the flock of the pinned machine, or the default flock.
    pub flock: Option<&'a str>,
    /// What the run's flags said about the agent; `None` keeps the agent
    /// `spec` already carries.
    pub ask: Option<&'a AgentChoice>,
    /// `--priority`; `None` lets the flock, the pinned machine or
    /// `[defaults]` set the level.
    pub priority: Option<Priority>,
    pub role: TaskRole,
    pub description: Option<String>,
    pub preempt: bool,
    pub summary: Option<crate::task::SummaryMode>,
}

/// `Fleet::report`'s arguments, grouped: what a pull machine's
/// `IpcRequest::TaskReport` says of one task.
pub struct PullReport {
    pub state: TaskState,
    pub pane: Option<String>,
    pub detail: Option<String>,
    pub limit: Option<crate::limit::Limit>,
    pub trusted: Option<Vec<String>>,
    pub checkout: Option<crate::task::Checkout>,
    pub workdir_skipped: Vec<crate::task::WorkdirSkip>,
}

/// `Fleet::pausable_for`'s arguments, grouped.
struct PausableFor<'a> {
    views: &'a [MachineView],
    flock: &'a str,
    task: &'a Task,
    claim: Claim,
    accepts: &'a dyn Fn(&str) -> bool,
    later: &'a [Task],
    takes: &'a dyn Fn(&Task, &str) -> bool,
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
            repos: RwLock::default(),
            harnesses: RwLock::default(),
            keeper: RwLock::default(),
            workdirs: RwLock::default(),
            profiles: RwLock::default(),
            head_address: RwLock::default(),
            max_orchestrators: std::sync::atomic::AtomicU32::new(1),
            limits: RwLock::default(),
            herdr_pin: RwLock::default(),
            limits_seen: Default::default(),
            store,
            spawner: None,
            forward: None,
            pull_seen: Default::default(),
            claimed: tokio::sync::Notify::new(),
            dispatch_lock: tokio::sync::Mutex::new(()),
            in_flight: Default::default(),
            reply_wait_ms: reply_wait_ms(&PastorConfig::default()),
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
        *self.wanted.write().recover() = flock;
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
            repos: RwLock::default(),
            harnesses: RwLock::default(),
            keeper: RwLock::default(),
            workdirs: RwLock::default(),
            profiles: RwLock::default(),
            head_address: RwLock::default(),
            max_orchestrators: std::sync::atomic::AtomicU32::new(1),
            limits: RwLock::default(),
            herdr_pin: RwLock::default(),
            limits_seen: Default::default(),
            store,
            spawner: Some(Spawner { connect, events }),
            forward: None,
            pull_seen: Default::default(),
            claimed: tokio::sync::Notify::new(),
            dispatch_lock: tokio::sync::Mutex::new(()),
            in_flight: Default::default(),
            reply_wait_ms: reply_wait_ms(&PastorConfig::default()),
        }
    }

    /// Every machine in flock order, as of now.
    pub fn machines(&self) -> Vec<MachineHandle> {
        self.members
            .read()
            .recover()
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
        let head_address = self.head_address.read().recover().clone();
        self.members
            .read()
            .recover()
            .iter()
            .map(|m| {
                let flock = flock_of(&wanted, &m.handle.name);
                if m.pull {
                    self.count_pull(&m.handle);
                }
                let s = m.handle.snapshot();
                // On a store error none are named; the count stands.
                let now = self
                    .store
                    .tasks_on_machine(&m.handle.name)
                    .map(|ts| {
                        ts.iter()
                            .filter(|t| t.spec.now)
                            .map(Task::display_id)
                            .collect()
                    })
                    .unwrap_or_default();
                crate::machine::MachineStatus {
                    now,
                    profile: self.own_profile(&flock, Some(&m.handle.name)),
                    flock: Some(flock),
                    flocks: seats(&wanted, &s),
                    shutting_down: m.shutting_down,
                    description: wanted
                        .get(&m.handle.name)
                        .and_then(|c| crate::config::clean_description(c.description.as_deref())),
                    no_head_address: wanted
                        .get(&m.handle.name)
                        .is_some_and(|c| is_cut_off(head_address.as_deref(), c)),
                    reserved: wanted.get(&m.handle.name).map_or(0, |c| c.reserved),
                    cordon: self.store.cordon(&m.handle.name).ok().flatten(),
                    ..s
                }
            })
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<MachineHandle> {
        self.members
            .read()
            .recover()
            .iter()
            .find(|m| m.handle.name == name)
            .map(|m| m.handle.clone())
    }

    /// The flock as last applied: what a reload falls back to when
    /// `flock.toml` does not load, and what it applies again to finish a
    /// swap a machine that was shutting down held up. Empty for a fixed
    /// fleet.
    pub fn flock(&self) -> Flock {
        self.wanted.read().recover().clone()
    }

    /// The harness manifests the actors read, replaced at start and by a
    /// reload that loads them (the next `apply_config` replaces the actors
    /// whose settings changed with them).
    pub fn set_harnesses(&self, harnesses: Arc<crate::harness_manifest::Harnesses>) {
        *self.harnesses.write().recover() = harnesses;
    }

    pub fn harnesses(&self) -> Arc<crate::harness_manifest::Harnesses> {
        self.harnesses.read().recover().clone()
    }

    /// The keeper whose gate the actors ask before they start a task. Set
    /// before the first `apply_flock`, so every actor has it.
    pub fn set_keeper(&self, keeper: Arc<crate::bundle::keep::Keeper>) {
        *self.keeper.write().recover() = Some(keeper);
    }

    pub fn keeper(&self) -> Option<Arc<crate::bundle::keep::Keeper>> {
        self.keeper.read().recover().clone()
    }

    /// The gate that places tasks' workdir sets. Set before the first
    /// `apply_flock`, like the keeper.
    pub fn set_workdirs(&self, gate: Arc<crate::bundle::workdir_task::HeadGate>) {
        *self.workdirs.write().recover() = Some(gate);
    }

    /// A `bundle.*` event about `machine`.
    pub fn emit_machine(&self, kind: &str, machine: &str, detail: Option<serde_json::Value>) {
        self.emit(kind, machine, None, detail);
    }

    /// The herdr version `[artifacts] herdr` pins, as last applied.
    pub fn herdr_pin(&self) -> Option<String> {
        self.herdr_pin.read().recover().clone()
    }

    /// `head_address` of `pastor.toml` as last applied.
    pub fn head_address(&self) -> Option<String> {
        self.head_address.read().recover().clone()
    }

    /// Take `[defaults]` and `[agents]` from `pastor.toml` as now loaded;
    /// the scheduler calls it at start, and a reload through `apply_config`.
    pub fn set_config(&self, config: &PastorConfig) {
        *self.defaults.write().recover() = config.defaults.clone();
        *self.head_address.write().recover() = config.head_address.clone();
        *self.agents.write().recover() = config.agents.clone();
        *self.models.write().recover() = config.models.clone();
        *self.repos.write().recover() = config.repos.clone();
        *self.profiles.write().recover() = config.profiles.clone();
        *self.limits.write().recover() = config.limits.clone();
        *self.herdr_pin.write().recover() = config
            .artifacts
            .herdr
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string);
        self.max_orchestrators.store(
            config.max_orchestrators,
            std::sync::atomic::Ordering::Relaxed,
        );
        self.reply_wait_ms.store(
            reply_wait_ms(config).into_inner(),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// How long a request to a machine's actor may take, as last applied.
    pub fn reply_wait(&self) -> Duration {
        Duration::from_millis(
            self.reply_wait_ms
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// Set `reply_wait` below any `request_timeout`, for a test.
    #[cfg(all(test, feature = "fake-herdr"))]
    fn set_reply_wait(&self, wait: Duration) {
        self.reply_wait_ms.store(
            u64::try_from(wait.as_millis()).unwrap(),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    /// `request` to `machine`'s actor, failing with `NoReply` once it has
    /// taken `reply_wait`. The actor bounds each herdr call by
    /// `request_timeout`; this bounds the wait for an actor stuck elsewhere,
    /// so a caller never waits on it forever.
    pub async fn bounded<T>(
        &self,
        machine: &str,
        request: impl std::future::Future<Output = anyhow::Result<T>>,
    ) -> anyhow::Result<T> {
        within(machine, self.reply_wait(), request).await
    }

    /// `max_orchestrators` in `pastor.toml` as last applied.
    pub fn max_orchestrators(&self) -> u32 {
        self.max_orchestrators
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// `[defaults]` as last applied.
    pub fn defaults(&self) -> Defaults {
        self.defaults.read().recover().clone()
    }

    /// `[profiles]` as last applied.
    pub fn profiles(&self) -> crate::config::profile::Profiles {
        self.profiles.read().recover().clone()
    }

    /// `[agents]` as last applied.
    pub fn agents(&self) -> Agents {
        self.agents.read().recover().clone()
    }

    /// `[models]` as last applied.
    pub fn models(&self) -> Models {
        self.models.read().recover().clone()
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
        let wanted = self.wanted.read().recover();
        self.defaults.read().recover().resolve_agent_on(
            ask,
            machine.and_then(|m| wanted.get(m)),
            wanted.entry(flock),
        )
    }

    /// The own profile of `machine` for a task in `flock`, which decides
    /// whether it may ask for `unrestricted` there: the machine's, else the
    /// flock's, else `[defaults]` (`Defaults::own_profile`).
    pub fn own_profile(&self, flock: &str, machine: Option<&str>) -> Option<String> {
        let wanted = self.wanted.read().recover();
        self.defaults
            .read()
            .recover()
            .own_profile(machine.and_then(|m| wanted.get(m)), wanted.entry(flock))
    }

    /// `resolve_agent` written into `spec`, with the ask and where the agent
    /// and its args came from (`DispatchSpec::agent_source`), its model's
    /// args put in front (`Models::apply`), its fallback list
    /// (`Models::fallback`) and its profile's lists added
    /// (`Profiles::apply`). `asked_by` names the ask: `task run` or `job
    /// <name>`. Refused when the model or a fallback is not in `[models]`,
    /// the model is not of the agent's kind, or the profile is unknown or
    /// `unrestricted` where the machine's own is not; `spec` is then only
    /// partly settled.
    fn settle(
        &self,
        spec: &mut crate::task::DispatchSpec,
        ask: &AgentChoice,
        flock: &str,
        machine: Option<&str>,
        asked_by: &str,
    ) -> Result<(), AgentRefusal> {
        let models = self.models.read().recover();
        let agents = self.agents.read().recover();
        let pick = {
            let wanted = self.wanted.read().recover();
            self.defaults.read().recover().resolve_agent_for(
                ask,
                machine.and_then(|m| wanted.get(m)),
                wanted.entry(flock),
                &models,
                &agents,
            )
        };
        if let Some(kind) = &pick.missing_kind {
            return Err(AgentRefusal {
                code: AGENT_KIND_MISSING,
                message: format!(
                    "flock {flock} runs {kind} agents, and machine {} has none: its agent {} is {}, and it has no agents.{kind}",
                    machine.unwrap_or("-"),
                    pick.agent,
                    agents.kind(&pick.agent)
                ),
            });
        }
        pick.apply_to(spec);
        let label = |layer| layer_label(layer, asked_by, flock, machine);
        // Settled once, when the task is queued (`settle_run`); kept here.
        let (timeout_from, place_from) = spec
            .agent_source
            .as_ref()
            .map(|s| (s.timeout_from.clone(), s.place_from.clone()))
            .unwrap_or_default();
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
            fallback: models.fallback(&pick)?,
            fallback_from: pick.fallback.as_ref().map(|&(_, layer)| label(layer)),
            fallback_use: None,
            profile: pick.profile.as_ref().map(|(name, _)| name.clone()),
            profile_from: pick.profile.as_ref().map(|&(_, layer)| label(layer)),
            timeout_from,
            place_from,
            lists_unapplied: None,
        }));
        // What the machine's owner lets run there (the unrestricted rule),
        // and for agy, whose rules are per machine, what it denies.
        let own = self.own_profile(flock, machine);
        pick.settle(
            &agents,
            &models,
            &self.profiles.read().recover(),
            own.as_deref(),
            machine,
            spec,
        )?;
        let unapplied = agents.lists_unapplied(spec);
        if let Some(source) = spec.agent_source.as_mut() {
            source.lists_unapplied = unapplied;
        }
        Ok(())
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
        let wanted = self.wanted.read().recover();
        let (priority, layer) = self.defaults.read().recover().resolve_priority(
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
        let wanted = self.wanted.read().recover();
        let picked = self
            .defaults
            .read()
            .recover()
            .resolve_label(spec.label.template.as_deref(), wanted.entry(flock));
        spec.label = crate::task::WorkspaceLabel {
            from: picked
                .as_ref()
                .map(|&(_, layer)| layer_label(layer, asked_by, flock, None)),
            template: picked.map(|(template, _)| template),
            ..Default::default()
        };
    }

    /// A task's `keep_pane` as it is queued in `flock`: what its run or job
    /// asked for (the spec's own), else the flock's, else `[defaults]`
    /// (`Defaults::resolve_keep_pane`), with where it came from; and its
    /// `remove_worktree` the same way (`Defaults::resolve_remove_worktree`).
    fn settle_keep_pane(&self, spec: &mut crate::task::DispatchSpec, flock: &str, asked_by: &str) {
        let wanted = self.wanted.read().recover();
        let defaults = self.defaults.read().recover();
        let picked = defaults.resolve_keep_pane(spec.keep_pane, wanted.entry(flock));
        spec.keep_pane = picked.map(|(keep, _)| keep);
        spec.keep_pane_from = picked.map(|(_, layer)| layer_label(layer, asked_by, flock, None));
        let picked = defaults.resolve_remove_worktree(spec.remove_worktree, wanted.entry(flock));
        spec.remove_worktree = picked.map(|(remove, _)| remove);
        spec.remove_worktree_from =
            picked.map(|(_, layer)| layer_label(layer, asked_by, flock, None));
    }

    /// A task's load as it is queued in `flock`: what its run asked for,
    /// else its agent's, else its repo's (`config::resolve_load`), with
    /// where it came from. `settle_agent` names the agent first. A load no
    /// machine of the flock takes, or that a pinned machine does not take,
    /// is refused here rather than left to wait for a machine the flock
    /// file does not have.
    fn settle_load(
        &self,
        spec: &mut crate::task::DispatchSpec,
        flock: &str,
        asked_by: &str,
    ) -> Result<(), AgentRefusal> {
        let refuse = |code, message| Err(AgentRefusal { code, message });
        if let Some(asked) = &spec.load
            && let Err(e) = crate::task::check_load(asked)
        {
            return refuse(crate::task::INVALID_LOAD, e);
        }
        let picked = crate::config::resolve_load(
            spec.load.as_deref(),
            &spec.agent,
            &self.agents.read().recover(),
            spec.repo.as_deref(),
            &self.repos.read().recover(),
            asked_by,
        );
        spec.load = picked.as_ref().map(|(load, _)| load.clone());
        spec.load_from = picked.as_ref().map(|(_, from)| from.clone());
        let Some((load, from)) = picked else {
            return Ok(());
        };
        let wanted = self.wanted.read().recover();
        let takers = wanted.takers(flock, &load);
        if let Some(m) = &spec.machine
            && !takers.contains(&m.as_str())
        {
            return refuse(
                crate::task::LOAD_NOT_TAKEN,
                format!(
                    "machine {m} does not take load {load} (from {from}): flock.toml lists the loads a machine or its flocks take"
                ),
            );
        }
        if takers.is_empty() {
            return refuse(
                crate::task::LOAD_NOT_TAKEN,
                format!(
                    "no machine of flock {flock} takes load {load} (from {from}): flock.toml lists the loads a machine or its flocks take"
                ),
            );
        }
        Ok(())
    }

    /// A task's stored workdir bundles as it is queued in `flock`: the
    /// flock's `workdir_bundles`, then those its job named, each once, with
    /// where each came from. A bundle the job names must be a workdir
    /// bundle whose vars the flock gives (flock.toml's own check covers the
    /// flock's), and the task's own set may not take a listed bundle's id.
    /// Only the ids are settled: each start places the newest version kept.
    fn settle_workdir_bundles(
        &self,
        spec: &mut crate::task::DispatchSpec,
        flock: &str,
        asked_by: &str,
    ) -> Result<(), AgentRefusal> {
        use crate::task::WorkdirBundle;
        let refuse = |code: &'static str, message: String| Err(AgentRefusal { code, message });
        let asked = std::mem::take(&mut spec.workdir_bundles);
        let wanted = self.wanted.read().recover();
        let entry = wanted.entry(flock);
        let mut list: Vec<WorkdirBundle> = entry
            .map(|f| f.workdir_bundles.as_slice())
            .unwrap_or_default()
            .iter()
            .map(|id| WorkdirBundle {
                from: Some(format!("flock {flock}")),
                ..WorkdirBundle::named(id)
            })
            .collect();
        let mut from_job = Vec::new();
        for b in asked {
            if list.iter().any(|l| l.id == b.id) {
                continue;
            }
            from_job.push(b.id.clone());
            list.push(WorkdirBundle {
                from: Some(b.from.unwrap_or_else(|| asked_by.to_string())),
                ..WorkdirBundle::named(&b.id)
            });
        }
        let gate = self.workdirs.read().recover().clone();
        if let (false, Some(gate)) = (from_job.is_empty(), gate) {
            let catalog = crate::bundle::Catalog::load(gate.paths()).map_err(|e| AgentRefusal {
                code: crate::bundle::BUNDLE_INVALID,
                message: format!("{e:#}"),
            })?;
            for id in &from_job {
                let manifest = match catalog.check(id) {
                    Ok(b) => &b.manifest,
                    Err(why) => return refuse(crate::bundle::UNKNOWN_BUNDLE, why),
                };
                if manifest.target() != crate::bundle::manifest::Target::Workdir {
                    return refuse(
                        crate::bundle::BUNDLE_INVALID,
                        format!(
                            "bundle {id} ({asked_by}) is a home bundle: name it in a flock's bundles, not workdir_bundles"
                        ),
                    );
                }
                let given = entry.map(|f| &f.bundle_vars);
                if let Some(var) = manifest
                    .var_names()
                    .find(|v| !given.is_some_and(|g| g.contains_key(*v)))
                {
                    return refuse(
                        crate::bundle::BUNDLE_INVALID,
                        format!(
                            "bundle {id} ({asked_by}) needs bundle_vars {var}, which flock {flock} does not give"
                        ),
                    );
                }
            }
        }
        let own = spec
            .workdir
            .as_deref()
            .and_then(|w| w.set.get("id"))
            .and_then(|id| id.as_str());
        if let Some(clash) = own.and_then(|own| list.iter().find(|b| b.id == own)) {
            return refuse(
                crate::bundle::workdir::WORKDIR_CONFLICT,
                format!(
                    "the task's workdir set is named {}, as is the workdir bundle {} from {}: give the set another id",
                    clash.id,
                    clash.id,
                    clash.from.as_deref().unwrap_or(asked_by)
                ),
            );
        }
        spec.workdir_bundles = list;
        Ok(())
    }

    /// A task's workdir set as it is queued in `flock`, once `settle_agent`
    /// named its agent and profile: a set for that kind, naming that profile
    /// or none, on a machine the head can place
    /// it on (`workdir_task`). A task pinned to a pull or command machine,
    /// or in a flock with no other, is refused rather than left to wait.
    fn settle_workdir(
        &self,
        spec: &mut crate::task::DispatchSpec,
        flock: &str,
    ) -> Result<(), AgentRefusal> {
        use crate::bundle::workdir_task;
        let profile = spec.profile().map(str::to_string);
        let kind = self.agents.read().recover().kind(&spec.agent).to_string();
        let machine = spec.machine.clone();
        let Some(w) = spec.workdir.as_mut() else {
            return Ok(());
        };
        // The head renders it only for a pull machine's claim, never on a
        // queued row: a rendering a client sent is not the head's.
        w.render = None;
        // Checked only: dispatch fills in the profile the machine settles.
        workdir_task::settle(&w.set, &kind, profile.as_deref())?;
        let wanted = self.wanted.read().recover();
        if let Some(m) = machine.as_deref().and_then(|m| wanted.get(m))
            && let Some(refused) = workdir_task::refusal_for(m)
        {
            return Err(refused);
        }
        let takers = wanted
            .members(flock)
            .into_iter()
            .filter_map(|m| wanted.get(m))
            .any(workdir_task::may_take_sets);
        if machine.is_none() && !takers {
            return Err(AgentRefusal {
                code: crate::bundle::workdir::WORKDIR_UNSUPPORTED,
                message: format!(
                    "no machine of flock {flock} takes a workdir set: pastor places a task's set on push and pull machines, not on command ones"
                ),
            });
        }
        Ok(())
    }

    /// A task's timeout and place as it is queued in `flock`, from what its
    /// run or job asked for, else the flock's, else `[defaults]`
    /// (`Defaults::resolve_timeout`, `resolve_place`), with where each came
    /// from in its `agent_source`. `settle_agent` makes that first. An ask
    /// from a client that predates flock timeouts carries neither, and
    /// keeps the spec's own unless the flock sets one.
    fn settle_run(
        &self,
        spec: &mut crate::task::DispatchSpec,
        ask: &AgentChoice,
        flock: &str,
        asked_by: &str,
    ) {
        let wanted = self.wanted.read().recover();
        let entry = wanted.entry(flock);
        let defaults = self.defaults.read().recover();
        let label = |layer| layer_label(layer, asked_by, flock, None);
        let (timeout, timeout_from) = defaults.resolve_timeout(ask.timeout_secs, entry);
        let (place, place_from) = defaults.resolve_place(ask.place.as_ref(), entry);
        let mut from = (Some(label(timeout_from)), Some(label(place_from)));
        if timeout_from == Layer::Defaults {
            from.0 = None;
        } else {
            spec.timeout_secs = timeout;
        }
        if place_from == Layer::Defaults {
            from.1 = None;
        } else {
            spec.place = place;
        }
        if let Some(source) = spec.agent_source.as_mut() {
            (source.timeout_from, source.place_from) = from;
        }
    }

    /// A task's `summary` setting as it is queued in `flock`
    /// (`Defaults::resolve_summary`), from the flock and defaults as they
    /// stand now.
    fn settle_summary(
        &self,
        ask: Option<crate::task::SummaryMode>,
        flock: &str,
    ) -> crate::task::SummaryMode {
        let wanted = self.wanted.read().recover();
        self.defaults
            .read()
            .recover()
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
        // may run and which profile an agy machine runs.
        let per_machine = |e: &AgentRefusal| {
            pinned.is_none()
                && ((e.code == MODEL_KIND_MISMATCH && ask.agent.is_none())
                    || e.code == AGENT_KIND_MISSING
                    || e.code == PROFILE_NOT_ALLOWED
                    || e.code == crate::config::agy::AGY_PROFILE_MISMATCH)
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
        let agents = self.agents.read().recover();
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
            .recover()
            .iter()
            .any(|m| m.handle.name == name && m.shutting_down)
    }

    /// Is `name` in the flock? For a managed fleet that is the flock last
    /// applied, so a removed machine held only until its old actor ends is
    /// not. A fixed fleet has no applied flock; its machines are the flock,
    /// plus those of the flock file it was given (`with_flock`), which is
    /// all the daemon-less scheduler has.
    pub fn in_flock(&self, name: &str) -> bool {
        let wanted = self.wanted.read().recover().get(name).is_some();
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
        *self.wanted.write().recover() = flock;
    }

    /// Hold the dispatch lock, as a dispatch pass does, for a test.
    #[cfg(test)]
    pub async fn hold_dispatch_lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.dispatch_lock.lock().await
    }

    /// Is any machine waiting for its old actor to end?
    pub fn any_shutting_down(&self) -> bool {
        self.members
            .read()
            .recover()
            .iter()
            .any(|m| m.shutting_down)
    }

    /// Make the running set match `flock` and `settings`: spawn actors for
    /// new machines, stop the ones for machines that are gone, and replace
    /// the ones whose entry or timings changed. A machine whose entry is the
    /// same keeps its actor, connection and event stream.
    ///
    /// Takes the dispatch lock, so no pass is placing tasks on a handle that
    /// is being stopped; a send a pass placed before the lock was taken fails
    /// with `ActorStopped`, and its task stays queued. The old actors are
    /// stopped together. Stopping an actor touches nothing on its machine;
    /// tasks left there keep their last state. A replacement is spawned, or a removed
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
        *self.wanted.write().recover() = flock.clone();
        let mut diff = FlockDiff::default();
        // A copy: readers keep seeing the old set until the new one is ready,
        // and the lock is not held across the waits below. The dispatch lock
        // keeps any other `apply_flock` out meanwhile.
        let mut old: Vec<Member> = self.members.read().recover().clone();
        enum Step {
            Keep(Member),
            Add,
            /// Replace this member once its actor has ended.
            Replace(Member),
        }
        let mut plan: Vec<Step> = Vec::new();
        for m in &flock.machines {
            let want = (actor_config(m), self.settings_of(m, settings));
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
        for m in self.members.write().recover().iter_mut() {
            if stopping.contains(&m.handle.name) {
                m.shutting_down = true;
            }
        }
        // Every old actor is stopped at once, so a reload that stops several
        // waits about one `SHUTDOWN_WAIT` for them, not one each.
        let mut stops = tokio::task::JoinSet::new();
        for o in plan
            .iter()
            .filter_map(|s| match s {
                Step::Replace(o) => Some(o),
                _ => None,
            })
            .chain(&gone)
        {
            let handle = o.handle.clone();
            stops.spawn(async move { (handle.name.clone(), handle.shutdown().await) });
        }
        let mut stopped: HashMap<String, ShutdownOutcome> = HashMap::new();
        while let Some(res) = stops.join_next().await {
            match res {
                Ok((name, outcome)) => {
                    stopped.insert(name, outcome);
                }
                Err(err) => tracing::warn!(%err, "stop an actor"),
            }
        }
        // A stop that panicked is treated as one that has not finished: the
        // next reload pass waits for it again.
        let outcome = |name: &str| {
            stopped
                .get(name)
                .copied()
                .unwrap_or(ShutdownOutcome::StillRunning)
        };
        let mut members: Vec<Member> = Vec::new();
        for (step, m) in plan.into_iter().zip(&flock.machines) {
            members.push(match step {
                Step::Keep(kept) => kept,
                Step::Add => {
                    diff.added.push(m.name.clone());
                    self.spawn(spawner, m, settings)
                }
                Step::Replace(o) => match outcome(&o.handle.name) {
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
            match outcome(&o.handle.name) {
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
            .recover()
            .retain(|name, _| members.iter().any(|m| m.pull && &m.handle.name == name));
        *self.members.write().recover() = members;
        diff
    }

    /// Keep `m` in the fleet, out of dispatch, until its actor ends.
    fn hold(mut m: Member) -> Member {
        m.shutting_down = true;
        m.handle.status.write().recover().error =
            Some("shutting down: the old actor has not stopped yet".into());
        m
    }

    /// The settings `m`'s actor runs with: `actor_settings`, and its own
    /// profile (`own_policy`).
    fn settings_of(&self, m: &MachineConfig, settings: &MachineSettings) -> MachineSettings {
        MachineSettings {
            own_policy: self.own_policy(m),
            harness_pins: self.flock().pins_for(m),
            // Only the manifests the machine's pins read, so editing another
            // replaces no actor.
            harnesses: Arc::new(
                self.harnesses()
                    .only(self.flock().pins_for(m).keys().map(String::as_str)),
            ),
            bundle_gate: crate::bundle::keep::GateHandle(
                self.keeper()
                    .map(|k| k as Arc<dyn crate::bundle::keep::Gate>),
            ),
            // Only where the head can place a set: a task with one is never
            // placed elsewhere (`MachineView::workdir_sets`).
            workdir_gate: crate::bundle::workdir_task::GateHandle(
                crate::bundle::workdir_task::takes_sets(m)
                    .then(|| self.workdirs.read().recover().clone())
                    .flatten()
                    .map(|g| g as Arc<dyn crate::bundle::workdir_task::Gate>),
            ),
            env: crate::config::EnvLayers::of(&self.defaults(), &self.flock(), m),
            ..actor_settings(m, settings)
        }
    }

    /// `m`'s own profile, from the flock and `pastor.toml` as last applied:
    /// its `[[machine]]` profile, else its primary flock's, else
    /// `[defaults]` (`own_profile`). `None` with no profile, or one that no
    /// longer resolves (`check_config` refuses that file anyway).
    pub fn own_policy(&self, m: &MachineConfig) -> Option<crate::config::agy::Policy> {
        let flock = self.flock().primary_flock(m).to_string();
        let name = self.own_profile(&flock, Some(&m.name))?;
        self.profiles
            .read()
            .recover()
            .resolve(&name)
            .ok()
            .map(|p| p.agy_policy())
    }

    fn spawn(&self, spawner: &Spawner, m: &MachineConfig, settings: &MachineSettings) -> Member {
        let settings = &self.settings_of(m, settings);
        if m.pull {
            // Heard from as of now: a head that starts gives the machine
            // `pull_lost_after` to claim before its tasks go stale.
            self.pull_seen.lock().recover().insert(
                m.name.clone(),
                PullSeen {
                    at: tokio::time::Instant::now(),
                    lost: false,
                    places_sets: false,
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

    /// Every machine as dispatch sees it: the tasks on it in the store, as
    /// its actor counts them (`refresh_live`), plus the ones passes are
    /// sending it and it has not claimed yet (`Placed`). Read from the
    /// store rather than the actor's last count, so a pass sees a task
    /// another placed and the actor claimed at once, and not twice.
    pub fn views(&self) -> Vec<MachineView> {
        self.views_where(false)
    }

    /// `views`, with a pull machine healthy while it is connected: what
    /// `pastor queue` explains a wait with. A pass never pushes to one,
    /// but its claims take a task under the same room and seat rules, so
    /// a task pinned to it waits for a slot or its flock's seat, not for a
    /// connection it has. The view says it is a pull machine, so a loose
    /// task is never read as going there (`queue::why_waiting`).
    fn queue_views(&self) -> Vec<MachineView> {
        self.views_where(true)
    }

    fn views_where(&self, pull_claims: bool) -> Vec<MachineView> {
        let wanted = self.flock();
        let defaults = self.defaults.read().recover().clone();
        let in_flight = self.in_flight.placed();
        self.members
            .read()
            .recover()
            .iter()
            .map(|m| {
                let mut s = m.handle.snapshot();
                // On a store error the actor's last count stands.
                let on_it = self.store.tasks_on_machine(&m.handle.name).ok();
                if let Some(tasks) = &on_it {
                    crate::machine::count_live(&mut s, tasks);
                    s.live += s.orphans.len();
                }
                let claimed = |id: i64| on_it.iter().flatten().any(|t| t.id == id);
                for p in in_flight
                    .iter()
                    .filter(|p| p.machine == m.handle.name && !claimed(p.task_id))
                {
                    s.live += 1;
                    s.live_jobs += usize::from(p.from_job);
                    s.live_by_flock.push((p.flock.clone(), 1));
                }
                let flocks = seats(&wanted, &s);
                // A machine held only until its old actor ends has no
                // entry, and so no floor. The head never reads a pull
                // machine's room (`Fleet::claim`), so its floor stays at
                // the default too.
                let floor = (!m.pull)
                    .then(|| wanted.get(&m.handle.name))
                    .flatten()
                    .map(|entry| {
                        let (short, unread) = crate::dispatch::floor_against(
                            &m.handle.name,
                            s.room.as_ref(),
                            defaults.resolve_floor(entry),
                        );
                        let hold_unread = flocks
                            .iter()
                            .filter(|f| {
                                !crate::config::resolve_unknown_room(wanted.entry(&f.name), entry)
                                    .counts_as_room()
                            })
                            .map(|f| f.name.clone())
                            .collect();
                        FloorView {
                            short,
                            unread,
                            hold_unread,
                            unreachable: false,
                            cordoned: false,
                        }
                    })
                    .unwrap_or_default();
                // A machine the head found cannot reach it, in the last check
                // it ran, is held like one under its floor.
                let floor = FloorView {
                    unreachable: s.reach.as_ref().is_some_and(|r| !r.ok),
                    cordoned: self.is_cordoned(&m.handle.name),
                    ..floor
                };
                // The slots kept for panes pastor did not start are not its
                // to fill: read from the flock last applied, like the floor.
                let reserved = wanted.get(&m.handle.name).map_or(0, |e| e.reserved);
                MachineView {
                    name: m.handle.name.clone(),
                    max_agents: m.handle.max_agents.saturating_sub(reserved),
                    job_slots: m.handle.job_slots,
                    burst: m.handle.burst,
                    tags: m.handle.tags.clone(),
                    loads: wanted
                        .get(&m.handle.name)
                        .map(|c| wanted.loads_of(c))
                        .unwrap_or_default(),
                    live: s.live,
                    live_jobs: s.live_jobs,
                    // An aborted actor answers nothing, and a dispatch to
                    // it would wait for as long as it stays wedged. A pull
                    // machine takes its tasks itself (`claim`).
                    healthy: (pull_claims || !m.pull)
                        && !m.shutting_down
                        && s.channel.accepts_dispatch()
                        && self.bootstrap_ready(&m.handle.name),
                    pull: m.pull,
                    workdir_sets: if m.pull {
                        self.pull_places_sets(&m.handle.name)
                    } else {
                        wanted
                            .get(&m.handle.name)
                            .is_some_and(crate::bundle::workdir_task::takes_sets)
                    },
                    flocks,
                    waiting_under_share: Vec::new(),
                    floor,
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
    /// `--priority`; without it the flock, the pinned machine or
    /// `[defaults]` set the level.
    pub async fn queue_run(
        &self,
        prompt: String,
        spec: crate::task::DispatchSpec,
        flock: Option<&str>,
        ask: Option<&AgentChoice>,
        priority: Option<Priority>,
    ) -> Result<Task, QueueError> {
        self.queue_run_as(RunAsk {
            prompt,
            spec,
            flock,
            ask,
            priority,
            role: TaskRole::Agent,
            description: None,
            preempt: false,
            summary: None,
        })
        .await
    }

    /// `queue_run`, for a task of `role` (`task run --role`). `summary` is
    /// `task run --summary`; without it the flock or `[defaults]` decide.
    pub async fn queue_run_as(&self, run: RunAsk<'_>) -> Result<Task, QueueError> {
        let RunAsk {
            prompt,
            mut spec,
            flock,
            ask,
            priority,
            role,
            description,
            preempt,
            summary,
        } = run;
        let _pass = self.dispatch_lock.lock().await;
        if let Some(m) = &spec.machine
            && !self.in_flock(m)
        {
            return Err(QueueError::UnknownMachine(m.clone()));
        }
        if spec.now {
            let refuse = |code, message| {
                Err(QueueError::Agent(crate::config::AgentRefusal {
                    code,
                    message,
                }))
            };
            let Some(m) = &spec.machine else {
                return refuse(
                    "now_needs_machine",
                    "--now needs --machine: name the machine the task may run past the limits of"
                        .into(),
                );
            };
            // A pull machine counts while the head hears from it: it takes
            // the task on its next claim (`claim`).
            if !self.queue_views().iter().any(|v| &v.name == m && v.healthy) {
                return refuse(
                    "machine_not_connected",
                    format!("machine {m} is not connected, so --now cannot start the task there"),
                );
            }
            // Unlike its room floor, a cordon is not one --now runs past.
            if self.is_cordoned(m) {
                return refuse(
                    MACHINE_CORDONED,
                    format!(
                        "machine {m} is cordoned, so --now cannot start the task there: it runs past a room floor, not a cordon; `pastor machine uncordon {m}` first"
                    ),
                );
            }
        }
        let flock = self
            .flock()
            .task_flock(flock, spec.machine.as_deref())
            .map_err(QueueError::Flock)?;
        if let Some(ask) = ask {
            self.settle_agent(&mut spec, ask, &flock, "task run")
                .map_err(QueueError::Agent)?;
            self.settle_run(&mut spec, ask, &flock, "task run");
        }
        self.settle_load(&mut spec, &flock, "task run")
            .map_err(QueueError::Agent)?;
        self.settle_workdir(&mut spec, &flock)
            .map_err(QueueError::Agent)?;
        self.settle_workdir_bundles(&mut spec, &flock, "task run")
            .map_err(QueueError::Agent)?;
        self.settle_label(&mut spec, &flock, "task run");
        self.settle_keep_pane(&mut spec, &flock, "task run");
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
            fallback: job.fallback_for(item).map_err(anyhow::Error::msg)?,
            ..job.agent.clone()
        };
        let asked_by = format!("job {}", job.name);
        self.settle_agent(&mut settled, &ask, &flock, &asked_by)
            .map_err(|e| anyhow::Error::msg(e.message))?;
        self.settle_run(&mut settled, &ask, &flock, &asked_by);
        self.settle_load(&mut settled, &flock, &asked_by)
            .map_err(|e| anyhow::Error::msg(e.message))?;
        self.settle_workdir(&mut settled, &flock)
            .map_err(|e| anyhow::Error::msg(format!("{} ({})", e.message, e.code)))?;
        self.settle_workdir_bundles(&mut settled, &flock, &asked_by)
            .map_err(|e| anyhow::Error::msg(format!("{} ({})", e.message, e.code)))?;
        self.settle_label(&mut settled, &flock, &asked_by);
        self.settle_keep_pane(&mut settled, &flock, &asked_by);
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
        self.store
            .insert_job_task_at(crate::store::InsertJobTaskAt {
                job: &job.name,
                flock: &flock,
                item,
                level,
                preempt,
                description: description.as_deref(),
                render: |id| {
                    let (prompt, mut spec) = render(id)?;
                    spec.agent = settled.agent;
                    spec.agent_args = settled.agent_args;
                    spec.allow = settled.allow;
                    spec.deny = settled.deny;
                    spec.agent_source = settled.agent_source;
                    spec.timeout_secs = settled.timeout_secs;
                    spec.place = settled.place;
                    spec.label = settled.label;
                    spec.keep_pane = settled.keep_pane;
                    spec.keep_pane_from = settled.keep_pane_from;
                    spec.remove_worktree = settled.remove_worktree;
                    spec.remove_worktree_from = settled.remove_worktree_from;
                    spec.load = settled.load;
                    spec.load_from = settled.load_from;
                    spec.workdir = settled.workdir;
                    spec.workdir_bundles = settled.workdir_bundles;
                    spec.summary = summary;
                    // Only a person's `task run --now` skips the queue.
                    spec.now = false;
                    Ok((prompt, spec))
                },
            })
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
    /// job's next run checks. An unreachable head, a head too old for the
    /// job's dispatch table (`head_too_old`), or an error reply such as
    /// `job_name_taken`, is an `Err` with its code, and nothing is kept.
    pub async fn submit_to_head(
        &self,
        job: &crate::config::job::Job,
        items: Vec<serde_json::Value>,
    ) -> anyhow::Result<crate::scheduler::Submitted> {
        let Some(forward) = &self.forward else {
            anyhow::bail!("this pastor serve is the head; it queues its jobs' items itself");
        };
        // What the job's `[dispatch]` names (a model, a profile, a label)
        // rides in `dispatch`; the forward refuses a head too old for any of
        // it before the submit goes (`IpcRequest::min_protocol`).
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
            .recover()
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
    /// or names a model `[models]` or a bundle the bundles dir lacks, leaves
    /// the wanted flock as it was; that reload logs it and falls back to
    /// this flock, so it must still be the last valid one.
    pub async fn edit_flock_file<T>(
        &self,
        file: &std::path::Path,
        provisioners: &crate::provision::Catalog,
        bundles: &crate::bundle::Catalog,
        edit: impl FnOnce(&std::path::Path) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let _pass = self.dispatch_lock.lock().await;
        let done = edit(file)?;
        // A fixed fleet has no applied flock, which a reload leaves alone too.
        if self.spawner.is_some()
            && let Ok(flock) = Flock::load_existing(file)
            && flock
                .check_config(
                    &self.models.read().recover(),
                    &self.agents.read().recover(),
                    &self.profiles.read().recover(),
                    provisioners,
                )
                .is_ok()
            && flock.check_bundles(bundles).is_ok()
        {
            *self.wanted.write().recover() = flock;
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
                        .recover()
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
        // A pass decides under this lock and sends after it; a task it
        // placed keeps its queued row until the send ends.
        if self.in_flight.holds(id) {
            return Err(PriorityError::InFlight(id));
        }
        self.store
            .set_priority_preempting(id, priority, from, preempt)
    }

    /// Move a queued task (`Store::move_queued`), under the dispatch lock
    /// for the reason `set_priority` is: a pass must not take the queue in
    /// the old order after the move has answered. A task in flight is also
    /// excluded from the queue this moves within: its row still says
    /// `queued`, but a dispatch pass has already placed it, so it must not
    /// be `--before` or `--after`'s target, or counted toward `--to`'s
    /// position (`Store::move_queued`).
    pub async fn move_queued(&self, id: i64, to: QueueSpot) -> Result<Moved, MoveError> {
        let _pass = self.dispatch_lock.lock().await;
        // As in `set_priority_preempting`.
        if self.in_flight.holds(id) {
            return Err(MoveError::InFlight(id));
        }
        let in_flight: Vec<i64> = self.in_flight.placed().iter().map(|p| p.task_id).collect();
        self.store.move_queued(id, to, &in_flight)
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
            let target = task.flock.as_deref().unwrap_or(wanted.default_flock());
            self.settled_on(task, target, machine)
                .is_none_or(|r| r.is_ok())
        };
        let mut entries = crate::queue::entries(
            queued,
            &self.queue_views(),
            wanted.default_flock(),
            &accepts,
        );
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
        let now = chrono::Utc::now();
        let live = self.live_limits(now);
        let Some(source) = task.spec.agent_source.as_ref() else {
            // It keeps its agent, so only that agent's limit can stop it.
            let held = self.limits_holding(&live, machine, &task.spec);
            return (!held.is_empty()).then(|| Err(exhausted(&held, now)));
        };
        let mut ask = source.ask.clone();
        if ask.profile.is_none() {
            ask.profile = source.profile.clone();
        }
        let by = asked_by(task);
        let mut spec = task.spec.clone();
        if let Err(e) = self.settle(&mut spec, &ask, flock, Some(machine), &by) {
            return Some(Err(e));
        }
        if let Err(e) = self.workdir_fits(&spec) {
            return Some(Err(e));
        }
        let mut held = self.limits_holding(&live, machine, &spec);
        if held.is_empty() {
            return Some(Ok(spec));
        }
        // Its own model's account is exhausted here: the first model of its
        // fallback list whose account is not, as that model settles here.
        let primary = spec.agent_source.clone().expect("settled");
        for (i, name) in primary.fallback.iter().enumerate() {
            let Some(mut fb) = self.settle_model(task, flock, machine, name) else {
                continue;
            };
            let more = self.limits_holding(&live, machine, &fb);
            if !more.is_empty() {
                for l in more {
                    if !held
                        .iter()
                        .any(|h| h.account == l.account && h.model == l.model)
                    {
                        held.push(l);
                    }
                }
                continue;
            }
            if let Some(src) = fb.agent_source.as_mut() {
                src.model_from = primary.fallback_from.clone();
                src.fallback = primary.fallback.clone();
                src.fallback_from = primary.fallback_from.clone();
                src.fallback_use = Some(crate::task::FallbackUse {
                    pos: i + 1,
                    of: primary.fallback.len(),
                    why: held
                        .iter()
                        .map(|l| l.short_note(now))
                        .collect::<Vec<_>>()
                        .join(", "),
                });
            }
            return Some(Ok(fb));
        }
        Some(Err(exhausted(&held, now)))
    }

    /// `task`'s spec settled for `machine` in `flock` with the `[models]`
    /// name `model` in place of its own, as its ask would have it there;
    /// `None` when the machine cannot run it.
    fn settle_model(
        &self,
        task: &Task,
        flock: &str,
        machine: &str,
        model: &str,
    ) -> Option<crate::task::DispatchSpec> {
        let source = task.spec.agent_source.as_ref()?;
        let mut ask = source.ask.clone();
        if ask.profile.is_none() {
            ask.profile = source.profile.clone();
        }
        ask.model = Some(model.to_string());
        let mut spec = task.spec.clone();
        self.settle(&mut spec, &ask, flock, Some(machine), &asked_by(task))
            .ok()?;
        // A model whose agent cannot take the task's workdir set is not one
        // the task can move to: its start would fail.
        self.workdir_fits(&spec).ok()?;
        Some(spec)
    }

    /// Whether `spec`'s settled agent and profile take its workdir set
    /// (`workdir_task::settle`). A task with no set fits any agent.
    fn workdir_fits(&self, spec: &crate::task::DispatchSpec) -> Result<(), AgentRefusal> {
        let Some(w) = spec.workdir.as_deref() else {
            return Ok(());
        };
        let kind = self.agents.read().recover().kind(&spec.agent).to_string();
        crate::bundle::workdir_task::settle(&w.set, &kind, spec.profile()).map(|_| ())
    }

    /// What waiting `task`, pinned to `machine`, does now that its time has
    /// come, as its limit left it (`task::LimitStop::then`) and `live`
    /// holds its models there. Its own model is its own session; another
    /// model is a new round. A task that was to move on takes the first
    /// free model after its own, else before it; with every one of them
    /// exhausted, or none it can run here, it waits.
    fn wake(&self, task: &Task, machine: &str, live: &[AccountLimit]) -> Wake {
        use crate::task::NextRound;
        let own = self.limits_holding(live, machine, &task.spec);
        let then = task
            .spec
            .rounds
            .stop
            .as_deref()
            .map_or(NextRound::Own, |s| s.then);
        let source = task.spec.agent_source.as_deref();
        let Some(source) = source.filter(|s| then != NextRound::Own && !s.fallback.is_empty())
        else {
            return if own.is_empty() {
                Wake::Own
            } else {
                Wake::Hold
            };
        };
        let flock = self.flock();
        let target = task.flock.as_deref().unwrap_or(flock.default_flock());
        // Its models in order: the one it started on, then its list.
        let current = task.model().map(str::to_string);
        let first = match task.spec.rounds.ended.first() {
            Some(r) => r.model.clone(),
            None => current.clone(),
        };
        let mut list = vec![first];
        for m in &source.fallback {
            if !list.contains(&Some(m.clone())) {
                list.push(Some(m.clone()));
            }
        }
        let from = match then {
            NextRound::Switch => list.iter().position(|m| *m == current).map_or(0, |i| i + 1),
            _ => 0,
        };
        let mut held: Vec<AccountLimit> = Vec::new();
        let mut hold = |more: Vec<AccountLimit>| {
            for l in more {
                if !held
                    .iter()
                    .any(|h| h.account == l.account && h.model == l.model)
                {
                    held.push(l);
                }
            }
        };
        // A move tries the models after its own first, then the ones
        // before it: they are its models too, and a wait lasts until the
        // earliest reset among all of them.
        let (before, after) = list.split_at(from);
        let mut runnable = false;
        for model in after.iter().chain(before) {
            if *model == current {
                // Its own model just stopped on the limit: a move does not
                // go back to it before `waiting_until`.
                if then != NextRound::Switch && own.is_empty() {
                    return Wake::Own;
                }
                continue;
            }
            let Some(name) = model else { continue };
            let Some(spec) = self.settle_model(task, target, machine, name) else {
                continue;
            };
            runnable = true;
            let more = self.limits_holding(live, machine, &spec);
            if more.is_empty() {
                return Wake::Switch(Box::new(spec));
            }
            hold(more);
        }
        if then != NextRound::Switch {
            return Wake::Hold;
        }
        // Its own model does not make it `runnable`: that is always in the
        // list. With no other model this machine can run, it waits on its
        // own, as a task with no list does.
        if !runnable {
            return Wake::Wait {
                why: "no_fallback",
                held: own,
            };
        }
        hold(own);
        Wake::Wait {
            why: "all_exhausted",
            held,
        }
    }

    /// Move waiting `task` to `spec`, its next model as `wake` settled it
    /// on `machine`: the round that stopped on the limit ends with `limit:
    /// <line>`, the new one carries on in the same session when the agent
    /// is of the same kind and account and has one, else from the prompt
    /// with the end of the last pane. The row is written for the resume to
    /// claim, and `task.agent_switched` says so.
    fn switch_model(
        &self,
        task: &Task,
        machine: &str,
        mut spec: crate::task::DispatchSpec,
    ) -> anyhow::Result<Option<Task>> {
        use crate::task::{AgentRound, Handover, NextRound};
        let Some(mut stop) = task.spec.rounds.stop.clone() else {
            return Ok(None);
        };
        let old = task.spec.agent_source.as_deref();
        let (same_kind, same_account) = {
            let agents = self.agents.read().recover();
            (
                agents.kind(&task.spec.agent) == agents.kind(&spec.agent),
                agents.limit_key(machine, &task.spec.agent)
                    == agents.limit_key(machine, &spec.agent),
            )
        };
        let handover = if same_kind && same_account && task.spec.session_id.is_some() {
            Handover::Session
        } else {
            spec.session_id = None;
            Handover::PaneTail
        };
        if let (Some(src), Some(old)) = (spec.agent_source.as_mut(), old) {
            src.model_from = Some(format!("fallback after a {}", stop.why.replace('_', " ")));
            src.fallback = old.fallback.clone();
            src.fallback_from = old.fallback_from.clone();
            src.fallback_use = None;
        }
        spec.rounds = task.spec.rounds.clone();
        spec.rounds.ended.push(AgentRound {
            agent: task.spec.agent.clone(),
            model: task.model().map(str::to_string),
            ended: format!("limit: {}", stop.line),
        });
        let from = serde_json::json!({ "agent": task.spec.agent, "model": task.model() });
        stop.then = NextRound::Own;
        stop.waited = None;
        stop.handover = Some(handover);
        let (why, until, line) = (stop.why.clone(), stop.until, stop.line.clone());
        spec.rounds.stop = Some(stop);
        let Some(mut fresh) = self.store.get_task(task.id)? else {
            return Ok(None);
        };
        if fresh.state != TaskState::Waiting || fresh.spec != task.spec {
            return Ok(None);
        }
        fresh.spec = spec;
        self.store.update_task(&mut fresh)?;
        if let Err(err) = self.store.end_round_on_limit(task.id, &line) {
            tracing::error!(task = %task.display_id(), %err, "save the round's summary");
        }
        tracing::info!(task = %task.display_id(), machine, from = %task.spec.agent, to = %fresh.spec.agent, model = ?fresh.model(), "moves to its next model");
        self.emit(
            "task.agent_switched",
            machine,
            Some(&fresh),
            Some(serde_json::json!({
                "from": from,
                "to": { "agent": fresh.spec.agent, "model": fresh.model() },
                "why": why,
                "until": until,
                "handover": handover.as_str(),
            })),
        );
        Ok(Some(fresh))
    }

    /// Keep waiting `task` until the earliest of `held`, the limits on
    /// every model it may run: no other one was free
    /// (`Wake::Wait`). At that time it starts on the first of its models
    /// that is free. With `no_fallback` it has no other model here: it
    /// waits on its own and resumes on it, and `task.waiting` names that
    /// account and model as a wait with no list does. Emits
    /// `task.waiting`.
    fn wait_longer(
        &self,
        task: &Task,
        machine: &str,
        why: &str,
        held: &[AccountLimit],
    ) -> anyhow::Result<()> {
        let now = chrono::Utc::now();
        let Some(mut fresh) = self.store.get_task(task.id)? else {
            return Ok(());
        };
        if fresh.state != TaskState::Waiting || fresh.spec != task.spec {
            return Ok(());
        }
        let until = held
            .iter()
            .map(|l| l.retry_at)
            .min()
            .or(fresh.spec.rounds.stop.as_ref().map(|s| s.until))
            .unwrap_or(now);
        let own = why == "no_fallback";
        fresh.waiting_until = Some(until);
        if let Some(stop) = fresh.spec.rounds.stop.as_mut() {
            stop.then = if own {
                crate::task::NextRound::Own
            } else {
                crate::task::NextRound::First
            };
        }
        self.store.update_task(&mut fresh)?;
        tracing::info!(task = %task.display_id(), machine, why, %until, "waiting for a usage limit to reset");
        let shown = format!("waiting {}", crate::limit::local_time(until, now));
        let detail = if own {
            serde_json::json!({
                "why": why,
                "account": held.first().map(|l| &l.account),
                "model": fresh.model(),
                "until": until,
                "shown": shown,
            })
        } else {
            let models: Vec<_> = held
                .iter()
                .map(|l| serde_json::json!({ "account": l.account, "model": l.model, "until": l.retry_at }))
                .collect();
            serde_json::json!({
                "why": why,
                "until": until,
                "models": models,
                "shown": shown,
            })
        };
        self.emit("task.waiting", machine, Some(&fresh), Some(detail));
        Ok(())
    }

    /// The live limits, as the store has them at `now`; none when it cannot
    /// be read, so a store error never holds work back.
    fn live_limits(&self, now: chrono::DateTime<chrono::Utc>) -> Vec<AccountLimit> {
        let mut seen = self.limits_seen.lock().recover();
        let rows = match &*seen {
            Some((at, rows)) if at.elapsed() < LIMITS_FRESH => rows.clone(),
            _ => match self.store.limits() {
                Ok(rows) => {
                    *seen = Some((std::time::Instant::now(), rows.clone()));
                    rows
                }
                Err(err) => {
                    tracing::warn!(%err, "read the usage limits");
                    Vec::new()
                }
            },
        };
        rows.into_iter().filter(|l| l.retry_at > now).collect()
    }

    /// Forget the limits as last read: the table changed.
    fn limits_changed(&self) {
        *self.limits_seen.lock().recover() = None;
    }

    /// Of `live`, the limits that stop `spec`'s agent and model on
    /// `machine`: those of its account (`Agents::limit_key`) for the whole
    /// account or for its model, and its signed-out mark there
    /// (`Agents::signed_out_key`).
    fn limits_holding(
        &self,
        live: &[AccountLimit],
        machine: &str,
        spec: &crate::task::DispatchSpec,
    ) -> Vec<AccountLimit> {
        let model = spec.agent_source.as_ref().and_then(|s| s.model.as_deref());
        live.iter()
            .filter(|l| self.holds(l, machine, &spec.agent, model))
            .cloned()
            .collect()
    }

    /// Whether `limit` stops `agent` running `model` on `machine`.
    fn holds(&self, limit: &AccountLimit, machine: &str, agent: &str, model: Option<&str>) -> bool {
        let agents = self.agents.read().recover();
        if limit.signed_out {
            limit.account == agents.signed_out_key(machine, agent)
        } else {
            limit.account == agents.limit_key(machine, agent) && limit.stops(model)
        }
    }

    /// Keep `limit` and say so (`agent.exhausted`, or `agent.signed_out`
    /// for a signed-out agent). A headless serve keeps none: the head owns
    /// the table.
    pub fn record_limit(&self, limit: &AccountLimit) -> anyhow::Result<()> {
        self.store.record_limit(limit)?;
        self.limits_changed();
        if limit.signed_out {
            tracing::warn!(account = %limit.account, machine = ?limit.machine, agent = ?limit.agent, "agent signed out");
        } else {
            tracing::warn!(account = %limit.account, model = ?limit.model, retry_at = %limit.retry_at, "account exhausted");
        }
        self.emit_limit(limit.kept_event(), limit, None);
        Ok(())
    }

    /// The live limit that stops `agent` running `model` on `machine` at
    /// `now`, if one does.
    pub fn limit_holding(
        &self,
        machine: &str,
        agent: &str,
        model: Option<&str>,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Option<AccountLimit> {
        self.live_limits(now)
            .into_iter()
            .find(|l| self.holds(l, machine, agent, model))
    }

    /// `[limits]` as last applied.
    pub fn limits_config(&self) -> LimitsConfig {
        self.limits.read().recover().clone()
    }

    /// Keep `limit`, which `task`'s agent stopped on at `now` on `machine`,
    /// under the account that agent names there (`Agents::limit_key`), or
    /// under the machine for a sign-out (`Agents::signed_out_key`); a limit
    /// that is one model's is kept for the task's model.
    pub fn note_limit(
        &self,
        task: &Task,
        machine: &str,
        limit: &crate::limit::Limit,
        now: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<AccountLimit> {
        let agents = self.agents.read().recover();
        let key = if limit.signed_out {
            agents.signed_out_key(machine, &task.spec.agent)
        } else {
            agents.limit_key(machine, &task.spec.agent)
        };
        let row = AccountLimit::of(
            &key,
            task.model(),
            limit,
            Some(task.id),
            Some(machine),
            Some(&task.spec.agent),
            now,
            &self.limits_config(),
        );
        self.record_limit(&row)?;
        Ok(row)
    }

    /// Drop the limits past their `retry_at`, each with `agent.reset`
    /// (`by: time`).
    pub fn expire_limits(&self) {
        match self.store.expire_limits(chrono::Utc::now()) {
            Ok(gone) => {
                if !gone.is_empty() {
                    self.limits_changed();
                }
                for l in gone {
                    tracing::info!(account = %l.account, model = ?l.model, "usage limit reset");
                    self.emit_limit("agent.reset", &l, Some("time"));
                }
            }
            Err(err) => tracing::warn!(%err, "expire the usage limits"),
        }
    }

    /// Clear `account`'s limits by hand (`limit clear`), each with
    /// `agent.reset` (`agent.signed_in` for a signed-out agent; `by:
    /// hand`), then dispatch what they held. A waiting task they held is due
    /// now, not at its reset.
    pub fn clear_limits(
        &self,
        account: &str,
        model: Option<&str>,
    ) -> anyhow::Result<Vec<AccountLimit>> {
        self.clear_limits_by(account, model, "hand")
    }

    /// `clear_limits`, with `by` saying who cleared them: `hand`, or
    /// `check` for a sign-in that `agent signed-in` saw.
    pub fn clear_limits_by(
        &self,
        account: &str,
        model: Option<&str>,
        by: &str,
    ) -> anyhow::Result<Vec<AccountLimit>> {
        let gone = self.store.clear_limits(account, model)?;
        self.limits_changed();
        for l in &gone {
            tracing::info!(account = %l.account, model = ?l.model, by, "usage limit cleared");
            self.emit_limit(l.cleared_event(), l, Some(by));
        }
        if !gone.is_empty() {
            self.wake_waiting(&gone);
        }
        Ok(gone)
    }

    /// Clear `account`'s signed-out marks, its usage limits left alone, each
    /// with `agent.signed_in` (`by`), then bring forward the waiting tasks
    /// they held.
    pub fn clear_signed_out(&self, account: &str, by: &str) -> anyhow::Result<Vec<AccountLimit>> {
        let gone = self.store.clear_signed_out(account)?;
        self.limits_changed();
        for l in &gone {
            tracing::info!(account = %l.account, by, "agent signed in");
            self.emit_limit(l.cleared_event(), l, Some(by));
        }
        if !gone.is_empty() {
            self.wake_waiting(&gone);
        }
        Ok(gone)
    }

    /// A connector to machine `m`, made the way its actor's is; `None` for
    /// a fixed fleet, which makes none.
    pub fn connector(&self, m: &MachineConfig) -> Option<Arc<dyn Connector>> {
        self.spawner.as_ref().map(|s| (s.connect)(m))
    }

    /// Bring forward the waiting tasks `gone` held, so the next pass
    /// resumes them. A store error leaves them to their reset.
    fn wake_waiting(&self, gone: &[AccountLimit]) {
        let waiting = match self.store.waiting_tasks() {
            Ok(w) => w,
            Err(err) => return tracing::error!(%err, "list waiting"),
        };
        let now = chrono::Utc::now();
        for task in waiting {
            let Some(machine) = task.pinned_machine() else {
                continue;
            };
            if self.limits_holding(gone, machine, &task.spec).is_empty() {
                continue;
            }
            if let Err(err) = self.store.wake_waiting(task.id, now) {
                tracing::error!(task = %task.display_id(), %err, "wake a waiting task");
            }
        }
    }

    /// `agent.exhausted` or `agent.reset` about `limit`, with `by` for a
    /// reset.
    fn emit_limit(&self, kind: &str, limit: &AccountLimit, by: Option<&str>) {
        let Some(spawner) = &self.spawner else {
            return;
        };
        let detail = limit.event_detail(by);
        let _ = spawner.events.send(PastorEvent {
            kind: kind.into(),
            task_id: limit.task_id,
            machine: limit.machine.clone(),
            job: None,
            detail: Some(detail),
            summary: None,
        });
    }

    /// Is `name` a pull machine (`MachineConfig::pull`) of the fleet?
    pub fn is_pull(&self, name: &str) -> bool {
        self.members
            .read()
            .recover()
            .iter()
            .any(|m| m.pull && m.handle.name == name)
    }

    /// The handle of pull machine `name`, or why a claim or report from it
    /// is refused.
    fn pull_handle(&self, name: &str) -> anyhow::Result<MachineHandle> {
        let members = self.members.read().recover();
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
            Ok(tasks) => crate::machine::count_live(&mut handle.status.write().recover(), &tasks),
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

    /// Whether pull machine `name`'s last claim said its serve places
    /// workdir sets, and the head has what it needs to build them (its
    /// workdir gate, for the bundles' paths).
    fn pull_places_sets(&self, name: &str) -> bool {
        self.workdirs.read().recover().is_some()
            && self
                .pull_seen
                .lock()
                .recover()
                .get(name)
                .is_some_and(|s| s.places_sets)
    }

    /// Pull machine `handle` was heard from now: it is connected, and one
    /// counted lost is announced back.
    fn heard(&self, handle: &MachineHandle) {
        let was_lost = {
            let mut seen = self.pull_seen.lock().recover();
            let e = seen.entry(handle.name.clone()).or_insert(PullSeen {
                at: tokio::time::Instant::now(),
                lost: false,
                places_sets: false,
            });
            e.at = tokio::time::Instant::now();
            std::mem::replace(&mut e.lost, false)
        };
        {
            let mut s = handle.status.write().recover();
            s.channel = crate::machine::ChannelState::Connected;
            s.error = None;
        }
        if was_lost {
            self.emit("machine.connected", &handle.name, None, None);
        }
    }

    /// Whether an operator cordoned `machine` (`pastor machine cordon`). A
    /// cordon the store cannot read holds the machine: the safe side.
    fn is_cordoned(&self, machine: &str) -> bool {
        match self.store.cordon(machine) {
            Ok(cordon) => cordon.is_some(),
            Err(err) => {
                tracing::warn!(%machine, %err, "read the machine's cordon; holding it");
                true
            }
        }
    }

    /// Whether dispatch may place work on `machine`: a pending or failed
    /// bootstrap state holds it back until a `BootstrapReport` says it is
    /// ok. No row reads as ok, so a machine from before bootstrap tracking
    /// is unaffected, and so is one whose state cannot be read: that is
    /// logged and does not wedge the fleet.
    fn bootstrap_ready(&self, machine: &str) -> bool {
        match self.store.machine_state(machine) {
            Ok(None) => true,
            Ok(Some(s)) => s.bootstrap == crate::store::BootstrapState::Ok,
            Err(err) => {
                tracing::error!(machine, %err, "read machine_state");
                true
            }
        }
    }

    /// `BootstrapReport`: what a bootstrap run found on `machine`, saved as
    /// its state and announced as `machine.bootstrapped` or
    /// `machine.bootstrap_failed`. The text in it comes from the machine, so
    /// it is cut to size and stripped of control characters before it is
    /// kept or shown. A host key already on record is never replaced by a
    /// report: one that names another is refused whole (`host_key_mismatch`).
    /// True when the report made a held machine ready, so the caller places
    /// what waited for it.
    pub fn report_bootstrap(
        &self,
        machine: &str,
        outcome: crate::bootstrap::Outcome,
    ) -> anyhow::Result<bool> {
        use crate::store::{BootstrapState, MachineState};
        if self.flock().get(machine).is_none() {
            return Err(crate::cli::CliError::err(
                "unknown_machine",
                format!("no machine {machine} in the flock"),
            ));
        }
        let clean = |s: &str| clean_report_text(s);
        let on_record = self.store.machine_state(machine)?;
        let host_key = match (
            on_record.as_ref().and_then(|s| s.host_key.clone()),
            outcome.host_key.as_deref().map(clean),
        ) {
            (Some(old), Some(new))
                if old
                    .split_whitespace()
                    .take(2)
                    .ne(new.split_whitespace().take(2)) =>
            {
                return Err(crate::cli::CliError::err(
                    "host_key_mismatch",
                    format!("{machine} reports host key {new}, not the {old} on record"),
                ));
            }
            (Some(old), _) => Some(old),
            (None, new) => new,
        };
        let failure = outcome
            .failure()
            .map(|(step, why)| (clean(&step), clean(&why)));
        let missing: Vec<String> = outcome
            .steps
            .iter()
            .flat_map(|s| s.missing.iter().map(|m| clean(m)))
            .take(20)
            .collect();
        let was_held = !self.bootstrap_ready(machine);
        self.store.put_machine_state(&MachineState {
            machine: machine.to_string(),
            bootstrap: if outcome.ok {
                BootstrapState::Ok
            } else {
                BootstrapState::Failed
            },
            isolation: Some(outcome.isolation.as_str().to_string()),
            failed_step: failure.as_ref().map(|f| f.0.clone()),
            why: failure.as_ref().map(|f| f.1.clone()),
            host_key,
            herdr_version: outcome.herdr_version.as_deref().map(clean),
            pastor_version: outcome.pastor_version.as_deref().map(clean),
            generation: on_record.as_ref().map_or(1, |s| s.generation),
            updated_at: chrono::Utc::now(),
        })?;
        match failure {
            None => self.emit(
                "machine.bootstrapped",
                machine,
                None,
                Some(serde_json::json!({
                    "isolation": outcome.isolation.as_str(),
                    "missing": missing,
                })),
            ),
            Some((step, why)) => self.emit(
                "machine.bootstrap_failed",
                machine,
                None,
                Some(serde_json::json!({ "step": step, "why": why })),
            ),
        }
        Ok(outcome.ok && was_held)
    }

    /// A bootstrap that failed before its steps ran on a machine already
    /// `ok`: nothing there changed, so its state stays as it is and only
    /// `machine.bootstrap_failed` says what happened.
    pub fn report_bootstrap_kept(
        &self,
        machine: &str,
        outcome: &crate::bootstrap::Outcome,
    ) -> anyhow::Result<()> {
        if self.flock().get(machine).is_none() {
            return Err(crate::cli::CliError::err(
                "unknown_machine",
                format!("no machine {machine} in the flock"),
            ));
        }
        let (step, why) = outcome
            .failure()
            .unwrap_or_else(|| ("the bootstrap".into(), String::new()));
        self.emit(
            "machine.bootstrap_failed",
            machine,
            None,
            Some(serde_json::json!({
                "step": clean_report_text(&step),
                "why": clean_report_text(&why),
            })),
        );
        Ok(())
    }

    /// Keep the `reach` row of a `machine check` of `machine` as its last
    /// reach result: a pass says it reaches the head, a failure that it does
    /// not (`FloorView::unreachable`). A check without the row (no
    /// `head_address`, a machine not reached by ssh) or whose row could not
    /// run leaves what was there. True when the result freed a held
    /// machine, so the caller places what waited for it.
    pub fn note_reach(&self, machine: &str, report: &crate::machine_check::Report) -> bool {
        let Some(row) = report
            .rows
            .iter()
            .find(|r| r.id == crate::machine_check::REACH_ROW)
        else {
            return false;
        };
        let ok = match row.status {
            crate::provision::run::Status::Pass => true,
            crate::provision::run::Status::Fail => false,
            _ => return false,
        };
        let Some(handle) = self.get(machine) else {
            return false;
        };
        let mut status = handle.status.write().recover();
        let was_held = status.reach.as_ref().is_some_and(|r| !r.ok);
        status.reach = Some(crate::machine::Reach {
            ok,
            at: chrono::Utc::now(),
            why: (!ok).then(|| clean_report_text(row.reason.as_deref().unwrap_or(&row.first_line))),
        });
        ok && was_held
    }

    /// `TaskClaim`: at most `free_slots` queued tasks for pull machine
    /// `machine` to start, each `starting` there before this returns. Those
    /// pinned to it first, then, with `flock_work`, any a dispatch pass
    /// would place there now; both only while the machine and the task's
    /// flock have room, as for any machine. Ahead of them, and not counted
    /// in `free_slots`, the `--now` tasks pinned to it, under the rule a
    /// pass places one by on a push machine (`now_machine`): past its
    /// slots and its flock's seat, but not past a cordon, a flock it left
    /// or is drained from, its tags or the task's model. Under the dispatch
    /// lock, so a pass and a claim never take the same slot.
    ///
    /// With `workdir_sets` (its serve places sets itself) a task with a set
    /// may go too, each handed with its sets settled, built and rendered
    /// for the machine (`workdir_task::for_claim`), at most
    /// `MAX_CLAIM_SETS` of them in all. One whose sets cannot be built
    /// fails here, `workdir_failed`, and is handed to nobody. A `--now`
    /// task goes whatever the sets before it took.
    pub async fn claim(
        &self,
        machine: &str,
        free_slots: u32,
        flock_work: bool,
        workdir_sets: bool,
    ) -> anyhow::Result<Vec<Task>> {
        use crate::bundle::workdir_task;
        let _pass = self.dispatch_lock.lock().await;
        let handle = self.pull_handle(machine)?;
        self.heard(&handle);
        if let Some(seen) = self.pull_seen.lock().recover().get_mut(machine) {
            seen.places_sets = workdir_sets;
        }
        let gate = self.workdirs.read().recover().clone();
        let places_sets = workdir_sets && gate.is_some();
        self.count_pull(&handle);
        let flock = self.flock();
        let s = handle.snapshot();
        let mut view = MachineView {
            name: machine.to_string(),
            max_agents: handle
                .max_agents
                .saturating_sub(flock.get(machine).map_or(0, |e| e.reserved)),
            job_slots: handle.job_slots,
            burst: handle.burst,
            tags: handle.tags.clone(),
            loads: flock
                .get(machine)
                .map(|c| flock.loads_of(c))
                .unwrap_or_default(),
            live: s.live,
            live_jobs: s.live_jobs,
            // A pull machine still pending or failed claims nothing.
            healthy: self.bootstrap_ready(machine),
            pull: true,
            workdir_sets: places_sets,
            flocks: seats(&flock, &s),
            waiting_under_share: Vec::new(),
            // The head never reads a pull machine's room, but an operator's
            // cordon holds its claims like any other machine's placement.
            floor: FloorView {
                cordoned: self.is_cordoned(machine),
                ..FloorView::default()
            },
        };
        let queued = self.store.queued_tasks()?;
        let pinned = |now: bool| {
            queued
                .iter()
                .filter(move |t| t.spec.machine.as_deref() == Some(machine) && t.spec.now == now)
        };
        let loose = queued
            .iter()
            .filter(|t| flock_work && t.spec.machine.is_none());
        let order: Vec<Task> = pinned(true)
            .chain(pinned(false))
            .chain(loose)
            .cloned()
            .collect();
        let default = flock.default_flock();
        let takes = |task: &Task, machine: &str| {
            let theirs = task.flock.as_deref().unwrap_or(default);
            self.settled_on(task, theirs, machine)
                .is_none_or(|r| r.is_ok())
        };
        let mut claimed = Vec::new();
        // The bytes of sets the reply carries so far.
        let mut sets_sent = 0;
        // The tasks taken within `free_slots`: a `--now` one is not.
        let mut in_slots = 0;
        for (i, task) in order.iter().enumerate() {
            if !task.spec.now && in_slots >= free_slots as usize {
                break;
            }
            if task.state != TaskState::Queued {
                continue;
            }
            let target = task.flock.as_deref().unwrap_or(default);
            let settled = self.settled_on(task, target, machine);
            let claim = Claim::of(task);
            let accepts = |_: &str| settled.as_ref().is_none_or(|r| r.is_ok());
            let fits = if task.spec.now {
                now_machine(std::slice::from_ref(&view), target, &task.spec, &accepts).is_some()
            } else {
                let later = &order[i + 1..];
                mark_waiting_under_share(
                    std::slice::from_mut(&mut view),
                    target,
                    later,
                    default,
                    &takes,
                );
                let views = std::slice::from_ref(&view);
                pick_machine_where(views, target, &task.spec, claim, &accepts).is_some()
            };
            if !fits {
                continue;
            }
            let mut on = task.clone();
            if let Some(Ok(spec)) = settled {
                on.spec = spec;
            }
            if on.error.as_deref().is_some_and(is_waiting_note) {
                on.error = None;
            }
            if (on.spec != task.spec || on.error != task.error)
                && let Err(err) = self.store.update_task(&mut on)
            {
                tracing::warn!(task = %task.display_id(), machine, %err, "settle agent");
                continue;
            }
            // What the machine needs to place the task's sets itself, built
            // before the task is taken, so one past the reply's bound stays
            // queued for the next claim.
            let has_sets = on.spec.workdir.is_some() || !on.spec.workdir_bundles.is_empty();
            let built = match &gate {
                Some(gate) if places_sets && has_sets => {
                    let kind = self
                        .agents
                        .read()
                        .recover()
                        .kind(&on.spec.agent)
                        .to_string();
                    let mut spec = on.spec.clone();
                    match workdir_task::for_claim(
                        gate.paths(),
                        &flock,
                        &mut spec,
                        &kind,
                        machine,
                        target,
                    ) {
                        Ok(bytes) => {
                            if !on.spec.now
                                && !claimed.is_empty()
                                && sets_sent + bytes > workdir_task::MAX_CLAIM_SETS
                            {
                                break;
                            }
                            sets_sent += bytes;
                            Some(Ok(spec))
                        }
                        Err(why) => Some(Err(why)),
                    }
                }
                _ => None,
            };
            let Some(mut t) = self.store.claim_task(task.id, machine)? else {
                continue;
            };
            tracing::info!(task = %t.display_id(), machine, "claimed by a pull machine");
            match built {
                Some(Err(why)) => {
                    self.fail_claimed(t, machine, &why);
                    continue;
                }
                Some(Ok(spec)) => {
                    // The head's row keeps the skips, never the sets.
                    let skipped: Vec<Option<String>> = spec
                        .workdir_bundles
                        .iter()
                        .map(|b| b.skipped.clone())
                        .collect();
                    if skipped.iter().any(Option::is_some) {
                        for (b, why) in t.spec.workdir_bundles.iter_mut().zip(skipped) {
                            b.skipped = why;
                        }
                        self.note_skipped(&mut t, machine);
                    }
                    t.spec = spec;
                }
                // Its serve does not place sets. A stored bundle never keeps
                // a task off a machine, so it starts without.
                None if !t.spec.workdir_bundles.is_empty() => {
                    let why = format!(
                        "{machine} is a pull machine whose pastor does not place workdir sets: upgrade pastor there"
                    );
                    for b in &mut t.spec.workdir_bundles {
                        b.skipped = Some(why.clone());
                    }
                    self.note_skipped(&mut t, machine);
                }
                None => {}
            }
            view.take(target, claim);
            in_slots += usize::from(!t.spec.now);
            claimed.push(t);
        }
        self.count_pull(&handle);
        // Every claim: one may have failed a task rather than taken it.
        self.claimed.notify_waiters();
        Ok(claimed)
    }

    /// How long a `task run --now` waits for its pull machine's claim:
    /// `reply_wait`, as a push machine's start may take, but never past
    /// `NOW_CLAIM_WAIT`.
    pub fn now_claim_wait(&self) -> Duration {
        self.reply_wait().min(NOW_CLAIM_WAIT)
    }

    /// Write `t`'s skipped workdir bundles on its row and raise
    /// `workdir.skipped`, as a start that leaves them out does.
    fn note_skipped(&self, t: &mut Task, machine: &str) {
        match self.store.update_task(t) {
            Ok(()) => {
                let detail = crate::machine::workdir_skipped(t);
                self.emit("workdir.skipped", machine, Some(t), detail);
            }
            Err(err) => {
                tracing::warn!(task = %t.display_id(), machine, %err, "note skipped workdir bundles");
            }
        }
    }

    /// Fail task `t`, just claimed by pull machine `machine`, whose sets
    /// could not be built for it: `workdir_failed: <why>`, as a start on a
    /// push machine fails, rather than starting it there without them.
    fn fail_claimed(&self, mut t: Task, machine: &str, why: &str) {
        use crate::bundle::workdir_push::WORKDIR_FAILED;
        t.state = TaskState::Failed;
        t.error = Some(format!("{WORKDIR_FAILED}: {why}"));
        t.finished_at = Some(chrono::Utc::now());
        match self.store.update_task(&mut t) {
            Ok(()) => self.emit("task.failed", machine, Some(&t), None),
            Err(err) => {
                tracing::error!(task = %t.display_id(), machine, %err, "fail a claimed task whose workdir sets cannot be built");
            }
        }
    }

    /// Wait up to `within` for task `id` to leave the queue, as a pull
    /// machine's claim takes or fails it, and answer whether it did. Woken
    /// by each claim, and reading the row again each `NOW_CLAIM_RECHECK`
    /// for a close meanwhile. A row that is gone or cannot be read ends the
    /// wait: there is nothing to wait for.
    pub async fn wait_claimed(&self, id: i64, within: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            // Armed before the read, so a claim between the two still wakes it.
            let woken = self.claimed.notified();
            tokio::pin!(woken);
            woken.as_mut().enable();
            match self.store.get_task(id) {
                Ok(Some(t)) if t.state == TaskState::Queued => {}
                _ => return true,
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return false;
            }
            let until = deadline.min(now + NOW_CLAIM_RECHECK);
            let _ = tokio::time::timeout_at(until, woken).await;
        }
    }

    /// `PullTrust`: the folder trust saved for pull machine `machine`, and
    /// only its own, so its actor answers its tasks' trust prompts as the
    /// head's actor answers another machine's (`Actor::auto_answer`).
    pub fn pull_trust(&self, machine: &str) -> anyhow::Result<Vec<crate::store::TrustedRepo>> {
        self.pull_handle(machine)?;
        Ok(self
            .store
            .trusted_repos()?
            .into_iter()
            .filter(|t| t.machine == machine)
            .collect())
    }

    /// `TaskReport`: what pull machine `machine` saw become of task `id`,
    /// written on the row with the event its own actor would have emitted.
    /// A closed or failed row stays as it is, a stale one stays stale while
    /// its agent works (as `task::next_state` keeps it) unless only the
    /// machine's silence made it stale (`check_pull_lost`), and one `task done`
    /// ended stays done until its pane closes; each answers the row as it
    /// is. An ended one reported `failed` is closed instead, as `task
    /// close` would close it, with no error: its agent is gone, and the
    /// machine forgets the row after that report, so nothing would ever
    /// report it closed and it would hold its seat for good.
    /// Trust keys the machine pressed (`PullReport::trusted`) are
    /// `task.trusted` once per task, after a `task.blocked` the same report
    /// brings and before any other state's event, as the actor emits them.
    /// `checkout` fills a row's own once, the first time a report carries
    /// it: a pull machine dispatches a worktree task itself and never goes
    /// through the head's `dispatch` (`src/dispatch.rs`), which is the only
    /// other place a row's checkout is set.
    pub async fn report(&self, machine: &str, id: i64, report: PullReport) -> anyhow::Result<Task> {
        let PullReport {
            state,
            pane,
            detail,
            limit,
            trusted,
            checkout,
            workdir_skipped,
        } = report;
        let handle = self.pull_handle(machine)?;
        self.heard(&handle);
        // Once per task, as `Actor::auto_answer` claims it, so a report sent
        // again (after a restart of the serve, say) is not a second event.
        let trust = |task: &Task| -> anyhow::Result<()> {
            if let Some(keys) = &trusted
                && self.store.claim_trust_sent(task.id, keys)?
            {
                // The actor claims both for its own tasks: the two tables
                // agree on a pull machine's task too.
                self.store.claim_answered(task.id, "trust", keys)?;
                self.emit(
                    "task.trusted",
                    machine,
                    Some(task),
                    Some(serde_json::json!({ "keys": keys })),
                );
            }
            Ok(())
        };
        // The head owns the limits: one a pull machine read goes in the
        // table as one this head read would, keyed by the machine it ran on.
        if let Some(limit) = &limit
            && let Some(task) = self.store.get_task(id)?
            && task.machine.as_deref() == Some(machine)
        {
            self.note_limit(&task, machine, limit, chrono::Utc::now())?;
        }
        if matches!(
            state,
            TaskState::Queued | TaskState::Paused | TaskState::Waiting
        ) {
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
            let ended = from == TaskState::Done && task.ended;
            let gone = ended && state == TaskState::Failed;
            let state = if gone { TaskState::Closed } else { state };
            let working = matches!(
                state,
                TaskState::Starting | TaskState::Running | TaskState::Blocked
            );
            let new_checkout = checkout.is_some() && task.spec.checkout.is_none();
            // The machine's own start left these out. A bundle the row
            // already says why of keeps that: the claim's reason stands.
            let mut newly_skipped: Vec<(String, String)> = Vec::new();
            // Reports carry no sequence and may arrive out of order, so a
            // failed row takes no later report, and an ended one only its
            // pane closing, which `next_state` also lets through. A row
            // stale only because the machine went silent (`check_pull_lost`)
            // was never seen to stop: its agent at work puts it back.
            let kept = matches!(from, TaskState::Closed | TaskState::Failed)
                || ended && state != TaskState::Closed
                || from == TaskState::Stale && working && !stale_from_loss(&task, machine);
            if kept {
                return Ok(task);
            }
            for skip in &workdir_skipped {
                if let Some(b) = task
                    .spec
                    .workdir_bundles
                    .iter_mut()
                    .find(|b| b.id == skip.id && b.skipped.is_none())
                {
                    let why = clean_report_text(&skip.why);
                    b.skipped = Some(why.clone());
                    newly_skipped.push((b.id.clone(), why));
                }
            }
            if !new_checkout
                && newly_skipped.is_empty()
                && from == state
                && task.error == detail
                && pane
                    .as_ref()
                    .is_none_or(|p| task.pane_id.as_ref() == Some(p))
            {
                trust(&task)?;
                return Ok(task);
            }
            task.state = state;
            if pane.is_some() {
                task.pane_id = pane.clone();
            }
            if new_checkout {
                task.spec.checkout = checkout.clone().map(Box::new);
            }
            // The failure of an agent that had ended its task is no
            // error of the task's: it finished as the agent said.
            if !gone {
                task.error = detail.clone();
            }
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
            // As the actor raises it for a start that left bundles out,
            // before the start's own event.
            if let Some((_, why)) = newly_skipped.first() {
                let ids: Vec<&str> = newly_skipped.iter().map(|(id, _)| id.as_str()).collect();
                self.emit(
                    "workdir.skipped",
                    machine,
                    Some(&task),
                    Some(serde_json::json!({"ids": ids, "why": why})),
                );
            }
            if state != TaskState::Blocked {
                trust(&task)?;
            }
            if from != state {
                let question = (state == TaskState::Blocked)
                    .then(|| detail.as_deref()?.strip_prefix("agent asked: "))
                    .flatten()
                    .map(|q| serde_json::json!({ "question": q }));
                self.emit(&format!("task.{state}"), machine, Some(&task), question);
            }
            if state == TaskState::Blocked {
                trust(&task)?;
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
                    "{} needs a summary: pastor task done --current --summary '<summary>', a text in single quotes (a single quote inside it is written '\\'') whose first line is done, partial, blocked or nothing to do, then up to five short lines",
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
    /// stale, as a task does whose agent pastor lost sight of. A task its
    /// agent ended (`task done`, done and ended here) is closed, its finish
    /// kept: it waited for the machine to close its pane and report that,
    /// which a machine that is gone may never do, and until then it held
    /// its slot and its flock's seat. Tasks pinned to the machine stay
    /// queued. Answers the machines it counted lost now.
    pub async fn check_pull_lost(&self, after: Duration) -> Vec<String> {
        let _pass = self.dispatch_lock.lock().await;
        let now = tokio::time::Instant::now();
        let lost: Vec<String> = self
            .pull_seen
            .lock()
            .recover()
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
            let why = format!("{} for {after:?}", pull_lost_note(name));
            {
                let mut s = handle.status.write().recover();
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
                let state = match t.state {
                    TaskState::Starting | TaskState::Running => TaskState::Stale,
                    TaskState::Done if t.ended => TaskState::Closed,
                    _ => continue,
                };
                t.state = state;
                if state == TaskState::Stale {
                    t.error = Some(why.clone());
                }
                match self.store.update_task(&mut t) {
                    Ok(()) => self.emit(&format!("task.{state}"), name, Some(&t), None),
                    Err(err) => {
                        tracing::warn!(task = %t.display_id(), %err, "mark {state}")
                    }
                }
            }
            self.count_pull(&handle);
        }
        lost
    }

    /// Try to place every queued task, oldest first. The placements are
    /// decided under the dispatch lock (`place_queued`), then sent with the
    /// lock let go: one machine slow to start an agent, or wedged, holds up
    /// neither the other machines nor `task run`, pull claims or a reload.
    /// Each machine gets its sends in order, the machines at once, each
    /// bounded by `reply_wait`.
    ///
    /// Passes still see each other's placements: a task placed and not yet
    /// claimed counts on its machine (`Placed`, in `views`), one claimed
    /// counts from its row, and the next pass skips it until its send ends.
    /// The claim inside the actor (`Store::claim_task`) is the second line
    /// of defence: it makes a double dispatch of one task impossible even
    /// if this were bypassed.
    pub async fn dispatch_queued(&self) {
        let placed = self.place_queued().await;
        let wait = self.reply_wait();
        let mut by_machine: Vec<(String, Vec<Placement>)> = Vec::new();
        for p in placed {
            match by_machine.iter_mut().find(|(m, _)| *m == p.handle.name) {
                Some((_, sends)) => sends.push(p),
                None => by_machine.push((p.handle.name.clone(), vec![p])),
            }
        }
        // A `--now` placement is urgent: send it ahead of this pass's other
        // placements for the same machine, so an earlier queued task's
        // dispatch (up to `reply_wait`) never makes it wait. `sort_by_key`
        // is stable, so it does not reorder within either group. One sent
        // by an earlier pass that the actor is still starting does not hold
        // it up either: it goes on the actor's urgent channel (`Outbound::Now`).
        for (_, sends) in &mut by_machine {
            sends.sort_by_key(|p| !p.task.spec.now);
        }
        let mut sending = tokio::task::JoinSet::new();
        for (_, sends) in by_machine {
            sending.spawn(async move {
                for p in sends {
                    send_placement(p, wait).await;
                }
            });
        }
        while sending.join_next().await.is_some() {}
    }

    /// Lift the queued tasks that have waited their flock's `age_after` a
    /// level (`Store::age_queued`), so a `low` task still runs behind a
    /// steady stream of `normal` and `high` ones (never `critical` ones:
    /// ageing stops at `high`). Each dispatch pass does it first,
    /// under the dispatch lock, so a hand change never races it.
    /// A task in flight has left the queue though its row still says
    /// `queued`, and keeps the level it was placed at.
    fn age_queued(&self, now: chrono::DateTime<chrono::Utc>) {
        let flock = self.flock();
        let defaults = self.defaults.read().recover().clone();
        let after = |t: &Task| {
            let theirs = t.flock.as_deref().unwrap_or(flock.default_flock());
            defaults.resolve_age_after(flock.entry(theirs))
        };
        let in_flight: Vec<i64> = self.in_flight.placed().iter().map(|p| p.task_id).collect();
        match self.store.age_queued(now, after, &in_flight) {
            Ok(aged) => {
                for t in aged {
                    tracing::info!(
                        task = t.id,
                        aged_from = %t.aged_from.unwrap_or(t.priority),
                        to = %t.priority,
                        "queued task aged a level"
                    );
                }
            }
            Err(err) => tracing::error!(%err, "age queued"),
        }
    }

    /// `dispatch_queued`'s decisions, under the dispatch lock: every task
    /// placed is reserved on its machine before the next one is looked at.
    async fn place_queued(&self) -> Vec<Placement> {
        let _pass = self.dispatch_lock.lock().await;
        self.expire_limits();
        let mut placed = Vec::new();
        self.age_queued(chrono::Utc::now());
        let queued = match self.store.queued_tasks() {
            Ok(q) => q,
            Err(err) => {
                tracing::error!(%err, "list queued");
                return placed;
            }
        };
        let flock = self.flock();
        // Would `machine` take `task` as far as its agent goes? For the
        // tasks behind the one being placed (`mark_waiting_under_share`).
        let takes = |task: &Task, machine: &str| {
            let theirs = task.flock.as_deref().unwrap_or(flock.default_flock());
            self.settled_on(task, theirs, machine)
                .is_none_or(|r| r.is_ok())
        };
        // A waiting task goes on once its `waiting_until` has passed and no
        // live limit holds its agent and model on its machine: at its
        // reset, or when its limit is cleared (`clear_limits` brings
        // `waiting_until` forward). The time alone holds it when its limit
        // never reached the table, or the table was read before it did.
        // Before the queue: it has started already, and its checkout and
        // slot are on its machine only.
        match self.store.waiting_tasks() {
            Ok(waiting) => {
                let now = chrono::Utc::now();
                let live = self.live_limits(now);
                for task in &waiting {
                    let Some(machine) = task.pinned_machine() else {
                        continue;
                    };
                    if self.is_in_flight(task.id) || task.waiting_until.is_some_and(|u| u > now) {
                        continue;
                    }
                    let task = match self.wake(task, machine, &live) {
                        Wake::Hold => continue,
                        Wake::Own => task.clone(),
                        Wake::Switch(spec) => match self.switch_model(task, machine, *spec) {
                            Ok(Some(t)) => t,
                            Ok(None) => continue,
                            Err(err) => {
                                tracing::error!(task = %task.display_id(), %err, "move to the next model");
                                continue;
                            }
                        },
                        Wake::Wait { why, held } => {
                            if let Err(err) = self.wait_longer(task, machine, why, &held) {
                                tracing::error!(task = %task.display_id(), %err, "wait for a usage limit");
                            }
                            continue;
                        }
                    };
                    placed.extend(self.resume_paused(&task, &queued, &takes));
                }
            }
            Err(err) => tracing::error!(%err, "list waiting"),
        }
        for (i, task) in queued.iter().enumerate() {
            let later = &queued[i + 1..];
            if self.is_in_flight(task.id) {
                continue;
            }
            if task.state == TaskState::Paused {
                placed.extend(self.resume_paused(task, later, &takes));
                continue;
            }
            let target = task.flock.as_deref().unwrap_or(flock.default_flock());
            let default = flock.default_flock();
            let mut views = self.views();
            mark_waiting_under_share(&mut views, target, later, default, &takes);
            // The agent can depend on the machine: a machine whose agent
            // cannot run the task's model does not take it. A task from
            // before `agent_source` keeps the agent it was queued with.
            // A profile it inherited (from a machine or flock, not its own
            // ask) is pinned onto the ask here, so a re-settle that can no
            // longer resolve it refuses instead of quietly dropping it.
            let settled_on = |machine: &str| self.settled_on(task, target, machine);
            let claim = Claim::of(task);
            let accepts = |m: &str| settled_on(m).is_none_or(|r| r.is_ok());
            // A `--now` task takes its pinned machine past its limits, or
            // nothing: it neither waits for a slot nor pauses another task.
            if task.spec.now {
                let Some(name) = now_machine(&views, target, &task.spec, &accepts) else {
                    continue;
                };
                let Some(handle) = self.get(&name) else {
                    continue;
                };
                if let Some(Ok(spec)) = settled_on(&name) {
                    let mut on = task.clone();
                    on.spec = spec;
                    if on.spec != task.spec
                        && let Err(err) = self.store.update_task(&mut on)
                    {
                        tracing::warn!(task = %task.display_id(), machine = %name, %err, "settle agent");
                        continue;
                    }
                }
                placed.push(self.reserve(task, handle, Outbound::Now));
                continue;
            }
            let mut picked = pick_machine_where(&views, target, &task.spec, claim, &accepts);
            // A critical task with `preempt` that finds no room pauses the
            // newest low Claude task on a machine it would fit once that one
            // is gone, then takes the slot: the pause and the dispatch go
            // to that machine one after the other.
            let mut send = Outbound::Dispatch;
            if picked.is_none()
                && task.pause.preempt
                && task.priority == Priority::Critical
                && let Some((machine, victim)) = self.pausable_for(PausableFor {
                    views: &views,
                    flock: target,
                    task,
                    claim,
                    accepts: &accepts,
                    later,
                    takes: &takes,
                })
            {
                picked = Some(machine);
                send = Outbound::PauseFor(victim);
            }
            let Some(name) = picked else {
                // Say why: no machine of the flock has an agent for its
                // model, or one would take it but for its model.
                let none_has = || {
                    let kind = self
                        .models
                        .read()
                        .recover()
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
                // Or a machine would take it but for its room floor: the
                // first such machine, under its floor or unread.
                let floor_short = || {
                    views
                        .iter()
                        .filter(|v| task.spec.machine.as_ref().is_none_or(|p| *p == v.name))
                        .filter(|v| {
                            v.healthy
                                && v.has_room(claim)
                                && v.flock_may_take(target)
                                && v.suits(&task.spec)
                                && accepts(&v.name)
                        })
                        .find_map(|v| v.floor_held(target))
                };
                // Or a machine would take it but its flock is at its
                // number there, or past its share while another waits.
                let flock_full = || {
                    views
                        .iter()
                        .filter(|v| task.spec.machine.as_ref().is_none_or(|p| *p == v.name))
                        .filter(|v| {
                            v.healthy
                                && (v.has_room(claim)
                                    || v.seat(target).is_some_and(FlockSeat::drained))
                                && v.suits(&task.spec)
                                && settled_on(&v.name).is_none_or(|r| r.is_ok())
                        })
                        .find_map(|v| flock_held(v, target))
                };
                // Or every model it may run there is on an exhausted
                // account, which gets a note of its own.
                let why = none_has()
                    .or_else(floor_short)
                    .map(|why| format!("{WAITING_FOR_MODEL}: {why}"))
                    .or_else(|| {
                        let m = pick_machine(&views, target, &task.spec, claim)?;
                        let refused = settled_on(&m)?.err()?;
                        Some(if refused.code == ACCOUNT_EXHAUSTED {
                            format!("{WAITING_FOR_ACCOUNT}: {}", refused.message)
                        } else {
                            format!("{WAITING_FOR_MODEL}: {refused}")
                        })
                    })
                    .or_else(|| flock_full().map(|why| format!("{WAITING_FOR_MODEL}: {why}")));
                let note = why;
                let stale = task.error.as_deref().is_some_and(is_waiting_note);
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
            if on.error.as_deref().is_some_and(is_waiting_note) {
                on.error = None;
            }
            if (on.spec != task.spec || on.error != task.error)
                && let Err(err) = self.store.update_task(&mut on)
            {
                tracing::warn!(task = %task.display_id(), machine = %name, %err, "settle agent");
                continue;
            }
            placed.push(self.reserve(task, handle, send));
        }
        placed
    }

    /// Is a pass sending `task_id` right now?
    fn is_in_flight(&self, task_id: i64) -> bool {
        self.in_flight.holds(task_id)
    }

    /// `dispatch_queued`, then, if another pass placed `task_id` and is
    /// still sending it, wait for that send to end, at most `reply_wait`
    /// (its reservation itself is not bounded this way, so callers besides
    /// this one still see the task as in flight for as long as the actor
    /// may still claim it), so a reply with the row shows where it went.
    /// Sends for other tasks are not waited for.
    pub async fn dispatch_queued_for(&self, task_id: i64) {
        self.dispatch_queued().await;
        let wait = async {
            loop {
                let sent = self.in_flight.sent.notified();
                tokio::pin!(sent);
                sent.as_mut().enable();
                if !self.is_in_flight(task_id) {
                    return;
                }
                sent.await;
            }
        };
        let _ = tokio::time::timeout(self.reply_wait(), wait).await;
    }

    /// Count `task` on `handle`'s machine until the returned placement's
    /// send has ended.
    fn reserve(&self, task: &Task, handle: MachineHandle, send: Outbound) -> Placement {
        self.in_flight.placed.lock().unwrap().push(Placed {
            task_id: task.id,
            machine: handle.name.clone(),
            flock: task.flock.clone(),
            from_job: task.from_job(),
        });
        Placement {
            task: task.clone(),
            handle,
            send,
            _held: Reservation {
                in_flight: self.in_flight.clone(),
                task_id: task.id,
            },
        }
    }

    /// Where critical task `task` could start by pausing a task: the
    /// machines it may run on (in its flock, healthy, its tags, its pin, one
    /// `accepts`) that have no room for `claim` or `flock` is at its number
    /// there now, but would have both once one pausable task
    /// (`Task::pausable`) is gone, the newest such, the machine with the
    /// fewest live tasks first. Pausing another flock's task frees a slot,
    /// not a seat in `flock`, so it only helps a machine short of slots.
    /// Past its share, `flock` may take the freed slot only while no flock
    /// under its share there has a task in `later` waiting for it, the same
    /// rule dispatch applies, so a pause never frees a slot the task then
    /// may not take. Answers the machine and that task's id.
    fn pausable_for(&self, ask: PausableFor<'_>) -> Option<(String, i64)> {
        let PausableFor {
            views,
            flock,
            task,
            claim,
            accepts,
            later,
            takes,
        } = ask;
        let agents = self.agents.read().recover().clone();
        let default = self.flock().default_flock().to_string();
        let now = chrono::Utc::now();
        views
            .iter()
            .filter(|m| {
                m.in_flock(flock)
                    && m.healthy
                    && !(m.has_room(claim) && m.flock_may_take(flock))
                    // A pause frees a slot, not disk or memory.
                    && m.floor_held(flock).is_none()
                    && task.spec.machine.as_ref().is_none_or(|p| *p == m.name)
                    && m.suits(&task.spec)
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
                        // The pause can bring `flock` from its max to
                        // past its share, where waiters matter again.
                        mark_waiting_under_share(
                            std::slice::from_mut(&mut after),
                            flock,
                            later,
                            &default,
                            takes,
                        );
                        after.has_room(claim) && after.flock_may_take(flock)
                    })
                    .max_by_key(|t| t.id)?;
                Some((m.live, m.name.clone(), victim.id))
            })
            .min_by_key(|(live, _, _)| *live)
            .map(|(_, name, id)| (name, id))
    }

    /// Resume paused or waiting task `task` on the machine it was paused or
    /// waiting on, once that machine is healthy, still in the flock and has
    /// room for it, its own flock under its number there. Past its share, it also waits while a
    /// flock under its share there has a task in `later` for that slot, as
    /// a queued task would (`MachineView::flock_may_take`). It is not
    /// settled again: it goes back to the agent and session it had.
    fn resume_paused(
        &self,
        task: &Task,
        later: &[Task],
        takes: &(dyn Fn(&Task, &str) -> bool + Sync),
    ) -> Option<Placement> {
        let machine = task.pinned_machine()?;
        let flock = self.flock();
        let target = task.flock.as_deref().unwrap_or(flock.default_flock());
        let mut views = self.views();
        mark_waiting_under_share(&mut views, target, later, flock.default_flock(), takes);
        let fits = views.iter().find(|m| m.name == machine).is_some_and(|m| {
            m.healthy
                && !m.floor.cordoned
                && m.has_room(Claim::of(task))
                && m.flock_may_take(target)
        });
        if !fits || !self.in_flock(machine) {
            return None;
        }
        let handle = self.get(machine)?;
        Some(self.reserve(task, handle, Outbound::Resume))
    }
}

/// Send what a dispatch pass decided for one task, then move on after at
/// most `wait`: one machine slow to answer holds up neither the other
/// machines nor the caller. The command already sent to the actor's channel
/// cannot be un-sent, so its reservation is not bounded by `wait` the way
/// the wait for an answer is: `run_placement` keeps it, in a task of its
/// own, until the actor genuinely answers (or the request fails outright),
/// however long that takes. Otherwise a priority change or a queue move
/// could see the task as free the moment `wait` passes, while the actor
/// still has this exact placement to carry out.
async fn send_placement(p: Placement, wait: Duration) {
    let disp = p.task.display_id();
    let machine = p.handle.name.clone();
    let done = tokio::spawn(run_placement(p));
    if tokio::time::timeout(wait, done).await.is_err() {
        tracing::warn!(task = %disp, machine = %machine, ?wait, "no answer within reply_wait; still in flight until it answers");
    }
}

/// The sends `send_placement` decided for one task, unbounded: `wait` only
/// bounds how long `send_placement` waits on this before moving on, never
/// how long the placement itself, or the reservation it holds, may take.
async fn run_placement(p: Placement) {
    let Placement {
        task, handle, send, ..
    } = &p;
    let machine = &handle.name;
    let (res, what) = match *send {
        Outbound::Resume => (handle.resume(task.id).await, "resume"),
        Outbound::PauseFor(victim) => {
            match handle.pause(victim, task.id).await {
                Ok(_) => {
                    tracing::info!(task = %task.display_id(), paused = %Task::agent_name_for(victim), machine = %machine, "paused a low task");
                }
                Err(err) => {
                    tracing::warn!(task = %task.display_id(), victim = %Task::agent_name_for(victim), machine = %machine, %err, "pause failed");
                    return;
                }
            }
            (handle.dispatch(task.id).await, "dispatch")
        }
        Outbound::Dispatch => (handle.dispatch(task.id).await, "dispatch"),
        Outbound::Now => (handle.dispatch_now(task.id).await, "dispatch"),
    };
    match res {
        Ok(t) if what == "resume" => {
            tracing::info!(task = %t.display_id(), machine = %machine, state = %t.state, "resumed")
        }
        Ok(t) => {
            tracing::info!(task = %t.display_id(), machine = %machine, state = %t.state, "dispatched")
        }
        Err(err) => {
            tracing::warn!(task = %task.display_id(), machine = %machine, err = %format!("{err:#}"), "{what} failed")
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

use crate::queue::{WAITING_FOR_ACCOUNT, WAITING_FOR_MODEL, asked_by, is_waiting_note};

/// The code `task retry` refuses a waiting task with.
pub const TASK_WAITING: &str = "task_waiting";

/// How long `Fleet::live_limits` trusts the table as last read.
const LIMITS_FRESH: Duration = Duration::from_secs(1);

/// What a waiting task does once its time has come (`Fleet::wake`).
enum Wake {
    /// Its models are still exhausted: it keeps waiting.
    Hold,
    /// Its own model is free: it resumes on it.
    Own,
    /// It goes on under another model, its spec settled for it.
    Switch(Box<crate::task::DispatchSpec>),
    /// It was to move on, and no other model of its list is free: it
    /// waits for the earliest of `held` (`all_exhausted`); or none runs on
    /// its machine: it waits on its own model, `held` its limits
    /// (`no_fallback`).
    Wait {
        why: &'static str,
        held: Vec<AccountLimit>,
    },
}

/// The code of a task every model of which is on an exhausted account
/// where it would run.
pub const ACCOUNT_EXHAUSTED: &str = "account_exhausted";

/// The refusal for a task that `held` stops, as its waiting note says it.
fn exhausted(held: &[AccountLimit], now: chrono::DateTime<chrono::Utc>) -> AgentRefusal {
    AgentRefusal {
        code: ACCOUNT_EXHAUSTED,
        message: held
            .iter()
            .map(|l| l.note(now))
            .collect::<Vec<_>>()
            .join("; "),
    }
}

/// An error from code the CLI shares with the head, as a reply: its
/// `CliError` code when it has one, else `runtime_error`.
/// `cli_error`, with a flock.toml edit's own code.
fn join_error(err: anyhow::Error) -> IpcResponse {
    match err.downcast_ref::<EditError>() {
        Some(e) => IpcResponse::error(e.code(), e),
        None => cli_error(err),
    }
}

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
            "{task} is an orchestrator, and an orchestrator may only run, retry, send to and close tasks, enable and disable jobs and keep its note besides reading"
        ),
    }
}

/// What the head says when it refuses orchestrator `name`'s pre or post
/// script a change outside the role's table.
pub fn script_refusal(name: &str) -> String {
    format!(
        "this is orchestrator {name}'s script, and an orchestrator may only run, retry, send to and close tasks, enable and disable jobs and keep its note besides reading"
    )
}

pub fn agent_refusal(task: &str) -> String {
    format!(
        "{task} is an agent pastor started, and agents may not change the fleet (run, send to, attach to, retry, reprioritize, move in the queue, close or prune tasks, tick (dry runs too), run or reload jobs, install, link, uninstall or unlink connectors, edit machines, flocks, jobs or pastor.toml, serve or set up a head, open herdr's UI; `pastor task done` may end only its own task)"
    )
}

/// Text a machine reported, for the store, the events and `machine
/// describe`: control characters out, and no more than 400 characters.
fn clean_report_text(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(400)
        .collect::<String>()
        .trim()
        .to_string()
}

/// The code of `--now` onto a cordoned machine, which no flag runs past.
pub const MACHINE_CORDONED: &str = "machine_cordoned";

pub struct Daemon {
    paths: Paths,
    store: Arc<Store>,
    fleet: Arc<Fleet>,
    scheduler: SchedulerHandle,
    events: broadcast::Sender<PastorEvent>,
    orchestrators: Arc<crate::orchestrator::Runner>,
    /// The sign-ins `pastor login` started and the head watches.
    logins: Arc<crate::login::Logins>,
    /// How the head reaches a push machine to place a workdir set there.
    workdir_reach: crate::bundle::keep::Reach,
}

/// The longest IPC request line, newline excluded. The largest real one is a
/// `workdir place`, whose set is at most `workdir::MAX_SET` (4 MiB) and
/// grows as JSON, where each newline and quote takes two bytes.
const MAX_IPC_REQUEST: usize = 9 * 1024 * 1024;

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
    // A path past `sun_path` fails the ping's connect, which would read as an
    // unresponsive daemon; say what is wrong instead.
    crate::ssh::check_socket_path(socket, crate::ssh::SHORTER_STATE_DIR)?;
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
        HeadPing::Denied => Err(crate::cli::socket_denied(socket)),
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

impl crate::login::Head for Fleet {
    fn clear_signed_out(&self, key: &str, by: &str) -> anyhow::Result<Vec<AccountLimit>> {
        Fleet::clear_signed_out(self, key, by)
    }
    fn emit(&self, kind: &str, machine: &str, detail: serde_json::Value) {
        Fleet::emit(self, kind, machine, None, Some(detail));
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
        let paths_for_reach = paths.clone();
        let store = Arc::new(Store::open(&paths.db_file())?);
        // Before any actor or the scheduler reads a row, so none of them
        // sees a task from before flocks without one.
        store.adopt_default_flock(flock.default_flock())?;
        let (events, log_rx) = broadcast::channel(1024);
        let connect = connect.unwrap_or_else(|| endpoint_factory(paths.clone()));
        let fleet = Arc::new(Fleet::managed(store.clone(), events.clone(), connect));
        // Before the actors, which take their manifests from the fleet at
        // spawn: a user's manifest would else wait for some other edit.
        match crate::harness_manifest::Harnesses::load(&paths) {
            Ok(h) => fleet.set_harnesses(Arc::new(h)),
            Err(err) => {
                tracing::error!(%err, "the harness manifests do not load; the built-in ones are used")
            }
        }
        // Before the actors, which each take its gate from the fleet.
        let keeper = crate::bundle::keep::Keeper::new(
            paths.clone(),
            Arc::downgrade(&fleet),
            crate::bundle::keep::reach(paths.clone()),
        );
        fleet.set_keeper(keeper.clone());
        fleet.set_workdirs(crate::bundle::workdir_task::HeadGate::new(
            paths.clone(),
            Arc::downgrade(&(fleet.clone() as Arc<dyn crate::bundle::workdir_task::HeadSide>)),
            crate::bundle::keep::reach(paths.clone()),
        ));
        fleet.apply_flock(&flock, &machine_settings(&config)).await;
        keeper.spawn();
        crate::bundle::workdir_keep::Keeper::new(
            paths.clone(),
            Arc::downgrade(&fleet),
            crate::bundle::keep::reach(paths.clone()),
        )
        .spawn();
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
            logins: Default::default(),
            workdir_reach: crate::bundle::keep::reach(paths_for_reach),
        })
    }

    /// `workdir place|remove|check|list` through the head
    /// (`bundle::workdir_push`), with pastor.toml as it is on disk, as
    /// `FileGet` hands it to `profile render`, and the flock last applied.
    async fn workdir(&self, req: IpcRequest) -> IpcResponse {
        if let IpcRequest::WorkdirList { machine } = &req {
            return match crate::bundle::workdir_push::list(&self.paths, machine.as_deref()) {
                Ok(records) => IpcResponse::Workdirs(records),
                Err(err) => cli_error(err),
            };
        }
        let config = match PastorConfig::load(&self.paths.config_file()) {
            Ok(c) => c,
            Err(err) => return IpcResponse::error("config_error", format!("{err:#}")),
        };
        let flock = self.fleet.flock();
        let reach = self.workdir_reach.clone();
        let fleet = self.fleet.clone();
        let emit = move |kind: &'static str, machine: &str, detail: serde_json::Value| {
            fleet.emit_machine(kind, machine, Some(detail));
        };
        let head = crate::bundle::workdir_push::Head {
            paths: &self.paths,
            config: &config,
            flock: &flock,
            reach: &reach,
            emit: &emit,
        };
        let done = match req {
            IpcRequest::WorkdirPlace {
                machine,
                dir,
                set,
                bundle,
                kind,
                flock,
                dry_run,
            } => match (set, bundle, kind) {
                (Some(set), None, None) => head
                    .place(&machine, &dir, &set, dry_run)
                    .await
                    .map(|p| vec![p]),
                (None, Some(bundle), Some(kind)) => head
                    .place_bundle(&machine, &dir, &bundle, &kind, flock.as_deref(), dry_run)
                    .await
                    .map(|p| p.into_iter().collect()),
                _ => Err(crate::cli::CliError::err(
                    crate::bundle::workdir::WORKDIR_INVALID,
                    "a place names a set, or a bundle and the kind of agent",
                )),
            },
            IpcRequest::WorkdirRemove {
                machine,
                dir,
                id,
                dry_run,
            } => head
                .remove(&machine, &dir, id.as_deref(), dry_run)
                .await
                .map(|p| vec![p]),
            IpcRequest::WorkdirCheck { machine, dir, id } => {
                head.check(&machine, &dir, id.as_deref()).await
            }
            IpcRequest::WorkdirReport { machine, reports } => {
                return match head.report(&machine, &reports) {
                    Ok(()) => IpcResponse::Text(format!("{machine}: workdir report recorded")),
                    Err(err) => cli_error(err),
                };
            }
            other => unreachable!("not a workdir request: {other:?}"),
        };
        match done {
            Ok(placed) => IpcResponse::Workdir(placed),
            Err(err) => cli_error(err),
        }
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
    pub fn logins(&self) -> Arc<crate::login::Logins> {
        self.logins.clone()
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
        crate::ssh::check_socket_path(socket, crate::ssh::SHORTER_STATE_DIR)?;
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
    /// (`ipc::TASK_ENV`): such a caller may read but not change the fleet,
    /// save to end its own task, and an orchestrator
    /// (`TaskRole::Orchestrator`) may make the changes
    /// `IpcRequest::orchestrator_may` lists. No caller in a task may make an
    /// orchestrator: only a person does.
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
        // `task run --now` runs past a machine's limits on a person's say.
        if matches!(req, IpcRequest::Run { now: true, .. }) {
            let who = match (&caller.task, &caller.orchestrator) {
                (Some(task), _) => Some(format!("{task} is a task")),
                (None, Some(o)) => Some(format!("orchestrator {o}'s script is not a person")),
                (None, None) => None,
            };
            if let Some(who) = who {
                return IpcResponse::error(
                    "now_refused",
                    format!("{who}, and only a person may run a task with --now"),
                );
            }
        }
        if let Some(task) = caller.task.as_deref() {
            if let Some(why) = self.makes_orchestrator(&req) {
                return IpcResponse::error("role_refused", format!("{task} is a task, and {why}"));
            }
            if req.changes_fleet() && !req.ends_own_task(task) {
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
                if !req.orchestrator_may() {
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
        // The catalogs the edited file is checked against: a machine's
        // `provisioner` and the `bundles` it gets must name ones in them.
        let catalogs = crate::provision::Catalog::load(&self.paths)
            .and_then(|p| Ok((p, crate::bundle::Catalog::load(&self.paths)?)));
        let (provisioners, bundles) = match catalogs {
            Ok(c) => c,
            Err(err) => return IpcResponse::error("runtime_error", format!("{err:#}")),
        };
        let done = self
            .fleet
            .edit_flock_file(&self.paths.flock_file(), &provisioners, &bundles, edit)
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

    /// `machine remove`: the machine out of flock.toml, then what the head
    /// kept for it (its marked authorized_keys lines, its known_hosts entry,
    /// its bootstrap row). With `dry_run` only the list of what would go.
    async fn machine_remove(&self, name: &str, dry_run: bool) -> IpcResponse {
        let host = Flock::load(&self.paths.flock_file())
            .ok()
            .and_then(|f| f.get(name).and_then(|m| m.ssh.clone()))
            .and_then(|t| crate::leave::host_of(&t).map(str::to_string));
        let cleanup = || {
            crate::leave::machine_cleanup(
                &self.paths,
                Some(&self.store),
                name,
                host.as_deref(),
                dry_run,
            )
        };
        if dry_run {
            return match Flock::load(&self.paths.flock_file()) {
                Ok(f) if f.get(name).is_some() => match cleanup() {
                    Ok(said) => IpcResponse::Text(
                        std::iter::once(format!("would remove {name} from flock.toml"))
                            .chain(said)
                            .collect::<Vec<_>>()
                            .join("; "),
                    ),
                    Err(err) => IpcResponse::error("runtime_error", format!("{err:#}")),
                },
                Ok(_) => IpcResponse::error("unknown_machine", format!("no machine {name}")),
                Err(err) => IpcResponse::error("runtime_error", format!("{err:#}")),
            };
        }
        let done = self
            .edit_flock_file("; ", |file| crate::fleet_edit::remove_machine(file, name))
            .await;
        let IpcResponse::Text(text) = done else {
            return done;
        };
        match cleanup() {
            Ok(said) if said.is_empty() => IpcResponse::Text(text),
            Ok(said) => IpcResponse::Text(format!("{text}; {}", said.join("; "))),
            Err(err) => {
                IpcResponse::Text(format!("{text}; the clean-up after it failed ({err:#})"))
            }
        }
    }

    /// `TaskSweep`: ask each connected machine (or `machine`) for the
    /// worktrees pastor kept there for tasks that ended, removing them when
    /// `yes`. A machine that cannot be asked, or a pull machine (which the
    /// head never connects to; it sweeps itself), is named with why and the
    /// others go on. A sweep takes many calls, so it is waited for longer
    /// than a single request.
    async fn sweep(&self, machine: Option<&str>, yes: bool) -> IpcResponse {
        let names: Vec<String> = match machine {
            Some(m) => {
                if self.fleet.get(m).is_none() {
                    return IpcResponse::error("unknown_machine", format!("no machine {m}"));
                }
                vec![m.to_string()]
            }
            None => self.fleet.statuses().into_iter().map(|s| s.name).collect(),
        };
        let mut entries = Vec::new();
        let mut failed = Vec::new();
        for name in names {
            let Some(handle) = self.fleet.get(&name) else {
                continue;
            };
            if self.fleet.is_pull(&name) {
                if machine.is_some() {
                    failed.push((
                        name,
                        "a pull machine is not swept from the head".to_string(),
                    ));
                }
                continue;
            }
            match within(&name, SWEEP_WAIT, handle.sweep(yes)).await {
                Ok(mut found) => entries.append(&mut found),
                Err(err) => failed.push((name, format!("{err:#}"))),
            }
        }
        IpcResponse::Swept { entries, failed }
    }

    /// `MachineBootstrap`: `bootstrap::push` for `name`, then a reload so a
    /// machine just added is connected to, and what waited for it placed. A
    /// push takes minutes (a download, copies, the machine's own steps), so it
    /// is waited for as long as `PUSH_WAIT`.
    async fn bootstrap_machine(
        &self,
        name: &str,
        machine: Option<crate::config::flock::MachineConfig>,
        host_key: Option<String>,
    ) -> IpcResponse {
        let config = match PastorConfig::load(&self.paths.config_file()) {
            Ok(c) => c,
            Err(err) => return IpcResponse::error("runtime_error", format!("{err:#}")),
        };
        let Some(head) = config.head_address.clone().filter(|h| !h.trim().is_empty()) else {
            return IpcResponse::error(
                "no_head_address",
                "set head_address in pastor.toml: the machine's pastor reaches the head by it",
            );
        };
        let entry = self.fleet.flock().get(name).cloned();
        let Some(login) = machine
            .as_ref()
            .or(entry.as_ref())
            .and_then(|m| m.ssh.clone())
        else {
            return IpcResponse::error(
                "usage_error",
                format!("{name} is not reached over ssh, so the head cannot bootstrap it"),
            );
        };
        let head_key = std::fs::read_to_string(format!("{}.pub", self.paths.head_key().display()))
            .ok()
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty());
        let fleet_versions: Vec<String> = self
            .fleet
            .statuses()
            .into_iter()
            .filter_map(|s| s.herdr_version)
            .collect();
        let opts = crate::bootstrap::push::PushOpts {
            host_key,
            machine,
            head,
            head_key,
            artifacts: config.artifacts.clone(),
            fleet_versions,
            authorized_keys: crate::authkeys::default_file(),
            runner: Arc::new(crate::bootstrap::push::SystemPushRunner),
            fetch: Arc::new(crate::artifacts::CurlFetch),
        };
        let pushed = within(
            name,
            PUSH_WAIT,
            crate::bootstrap::push::push(&self.paths, &self.store, &self.fleet, name, &login, opts),
        )
        .await;
        // The machine is in the flock by now, if it is going to be.
        let _ = self.scheduler.reload().await;
        self.fleet.dispatch_queued().await;
        match pushed {
            Ok(outcome) => IpcResponse::Bootstrapped(outcome),
            Err(err) => cli_error(err),
        }
    }

    /// `ArtifactLocate`: where the cache holds `name` at `version` for
    /// `target`. Only what is cached already, whose file still hashes to its
    /// recorded sum, and only a name that is one plain path part each: the
    /// asker is a shepherd, and cannot make the head download or read
    /// outside the cache.
    fn locate_artifact(&self, name: &str, version: &str, target: &str) -> IpcResponse {
        let a = match crate::artifacts::Artifact::new(name, version, target) {
            Ok(a) => a,
            Err(err) => return IpcResponse::error(err.code(), err),
        };
        let Some((path, sha256)) = crate::artifacts::cached(&self.paths, &a) else {
            return IpcResponse::error(
                "artifact_not_cached",
                format!("the head's cache does not hold {name} {version} for {target}"),
            );
        };
        match std::fs::metadata(&path) {
            Ok(m) if m.is_file() => IpcResponse::Artifact {
                path: path.to_string_lossy().into_owned(),
                sha256,
                size: m.len(),
            },
            _ => IpcResponse::error(
                "artifact_not_cached",
                format!("the head's cache does not hold {name} {version} for {target}"),
            ),
        }
    }

    /// `ShepherdCheck`: whether a shepherd bridge started as `--gen
    /// generation` still speaks for `machine`. OpenSSH fixes a connection's
    /// forced command when it authenticates and never reads
    /// `authorized_keys` again for it, so this check, before each request,
    /// is what cuts off a key whose line a removal or a new enrollment moved
    /// past.
    fn shepherd_check(&self, machine: &str, generation: i64) -> IpcResponse {
        let revoked = |why: String| IpcResponse::error(crate::bridge::MACHINE_REVOKED, why);
        if self.fleet.flock().get(machine).is_none() {
            return revoked(format!(
                "{machine} is not in the head's flock.toml: it was removed"
            ));
        }
        match self.store.machine_state(machine) {
            Ok(Some(state)) if state.generation == generation => {
                IpcResponse::Text("current".into())
            }
            Ok(Some(state)) => revoked(format!(
                "this key's line is from enrollment {generation} of {machine}, which is now at {}",
                state.generation
            )),
            Ok(None) => revoked(format!(
                "{machine} has no enrollment on the head; write its key line again with `pastor machine authorized-key {machine} --shepherd`"
            )),
            Err(err) => IpcResponse::error("store_error", err),
        }
    }

    /// `JoinCreate`: a token for the head at `head_address` (else
    /// `user@hostname`), into a flock that exists, or taking over a name
    /// flock.toml has.
    fn join_create(
        &self,
        flock: Option<String>,
        tags: Vec<String>,
        replace: Option<String>,
        ttl_secs: u64,
    ) -> IpcResponse {
        match crate::join::HeadFiles::for_head(&self.paths) {
            Ok(files) => self.join_create_with(
                &crate::setup::SystemRunner,
                &files,
                crate::join::Ask {
                    flock,
                    tags,
                    replace,
                    ttl: Duration::from_secs(ttl_secs),
                },
                chrono::Utc::now(),
            ),
            Err(err) => cli_error(err),
        }
    }

    /// `join_create` with `ssh-keygen` and the head's files as seams.
    fn join_create_with(
        &self,
        keygen: &dyn crate::setup::Runner,
        files: &crate::join::HeadFiles,
        ask: crate::join::Ask,
        now: chrono::DateTime<chrono::Utc>,
    ) -> IpcResponse {
        let flock = self.fleet.flock();
        if let Some(f) = &ask.flock
            && !flock.has_flock(f)
        {
            return IpcResponse::error("unknown_flock", format!("flock {f} does not exist"));
        }
        if let Some(name) = &ask.replace
            && flock.get(name).is_none()
        {
            return IpcResponse::error(
                "unknown_machine",
                format!("no machine {name} in flock.toml to replace"),
            );
        }
        let head = PastorConfig::load(&self.paths.config_file())
            .map(|c| crate::join::head_destination(&c));
        let made = head.and_then(|head| {
            crate::join::create_with(keygen, files, &self.store, &head, &ask, now)
        });
        match made {
            Ok(made) => {
                tracing::info!(id = made.row.id, "join token made");
                IpcResponse::JoinCreated {
                    id: made.row.id,
                    token: made.token.encode(),
                    expires_at: made.row.expires_at,
                }
            }
            Err(err) => cli_error(err),
        }
    }

    /// The herdr a machine that just joined should run: `[artifacts]
    /// herdr`, else the one most machines run, else pastor's default.
    fn join_herdr_version(&self) -> String {
        let config = PastorConfig::load(&self.paths.config_file()).unwrap_or_default();
        let fleet: Vec<String> = self
            .fleet
            .statuses()
            .into_iter()
            .filter_map(|s| s.herdr_version)
            .collect();
        crate::artifacts::herdr_version(&config.artifacts, &fleet)
    }

    /// `Join`: redeem token `id` for pull machine `name`. The name and the
    /// keys are checked before the token is spent, so a malformed try
    /// leaves it good. Spending it is one store statement, so of two joins
    /// with one token one goes through; from then on its line goes,
    /// whatever else fails. A name flock.toml has is taken only by a token
    /// made to replace it, and such a token joins as that name only. The
    /// machine is added under the dispatch lock, then enrolled one
    /// generation past any record it had, which cuts off a shepherd bridge
    /// of the machine it replaces, then given its shepherd and agent lines.
    async fn join_with(
        &self,
        files: &crate::join::HeadFiles,
        id: &str,
        name: &str,
        machine_key: &str,
        agent_key: &str,
        host_key: &str,
    ) -> anyhow::Result<()> {
        use crate::authkeys::{self, Marker};
        use crate::cli::CliError;
        let now = chrono::Utc::now();
        // Before anything is spent or written: the name goes into forced
        // commands, which a shell runs.
        if !authkeys::valid_name(name) {
            return Err(CliError::err(
                "invalid_machine",
                format!(
                    "{name:?} cannot be a machine name: use letters, digits, '.', '_' and '-', starting with a letter or digit"
                ),
            ));
        }
        let as_machine = Marker::Machine(name.into());
        let as_agent = Marker::Agent(name.into());
        let agent_args = format!("bridge --agent --machine {name}");
        // Each makes the line it will write, so a key or a pastor path that
        // cannot go in one refuses the join here.
        let (_, machine_blob) = authkeys::check_public_key(machine_key)?;
        authkeys::line(&files.pastor, &agent_args, agent_key, &as_agent)?;
        let (_, agent_blob) = authkeys::check_public_key(agent_key)?;
        if machine_blob == agent_blob {
            return Err(CliError::err(
                "invalid_key",
                "the machine key and the agent key must differ: sshd only ever matches the first line a key is on",
            ));
        }
        let host_key = host_key.trim();
        authkeys::check_public_key(host_key).map_err(|_| {
            CliError::err("invalid_key", "the host key must be one public key line")
        })?;
        let row = crate::join::spend(&self.store, id, now)?;
        // Spent: the line goes, whatever the outcome.
        let drop_line = || {
            if let Err(err) = authkeys::remove(&files.authorized_keys, &Marker::Join(id.into())) {
                tracing::warn!(id, err = %format!("{err:#}"), "remove a spent join token's line; sshd's expiry and the sweep take it");
            }
        };
        let refused = |err: anyhow::Error| {
            drop_line();
            err
        };
        if let Some(only) = row.replace.as_deref()
            && only != name
        {
            return Err(refused(CliError::err(
                crate::join::NAME_TAKEN,
                format!("join token {id} was made to replace {only}, and joins as {only} only"),
            )));
        }
        let (provisioners, bundles) = crate::provision::Catalog::load(&self.paths)
            .and_then(|p| Ok((p, crate::bundle::Catalog::load(&self.paths)?)))
            .map_err(refused)?;
        let replaced = self
            .fleet
            .edit_flock_file(&self.paths.flock_file(), &provisioners, &bundles, |file| {
                let mut doc = crate::config::flock::FlockDoc::open(file)?;
                let exists = doc
                    .flock()
                    .map_err(anyhow::Error::msg)?
                    .get(name)
                    .is_some();
                if exists && row.replace.as_deref() != Some(name) {
                    return Err(CliError::err(
                        crate::join::NAME_TAKEN,
                        format!(
                            "machine {name} is already in flock.toml; only a token made with --replace {name} may take it over"
                        ),
                    ));
                }
                let mut m: MachineConfig =
                    serde_json::from_value(serde_json::json!({ "name": name, "pull": true }))?;
                m.flock = row.flock.clone();
                m.tags = row.tags.clone();
                if exists {
                    doc.remove_machine(name)?;
                }
                doc.add_machine(&m)?;
                doc.save(file)?;
                Ok(exists)
            })
            .await
            .map_err(refused)?;
        // In flock.toml with the token spent: from here a failure is
        // reported with what to run, not undone.
        let after = |what: &str, err: anyhow::Error| {
            drop_line();
            CliError::err(
                crate::join::FAILED,
                format!(
                    "machine {name} is in flock.toml and join token {id} is spent, but {what} failed ({err:#}); make a new token with `pastor join create --replace {name}` and join the machine again with it"
                ),
            )
        };
        // Past any generation the name had, so a shepherd bridge of the
        // machine this one replaces is refused at its next request.
        let generation = self
            .store
            .machine_state(name)
            .map_err(|e| after("reading its enrollment", e))?
            .map_or(1, |s| s.generation + 1);
        self.store
            .put_machine_state(&crate::store::MachineState {
                machine: name.into(),
                bootstrap: crate::store::BootstrapState::Pending,
                isolation: None,
                failed_step: None,
                why: None,
                host_key: Some(host_key.into()),
                herdr_version: None,
                pastor_version: None,
                generation,
                updated_at: now,
            })
            .map_err(|e| after("saving its enrollment", e))?;
        authkeys::upsert(
            &files.authorized_keys,
            &as_machine,
            &files.pastor,
            &format!("bridge --shepherd --machine {name} --gen {generation}"),
            machine_key,
        )
        .map_err(|e| after("writing its shepherd line to authorized_keys", e))?;
        authkeys::upsert(
            &files.authorized_keys,
            &as_agent,
            &files.pastor,
            &agent_args,
            agent_key,
        )
        .map_err(|e| after("writing its agent line to authorized_keys", e))?;
        drop_line();
        tracing::info!(machine = name, id, generation, "machine joined");
        let _ = self.events.send(PastorEvent {
            kind: "machine.joined".into(),
            task_id: None,
            machine: Some(name.into()),
            job: None,
            detail: Some(serde_json::json!({
                "id": id,
                "flock": row.flock,
                "tags": row.tags,
                "replaced": replaced,
                "generation": generation,
            })),
            summary: None,
        });
        if let Err(err) = self.scheduler.reload().await {
            tracing::warn!(machine = name, %err, "reload after a join; the next pass picks it up");
        }
        Ok(())
    }

    /// Close `t`, a `--now` task read as still queued, and say it could not
    /// start (`why`). A claim that took the row since that read wins: the
    /// answer is the task, starting where it was claimed, not a close that
    /// did not happen.
    fn close_unstarted_now(&self, t: Task, why: &str) -> IpcResponse {
        match self.store.close_queued(t.id) {
            Ok(Some(closed)) => {
                let _ = self.events.send(PastorEvent {
                    detail: None,
                    kind: "task.closed".into(),
                    task_id: Some(closed.id),
                    machine: None,
                    job: Some(closed.job.clone()),
                    summary: None,
                });
            }
            // Another close says so itself; a claim's start is the answer.
            Ok(None) => {
                if let Ok(Some(now)) = self.store.get_task(t.id)
                    && now.state.is_open()
                    && now.state != TaskState::Queued
                {
                    return IpcResponse::Task(now);
                }
            }
            Err(err) => {
                tracing::warn!(task = %t.display_id(), %err, "close a --now task");
            }
        }
        IpcResponse::error(
            "now_not_started",
            format!(
                "{} could not start at once ({why}), and is closed",
                t.display_id()
            ),
        )
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
                mut spec,
                flock,
                agent,
                priority,
                role,
                description,
                preempt,
                summary,
                now,
            } => {
                // From the flag alone: a spec that says so does not skip
                // the queue.
                spec.now = now;
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
                    .queue_run_as(RunAsk {
                        prompt,
                        spec,
                        flock: flock.as_deref(),
                        ask: agent.as_ref(),
                        priority,
                        role,
                        description: crate::config::clean_description(description.as_deref()),
                        preempt,
                        summary,
                    })
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
                self.fleet.dispatch_queued_for(task.id).await;
                // A pull machine takes a `--now` task on its next claim, not
                // from a pass: the run waits for that claim, as long as a
                // push machine's start may take.
                let pull = task
                    .spec
                    .machine
                    .as_deref()
                    .filter(|m| task.spec.now && self.fleet.is_pull(m));
                let wait = self.fleet.now_claim_wait();
                if pull.is_some() {
                    self.fleet.wait_claimed(task.id, wait).await;
                }
                match self.store.get_task(task.id) {
                    // A `--now` task never waits in the queue: one its
                    // machine did not take (it went away since the check,
                    // or its agent cannot run the model) is closed.
                    Ok(Some(t)) if t.spec.now && t.state == TaskState::Queued => {
                        let why = t.error.clone().unwrap_or_else(|| match pull {
                            Some(m) => format!("{m} did not claim it within {wait:?}"),
                            None => format!(
                                "{} did not take it",
                                t.spec.machine.as_deref().unwrap_or("its machine")
                            ),
                        });
                        self.close_unstarted_now(t, &why)
                    }
                    // A pull machine's claim that failed it (its sets could
                    // not be built) started nothing either.
                    Ok(Some(t)) if pull.is_some() && t.spec.now && t.state == TaskState::Failed => {
                        IpcResponse::error(
                            "now_not_started",
                            format!(
                                "{} could not start at once ({}), and failed",
                                t.display_id(),
                                t.error.as_deref().unwrap_or("its claim failed it")
                            ),
                        )
                    }
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
                    Err(err @ (PriorityError::NotQueued { .. } | PriorityError::InFlight(_))) => {
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
                Err(err @ (MoveError::NotQueued { .. } | MoveError::InFlight(_))) => {
                    IpcResponse::error("not_queued", err)
                }
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
                match self
                    .fleet
                    .bounded(&handle.name, handle.read(id, lines))
                    .await
                {
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
            IpcRequest::MachineRemove { name, dry_run } => {
                self.machine_remove(&name, dry_run).await
            }
            IpcRequest::MachineCordon { name, reason } => self.cordon_machine(&name, reason),
            IpcRequest::MachineUncordon { name } => self.uncordon_machine(&name).await,
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
            IpcRequest::EventsSince {
                after,
                limit,
                task,
                machine,
            } => {
                // Up to two log generations of file reading: off the runtime.
                let path = self.paths.events_file();
                match tokio::task::spawn_blocking(move || {
                    crate::events::since_about(&path, after, limit, task, machine.as_deref())
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
                    // The set, if any, came in `spec`.
                    workdir_set: None,
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
            IpcRequest::LimitList => match self.store.limits() {
                Ok(list) => IpcResponse::Limits(list),
                Err(err) => IpcResponse::error("store_error", err),
            },
            IpcRequest::LimitClear { account, model } => {
                match self.fleet.clear_limits(&account, model.as_deref()) {
                    Ok(gone) if gone.is_empty() => IpcResponse::error(
                        crate::limit_cli::NOT_EXHAUSTED,
                        crate::limit_cli::not_exhausted(&account, model.as_deref()),
                    ),
                    Ok(gone) => IpcResponse::Limits(gone),
                    Err(err) => IpcResponse::error("store_error", err),
                }
            }
            IpcRequest::AgentSignedIn { machine, agent } => {
                self.agent_signed_in(&machine, &agent).await
            }
            IpcRequest::LoginStart { machine, agent } => self.login_start(&machine, &agent).await,
            IpcRequest::LoginList => IpcResponse::Logins(self.logins.list()),
            IpcRequest::LoginCode {
                machine,
                agent,
                code,
            } => self.login_code(&machine, &agent, &code).await,
            IpcRequest::LoginCancel { machine, agent } => self.login_cancel(&machine, &agent).await,
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
            IpcRequest::MachineCheck { name, provisioner } => {
                self.check_machine(&name, provisioner.as_deref()).await
            }
            req @ (IpcRequest::WorkdirPlace { .. }
            | IpcRequest::WorkdirRemove { .. }
            | IpcRequest::WorkdirCheck { .. }
            | IpcRequest::WorkdirList { .. }
            | IpcRequest::WorkdirReport { .. }) => self.workdir(req).await,
            IpcRequest::TaskClaim {
                machine,
                free_slots,
                flock_work,
                herdr_version,
                pastor_version,
                workdir_sets,
            } => match self
                .fleet
                .claim(&machine, free_slots, flock_work, workdir_sets)
                .await
            {
                Ok(tasks) => {
                    // Only a claim the head took speaks for the machine; a
                    // machine with no `machine_state` row keeps none.
                    if (herdr_version.is_some() || pastor_version.is_some())
                        && let Err(err) = self.store.set_versions(
                            &machine,
                            herdr_version.as_deref(),
                            pastor_version.as_deref(),
                        )
                    {
                        tracing::warn!(%machine, %err, "keep a pull machine's versions");
                    }
                    IpcResponse::Tasks(tasks)
                }
                Err(err) => cli_error(err),
            },
            IpcRequest::TaskReport {
                machine,
                id,
                state,
                pane,
                detail,
                limit,
                trusted,
                checkout,
                workdir_skipped,
            } => match self
                .fleet
                .report(
                    &machine,
                    id,
                    PullReport {
                        state,
                        pane,
                        detail,
                        limit,
                        trusted,
                        checkout,
                        workdir_skipped,
                    },
                )
                .await
            {
                Ok(task) => IpcResponse::Task(task),
                Err(err) => cli_error(err),
            },
            IpcRequest::JoinCreate {
                flock,
                tags,
                replace,
                ttl_secs,
            } => self.join_create(flock, tags, replace, ttl_secs),
            IpcRequest::JoinList => match self.store.join_tokens() {
                Ok(rows) => IpcResponse::JoinTokens(rows),
                Err(err) => IpcResponse::error("store_error", err),
            },
            IpcRequest::JoinRevoke { id } => {
                match crate::join::revoke(&crate::authkeys::default_file(), &self.store, &id) {
                    Ok(()) => IpcResponse::Text(format!("revoked join token {id}")),
                    Err(err) => cli_error(err),
                }
            }
            IpcRequest::Join {
                id,
                name,
                machine_key,
                agent_key,
                host_key,
            } => {
                let joined = match crate::join::HeadFiles::for_head(&self.paths) {
                    Ok(files) => {
                        self.join_with(&files, &id, &name, &machine_key, &agent_key, &host_key)
                            .await
                    }
                    Err(err) => Err(err),
                };
                match joined {
                    Ok(()) => IpcResponse::Joined {
                        name,
                        herdr_version: Some(self.join_herdr_version()),
                    },
                    Err(err) => join_error(err),
                }
            }
            IpcRequest::BootstrapReport { machine, result } => {
                match self.fleet.report_bootstrap(&machine, result) {
                    Ok(ready) => {
                        // What waited on it starts now, not next tick.
                        if ready {
                            self.fleet.dispatch_queued().await;
                        }
                        IpcResponse::Text(format!("{machine}: bootstrap recorded"))
                    }
                    Err(err) => cli_error(err),
                }
            }
            IpcRequest::PullTrust { machine } => match self.fleet.pull_trust(&machine) {
                Ok(list) => IpcResponse::Trusted(list),
                Err(err) => cli_error(err),
            },
            IpcRequest::ShepherdCheck {
                machine,
                generation,
            } => self.shepherd_check(&machine, generation),
            IpcRequest::ArtifactLocate {
                name,
                version,
                target,
            } => self.locate_artifact(&name, &version, &target),
            IpcRequest::TaskSweep { machine, yes } => self.sweep(machine.as_deref(), yes).await,
            IpcRequest::MachineBootstrap {
                name,
                machine,
                host_key,
            } => self.bootstrap_machine(&name, machine, host_key).await,
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
            IpcRequest::OrchestratorStart { name } => {
                match self
                    .orchestrators
                    .start_by_hand(&name, chrono::Utc::now())
                    .await
                {
                    Ok(said) => IpcResponse::Text(said),
                    Err((code, message)) => IpcResponse::error(&code, message),
                }
            }
            IpcRequest::OrchestratorStop { name } => {
                match self
                    .orchestrators
                    .stop_by_hand(&name, chrono::Utc::now())
                    .await
                {
                    Ok(said) => IpcResponse::Text(said),
                    Err((code, message)) => IpcResponse::error(&code, message),
                }
            }
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

    /// `MachineCordon`: `name` takes no new task from now on. The reason is
    /// one line, cut to size; cordoning again replaces it. Tasks already
    /// there go on.
    fn cordon_machine(&self, name: &str, reason: Option<String>) -> IpcResponse {
        if self.fleet.flock().get(name).is_none() {
            return IpcResponse::error(
                "unknown_machine",
                format!("no machine {name} in the flock"),
            );
        }
        let reason = reason
            .map(|r| clean_report_text(&r))
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty());
        let was = self.store.cordon(name).ok().flatten();
        let cordon = crate::store::Cordon {
            machine: name.to_string(),
            reason,
            // The first cordon's time stays through a new reason.
            since: was.as_ref().map_or_else(chrono::Utc::now, |c| c.since),
        };
        if let Err(err) = self.store.put_cordon(&cordon) {
            return IpcResponse::error("store_error", err);
        }
        self.fleet.emit_machine(
            "machine.cordoned",
            name,
            Some(serde_json::json!({ "reason": cordon.reason })),
        );
        IpcResponse::Text(match (&cordon.reason, was) {
            (Some(r), None) => format!("{name} is cordoned: {r}; it takes no new task"),
            (None, None) => format!("{name} is cordoned; it takes no new task"),
            (Some(r), Some(_)) => format!("{name} stays cordoned, now: {r}"),
            (None, Some(_)) => format!("{name} stays cordoned"),
        })
    }

    /// `MachineUncordon`: `name` takes tasks again, and what waited for it is
    /// placed at once.
    async fn uncordon_machine(&self, name: &str) -> IpcResponse {
        if self.fleet.flock().get(name).is_none() {
            return IpcResponse::error(
                "unknown_machine",
                format!("no machine {name} in the flock"),
            );
        }
        match self.store.drop_cordon(name) {
            Ok(true) => {
                self.fleet.emit_machine("machine.uncordoned", name, None);
                self.fleet.dispatch_queued().await;
                IpcResponse::Text(format!("{name} is uncordoned; it takes tasks again"))
            }
            Ok(false) => IpcResponse::Text(format!("{name} was not cordoned")),
            Err(err) => IpcResponse::error("store_error", err),
        }
    }

    /// `MachineCheck`: the machine's provisioner's checks, run there with the
    /// flock and pastor.toml as last applied. Each failed row is a
    /// `machine.check_failed` event, so whoever watches the head sees what a
    /// machine is missing without reading the report.
    async fn check_machine(&self, name: &str, provisioner: Option<&str>) -> IpcResponse {
        let flock = self.fleet.flock();
        let Some(m) = flock.get(name) else {
            return IpcResponse::error(
                "unknown_machine",
                format!("no machine {name} in the flock"),
            );
        };
        let catalog = match crate::provision::Catalog::load(&self.paths) {
            Ok(c) => c,
            Err(err) => return IpcResponse::error("runtime_error", format!("{err:#}")),
        };
        let shell = match crate::provision::run::MachineShell::new(m, &self.paths) {
            Ok(shell) => Arc::new(shell),
            Err(err) => return IpcResponse::error(err.code(), err),
        };
        let (defaults, agents, models, profiles, harnesses) = (
            self.fleet.defaults(),
            self.fleet.agents(),
            self.fleet.models(),
            self.fleet.profiles(),
            self.fleet.harnesses(),
        );
        let herdr_pin = self.fleet.herdr_pin();
        let head_address = self.fleet.head_address();
        let config = crate::machine_check::Config {
            defaults: &defaults,
            agents: &agents,
            models: &models,
            profiles: &profiles,
            harnesses: &harnesses,
            herdr_pin: herdr_pin.as_deref(),
            head_address: head_address.as_deref(),
        };
        let running = crate::machine_check::running_herdr(m, &self.paths).await;
        let mut report = match crate::machine_check::check(
            shell.clone(),
            m,
            &flock,
            &config,
            &catalog,
            provisioner,
            running.as_deref(),
            Some(Arc::new(crate::herdr::transport::Endpoint::from_machine(
                m,
                &self.paths,
            ))),
        )
        .await
        {
            Ok(report) => report,
            Err(err) => return cli_error(err),
        };
        report.rows.extend(
            crate::bundle::keep::check_rows(&self.paths, &flock, &config, m, shell.clone()).await,
        );
        report.rows.extend(self.workdir_rows(&flock, name).await);
        let inbox =
            crate::machine_check::inbox_rows(m, &flock, &config, &self.paths, &report.rows).await;
        report.rows.extend(inbox);
        // What the check found of the way to the head is kept, and a machine
        // that now reaches it takes what waited for it.
        if self.fleet.note_reach(name, &report) {
            self.fleet.dispatch_queued().await;
        }
        for row in report.failed() {
            let _ = self.events.send(PastorEvent {
                kind: crate::machine_check::CHECK_FAILED.into(),
                task_id: None,
                machine: Some(name.to_string()),
                job: None,
                detail: Some(report.event_detail(row)),
                summary: None,
            });
        }
        IpcResponse::MachineChecked(report)
    }

    /// The `workdirs` row of `machine check` for `machine`: a read of the
    /// workdir sets the head placed there, when it placed any.
    async fn workdir_rows(&self, flock: &Flock, machine: &str) -> Vec<crate::provision::run::Row> {
        let placed = crate::bundle::workdir_push::list(&self.paths, Some(machine))
            .is_ok_and(|r| !r.is_empty());
        if !placed {
            return vec![];
        }
        let config = match PastorConfig::load(&self.paths.config_file()) {
            Ok(c) => c,
            Err(err) => {
                return vec![crate::provision::run::Row {
                    id: crate::bundle::workdir_keep::ROW.into(),
                    agent: None,
                    status: crate::provision::run::Status::Warn,
                    first_line: format!("the workdir sets cannot be read: {err:#}"),
                    reason: None,
                }];
            }
        };
        let emit = |_: &'static str, _: &str, _: serde_json::Value| {};
        let head = crate::bundle::workdir_push::Head {
            paths: &self.paths,
            config: &config,
            flock,
            reach: &self.workdir_reach,
            emit: &emit,
        };
        crate::bundle::workdir_keep::check_rows(&head, machine).await
    }

    /// `m`'s bundles for `machine describe`, with the keeper's last reading
    /// of its homes.
    fn bundle_config(
        &self,
        flock: &Flock,
        m: &crate::config::flock::MachineConfig,
    ) -> Option<crate::bundle::keep::ConfigDescription> {
        let (defaults, agents, models, profiles) = (
            self.fleet.defaults(),
            self.fleet.agents(),
            self.fleet.models(),
            self.fleet.profiles(),
        );
        let harnesses = self.fleet.harnesses();
        let config = crate::machine_check::Config {
            defaults: &defaults,
            agents: &agents,
            models: &models,
            profiles: &profiles,
            harnesses: &harnesses,
            herdr_pin: None,
            head_address: None,
        };
        let read = self.fleet.keeper().and_then(|k| k.last_read(&m.name));
        crate::bundle::keep::describe(&self.paths, flock, &config, m, read.as_ref())
    }

    /// `MachineDescribe`: one of the head's machines, as its actor sees it.
    /// `AgentSignedIn`: run `agent`'s login check on `machine`, and when it
    /// passes, clear the signed-out mark its agent has there
    /// (`Fleet::clear_signed_out`, `by: check`).
    async fn agent_signed_in(&self, machine: &str, agent: &str) -> IpcResponse {
        let flock = self.fleet.flock();
        let Some(m) = flock.get(machine) else {
            return IpcResponse::error(
                "unknown_machine",
                format!("no machine {machine} in the flock"),
            );
        };
        let agents = self.fleet.agents();
        let harnesses = self.fleet.harnesses();
        let ep = crate::herdr::transport::Endpoint::from_machine(m, &self.paths);
        if let Err(e) = crate::agent_cli::login_check(&ep, &agents, &harnesses, m, agent).await {
            return IpcResponse::error(&e.code, e.message);
        }
        let key = agents.signed_out_key(machine, agent);
        match self.fleet.clear_signed_out(&key, "check") {
            Ok(gone) => IpcResponse::Limits(gone),
            Err(err) => IpcResponse::error("store_error", err),
        }
    }

    /// The machine `name` of the flock and a connector to it, for a login.
    fn login_machine(
        &self,
        name: &str,
    ) -> Result<(MachineConfig, Arc<dyn Connector>), crate::cli::CliError> {
        let flock = self.fleet.flock();
        let refuse = |code: &str, message: String| crate::cli::CliError {
            code: code.into(),
            message,
        };
        let Some(m) = flock.get(name) else {
            return Err(refuse(
                "unknown_machine",
                format!("no machine {name} in the flock"),
            ));
        };
        match self.fleet.connector(m) {
            Some(conn) => Ok((m.clone(), conn)),
            None => Err(refuse(
                "machine_unreachable",
                format!("the head has no way to reach {name}"),
            )),
        }
    }

    /// `LoginStart`: open `agent`'s interactive login on `machine` and
    /// watch it (`login::start`).
    async fn login_start(&self, machine: &str, agent: &str) -> IpcResponse {
        let (m, conn) = match self.login_machine(machine) {
            Ok(found) => found,
            Err(e) => return IpcResponse::error(&e.code, e.message),
        };
        let plan =
            match crate::login::plan(&self.fleet.agents(), &self.fleet.harnesses(), &m, agent) {
                Ok(plan) => plan,
                Err(e) => return IpcResponse::error(&e.code, e.message),
            };
        let fleet: Arc<dyn crate::login::Head> = self.fleet.clone();
        match crate::login::start(&self.logins, Arc::downgrade(&fleet), conn, plan).await {
            Ok(started) => IpcResponse::Login(started),
            Err(e) => IpcResponse::error(&e.code, e.message),
        }
    }

    /// `LoginCode`: type a code pasted back from the browser into a
    /// pending login's pane.
    async fn login_code(&self, machine: &str, agent: &str, code: &str) -> IpcResponse {
        let (_, conn) = match self.login_machine(machine) {
            Ok(found) => found,
            Err(e) => return IpcResponse::error(&e.code, e.message),
        };
        match crate::login::paste(&self.logins, conn.as_ref(), machine, agent, code).await {
            Ok(login) => IpcResponse::Logins(vec![login]),
            Err(e) => IpcResponse::error(&e.code, e.message),
        }
    }

    /// `LoginCancel`: close a pending login's pane and forget it.
    async fn login_cancel(&self, machine: &str, agent: &str) -> IpcResponse {
        let (_, conn) = match self.login_machine(machine) {
            Ok(found) => found,
            Err(e) => return IpcResponse::error(&e.code, e.message),
        };
        let head: &dyn crate::login::Head = self.fleet.as_ref();
        match crate::login::cancel(&self.logins, head, conn.as_ref(), machine, agent).await {
            Ok(login) => IpcResponse::Logins(vec![login]),
            Err(e) => IpcResponse::error(&e.code, e.message),
        }
    }

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
            fallback: m.fallback.clone(),
            agents_by_kind: m.agents.clone(),
            tasks,
            recent_errors: crate::describe::machine_errors(events, name),
            config: self.bundle_config(&flock, m),
            workdirs: crate::bundle::workdir_keep::describe(
                &self.paths,
                name,
                !m.pull && (self.workdir_reach)(m).is_some(),
            ),
            // A description is still worth sending without the limits too.
            signed_out: crate::describe::signed_out_on(
                self.store.limits().unwrap_or_default(),
                name,
            ),
            bootstrap: self.store.machine_state(name).unwrap_or_default(),
        })
    }

    /// `TaskSend`: through the actor of the task's machine, which checks the
    /// row again and types into the pane. A task on no machine, or one whose
    /// machine has left the flock, has no agent to type into.
    async fn send(&self, id: i64, input: SendInput) -> IpcResponse {
        if input.is_empty() || input.pick.as_deref().is_some_and(|p| p.trim().is_empty()) {
            return IpcResponse::error("nothing_to_send", "give text, --key, --trust or --pick");
        }
        if input.trust && (input.text.is_some() || !input.keys.is_empty()) {
            return IpcResponse::error(
                "usage_error",
                "--trust sends the agent's own keys; give no text or --key with it",
            );
        }
        if input.pick.is_some() && (input.text.is_some() || !input.keys.is_empty() || input.trust) {
            return IpcResponse::error(
                "usage_error",
                "--pick answers the picker on screen; give no text, --key or --trust with it",
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
        match self
            .fleet
            .bounded(&handle.name, handle.send(id, input))
            .await
        {
            Ok(t) if trust => IpcResponse::Text(match t.spec.trust_dir() {
                Some(dir) => format!(
                    "sent the trust keys to {}; {dir} on {} is trusted from now on",
                    t.display_id(),
                    handle.name
                ),
                None => format!(
                    "sent the trust keys to {}; it has no folder on record, so nothing was saved",
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
        match self
            .fleet
            .bounded(&handle.name, handle.end_by(id, summary, by))
            .await
        {
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
        self.fleet.dispatch_queued_for(task.id).await;
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
            // A waiting task goes on by itself when its limit resets; a
            // retry would start a second agent on the same work.
            Err(QueueError::Store(RetryError::NotRetryable {
                id,
                state: TaskState::Waiting,
            })) => {
                return IpcResponse::error(
                    TASK_WAITING,
                    format!(
                        "t-{id} is waiting for a usage limit to reset and resumes by itself; \
                         `pastor limit clear` wakes it sooner, `pastor task close t-{id}` gives it up"
                    ),
                );
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
        self.fleet.dispatch_queued_for(task.id).await;
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
            return match self
                .fleet
                .bounded(&handle.name, handle.close(id, remove_worktree))
                .await
            {
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
            // A paused or waiting task holds no pane: a plain close is its
            // row alone, unless a resume claims it first. Its checkout is on
            // its machine, so `--remove-worktree` goes there.
            let paused =
                matches!(t.state, TaskState::Paused | TaskState::Waiting) && !remove_worktree;
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
        match self
            .fleet
            .bounded(&handle.name, handle.close(id, remove_worktree))
            .await
        {
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
    // Held from before flock.toml is read until the socket listens, so an
    // edit made with no head either lands before the load or sees this head
    // listening and stops (`fleet_edit::lock_fleet`).
    let fleet_lock = {
        let paths = paths.clone();
        tokio::task::spawn_blocking(move || {
            crate::fleet_edit::lock_fleet(&paths, crate::fleet_edit::FLEET_LOCK_WAIT)
        })
        .await??
    };
    // Before the loads: an edit that lands after them must still read as a
    // change on the scheduler's first pass.
    let on_disk = ConfigFingerprint::sample(&paths);
    let config = PastorConfig::load(&paths.config_file())?;
    let flock = Flock::load(&paths.flock_file())?;
    flock.check_config(
        &config.models,
        &config.agents,
        &config.profiles,
        &crate::provision::Catalog::load(&paths)?,
    )?;
    flock.check_bundles(&crate::bundle::Catalog::load(&paths)?)?;
    anyhow::ensure!(
        !flock.machines.is_empty(),
        "flock is empty; add a machine with `pastor machine add`"
    );
    warn_cut_off_from_head(&config, &flock.machines);
    let (daemon, listener) = Daemon::bind_and_start(paths, config, flock, on_disk, None).await?;
    drop(fleet_lock);
    tracing::info!(socket = %daemon.socket_path().display(), machines = daemon.fleet.machines().len(), "pastor serve");
    daemon.run_with_listener(listener).await
}

#[cfg(all(test, feature = "fake-herdr"))]
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
            reserved: 0,
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
            loads: vec![],
            flock: None,
            agent: None,
            agent_args: None,
            model: None,
            fallback: None,
            priority: None,
            agents: Default::default(),
            profile: None,
            min_disk: None,
            min_memory: None,
            unknown_room: None,
            provisioner: None,
            bundles: Vec::new(),
            bundle_apply: None,
            pins: Default::default(),
            env: Default::default(),
        }
    }

    fn spec() -> DispatchSpec {
        DispatchSpec {
            now: false,
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
            cwd: None,
            keep_pane: None,
            keep_pane_from: None,
            remove_worktree: None,
            remove_worktree_from: None,
            load: None,
            load_from: None,
            fetch: Default::default(),
            start_prompt: None,
            workdir: None,
            workdir_bundles: Vec::new(),
            rounds: Default::default(),
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

    /// `version_every` in pastor.toml reaches the actors like the other
    /// timings, so a reload can shorten how long a new install stays unseen.
    #[test]
    fn machine_settings_carry_version_every() {
        let config = PastorConfig {
            version_every: "45s".into(),
            ..Default::default()
        };
        assert_eq!(
            machine_settings(&config).version_every,
            Duration::from_secs(45)
        );
        assert_eq!(
            machine_settings(&test_config()).version_every,
            Duration::from_secs(120)
        );
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
            close_failed_after: None,
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

    /// A state dir deep enough that `pastor.sock` overflows `sun_path` is
    /// refused before bind, with the hint to shorten `PASTOR_STATE_DIR`.
    #[tokio::test]
    async fn bind_socket_refuses_a_socket_path_past_sun_path() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp
            .path()
            .join("x".repeat(crate::ssh::UNIX_PATH_MAX))
            .join("pastor.sock");
        let err = Daemon::bind_socket(&socket).await.unwrap_err();
        let e = err
            .downcast_ref::<crate::cli::CliError>()
            .expect("CliError");
        assert_eq!(e.code, "config_error");
        assert!(e.message.contains("PASTOR_STATE_DIR"), "{}", e.message);
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

    /// An actor gets the manifests of the kinds its machine pins, and a
    /// manifest edit replaces only the actors that read it: one for another
    /// harness, or a harness no pin names, leaves them be.
    #[tokio::test]
    async fn a_manifest_edit_replaces_only_the_actors_that_read_it() {
        use crate::harness_manifest::{Harnesses, Manifest};
        let (fleet, _store) = managed(&[]);
        let mut flock = flock_of(&[("a", 2), ("b", 2)]);
        flock.machines[0].pins = [("claude".to_string(), "2".to_string())].into();
        let edited = |id: &str, home: &str| {
            Harnesses::builtin().clone().with(
                Manifest::parse(&format!(
                    "id = \"{id}\"\nbinary = \"{id}\"\n[version]\nargs = [\"--version\"]\n[paths]\nhome = \"{home}\"\n"
                ))
                .unwrap(),
            )
        };
        fleet.set_harnesses(Arc::new(Harnesses::builtin().clone()));
        fleet.apply_flock(&flock, &fast()).await;
        let read = |name: &str| {
            fleet
                .members
                .read()
                .recover()
                .iter()
                .find(|m| m.handle.name == name)
                .and_then(|m| m.spawned_from.clone())
                .unwrap()
                .1
                .harnesses
        };
        assert!(read("a").get("claude").is_some());
        assert!(read("a").get("codex").is_none(), "only what its pins read");
        assert!(read("b").get("claude").is_none(), "b pins nothing");
        // Another harness's manifest: nobody is replaced.
        fleet.set_harnesses(Arc::new(edited("codex", "~/.codex-mine")));
        let d = fleet.apply_flock(&flock, &fast()).await;
        assert!(d.is_empty(), "{d:?}");
        // The pinned one: its machine's actor, and only that.
        fleet.set_harnesses(Arc::new(edited("claude", "~/.claude-mine")));
        let d = fleet.apply_flock(&flock, &fast()).await;
        assert_eq!(d.retargeted, vec!["a".to_string()], "{d:?}");
        assert_eq!(
            read("a").get("claude").unwrap().paths.home,
            "~/.claude-mine"
        );
    }

    /// The manifests in the config dir are the ones the actors start with,
    /// not the built-ins until some file changes.
    #[tokio::test]
    async fn the_actors_start_with_the_users_manifests() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        std::fs::create_dir_all(paths.harnesses_dir()).unwrap();
        std::fs::write(
            paths.harnesses_dir().join("claude.toml"),
            "id = \"claude\"\nbinary = \"claude\"\n[version]\nargs = [\"-v\"]\n[paths]\nhome = \"~/.claude-mine\"\n",
        )
        .unwrap();
        let mut flock = flock_of(&[("a", 2)]);
        flock.machines[0].pins = [("claude".to_string(), "2".to_string())].into();
        flock.save(&paths.flock_file()).unwrap();
        std::fs::write(
            paths.config_file(),
            toml::to_string(&test_config()).unwrap(),
        )
        .unwrap();
        let on_disk = ConfigFingerprint::sample(&paths);
        let named = [("a", FakeHerdr::new())];
        let d = Daemon::start(paths, test_config(), flock, on_disk, Some(factory(&named)))
            .await
            .unwrap();
        let harnesses = d
            .fleet()
            .members
            .read()
            .recover()
            .iter()
            .find(|m| m.handle.name == "a")
            .and_then(|m| m.spawned_from.clone())
            .unwrap()
            .1
            .harnesses;
        let claude = harnesses.get("claude").unwrap();
        assert_eq!(claude.paths.home, "~/.claude-mine");
        assert!(claude.version_command().unwrap().contains("claude -v"));
    }

    /// A machine's actor gets the env layers that reach it (`[defaults]`, the
    /// flocks it is in, its own), and a change of any replaces only the actors
    /// of the machines it reaches.
    #[tokio::test]
    async fn the_env_layers_reach_each_machines_actor() {
        let (fleet, _store) = managed(&[]);
        let mut flock = flock_of(&[("a", 2), ("b", 2), ("c", 2)]);
        flock.flocks.push(crate::config::flock::FlockEntry {
            name: "ro".into(),
            machines: [("b".to_string(), crate::config::flock::FlockNumber::plain(1))].into(),
            env: [("FLOCK".to_string(), "ro".to_string())].into(),
            ..Default::default()
        });
        flock.machines[0].env = [("MACHINE".to_string(), "a".to_string())].into();
        let mut config = PastorConfig::default();
        config.defaults.env = [("ALL".to_string(), "1".to_string())].into();
        fleet.set_config(&config);
        fleet.apply_flock(&flock, &fast()).await;
        let layers = |name: &str| {
            fleet
                .members
                .read()
                .recover()
                .iter()
                .find(|m| m.handle.name == name)
                .and_then(|m| m.spawned_from.clone())
                .unwrap()
                .1
                .env
        };
        assert_eq!(layers("a").machine["MACHINE"], "a");
        assert_eq!(layers("a").defaults["ALL"], "1");
        assert!(layers("a").flocks.is_empty());
        assert_eq!(layers("b").flocks["ro"]["FLOCK"], "ro");
        assert!(layers("b").machine.is_empty());
        assert_eq!(layers("c").defaults["ALL"], "1");

        config.defaults.env.insert("ALL".into(), "2".into());
        fleet.set_config(&config);
        let d = fleet.apply_flock(&flock, &fast()).await;
        assert_eq!(
            d.retargeted.len(),
            3,
            "[defaults] reaches every machine: {d:?}"
        );
        assert_eq!(layers("c").defaults["ALL"], "2");

        flock.flocks[0].env.insert("FLOCK".into(), "ro2".into());
        let d = fleet.apply_flock(&flock, &fast()).await;
        assert_eq!(
            d.retargeted,
            vec!["b".to_string()],
            "only the flock's machine: {d:?}"
        );
    }

    /// A machine's actor gets its own profile: its
    /// `[[machine]]` one, else its flock's, else `[defaults]`. A change of
    /// that profile replaces the actor, as an `[agents]` edit does.
    #[tokio::test]
    async fn a_machines_own_profile_reaches_its_actor() {
        let (fleet, _store) = managed(&[]);
        let profiles = crate::config::profile::Profiles::default();
        let policy = |name: &str| Some(profiles.resolve(name).unwrap().agy_policy());
        let mut config = PastorConfig::default();
        config.defaults.profile = Some("develop".into());
        let mut flock = flock_of(&[("a", 2), ("b", 2), ("c", 2)]);
        flock.machines[0].profile = Some("review".into());
        flock.machines[1].flock = Some("ro".into());
        flock.flocks.push(crate::config::flock::FlockEntry {
            name: "ro".into(),
            profile: Some("unrestricted".into()),
            ..Default::default()
        });
        fleet.set_config(&config);
        fleet.apply_flock(&flock, &fast()).await;
        let spawned = |name: &str| {
            fleet
                .members
                .read()
                .recover()
                .iter()
                .find(|m| m.handle.name == name)
                .and_then(|m| m.spawned_from.clone())
                .unwrap()
                .1
                .own_policy
        };
        assert_eq!(spawned("a"), policy("review"));
        assert_eq!(spawned("b"), policy("unrestricted"));
        assert_eq!(spawned("c"), policy("develop"));

        config.defaults.profile = Some("review".into());
        fleet.set_config(&config);
        let d = fleet.apply_flock(&flock, &fast()).await;
        assert_eq!(d.retargeted, vec!["c".to_string()]);
        assert_eq!(spawned("c"), policy("review"));
    }

    /// The herdr version `machine check` holds machines to is `[artifacts]
    /// herdr` as last applied, blank counting as none, and a reload moves it.
    #[tokio::test]
    async fn the_herdr_pin_follows_the_applied_config() {
        let (fleet, _store) = managed(&[]);
        assert_eq!(fleet.herdr_pin(), None);
        let mut config = PastorConfig::default();
        config.artifacts.herdr = Some(" 0.9.5 ".into());
        fleet.set_config(&config);
        assert_eq!(fleet.herdr_pin().as_deref(), Some("0.9.5"));
        config.artifacts.herdr = Some("  ".into());
        fleet.set_config(&config);
        assert_eq!(fleet.herdr_pin(), None);
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
        daemon_with_flock_and_config(flock, fakes, test_config()).await
    }

    /// `daemon_with_flock`, with the timings of `config`.
    async fn daemon_with_flock_and_config(
        flock: Flock,
        fakes: &[(&str, u32, FakeHerdr)],
        config: PastorConfig,
    ) -> (Daemon, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        // On disk too, as `serve` would have found them: a `pastor job reload`
        // re-reads both, and a missing file would read as an empty flock and
        // default timings.
        flock.save(&paths.flock_file()).unwrap();
        std::fs::write(paths.config_file(), toml::to_string(&config).unwrap()).unwrap();
        let named: Vec<(&str, FakeHerdr)> = fakes.iter().map(|(n, _, f)| (*n, f.clone())).collect();
        let on_disk = ConfigFingerprint::sample(&paths);
        let d = Daemon::start(paths, config, flock, on_disk, Some(factory(&named)))
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
            now: false,
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
            herdr_version: None,
            pastor_version: None,
            workdir_sets: false,
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
            limit: None,
            trusted: None,
            checkout: None,
            workdir_skipped: Vec::new(),
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
    /// A pull machine's `machine_state` row at `generation`.
    fn enrolled(d: &Daemon, machine: &str, generation: i64) {
        d.store
            .put_machine_state(&crate::store::MachineState {
                machine: machine.into(),
                bootstrap: crate::store::BootstrapState::Ok,
                isolation: None,
                failed_step: None,
                why: None,
                host_key: None,
                herdr_version: None,
                pastor_version: None,
                generation,
                updated_at: chrono::Utc::now(),
            })
            .unwrap();
    }

    async fn shepherd_check(d: &Daemon, machine: &str, generation: i64) -> IpcResponse {
        d.handle(IpcRequest::ShepherdCheck {
            machine: machine.into(),
            generation,
        })
        .await
    }

    fn revoked(resp: &IpcResponse) -> bool {
        matches!(resp, IpcResponse::Error { code, .. } if code == crate::bridge::MACHINE_REVOKED)
    }

    /// A shepherd's generation is current only while it is the one on the
    /// machine's `machine_state` row: an older one, as after a new
    /// enrollment, is `machine_revoked`.
    #[tokio::test]
    async fn shepherd_stale_generation_refused() {
        let (d, _tmp) = pull_daemon(1).await;
        enrolled(&d, "laptop", 4);
        let resp = shepherd_check(&d, "laptop", 4).await;
        assert!(matches!(resp, IpcResponse::Text(_)), "{resp:?}");
        let resp = shepherd_check(&d, "laptop", 3).await;
        assert!(revoked(&resp), "{resp:?}");
        enrolled(&d, "laptop", 5);
        let resp = shepherd_check(&d, "laptop", 4).await;
        assert!(revoked(&resp), "moved past on a live bridge: {resp:?}");
    }

    /// A machine with no `machine_state` row, or none in flock.toml any
    /// more, is `machine_revoked` whatever generation it names.
    #[tokio::test]
    async fn shepherd_unknown_machine_refused() {
        let (d, _tmp) = pull_daemon(1).await;
        let resp = shepherd_check(&d, "laptop", 0).await;
        assert!(revoked(&resp), "no row: {resp:?}");
        enrolled(&d, "gone", 0);
        let resp = shepherd_check(&d, "gone", 0).await;
        assert!(revoked(&resp), "not in flock.toml: {resp:?}");
    }

    /// The herdr and pastor versions a claim carries land on the machine's
    /// `machine_state` row; a claim without them leaves what is there.
    #[tokio::test]
    async fn claim_versions_stored() {
        let (d, _tmp) = pull_daemon(1).await;
        enrolled(&d, "laptop", 0);
        let resp = d
            .handle(IpcRequest::TaskClaim {
                machine: "laptop".into(),
                free_slots: 1,
                flock_work: false,
                herdr_version: Some("0.9.1".into()),
                pastor_version: Some("0.10.0".into()),
                workdir_sets: false,
            })
            .await;
        assert!(matches!(resp, IpcResponse::Tasks(_)), "{resp:?}");
        let state = d.store.machine_state("laptop").unwrap().unwrap();
        assert_eq!(
            (
                state.herdr_version.as_deref(),
                state.pastor_version.as_deref()
            ),
            (Some("0.9.1"), Some("0.10.0"))
        );
        claim(&d, 1, false).await;
        let state = d.store.machine_state("laptop").unwrap().unwrap();
        assert_eq!(state.herdr_version.as_deref(), Some("0.9.1"));
    }

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
                    herdr_version: None,
                    pastor_version: None,
                    workdir_sets: false,
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

    /// A pull machine is handed the folder trust saved for it and nobody
    /// else's; a machine the head reaches itself, or one it does not know,
    /// is refused as a claim would be.
    #[tokio::test]
    async fn a_pull_machine_gets_only_its_own_saved_trust() {
        let (d, _tmp) = pull_daemon(2).await;
        d.store().trust_repo("laptop", "~/work/app").unwrap();
        d.store().trust_repo("a", "~/work/app").unwrap();
        d.store().trust_repo("a", "~/work/lib").unwrap();
        let ask = |machine: &str| IpcRequest::PullTrust {
            machine: machine.into(),
        };
        let IpcResponse::Trusted(list) = d.handle(ask("laptop")).await else {
            panic!()
        };
        let got: Vec<(String, String)> = list.into_iter().map(|t| (t.machine, t.repo)).collect();
        assert_eq!(got, [("laptop".to_string(), "~/work/app".to_string())]);
        assert_eq!(error_code(d.handle(ask("a")).await), "not_pull_machine");
        assert_eq!(error_code(d.handle(ask("nope")).await), "unknown_machine");
    }

    /// The trust keys a pull machine pressed become `task.trusted`, with
    /// the keys, once per task: after the `task.blocked` the same report
    /// brings, before a `task.running`, and never again for a report sent
    /// twice.
    #[tokio::test]
    async fn a_pull_machine_s_trust_answer_is_task_trusted_once() {
        let (d, _tmp) = pull_daemon(3).await;
        let t1 = queued(&d, "p1", Some("laptop")).await;
        let t2 = queued(&d, "p2", Some("laptop")).await;
        claim(&d, 2, false).await;
        let keys = vec!["Down".to_string(), "Enter".to_string()];
        let trusted = |t: &Task, state| {
            let mut req = report("laptop", t, state, None);
            if let IpcRequest::TaskReport { trusted, .. } = &mut req {
                *trusted = Some(keys.clone());
            }
            req
        };
        let mut rx = d.subscribe();
        let detail = |rx: &mut broadcast::Receiver<PastorEvent>| {
            std::iter::from_fn(|| rx.try_recv().ok())
                .map(|e| (e.kind, e.task_id, e.detail))
                .collect::<Vec<_>>()
        };
        d.handle(trusted(&t1, TaskState::Blocked)).await;
        d.handle(trusted(&t1, TaskState::Blocked)).await;
        d.handle(trusted(&t1, TaskState::Running)).await;
        d.handle(trusted(&t2, TaskState::Running)).await;
        let with_keys = Some(serde_json::json!({ "keys": keys }));
        assert_eq!(
            detail(&mut rx),
            [
                ("task.blocked".to_string(), Some(t1.id), None),
                ("task.trusted".to_string(), Some(t1.id), with_keys.clone()),
                ("task.running".to_string(), Some(t1.id), None),
                ("task.trusted".to_string(), Some(t2.id), with_keys),
                ("task.running".to_string(), Some(t2.id), None),
            ]
        );
        assert!(d.store().trust_sent(t1.id).unwrap());
        assert!(
            d.store().answered(t1.id, "trust").unwrap(),
            "claimed as the actor claims it, so the two tables agree"
        );
        // A report without keys says nothing of trust.
        let t3 = queued(&d, "p3", Some("laptop")).await;
        claim(&d, 1, false).await;
        d.handle(report("laptop", &t3, TaskState::Blocked, None))
            .await;
        assert!(!d.store().trust_sent(t3.id).unwrap());
    }

    /// A pull machine dispatches a worktree task itself, never through the
    /// head's own `dispatch` (`src/dispatch.rs`): only its `TaskReport`
    /// tells the head the checkout it opened. The head fills the row's own
    /// once, on the report that first carries it, and a later report that
    /// says nothing new about it does not clear it.
    #[tokio::test]
    async fn a_pull_machine_s_checkout_report_fills_the_head_s_row() {
        let (d, _tmp) = pull_daemon(2).await;
        let t = queued(&d, "p", Some("laptop")).await;
        claim(&d, 1, false).await;
        let checkout = crate::task::Checkout {
            branch: "pastor/t-1".into(),
            path: "/tmp/wt".into(),
            already_open: false,
        };
        let IpcResponse::Task(reported) = d
            .handle(IpcRequest::TaskReport {
                machine: "laptop".into(),
                id: t.id,
                state: TaskState::Running,
                pane: Some("p-1".into()),
                detail: None,
                limit: None,
                trusted: None,
                checkout: Some(checkout.clone()),
                workdir_skipped: Vec::new(),
            })
            .await
        else {
            panic!()
        };
        assert_eq!(reported.spec.checkout.as_deref(), Some(&checkout));
        let row = d.store().get_task(t.id).unwrap().unwrap();
        assert_eq!(row.spec.checkout.as_deref(), Some(&checkout));
        d.handle(report("laptop", &t, TaskState::Running, None))
            .await;
        let row = d.store().get_task(t.id).unwrap().unwrap();
        assert_eq!(
            row.spec.checkout.as_deref(),
            Some(&checkout),
            "a later report with no checkout does not clear the one already recorded"
        );
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
    /// stay queued, and its next claim brings it back. A report of its end
    /// moves a stale task on.
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
        d.handle(report("laptop", &running, TaskState::Done, None))
            .await;
        assert_eq!(state_of(&d, &running), TaskState::Done);
    }

    /// A task stale only because its pull machine went silent was never
    /// seen to stop: the head just did not hear. A report of its agent at
    /// work puts it back, with the loss note cleared. A task the machine
    /// itself reported stale (its own actor timed it out) stays stale.
    #[tokio::test(start_paused = true)]
    async fn a_working_report_brings_back_a_task_staled_by_the_loss() {
        let (d, _tmp) = pull_daemon(4).await;
        let fleet = d.fleet();
        let after = Duration::from_millis(100);
        let running = queued(&d, "r", Some("laptop")).await;
        let starting = queued(&d, "s", Some("laptop")).await;
        let blocked = queued(&d, "b", Some("laptop")).await;
        let own = queued(&d, "o", Some("laptop")).await;
        claim(&d, 4, false).await;
        d.handle(report("laptop", &running, TaskState::Running, None))
            .await;
        // Working when the silence began; it asks a question after.
        d.handle(report("laptop", &blocked, TaskState::Running, None))
            .await;
        d.handle(report("laptop", &own, TaskState::Stale, Some("timed out")))
            .await;
        tokio::time::advance(after).await;
        assert_eq!(fleet.check_pull_lost(after).await, ["laptop"]);
        for t in [&running, &starting, &blocked] {
            assert_eq!(state_of(&d, t), TaskState::Stale);
        }
        let mut rx = d.subscribe();

        // Each state a working agent reports puts its task back, with the
        // loss note cleared.
        for (t, state) in [
            (&running, TaskState::Running),
            (&starting, TaskState::Starting),
            (&blocked, TaskState::Blocked),
        ] {
            d.handle(report("laptop", t, state, None)).await;
            let row = d.store().get_task(t.id).unwrap().unwrap();
            assert_eq!(row.state, state);
            assert_eq!(row.error, None, "{state}");
        }
        let running_events: Vec<_> = events_of(&mut rx)
            .into_iter()
            .filter(|e| e.1 == Some(running.id))
            .collect();
        assert_eq!(
            running_events,
            [(
                "task.running".to_string(),
                Some(running.id),
                "laptop".to_string()
            )]
        );
        // One the machine itself reported stale stays stale, whatever it
        // reports of its agent after.
        for state in [TaskState::Running, TaskState::Starting, TaskState::Blocked] {
            d.handle(report("laptop", &own, state, None)).await;
            assert_eq!(state_of(&d, &own), TaskState::Stale, "{state}");
        }
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
        for late in [
            TaskState::Stale,
            TaskState::Running,
            TaskState::Blocked,
            TaskState::Done,
        ] {
            d.handle(report("laptop", &ended, late, None)).await;
            assert_eq!(state_of(&d, &ended), TaskState::Done, "{late}");
        }
        d.handle(report("laptop", &ended, TaskState::Closed, None))
            .await;
        assert_eq!(state_of(&d, &ended), TaskState::Closed);
    }

    /// A pull machine reports a task its agent ended (`task done` reaches
    /// the head, which keeps the row done) `failed` once the agent is gone,
    /// and then forgets the row: nobody will ever report it closed. The
    /// head closes it on that report, with its finish kept, so the task
    /// stops counting as live and the machine's next claim gets its seat.
    #[tokio::test]
    async fn a_failed_report_on_an_ended_pull_task_closes_it_and_frees_the_seat() {
        let (d, _tmp) = pull_daemon(1).await;
        let first = queued(&d, "first", Some("laptop")).await;
        assert_eq!(claim(&d, 1, false).await, ["first"]);
        d.handle(report("laptop", &first, TaskState::Running, None))
            .await;
        d.handle(IpcRequest::TaskDone {
            id: first.id,
            summary: Some("done: all pushed".into()),
        })
        .await;
        let finished = d.store().get_task(first.id).unwrap().unwrap().finished_at;
        assert!(finished.is_some());
        let mut rx = d.subscribe();
        d.handle(report("laptop", &first, TaskState::Blocked, None))
            .await;
        assert_eq!(state_of(&d, &first), TaskState::Done, "ended stays done");
        d.handle(report(
            "laptop",
            &first,
            TaskState::Failed,
            Some("agent t-1 not found"),
        ))
        .await;
        let row = d.store().get_task(first.id).unwrap().unwrap();
        assert_eq!(
            row.state,
            TaskState::Closed,
            "its agent is gone: nothing is left to close"
        );
        assert_eq!(
            row.finished_at, finished,
            "the finish is the agent's, not the close's"
        );
        assert_eq!(
            row.error, None,
            "the agent's going is no error of the task's"
        );
        assert_eq!(
            events_of(&mut rx),
            [(
                "task.closed".to_string(),
                Some(first.id),
                "laptop".to_string()
            )]
        );
        let second = queued(&d, "second", Some("laptop")).await;
        assert_eq!(
            claim(&d, 1, false).await,
            ["second"],
            "the seat is free again"
        );
        assert_eq!(state_of(&d, &second), TaskState::Starting);
    }

    /// `pastor queue` explains a task pinned to a pull machine by what the
    /// machine's claims see: full while its slots are taken, not connected
    /// only while it has never claimed or was counted lost. The dispatch
    /// pass still never pushes to it.
    #[tokio::test]
    async fn the_queue_explains_a_pull_machine_by_its_slots_not_its_actor() {
        let (d, _tmp) = pull_daemon(1).await;
        let why = |id: i64| {
            d.fleet()
                .queue(None, None)
                .unwrap()
                .into_iter()
                .find(|e| e.task.id == id)
                .map(|e| e.why)
                .expect("queued")
        };
        let first = queued(&d, "first", Some("laptop")).await;
        assert_eq!(
            why(first.id),
            "machine laptop is not connected",
            "never claimed yet"
        );
        assert_eq!(claim(&d, 1, false).await, ["first"]);
        d.handle(report("laptop", &first, TaskState::Running, None))
            .await;
        let second = queued(&d, "second", Some("laptop")).await;
        assert_eq!(why(second.id), "machine laptop is full (1/1)");
        assert!(
            !d.fleet()
                .views()
                .iter()
                .any(|v| v.name == "laptop" && v.healthy),
            "a pass never dispatches to a pull machine"
        );
        let lost = d.fleet().check_pull_lost(Duration::ZERO).await;
        assert_eq!(lost, ["laptop"]);
        assert_eq!(why(second.id), "machine laptop is not connected");
    }

    /// A pull machine with room is nowhere a loose task goes: a pass never
    /// pushes to it, and whether its claims take flock work is its serve's
    /// own setting. With the push machine full, a loose task reads its
    /// flock as full rather than the pull machine as where the next pass
    /// starts it, and takes no slot there from the task pinned behind it.
    #[tokio::test]
    async fn the_queue_never_reads_a_pull_machine_as_where_a_loose_task_goes() {
        let (d, _tmp) = pull_daemon(1).await;
        assert!(
            claim(&d, 1, false).await.is_empty(),
            "connected, nothing to take"
        );
        let on_a = queued(&d, "on a", None).await;
        assert_eq!(on_a.machine.as_deref(), Some("a"), "a has one slot");
        let loose = queued(&d, "loose", None).await;
        let pinned = queued(&d, "pinned", Some("laptop")).await;
        let whys: Vec<(i64, String)> = d
            .fleet()
            .queue(None, None)
            .unwrap()
            .into_iter()
            .map(|e| (e.task.id, e.why))
            .collect();
        assert_eq!(
            whys,
            [
                (loose.id, "flock default is full".to_string()),
                (pinned.id, "next claim: laptop has room".to_string()),
            ]
        );
    }

    /// A pull machine counted lost may never report again, and a task its
    /// agent ended there waits for that: its pane closed and reported, so
    /// the head stops counting it live. The head closes it itself, its
    /// finish and summary kept, so its slot and its flock's seat are free
    /// for the machine's next claim.
    #[tokio::test]
    async fn a_lost_pull_machine_s_ended_task_is_closed_and_frees_its_seat() {
        let (d, _tmp) = pull_daemon(1).await;
        let first = queued(&d, "first", Some("laptop")).await;
        assert_eq!(claim(&d, 1, false).await, ["first"]);
        d.handle(report("laptop", &first, TaskState::Running, None))
            .await;
        d.handle(IpcRequest::TaskDone {
            id: first.id,
            summary: Some("done: all pushed".into()),
        })
        .await;
        let ended = d.store().get_task(first.id).unwrap().unwrap();
        assert!(ended.ended && ended.finished_at.is_some());
        let second = queued(&d, "second", Some("laptop")).await;
        let mut rx = d.subscribe();
        assert_eq!(d.fleet().check_pull_lost(Duration::ZERO).await, ["laptop"]);
        let row = d.store().get_task(first.id).unwrap().unwrap();
        assert_eq!(row.state, TaskState::Closed);
        assert_eq!(
            row.finished_at, ended.finished_at,
            "the finish is the agent's"
        );
        assert_eq!(row.error, None);
        assert_eq!(
            row.summary.as_ref().map(|s| s.text.as_str()),
            Some("done: all pushed")
        );
        let lap = |kind: &str, id: Option<i64>| (kind.to_string(), id, "laptop".to_string());
        assert_eq!(
            events_of(&mut rx),
            [
                lap("machine.lost", None),
                lap("task.closed", Some(first.id))
            ]
        );
        assert_eq!(claim(&d, 1, false).await, ["second"], "the seat is free");
        assert_eq!(state_of(&d, &second), TaskState::Starting);
        assert_eq!(events_of(&mut rx)[0], lap("machine.connected", None));
        // The machine, back, reports the pane closed at last: the row is
        // closed already, and stays so.
        d.handle(report("laptop", &first, TaskState::Closed, None))
            .await;
        assert_eq!(state_of(&d, &first), TaskState::Closed);
        assert!(events_of(&mut rx).is_empty());
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

    /// A done task that keeps its pane holds no slot: a machine at
    /// `max_agents = 1` starts the next task while the kept pane stays.
    #[tokio::test]
    async fn a_kept_done_task_does_not_hold_a_slot() {
        let fake = FakeHerdr::new();
        let (d, _tmp) = daemon(&[("a", 1, fake.clone())]).await;
        let task = |resp: IpcResponse| match resp {
            IpcResponse::Task(t) => t,
            other => panic!("{other:?}"),
        };
        let mut kept = run_at("1", None);
        if let IpcRequest::Run { spec, .. } = &mut kept {
            spec.keep_pane = Some(true);
        }
        let first = task(d.handle(kept).await);
        assert_eq!(first.state, TaskState::Running);
        let next = task(d.handle(run_at("2", None)).await);
        assert_eq!(next.state, TaskState::Queued);
        let done = task(
            d.handle(IpcRequest::TaskDone {
                id: first.id,
                summary: Some("done".into()),
            })
            .await,
        );
        assert_eq!(done.state, TaskState::Done);
        d.fleet().dispatch_queued().await;
        let state = |id: i64| d.store.get_task(id).unwrap().unwrap().state;
        assert_eq!(state(next.id), TaskState::Running);
        assert_eq!(state(first.id), TaskState::Done);
        assert!(
            fake.agents()
                .iter()
                .any(|a| Some(&a.pane_id) == first.pane_id.as_ref())
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
        // The connect read the fake's pastor version; the description says
        // when, so a stale-looking version can be told from one never asked.
        assert!(m.row.pastor_read_at.is_some(), "{m:?}");
        let text = crate::describe::machine_text(&m);
        assert!(text.contains("version read:"), "{text}");
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
                now: false,
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
                now: false,
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
                now: false,
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
            now: false,
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

    /// A task's level comes from `--priority`, else its flock, else the
    /// machine it is pinned to, else `[defaults]`; an unpinned task never
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
        let mut loud = machine("c", 4);
        loud.flock = Some("play".into());
        loud.priority = Some(Priority::Critical);
        let flock = Flock {
            flocks: vec![
                FlockEntry {
                    name: "work".into(),
                    default: true,
                    priority: Some(Priority::High),
                    ..Default::default()
                },
                FlockEntry {
                    name: "play".into(),
                    ..Default::default()
                },
            ],
            machines: vec![urgent, plain, loud],
        };
        let (d, _tmp) = daemon_with_flock(
            flock,
            &[
                ("a", 4, FakeHerdr::new()),
                ("b", 4, FakeHerdr::new()),
                ("c", 4, FakeHerdr::new()),
            ],
        )
        .await;
        let run = |machine: Option<&str>, priority: Option<Priority>| IpcRequest::Run {
            now: false,
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
        // The flock before the machine it is pinned to.
        assert_eq!(
            level(d.handle(run(Some("a"), None)).await),
            (Priority::High, "flock work".into())
        );
        // A flock that sets none leaves it to the machine.
        assert_eq!(
            level(d.handle(run(Some("c"), None)).await),
            (Priority::Critical, "machine c".into())
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

    /// A flock's timeout and place reach its tasks that set none, run or
    /// job, and describe says so; `--timeout` and `--place` win.
    #[tokio::test]
    async fn a_flocks_timeout_and_place_reach_its_tasks() {
        use crate::config::flock::FlockEntry;
        use crate::task::Place;
        let mut pi = machine("pi", 4);
        pi.flock = Some("work".into());
        let flock = Flock {
            flocks: vec![FlockEntry {
                name: "work".into(),
                default: true,
                timeout: Some("30m".into()),
                place: Some(Place::Own),
                ..Default::default()
            }],
            machines: vec![pi],
        };
        let (d, _tmp) = daemon_with_flock(flock, &[("pi", 4, FakeHerdr::new())]).await;
        let run = |ask: AgentChoice| IpcRequest::Run {
            now: false,
            preempt: false,
            summary: None,
            prompt: "x".into(),
            spec: spec(),
            flock: None,
            agent: Some(ask),
            priority: None,
            role: TaskRole::Agent,
            description: None,
        };
        let IpcResponse::Task(t) = d.handle(run(AgentChoice::default())).await else {
            panic!()
        };
        assert_eq!((t.spec.timeout_secs, &t.spec.place), (1800, &Place::Own));
        let text = crate::cli::task_detail(&t);
        assert!(
            text.contains("timeout:    1800s (from flock work)"),
            "{text}"
        );
        assert!(text.contains("place:      own (from flock work)"), "{text}");
        let asked = AgentChoice {
            timeout_secs: Some(60),
            place: Some(Place::Pastor),
            ..Default::default()
        };
        let IpcResponse::Task(t) = d.handle(run(asked)).await else {
            panic!()
        };
        assert_eq!((t.spec.timeout_secs, &t.spec.place), (60, &Place::Pastor));
        let text = crate::cli::task_detail(&t);
        assert!(text.contains("timeout:    60s (from task run)"), "{text}");

        let config = test_config();
        let job = |dispatch: &str| {
            crate::config::job::Job::parse(
                &format!("every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"p\"\n{dispatch}"),
                "j",
                &config.defaults,
                &crate::connector::Builtins,
            )
            .unwrap()
        };
        let queue = |job: crate::config::job::Job, key: &str| {
            let fleet = d.fleet().clone();
            let item = serde_json::json!({ "key": key });
            async move {
                fleet
                    .queue_job_task(&job, &item, |_| Ok(("p".into(), job.spec.clone())))
                    .await
                    .unwrap()
            }
        };
        let t = queue(job(""), "a").await;
        assert_eq!((t.spec.timeout_secs, &t.spec.place), (1800, &Place::Own));
        let t = queue(job("timeout = \"5m\"\nplace = \"repo\"\n"), "b").await;
        assert_eq!((t.spec.timeout_secs, &t.spec.place), (300, &Place::Repo));
        let text = crate::cli::task_detail(&t);
        assert!(text.contains("timeout:    300s (from job j)"), "{text}");
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
            now: false,
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
                now: false,
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
                now: false,
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

    /// `task run --now` (`Run::now`, `DispatchSpec::now`).
    mod now {
        use super::*;

        fn run_now(prompt: &str, machine: Option<&str>) -> IpcRequest {
            let mut req = run_on(prompt, machine);
            if let IpcRequest::Run { now, .. } = &mut req {
                *now = true;
            }
            req
        }

        fn no_rows(d: &Daemon) -> bool {
            d.store
                .list_tasks(&TaskFilter::default())
                .unwrap()
                .is_empty()
        }

        /// On a full machine a `--now` task starts in the same request,
        /// past `max_agents`, while a plain task stays queued; it counts
        /// there, so the queue does not fill the machine further, and
        /// `machine list` names it.
        #[tokio::test]
        async fn a_now_task_starts_past_its_machines_limits() {
            let (d, _tmp) = daemon(&[("a", 1, FakeHerdr::new())]).await;
            let busy = queued(&d, "busy", Some("a")).await;
            assert_eq!(busy.state, TaskState::Running);
            let waiting = queued(&d, "waits", Some("a")).await;
            assert_eq!(waiting.state, TaskState::Queued);

            let IpcResponse::Task(t) = d.handle(run_now("urgent", Some("a"))).await else {
                panic!()
            };
            assert_eq!(t.state, TaskState::Running, "{:?}", t.error);
            assert_eq!(t.machine.as_deref(), Some("a"));
            assert!(t.spec.now);
            let view = d
                .fleet()
                .views()
                .into_iter()
                .find(|v| v.name == "a")
                .unwrap();
            assert_eq!((view.live, view.max_agents), (2, 1));

            d.fleet().dispatch_queued().await;
            assert_eq!(
                d.store.get_task(waiting.id).unwrap().unwrap().state,
                TaskState::Queued,
                "the queue still waits for a slot"
            );
            let status = d.fleet().statuses();
            let a = status.iter().find(|m| m.name == "a").unwrap();
            assert_eq!(a.now, vec![t.display_id()]);
        }

        /// `--now` needs `--machine`, and a machine that is connected: a
        /// pull machine is once the head has heard from it, until it is
        /// counted lost. Nothing is queued either way.
        #[tokio::test]
        async fn now_needs_a_connected_machine() {
            let (d, _tmp) = pull_daemon(1).await;
            assert_eq!(
                error_code(d.handle(run_now("x", None)).await),
                "now_needs_machine"
            );
            assert_eq!(
                error_code(d.handle(run_now("x", Some("laptop"))).await),
                "machine_not_connected",
                "never heard from"
            );
            claim(&d, 1, false).await;
            assert_eq!(d.fleet().check_pull_lost(Duration::ZERO).await, ["laptop"]);
            assert_eq!(
                error_code(d.handle(run_now("x", Some("laptop"))).await),
                "machine_not_connected",
                "lost"
            );
            assert!(no_rows(&d));
        }

        /// Wait until a `--now` task is queued, then answer `then`.
        async fn once_now_is_queued<F: std::future::Future>(d: &Daemon, then: F) -> F::Output {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !d.store.queued_tasks().unwrap().iter().any(|t| t.spec.now) {
                assert!(Instant::now() < deadline, "never queued");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            then.await
        }

        /// A connected pull machine takes a `--now` task on its next claim:
        /// first, past its `max_agents` and past the slots the claim asked
        /// for, while a plain task pinned there waits. The run answers once
        /// the claim took it, and the task counts there after.
        #[tokio::test]
        async fn a_now_task_goes_out_on_a_pull_machine_s_next_claim() {
            let (d, _tmp) = pull_daemon(1).await;
            queued(&d, "busy", Some("laptop")).await;
            assert_eq!(claim(&d, 1, false).await, ["busy"]);
            let waiting = queued(&d, "waits", Some("laptop")).await;
            let (resp, got) = tokio::join!(
                d.handle(run_now("urgent", Some("laptop"))),
                once_now_is_queued(&d, claim(&d, 0, false))
            );
            assert_eq!(got, ["urgent"]);
            let IpcResponse::Task(t) = resp else {
                panic!("{resp:?}")
            };
            assert_eq!(t.state, TaskState::Starting);
            assert_eq!(t.machine.as_deref(), Some("laptop"));
            assert!(t.spec.now);
            assert_eq!(state_of(&d, &waiting), TaskState::Queued);
            assert!(
                claim(&d, 1, false).await.is_empty(),
                "still full: the --now task counts"
            );
        }

        /// A run waits for a pull machine's claim at most `NOW_CLAIM_WAIT`,
        /// which ends well before the client gives up on the request, so the
        /// client hears `now_not_started` itself, however long
        /// `request_timeout` makes the wait for a push machine's start.
        #[tokio::test]
        async fn the_wait_for_a_claim_ends_before_the_client_gives_up() {
            const {
                assert!(
                    NOW_CLAIM_WAIT.as_secs() * 2 <= crate::ipc::DEFAULT_REQUEST_TIMEOUT.as_secs()
                )
            };
            let (d, _tmp) = pull_daemon(1).await;
            assert!(d.fleet().now_claim_wait() < crate::ipc::DEFAULT_REQUEST_TIMEOUT);
            d.fleet().set_reply_wait(Duration::from_secs(600));
            assert_eq!(d.fleet().now_claim_wait(), NOW_CLAIM_WAIT);
            d.fleet().set_reply_wait(Duration::from_secs(5));
            assert_eq!(d.fleet().now_claim_wait(), Duration::from_secs(5));
        }

        /// A claim that fails a `--now` task instead of taking it (its sets
        /// cannot be built) ends the run's wait at once, and the run says it
        /// did not start rather than answering the failed task.
        #[tokio::test(start_paused = true)]
        async fn a_claim_that_fails_a_now_task_answers_the_run_at_once() {
            let (d, tmp) = pull_daemon(1).await;
            keep_workdir_bundle(&d, tmp.path(), "repo-notes");
            claim(&d, 1, false).await;
            d.fleet().set_reply_wait(Duration::from_secs(30));
            let mut req = run_with_sets("urgent", "Check.\n", &["repo-notes"]);
            if let IpcRequest::Run { now, .. } = &mut req {
                *now = true;
            }
            let fail = async {
                crate::bundle::store::remove(&d.paths, "repo-notes").unwrap();
                claim_with_sets(&d, 1).await
            };
            let started = tokio::time::Instant::now();
            let (resp, got) = tokio::join!(d.handle(req), once_now_is_queued(&d, fail));
            assert!(got.is_empty(), "{got:?}");
            assert!(
                started.elapsed() < NOW_CLAIM_RECHECK,
                "woken by the claim: {:?}",
                started.elapsed()
            );
            let IpcResponse::Error { code, message } = resp else {
                panic!("{resp:?}")
            };
            assert_eq!(code, "now_not_started");
            assert!(message.contains("workdir_failed"), "{message}");
        }

        /// A claim that takes a `--now` task between the run's last read
        /// and its close wins: the run answers the task, starting where it
        /// was claimed, not that it is closed.
        #[tokio::test]
        async fn a_claim_that_beats_the_close_is_the_answer() {
            let (d, _tmp) = pull_daemon(1).await;
            // Read as queued, then claimed before the close.
            let t = queued(&d, "urgent", Some("laptop")).await;
            let read = d.store.get_task(t.id).unwrap().unwrap();
            assert_eq!(read.state, TaskState::Queued);
            assert_eq!(claim(&d, 1, false).await, ["urgent"]);
            let IpcResponse::Task(answer) = d.close_unstarted_now(read, "no claim") else {
                panic!()
            };
            assert_eq!(answer.state, TaskState::Starting);
            assert_eq!(answer.machine.as_deref(), Some("laptop"));
        }

        /// A `--now` task no claim takes in time is closed, as one a push
        /// machine did not take: here the machine is cordoned after it was
        /// queued, and a cordon holds a claim's `--now` task too.
        #[tokio::test(start_paused = true)]
        async fn a_now_task_no_claim_takes_is_closed() {
            let (d, _tmp) = pull_daemon(1).await;
            claim(&d, 1, false).await;
            d.fleet().set_reply_wait(Duration::from_millis(500));
            let held = async {
                d.handle(IpcRequest::MachineCordon {
                    name: "laptop".into(),
                    reason: None,
                })
                .await;
                claim(&d, 1, false).await
            };
            let (resp, got) = tokio::join!(
                d.handle(run_now("urgent", Some("laptop"))),
                once_now_is_queued(&d, held)
            );
            assert!(got.is_empty(), "{got:?}");
            let IpcResponse::Error { code, message } = resp else {
                panic!("{resp:?}")
            };
            assert_eq!(code, "now_not_started");
            assert!(message.contains("laptop did not claim it"), "{message}");
            let rows = d.store.list_tasks(&TaskFilter::default()).unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].state, TaskState::Closed);
        }

        /// Only a person may skip the queue: an agent, an orchestrator
        /// task and an orchestrator's script are refused.
        #[tokio::test]
        async fn only_a_person_runs_a_task_now() {
            let (d, _tmp) = daemon(&[("a", 1, FakeHerdr::new())]).await;
            let mut req = run_on("orchestrate", None);
            if let IpcRequest::Run { role, .. } = &mut req {
                *role = TaskRole::Orchestrator;
            }
            let IpcResponse::Task(orch) = d.handle(req).await else {
                panic!()
            };
            for caller in [
                crate::ipc::Caller::task(Some("t-9")),
                crate::ipc::Caller::task(Some(&orch.display_id())),
                crate::ipc::Caller {
                    task: None,
                    orchestrator: Some("merge".into()),
                },
            ] {
                let resp = d.handle_as(run_now("x", Some("a")), &caller).await;
                assert_eq!(error_code(resp), "now_refused", "{caller:?}");
            }
            assert_eq!(d.store.list_tasks(&TaskFilter::default()).unwrap().len(), 1);
        }

        /// A `--now` task whose pinned machine will not take it (its tags
        /// do not match) never waits in the queue: it is closed at once,
        /// and that close is announced like any other, not left silent
        /// (GPT review on #103, finding 3).
        #[tokio::test]
        async fn a_now_task_that_cannot_start_is_closed_and_announced() {
            let (d, _tmp) = daemon(&[("a", 1, FakeHerdr::new())]).await;
            let mut req = run_now("urgent", Some("a"));
            if let IpcRequest::Run { spec, .. } = &mut req {
                spec.tags = vec!["gpu".into()];
            }
            let mut events = d.subscribe();
            let resp = d.handle(req).await;
            assert_eq!(error_code(resp), "now_not_started");
            let rows = d.store.list_tasks(&TaskFilter::default()).unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].state, TaskState::Closed);
            let events = events_of(&mut events);
            assert!(
                events
                    .iter()
                    .any(|(kind, id, _)| kind == "task.closed" && *id == Some(rows[0].id)),
                "{events:?}"
            );
        }

        /// The head sets `spec.now` from `Run::now` alone: a spec that
        /// claims it without the flag queues like any other task.
        #[tokio::test]
        async fn a_spec_that_says_now_without_the_flag_queues() {
            let (d, _tmp) = daemon(&[("a", 1, FakeHerdr::new())]).await;
            queued(&d, "busy", Some("a")).await;
            let mut req = run_on("sneaky", Some("a"));
            if let IpcRequest::Run { spec, .. } = &mut req {
                spec.now = true;
            }
            let IpcResponse::Task(t) = d.handle(req).await else {
                panic!()
            };
            assert_eq!(t.state, TaskState::Queued);
            assert!(!t.spec.now);
        }
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
            now: false,
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
            now: false,
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
            ..Default::default()
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
            now: false,
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
        flock.flocks[1].agent = Some("claude-alt".into());
        flock.flocks[1].deny = vec!["WebFetch".into()];
        let (d, _tmp) = daemon_with_flock(
            flock,
            &[("h", 2, FakeHerdr::new()), ("w", 2, FakeHerdr::new())],
        )
        .await;
        let mut config = test_config();
        config.agents.0.insert(
            "claude-alt".into(),
            crate::config::AgentDef {
                kind: Some("claude".into()),
                ..Default::default()
            },
        );
        // `task run` reads pastor.toml afresh, as the CLI's edit would land.
        std::fs::write(d.paths.config_file(), toml::to_string(&config).unwrap()).unwrap();
        let resp = d
            .handle(IpcRequest::Run {
                now: false,
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
        assert_eq!(t.spec.agent, "claude-alt");
        assert_eq!(t.spec.deny, vec!["WebFetch"]);
    }

    /// pastor.toml with `sonnet` and `opus` for claude and `gpt` for codex,
    /// and `claude-alt`, a claude.
    fn models_config() -> PastorConfig {
        let mut c = test_config();
        c.agents.0.insert(
            "claude-alt".into(),
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
    /// `claude-alt` with `-v` and the model `flock_model`, and
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
                agent: Some("claude-alt".into()),
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
            now: false,
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
        assert_eq!(t.spec.agent, "claude-alt");
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
        // The tool every Claude task is allowed, then the session.
        assert_eq!(args[3], "--allowedTools");
        assert_eq!(args[4], crate::config::TASK_DONE_ALLOW);
        assert_eq!(args[5], "--session-id");
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
        assert_eq!(t.spec.agent, "claude-alt");
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
        assert_eq!(t.spec.agent, "claude-alt");
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

    /// A flock that names an agent sets its kind, and each machine runs
    /// its own agent of that kind: an unpinned task skips a machine that
    /// has none and waits, saying why, when no machine is left; pinned to
    /// such a machine, it is refused.
    #[tokio::test]
    async fn a_flocks_agent_kind_runs_on_the_machines_agent_of_that_kind() {
        use crate::config::flock::FlockEntry;
        let own = |name: &str, extra: crate::config::KindAgents| MachineConfig {
            agent: Some("claude-alt".into()),
            agents: extra,
            ..machine(name, 1)
        };
        let flock = Flock {
            flocks: vec![FlockEntry {
                name: "personal".into(),
                default: true,
                agent: Some("codex".into()),
                ..Default::default()
            }],
            machines: vec![
                own("pi", Default::default()),
                own(
                    "cx",
                    [("codex".to_string(), "codex-work".to_string())].into(),
                ),
            ],
        };
        let (d, _tmp) = daemon_with_flock(
            flock,
            &[("pi", 1, FakeHerdr::new()), ("cx", 1, FakeHerdr::new())],
        )
        .await;
        let mut config = models_config();
        config.agents.0.insert(
            "codex-work".into(),
            crate::config::AgentDef {
                kind: Some("codex".into()),
                ..Default::default()
            },
        );
        std::fs::write(d.paths.config_file(), toml::to_string(&config).unwrap()).unwrap();
        d.fleet().set_config(&config);
        let IpcResponse::Task(t) = d.handle(run_model(None, None, None)).await else {
            panic!()
        };
        assert_eq!(
            (t.machine.as_deref(), t.spec.agent.as_str()),
            (Some("cx"), "codex-work")
        );
        // cx is full, and pi has no codex agent.
        let IpcResponse::Task(t) = d.handle(run_model(None, None, None)).await else {
            panic!()
        };
        assert_eq!(t.state, TaskState::Queued);
        let err = t.error.unwrap_or_default();
        assert!(err.starts_with("waiting for a machine"), "{err}");
        assert!(err.contains("machine pi has none"), "{err}");
        let resp = d.handle(run_model(None, None, Some("pi"))).await;
        assert_eq!(error_code(resp), "agent_kind_missing");
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

    /// `models_daemon` whose flock falls back to sonnet then gpt, over pi,
    /// which falls back to opus, and cx, which sets `[]`; `models_config`
    /// is the fleet's too, for a job's tasks, which re-read nothing.
    async fn fallback_daemon() -> (Daemon, tempfile::TempDir) {
        let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let pi = MachineConfig {
            fallback: Some(names(&["opus"])),
            ..machine("pi", 2)
        };
        let cx = MachineConfig {
            fallback: Some(vec![]),
            ..machine("cx", 2)
        };
        let (d, tmp) = models_daemon(
            None,
            vec![pi, cx, machine("bare", 2)],
            &[
                ("pi", 2, FakeHerdr::new()),
                ("cx", 2, FakeHerdr::new()),
                ("bare", 2, FakeHerdr::new()),
            ],
        )
        .await;
        let mut flock = Flock::load(&d.paths.flock_file()).unwrap();
        flock.flocks[0].fallback = Some(names(&["sonnet", "gpt"]));
        flock.save(&d.paths.flock_file()).unwrap();
        d.fleet().replace_flock(flock).await;
        d.fleet().set_config(&models_config());
        (d, tmp)
    }

    fn run_fallback(
        model: Option<&str>,
        fallback: Option<&[&str]>,
        machine: Option<&str>,
    ) -> IpcRequest {
        let IpcRequest::Run {
            preempt,
            summary,
            role,
            description,
            prompt,
            spec,
            flock,
            agent,
            priority,
            now,
        } = run_model(model, None, machine)
        else {
            unreachable!()
        };
        IpcRequest::Run {
            preempt,
            summary,
            role,
            description,
            prompt,
            spec,
            flock,
            agent: agent.map(|a| AgentChoice {
                fallback: fallback.map(|f| f.iter().map(|s| s.to_string()).collect()),
                ..a
            }),
            priority,
            now,
        }
    }

    /// The fallback list a task names is kept with where it came from, less
    /// the task's own model; `task describe` shows it.
    #[tokio::test]
    async fn a_task_keeps_the_fallback_it_names() {
        let (d, _tmp) = fallback_daemon().await;
        let run = |model, fallback| d.handle(run_fallback(model, Some(fallback), Some("bare")));
        let IpcResponse::Task(t) = run(Some("opus"), &["sonnet", "gpt"]).await else {
            panic!()
        };
        assert_eq!(t.fallback(), ["sonnet", "gpt"]);
        let source = t.spec.agent_source.clone().unwrap();
        assert_eq!(source.fallback_from.as_deref(), Some("task run"));
        let text = crate::cli::task_detail(&t);
        assert!(
            text.contains("fallback:   sonnet, gpt (from task run)\n"),
            "{text}"
        );
        let IpcResponse::Task(t) = run(Some("sonnet"), &["sonnet", "gpt"]).await else {
            panic!()
        };
        assert_eq!(t.fallback(), ["gpt"]);
        let IpcResponse::Task(t) = run(None, &[]).await else {
            panic!()
        };
        assert!(t.fallback().is_empty());
    }

    /// With no list of its own, a task takes its machine's, else its
    /// flock's; a machine's `[]` gives it none although the flock has one.
    #[tokio::test]
    async fn a_machines_fallback_beats_its_flocks() {
        let (d, _tmp) = fallback_daemon().await;
        let placed = |machine| {
            let d = &d;
            async move {
                let IpcResponse::Task(t) = d.handle(run_fallback(None, None, Some(machine))).await
                else {
                    panic!()
                };
                let from = t.spec.agent_source.as_ref().unwrap().fallback_from.clone();
                (t.fallback().to_vec(), from)
            }
        };
        assert_eq!(
            placed("pi").await,
            (vec!["opus".to_string()], Some("machine pi".into()))
        );
        assert_eq!(placed("cx").await, (vec![], Some("machine cx".into())));
        assert_eq!(
            placed("bare").await,
            (
                vec!["sonnet".to_string(), "gpt".to_string()],
                Some("flock personal".into())
            )
        );
    }

    /// `models_daemon` over `pi` and `pi-2`, whose flock runs
    /// `claude-alt`, logged in to the account `me` when `account`; and
    /// `desk`, whose codex agent runs `gpt`, with room for one each.
    async fn limits_daemon(account: bool) -> (Daemon, tempfile::TempDir) {
        let desk = MachineConfig {
            agents: [("codex".to_string(), "codex".to_string())].into(),
            ..machine("desk", 1)
        };
        let (d, tmp) = models_daemon(
            None,
            vec![machine("pi", 1), machine("pi-2", 1), desk],
            &[
                ("pi", 1, FakeHerdr::new()),
                ("pi-2", 1, FakeHerdr::new()),
                ("desk", 1, FakeHerdr::new()),
            ],
        )
        .await;
        let mut config = models_config();
        if account {
            config.agents.0.get_mut("claude-alt").unwrap().account = Some("me".into());
        }
        std::fs::write(d.paths.config_file(), toml::to_string(&config).unwrap()).unwrap();
        d.fleet().set_config(&config);
        (d, tmp)
    }

    /// A limit read on `machine`'s `claude-alt` by task 9, under the
    /// key its config gives, for `model` alone when set; it resets in an
    /// hour.
    fn seen_limit(d: &Daemon, machine: &str, model: Option<&str>) -> AccountLimit {
        let now = chrono::Utc::now();
        let limit = crate::limit::Limit {
            hard: true,
            until: Some(now + chrono::Duration::hours(1)),
            model_scoped: model.is_some(),
            no_credit: false,
            signed_out: false,
            line: "You've hit your session limit".into(),
        };
        let key = d.fleet().agents().limit_key(machine, "claude-alt");
        AccountLimit::of(
            &key,
            model,
            &limit,
            Some(9),
            Some(machine),
            Some("claude-alt"),
            now,
            &Default::default(),
        )
    }

    /// Two machines whose agent names one account share its limit: one
    /// row, seen on pi, keeps a new task off pi-2 too, and the task says why.
    #[tokio::test]
    async fn machines_whose_agent_names_one_account_share_its_limit() {
        let (d, _tmp) = limits_daemon(true).await;
        let mut events = d.subscribe();
        d.fleet().record_limit(&seen_limit(&d, "pi", None)).unwrap();
        d.fleet()
            .record_limit(&seen_limit(&d, "pi-2", None))
            .unwrap();
        let rows = d.store.limits().unwrap();
        assert_eq!(rows.len(), 1, "one account, one row: {rows:?}");
        assert_eq!(rows[0].account, "me");
        let ev = events.recv().await.unwrap();
        assert_eq!(ev.kind, "agent.exhausted");
        assert_eq!(ev.detail.as_ref().unwrap()["account"], "me");
        let IpcResponse::Task(t) = d
            .handle(run_fallback(Some("sonnet"), Some(&[]), None))
            .await
        else {
            panic!()
        };
        assert_eq!(t.state, TaskState::Queued);
        let err = t.error.clone().unwrap_or_default();
        assert!(err.starts_with("waiting: me exhausted until "), "{err}");
        assert!(err.ends_with("(session limit, seen by t-9)"), "{err}");
        let IpcResponse::Queue(q) = d
            .handle(IpcRequest::Queue {
                flock: None,
                machine: None,
            })
            .await
        else {
            panic!()
        };
        assert_eq!(q[0].why, err);
    }

    /// A waiting task stays so while its limit holds, whatever room its
    /// machine has; `limit clear` wakes it on the next pass, back on its
    /// machine with its session. `task retry` refuses it.
    #[tokio::test]
    async fn a_waiting_task_goes_on_when_its_limit_is_cleared() {
        let (d, _tmp) = limits_daemon(true).await;
        d.fleet().record_limit(&seen_limit(&d, "pi", None)).unwrap();
        let IpcResponse::Task(t) = d
            .handle(run_fallback(Some("sonnet"), Some(&[]), Some("pi")))
            .await
        else {
            panic!()
        };
        assert_eq!(t.state, TaskState::Queued);
        let mut t = d.store.claim_task(t.id, "pi").unwrap().unwrap();
        t.state = TaskState::Waiting;
        t.agent_name = None;
        t.error = Some("me exhausted until 03:00 (session limit, seen by t-9)".into());
        t.spec.session_id = crate::task::new_session_id();
        t.waiting_until = d.store.limits().unwrap().first().map(|l| l.retry_at);
        d.store.update_task(&mut t).unwrap();

        d.fleet().dispatch_queued().await;
        let get = |d: &Daemon, id: i64| d.store.get_task(id).unwrap().unwrap();
        assert_eq!(get(&d, t.id).state, TaskState::Waiting, "its limit holds");
        let IpcResponse::Error { code, .. } = d
            .handle(IpcRequest::TaskRetry {
                id: t.id,
                place: None,
            })
            .await
        else {
            panic!()
        };
        assert_eq!(code, TASK_WAITING);

        let IpcResponse::Limits(gone) = d
            .handle(IpcRequest::LimitClear {
                account: "me".into(),
                model: None,
            })
            .await
        else {
            panic!()
        };
        assert_eq!(gone.len(), 1);
        d.fleet().dispatch_queued().await;
        let back = get(&d, t.id);
        assert_eq!(back.state, TaskState::Running, "{:?}", back.error);
        assert_eq!(back.machine.as_deref(), Some("pi"));
        assert_eq!(back.waiting_until, None);
        assert_eq!(back.spec.session_id, t.spec.session_id);
    }

    /// A signed-out mark seen on pi's `claude-alt`, by task 9.
    fn seen_signed_out(d: &Daemon, machine: &str) -> AccountLimit {
        let key = d.fleet().agents().signed_out_key(machine, "claude-alt");
        AccountLimit::of(
            &key,
            None,
            &crate::limit::Limit::signed_out("Select login method:"),
            Some(9),
            Some(machine),
            Some("claude-alt"),
            chrono::Utc::now(),
            &Default::default(),
        )
    }

    /// An agent found signed out keeps new tasks off it, with
    /// `agent.signed_out` and a note that says so; no time clears it. Its
    /// login check passing clears it (`agent.signed_in`, `by: check`) and
    /// wakes the task it held; one failing leaves it, `signed_out`.
    #[tokio::test]
    async fn a_signed_out_agent_holds_its_tasks_until_it_signs_in() {
        let (d, _tmp) = limits_daemon(false).await;
        let mut events = d.subscribe();
        d.fleet().record_limit(&seen_signed_out(&d, "pi")).unwrap();
        let ev = events.recv().await.unwrap();
        assert_eq!(ev.kind, "agent.signed_out");
        assert_eq!(ev.machine.as_deref(), Some("pi"));
        assert_eq!(ev.task_id, Some(9));
        let detail = ev.detail.unwrap();
        assert_eq!(detail["account"], "pi/claude-alt");
        assert_eq!(detail["agent"], "claude-alt");
        assert_eq!(detail["signed_out"], true);
        let IpcResponse::Task(t) = d
            .handle(run_fallback(Some("sonnet"), Some(&[]), Some("pi")))
            .await
        else {
            panic!()
        };
        assert_eq!(t.state, TaskState::Queued);
        let err = t.error.clone().unwrap_or_default();
        assert_eq!(err, "waiting: pi/claude-alt signed out (seen by t-9)");
        let mut t = d.store.claim_task(t.id, "pi").unwrap().unwrap();
        t.state = TaskState::Waiting;
        t.agent_name = None;
        t.waiting_until = Some(crate::limit::signed_out_until());
        d.store.update_task(&mut t).unwrap();
        d.fleet().expire_limits();
        d.fleet().dispatch_queued().await;
        let get = |d: &Daemon, id: i64| d.store.get_task(id).unwrap().unwrap();
        assert_eq!(get(&d, t.id).state, TaskState::Waiting, "still signed out");

        let fake = FakeHerdr::new();
        let agents = d.fleet().agents();
        let harnesses = d.fleet().harnesses();
        let pi = d.fleet().flock().get("pi").cloned().unwrap();
        fake.set_command_answer(Some(false));
        let e = crate::agent_cli::login_check(&fake, &agents, &harnesses, &pi, "claude-alt")
            .await
            .unwrap_err();
        assert_eq!(e.code, crate::agent_cli::SIGNED_OUT);
        fake.set_command_answer(Some(true));
        crate::agent_cli::login_check(&fake, &agents, &harnesses, &pi, "claude-alt")
            .await
            .unwrap();
        let gone = d
            .fleet()
            .clear_signed_out("pi/claude-alt", "check")
            .unwrap();
        assert_eq!(gone.len(), 1);
        let mut signed_in = None;
        while let Ok(ev) = events.try_recv() {
            if ev.kind == "agent.signed_in" {
                signed_in = ev.detail;
            }
        }
        assert_eq!(signed_in.expect("agent.signed_in")["by"], "check");
        d.fleet().dispatch_queued().await;
        let back = get(&d, t.id);
        assert_eq!(back.state, TaskState::Running, "{:?}", back.error);
        assert_eq!(back.machine.as_deref(), Some("pi"));
    }

    /// A sign-out holds the agent on its machine only, even when it names
    /// an account other machines share: pi-2's login is its own.
    #[tokio::test]
    async fn a_sign_out_holds_only_its_machine() {
        let (d, _tmp) = limits_daemon(true).await;
        let IpcResponse::Task(seen) = d
            .handle(run_fallback(Some("sonnet"), Some(&[]), Some("pi-2")))
            .await
        else {
            panic!()
        };
        let now = chrono::Utc::now();
        let out = crate::limit::Limit::signed_out("Select login method:");
        let row = d.fleet().note_limit(&seen, "pi", &out, now).unwrap();
        assert_eq!(row.account, "pi/claude-alt", "not the account `me`");
        let holding = |m: &str| {
            d.fleet()
                .limit_holding(m, "claude-alt", Some("sonnet"), now)
        };
        assert_eq!(holding("pi"), Some(row));
        assert_eq!(holding("pi-2"), None);
        let IpcResponse::Task(t) = d
            .handle(run_fallback(Some("sonnet"), Some(&[]), Some("pi")))
            .await
        else {
            panic!()
        };
        assert_eq!(t.state, TaskState::Queued);
        let err = t.error.unwrap_or_default();
        assert!(err.contains("pi/claude-alt signed out"), "{err}");
    }

    /// `limit clear` on a signed-out mark says `agent.signed_in` by hand;
    /// clearing signed-out marks leaves the account's usage limits alone.
    #[tokio::test]
    async fn a_signed_out_mark_clears_by_hand_and_alone() {
        let (d, _tmp) = limits_daemon(false).await;
        d.fleet().record_limit(&seen_signed_out(&d, "pi")).unwrap();
        let mut events = d.subscribe();
        let IpcResponse::Limits(gone) = d
            .handle(IpcRequest::LimitClear {
                account: "pi/claude-alt".into(),
                model: None,
            })
            .await
        else {
            panic!()
        };
        assert_eq!(gone.len(), 1);
        let ev = events.recv().await.unwrap();
        assert_eq!(ev.kind, "agent.signed_in");
        assert_eq!(ev.detail.unwrap()["by"], "hand");

        d.fleet().record_limit(&seen_limit(&d, "pi", None)).unwrap();
        assert!(
            d.fleet()
                .clear_signed_out("pi/claude-alt", "check")
                .unwrap()
                .is_empty()
        );
        assert_eq!(d.store.limits().unwrap().len(), 1, "the usage limit stays");
    }

    /// A daemon over machine `pi` (its fake returned) with `models_config`,
    /// whose logins read the pane every 20ms, and `claude-alt` marked
    /// signed out there.
    async fn login_daemon() -> (Daemon, tempfile::TempDir, FakeHerdr) {
        let fake = FakeHerdr::new();
        let (d, tmp) =
            models_daemon(None, vec![machine("pi", 1)], &[("pi", 1, fake.clone())]).await;
        let mut config = models_config();
        config.agents.0.get_mut("claude-alt").unwrap().env =
            [("CLAUDE_CONFIG_DIR".to_string(), "~/.claude-alt".to_string())].into();
        std::fs::write(d.paths.config_file(), toml::to_string(&config).unwrap()).unwrap();
        d.fleet().set_config(&config);
        d.logins().set_poll(Duration::from_millis(20));
        fake.set_home(Some("/home/me"));
        fake.set_command_answer(Some(false));
        d.fleet().record_limit(&seen_signed_out(&d, "pi")).unwrap();
        (d, tmp, fake)
    }

    fn login_start(machine: &str, agent: &str) -> IpcRequest {
        IpcRequest::LoginStart {
            machine: machine.into(),
            agent: agent.into(),
        }
    }

    /// The next event of `kind`, within five seconds.
    async fn next_event(events: &mut broadcast::Receiver<PastorEvent>, kind: &str) -> PastorEvent {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let ev = events.recv().await.unwrap();
                if ev.kind == kind {
                    return ev;
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no {kind} event"))
    }

    /// Wait up to five seconds for `done`.
    async fn until(mut done: impl FnMut() -> bool, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn typed(fake: &FakeHerdr, pane: &str) -> Vec<crate::herdr::fake::PaneInput> {
        fake.pane_input(pane)
    }

    /// Claude: `pastor login` opens `login/<agent>` with the agent's env,
    /// types `claude auth login`, and answers with the URL the pane shows
    /// and that it waits for a code. `login code` types the code back. Once
    /// the check passes, the pane closes, the mark clears with
    /// `agent.signed_in` (`by: login`) and the login is gone from the list.
    #[tokio::test]
    async fn pastor_login_reads_claudes_url_and_clears_the_mark_once_signed_in() {
        let (d, _tmp, fake) = login_daemon().await;
        let mut events = d.subscribe();
        fake.set_pane_text(
            "w1:p1",
            "Opening browser to sign in…\nIf the browser didn't open, visit: https://claude.ai/oauth/authorize?code=true&state=s1\nPaste code here if prompted > \n",
        );
        let IpcResponse::Login(started) = d.handle(login_start("pi", "claude-alt")).await else {
            panic!()
        };
        let l = started.pending.expect("a pending login");
        assert_eq!(l.pane, "w1:p1");
        assert_eq!(
            l.url.as_deref(),
            Some("https://claude.ai/oauth/authorize?code=true&state=s1")
        );
        assert_eq!(l.code, None);
        assert!(l.paste);
        let created = fake
            .requests()
            .into_iter()
            .find(|r| r.method == "workspace.create")
            .unwrap();
        assert_eq!(created.params["label"], "login/claude-alt");
        assert_eq!(
            fake.pane_env("w1:p1")["CLAUDE_CONFIG_DIR"],
            "/home/me/.claude-alt"
        );
        use crate::herdr::fake::PaneInput;
        let input = typed(&fake, "w1:p1");
        let PaneInput::Text(line) = &input[0] else {
            panic!("{input:?}")
        };
        assert!(line.starts_with("sh -c "), "{line}");
        assert!(line.contains("exec claude auth login"), "{line}");
        assert_eq!(input[1], PaneInput::Keys(vec!["Enter".into()]));
        let ev = next_event(&mut events, "login.pending").await;
        assert_eq!(ev.machine.as_deref(), Some("pi"));
        assert_eq!(ev.detail.unwrap()["url"], l.url.clone().unwrap());
        let IpcResponse::Logins(list) = d.handle(IpcRequest::LoginList).await else {
            panic!()
        };
        assert_eq!(list, vec![l.clone()]);
        // A second `pastor login` answers the same login, with no new pane.
        let IpcResponse::Login(again) = d.handle(login_start("pi", "claude-alt")).await else {
            panic!()
        };
        assert_eq!(again.pending, Some(l));

        let IpcResponse::Logins(_) = d
            .handle(IpcRequest::LoginCode {
                machine: "pi".into(),
                agent: "claude-alt".into(),
                code: "  abc#123  \n".into(),
            })
            .await
        else {
            panic!()
        };
        let input = typed(&fake, "w1:p1");
        assert_eq!(
            input[2..],
            [
                PaneInput::Text("abc#123".into()),
                PaneInput::Keys(vec!["Enter".into()])
            ]
        );
        assert!(
            d.store.limits().unwrap().iter().any(|l| l.signed_out),
            "still signed out until the check passes"
        );

        fake.set_command_answer(Some(true));
        let ev = next_event(&mut events, "agent.signed_in").await;
        assert_eq!(ev.detail.unwrap()["by"], "login");
        until(|| d.logins().list().is_empty(), "the login to end").await;
        assert!(fake.panes("w1").is_empty(), "the pane is closed");
        assert!(d.store.limits().unwrap().is_empty(), "the mark cleared");
        let ran = fake.commands_run();
        assert!(
            ran.iter().all(|c| c.contains("claude auth status")),
            "{ran:?}"
        );
    }

    /// agy opens on its list of ways to sign in: pastor presses Enter once
    /// there, then reads the URL agy draws wrapped in its box.
    #[tokio::test]
    async fn pastor_login_picks_agys_way_in_and_reads_its_wrapped_url() {
        let (d, _tmp, fake) = login_daemon().await;
        fake.set_pane_text(
            "w1:p1",
            " Welcome to the Antigravity CLI. You are currently not signed in.\n Select login method:\n > 1. Google OAuth\n   2. Use a Google Cloud project\n",
        );
        use crate::herdr::fake::PaneInput;
        let enter = PaneInput::Keys(vec!["Enter".into()]);
        let url = "https://accounts.google.com/o/oauth2/auth?access_type=offline&client_id=1234567890-abcdefghij.apps.googleusercontent.com&redirect_uri=https%3A%2F%2Fantigravity.google%2Foauth-callback&state=xyz";
        let boxed: String = url
            .as_bytes()
            .chunks(77)
            .map(|c| format!(" {:<77}\n", std::str::from_utf8(c).unwrap()))
            .collect();
        let drive = async {
            until(
                || {
                    typed(&fake, "w1:p1")
                        .iter()
                        .filter(|i| **i == enter)
                        .count()
                        == 2
                },
                "the pick",
            )
            .await;
            fake.set_pane_text(
                "w1:p1",
                &format!(" Signing in...Your browser should open automatically. If not:\n\n{boxed}\n  (1-20 of 29 lines)\n"),
            );
        };
        let (reply, ()) = tokio::join!(d.handle(login_start("pi", "agy")), drive);
        let IpcResponse::Login(started) = reply else {
            panic!()
        };
        let l = started.pending.unwrap();
        assert_eq!(l.url.as_deref(), Some(url));
        assert!(!l.paste);
        let input = typed(&fake, "w1:p1");
        assert_eq!(
            input.iter().filter(|i| **i == enter).count(),
            2,
            "the pick is pressed once: {input:?}"
        );
        let PaneInput::Text(line) = &input[0] else {
            panic!()
        };
        assert!(line.contains("exec agy"), "{line}");
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            fake.commands_run().iter().any(|c| c.contains("agy models")),
            "agy's check is its manifest's"
        );
    }

    /// A harness of the user's own with a device code: the code is read
    /// after its label, and `login.pending` carries it.
    #[tokio::test]
    async fn pastor_login_reads_a_device_code() {
        let (d, _tmp, fake) = login_daemon().await;
        let manifest = crate::harness_manifest::Manifest::parse(
            "id = \"codex\"\nbinary = \"codex\"\n[login]\ncheck = [\"codex\", \"login\", \"status\"]\nstart = [\"codex\", \"login\", \"--device-auth\"]\ncode_after = \"Enter this one-time code\"\n[paths]\nhome = \"~/.codex\"\n",
        )
        .unwrap();
        d.fleet().set_harnesses(Arc::new(
            crate::harness_manifest::Harnesses::builtin()
                .clone()
                .with(manifest),
        ));
        let mut events = d.subscribe();
        fake.set_pane_text(
            "w1:p1",
            "1. Open this link in your browser and sign in to your account\n   https://auth.openai.com/codex/device\n2. Enter this one-time code (expires in 15 minutes)\n   ABCD-12345\n",
        );
        let IpcResponse::Login(started) = d.handle(login_start("pi", "codex")).await else {
            panic!()
        };
        let l = started.pending.unwrap();
        assert_eq!(
            l.url.as_deref(),
            Some("https://auth.openai.com/codex/device")
        );
        assert_eq!(l.code.as_deref(), Some("ABCD-12345"));
        let ev = next_event(&mut events, "login.pending").await;
        assert_eq!(ev.detail.unwrap()["code"], "ABCD-12345");
        assert!(
            fake.commands_run()
                .iter()
                .all(|c| c.contains("codex login status")),
            "the manifest's check"
        );
    }

    /// A pane closed before the agent signs in ends the login with
    /// `login.closed` and leaves the mark; so does `login cancel`, which
    /// closes the pane itself.
    #[tokio::test]
    async fn a_login_closed_without_signing_in_keeps_the_mark() {
        let (d, _tmp, fake) = login_daemon().await;
        let mut events = d.subscribe();
        fake.set_pane_text("w1:p1", "visit: https://claude.ai/oauth/authorize?x=1\n");
        let IpcResponse::Login(_) = d.handle(login_start("pi", "claude-alt")).await else {
            panic!()
        };
        fake.close_pane("w1:p1");
        let ev = next_event(&mut events, "login.closed").await;
        assert_eq!(ev.detail.unwrap()["why"], "the pane closed");
        assert!(d.logins().list().is_empty());
        assert!(d.store.limits().unwrap().iter().any(|l| l.signed_out));

        fake.set_pane_text("w2:p1", "visit: https://claude.ai/oauth/authorize?x=2\n");
        let IpcResponse::Login(started) = d.handle(login_start("pi", "claude-alt")).await else {
            panic!()
        };
        assert_eq!(started.pending.unwrap().pane, "w2:p1");
        let IpcResponse::Logins(gone) = d
            .handle(IpcRequest::LoginCancel {
                machine: "pi".into(),
                agent: "claude-alt".into(),
            })
            .await
        else {
            panic!()
        };
        assert_eq!(gone.len(), 1);
        let ev = next_event(&mut events, "login.closed").await;
        assert_eq!(ev.detail.unwrap()["why"], "cancelled");
        assert!(fake.panes("w2").is_empty());
        assert!(d.store.limits().unwrap().iter().any(|l| l.signed_out));
        assert_eq!(
            error_code(
                d.handle(IpcRequest::LoginCancel {
                    machine: "pi".into(),
                    agent: "claude-alt".into(),
                })
                .await
            ),
            crate::login::NOT_PENDING
        );
    }

    /// An agent whose check passes already gets no pane: its mark clears.
    #[tokio::test]
    async fn pastor_login_of_a_signed_in_agent_only_clears_its_mark() {
        let (d, _tmp, fake) = login_daemon().await;
        fake.set_command_answer(Some(true));
        let IpcResponse::Login(started) = d.handle(login_start("pi", "claude-alt")).await else {
            panic!()
        };
        assert_eq!(started.pending, None);
        assert_eq!(started.cleared.len(), 1);
        assert!(
            !fake
                .requests()
                .iter()
                .any(|r| r.method == "workspace.create")
        );
        assert!(d.store.limits().unwrap().is_empty());
    }

    /// What pastor cannot sign in is refused before anything opens.
    #[tokio::test]
    async fn pastor_login_refuses_what_it_cannot_sign_in() {
        let (d, _tmp, fake) = login_daemon().await;
        // Codex's built-in manifest names no interactive login.
        assert_eq!(
            error_code(d.handle(login_start("pi", "codex")).await),
            "no_login_start"
        );
        assert_eq!(
            error_code(d.handle(login_start("nope", "claude")).await),
            "unknown_machine"
        );
        fake.set_command_answer(None);
        assert_eq!(
            error_code(d.handle(login_start("pi", "claude")).await),
            "check_unknown"
        );
        assert!(
            !fake
                .requests()
                .iter()
                .any(|r| r.method == "workspace.create")
        );
    }

    /// The card's walk: `fallback = ["sonnet", "gpt5"]` and limits with no
    /// reset near. Opus runs out on its own (`Opus weekly limit`): the task
    /// goes on under Sonnet in its own session with the switch line. Then
    /// the whole account runs out: it goes on under the machine's opencode
    /// with its prompt, the handover and the end of the last pane. Same
    /// worktree and branch each time, one row, one round per model. Once
    /// Opus is cleared, the task stays on the model it moved to, and a new
    /// task starts on Opus again.
    #[tokio::test]
    async fn a_limited_task_moves_down_its_fallback_list() {
        let pi = MachineConfig {
            agents: [("opencode".to_string(), "opencode".to_string())].into(),
            ..machine("pi", 3)
        };
        let fake = FakeHerdr::new();
        let (d, _tmp) = opencode_daemon(vec![pi], &[("pi", 3, fake.clone())]).await;
        let mut config = models_config();
        config.models.0.insert(
            "gpt5".into(),
            crate::config::ModelDef {
                kind: "opencode".into(),
                args: vec!["--model".into(), "openai/gpt-5.5".into()],
            },
        );
        config.agents.0.get_mut("claude-alt").unwrap().account = Some("me".into());
        std::fs::write(d.paths.config_file(), toml::to_string(&config).unwrap()).unwrap();
        d.fleet().set_config(&config);
        let mut events = d.subscribe();
        let IpcRequest::Run {
            preempt,
            summary,
            role,
            description,
            prompt,
            mut spec,
            flock,
            agent,
            priority,
            now,
        } = run_fallback(Some("opus"), Some(&["sonnet", "gpt5"]), Some("pi"))
        else {
            unreachable!()
        };
        spec.repo = Some("/r".into());
        spec.worktree = true;
        let run = IpcRequest::Run {
            preempt,
            summary,
            role,
            description,
            prompt,
            spec,
            flock,
            agent,
            priority,
            now,
        };
        let IpcResponse::Task(t) = d.handle(run).await else {
            panic!()
        };
        let get = |id: i64| d.store.get_task(id).unwrap().unwrap();
        let until = async |what: &str, f: &dyn Fn(&Task) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let t = get(t.id);
                if f(&t) {
                    return t;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for {what}: {t:?}"
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        let first = until("running", &|t| t.state == TaskState::Running).await;
        assert_eq!(first.model(), Some("opus"));
        let checkout = first.spec.checkout.clone().expect("a worktree");
        let session = first.spec.session_id.clone().expect("a Claude session");
        let stop_on = |pane: &str, line: &str| {
            fake.set_status(pane, crate::herdr::AgentStatus::Working);
            fake.set_pane_text(
                pane,
                &format!("❯ x\n\n● Half of it is done.\n\n● {line}\n\n────────\n❯ \n────────\n"),
            );
        };
        let idle = |pane: &str| fake.set_status(pane, crate::herdr::AgentStatus::Idle);
        let last = |method: &str| {
            fake.requests()
                .into_iter()
                .rev()
                .find(|r| r.method == method)
                .unwrap()
                .params
        };

        // Opus runs out alone: Sonnet, in the same session.
        let pane = first.pane_id.clone().unwrap();
        stop_on(&pane, "Opus weekly limit reached");
        tokio::time::sleep(Duration::from_millis(300)).await;
        idle(&pane);
        until("moved on", &|t| t.state == TaskState::Waiting).await;
        d.fleet().dispatch_queued().await;
        let second = until("running on sonnet", &|t| {
            t.state == TaskState::Running && t.model() == Some("sonnet")
        })
        .await;
        assert_eq!(second.spec.session_id.as_deref(), Some(session.as_str()));
        assert_eq!(second.spec.checkout, Some(checkout.clone()));
        let start = last("agent.start");
        let args = start["args"].to_string();
        assert!(
            args.contains("claude-sonnet-5") && args.contains(&session),
            "{args}"
        );
        assert_eq!(
            last("agent.prompt")["text"],
            crate::task::switch_prompt("claude-alt (opus)", "claude-alt (sonnet)")
        );
        assert_eq!(second.spec.rounds.ended.len(), 1);
        assert_eq!(
            second.spec.rounds.ended[0].ended,
            "limit: Opus weekly limit reached"
        );

        // Opus is back: the task stays on Sonnet, a new one starts on Opus.
        let IpcResponse::Limits(_) = d
            .handle(IpcRequest::LimitClear {
                account: "me".into(),
                model: Some("opus".into()),
            })
            .await
        else {
            panic!()
        };
        let IpcResponse::Task(other) = d
            .handle(run_fallback(Some("opus"), Some(&["sonnet"]), Some("pi")))
            .await
        else {
            panic!()
        };
        assert_eq!(other.model(), Some("opus"));
        assert_eq!(other.spec.agent_source.unwrap().fallback_use, None);
        d.fleet().dispatch_queued().await;
        assert_eq!(get(t.id).model(), Some("sonnet"));

        // The whole account runs out: opencode, from the prompt with the
        // handover and the end of the last pane.
        let pane = second.pane_id.clone().unwrap();
        stop_on(&pane, "You've hit your limit");
        tokio::time::sleep(Duration::from_millis(300)).await;
        idle(&pane);
        until("moved on", &|t| t.state == TaskState::Waiting).await;
        d.fleet().dispatch_queued().await;
        let third = until("running on gpt5", &|t| {
            t.state == TaskState::Running && t.model() == Some("gpt5")
        })
        .await;
        assert_eq!(third.spec.agent, "opencode");
        assert_eq!(third.spec.checkout, Some(checkout.clone()));
        assert_eq!(last("agent.start")["kind"], "opencode");
        let text = last("agent.prompt")["text"].as_str().unwrap().to_string();
        assert!(text.starts_with("x"), "{text}");
        assert!(
            text.contains("another agent (claude-alt (sonnet))"),
            "{text}"
        );
        assert!(text.contains("Half of it is done."), "{text}");
        assert!(text.contains("You've hit your limit"), "{text}");
        let summaries = d.store.summaries(t.id).unwrap();
        let texts: Vec<_> = summaries.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "limit: Opus weekly limit reached",
                "limit: You've hit your limit"
            ]
        );

        let mut switched = Vec::new();
        while let Ok(ev) = events.try_recv() {
            if ev.kind == "task.agent_switched" && ev.task_id == Some(t.id) {
                switched.push(ev.detail.unwrap());
            }
        }
        assert_eq!(switched.len(), 2, "{switched:?}");
        assert_eq!(switched[0]["from"]["model"], "opus");
        assert_eq!(switched[0]["to"]["model"], "sonnet");
        assert_eq!(switched[0]["why"], "limit");
        assert_eq!(switched[0]["handover"], "session");
        assert!(switched[0]["until"].is_string(), "{:?}", switched[0]);
        assert_eq!(switched[1]["to"]["agent"], "opencode");
        assert_eq!(switched[1]["handover"], "pane_tail");
    }

    /// A task that was to move on finds every model after its own
    /// exhausted: it waits, until the earliest reset of them, with
    /// `all_exhausted`; at that time it starts on the first free one.
    #[tokio::test]
    async fn a_task_with_every_fallback_exhausted_waits_for_the_earliest_reset() {
        let (d, _tmp) = limits_daemon(true).await;
        let mut events = d.subscribe();
        let IpcResponse::Task(t) = d
            .handle(run_fallback(Some("opus"), Some(&["sonnet"]), Some("pi")))
            .await
        else {
            panic!()
        };
        // The whole account is out, as the actor left it: to move on now.
        d.fleet().record_limit(&seen_limit(&d, "pi", None)).unwrap();
        let mut t = d.store.get_task(t.id).unwrap().unwrap();
        t.state = TaskState::Waiting;
        t.pane_id = None;
        t.waiting_until = Some(chrono::Utc::now());
        t.spec.rounds.stop = Some(Box::new(crate::task::LimitStop {
            agent: t.spec.agent.clone(),
            model: Some("opus".into()),
            why: "limit".into(),
            line: "You've hit your session limit".into(),
            until: chrono::Utc::now() + chrono::Duration::hours(1),
            tail: String::new(),
            then: crate::task::NextRound::Switch,
            waited: None,
            handover: None,
        }));
        d.store.update_task(&mut t).unwrap();
        d.fleet().dispatch_queued().await;
        let back = d.store.get_task(t.id).unwrap().unwrap();
        assert_eq!(back.state, TaskState::Waiting);
        let limit = d.store.limits().unwrap()[0].retry_at;
        assert_eq!(back.waiting_until, Some(limit));
        assert_eq!(
            back.spec.rounds.stop.as_ref().map(|s| s.then),
            Some(crate::task::NextRound::First)
        );
        let waiting = std::iter::from_fn(|| events.try_recv().ok())
            .find(|e| e.kind == "task.waiting")
            .and_then(|e| e.detail)
            .unwrap();
        assert_eq!(waiting["why"], "all_exhausted");
        assert_eq!(waiting["until"], serde_json::json!(limit));
        assert_eq!(waiting["models"][0]["account"], "me");
    }

    /// Task on pi that moved from opus to sonnet, the last model of its
    /// list, and stopped there on a limit: to move on now, with `limits`
    /// (each a model and the minutes to its reset) in the table.
    async fn limited_on_the_last_model(d: &Daemon, limits: &[(&str, i64)]) -> Task {
        let IpcResponse::Task(t) = d
            .handle(run_fallback(Some("opus"), Some(&["sonnet"]), Some("pi")))
            .await
        else {
            panic!()
        };
        let now = chrono::Utc::now();
        for (model, mins) in limits {
            let mut row = seen_limit(d, "pi", Some(model));
            row.retry_at = now + chrono::Duration::minutes(*mins);
            row.until = Some(row.retry_at);
            d.fleet().record_limit(&row).unwrap();
        }
        let mut t = d.store.get_task(t.id).unwrap().unwrap();
        t.state = TaskState::Waiting;
        t.pane_id = None;
        t.waiting_until = Some(now);
        t.spec.agent_source.as_mut().unwrap().model = Some("sonnet".into());
        t.spec.rounds.ended.push(crate::task::AgentRound {
            agent: t.spec.agent.clone(),
            model: Some("opus".into()),
            ended: "limit: Opus weekly limit reached".into(),
        });
        t.spec.rounds.stop = Some(Box::new(crate::task::LimitStop {
            agent: t.spec.agent.clone(),
            model: Some("sonnet".into()),
            why: "limit".into(),
            line: "You've hit your session limit".into(),
            until: now + chrono::Duration::hours(1),
            tail: String::new(),
            then: crate::task::NextRound::Switch,
            waited: None,
            handover: None,
        }));
        d.store.update_task(&mut t).unwrap();
        t
    }

    /// A task on the last model of its list has no model after its own,
    /// but the ones before it are still its models: it waits with
    /// `all_exhausted` until the earliest reset among all of them, here
    /// opus's, which comes before sonnet's.
    #[tokio::test]
    async fn a_task_on_its_last_model_waits_for_the_earliest_reset_of_its_list() {
        let (d, _tmp) = limits_daemon(true).await;
        let mut events = d.subscribe();
        let t = limited_on_the_last_model(&d, &[("opus", 20), ("sonnet", 60)]).await;
        d.fleet().dispatch_queued().await;
        let back = d.store.get_task(t.id).unwrap().unwrap();
        assert_eq!(back.state, TaskState::Waiting);
        let opus = d
            .store
            .limits()
            .unwrap()
            .into_iter()
            .find(|l| l.model.as_deref() == Some("opus"))
            .unwrap()
            .retry_at;
        assert_eq!(back.waiting_until, Some(opus));
        let waiting = std::iter::from_fn(|| events.try_recv().ok())
            .find(|e| e.kind == "task.waiting")
            .and_then(|e| e.detail)
            .unwrap();
        assert_eq!(waiting["why"], "all_exhausted");
        assert_eq!(waiting["until"], serde_json::json!(opus));
        let mut models: Vec<_> = waiting["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["model"].as_str().unwrap().to_string())
            .collect();
        models.sort();
        assert_eq!(models, vec!["opus", "sonnet"]);
    }

    /// A task on the last model of its list whose first model is free
    /// again goes on under it at once instead of waiting for nothing.
    #[tokio::test]
    async fn a_task_on_its_last_model_goes_back_to_a_free_earlier_one() {
        let (d, _tmp) = limits_daemon(true).await;
        let t = limited_on_the_last_model(&d, &[("sonnet", 60)]).await;
        d.fleet().dispatch_queued().await;
        let back = d.store.get_task(t.id).unwrap().unwrap();
        assert_ne!(back.state, TaskState::Waiting, "{:?}", back.error);
        assert_eq!(back.model(), Some("opus"));
    }

    /// A task whose list has no other model its machine can run (pi has
    /// no codex agent for gpt) waits on its own model, as a task with no
    /// list does: `no_fallback`, with its account and model, until its
    /// own reset, and it resumes on that model.
    #[tokio::test]
    async fn a_task_with_no_fallback_its_machine_runs_waits_on_its_own_model() {
        let (d, _tmp) = limits_daemon(true).await;
        let mut events = d.subscribe();
        let IpcResponse::Task(t) = d
            .handle(run_fallback(Some("opus"), Some(&["sonnet"]), Some("pi")))
            .await
        else {
            panic!()
        };
        let now = chrono::Utc::now();
        let mut row = seen_limit(&d, "pi", Some("opus"));
        row.retry_at = now + chrono::Duration::minutes(40);
        row.until = Some(row.retry_at);
        d.fleet().record_limit(&row).unwrap();
        let mut t = d.store.get_task(t.id).unwrap().unwrap();
        t.state = TaskState::Waiting;
        t.pane_id = None;
        t.waiting_until = Some(now);
        t.spec.agent_source.as_mut().unwrap().fallback = vec!["gpt".into()];
        t.spec.rounds.stop = Some(Box::new(crate::task::LimitStop {
            agent: t.spec.agent.clone(),
            model: Some("opus".into()),
            why: "limit".into(),
            line: "You've hit your session limit".into(),
            until: now + chrono::Duration::hours(1),
            tail: String::new(),
            then: crate::task::NextRound::Switch,
            waited: None,
            handover: None,
        }));
        d.store.update_task(&mut t).unwrap();
        d.fleet().dispatch_queued().await;
        let back = d.store.get_task(t.id).unwrap().unwrap();
        assert_eq!(back.state, TaskState::Waiting);
        let opus = d.store.limits().unwrap()[0].retry_at;
        assert_eq!(back.waiting_until, Some(opus));
        assert_eq!(back.model(), Some("opus"));
        assert_eq!(
            back.spec.rounds.stop.as_ref().map(|s| s.then),
            Some(crate::task::NextRound::Own)
        );
        let waiting = std::iter::from_fn(|| events.try_recv().ok())
            .find(|e| e.kind == "task.waiting")
            .and_then(|e| e.detail)
            .unwrap();
        assert_eq!(waiting["why"], "no_fallback");
        assert_eq!(waiting["account"], "me");
        assert_eq!(waiting["model"], "opus");
        assert_eq!(waiting["until"], serde_json::json!(opus));
        assert!(waiting.get("models").is_none(), "{waiting}");
    }

    /// Tasks on pi that wait for a limit, each until `in_mins` from now,
    /// as `wait_on_limit` leaves them. The limit that kept them queued is
    /// then taken out of the table behind the fleet's back, as though it
    /// never reached it: only their own time holds them.
    async fn waiting_on_pi(d: &Daemon, in_mins: &[i64]) -> Vec<Task> {
        d.fleet().record_limit(&seen_limit(d, "pi", None)).unwrap();
        let now = chrono::Utc::now();
        let mut waiting = Vec::new();
        for mins in in_mins {
            let IpcResponse::Task(t) = d
                .handle(run_fallback(Some("sonnet"), Some(&[]), Some("pi")))
                .await
            else {
                panic!()
            };
            let mut t = d.store.claim_task(t.id, "pi").unwrap().unwrap();
            t.state = TaskState::Waiting;
            t.error = Some("me exhausted (session limit, seen by t-9)".into());
            t.spec.session_id = crate::task::new_session_id();
            t.waiting_until = Some(now + chrono::Duration::minutes(*mins));
            d.store.update_task(&mut t).unwrap();
            waiting.push(t);
        }
        d.store.clear_limits("me", None).unwrap();
        d.fleet().limits_changed();
        waiting
    }

    /// A waiting task whose limit never reached the table (or was read
    /// before it did) still waits for its own `waiting_until`: no live
    /// limit is not enough to resume it early.
    #[tokio::test]
    async fn a_waiting_task_waits_for_its_time_with_no_limit_in_the_table() {
        let (d, _tmp) = limits_daemon(true).await;
        let t = waiting_on_pi(&d, &[60]).await.remove(0);
        d.fleet().dispatch_queued().await;
        let get = |id: i64| d.store.get_task(id).unwrap().unwrap();
        assert_eq!(get(t.id).state, TaskState::Waiting);
    }

    /// A waiting task goes on by itself once its reset passes: the pass
    /// drops the row (`agent.reset`, `by: time`) and resumes it on its
    /// machine with its session.
    #[tokio::test]
    async fn a_waiting_task_goes_on_at_its_reset() {
        let (d, _tmp) = limits_daemon(true).await;
        let mut events = d.subscribe();
        let t = waiting_on_pi(&d, &[-1]).await.remove(0);
        let mut row = seen_limit(&d, "pi", None);
        row.retry_at = t.waiting_until.unwrap();
        row.until = Some(row.retry_at);
        d.store.record_limit(&row).unwrap();
        d.fleet().limits_changed();

        d.fleet().dispatch_queued().await;
        let back = d.store.get_task(t.id).unwrap().unwrap();
        assert_eq!(back.state, TaskState::Running, "{:?}", back.error);
        assert_eq!(back.machine.as_deref(), Some("pi"));
        assert_eq!(back.spec.session_id, t.spec.session_id);
        assert!(d.store.limits().unwrap().is_empty());
        let mut by_time = false;
        while let Ok(e) = events.try_recv() {
            by_time |=
                e.kind == "agent.reset" && e.detail.as_ref().is_some_and(|d| d["by"] == "time");
        }
        assert!(by_time, "the reset is said");
    }

    /// Two tasks wait on pi, which has room for one: the soonest reset
    /// goes on, whatever the order they were made in, and the other keeps
    /// waiting for the slot.
    #[tokio::test]
    async fn waiting_tasks_on_a_full_machine_go_on_soonest_first() {
        let (d, _tmp) = limits_daemon(true).await;
        let w = waiting_on_pi(&d, &[-1, -2]).await;
        let (later, sooner) = (&w[0], &w[1]);
        d.fleet().dispatch_queued().await;
        let get = |id: i64| d.store.get_task(id).unwrap().unwrap();
        assert_eq!(get(sooner.id).state, TaskState::Running);
        assert_eq!(get(later.id).state, TaskState::Waiting);
    }

    /// With no account, a limit holds only for the machine it was seen on:
    /// two machines give two rows, and a task still starts on a third.
    #[tokio::test]
    async fn an_agent_with_no_account_keeps_a_limit_per_machine() {
        let (d, _tmp) = limits_daemon(false).await;
        d.fleet().record_limit(&seen_limit(&d, "pi", None)).unwrap();
        let IpcResponse::Task(t) = d
            .handle(run_fallback(Some("sonnet"), Some(&[]), None))
            .await
        else {
            panic!()
        };
        assert_eq!(t.machine.as_deref(), Some("pi-2"), "{:?}", t.error);
        d.fleet()
            .record_limit(&seen_limit(&d, "pi-2", None))
            .unwrap();
        let keys: Vec<String> = d
            .store
            .limits()
            .unwrap()
            .into_iter()
            .map(|l| l.account)
            .collect();
        assert_eq!(keys, ["pi-2/claude-alt", "pi/claude-alt"]);
    }

    /// Every model of the account exhausted: the task starts on the first
    /// free model of its fallback list, and describe says which and why.
    #[tokio::test]
    async fn a_task_starts_on_its_first_free_fallback() {
        let (d, _tmp) = limits_daemon(true).await;
        d.fleet().record_limit(&seen_limit(&d, "pi", None)).unwrap();
        let IpcResponse::Task(t) = d
            .handle(run_fallback(Some("sonnet"), Some(&["opus", "gpt"]), None))
            .await
        else {
            panic!()
        };
        assert_eq!(
            (t.machine.as_deref(), t.spec.agent.as_str(), t.model()),
            (Some("desk"), "codex", Some("gpt")),
            "{:?}",
            t.error
        );
        assert_eq!(t.error, None);
        assert_eq!(t.fallback(), ["opus", "gpt"]);
        let used = t
            .spec
            .agent_source
            .as_ref()
            .unwrap()
            .fallback_use
            .clone()
            .unwrap();
        assert_eq!((used.pos, used.of), (2, 2));
        let text = crate::cli::task_detail(&t);
        assert!(
            text.contains("model:      gpt (fallback 2 of 2; me exhausted until "),
            "{text}"
        );
        assert_eq!(used.why.matches("me exhausted").count(), 1, "{}", used.why);
    }

    /// A limit that is one model's stops only that model: the task goes on
    /// under the next one, on the same account and machine.
    #[tokio::test]
    async fn a_model_scoped_limit_stops_only_that_model() {
        let (d, _tmp) = limits_daemon(true).await;
        d.fleet()
            .record_limit(&seen_limit(&d, "pi", Some("sonnet")))
            .unwrap();
        let IpcResponse::Task(t) = d
            .handle(run_fallback(Some("sonnet"), Some(&["opus"]), None))
            .await
        else {
            panic!()
        };
        assert_eq!(
            (t.spec.agent.as_str(), t.model()),
            ("claude-alt", Some("opus"))
        );
        assert_eq!(t.spec.agent_args[..2], ["--model", "claude-opus-5-5"]);
        let IpcResponse::Task(t) = d.handle(run_fallback(Some("opus"), Some(&[]), None)).await
        else {
            panic!()
        };
        assert_eq!(t.model(), Some("opus"), "opus is free");
        assert!(t.machine.is_some(), "{:?}", t.error);
    }

    /// Clearing the account lets the task it held start on the next pass,
    /// with `agent.reset` by hand; a row past its retry goes on its own,
    /// with `agent.reset` by time.
    #[tokio::test]
    async fn a_cleared_or_expired_limit_lets_the_task_start() {
        let (d, _tmp) = limits_daemon(true).await;
        d.fleet().record_limit(&seen_limit(&d, "pi", None)).unwrap();
        let IpcResponse::Task(t) = d
            .handle(run_fallback(Some("sonnet"), Some(&[]), None))
            .await
        else {
            panic!()
        };
        assert_eq!(t.state, TaskState::Queued);
        let mut events = d.subscribe();
        assert_eq!(d.fleet().clear_limits("me", None).unwrap().len(), 1);
        let ev = events.recv().await.unwrap();
        assert_eq!(ev.kind, "agent.reset");
        assert_eq!(ev.detail.as_ref().unwrap()["by"], "hand");
        d.fleet().dispatch_queued().await;
        let t = d.store.get_task(t.id).unwrap().unwrap();
        assert_ne!(t.state, TaskState::Queued);
        assert_eq!(t.error, None);

        let mut past = seen_limit(&d, "pi", None);
        past.retry_at = chrono::Utc::now() - chrono::Duration::seconds(1);
        d.store.record_limit(&past).unwrap();
        d.fleet().dispatch_queued().await;
        assert!(d.store.limits().unwrap().is_empty());
        let ev = loop {
            let ev = events.recv().await.unwrap();
            if ev.kind == "agent.reset" {
                break ev;
            }
        };
        assert_eq!(ev.detail.as_ref().unwrap()["by"], "time");
    }

    /// A fallback name `[models]` lacks is refused by `task run`, and by a
    /// retry once it is dropped from `[models]`.
    #[tokio::test]
    async fn an_unknown_fallback_is_refused_on_run_and_retry() {
        let (d, _tmp) = fallback_daemon().await;
        let resp = d
            .handle(run_fallback(None, Some(&["sonnet", "haiku"]), Some("bare")))
            .await;
        match resp {
            IpcResponse::Error { code, message } => {
                assert_eq!(code, "unknown_model", "{message}");
                assert!(message.contains("model haiku"), "{message}");
            }
            other => panic!("{other:?}"),
        }
        let IpcResponse::Task(mut t) = d
            .handle(run_fallback(None, Some(&["opus"]), Some("bare")))
            .await
        else {
            panic!()
        };
        t.state = TaskState::Failed;
        t.finished_at = Some(chrono::Utc::now());
        d.store.update_task(&mut t).unwrap();
        let mut config = models_config();
        config.models.0.remove("opus");
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
        assert_eq!(copy.fallback(), ["opus"]);
    }

    /// A job's fallback renders per item: an item with none leaves the
    /// task no list although its flock has one, and a name `[models]`
    /// lacks refuses the item.
    #[tokio::test]
    async fn a_jobs_fallback_renders_per_item_and_is_checked() {
        let (d, _tmp) = fallback_daemon().await;
        let text = "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nmachine = \"bare\"\nfallback = [\"{{ item.fallback }}\"]\nprompt = \"p\"\n";
        let job = crate::config::job::Job::parse(
            text,
            "j",
            &models_config().defaults,
            &crate::connector::Builtins,
        )
        .unwrap();
        let queue = |item: serde_json::Value| {
            let job = &job;
            let d = &d;
            async move {
                d.fleet()
                    .queue_job_task(job, &item, |_| Ok(("p".into(), job.spec.clone())))
                    .await
            }
        };
        let t = queue(serde_json::json!({"key": "k1"})).await.unwrap();
        assert!(t.fallback().is_empty());
        assert_eq!(
            t.spec
                .agent_source
                .as_ref()
                .unwrap()
                .fallback_from
                .as_deref(),
            Some("job j")
        );
        let t = queue(serde_json::json!({"key": "k2", "fallback": "gpt"}))
            .await
            .unwrap();
        assert_eq!(t.fallback(), ["gpt"]);
        let err = queue(serde_json::json!({"key": "k3", "fallback": "haiku"}))
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("model haiku is not in [models]"),
            "{err:#}"
        );
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
            now: false,
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
            now: false,
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

    /// A profiled Codex task runs with no approval prompts in its
    /// workspace-write sandbox, not refused for its lists, which it keeps
    /// and `task describe` marks as not applied.
    #[tokio::test]
    async fn a_codex_task_runs_under_a_profile_in_its_sandbox() {
        let fake = FakeHerdr::new();
        let (d, _tmp) = profiles_daemon(vec![machine("pi", 2)], &[("pi", 2, fake.clone())]).await;
        let IpcRequest::Run {
            agent: Some(choice),
            ..
        } = run_profile(Some("ci"), None)
        else {
            unreachable!()
        };
        let IpcRequest::Run {
            now,
            preempt,
            summary,
            prompt,
            spec,
            flock,
            priority,
            role,
            description,
            ..
        } = run_profile(Some("ci"), None)
        else {
            unreachable!()
        };
        let run = IpcRequest::Run {
            now,
            preempt,
            summary,
            prompt,
            spec,
            flock,
            priority,
            role,
            description,
            agent: Some(AgentChoice {
                agent: Some("codex".into()),
                ..choice
            }),
        };
        let resp = d.handle(run).await;
        let IpcResponse::Task(t) = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(t.spec.agent, "codex");
        assert!(t.spec.deny.contains(&"Bash(sudo:*)".to_string()));
        let why = t
            .spec
            .agent_source
            .as_ref()
            .unwrap()
            .lists_unapplied
            .clone();
        assert!(
            why.as_deref().unwrap().contains("workspace-write"),
            "{why:?}"
        );
        let reqs = fake.requests();
        let start = reqs.iter().find(|r| r.method == "agent.start").unwrap();
        let args: Vec<&str> = start.params["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        assert!(
            args.ends_with(&[
                "--ask-for-approval",
                "never",
                "--sandbox",
                "workspace-write"
            ]),
            "{args:?}"
        );
        let text = crate::cli::task_detail(&t);
        assert!(
            text.contains("(not applied: codex has no per-command"),
            "{text}"
        );
    }

    /// End to end: a profiled agy task starts with the flags that stop it
    /// asking and `task describe` says its lists are not passed; on a
    /// machine whose agy settings lack a rule it fails before `agent.start`,
    /// naming the rule; `unrestricted`, which denies nothing, runs no probe;
    /// with `own_permissions` nothing is added or checked.
    #[tokio::test]
    async fn an_agy_task_under_a_profile_starts_without_asking_or_fails_naming_the_rule() {
        let run_agy = |profile: Option<&str>| {
            let IpcRequest::Run {
                now,
                preempt,
                summary,
                prompt,
                spec,
                flock,
                priority,
                role,
                description,
                agent: Some(choice),
            } = run_profile(profile, None)
            else {
                unreachable!()
            };
            IpcRequest::Run {
                now,
                preempt,
                summary,
                prompt,
                spec,
                flock,
                priority,
                role,
                description,
                agent: Some(AgentChoice {
                    agent: Some("agy".into()),
                    ..choice
                }),
            }
        };
        let fake = FakeHerdr::new();
        let (d, _tmp) = profiles_daemon(vec![machine("pi", 4)], &[("pi", 4, fake.clone())]).await;
        let IpcResponse::Task(t) = d.handle(run_agy(Some("develop"))).await else {
            panic!()
        };
        assert_eq!(t.profile(), Some("develop"));
        let reqs = fake.requests();
        let start = reqs.iter().find(|r| r.method == "agent.start").unwrap();
        assert_eq!(start.params["kind"], "agy");
        let args: Vec<&str> = start.params["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        assert_eq!(
            &args[args.len() - 3..],
            ["--dangerously-skip-permissions", "--mode", "accept-edits"]
        );
        for d in &t.spec.deny {
            assert!(!args.contains(&d.as_str()), "{args:?}");
        }
        let text = crate::cli::task_detail(&t);
        assert!(text.contains("(agy takes no lists:"), "{text}");

        fake.set_agy_settings(Some(crate::provision::agy::AgySettings {
            content: Some(r#"{"permissions":{"deny":["command(sudo)"]}}"#.into()),
            link: false,
        }));
        let before = fake
            .requests()
            .iter()
            .filter(|r| r.method == "agent.start")
            .count();
        let IpcResponse::Task(t) = d.handle(run_agy(Some("develop"))).await else {
            panic!()
        };
        let t = d.store.get_task(t.id).unwrap().unwrap();
        assert_eq!(t.state, TaskState::Failed);
        let err = t.error.clone().unwrap_or_default();
        assert!(err.contains("agy_permissions_missing"), "{err}");
        assert!(err.contains("command(rm -rf)"), "{err}");
        assert!(err.contains("command(git push --force)"), "{err}");
        assert!(!err.contains("command(sudo)"), "{err}");
        assert_eq!(
            fake.requests()
                .iter()
                .filter(|r| r.method == "agent.start")
                .count(),
            before
        );

        // `unrestricted` denies nothing: no probe, no refusal.
        let mut config = profiles_config();
        config.defaults.profile = Some("unrestricted".into());
        apply_config(&d, &config).await;
        fake.set_agy_settings(None);
        let IpcResponse::Task(t) = d.handle(run_agy(Some("unrestricted"))).await else {
            panic!()
        };
        assert_eq!(
            d.store.get_task(t.id).unwrap().unwrap().state,
            TaskState::Running
        );

        // `own_permissions`: its args alone, nothing checked.
        let mut config = profiles_config();
        config.agents.0.insert(
            "agy".into(),
            crate::config::AgentDef {
                own_permissions: true,
                ..Default::default()
            },
        );
        apply_config(&d, &config).await;
        let IpcResponse::Task(t) = d.handle(run_agy(Some("develop"))).await else {
            panic!()
        };
        let reqs = fake.requests();
        let start = reqs.iter().rfind(|r| r.method == "agent.start").unwrap();
        let args: Vec<&str> = start.params["args"]
            .as_array()
            .map(|a| a.iter().map(|a| a.as_str().unwrap()).collect())
            .unwrap_or_default();
        assert_eq!(args, t.spec.agent_args, "{args:?}");
        assert!(crate::cli::task_detail(&t).contains("own settings on the machine"));
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
            now: false,
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
            now: false,
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

    /// With no `head_address`, `machine list` and `machine describe` mark
    /// each `ssh` machine: its agents cannot run `pastor task done`. Setting
    /// it clears the mark; a machine the head reaches another way never has
    /// it.
    #[tokio::test]
    async fn machines_cut_off_from_the_head_say_so() {
        let pi = MachineConfig {
            ssh: Some("user@pi".into()),
            command: None,
            ..machine("pi", 1)
        };
        let (d, _tmp) = models_daemon(
            None,
            vec![pi, machine("box", 1)],
            &[("pi", 1, FakeHerdr::new()), ("box", 1, FakeHerdr::new())],
        )
        .await;
        apply_config(&d, &test_config()).await;
        let cut_off = |ms: &[crate::machine::MachineStatus]| -> Vec<(String, bool)> {
            ms.iter()
                .map(|m| (m.name.clone(), m.no_head_address))
                .collect()
        };
        let IpcResponse::Machines(ms) = d.handle(IpcRequest::FlockList).await else {
            panic!()
        };
        assert_eq!(
            cut_off(&ms),
            [("pi".to_string(), true), ("box".to_string(), false)]
        );
        let IpcResponse::MachineDescription(m) = d
            .handle(IpcRequest::MachineDescribe { name: "pi".into() })
            .await
        else {
            panic!()
        };
        assert!(m.row.no_head_address);
        apply_config(
            &d,
            &PastorConfig {
                head_address: Some("user@head.example".into()),
                ..test_config()
            },
        )
        .await;
        let IpcResponse::Machines(ms) = d.handle(IpcRequest::FlockList).await else {
            panic!()
        };
        assert!(ms.iter().all(|m| !m.no_head_address), "{:?}", cut_off(&ms));
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
        // The label column is as wide as the longest label, so match the
        // field and not its padding.
        assert!(
            text.lines().any(|l| l
                .split_once(':')
                .is_some_and(|(k, v)| k == "profile" && v.trim() == "unrestricted")),
            "{text}"
        );
    }

    /// agy's rules are per machine, so a profiled agy task runs only on a
    /// machine whose own profile denies everything the task's does: an
    /// unpinned `review` task skips a `develop` machine for a `review` one,
    /// pinned to the `develop` machine it is refused, and a machine with no
    /// own profile is tried.
    #[tokio::test]
    async fn a_profiled_agy_task_needs_a_machine_whose_own_profile_covers_it() {
        let dev = MachineConfig {
            profile: Some("develop".into()),
            ..machine("dev", 2)
        };
        let rev = MachineConfig {
            profile: Some("review".into()),
            ..machine("rev", 1)
        };
        let (d, _tmp) = profiles_daemon(
            vec![dev, rev, machine("pi", 2)],
            &[
                ("dev", 2, FakeHerdr::new()),
                ("rev", 1, FakeHerdr::new()),
                ("pi", 2, FakeHerdr::new()),
            ],
        )
        .await;
        let run = |profile: Option<&str>, machine: Option<&str>| {
            let IpcRequest::Run {
                now,
                preempt,
                summary,
                prompt,
                spec,
                flock,
                priority,
                role,
                description,
                agent: Some(choice),
            } = run_profile(profile, machine)
            else {
                unreachable!()
            };
            IpcRequest::Run {
                now,
                preempt,
                summary,
                prompt,
                spec,
                flock,
                priority,
                role,
                description,
                agent: Some(AgentChoice {
                    agent: Some("agy".into()),
                    ..choice
                }),
            }
        };
        let resp = d.handle(run(Some("review"), Some("dev"))).await;
        let IpcResponse::Error { code, message } = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(code, crate::config::agy::AGY_PROFILE_MISMATCH);
        assert!(message.contains("Bash(git push:*)"), "{message}");
        assert!(message.contains("develop"), "{message}");

        let IpcResponse::Task(t) = d.handle(run(Some("review"), None)).await else {
            panic!()
        };
        assert_eq!(t.machine.as_deref(), Some("rev"));
        // rev is full and dev does not cover review; pi has no own profile,
        // so it is tried and the dispatch check decides.
        let IpcResponse::Task(t) = d.handle(run(Some("review"), None)).await else {
            panic!()
        };
        assert_eq!(t.machine.as_deref(), Some("pi"));
        // develop on the develop machine is fine.
        let IpcResponse::Task(t) = d.handle(run(Some("develop"), Some("dev"))).await else {
            panic!()
        };
        assert_eq!(t.machine.as_deref(), Some("dev"));
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
                now: false,
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

    /// `personal` holds `own`, which runs `claude-alt`, and `plain`,
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
                    agent: Some("claude-alt".into()),
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
            now: false,
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
                    "claude-alt".into(),
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
        assert_eq!(kind(&own), "claude-alt");
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
        assert_eq!(t.spec.agent, "claude-alt");
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

    /// A task a pass placed keeps its queued row until the send ends
    /// (`in_flight`). A priority change or a move then would be reported
    /// applied but have no effect on that dispatch, so both are refused
    /// until the send is over.
    #[tokio::test]
    async fn priority_and_move_refuse_a_task_in_flight() {
        let (d, _tmp) = spare_daemon().await;
        let fleet = d.fleet();
        let t = fleet
            .queue_run("x".into(), spec(), Some("spare"), None, None)
            .await
            .unwrap();
        fleet.in_flight.placed.lock().unwrap().push(Placed {
            task_id: t.id,
            machine: "nowhere".into(),
            flock: t.flock.clone(),
            from_job: false,
        });
        let held = Reservation {
            in_flight: fleet.in_flight.clone(),
            task_id: t.id,
        };
        let err = fleet
            .set_priority_preempting(t.id, Priority::Critical, "test", true)
            .await
            .unwrap_err();
        assert!(
            matches!(err, PriorityError::InFlight(id) if id == t.id),
            "{err:?}"
        );
        let err = fleet.move_queued(t.id, QueueSpot::Top).await.unwrap_err();
        assert!(
            matches!(err, MoveError::InFlight(id) if id == t.id),
            "{err:?}"
        );
        drop(held);
        let t = fleet
            .set_priority(t.id, Priority::High, "test")
            .await
            .unwrap();
        assert_eq!(t.priority, Priority::High);
        fleet.move_queued(t.id, QueueSpot::Top).await.unwrap();
    }

    /// A dispatch that outlives `reply_wait` (a slow-to-ready agent; herdr
    /// itself answers every request) keeps its reservation past
    /// `dispatch_queued` returning: the command already sent to the actor's
    /// channel cannot be un-sent, so a priority change or a queue move in
    /// that window must still see the task as in flight, not free to
    /// reprioritize as if the placement had never happened. Once the actor
    /// really answers, the task has moved on to `running`, not back to
    /// `queued`, so the same change is then refused as not queued instead.
    #[tokio::test]
    async fn a_dispatch_past_reply_wait_keeps_its_reservation() {
        let fake = FakeHerdr::new();
        let (fleet, store) = managed(&[("a", fake.clone())]);
        let settings = MachineSettings {
            agent_ready_timeout: Duration::from_millis(1500),
            ..fast()
        };
        fleet.apply_flock(&flock_of(&[("a", 1)]), &settings).await;
        healthy(&fleet, "a").await;
        fake.set_ready_after(Duration::from_millis(100));
        fleet.set_reply_wait(Duration::from_millis(50));

        let t = fleet
            .queue_run("x".into(), spec(), None, None, None)
            .await
            .unwrap();
        fleet.dispatch_queued().await;
        assert_eq!(
            store.get_task(t.id).unwrap().unwrap().state,
            TaskState::Starting,
            "claimed, but the agent is not ready yet: reply_wait gave up long before it is"
        );

        let err = fleet
            .set_priority_preempting(t.id, Priority::Critical, "test", true)
            .await
            .unwrap_err();
        assert!(
            matches!(err, PriorityError::InFlight(id) if id == t.id),
            "{err:?}"
        );
        let err = fleet.move_queued(t.id, QueueSpot::Top).await.unwrap_err();
        assert!(
            matches!(err, MoveError::InFlight(id) if id == t.id),
            "{err:?}"
        );

        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(
            store.get_task(t.id).unwrap().unwrap().state,
            TaskState::Running,
            "the slow dispatch finished on its own past reply_wait"
        );
        assert!(
            matches!(
                fleet
                    .set_priority(t.id, Priority::Low, "test")
                    .await
                    .unwrap_err(),
                PriorityError::NotQueued {
                    state: TaskState::Running,
                    ..
                }
            ),
            "no longer in flight, but not queued either now it is running"
        );
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
            [
                ("work", Some(crate::config::flock::FlockNumber::plain(1))),
                ("spare", Some(crate::config::flock::FlockNumber::plain(2)))
            ]
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
        assert_eq!(
            on_disk().machine_flocks("h").unwrap(),
            [("spare", Some(crate::config::flock::FlockNumber::plain(2)))]
        );

        let said = text(
            d.handle(IpcRequest::MachineRemove {
                name: "w".into(),
                dry_run: false,
            })
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
                    dry_run: false,
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
            .edit_flock_file(
                &file,
                &crate::provision::Catalog::builtin(),
                &crate::bundle::Catalog::default(),
                |f| crate::fleet_edit::move_machine(f, "w", "home"),
            )
            .await
            .unwrap();
        let t = fleet
            .queue_run("x".into(), spec_on("w"), None, None, None)
            .await
            .unwrap();
        assert_eq!(t.flock.as_deref(), Some("home"), "w moved to home");
        fleet
            .edit_flock_file(
                &file,
                &crate::provision::Catalog::builtin(),
                &crate::bundle::Catalog::default(),
                |f| crate::fleet_edit::remove_machine(f, "w"),
            )
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
            .edit_flock_file(
                &file,
                &crate::provision::Catalog::builtin(),
                &crate::bundle::Catalog::default(),
                |f| {
                    let mut flock = Flock::load_existing(f)?;
                    flock.machines[0].model = Some("nope".into());
                    flock.save(f)
                },
            )
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

    /// A machine's loads are read from the flock last applied too: editing
    /// them keeps the actor, as the same edit on a flock does, and the machine's
    /// view carries the new loads.
    #[tokio::test]
    async fn changing_a_machines_loads_keeps_its_actor() {
        let (d, _tmp) = flocked_daemon().await;
        let mut changed = home_and_work();
        changed.machines[0].loads = vec!["rust".into()];
        let diff = d
            .fleet()
            .apply_flock(&changed, &machine_settings(&test_config()))
            .await;
        assert!(diff.is_empty(), "{diff:?}");
        let views = d.fleet().views();
        let h = views.iter().find(|v| v.name == "h").unwrap();
        assert_eq!(h.loads, ["rust"], "{h:?}");
    }

    /// A machine's agent is read by dispatch from the flock last applied,
    /// like its flock: changing it keeps the actor, and the next task on
    /// the machine runs the new agent.
    #[tokio::test]
    async fn changing_a_machines_agent_keeps_its_actor() {
        let (d, _tmp) = flocked_daemon().await;
        let mut changed = home_and_work();
        changed.machines[0].agent = Some("claude-alt".into());
        let diff = d
            .fleet()
            .apply_flock(&changed, &machine_settings(&test_config()))
            .await;
        assert!(diff.is_empty(), "{diff:?}");
        let resp = d
            .handle(IpcRequest::Run {
                now: false,
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
        assert_eq!(t.spec.agent, "claude-alt");
    }

    /// Copilot 4281285656: a provisioner has no runtime behavior yet, so
    /// adding or changing one must not make `apply_flock` replace the
    /// machine's actor and reconnect it.
    #[tokio::test]
    async fn changing_a_machines_provisioner_keeps_its_actor() {
        let (d, _tmp) = flocked_daemon().await;
        let mut changed = home_and_work();
        changed.machines[0].provisioner = Some("arch".into());
        let diff = d
            .fleet()
            .apply_flock(&changed, &machine_settings(&test_config()))
            .await;
        assert!(diff.is_empty(), "{diff:?}");
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
                now: false,
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
                now: false,
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
            &crate::ipc::Caller::default(),
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
            now: false,
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
        assert!(!message.contains("agents_change_fleet"), "{message}");
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

    /// A task's `keep_pane` comes from `task run --keep-pane`, else its
    /// job's, else its flock's, else `[defaults]` (unset here: no layer),
    /// and is stored on the task with where it came from.
    #[tokio::test]
    async fn keep_pane_is_settled_when_a_task_is_queued() {
        let mut flock = home_and_work();
        flock.flocks.push(crate::config::flock::FlockEntry {
            name: "kept".into(),
            keep_pane: Some(true),
            ..Default::default()
        });
        let (d, _tmp) = daemon_with_flock(
            flock,
            &[("h", 2, FakeHerdr::new()), ("w", 2, FakeHerdr::new())],
        )
        .await;
        let settled = |resp: IpcResponse| match resp {
            IpcResponse::Task(t) => (t.spec.keep_pane, t.spec.keep_pane_from),
            other => panic!("{other:?}"),
        };
        assert_eq!(settled(d.handle(run_in(None, None)).await), (None, None));
        assert_eq!(
            settled(d.handle(run_in(Some("kept"), None)).await),
            (Some(true), Some("flock kept".into()))
        );
        let mut asked = run_in(None, None);
        if let IpcRequest::Run { spec, .. } = &mut asked {
            spec.keep_pane = Some(true);
        }
        assert_eq!(
            settled(d.handle(asked).await),
            (Some(true), Some("task run".into()))
        );
        let submit = |job: &str, dispatch: serde_json::Value| IpcRequest::JobSubmit {
            job: job.into(),
            dispatch,
            prompt: "p".into(),
            items: vec![serde_json::json!({"key": "k"})],
        };
        let job_task = |resp: IpcResponse| match resp {
            IpcResponse::JobSubmitted { tasks, .. } => (
                tasks[0].spec.keep_pane,
                tasks[0].spec.keep_pane_from.clone(),
            ),
            other => panic!("{other:?}"),
        };
        let from_flock = d
            .handle(submit("j1", serde_json::json!({"flock": "kept"})))
            .await;
        assert_eq!(
            job_task(from_flock),
            (Some(true), Some("flock kept".into()))
        );
        let from_job = d
            .handle(submit(
                "j2",
                serde_json::json!({"flock": "kept", "keep_pane": false}),
            ))
            .await;
        assert_eq!(job_task(from_job), (Some(false), Some("job j2".into())));
    }

    /// A task's workdir set is checked when it is queued: for its agent's
    /// kind, on a machine the head can place it on. A task pinned to a
    /// machine reached through a command bridge, or in a flock of only such
    /// machines, is refused rather than left to wait; the views keep a set
    /// off such a machine.
    #[tokio::test]
    async fn a_task_s_workdir_set_is_checked_when_it_is_queued() {
        let mut flock = home_and_work();
        // Test machines speak through a command bridge, which runs no shell;
        // h is reached over ssh, so the head can place a set there.
        flock.machines[0].command = None;
        flock.machines[0].ssh = Some("h.example".into());
        let (d, _tmp) = daemon_with_flock(
            flock,
            &[("h", 2, FakeHerdr::new()), ("w", 2, FakeHerdr::new())],
        )
        .await;
        let takes = |name: &str| {
            let views = d.fleet().views();
            views.iter().find(|v| v.name == name).unwrap().workdir_sets
        };
        assert!(takes("h"));
        assert!(!takes("w"));
        let run = |flock: Option<&str>, machine: Option<&str>, agent: &str| {
            let mut req = run_in(flock, machine);
            if let IpcRequest::Run { spec, .. } = &mut req {
                spec.agent = agent.into();
                spec.workdir = Some(Box::new(crate::task::TaskWorkdir {
                    set: serde_json::json!({"id": "reviewer", "kind": "claude", "files": [
                        {"path": ".claude/agents/checker.md", "content": "Check.\n"}]}),
                    placed_in: None,
                    render: None,
                }));
            }
            req
        };
        match d.handle(run(None, None, "claude")).await {
            IpcResponse::Task(t) => {
                assert_eq!(t.spec.workdir.unwrap().set["id"], "reviewer");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            error_code(d.handle(run(None, None, "agy")).await),
            crate::bundle::workdir_task::WORKDIR_KIND_MISMATCH
        );
        assert_eq!(
            error_code(d.handle(run(Some("work"), Some("w"), "claude")).await),
            crate::bundle::workdir::WORKDIR_UNSUPPORTED,
            "pinned to a command machine"
        );
        assert_eq!(
            error_code(d.handle(run(Some("work"), None, "claude")).await),
            crate::bundle::workdir::WORKDIR_UNSUPPORTED,
            "no machine of work takes one"
        );
    }

    /// Write workdir bundle `id` into the bundles dir under `root`, with a
    /// var `var` when given; `home` makes it a home bundle.
    fn write_workdir_bundle(root: &std::path::Path, id: &str, var: Option<&str>, home: bool) {
        let dir = root.join("c/bundles").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.md"), "Notes.\n").unwrap();
        let vars = var.map_or(String::new(), |v| format!("[vars]\n{v} = {{}}\n"));
        let body = if home {
            format!(
                "id = \"{id}\"\nversion = \"1\"\n{vars}[[file]]\nsrc = \"f.md\"\nslot = \"mods\"\n"
            )
        } else {
            format!(
                "id = \"{id}\"\nversion = \"1\"\ntarget = \"workdir\"\n{vars}[[file]]\nsrc = \"f.md\"\npath = \".claude/agents/{id}.md\"\n"
            )
        };
        std::fs::write(dir.join(crate::bundle::MANIFEST_FILE), body).unwrap();
    }

    /// A task's stored workdir bundles are settled when it is queued: its
    /// flock's, then those it asked for, each once and with where each came
    /// from. One it asks for must be a workdir bundle in the bundles dir
    /// whose vars the flock gives, and its own set may not take a listed
    /// bundle's id.
    #[tokio::test]
    async fn a_task_s_workdir_bundles_are_settled_when_it_is_queued() {
        let mut flock = home_and_work();
        flock.machines[0].command = None;
        flock.machines[0].ssh = Some("h.example".into());
        let (d, tmp) = daemon_with_flock(
            flock,
            &[("h", 2, FakeHerdr::new()), ("w", 2, FakeHerdr::new())],
        )
        .await;
        for (id, var, home) in [
            ("repo-notes", None, false),
            ("style", None, false),
            ("base", None, true),
            ("regional", Some("region"), false),
        ] {
            write_workdir_bundle(tmp.path(), id, var, home);
        }
        {
            let fleet = d.fleet();
            let mut wanted = fleet.wanted.write().recover();
            let home = wanted.flocks.iter_mut().find(|f| f.name == "home").unwrap();
            home.workdir_bundles = vec!["repo-notes".into()];
        }
        let run = |ids: &[&str], own: Option<&str>| {
            let mut req = run_in(None, None);
            if let IpcRequest::Run { spec, .. } = &mut req {
                spec.workdir_bundles = ids
                    .iter()
                    .map(|id| crate::task::WorkdirBundle::named(id))
                    .collect();
                spec.workdir = own.map(|id| {
                    Box::new(crate::task::TaskWorkdir {
                        set: serde_json::json!({"id": id, "kind": "claude", "files": [
                            {"path": ".claude/agents/checker.md", "content": "Check.\n"}]}),
                        placed_in: None,
                        render: None,
                    })
                });
            }
            req
        };
        match d.handle(run(&["style", "repo-notes"], None)).await {
            IpcResponse::Task(t) => {
                let got: Vec<(String, Option<String>)> = t
                    .spec
                    .workdir_bundles
                    .iter()
                    .map(|b| (b.id.clone(), b.from.clone()))
                    .collect();
                assert_eq!(
                    got,
                    [
                        ("repo-notes".to_string(), Some("flock home".to_string())),
                        ("style".to_string(), Some("task run".to_string())),
                    ]
                );
            }
            other => panic!("{other:?}"),
        }
        // A flock that names none, and a task that asks for none: none.
        match d.handle(run_in(Some("work"), None)).await {
            IpcResponse::Task(t) => assert!(t.spec.workdir_bundles.is_empty()),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            error_code(d.handle(run(&["base"], None)).await),
            crate::bundle::BUNDLE_INVALID
        );
        assert_eq!(
            error_code(d.handle(run(&["absent"], None)).await),
            crate::bundle::UNKNOWN_BUNDLE
        );
        assert_eq!(
            error_code(d.handle(run(&["regional"], None)).await),
            crate::bundle::BUNDLE_INVALID,
            "flock home gives no region"
        );
        assert_eq!(
            error_code(d.handle(run(&[], Some("repo-notes"))).await),
            crate::bundle::workdir::WORKDIR_CONFLICT
        );
        match d.handle(run(&[], Some("reviewer"))).await {
            IpcResponse::Task(t) => assert_eq!(t.spec.workdir_bundles.len(), 1),
            other => panic!("{other:?}"),
        }
    }

    /// A stored workdir bundle does not keep a task off a pull machine: it
    /// claims the task, which starts without its bundles, marked skipped on
    /// the head with `workdir.skipped`.
    #[tokio::test]
    async fn a_pull_machine_takes_a_task_without_its_workdir_bundles() {
        let (d, tmp) = pull_daemon(1).await;
        write_workdir_bundle(tmp.path(), "repo-notes", None, false);
        let mut req = run_on("p1", Some("laptop"));
        if let IpcRequest::Run { spec, .. } = &mut req {
            spec.workdir_bundles = vec![crate::task::WorkdirBundle::named("repo-notes")];
        }
        let IpcResponse::Task(t) = d.handle(req).await else {
            panic!()
        };
        let mut rx = d.subscribe();
        assert_eq!(claim(&d, 1, false).await, ["p1"]);
        let row = d.store().get_task(t.id).unwrap().unwrap();
        let why = row.spec.workdir_bundles[0].skipped.clone().unwrap();
        assert!(why.contains("laptop is a pull machine"), "{why}");
        let e = rx.try_recv().unwrap();
        assert_eq!(e.kind, "workdir.skipped");
        assert_eq!(e.task_id, Some(t.id));
        assert_eq!(e.detail.unwrap()["ids"], serde_json::json!(["repo-notes"]));
    }

    /// A claim from `laptop` whose serve places workdir sets, and what it
    /// was handed.
    async fn claim_with_sets(d: &Daemon, free_slots: u32) -> Vec<Task> {
        let req = IpcRequest::TaskClaim {
            machine: "laptop".into(),
            free_slots,
            flock_work: false,
            herdr_version: None,
            pastor_version: None,
            workdir_sets: true,
        };
        match d.handle(req).await {
            IpcResponse::Tasks(tasks) => tasks,
            other => panic!("{other:?}"),
        }
    }

    /// A worktree task pinned to `laptop` with its own set of `content` and
    /// the stored bundles `bundles`.
    fn run_with_sets(prompt: &str, content: &str, bundles: &[&str]) -> IpcRequest {
        let mut req = run_on(prompt, Some("laptop"));
        if let IpcRequest::Run { spec, .. } = &mut req {
            spec.repo = Some("~/work/app".into());
            spec.worktree = true;
            spec.workdir = Some(Box::new(crate::task::TaskWorkdir {
                set: serde_json::json!({"id": "reviewer", "kind": "claude", "files": [
                    {"path": ".claude/agents/checker.md", "content": content}]}),
                placed_in: None,
                render: None,
            }));
            spec.workdir_bundles = bundles
                .iter()
                .map(|id| crate::task::WorkdirBundle::named(id))
                .collect();
        }
        req
    }

    /// Keep a version of workdir bundle `id` in the head's store.
    fn keep_workdir_bundle(d: &Daemon, root: &std::path::Path, id: &str) {
        write_workdir_bundle(root, id, None, false);
        let src = root.join("c/bundles").join(id);
        crate::bundle::store::add(&d.paths, &crate::bundle::store::Source::Dir(src)).unwrap();
    }

    /// A task with a set pinned to a pull machine is queued, and waits for
    /// a claim that says its serve places sets: that claim hands it with
    /// its own set settled and each stored bundle's set built for the
    /// agent's kind. The head's row keeps neither; the machine then counts
    /// as one that takes sets.
    #[tokio::test]
    async fn a_pull_machine_that_places_sets_gets_them_with_the_claim() {
        let (d, tmp) = pull_daemon(2).await;
        keep_workdir_bundle(&d, tmp.path(), "repo-notes");
        let IpcResponse::Task(t) = d
            .handle(run_with_sets("p1", "Check.\n", &["repo-notes"]))
            .await
        else {
            panic!("a task with a set pinned to a pull machine is queued")
        };
        assert!(
            claim(&d, 2, false).await.is_empty(),
            "not to a serve that places none"
        );
        let views = d.fleet().views();
        assert!(
            !views
                .iter()
                .find(|v| v.name == "laptop")
                .unwrap()
                .workdir_sets
        );

        let got = claim_with_sets(&d, 2).await;
        assert_eq!(got.len(), 1);
        let w = got[0].spec.workdir.as_deref().unwrap();
        assert_eq!(w.set["id"], "reviewer");
        let built = got[0].spec.workdir_bundles[0].set.as_ref().unwrap();
        assert_eq!(built["id"], "repo-notes");
        assert_eq!(built["kind"], "claude");
        assert_eq!(built["files"][0]["path"], ".claude/agents/repo-notes.md");

        let row = d.store().get_task(t.id).unwrap().unwrap();
        assert_eq!(row.state, TaskState::Starting);
        assert!(
            row.spec.workdir_bundles[0].set.is_none(),
            "never on the head's row"
        );
        assert!(row.spec.workdir_bundles[0].skipped.is_none());
        assert!(row.spec.workdir.unwrap().render.is_none());
        let views = d.fleet().views();
        assert!(
            views
                .iter()
                .find(|v| v.name == "laptop")
                .unwrap()
                .workdir_sets
        );
    }

    /// A task that is not a worktree task runs in a folder others may share,
    /// so a claim marks its stored bundles skipped on the head's row, with
    /// `workdir.skipped`, and builds none for the machine.
    #[tokio::test]
    async fn a_claim_leaves_a_shared_folder_s_bundles_out() {
        let (d, tmp) = pull_daemon(2).await;
        keep_workdir_bundle(&d, tmp.path(), "repo-notes");
        let mut req = run_on("p1", Some("laptop"));
        if let IpcRequest::Run { spec, .. } = &mut req {
            spec.workdir_bundles = vec![crate::task::WorkdirBundle::named("repo-notes")];
        }
        let IpcResponse::Task(t) = d.handle(req).await else {
            panic!()
        };
        let mut rx = d.subscribe();
        let got = claim_with_sets(&d, 2).await;
        assert_eq!(got.len(), 1);
        let handed = &got[0].spec.workdir_bundles[0];
        assert!(handed.set.is_none(), "none built");
        let why = handed.skipped.clone().unwrap();
        assert!(
            why.starts_with("shared folder (its working directory)"),
            "{why}"
        );
        let row = d.store().get_task(t.id).unwrap().unwrap();
        assert_eq!(
            row.spec.workdir_bundles[0].skipped.as_deref(),
            Some(why.as_str())
        );
        let e = rx.try_recv().unwrap();
        assert_eq!(e.kind, "workdir.skipped");
        assert_eq!(e.task_id, Some(t.id));
        assert_eq!(e.detail.unwrap()["ids"], serde_json::json!(["repo-notes"]));

        // The machine's start says the same in its own words: the claim's
        // reason stands, and nothing is raised again.
        let mut again = report("laptop", &t, TaskState::Running, None);
        if let IpcRequest::TaskReport {
            workdir_skipped, ..
        } = &mut again
        {
            *workdir_skipped = vec![crate::task::WorkdirSkip {
                id: "repo-notes".into(),
                why: "shared folder (/home/u)".into(),
            }];
        }
        let IpcResponse::Task(row) = d.handle(again).await else {
            panic!()
        };
        assert_eq!(
            row.spec.workdir_bundles[0].skipped.as_deref(),
            Some(why.as_str())
        );
        assert!(
            !events_of(&mut rx).iter().any(|e| e.0 == "workdir.skipped"),
            "raised once"
        );
    }

    /// A bundle only the pull machine could leave out (here a checkout
    /// that was already open) comes back in its report: the head's row
    /// says so, as `task describe` reads it, with one `workdir.skipped`
    /// before the start's own event, however often the report is sent.
    #[tokio::test]
    async fn a_pull_machine_s_own_workdir_skips_reach_the_head() {
        let (d, tmp) = pull_daemon(2).await;
        keep_workdir_bundle(&d, tmp.path(), "repo-notes");
        let IpcResponse::Task(t) = d
            .handle(run_with_sets("p1", "Check.\n", &["repo-notes"]))
            .await
        else {
            panic!()
        };
        let got = claim_with_sets(&d, 2).await;
        assert!(got[0].spec.workdir_bundles[0].set.is_some());
        let mut rx = d.subscribe();
        let why = "shared folder (/home/u/work/app): stored workdir bundles go only in a task's own worktree";
        let skipped = |state| {
            let mut req = report("laptop", &t, state, None);
            if let IpcRequest::TaskReport {
                workdir_skipped, ..
            } = &mut req
            {
                *workdir_skipped = vec![crate::task::WorkdirSkip {
                    id: "repo-notes".into(),
                    why: why.into(),
                }];
            }
            req
        };
        let IpcResponse::Task(row) = d.handle(skipped(TaskState::Running)).await else {
            panic!()
        };
        assert_eq!(row.spec.workdir_bundles[0].skipped.as_deref(), Some(why));
        let kinds: Vec<String> = events_of(&mut rx).into_iter().map(|e| e.0).collect();
        assert_eq!(kinds, ["workdir.skipped", "task.running"]);
        let row = d.store().get_task(t.id).unwrap().unwrap();
        assert_eq!(row.spec.workdir_bundles[0].skipped.as_deref(), Some(why));
        assert!(row.spec.workdir_bundles[0].set.is_none());

        let IpcResponse::Task(_) = d.handle(skipped(TaskState::Running)).await else {
            panic!()
        };
        assert!(events_of(&mut rx).is_empty(), "sent again: nothing new");
    }

    /// A task whose sets cannot be built when a pull machine claims it, a
    /// bundle no longer kept here, fails on the head `workdir_failed` and
    /// is handed to nobody, rather than starting there without them.
    #[tokio::test]
    async fn a_claimed_task_whose_sets_cannot_be_built_fails_on_the_head() {
        let (d, tmp) = pull_daemon(2).await;
        keep_workdir_bundle(&d, tmp.path(), "repo-notes");
        let IpcResponse::Task(t) = d
            .handle(run_with_sets("p1", "Check.\n", &["repo-notes"]))
            .await
        else {
            panic!()
        };
        crate::bundle::store::remove(&d.paths, "repo-notes").unwrap();
        let mut rx = d.subscribe();
        assert!(claim_with_sets(&d, 2).await.is_empty());
        let row = d.store().get_task(t.id).unwrap().unwrap();
        assert_eq!(row.state, TaskState::Failed);
        let error = row.error.unwrap();
        assert!(
            error.starts_with("workdir_failed: bundle repo-notes"),
            "{error}"
        );
        assert_eq!(rx.try_recv().unwrap().kind, "task.failed");
    }

    /// A claim reply carries at most `MAX_CLAIM_SETS` of sets: a task past
    /// it waits for the next claim, while the first goes whatever its size.
    #[tokio::test]
    async fn a_claim_reply_carries_a_bounded_amount_of_sets() {
        let (d, _tmp) = pull_daemon(4).await;
        let big = "x".repeat(3 * 1024 * 1024);
        for p in ["p1", "p2", "p3"] {
            let IpcResponse::Task(_) = d.handle(run_with_sets(p, &big, &[])).await else {
                panic!()
            };
        }
        let got: Vec<String> = claim_with_sets(&d, 4)
            .await
            .into_iter()
            .map(|t| t.prompt)
            .collect();
        assert_eq!(got, ["p1", "p2"]);
        let got: Vec<String> = claim_with_sets(&d, 4)
            .await
            .into_iter()
            .map(|t| t.prompt)
            .collect();
        assert_eq!(got, ["p3"]);
    }

    /// A machine reached through a command bridge places no sets, so a
    /// task with stored workdir bundles starts there without them, and its
    /// actor raises `workdir.skipped`.
    #[tokio::test]
    async fn a_start_that_leaves_workdir_bundles_out_says_so() {
        let (d, tmp) = pull_daemon(1).await;
        write_workdir_bundle(tmp.path(), "repo-notes", None, false);
        let mut rx = d.subscribe();
        let mut req = run_on("u1", Some("a"));
        if let IpcRequest::Run { spec, .. } = &mut req {
            spec.workdir_bundles = vec![crate::task::WorkdirBundle::named("repo-notes")];
        }
        let IpcResponse::Task(t) = d.handle(req).await else {
            panic!()
        };
        d.fleet().dispatch_queued().await;
        let deadline = Instant::now() + Duration::from_secs(10);
        let skipped = loop {
            match tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
                Ok(Ok(e)) if e.kind == "workdir.skipped" => break e,
                _ => assert!(Instant::now() < deadline, "no workdir.skipped"),
            }
        };
        assert_eq!(skipped.task_id, Some(t.id));
        let why = skipped.detail.unwrap()["why"].as_str().unwrap().to_string();
        assert!(why.contains("a does not place workdir sets"), "{why}");
        let row = d.store().get_task(t.id).unwrap().unwrap();
        assert_ne!(row.state, TaskState::Failed, "{:?}", row.error);
    }

    /// The daemon gives its workdir gate to the actors of the machines the
    /// head can place a set on, and to no other.
    #[tokio::test]
    async fn the_head_s_workdir_gate_goes_to_push_machines_only() {
        let mut flock = home_and_work();
        flock.machines[0].command = None;
        flock.machines[0].ssh = Some("h.example".into());
        let (d, _tmp) = daemon_with_flock(
            flock,
            &[("h", 2, FakeHerdr::new()), ("w", 2, FakeHerdr::new())],
        )
        .await;
        let flock = d.fleet().flock();
        let gate = |name: &str| {
            let m = flock.get(name).unwrap();
            d.fleet()
                .settings_of(m, &MachineSettings::default())
                .workdir_gate
                .0
                .is_some()
        };
        assert!(gate("h"), "a push machine places sets");
        assert!(!gate("w"), "a command bridge runs no shell");
    }

    /// A model whose agent is of another kind than the task's workdir set
    /// is not one the task moves to: with Opus out it goes on under
    /// Sonnet, and with the whole account out it waits rather than move to
    /// the machine's opencode, where its set could not be placed.
    #[tokio::test]
    async fn a_fallback_of_another_kind_is_skipped_for_a_task_with_a_set() {
        let pi = MachineConfig {
            agents: [("opencode".to_string(), "opencode".to_string())].into(),
            command: None,
            ssh: Some("pi.example".into()),
            ..machine("pi", 3)
        };
        let (d, _tmp) = opencode_daemon(vec![pi], &[("pi", 3, FakeHerdr::new())]).await;
        // Held from the start, so nothing dispatches under the test.
        d.fleet().record_limit(&seen_limit(&d, "pi", None)).unwrap();
        let mut run = run_fallback(Some("opus"), Some(&["sonnet", "gpt5"]), Some("pi"));
        if let IpcRequest::Run { spec, .. } = &mut run {
            spec.workdir = Some(Box::new(crate::task::TaskWorkdir {
                set: serde_json::json!({"id": "reviewer", "kind": "claude", "files": [
                    {"path": ".claude/agents/checker.md", "content": "Check.\n"}]}),
                placed_in: None,
                render: None,
            }));
        }
        let IpcResponse::Task(t) = d.handle(run).await else {
            panic!()
        };
        let flock = d.fleet().flock();
        let target = t.flock.clone().unwrap_or(flock.default_flock().to_string());
        match d.fleet().settled_on(&t, &target, "pi") {
            Some(Err(_)) => {}
            Some(Ok(spec)) => panic!(
                "moved to {} ({:?})",
                spec.agent,
                spec.agent_source.and_then(|s| s.model)
            ),
            None => panic!("not settled"),
        }

        // Only Opus out: Sonnet, a claude model, takes the set.
        let d2 = {
            let pi = MachineConfig {
                agents: [("opencode".to_string(), "opencode".to_string())].into(),
                command: None,
                ssh: Some("pi.example".into()),
                ..machine("pi", 3)
            };
            opencode_daemon(vec![pi], &[("pi", 3, FakeHerdr::new())]).await
        };
        let (d2, _tmp2) = d2;
        d2.fleet()
            .record_limit(&seen_limit(&d2, "pi", Some("opus")))
            .unwrap();
        let fb = d2
            .fleet()
            .settle_model(&t, &target, "pi", "sonnet")
            .unwrap();
        assert_eq!(
            fb.agent_source.and_then(|s| s.model).as_deref(),
            Some("sonnet")
        );
        assert!(d2.fleet().settle_model(&t, &target, "pi", "gpt5").is_none());
    }

    /// A task's load is its run's, else its agent's, else its repo's, kept
    /// with where it came from; a load no machine of the task's flock takes,
    /// or that its pinned machine does not, is refused at submit. A machine
    /// takes its own loads and its flocks', whatever flock the task is in.
    #[tokio::test]
    async fn a_task_s_load_is_settled_and_checked_when_it_is_queued() {
        let mut flock = home_and_work();
        flock.machines[0].loads = vec!["rust".into()];
        flock.machines[1].loads = vec!["rust".into()];
        flock.flocks[1].loads = vec!["light".into()];
        let (d, _tmp) = daemon_with_flock(
            flock,
            &[("h", 2, FakeHerdr::new()), ("w", 2, FakeHerdr::new())],
        )
        .await;
        let loads = |name: &str| {
            let views = d.fleet().views();
            views.iter().find(|v| v.name == name).unwrap().loads.clone()
        };
        assert_eq!(loads("h"), ["rust"]);
        assert_eq!(loads("w"), ["rust", "light"]);
        let run = |flock: Option<&str>, machine: Option<&str>, load: Option<&str>| {
            let mut req = run_in(flock, machine);
            if let IpcRequest::Run { spec, .. } = &mut req {
                spec.load = load.map(Into::into);
                spec.repo = Some("~/r/".into());
            }
            req
        };
        let settled = |resp: IpcResponse| match resp {
            IpcResponse::Task(t) => (t.spec.load, t.spec.load_from),
            other => panic!("{other:?}"),
        };
        let pair = |l: &str, from: &str| (Some(l.to_string()), Some(from.to_string()));
        assert_eq!(settled(d.handle(run(None, None, None)).await), (None, None));
        assert_eq!(
            settled(d.handle(run(None, None, Some("rust"))).await),
            pair("rust", "task run")
        );
        assert_eq!(
            error_code(d.handle(run(None, None, Some("light"))).await),
            crate::task::LOAD_NOT_TAKEN,
            "w takes light, but is not in home"
        );
        assert_eq!(
            error_code(d.handle(run(None, None, Some("bad load"))).await),
            crate::task::INVALID_LOAD
        );
        assert_eq!(
            settled(d.handle(run(Some("work"), Some("w"), Some("light"))).await),
            pair("light", "task run")
        );
        // The agent's load, then the repo's, as pastor.toml has them.
        let mut cfg = test_config();
        cfg.repos.0.insert(
            "~/r".into(),
            crate::config::RepoDef {
                load: Some("gpu".into()),
            },
        );
        d.fleet().set_config(&cfg);
        let resp = d.handle(run(Some("work"), None, None)).await;
        let IpcResponse::Error { code, message } = resp else {
            panic!("{resp:?}");
        };
        assert_eq!(code, crate::task::LOAD_NOT_TAKEN);
        assert!(message.contains("load gpu (from repo ~/r/)"), "{message}");
        cfg.agents.0.insert(
            "claude".into(),
            crate::config::AgentDef {
                load: Some("light".into()),
                ..Default::default()
            },
        );
        d.fleet().set_config(&cfg);
        assert_eq!(
            settled(d.handle(run(Some("work"), None, None)).await),
            pair("light", "agent claude"),
            "the agent's before the repo's"
        );
        assert_eq!(
            settled(d.handle(run(Some("work"), None, Some("rust"))).await),
            pair("rust", "task run"),
            "the run's before the agent's"
        );
        assert_eq!(
            error_code(d.handle(run(Some("work"), Some("w"), Some("gpu"))).await),
            crate::task::LOAD_NOT_TAKEN
        );
        // A job's tasks take their agent's load the same way.
        let submitted = d
            .handle(IpcRequest::JobSubmit {
                job: "j".into(),
                dispatch: serde_json::json!({"flock": "work"}),
                prompt: "p".into(),
                items: vec![serde_json::json!({"key": "k"})],
            })
            .await;
        let IpcResponse::JobSubmitted { tasks, .. } = submitted else {
            panic!("{submitted:?}");
        };
        assert_eq!(
            (tasks[0].spec.load.clone(), tasks[0].spec.load_from.clone()),
            pair("light", "agent claude")
        );
    }

    /// A task's `remove_worktree` is settled like `keep_pane`: the run's
    /// own ask, else the job's, else the flock's, with where it came from.
    #[tokio::test]
    async fn remove_worktree_is_settled_when_a_task_is_queued() {
        let mut flock = home_and_work();
        flock.flocks.push(crate::config::flock::FlockEntry {
            name: "tidy".into(),
            remove_worktree: Some(true),
            ..Default::default()
        });
        let (d, _tmp) = daemon_with_flock(
            flock,
            &[("h", 2, FakeHerdr::new()), ("w", 2, FakeHerdr::new())],
        )
        .await;
        let settled = |resp: IpcResponse| match resp {
            IpcResponse::Task(t) => (t.spec.remove_worktree, t.spec.remove_worktree_from),
            other => panic!("{other:?}"),
        };
        assert_eq!(settled(d.handle(run_in(None, None)).await), (None, None));
        assert_eq!(
            settled(d.handle(run_in(Some("tidy"), None)).await),
            (Some(true), Some("flock tidy".into()))
        );
        let mut asked = run_in(None, None);
        if let IpcRequest::Run { spec, .. } = &mut asked {
            spec.remove_worktree = Some(true);
        }
        assert_eq!(
            settled(d.handle(asked).await),
            (Some(true), Some("task run".into()))
        );
        let from_job = d
            .handle(IpcRequest::JobSubmit {
                job: "j".into(),
                dispatch: serde_json::json!({"flock": "tidy", "remove_worktree": false}),
                prompt: "p".into(),
                items: vec![serde_json::json!({"key": "k"})],
            })
            .await;
        match from_job {
            IpcResponse::JobSubmitted { tasks, .. } => assert_eq!(
                (
                    tasks[0].spec.remove_worktree,
                    tasks[0].spec.remove_worktree_from.clone()
                ),
                (Some(false), Some("job j".into()))
            ),
            other => panic!("{other:?}"),
        }
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

    /// An agent pastor started may ask the head what a workdir set would
    /// change, check one and list them, never place or remove one: it could
    /// hand itself files another agent starts with.
    #[tokio::test]
    async fn an_agent_may_only_read_workdir_sets() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let set = serde_json::json!({"id": "x", "kind": "claude"});
        let place = |dry_run| IpcRequest::WorkdirPlace {
            machine: "zz".into(),
            dir: "/w".into(),
            set: Some(set.clone()),
            bundle: None,
            kind: None,
            flock: None,
            dry_run,
        };
        let remove = |dry_run| IpcRequest::WorkdirRemove {
            machine: "zz".into(),
            dir: "/w".into(),
            id: None,
            dry_run,
        };
        for req in [place(false), remove(false)] {
            let resp = d.handle_from(req, Some("t-1")).await;
            assert_eq!(code_of(&resp), Some("agent_refused"), "{resp:?}");
        }
        let reads = [
            place(true),
            remove(true),
            IpcRequest::WorkdirCheck {
                machine: "zz".into(),
                dir: "/w".into(),
                id: None,
            },
        ];
        for req in reads {
            let resp = d.handle_from(req, Some("t-1")).await;
            // Past the guard, to the head's own answer: no such machine,
            // or nothing placed there.
            assert_ne!(code_of(&resp), Some("agent_refused"), "{resp:?}");
        }
        let resp = d
            .handle_from(IpcRequest::WorkdirList { machine: None }, Some("t-1"))
            .await;
        let IpcResponse::Workdirs(records) = resp else {
            panic!("{resp:?}")
        };
        assert!(records.is_empty());
    }

    /// An orchestrator may run, retry, send to and close tasks and enable
    /// and disable a job, all from its own pane.
    #[tokio::test]
    async fn an_orchestrator_may_run_retry_send_close_enable_and_disable() {
        let (d, tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
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
                    reserved: 0,
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
                    loads: vec![],
                    flock: None,
                    agent: None,
                    agent_args: None,
                    model: None,
                    fallback: None,
                    priority: None,
                    agents: Default::default(),
                    profile: None,
                    min_disk: None,
                    min_memory: None,
                    unknown_room: None,
                    description: None,
                    provisioner: None,
                    bundles: Vec::new(),
                    bundle_apply: None,
                    pins: Default::default(),
                    env: Default::default(),
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
        let resp = d.handle_from(run_hi(), Some("t-99")).await;
        assert_eq!(code_of(&resp), Some("agent_refused"), "{resp:?}");
    }

    /// No task makes an orchestrator, not an orchestrator: neither with
    /// `--role orchestrator` nor by retrying one. A person may.
    #[tokio::test]
    async fn only_a_person_makes_an_orchestrator() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
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
        // A plain agent may not close tasks.
        let resp = d
            .handle_from(
                IpcRequest::TaskClose {
                    id: worker.id,
                    remove_worktree: false,
                },
                Some(&worker.display_id()),
            )
            .await;
        assert_eq!(code_of(&resp), Some("agent_refused"), "{resp:?}");
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

    /// A socket this caller may not connect to is refused with
    /// `socket_denied`, and left where it is: a head may well be live
    /// behind it.
    #[tokio::test]
    async fn a_denied_socket_is_refused_and_kept() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("pastor.sock");
        let _listener = tokio::net::UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
            // Root ignores the mode; there is nothing to deny.
            return;
        }
        let err = refuse_live_socket(&socket).await.unwrap_err();
        let err = err.downcast::<crate::cli::CliError>().unwrap();
        assert_eq!(err.code, "socket_denied", "{}", err.message);
        assert!(err.message.contains("sandbox"), "{}", err.message);
        assert!(socket.exists(), "never unlinked over a denial");
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

    /// A room as the fake reports it, in KiB.
    fn room_gib(home: u64, tmp: u64, mem: u64) -> crate::machine::Room {
        let kib = |gib: u64| Some(gib * 1024 * 1024);
        crate::machine::Room {
            home_free: kib(home),
            tmp_free: kib(tmp),
            mem_free: kib(mem),
            ..Default::default()
        }
    }

    fn insert_run(d: &Daemon, spec: DispatchSpec) -> Task {
        d.store()
            .insert_task(NewTask {
                job: "run".into(),
                item: serde_json::Value::Null,
                prompt: "p".into(),
                spec,
                flock: "default".into(),
                description: None,
            })
            .unwrap()
    }

    /// A machine under its floor takes no task, and the task's note says
    /// which side is short, as `pastor queue` shows it; once the room is
    /// back it takes the task and the note goes. A `--now` task pinned to
    /// it lands all the same.
    #[tokio::test]
    async fn a_machine_under_its_floor_takes_no_task_until_room_comes_back() {
        let fake = FakeHerdr::new();
        fake.set_room(Some(room_gib(50, 1, 8)));
        let flock = Flock {
            flocks: vec![],
            machines: vec![MachineConfig {
                min_disk: Some("4G".into()),
                ..machine("a", 2)
            }],
        };
        let (d, _tmp) = daemon_with_flock(flock, &[("a", 2, fake.clone())]).await;
        let fleet = d.fleet();
        let waiting = insert_run(&d, spec());
        fleet.dispatch_queued().await;
        let task = |id: i64| d.store().get_task(id).unwrap().unwrap();
        assert_eq!(task(waiting.id).state, TaskState::Queued);
        assert_eq!(
            task(waiting.id).error.as_deref(),
            Some("waiting for a machine: a has 1.0 GB free under /tmp")
        );
        let entry = crate::queue::entries(
            vec![task(waiting.id)],
            &fleet.queue_views(),
            "default",
            &|_, _| true,
        );
        assert_eq!(
            entry[0].why,
            "waiting for a machine: a has 1.0 GB free under /tmp"
        );

        let now = insert_run(
            &d,
            DispatchSpec {
                now: true,
                machine: Some("a".into()),
                ..spec()
            },
        );
        fleet.dispatch_queued().await;
        assert_eq!(
            task(now.id).state,
            TaskState::Running,
            "--now ignores the floor"
        );
        assert_eq!(task(waiting.id).state, TaskState::Queued);

        // The room comes back: as the next probe would read it.
        fake.set_room(Some(room_gib(50, 10, 8)));
        fleet.get("a").unwrap().status.write().recover().room = Some(room_gib(50, 10, 8));
        fleet.dispatch_queued().await;
        assert_eq!(task(waiting.id).state, TaskState::Running);
        assert_eq!(task(waiting.id).error, None, "the note goes");
    }

    /// A probe that fails leaves the channel connected and the machine's
    /// room unread, which counts as room by default; a flock with
    /// `unknown_room = "no_room"` holds the task, and says so.
    #[tokio::test]
    async fn a_failed_room_probe_counts_as_room_unless_the_flock_says_no_room() {
        for (unknown, placed) in [("", true), ("unknown_room = \"no_room\"\n", false)] {
            let fake = FakeHerdr::new();
            fake.set_room_fails(true);
            let flock: Flock = toml::from_str(&format!(
                "[[flock]]\nname = \"default\"\ndefault = true\n{unknown}machines = {{ a = 2 }}\n\n\
                 [[machine]]\nname = \"a\"\nlocal = true\nmax_agents = 2\nmin_disk = \"4G\"\n",
            ))
            .unwrap();
            flock.validate().unwrap();
            let (d, _tmp) = daemon_with_flock(flock, &[("a", 2, fake.clone())]).await;
            let fleet = d.fleet();
            let status = fleet.get("a").unwrap().snapshot();
            assert_eq!(status.channel, crate::machine::ChannelState::Connected);
            assert!(
                status.room.and_then(|r| r.error).is_some(),
                "the probe failed"
            );
            let t = insert_run(&d, spec());
            fleet.dispatch_queued().await;
            let t = d.store().get_task(t.id).unwrap().unwrap();
            if placed {
                assert_eq!(t.state, TaskState::Running, "{unknown:?}");
            } else {
                assert_eq!(t.state, TaskState::Queued, "{unknown:?}");
                assert_eq!(
                    t.error.as_deref(),
                    Some("waiting for a machine: a's room cannot be read")
                );
            }
        }
    }

    /// A machine whose entry sets no floor is never short, however little
    /// room it shows.
    #[tokio::test]
    async fn a_machine_with_no_floor_is_never_short() {
        let fake = FakeHerdr::new();
        fake.set_room(Some(room_gib(0, 0, 0)));
        let (d, _tmp) = daemon_with_flock(flock_of(&[("a", 2)]), &[("a", 2, fake.clone())]).await;
        let fleet = d.fleet();
        assert_eq!(fleet.views()[0].floor, FloorView::default());
        let t = insert_run(&d, spec());
        fleet.dispatch_queued().await;
        assert_eq!(
            d.store().get_task(t.id).unwrap().unwrap().state,
            TaskState::Running
        );
    }

    /// A pull machine's floor stays at the default, as `Fleet::claim`
    /// already keeps it: the head never reads its room, so an enabled
    /// floor under a flock whose `unknown_room` is `no_room` must not read
    /// it as floor-held. A task pinned there waits only for a connection,
    /// never for a room it never probes.
    #[tokio::test]
    async fn a_pull_machine_s_floor_stays_at_the_default() {
        let flock: Flock = toml::from_str(
            "[[flock]]\nname = \"default\"\ndefault = true\nunknown_room = \"no_room\"\n\
             machines = { laptop = 1 }\n\n\
             [[machine]]\nname = \"laptop\"\npull = true\nmax_agents = 1\nmin_disk = \"4G\"\n",
        )
        .unwrap();
        flock.validate().unwrap();
        let (d, _tmp) = daemon_with_flock(flock, &[]).await;
        let fleet = d.fleet();
        assert_eq!(
            fleet.views()[0].floor,
            FloorView::default(),
            "the head never reads a pull machine's room"
        );
        let pinned = insert_run(
            &d,
            DispatchSpec {
                machine: Some("laptop".into()),
                ..spec()
            },
        );
        let why = fleet
            .queue(None, None)
            .unwrap()
            .into_iter()
            .find(|e| e.task.id == pinned.id)
            .map(|e| e.why)
            .expect("queued");
        assert_eq!(
            why, "machine laptop is not connected",
            "not held by a floor it never reads"
        );
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

    /// A flock with a share and a max: past its share it waits while a
    /// flock under its share has a task for the machine, even one behind it
    /// in the queue, and takes the idle slot once nobody else wants it; at
    /// its max it waits however idle the machine is.
    #[tokio::test]
    async fn dispatch_lets_a_flock_past_its_share_only_into_idle_slots() {
        let fake = FakeHerdr::new();
        let flock: Flock = toml::from_str(
            "[[flock]]\nname = \"pastor\"\ndefault = true\nmachines = { desk = { share = 1, max = 2 } }\n\n\
             [[flock]]\nname = \"life\"\nmachines = { desk = 2 }\n\n\
             [[machine]]\nname = \"desk\"\nlocal = true\nmax_agents = 4\n",
        )
        .unwrap();
        flock.validate().unwrap();
        let (d, _tmp) = daemon_with_flock(flock, &[("desk", 4, fake.clone())]).await;
        let insert = |flock: &str| {
            d.store()
                .insert_task(NewTask {
                    job: "run".into(),
                    item: serde_json::Value::Null,
                    prompt: "p".into(),
                    spec: spec(),
                    flock: flock.into(),
                    description: None,
                })
                .unwrap()
        };
        let task = |id: i64| d.store().get_task(id).unwrap().unwrap();
        let fleet = d.fleet();

        // Two flocks want the same slot: the one under its share goes first.
        let first = insert("pastor");
        let second = insert("pastor");
        let life = insert("life");
        fleet.dispatch_queued().await;
        assert_eq!(task(first.id).state, TaskState::Running, "under its share");
        assert_eq!(task(life.id).state, TaskState::Running, "under its share");
        assert_eq!(task(second.id).state, TaskState::Queued);
        assert_eq!(
            task(second.id).error.as_deref(),
            Some(
                "waiting for a machine: flock pastor is past its share on desk, at 1 of 1/2, \
                 while flock life waits under its share"
            )
        );

        // Nobody under its share waits: the idle slot is pastor's.
        fleet.dispatch_queued().await;
        assert_eq!(task(second.id).state, TaskState::Running, "past its share");
        assert_eq!(task(second.id).error, None);

        // At its max it waits though the machine has room; life still goes.
        let third = insert("pastor");
        let more_life = insert("life");
        fleet.dispatch_queued().await;
        assert_eq!(task(third.id).state, TaskState::Queued, "held at its max");
        assert_eq!(
            task(third.id).error.as_deref(),
            Some("waiting for a machine: flock pastor is at 2 of 1/2 on desk")
        );
        assert_eq!(task(more_life.id).state, TaskState::Running);
        let status = fleet
            .statuses()
            .into_iter()
            .find(|s| s.name == "desk")
            .unwrap();
        let seats: Vec<(String, Option<u32>, Option<u32>, usize)> = status
            .flocks
            .iter()
            .map(|f| (f.name.clone(), f.share, f.max, f.live))
            .collect();
        assert_eq!(
            seats,
            [
                ("pastor".into(), Some(1), Some(2), 2),
                ("life".into(), Some(2), Some(2), 2)
            ]
        );
        assert_eq!(fake.agents().len(), 4);
    }

    /// A flock set to 0 on a machine drains it: the task already running
    /// there keeps running through dispatch passes, a new one pinned there
    /// stays queued with the reason, and the seat reads 1 of 0.
    #[tokio::test]
    async fn a_flock_at_0_drains_the_machine() {
        let fake = FakeHerdr::new();
        let file = |n: u32| {
            let flock: Flock = toml::from_str(&format!(
                "[[flock]]\nname = \"pastor\"\ndefault = true\nmachines = {{ desk = {n} }}\n\n\
                 [[machine]]\nname = \"desk\"\nlocal = true\nmax_agents = 4\n"
            ))
            .unwrap();
            flock.validate().unwrap();
            flock
        };
        let (d, _tmp) = daemon_with_flock(file(1), &[("desk", 4, fake.clone())]).await;
        let insert = || {
            d.store()
                .insert_task(NewTask {
                    job: "run".into(),
                    item: serde_json::Value::Null,
                    prompt: "p".into(),
                    spec: DispatchSpec {
                        machine: Some("desk".into()),
                        ..spec()
                    },
                    flock: "pastor".into(),
                    description: None,
                })
                .unwrap()
        };
        let task = |id: i64| d.store().get_task(id).unwrap().unwrap();
        let fleet = d.fleet();
        let running = insert();
        fleet.dispatch_queued().await;
        assert_eq!(task(running.id).state, TaskState::Running);

        fleet.replace_flock(file(0)).await;
        let held = insert();
        fleet.dispatch_queued().await;
        fleet.dispatch_queued().await;
        assert_eq!(task(running.id).state, TaskState::Running, "left to finish");
        assert_eq!(task(held.id).state, TaskState::Queued);
        assert_eq!(
            task(held.id).error.as_deref(),
            Some(
                "waiting for a machine: flock pastor has 0 slots on desk: \
                 it starts no new tasks there (1 still running)"
            )
        );
        assert_eq!(fake.agents().len(), 1);
        let status = fleet
            .statuses()
            .into_iter()
            .find(|s| s.name == "desk")
            .unwrap();
        let seats: Vec<(String, Option<u32>, Option<u32>, usize)> = status
            .flocks
            .iter()
            .map(|f| (f.name.clone(), f.share, f.max, f.live))
            .collect();
        assert_eq!(seats, [("pastor".into(), Some(0), Some(0), 1)]);
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
        // Disabled, so only the ticks below run it. Enabled, a new interval
        // job is due at once and the scheduler's own pass may run it first;
        // the clock item is keyed by the second, so a pass in the same second
        // leaves the dry run nothing new to create. A forced tick and
        // `JobRun` ignore `enabled`.
        std::fs::write(
            paths.jobs_dir().join("clock.toml"),
            "every = \"1h\"\nenabled = false\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"tick {{ item.key }} {{ task.id }}\"\n",
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
        // Only an `ssh` machine's agents are cut off when it is unset.
        assert!(actor_settings(&remote, &unset).cut_off_from_head);
        assert!(!actor_settings(&local, &unset).cut_off_from_head);
        assert!(!actor_settings(&remote, &settings).cut_off_from_head);
    }

    /// With no `head_address`, an agent on an `ssh` machine has no way back
    /// to the head, so its `task done` fails. The head names those machines
    /// so it can warn before any task goes there. A local machine uses the
    /// socket and a pull machine's own serve carries the request.
    #[test]
    fn ssh_machines_without_a_head_address_are_named() {
        let machine = |name: &str, text: &str| -> MachineConfig {
            toml::from_str(&format!("name = \"{name}\"\n{text}")).unwrap()
        };
        let machines = vec![
            machine("here", "local = true"),
            machine("pi", "ssh = \"pi\""),
            machine("puller", "pull = true"),
            machine("desk", "ssh = \"desk\""),
        ];
        assert_eq!(
            cut_off_from_head(&test_config(), &machines),
            vec!["pi".to_string(), "desk".to_string()]
        );
        let set = PastorConfig {
            head_address: Some("user@head.example".into()),
            ..test_config()
        };
        assert!(cut_off_from_head(&set, &machines).is_empty());
    }

    #[tokio::test]
    async fn task_send_reaches_a_live_task_and_refuses_the_rest() {
        let fake = FakeHerdr::new();
        let (d, _tmp) = daemon(&[("a", 2, fake.clone())]).await;
        let IpcResponse::Task(t) = d
            .handle(IpcRequest::Run {
                now: false,
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
        // --pick with text or --key is a usage error, like --trust.
        let resp = d
            .handle(IpcRequest::TaskSend {
                id: t.id,
                input: crate::machine::SendInput {
                    pick: Some("No".into()),
                    keys: vec!["esc".into()],
                    ..Default::default()
                },
            })
            .await;
        assert_eq!(error_code(resp), "usage_error");
        // An empty --pick would match every option; it is nothing to send.
        let resp = d
            .handle(IpcRequest::TaskSend {
                id: t.id,
                input: crate::machine::SendInput {
                    pick: Some("  ".into()),
                    ..Default::default()
                },
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
                now: false,
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
                now: false,
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
                now: false,
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
                now: false,
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

        /// A machine held off by its room floor is not one to pause a task
        /// on: pausing frees a slot, not disk. The critical task waits and
        /// nothing is paused.
        #[tokio::test]
        async fn a_preempting_task_does_not_pause_on_a_machine_under_its_floor() {
            let fake = FakeHerdr::new();
            fake.set_room(Some(room_gib(50, 10, 8)));
            let flock = Flock {
                flocks: vec![],
                machines: vec![MachineConfig {
                    min_disk: Some("4G".into()),
                    ..machine("a", 2)
                }],
            };
            let (d, _tmp) = daemon_with_flock(flock, &[("a", 2, fake.clone())]).await;
            let first = start(&d, run("first", wt(), Priority::Low, false)).await;
            let low = start(&d, run("low", wt(), Priority::Low, false)).await;
            assert_eq!(get(&d, low.id).state, TaskState::Running);
            // The next probe reads /tmp under the floor.
            d.fleet().get("a").unwrap().status.write().recover().room = Some(room_gib(50, 1, 8));

            let crit = start(&d, run("fix prod", spec(), Priority::Critical, true)).await;
            assert_eq!(crit.state, TaskState::Queued, "waits for room");
            assert_eq!(get(&d, low.id).state, TaskState::Running, "nothing paused");
            assert_eq!(get(&d, first.id).state, TaskState::Running);
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

        /// A paused task past its flock's share waits, as a queued one
        /// would, while a flock under its share there has a task for the
        /// freed slot; it resumes once nobody under a share wants it.
        #[tokio::test]
        async fn a_paused_task_past_its_share_leaves_the_slot_to_a_flock_under_its_share() {
            let fake = FakeHerdr::new();
            let flock: Flock = toml::from_str(
                "[[flock]]\nname = \"pastor\"\ndefault = true\nmachines = { desk = { share = 1, max = 2 } }\n\n\
                 [[flock]]\nname = \"life\"\nmachines = { desk = 2 }\n\n\
                 [[machine]]\nname = \"desk\"\nlocal = true\nmax_agents = 2\n",
            )
            .unwrap();
            flock.validate().unwrap();
            let (d, _tmp) = daemon_with_flock(flock, &[("desk", 2, fake.clone())]).await;
            let first = start(&d, run("first", spec(), Priority::Low, false)).await;
            let second = start(&d, run("second", spec(), Priority::Low, false)).await;
            assert_eq!(first.state, TaskState::Running);
            assert_eq!(
                second.state,
                TaskState::Running,
                "past its share, nobody waits"
            );
            let crit = start(&d, run("crit", spec(), Priority::Critical, true)).await;
            assert_eq!(crit.state, TaskState::Running);
            assert_eq!(get(&d, second.id).state, TaskState::Paused);

            let mut req = run("life", spec(), Priority::Low, false);
            if let IpcRequest::Run { flock, .. } = &mut req {
                *flock = Some("life".into());
            }
            let life = start(&d, req).await;
            assert_eq!(life.state, TaskState::Queued);

            // One slot frees: pastor is at its share again, life is under
            // its own and waits, so life takes it.
            d.handle(IpcRequest::TaskClose {
                id: crit.id,
                remove_worktree: false,
            })
            .await;
            d.fleet().dispatch_queued().await;
            assert_eq!(get(&d, life.id).state, TaskState::Running);
            assert_eq!(get(&d, second.id).state, TaskState::Paused);

            // Nobody under a share waits now: the paused task resumes.
            d.handle(IpcRequest::TaskClose {
                id: life.id,
                remove_worktree: false,
            })
            .await;
            d.fleet().dispatch_queued().await;
            assert_eq!(get(&d, second.id).state, TaskState::Running);
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

        /// A low task paused for a critical one, then turned `waiting` as
        /// `wait_on_limit` would leave it: its pane closed the same way,
        /// its checkout kept, due in an hour. The critical task is closed,
        /// so the slot is free and only the wait holds it.
        async fn waiting_low(d: &Daemon, spec: DispatchSpec) -> Task {
            let low = start(d, run("low", spec, Priority::Low, false)).await;
            let crit = start(d, run("crit", super::spec(), Priority::Critical, true)).await;
            let mut t = get(d, low.id);
            assert_eq!(t.state, TaskState::Paused);
            t.state = TaskState::Waiting;
            t.waiting_until = Some(chrono::Utc::now() + chrono::Duration::hours(1));
            d.store.update_task(&mut t).unwrap();
            d.handle(IpcRequest::TaskClose {
                id: crit.id,
                remove_worktree: false,
            })
            .await;
            t
        }

        /// A waiting task has no agent: `send` has nothing to type into,
        /// and `close` closes its row, with nothing asked of the machine.
        #[tokio::test]
        async fn send_and_close_on_a_waiting_task() {
            let fake = FakeHerdr::new();
            let (d, _tmp) = daemon(&[("a", 1, fake.clone())]).await;
            let t = waiting_low(&d, spec()).await;
            let resp = d
                .handle(IpcRequest::TaskSend {
                    id: t.id,
                    input: SendInput {
                        text: Some("hi".into()),
                        enter: true,
                        ..Default::default()
                    },
                })
                .await;
            assert_eq!(error_code(resp), "task_not_live");
            let before = fake.requests().len();
            let IpcResponse::Task(closed) = d
                .handle(IpcRequest::TaskClose {
                    id: t.id,
                    remove_worktree: false,
                })
                .await
            else {
                panic!()
            };
            assert_eq!(closed.state, TaskState::Closed);
            assert_eq!(fake.requests().len(), before, "the row alone");
            assert!(d.store.waiting_tasks().unwrap().is_empty());
        }

        /// `close --remove-worktree` on a waiting task removes its kept
        /// checkout, as for a paused one.
        #[tokio::test]
        async fn close_remove_worktree_on_a_waiting_task_removes_its_checkout() {
            let fake = FakeHerdr::new();
            let (d, _tmp) = daemon(&[("a", 1, fake.clone())]).await;
            let t = waiting_low(&d, wt()).await;
            let checkout = t.spec.checkout.clone().unwrap();
            let IpcResponse::Task(closed) = d
                .handle(IpcRequest::TaskClose {
                    id: t.id,
                    remove_worktree: true,
                })
                .await
            else {
                panic!()
            };
            assert_eq!(closed.state, TaskState::Closed);
            let left = fake.worktree_list("/srv/app").await.unwrap();
            assert!(!left.iter().any(|w| w.path == checkout.path), "{left:?}");
        }
    }

    /// Each dispatch pass first ages the queue: a task that waited its
    /// flock's `age_after` goes up a level, and one in a flock that never
    /// ages keeps its own.
    #[tokio::test]
    async fn a_dispatch_pass_ages_the_queue_by_flock() {
        use crate::config::flock::FlockEntry;
        let flock = Flock {
            flocks: vec![
                FlockEntry {
                    name: "work".into(),
                    default: true,
                    age_after: Some("5m".into()),
                    ..Default::default()
                },
                FlockEntry {
                    name: "still".into(),
                    age_after: Some("never".into()),
                    ..Default::default()
                },
            ],
            machines: vec![],
        };
        let (d, _tmp) = daemon_with_flock(flock, &[]).await;
        let store = d.store();
        let at = |flock: &str| {
            store
                .insert_task_at(
                    NewTask {
                        description: None,
                        job: "run".into(),
                        item: serde_json::Value::Null,
                        prompt: "p".into(),
                        spec: spec(),
                        flock: flock.into(),
                    },
                    Priority::Low,
                    Some("task run"),
                    crate::task::TaskRole::Agent,
                )
                .unwrap()
        };
        let work = at("work");
        let still = at("still");
        let fresh = at("work");
        let ago = (chrono::Utc::now() - chrono::Duration::minutes(10)).to_rfc3339();
        store.execute_raw(&format!(
            "UPDATE tasks SET created_at = '{ago}' WHERE id IN ({}, {})",
            work.id, still.id
        ));
        d.fleet().dispatch_queued().await;
        let level = |id| {
            let t = store.get_task(id).unwrap().unwrap();
            (t.priority, t.aged_from)
        };
        assert_eq!(level(work.id), (Priority::Normal, Some(Priority::Low)));
        assert_eq!(level(still.id), (Priority::Low, None));
        assert_eq!(level(fresh.id), (Priority::Low, None));
    }

    /// A task a pass placed keeps its `queued` row while its machine's
    /// actor works through what came before it (`in_flight`). It has left
    /// the queue, so a later pass must not age it: the level it runs at,
    /// and a retry keeps, is the one it was placed at, even when the
    /// placement is held across an ageing deadline.
    #[tokio::test]
    async fn a_placement_held_across_an_ageing_deadline_keeps_its_level() {
        use crate::config::flock::FlockEntry;
        let flock = Flock {
            flocks: vec![FlockEntry {
                name: "work".into(),
                default: true,
                age_after: Some("5m".into()),
                ..Default::default()
            }],
            machines: vec![],
        };
        let (d, _tmp) = daemon_with_flock(flock, &[]).await;
        let store = d.store();
        let fleet = d.fleet();
        let t = store
            .insert_task_at(
                NewTask {
                    description: None,
                    job: "run".into(),
                    item: serde_json::Value::Null,
                    prompt: "p".into(),
                    spec: spec(),
                    flock: "work".into(),
                },
                Priority::Low,
                Some("task run"),
                crate::task::TaskRole::Agent,
            )
            .unwrap();
        fleet.in_flight.placed.lock().unwrap().push(Placed {
            task_id: t.id,
            machine: "nowhere".into(),
            flock: t.flock.clone(),
            from_job: false,
        });
        let held = Reservation {
            in_flight: fleet.in_flight.clone(),
            task_id: t.id,
        };
        // The deadline passes while the placement is held.
        let ago = (chrono::Utc::now() - chrono::Duration::minutes(10)).to_rfc3339();
        store.execute_raw(&format!(
            "UPDATE tasks SET created_at = '{ago}' WHERE id = {}",
            t.id
        ));
        fleet.dispatch_queued().await;
        let got = store.get_task(t.id).unwrap().unwrap();
        assert_eq!(
            (got.priority, got.aged_from, got.aged_at),
            (Priority::Low, None, None)
        );
        drop(held);
    }

    /// A queued task pinned to `machine`, straight into the store.
    fn queue_on(store: &Store, prompt: &str, machine: Option<&str>) -> Task {
        store
            .insert_task(NewTask {
                description: None,
                job: "run".into(),
                item: serde_json::Value::Null,
                prompt: prompt.into(),
                spec: DispatchSpec {
                    machine: machine.map(Into::into),
                    ..spec()
                },
                flock: "default".into(),
            })
            .unwrap()
    }

    /// A queued `--now` task pinned to `machine`, straight into the store,
    /// as the head would have set `DispatchSpec::now` from `Run::now`.
    fn queue_now_on(store: &Store, prompt: &str, machine: &str) -> Task {
        store
            .insert_task(NewTask {
                description: None,
                job: "run".into(),
                item: serde_json::Value::Null,
                prompt: prompt.into(),
                spec: DispatchSpec {
                    now: true,
                    machine: Some(machine.into()),
                    ..spec()
                },
                flock: "default".into(),
            })
            .unwrap()
    }

    /// A `--now` placement is urgent: queued behind a slower placement for
    /// the same machine in one pass, its own dispatch must start without
    /// waiting for that placement to finish (`dispatch_queued` sorts a
    /// machine's sends so `now` goes first; GPT review on #103, finding 1).
    #[tokio::test]
    async fn now_starts_ahead_of_a_slower_placement_on_the_same_machine() {
        let slow = FakeHerdr::new();
        // The agent stays launching until the test lets it up, so no clock
        // decides whether `first` was held back: a loaded runner cannot
        // make the urgent agent ready early.
        slow.set_ready_after(Duration::from_secs(3600));
        // Nor does the dispatch's own wait for the agent give up on it: it
        // outlasts the longest the test waits.
        let config = PastorConfig {
            agent_ready_timeout: "900s".into(),
            request_timeout: "1000s".into(),
            ..test_config()
        };
        let flock = Flock {
            flocks: vec![],
            machines: vec![machine("a", 1)],
        };
        let (d, _tmp) =
            daemon_with_flock_and_config(flock, &[("a", 1, slow.clone())], config).await;
        let first = queue_on(&d.store(), "slow", Some("a"));
        let urgent = queue_now_on(&d.store(), "urgent", "a");
        let fleet = d.fleet();
        let pass = tokio::spawn({
            let fleet = fleet.clone();
            async move { fleet.dispatch_queued().await }
        });
        // A bound on how long a failure takes to show, not a timing the
        // test depends on.
        let deadline = Instant::now() + Duration::from_secs(600);
        while state_of(&d, &urgent) != TaskState::Starting {
            assert!(Instant::now() < deadline, "the --now task never started");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            state_of(&d, &first),
            TaskState::Queued,
            "a slower placement for the same machine must not go out ahead of --now"
        );
        slow.set_ready_after(Duration::ZERO);
        tokio::time::timeout(Duration::from_secs(600), pass)
            .await
            .expect("the pass ends once both agents are up")
            .unwrap();
        // The pass ends when the agents are up; the rows follow when the
        // actor reports them, a moment later.
        let deadline = Instant::now() + Duration::from_secs(600);
        while [&urgent, &first]
            .iter()
            .any(|t| state_of(&d, t) != TaskState::Running)
        {
            assert!(
                Instant::now() < deadline,
                "urgent {:?}, first {:?}",
                state_of(&d, &urgent),
                state_of(&d, &first)
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// A `--now` task sent while an earlier pass's task is still starting on
    /// the same machine starts beside it, on the actor's urgent channel,
    /// instead of waiting in the actor's queue behind that startup (GPT
    /// review on #103, second round).
    #[tokio::test]
    async fn now_starts_while_an_earlier_task_is_starting_on_its_machine() {
        let slow = FakeHerdr::new();
        // Wide next to the deadlines below (their sum is 5s): on a runner busy
        // with the rest of the suite, the two wait loops can themselves eat
        // seconds of real time before either one notices its state changed.
        slow.set_ready_after(Duration::from_secs(8));
        let (d, _tmp) = daemon(&[("a", 1, slow.clone())]).await;
        let first = queue_on(&d.store(), "slow", Some("a"));
        let fleet = d.fleet();
        // The scheduler's pass as the daemon starts may place `first` before
        // this one does, leaving this one nothing to send: each pass here
        // waits on its task's send, whichever pass placed it.
        let pass = tokio::spawn({
            let fleet = fleet.clone();
            let id = first.id;
            async move { fleet.dispatch_queued_for(id).await }
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        while state_of(&d, &first) != TaskState::Starting {
            assert!(Instant::now() < deadline, "a never claimed its task");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let urgent = queue_now_on(&d.store(), "urgent", "a");
        let now_pass = tokio::spawn({
            let fleet = fleet.clone();
            let id = urgent.id;
            async move { fleet.dispatch_queued_for(id).await }
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while state_of(&d, &urgent) != TaskState::Starting {
            assert!(
                Instant::now() < deadline,
                "the --now task waited behind the earlier task's startup"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(state_of(&d, &first), TaskState::Starting);
        assert!(!pass.is_finished(), "the earlier task is still starting");
        slow.set_ready_after(Duration::ZERO);
        tokio::time::timeout(Duration::from_secs(10), now_pass)
            .await
            .expect("the --now pass ends once its agent is up")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), pass)
            .await
            .expect("the earlier pass ends once its agent is up")
            .unwrap();
        assert_eq!(state_of(&d, &urgent), TaskState::Running);
        assert_eq!(state_of(&d, &first), TaskState::Running);
        assert_eq!(slow.agents().len(), 2);
    }

    /// A `--now` task sent after the earlier task has started, while an
    /// earlier `--now` task is still starting beside it, starts at once too:
    /// `run_dispatch` keeps taking urgent dispatches while it waits on the
    /// ones in flight (Gemini review on #103).
    #[tokio::test]
    async fn now_starts_while_an_earlier_now_task_is_still_starting() {
        let slow = FakeHerdr::new();
        slow.set_ready_after(Duration::from_secs(1));
        let (d, _tmp) = daemon(&[("a", 1, slow.clone())]).await;
        let first = queue_on(&d.store(), "slow", Some("a"));
        let fleet = d.fleet();
        let pass = tokio::spawn({
            let fleet = fleet.clone();
            async move { fleet.dispatch_queued().await }
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while state_of(&d, &first) != TaskState::Starting {
            assert!(Instant::now() < deadline, "a never claimed its task");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Half the window later, so `first` is up well before `urgent`.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let urgent = queue_now_on(&d.store(), "urgent", "a");
        let now_pass = tokio::spawn({
            let fleet = fleet.clone();
            async move { fleet.dispatch_queued().await }
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while state_of(&d, &first) != TaskState::Running {
            assert!(Instant::now() < deadline, "the earlier task never started");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // Hold `urgent` in its startup from here on.
        slow.set_ready_after(Duration::from_secs(60));
        assert_eq!(state_of(&d, &urgent), TaskState::Starting);
        let later = queue_now_on(&d.store(), "later", "a");
        let later_pass = tokio::spawn({
            let fleet = fleet.clone();
            async move { fleet.dispatch_queued().await }
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while state_of(&d, &later) != TaskState::Starting {
            assert!(
                Instant::now() < deadline,
                "the later --now task waited behind the earlier --now task's startup"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(state_of(&d, &urgent), TaskState::Starting);
        slow.set_ready_after(Duration::ZERO);
        for p in [pass, now_pass, later_pass] {
            tokio::time::timeout(Duration::from_secs(3), p)
                .await
                .expect("each pass ends once its agent is up")
                .unwrap();
        }
        assert_eq!(state_of(&d, &first), TaskState::Running);
        assert_eq!(state_of(&d, &urgent), TaskState::Running);
        assert_eq!(state_of(&d, &later), TaskState::Running);
        assert_eq!(slow.agents().len(), 3);
    }

    /// A pass sends outside the dispatch lock: while `a` is still starting
    /// its agent, a `task run` for `b` is queued and dispatched without
    /// waiting for it.
    #[tokio::test]
    async fn a_stalled_dispatch_holds_up_no_other_machine() {
        let slow = FakeHerdr::new();
        slow.set_ready_after(Duration::from_secs(3));
        let (d, _tmp) = daemon(&[("a", 1, slow.clone()), ("b", 1, FakeHerdr::new())]).await;
        let first = queue_on(&d.store(), "slow", Some("a"));
        let fleet = d.fleet();
        let pass = tokio::spawn({
            let fleet = fleet.clone();
            async move { fleet.dispatch_queued().await }
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while state_of(&d, &first) != TaskState::Starting {
            assert!(Instant::now() < deadline, "a never claimed its task");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let run = tokio::time::timeout(Duration::from_secs(1), async {
            let t = fleet
                .queue_run(
                    "quick".into(),
                    DispatchSpec {
                        machine: Some("b".into()),
                        ..spec()
                    },
                    None,
                    None,
                    None,
                )
                .await
                .unwrap();
            fleet.dispatch_queued().await;
            t
        })
        .await
        .expect("a run on b does not wait for a's dispatch");
        assert_eq!(state_of(&d, &run), TaskState::Running);
        // Ask the task, not `pass`: the scheduler's own tick also dispatches,
        // and under load it can claim `a`'s task first, leaving `pass` with
        // nothing to send.
        assert_eq!(
            state_of(&d, &first),
            TaskState::Starting,
            "a is still starting its agent"
        );
        pass.await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while state_of(&d, &first) != TaskState::Running {
            assert!(Instant::now() < deadline, "a never started its agent");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Two passes racing for one task that two machines could take send it
    /// once: the second skips what the first placed and has not sent yet.
    #[tokio::test]
    async fn two_passes_racing_for_one_task_dispatch_it_once() {
        let (a, b) = (FakeHerdr::new(), FakeHerdr::new());
        a.set_ready_after(Duration::from_millis(300));
        b.set_ready_after(Duration::from_millis(300));
        let (d, _tmp) = daemon(&[("a", 1, a.clone()), ("b", 1, b.clone())]).await;
        let t = queue_on(&d.store(), "once", None);
        let fleet = d.fleet();
        tokio::join!(fleet.dispatch_queued(), fleet.dispatch_queued());
        assert_eq!(state_of(&d, &t), TaskState::Running);
        assert_eq!(a.agents().len() + b.agents().len(), 1, "one agent in all");
        assert!(fleet.in_flight.placed().is_empty());
    }

    /// A reload that stops three actors stuck in a poll stops them together:
    /// it takes about one `SHUTDOWN_WAIT` (2s), not three.
    #[tokio::test(flavor = "multi_thread", worker_threads = 6)]
    async fn a_reload_stops_its_actors_at_once() {
        let wedged: Vec<FakeHerdr> = (0..3).map(|_| FakeHerdr::new()).collect();
        let _unwedge: Vec<Unwedge> = wedged
            .iter()
            .map(|f| {
                f.wedge_connects(true);
                Unwedge(f.clone())
            })
            .collect();
        let (fleet, _store) = managed(&[
            ("b", wedged[0].clone()),
            ("c", wedged[1].clone()),
            ("e", wedged[2].clone()),
        ]);
        fleet
            .apply_flock(
                &flock_of(&[("a", 1), ("b", 1), ("c", 1), ("e", 1)]),
                &fast(),
            )
            .await;
        let deadline = Instant::now() + Duration::from_secs(5);
        while wedged.iter().any(|f| f.wedged() == 0) {
            assert!(Instant::now() < deadline, "an actor never reached connect");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let started = Instant::now();
        let diff = fleet.apply_flock(&flock_of(&[("a", 1)]), &fast()).await;
        let took = started.elapsed();
        assert_eq!(diff.shutting_down, vec!["b", "c", "e"], "{diff:?}");
        assert!(took < Duration::from_secs(4), "took {took:?}");
    }

    /// An actor that never reads its queue: a request to it fails with
    /// `NoReply` after the bound, and a pass that placed a task on it ends
    /// then too, leaving the task queued. Its reservation stays, though: the
    /// command sent to the actor's channel cannot be un-sent, so the slot it
    /// holds is not free until the actor genuinely answers (never, here).
    #[tokio::test]
    async fn a_request_to_a_stuck_actor_fails_after_the_bound() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut stuck = crate::machine::pull_machine("a".into(), 1, vec![]);
        let (tx, _unread) = tokio::sync::mpsc::channel(8);
        stuck.tx = tx;
        stuck.status.write().unwrap().channel = crate::machine::ChannelState::Connected;
        let fleet = Fleet::new(vec![stuck.clone()], store.clone());
        assert_eq!(fleet.reply_wait(), Duration::from_secs(180), "3 x 60s");
        fleet.set_config(&PastorConfig {
            request_timeout: "1s".into(),
            ..Default::default()
        });
        assert_eq!(
            fleet.reply_wait(),
            Duration::from_secs(3),
            "3 x request_timeout"
        );
        let wait = Duration::from_millis(200);
        fleet.set_reply_wait(wait);

        let started = Instant::now();
        let err = fleet.bounded("a", stuck.read(1, 10)).await.unwrap_err();
        assert!(err.downcast_ref::<NoReply>().is_some(), "{err:#}");
        assert!(
            err.to_string().ends_with("within 200ms"),
            "a subsecond bound keeps its unit: {err:#}"
        );
        assert!(started.elapsed() >= wait);

        let t = queue_on(&store, "stuck", None);
        let started = Instant::now();
        tokio::time::timeout(Duration::from_secs(2), fleet.dispatch_queued())
            .await
            .expect("the pass ends after the bound");
        assert!(started.elapsed() >= wait, "it did send to a");
        assert_eq!(
            store.get_task(t.id).unwrap().unwrap().state,
            TaskState::Queued
        );
        assert!(
            fleet.in_flight.holds(t.id),
            "still in flight: the actor may yet read and claim it"
        );
        let err = fleet
            .set_priority_preempting(t.id, Priority::Critical, "test", true)
            .await
            .unwrap_err();
        assert!(
            matches!(err, PriorityError::InFlight(id) if id == t.id),
            "{err:?}"
        );
    }

    /// The bootstrap state a head keeps for a machine (`BootstrapReport`):
    /// dispatch places nothing on one that is pending or failed, and a
    /// machine with no state is not held.
    mod bootstrap {
        use super::*;
        use crate::bootstrap::{Isolation, Outcome, StepResult};
        use crate::store::{BootstrapState, MachineState};

        fn state(machine: &str, bootstrap: BootstrapState) -> MachineState {
            MachineState {
                machine: machine.into(),
                bootstrap,
                isolation: None,
                failed_step: None,
                why: None,
                host_key: None,
                herdr_version: None,
                pastor_version: None,
                generation: 1,
                updated_at: chrono::Utc::now(),
            }
        }

        fn ok_report(machine: &str) -> IpcRequest {
            IpcRequest::BootstrapReport {
                machine: machine.into(),
                result: Outcome {
                    ok: true,
                    isolation: Isolation::None,
                    steps: vec![StepResult::done("pastor")],
                    machine_key: None,
                    agent_key: None,
                    host_key: Some("ssh-ed25519 AAAAhost".into()),
                    herdr_version: Some("0.9.5".into()),
                    pastor_version: Some("0.10.0".into()),
                    check: None,
                },
            }
        }

        fn failed_report(machine: &str) -> IpcRequest {
            IpcRequest::BootstrapReport {
                machine: machine.into(),
                result: Outcome {
                    ok: false,
                    isolation: Isolation::None,
                    steps: vec![
                        StepResult::done("facts"),
                        StepResult::failed(
                            "herdr",
                            "herdr is not on the PATH",
                            "run pastor machine install pi --kind herdr",
                        ),
                    ],
                    machine_key: None,
                    agent_key: None,
                    host_key: None,
                    herdr_version: None,
                    pastor_version: None,
                    check: None,
                },
            }
        }

        async fn one_slot() -> (Daemon, tempfile::TempDir) {
            daemon_with_flock(flock_of(&[("a", 1)]), &[("a", 1, FakeHerdr::new())]).await
        }

        fn task_state(d: &Daemon, t: &Task) -> TaskState {
            d.store().get_task(t.id).unwrap().unwrap().state
        }

        /// A pending machine gets no task, though it is connected and has
        /// room; once its report says ok, the task that waited starts at
        /// once, without waiting for the next tick.
        #[tokio::test]
        async fn pending_machine_gets_no_task() {
            let (d, _tmp) = one_slot().await;
            d.store()
                .put_machine_state(&state("a", BootstrapState::Pending))
                .unwrap();
            let t = insert_run(&d, spec());
            d.fleet().dispatch_queued().await;
            assert_eq!(task_state(&d, &t), TaskState::Queued);
            // A task pinned to it waits too.
            let pinned = insert_run(
                &d,
                DispatchSpec {
                    machine: Some("a".into()),
                    ..spec()
                },
            );
            d.fleet().dispatch_queued().await;
            assert_eq!(task_state(&d, &pinned), TaskState::Queued);

            let resp = d.handle(ok_report("a")).await;
            assert!(matches!(resp, IpcResponse::Text(_)), "{resp:?}");
            let running = [&t, &pinned]
                .iter()
                .filter(|t| task_state(&d, t) == TaskState::Running)
                .count();
            assert_eq!(running, 1, "the one slot goes to a task at once");
        }

        /// A failed machine is held the same way, and a machine that never
        /// had a state is not.
        #[tokio::test]
        async fn a_failed_machine_gets_no_task_and_one_without_state_still_dispatches() {
            let (d, _tmp) = one_slot().await;
            d.store()
                .put_machine_state(&state("a", BootstrapState::Failed))
                .unwrap();
            let t = insert_run(&d, spec());
            d.fleet().dispatch_queued().await;
            assert_eq!(task_state(&d, &t), TaskState::Queued);
        }

        #[tokio::test]
        async fn machine_without_state_still_dispatches() {
            let (d, _tmp) = one_slot().await;
            assert!(d.store().machine_state("a").unwrap().is_none());
            let t = insert_run(&d, spec());
            d.fleet().dispatch_queued().await;
            assert_eq!(task_state(&d, &t), TaskState::Running);
        }

        /// A failed report keeps the step and why (the fix folded into it),
        /// raises `machine.bootstrap_failed` with them, holds the machine,
        /// and an ok report after it raises `machine.bootstrapped`.
        #[tokio::test]
        async fn failed_report_emits_event() {
            let (d, _tmp) = one_slot().await;
            let mut rx = d.subscribe();
            let resp = d.handle(failed_report("a")).await;
            assert!(matches!(resp, IpcResponse::Text(_)), "{resp:?}");
            let ev = rx.try_recv().expect("an event");
            assert_eq!(ev.kind, "machine.bootstrap_failed");
            assert_eq!(ev.machine.as_deref(), Some("a"));
            let detail = ev.detail.unwrap();
            assert_eq!(detail["step"], "herdr");
            assert_eq!(
                detail["why"],
                "herdr is not on the PATH; fix: run pastor machine install pi --kind herdr"
            );
            let s = d.store().machine_state("a").unwrap().unwrap();
            assert_eq!(s.bootstrap, BootstrapState::Failed);
            assert_eq!(s.failed_step.as_deref(), Some("herdr"));
            let t = insert_run(&d, spec());
            d.fleet().dispatch_queued().await;
            assert_eq!(task_state(&d, &t), TaskState::Queued);

            d.handle(ok_report("a")).await;
            let ev = rx.try_recv().expect("an event");
            assert_eq!(ev.kind, "machine.bootstrapped");
            let s = d.store().machine_state("a").unwrap().unwrap();
            assert_eq!(s.bootstrap, BootstrapState::Ok);
            assert_eq!(s.failed_step, None);
            assert_eq!(s.herdr_version.as_deref(), Some("0.9.5"));
            assert_eq!(s.isolation.as_deref(), Some("none"));
        }

        /// A machine the flock lacks is refused; a host key on record is
        /// never replaced by a report, and control characters in what a
        /// machine reports do not reach the store.
        #[tokio::test]
        async fn a_report_is_checked_and_cleaned() {
            let (d, _tmp) = one_slot().await;
            assert_eq!(
                error_code(d.handle(ok_report("nope")).await),
                "unknown_machine"
            );
            d.handle(ok_report("a")).await;
            let IpcRequest::BootstrapReport { mut result, .. } = ok_report("a") else {
                unreachable!()
            };
            result.host_key = Some("ssh-ed25519 AAAAother".into());
            let resp = d
                .handle(IpcRequest::BootstrapReport {
                    machine: "a".into(),
                    result: result.clone(),
                })
                .await;
            assert_eq!(error_code(resp), "host_key_mismatch");
            let s = d.store().machine_state("a").unwrap().unwrap();
            assert_eq!(s.host_key.as_deref(), Some("ssh-ed25519 AAAAhost"));

            result.host_key = None;
            result.ok = false;
            result.steps = vec![StepResult::failed("x\u{1b}[31my", "bad\u{7}\ntext", "")];
            d.handle(IpcRequest::BootstrapReport {
                machine: "a".into(),
                result,
            })
            .await;
            let s = d.store().machine_state("a").unwrap().unwrap();
            assert_eq!(s.failed_step.as_deref(), Some("x[31my"));
            assert_eq!(s.why.as_deref(), Some("badtext"));
            assert_eq!(s.host_key.as_deref(), Some("ssh-ed25519 AAAAhost"), "kept");
        }

        /// `machine describe` carries the state, and a pending pull machine
        /// claims nothing until its report says ok.
        #[tokio::test]
        async fn the_head_describes_the_state_and_holds_a_pending_pull_machine() {
            let (d, _tmp) = pull_daemon(2).await;
            let t = queued(&d, "work", Some("laptop")).await;
            d.store()
                .put_machine_state(&state("laptop", BootstrapState::Pending))
                .unwrap();
            assert!(claim(&d, 2, false).await.is_empty());
            let IpcResponse::MachineDescription(m) = d
                .handle(IpcRequest::MachineDescribe {
                    name: "laptop".into(),
                })
                .await
            else {
                panic!("not a description")
            };
            assert_eq!(
                m.bootstrap.map(|b| b.bootstrap),
                Some(BootstrapState::Pending)
            );
            d.handle(ok_report("laptop")).await;
            assert_eq!(claim(&d, 2, false).await, ["work"]);
            assert_eq!(task_state(&d, &t), TaskState::Starting);
        }
    }

    /// Where the last `machine check` found that a machine's agents cannot
    /// reach the head, dispatch skips it as it skips one under its room
    /// floor: a task waits with the reason, a `--now` task lands all the
    /// same, a machine that reaches it or has no result is placed as ever.
    mod reach {
        use super::*;
        use crate::machine::Reach;
        use crate::machine_check::{REACH_ROW, Report};
        use crate::provision::run::{Row, Status};

        fn set(d: &Daemon, ok: Option<bool>) {
            d.fleet().get("a").unwrap().status.write().recover().reach = ok.map(|ok| Reach {
                ok,
                at: chrono::Utc::now(),
                why: None,
            });
        }

        async fn one() -> (Daemon, tempfile::TempDir) {
            daemon_with_flock(flock_of(&[("a", 2)]), &[("a", 2, FakeHerdr::new())]).await
        }

        fn task(d: &Daemon, t: &Task) -> Task {
            d.store().get_task(t.id).unwrap().unwrap()
        }

        #[tokio::test]
        async fn an_unreachable_machine_is_skipped_and_the_task_says_why() {
            let (d, _tmp) = one().await;
            set(&d, Some(false));
            let waiting = insert_run(&d, spec());
            d.fleet().dispatch_queued().await;
            let t = task(&d, &waiting);
            assert_eq!(t.state, TaskState::Queued);
            assert_eq!(
                t.error.as_deref(),
                Some("waiting for a machine: a cannot reach the head")
            );
            let entry =
                crate::queue::entries(vec![t], &d.fleet().queue_views(), "default", &|_, _| true);
            assert_eq!(
                entry[0].why,
                "waiting for a machine: a cannot reach the head"
            );
            // A task pinned there waits too.
            let pinned = insert_run(
                &d,
                DispatchSpec {
                    machine: Some("a".into()),
                    ..spec()
                },
            );
            d.fleet().dispatch_queued().await;
            assert_eq!(task(&d, &pinned).state, TaskState::Queued);

            // --now ignores it, as it ignores the floor.
            let now = insert_run(
                &d,
                DispatchSpec {
                    now: true,
                    machine: Some("a".into()),
                    ..spec()
                },
            );
            d.fleet().dispatch_queued().await;
            assert_eq!(task(&d, &now).state, TaskState::Running);

            // A later check that passes frees it.
            set(&d, Some(true));
            d.fleet().dispatch_queued().await;
            assert_eq!(task(&d, &waiting).state, TaskState::Running);
            assert_eq!(task(&d, &waiting).error, None, "the note goes");
        }

        #[tokio::test]
        async fn a_reachable_machine_and_one_with_no_result_take_a_task() {
            for result in [Some(true), None] {
                let (d, _tmp) = one().await;
                set(&d, result);
                let t = insert_run(&d, spec());
                d.fleet().dispatch_queued().await;
                assert_eq!(task(&d, &t).state, TaskState::Running, "{result:?}");
            }
        }

        fn report(row: Option<Status>) -> Report {
            Report {
                machine: "a".into(),
                provisioner: None,
                version: None,
                agents: vec![],
                rows: row
                    .map(|status| Row {
                        id: REACH_ROW.into(),
                        agent: None,
                        status,
                        first_line: "ssh to h from a failed".into(),
                        reason: Some("Permission denied (publickey)".into()),
                    })
                    .into_iter()
                    .collect(),
            }
        }

        /// What a check found is what the head keeps: a failure holds the
        /// machine and keeps ssh's words, a pass frees it (and says so, for
        /// the caller to place what waited), a check without the row or
        /// whose row could not run leaves what there was.
        #[tokio::test]
        async fn a_check_sets_the_last_reach_result() {
            let (d, _tmp) = one().await;
            let reach = || d.fleet().get("a").unwrap().snapshot().reach;
            assert!(reach().is_none());
            assert!(!d.fleet().note_reach("a", &report(None)));
            assert!(reach().is_none(), "no row, no result");
            assert!(!d.fleet().note_reach("a", &report(Some(Status::Fail))));
            let held = reach().unwrap();
            assert!(!held.ok);
            assert_eq!(held.why.as_deref(), Some("Permission denied (publickey)"));
            assert!(!d.fleet().note_reach("a", &report(Some(Status::Error))));
            assert!(!reach().unwrap().ok, "an error row changes nothing");
            assert!(d.fleet().note_reach("a", &report(Some(Status::Pass))));
            assert!(reach().unwrap().ok);
            assert!(!d.fleet().note_reach("a", &report(Some(Status::Pass))));
            assert!(!d.fleet().note_reach("ghost", &report(Some(Status::Pass))));
        }
    }

    /// `ArtifactLocate`: what the cache holds, with its sum and size, and
    /// nothing else: a name that is not one plain part each is refused, a
    /// missing artifact is `artifact_not_cached` (it is not fetched), and a
    /// file that no longer hashes to its recorded sum is not held.
    #[tokio::test]
    async fn artifact_locate_answers_from_the_cache_only() {
        use sha2::{Digest, Sha256};
        let (d, _tmp) =
            daemon_with_flock(flock_of(&[("a", 1)]), &[("a", 1, FakeHerdr::new())]).await;
        let locate = |name: &str, version: &str, target: &str| IpcRequest::ArtifactLocate {
            name: name.into(),
            version: version.into(),
            target: target.into(),
        };
        let a = crate::artifacts::Artifact::new("pastor", "0.10.0", "x86_64-unknown-linux-musl")
            .unwrap();
        let dest = crate::artifacts::cache_path(&d.paths, &a);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, b"binary bytes").unwrap();
        let sum: String = Sha256::digest(b"binary bytes")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        std::fs::write(dest.with_file_name("pastor.sha256"), format!("{sum}\n")).unwrap();

        let resp = d
            .handle(locate("pastor", "0.10.0", "x86_64-unknown-linux-musl"))
            .await;
        let IpcResponse::Artifact { path, sha256, size } = resp else {
            panic!("not an artifact")
        };
        assert_eq!(
            (path.as_str(), sha256.as_str(), size),
            (dest.to_str().unwrap(), sum.as_str(), 12)
        );

        // Not held: a version the cache lacks is not downloaded.
        assert_eq!(
            error_code(
                d.handle(locate("pastor", "9.9.9", "x86_64-unknown-linux-musl"))
                    .await
            ),
            "artifact_not_cached"
        );
        assert!(!d.paths.state_dir.join("artifacts/pastor/9.9.9").exists());
        // Names that could leave the cache.
        for (n, v, t) in [
            ("..", "0.10.0", "t"),
            ("pastor", "../../x", "t"),
            ("pastor", "0.10.0", "a/b"),
            ("", "0.10.0", "t"),
        ] {
            assert_eq!(
                error_code(d.handle(locate(n, v, t)).await),
                "invalid_artifact",
                "{n} {v} {t}"
            );
        }
        // A file edited after it was recorded is not held.
        std::fs::write(&dest, b"binary bytez").unwrap();
        assert_eq!(
            error_code(
                d.handle(locate("pastor", "0.10.0", "x86_64-unknown-linux-musl"))
                    .await
            ),
            "artifact_not_cached"
        );
    }

    /// `reserved` keeps slots of a machine for panes pastor did not start:
    /// dispatch places at most `max_agents - reserved` of its own tasks
    /// there, the rest wait, `--now` looks past it, and a machine with none
    /// reserved takes all its slots.
    mod reserved {
        use super::*;

        async fn two_slots(reserved: u32) -> (Daemon, tempfile::TempDir) {
            let flock = Flock {
                flocks: vec![],
                machines: vec![MachineConfig {
                    reserved,
                    ..machine("a", 2)
                }],
            };
            daemon_with_flock(flock, &[("a", 2, FakeHerdr::new())]).await
        }

        fn state(d: &Daemon, t: &Task) -> TaskState {
            d.store().get_task(t.id).unwrap().unwrap().state
        }

        #[tokio::test]
        async fn a_machine_with_a_reserved_slot_takes_one_task_fewer() {
            let (d, _tmp) = two_slots(1).await;
            let first = insert_run(&d, spec());
            let second = insert_run(&d, spec());
            d.fleet().dispatch_queued().await;
            assert_eq!(state(&d, &first), TaskState::Running);
            assert_eq!(state(&d, &second), TaskState::Queued);
            let entry = crate::queue::entries(
                vec![d.store().get_task(second.id).unwrap().unwrap()],
                &d.fleet().queue_views(),
                "default",
                &|_, _| true,
            );
            assert!(entry[0].why.contains("full"), "{}", entry[0].why);
            // --now runs past it, as it runs past max_agents.
            let now = insert_run(
                &d,
                DispatchSpec {
                    now: true,
                    machine: Some("a".into()),
                    ..spec()
                },
            );
            d.fleet().dispatch_queued().await;
            assert_eq!(state(&d, &now), TaskState::Running);
            assert_eq!(state(&d, &second), TaskState::Queued);
            // The status carries it for `machine list` and `describe`.
            let status = d
                .fleet()
                .statuses()
                .into_iter()
                .find(|s| s.name == "a")
                .unwrap();
            assert_eq!(status.reserved, 1);
        }

        #[tokio::test]
        async fn a_machine_with_none_reserved_takes_all_its_slots() {
            let (d, _tmp) = two_slots(0).await;
            let first = insert_run(&d, spec());
            let second = insert_run(&d, spec());
            d.fleet().dispatch_queued().await;
            assert_eq!(state(&d, &first), TaskState::Running);
            assert_eq!(state(&d, &second), TaskState::Running);
        }

        /// Everything reserved is a machine pastor places nothing on.
        #[tokio::test]
        async fn a_machine_with_every_slot_reserved_takes_none() {
            let (d, _tmp) = two_slots(2).await;
            let t = insert_run(&d, spec());
            d.fleet().dispatch_queued().await;
            assert_eq!(state(&d, &t), TaskState::Queued);
        }
    }

    /// `TaskSweep` through the head: a dry run lists a failed task's clean
    /// worktree and removes nothing, `yes` removes it, a machine the head
    /// has not is refused, and a pull machine asked for by name is said not
    /// to be swept from here.
    #[tokio::test]
    async fn task_sweep_asks_the_machines() {
        let fake = FakeHerdr::new();
        let (d, _tmp) = daemon_with_flock(flock_of(&[("a", 1)]), &[("a", 1, fake.clone())]).await;
        let mut t = insert_run(&d, spec());
        t.machine = Some("a".into());
        t.state = TaskState::Failed;
        t.spec.checkout = Some(Box::new(crate::task::Checkout {
            branch: "pastor/t-1".into(),
            path: "/srv/worktrees/repo/t-1".into(),
            already_open: false,
        }));
        d.store().update_task(&mut t).unwrap();

        let sweep = |machine: Option<&str>, yes| IpcRequest::TaskSweep {
            machine: machine.map(str::to_string),
            yes,
        };
        let IpcResponse::Swept { entries, failed } = d.handle(sweep(None, false)).await else {
            panic!("not a sweep")
        };
        assert!(failed.is_empty(), "{failed:?}");
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].action, crate::machine::SweepAction::WouldRemove);
        assert!(fake.removed_checkouts().is_empty());

        let IpcResponse::Swept { entries, .. } = d.handle(sweep(Some("a"), true)).await else {
            panic!("not a sweep")
        };
        assert_eq!(entries[0].action, crate::machine::SweepAction::Removed);
        assert_eq!(fake.removed_checkouts(), ["/srv/worktrees/repo/t-1"]);

        assert_eq!(
            error_code(d.handle(sweep(Some("ghost"), true)).await),
            "unknown_machine"
        );
    }

    /// `bootstrap::push`: the head sets a machine up over ssh. A scripted
    /// runner stands in for ssh, ssh-keyscan and ssh-keygen, so what the head
    /// does is read off the calls it made; the cache holds the two binaries.
    mod push {
        use super::*;
        use crate::bootstrap::push::{PushOpts, PushRunner, push};
        use crate::bootstrap::{Isolation, Outcome, StepResult};
        use crate::setup::CmdOutput;
        use crate::store::{BootstrapState, MachineState};
        use sha2::{Digest, Sha256};
        use std::path::Path;
        use std::sync::Mutex;

        const LOGIN: &str = "me@pi-9.example";
        const KEY1: &str = "ssh-ed25519 AAAAhostkeyONE";
        const KEY2: &str = "ssh-ed25519 AAAAhostkeyTWO";
        const AGENT: &str = "ssh-ed25519 AAAAagentkeyONE pastor-agent";

        #[derive(Debug, Clone)]
        struct Call {
            program: String,
            args: Vec<String>,
            stdin: Option<std::path::PathBuf>,
        }

        impl Call {
            fn line(&self) -> String {
                format!("{} {}", self.program, self.args.join(" "))
            }
        }

        type Answer = Box<dyn Fn(&Call) -> Option<CmdOutput> + Send + Sync>;

        /// The machine and its network, as a push sees them. `scan` is the
        /// ed25519 key the machine offers, `bootstrap` what `pastor bootstrap`
        /// prints there; `refuse_pinned` makes ssh itself fail its host key
        /// check; `extra` answers first, for a test to watch or to break a
        /// step.
        struct Machine {
            calls: Mutex<Vec<Call>>,
            scan: Mutex<String>,
            bootstrap: Mutex<Outcome>,
            refuse_pinned: Mutex<bool>,
            extra: Mutex<Option<Answer>>,
        }

        impl Machine {
            fn new() -> Arc<Machine> {
                Arc::new(Machine {
                    calls: Mutex::new(vec![]),
                    scan: Mutex::new(KEY1.into()),
                    bootstrap: Mutex::new(ok_outcome()),
                    refuse_pinned: Mutex::new(false),
                    extra: Mutex::new(None),
                })
            }

            fn calls(&self) -> Vec<Call> {
                self.calls.lock().unwrap().clone()
            }

            fn count(&self, program: &str) -> usize {
                self.calls().iter().filter(|c| c.program == program).count()
            }

            /// The calls that went to the machine over ssh (not `-G`, not
            /// closing a master).
            fn remote(&self) -> Vec<Call> {
                self.calls()
                    .into_iter()
                    .filter(|c| {
                        c.program == "ssh" && !c.args.iter().any(|a| a == "-G" || a == "-O")
                    })
                    .collect()
            }
        }

        impl PushRunner for Arc<Machine> {
            fn run(
                &self,
                program: &str,
                args: &[String],
                stdin: Option<&Path>,
            ) -> std::io::Result<CmdOutput> {
                let call = Call {
                    program: program.into(),
                    args: args.to_vec(),
                    stdin: stdin.map(Path::to_path_buf),
                };
                self.calls.lock().unwrap().push(call.clone());
                let ok = |out: String| CmdOutput {
                    success: true,
                    stdout: out,
                    stderr: String::new(),
                };
                if let Some(answer) = self.extra.lock().unwrap().as_ref()
                    && let Some(out) = answer(&call)
                {
                    return Ok(out);
                }
                let line = call.line();
                Ok(match program {
                    "ssh-keyscan" => ok(format!("pi-9.example {}\n", self.scan.lock().unwrap())),
                    "ssh-keygen" => {
                        let f = args.last().unwrap();
                        let text = std::fs::read_to_string(f).unwrap_or_default();
                        let b64 = text.split_whitespace().nth(2).unwrap_or("").to_string();
                        ok(format!("256 SHA256:fp-{b64} pi-9.example (ED25519)\n"))
                    }
                    "ssh" if args.iter().any(|a| a == "-G") => {
                        ok("hostname pi-9.example\nport 22\n".into())
                    }
                    "ssh" if args.iter().any(|a| a == "-O") => ok(String::new()),
                    "ssh" if *self.refuse_pinned.lock().unwrap() => CmdOutput {
                        success: false,
                        stdout: String::new(),
                        stderr: "Host key verification failed.\n".into(),
                    },
                    "ssh" if line.contains("uname -sm") => ok("Linux x86_64\n64\n".into()),
                    "ssh" if line.contains("cat >") => {
                        let sum = sha_of(&std::fs::read(stdin.unwrap()).unwrap());
                        ok(format!("{sum}\n"))
                    }
                    "ssh" if line.contains("pastor bootstrap") => ok(format!(
                        "{}\n",
                        serde_json::to_string(&*self.bootstrap.lock().unwrap()).unwrap()
                    )),
                    other => panic!("an unexpected call: {other} {args:?}"),
                })
            }
        }

        fn sha_of(bytes: &[u8]) -> String {
            Sha256::digest(bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        }

        fn ok_outcome() -> Outcome {
            Outcome {
                ok: true,
                isolation: Isolation::None,
                steps: vec![StepResult::done("pastor")],
                machine_key: Some("ssh-ed25519 AAAAmachine".into()),
                agent_key: Some(AGENT.into()),
                host_key: Some(KEY1.into()),
                herdr_version: Some("0.9.5".into()),
                pastor_version: Some(env!("CARGO_PKG_VERSION").into()),
                check: None,
            }
        }

        fn failed_outcome() -> Outcome {
            Outcome {
                ok: false,
                steps: vec![
                    StepResult::done("facts"),
                    StepResult::failed(
                        "prerequisites",
                        "missing: curl",
                        "the machine's owner installs curl",
                    ),
                ],
                machine_key: None,
                agent_key: None,
                host_key: None,
                ..ok_outcome()
            }
        }

        struct NoFetch;
        impl crate::artifacts::Fetch for NoFetch {
            fn get(&self, url: &str, _to: &Path) -> anyhow::Result<()> {
                panic!("the cache should hold everything: {url}")
            }
        }

        struct Fixture {
            d: Daemon,
            _tmp: tempfile::TempDir,
            machine: Arc<Machine>,
            auth: std::path::PathBuf,
        }

        /// A head with one machine, `a`, and the cache holding pastor and
        /// herdr for linux x86_64.
        async fn fixture() -> Fixture {
            let (d, tmp) =
                daemon_with_flock(flock_of(&[("a", 1)]), &[("a", 1, FakeHerdr::new())]).await;
            for (name, version, target) in [
                (
                    "pastor",
                    env!("CARGO_PKG_VERSION"),
                    "x86_64-unknown-linux-musl",
                ),
                (
                    "herdr",
                    crate::artifacts::DEFAULT_HERDR_VERSION,
                    "linux-x86_64",
                ),
            ] {
                let a = crate::artifacts::Artifact::new(name, version, target).unwrap();
                let file = crate::artifacts::cache_path(&d.paths, &a);
                std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                let bytes = format!("{name} binary").into_bytes();
                std::fs::write(&file, &bytes).unwrap();
                std::fs::write(
                    file.with_file_name(format!("{name}.sha256")),
                    format!("{}\n", sha_of(&bytes)),
                )
                .unwrap();
            }
            let auth = tmp.path().join("authorized_keys");
            std::fs::write(&auth, "ssh-ed25519 AAAAmine me@laptop\n").unwrap();
            Fixture {
                d,
                _tmp: tmp,
                machine: Machine::new(),
                auth,
            }
        }

        fn entry() -> MachineConfig {
            MachineConfig {
                ssh: Some(LOGIN.into()),
                local: false,
                command: None,
                ..machine("pi-9", 2)
            }
        }

        impl Fixture {
            fn opts(&self, add: bool, host_key: Option<&str>) -> PushOpts {
                PushOpts {
                    host_key: host_key.map(str::to_string),
                    machine: add.then(entry),
                    head: "head@head.example".into(),
                    head_key: None,
                    artifacts: Default::default(),
                    fleet_versions: vec![],
                    authorized_keys: self.auth.clone(),
                    runner: Arc::new(self.machine.clone()),
                    fetch: Arc::new(NoFetch),
                }
            }

            async fn push(&self, add: bool, host_key: Option<&str>) -> anyhow::Result<Outcome> {
                push(
                    &self.d.paths,
                    &self.d.store,
                    &self.d.fleet,
                    "pi-9",
                    LOGIN,
                    self.opts(add, host_key),
                )
                .await
            }

            fn state(&self) -> Option<MachineState> {
                self.d.store().machine_state("pi-9").unwrap()
            }

            fn known_hosts(&self) -> String {
                std::fs::read_to_string(self.d.paths.known_hosts_file()).unwrap_or_default()
            }
        }

        fn failed_at(o: &Outcome) -> (&str, String) {
            let s = o
                .steps
                .iter()
                .find(|s| s.status == crate::bootstrap::StepStatus::Failed)
                .expect("a failed step");
            (s.step.as_str(), s.why.clone().unwrap_or_default())
        }

        /// The machine is on record as pending before anything touches it,
        /// is pending while its own steps run, and ends ok, with the key
        /// pastor scanned and pinned.
        #[tokio::test]
        async fn push_writes_pending_then_ok() {
            let f = fixture().await;
            let mut events = f.d.subscribe();
            let seen = Arc::new(Mutex::new(Vec::new()));
            let (store, log) = (f.d.store().clone(), seen.clone());
            *f.machine.extra.lock().unwrap() = Some(Box::new(move |c: &Call| {
                if c.program == "ssh-keyscan" || c.line().contains("pastor bootstrap") {
                    let state = store.machine_state("pi-9").unwrap().map(|s| s.bootstrap);
                    log.lock().unwrap().push((c.program.clone(), state));
                }
                None
            }));
            let outcome = f.push(true, None).await.unwrap();
            assert!(outcome.ok, "{outcome:?}");
            assert_eq!(
                *seen.lock().unwrap(),
                [
                    ("ssh-keyscan".to_string(), Some(BootstrapState::Pending)),
                    ("ssh".to_string(), Some(BootstrapState::Pending)),
                ]
            );
            let s = f.state().unwrap();
            assert_eq!(s.bootstrap, BootstrapState::Ok);
            assert_eq!(s.host_key.as_deref(), Some(KEY1));
            assert_eq!(f.known_hosts(), format!("pi-9.example {KEY1}\n"));
            assert!(f.d.fleet().flock().get("pi-9").is_some(), "added");
            let ev = events.try_recv().expect("an event");
            assert_eq!(ev.kind, "machine.bootstrapped");
            // The copies were checked on the machine against the cache's sum.
            let copies: Vec<String> = f
                .machine
                .remote()
                .iter()
                .filter(|c| c.line().contains("cat >"))
                .map(|c| c.line())
                .collect();
            assert_eq!(copies.len(), 2, "{copies:?}");
            assert!(
                f.machine
                    .remote()
                    .iter()
                    .filter(|c| c.line().contains("cat >"))
                    .all(|c| c.stdin.is_some()),
                "the file goes on stdin, not in the command"
            );
            assert!(copies[0].contains(&sha_of(b"pastor binary")), "{copies:?}");
            assert!(copies[1].contains(&sha_of(b"herdr binary")), "{copies:?}");
        }

        /// A first run that fails (at prerequisites) is on record as failed
        /// and the machine stays in the flock; the same command again goes
        /// on, with no `machine_exists`, and ends ok. The host is not
        /// scanned a second time.
        #[tokio::test]
        async fn rerun_after_failure_resumes() {
            let f = fixture().await;
            *f.machine.bootstrap.lock().unwrap() = failed_outcome();
            let first = f.push(true, None).await.unwrap();
            assert!(!first.ok);
            assert_eq!(failed_at(&first).0, "prerequisites");
            let s = f.state().unwrap();
            assert_eq!(s.bootstrap, BootstrapState::Failed);
            assert_eq!(s.failed_step.as_deref(), Some("prerequisites"));
            assert!(f.d.fleet().flock().get("pi-9").is_some());

            *f.machine.bootstrap.lock().unwrap() = ok_outcome();
            let second = f.push(true, None).await.unwrap();
            assert!(second.ok, "{second:?}");
            assert_eq!(f.state().unwrap().bootstrap, BootstrapState::Ok);
            assert_eq!(f.machine.count("ssh-keyscan"), 1, "pinned once");
            // An ok machine of that name is another matter.
            let err = f.push(true, None).await.unwrap_err();
            assert_eq!(
                err.downcast_ref::<crate::cli::CliError>().unwrap().code,
                "machine_exists"
            );
        }

        /// A `--host-key` that is not the key the machine offers stops the
        /// push before anything is pinned, copied or run, whether it is
        /// given as a fingerprint or as the key; the right one goes on.
        #[tokio::test]
        async fn host_key_mismatch_stops() {
            for wrong in ["SHA256:fp-AAAAsomethingelse", KEY2] {
                let f = fixture().await;
                let outcome = f.push(true, Some(wrong)).await.unwrap();
                assert!(!outcome.ok);
                let (step, why) = failed_at(&outcome);
                assert_eq!(step, "host_key", "{wrong}");
                assert!(why.contains("--host-key"), "{why}");
                assert_eq!(f.known_hosts(), "", "nothing pinned: {wrong}");
                assert!(
                    f.machine.remote().is_empty(),
                    "nothing went to the machine: {:?}",
                    f.machine.remote()
                );
                let s = f.state().unwrap();
                assert_eq!(s.bootstrap, BootstrapState::Failed);
                assert_eq!(s.host_key, None);
            }
            for right in ["SHA256:fp-AAAAhostkeyONE", KEY1] {
                let f = fixture().await;
                let outcome = f.push(true, Some(right)).await.unwrap();
                assert!(outcome.ok, "{right}: {outcome:?}");
                assert_eq!(f.known_hosts(), format!("pi-9.example {KEY1}\n"));
            }
        }

        /// A pinned key stands: a rerun does not scan the host again, ssh
        /// itself refuses a machine whose key changed, and that stops the
        /// push at its first connection, keeping the pin and the machine's
        /// ok state. A scan that disagrees with what is on record, or a pin
        /// that disagrees with the record, stops it too, and no flag
        /// replaces either.
        #[tokio::test]
        async fn rerun_keeps_pinned_host_key_and_rejects_a_changed_one() {
            let f = fixture().await;
            assert!(f.push(true, None).await.unwrap().ok);
            let mut events = f.d.subscribe();

            let copies = |f: &Fixture| {
                f.machine
                    .remote()
                    .iter()
                    .filter(|c| c.line().contains("cat >"))
                    .count()
            };
            let copied_before = copies(&f);
            // The machine now offers another key; ssh checks it against the pin.
            *f.machine.scan.lock().unwrap() = KEY2.into();
            *f.machine.refuse_pinned.lock().unwrap() = true;
            let rerun = f.push(false, None).await.unwrap();
            assert!(!rerun.ok);
            let (step, _) = failed_at(&rerun);
            assert_eq!(step, "connect");
            let remove = rerun.steps.last().unwrap().fix.clone().unwrap();
            assert!(remove.contains("pastor machine remove pi-9"), "{remove}");
            assert_eq!(f.machine.count("ssh-keyscan"), 1, "never scanned again");
            assert_eq!(
                f.known_hosts(),
                format!("pi-9.example {KEY1}\n"),
                "pin kept"
            );
            let s = f.state().unwrap();
            assert_eq!(
                s.bootstrap,
                BootstrapState::Ok,
                "a machine that was ok stays ok"
            );
            assert_eq!(s.host_key.as_deref(), Some(KEY1));
            assert_eq!(events.try_recv().unwrap().kind, "machine.bootstrap_failed");
            assert_eq!(
                copies(&f),
                copied_before,
                "nothing was copied to the changed machine"
            );

            // The pin and the record disagree: stopped, before ssh.
            let g = fixture().await;
            g.d.store()
                .put_machine_state(&MachineState {
                    host_key: Some(KEY2.into()),
                    ..state_row("pi-9", BootstrapState::Failed)
                })
                .unwrap();
            std::fs::create_dir_all(g.d.paths.state_dir.clone()).unwrap();
            std::fs::write(
                g.d.paths.known_hosts_file(),
                format!("pi-9.example {KEY1}\n"),
            )
            .unwrap();
            let out = g.push(true, None).await.unwrap();
            let (step, why) = failed_at(&out);
            assert_eq!(step, "host_key");
            assert!(
                why.contains("pinned with host key") && why.contains("on record"),
                "{why}"
            );
            assert!(g.machine.remote().is_empty());

            // Nothing pinned, the record has a key, the scan offers another.
            let h = fixture().await;
            h.d.store()
                .put_machine_state(&MachineState {
                    host_key: Some(KEY1.into()),
                    ..state_row("pi-9", BootstrapState::Failed)
                })
                .unwrap();
            *h.machine.scan.lock().unwrap() = KEY2.into();
            let out = h.push(true, None).await.unwrap();
            let (step, why) = failed_at(&out);
            assert_eq!(step, "host_key");
            assert!(
                why.contains("offered host key") && why.contains("on record"),
                "{why}"
            );
            assert_eq!(h.known_hosts(), "", "the new key is not pinned");
            assert!(h.machine.remote().is_empty());
        }

        fn state_row(machine: &str, bootstrap: BootstrapState) -> MachineState {
            MachineState {
                machine: machine.into(),
                bootstrap,
                isolation: None,
                failed_step: None,
                why: None,
                host_key: None,
                herdr_version: None,
                pastor_version: None,
                generation: 1,
                updated_at: chrono::Utc::now(),
            }
        }

        /// Everything goes to the machine as its login user, and the entry
        /// keeps that login; nothing calls sudo.
        #[tokio::test]
        async fn push_keeps_login_ssh() {
            let f = fixture().await;
            assert!(f.push(true, None).await.unwrap().ok);
            for c in f.machine.remote() {
                assert!(
                    c.args.windows(2).any(|w| w == ["--", LOGIN])
                        || c.args.contains(&LOGIN.to_string()),
                    "{}",
                    c.line()
                );
                assert!(!c.line().contains("sudo"), "{}", c.line());
            }
            assert!(f.machine.remote().len() >= 4, "{:?}", f.machine.remote());
            let flock = f.d.fleet().flock();
            assert_eq!(flock.get("pi-9").unwrap().ssh.as_deref(), Some(LOGIN));
            let on_disk = crate::config::flock::Flock::load(&f.d.paths.flock_file()).unwrap();
            assert_eq!(on_disk.get("pi-9").unwrap().ssh.as_deref(), Some(LOGIN));
            // Every remote command is checked strictly against pastor's own
            // known_hosts, and never through a shared master.
            for c in f.machine.remote() {
                let joined = c.args.join(" ");
                assert!(joined.contains("StrictHostKeyChecking=yes"), "{joined}");
                assert!(joined.contains("ControlPath=none"), "{joined}");
            }
        }

        /// The machine's agent key is on its marked line of the head's
        /// authorized_keys, locked to the agent bridge, beside the user's
        /// own; a failed run adds none, and a machine that reports a host key
        /// other than the pinned one is not registered.
        #[tokio::test]
        async fn agent_line_installed() {
            let f = fixture().await;
            *f.machine.bootstrap.lock().unwrap() = failed_outcome();
            assert!(!f.push(true, None).await.unwrap().ok);
            let lines = |f: &Fixture| -> Vec<String> {
                std::fs::read_to_string(&f.auth)
                    .unwrap()
                    .lines()
                    .map(str::to_string)
                    .collect()
            };
            assert_eq!(lines(&f), ["ssh-ed25519 AAAAmine me@laptop"]);

            *f.machine.bootstrap.lock().unwrap() = ok_outcome();
            assert!(f.push(true, None).await.unwrap().ok);
            let lines = lines(&f);
            assert_eq!(lines.len(), 2, "{lines:?}");
            assert_eq!(lines[0], "ssh-ed25519 AAAAmine me@laptop");
            let pastor = std::env::current_exe().unwrap();
            assert!(
                lines[1].starts_with(&format!(
                    "restrict,command=\"{} bridge --agent --machine pi-9\",",
                    pastor.display()
                )) && lines[1].contains("AAAAagentkeyONE")
                    && lines[1].ends_with("pastor:agent:pi-9"),
                "{}",
                lines[1]
            );

            // Another machine reporting a key that is not the pinned one.
            let g = fixture().await;
            let mut o = ok_outcome();
            o.host_key = Some(KEY2.into());
            *g.machine.bootstrap.lock().unwrap() = o;
            let out = g.push(true, None).await.unwrap();
            assert!(!out.ok);
            assert_eq!(failed_at(&out).0, "host_key");
            assert_eq!(
                std::fs::read_to_string(&g.auth).unwrap(),
                "ssh-ed25519 AAAAmine me@laptop\n"
            );
            assert_eq!(g.state().unwrap().bootstrap, BootstrapState::Failed);
        }
    }

    /// Join tokens on the head: a machine redeems one to join as a pull
    /// machine, once, under a name the token allows.
    mod join_tokens {
        use super::*;
        use crate::join::{Ask, HeadFiles};

        struct Head {
            d: Daemon,
            files: HeadFiles,
            _tmp: tempfile::TempDir,
            _dir: tempfile::TempDir,
        }

        async fn head() -> Head {
            let (d, tmp) =
                daemon_with_flock(flock_of(&[("a", 1)]), &[("a", 1, FakeHerdr::new())]).await;
            let dir = tempfile::tempdir().unwrap();
            let files = crate::join::tests::head_files(dir.path());
            Head {
                d,
                files,
                _tmp: tmp,
                _dir: dir,
            }
        }

        impl Head {
            fn token(&self, replace: Option<&str>, tags: &[&str]) -> String {
                let ask = Ask {
                    flock: None,
                    tags: tags.iter().map(|t| t.to_string()).collect(),
                    replace: replace.map(str::to_string),
                    ttl: Duration::from_secs(3600),
                };
                match self.d.join_create_with(
                    &crate::join::tests::FakeKeygen,
                    &self.files,
                    ask,
                    chrono::Utc::now(),
                ) {
                    IpcResponse::JoinCreated { id, .. } => id,
                    other => panic!("{other:?}"),
                }
            }

            /// Join with token `id` as `name`, with keys of its own.
            async fn join(&self, id: &str, name: &str) -> anyhow::Result<()> {
                let blob: String = name.chars().filter(char::is_ascii_alphanumeric).collect();
                self.d
                    .join_with(
                        &self.files,
                        id,
                        name,
                        &format!("ssh-ed25519 AAAAM{blob}"),
                        &format!("ssh-ed25519 AAAAA{blob}"),
                        "ssh-ed25519 AAAAHOST",
                    )
                    .await
            }

            fn keys(&self) -> String {
                std::fs::read_to_string(&self.files.authorized_keys).unwrap_or_default()
            }

            fn machine(&self, name: &str) -> Option<MachineConfig> {
                Flock::load(&self.d.paths.flock_file())
                    .unwrap()
                    .get(name)
                    .cloned()
            }

            fn used(&self, id: &str) -> bool {
                self.d
                    .store
                    .join_token(id)
                    .unwrap()
                    .unwrap()
                    .used_at
                    .is_some()
            }
        }

        fn code(err: anyhow::Error) -> String {
            match join_error(err) {
                IpcResponse::Error { code, .. } => code,
                other => panic!("{other:?}"),
            }
        }

        #[tokio::test]
        async fn join_adds_pull_machine() {
            let h = head().await;
            let mut events = h.d.events.subscribe();
            let id = h.token(None, &["gpu"]);
            h.join(&id, "pi-3").await.unwrap();
            let m = h.machine("pi-3").expect("pi-3 in flock.toml");
            assert!(m.pull);
            assert_eq!(m.tags, ["gpu"]);
            let state = h.d.store.machine_state("pi-3").unwrap().unwrap();
            assert_eq!(state.bootstrap, crate::store::BootstrapState::Pending);
            assert_eq!(state.generation, 1);
            assert_eq!(state.host_key.as_deref(), Some("ssh-ed25519 AAAAHOST"));
            let keys = h.keys();
            assert!(
                keys.contains(
                    "command=\"/usr/local/bin/pastor bridge --shepherd --machine pi-3 --gen 1\""
                ) && keys.contains("ssh-ed25519 AAAAMpi3 pastor:machine:pi-3"),
                "{keys}"
            );
            assert!(
                keys.contains("command=\"/usr/local/bin/pastor bridge --agent --machine pi-3\"")
                    && keys.contains("ssh-ed25519 AAAAApi3 pastor:agent:pi-3"),
                "{keys}"
            );
            let joined = loop {
                let ev = events.recv().await.unwrap();
                if ev.kind == "machine.joined" {
                    break ev;
                }
            };
            assert_eq!(joined.machine.as_deref(), Some("pi-3"));
            assert_eq!(joined.detail.unwrap()["generation"], 1);
        }

        #[tokio::test]
        async fn token_line_removed_after_use() {
            let h = head().await;
            let id = h.token(None, &[]);
            let marker = format!("pastor:join:{id}");
            assert!(h.keys().contains(&marker));
            h.join(&id, "pi-3").await.unwrap();
            assert!(!h.keys().contains(&marker), "{}", h.keys());
            assert!(h.used(&id));
        }

        #[tokio::test]
        async fn second_join_refused() {
            let h = head().await;
            let id = h.token(None, &[]);
            h.join(&id, "pi-3").await.unwrap();
            let err = h.join(&id, "pi-4").await.unwrap_err();
            assert_eq!(code(err), crate::join::USED);
            assert!(h.machine("pi-4").is_none());
            assert!(!h.keys().contains("pi-4"), "{}", h.keys());
        }

        #[tokio::test]
        async fn name_taken_refused() {
            let h = head().await;
            let id = h.token(None, &[]);
            let err = h.join(&id, "a").await.unwrap_err();
            assert_eq!(code(err), crate::join::NAME_TAKEN);
            // `a` is as it was, and the spent token's line is gone.
            assert!(!h.machine("a").unwrap().pull);
            assert!(h.d.store.machine_state("a").unwrap().is_none());
            assert!(!h.keys().contains("pastor:"), "{}", h.keys());
        }

        #[tokio::test]
        async fn replace_allows_same_name_only() {
            let h = head().await;
            let id = h.token(Some("a"), &[]);
            let err = h.join(&id, "b").await.unwrap_err();
            assert_eq!(code(err), crate::join::NAME_TAKEN);
            assert!(h.machine("b").is_none());
            let id = h.token(Some("a"), &[]);
            h.join(&id, "a").await.unwrap();
            assert!(h.machine("a").unwrap().pull);
        }

        #[tokio::test]
        async fn name_rejects_shell_metacharacters() {
            let h = head().await;
            let id = h.token(None, &[]);
            let before = std::fs::read_to_string(h.d.paths.flock_file()).unwrap();
            for name in [
                "x; rm -rf /",
                "$(id)",
                "`id`",
                "-x",
                "a b",
                "a\nb",
                "",
                "../x",
            ] {
                let err = h.join(&id, name).await.unwrap_err();
                assert_eq!(code(err), "invalid_machine", "{name:?}");
            }
            // Refused before anything changed: the token is still good.
            assert!(!h.used(&id));
            assert!(h.keys().contains(&format!("pastor:join:{id}")));
            assert_eq!(
                std::fs::read_to_string(h.d.paths.flock_file()).unwrap(),
                before
            );
            h.join(&id, "pi-3").await.unwrap();
        }

        #[tokio::test]
        async fn replace_bumps_generation() {
            let h = head().await;
            let id = h.token(None, &[]);
            h.join(&id, "pi-3").await.unwrap();
            let id = h.token(Some("pi-3"), &[]);
            h.join(&id, "pi-3").await.unwrap();
            let state = h.d.store.machine_state("pi-3").unwrap().unwrap();
            assert_eq!(state.generation, 2);
            let keys = h.keys();
            assert!(keys.contains("--machine pi-3 --gen 2\""), "{keys}");
            assert!(!keys.contains("--gen 1"), "{keys}");
            assert_eq!(keys.matches("pastor:machine:pi-3").count(), 1, "{keys}");
            // A shepherd bridge of the machine it replaced is cut off.
            let old =
                h.d.handle(IpcRequest::ShepherdCheck {
                    machine: "pi-3".into(),
                    generation: 1,
                })
                .await;
            assert!(
                matches!(&old, IpcResponse::Error { code, .. } if code == crate::bridge::MACHINE_REVOKED),
                "{old:?}"
            );
        }

        #[tokio::test]
        async fn unknown_token_id_refused() {
            let h = head().await;
            let before = std::fs::read_to_string(h.d.paths.flock_file()).unwrap();
            for id in ["0123456789abcdef", "pjt_abc", "x;rm", ""] {
                let err = h.join(id, "pi-3").await.unwrap_err();
                assert_eq!(code(err), crate::join::INVALID, "{id:?}");
            }
            assert_eq!(
                std::fs::read_to_string(h.d.paths.flock_file()).unwrap(),
                before
            );
            assert!(h.d.store.machine_state("pi-3").unwrap().is_none());
            assert!(h.keys().is_empty());
        }

        /// `join create` refuses a flock or a name to replace that the head
        /// does not have, and writes nothing.
        #[tokio::test]
        async fn create_refuses_unknown_flock_and_machine() {
            let h = head().await;
            for (flock, replace, want) in [
                (Some("nope"), None, "unknown_flock"),
                (None, Some("zz"), "unknown_machine"),
            ] {
                let ask = Ask {
                    flock: flock.map(str::to_string),
                    tags: Vec::new(),
                    replace: replace.map(str::to_string),
                    ttl: Duration::from_secs(60),
                };
                let resp = h.d.join_create_with(
                    &crate::join::tests::FakeKeygen,
                    &h.files,
                    ask,
                    chrono::Utc::now(),
                );
                assert!(
                    matches!(&resp, IpcResponse::Error { code, .. } if code == want),
                    "{resp:?}"
                );
            }
            assert!(h.keys().is_empty());
            assert!(h.d.store.join_tokens().unwrap().is_empty());
        }
    }

    /// `machine cordon`: a machine takes no new task on any placement path
    /// until it is uncordoned, the tasks it runs go on, and the cordon is
    /// the store's, so a restarted head still holds it.
    mod cordon {
        use super::*;

        async fn cordon(d: &Daemon, name: &str, reason: Option<&str>) -> IpcResponse {
            d.handle(IpcRequest::MachineCordon {
                name: name.into(),
                reason: reason.map(str::to_string),
            })
            .await
        }

        async fn uncordon(d: &Daemon, name: &str) -> String {
            let IpcResponse::Text(t) = d
                .handle(IpcRequest::MachineUncordon { name: name.into() })
                .await
            else {
                panic!()
            };
            t
        }

        fn note(d: &Daemon, t: &Task) -> Option<String> {
            d.store().get_task(t.id).unwrap().unwrap().error
        }

        /// A cordoned machine is skipped by a loose task and a pinned one,
        /// both wait with the reason, and the task already running is left
        /// alone; uncordoning places the waiting ones at once.
        #[tokio::test]
        async fn a_cordoned_machine_takes_no_new_task_and_keeps_its_running_one() {
            let (d, _tmp) = daemon(&[("a", 3, FakeHerdr::new()), ("b", 3, FakeHerdr::new())]).await;
            let running = queued(&d, "running", Some("a")).await;
            assert_eq!(running.state, TaskState::Running);

            let IpcResponse::Text(said) = cordon(&d, "a", Some("kernel update")).await else {
                panic!()
            };
            assert!(said.contains("a is cordoned: kernel update"), "{said}");
            assert_eq!(state_of(&d, &running), TaskState::Running);

            let pinned = queued(&d, "pinned", Some("a")).await;
            assert_eq!(pinned.state, TaskState::Queued);
            assert_eq!(
                note(&d, &pinned).as_deref(),
                Some("waiting for a machine: a is cordoned")
            );
            // A loose task goes to the machine that is not cordoned.
            let loose = queued(&d, "loose", None).await;
            assert_eq!(loose.machine.as_deref(), Some("b"));
            assert_eq!(state_of(&d, &running), TaskState::Running, "left alone");

            // The queue says why, too.
            let entries = crate::queue::entries(
                vec![d.store().get_task(pinned.id).unwrap().unwrap()],
                &d.fleet().queue_views(),
                "default",
                &|_, _| true,
            );
            assert_eq!(entries[0].why, "waiting for a machine: a is cordoned");

            // The statuses carry the cordon and its reason.
            let status = d
                .fleet()
                .statuses()
                .into_iter()
                .find(|s| s.name == "a")
                .unwrap();
            assert_eq!(
                status.cordon.and_then(|c| c.reason).as_deref(),
                Some("kernel update")
            );

            let said = uncordon(&d, "a").await;
            assert!(said.contains("takes tasks again"), "{said}");
            assert_eq!(state_of(&d, &pinned), TaskState::Running, "placed again");
            assert_eq!(note(&d, &pinned), None);
            assert!(uncordon(&d, "a").await.contains("was not cordoned"));
        }

        /// Only the machine named is cordoned, and only one in the flock.
        #[tokio::test]
        async fn cordoning_an_unknown_machine_is_refused() {
            let (d, _tmp) = daemon(&[("a", 1, FakeHerdr::new())]).await;
            for resp in [
                cordon(&d, "nowhere", None).await,
                d.handle(IpcRequest::MachineUncordon {
                    name: "nowhere".into(),
                })
                .await,
            ] {
                let IpcResponse::Error { code, .. } = resp else {
                    panic!("{resp:?}")
                };
                assert_eq!(code, "unknown_machine");
            }
        }

        /// `--now` onto a cordoned machine is refused, and the error says it
        /// is not the room floor, which it runs past.
        #[tokio::test]
        async fn now_is_refused_on_a_cordoned_machine() {
            let (d, _tmp) = daemon(&[("a", 1, FakeHerdr::new())]).await;
            cordon(&d, "a", None).await;
            let mut req = run_on("urgent", Some("a"));
            if let IpcRequest::Run { now, .. } = &mut req {
                *now = true;
            }
            let IpcResponse::Error { code, message } = d.handle(req).await else {
                panic!()
            };
            assert_eq!(code, MACHINE_CORDONED);
            assert!(
                message.contains("cordoned") && message.contains("room floor"),
                "{message}"
            );
            assert!(
                d.store()
                    .list_tasks(&TaskFilter::default())
                    .unwrap()
                    .is_empty(),
                "no row was made"
            );
            // A --now task already queued when the cordon came is not
            // placed either.
            uncordon(&d, "a").await;
            let queued_now = insert_now(&d, "a");
            cordon(&d, "a", None).await;
            d.fleet().dispatch_queued().await;
            assert_eq!(state_of(&d, &queued_now), TaskState::Queued);
        }

        fn insert_now(d: &Daemon, machine: &str) -> Task {
            d.store()
                .insert_task(NewTask {
                    job: "run".into(),
                    item: serde_json::Value::Null,
                    prompt: "p".into(),
                    spec: DispatchSpec {
                        now: true,
                        machine: Some(machine.into()),
                        ..spec()
                    },
                    flock: "default".into(),
                    description: None,
                })
                .unwrap()
        }

        /// A pull machine's claim is a placement path too: a cordoned one
        /// claims nothing, pinned or loose, and claims again once lifted.
        #[tokio::test]
        async fn a_cordoned_pull_machine_claims_nothing() {
            let (d, _tmp) = pull_daemon(3).await;
            let p1 = queued(&d, "p1", Some("laptop")).await;
            let p2 = queued(&d, "p2", Some("laptop")).await;
            cordon(&d, "laptop", Some("moving")).await;
            assert!(claim(&d, 5, true).await.is_empty());
            assert_eq!(state_of(&d, &p1), TaskState::Queued);
            assert_eq!(state_of(&d, &p2), TaskState::Queued);
            uncordon(&d, "laptop").await;
            assert_eq!(claim(&d, 5, true).await, ["p1", "p2"]);
        }

        /// A paused task is not resumed on a cordoned machine: that would put
        /// work back on it.
        #[tokio::test]
        async fn a_paused_task_waits_for_the_cordon_to_lift() {
            let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
            let t = queued(&d, "work", Some("a")).await;
            let fleet = d.fleet();
            let task = d.store().get_task(t.id).unwrap().unwrap();
            // With room and no cordon it would resume.
            assert!(fleet.resume_paused(&task, &[], &|_, _| true).is_some());
            cordon(&d, "a", None).await;
            assert!(fleet.resume_paused(&task, &[], &|_, _| true).is_none());
        }

        /// The cordon is kept in the store: a head started again on the same
        /// directories still holds the machine.
        #[tokio::test]
        async fn a_cordon_survives_a_restart() {
            let tmp = tempfile::tempdir().unwrap();
            let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
            let flock = Flock {
                flocks: vec![],
                machines: vec![machine("a", 2)],
            };
            flock.save(&paths.flock_file()).unwrap();
            std::fs::write(
                paths.config_file(),
                toml::to_string(&test_config()).unwrap(),
            )
            .unwrap();
            let start = || async {
                let fake = FakeHerdr::new();
                let on_disk = ConfigFingerprint::sample(&paths);
                let d = Daemon::start(
                    paths.clone(),
                    test_config(),
                    flock.clone(),
                    on_disk,
                    Some(factory(&[("a", fake)])),
                )
                .await
                .unwrap();
                let deadline = Instant::now() + Duration::from_secs(5);
                while d.fleet().views().iter().any(|v| !v.healthy) {
                    assert!(Instant::now() < deadline, "never connected");
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                d
            };
            let d = start().await;
            cordon(&d, "a", Some("rack move")).await;
            drop(d);
            let d = start().await;
            assert!(d.fleet().views()[0].floor.cordoned);
            let t = queued(&d, "after", Some("a")).await;
            assert_eq!(t.state, TaskState::Queued);
            assert_eq!(
                note(&d, &t).as_deref(),
                Some("waiting for a machine: a is cordoned")
            );
        }

        /// Removing a machine takes its cordon with it.
        #[test]
        fn removing_a_machine_drops_its_cordon() {
            let store = Store::open_in_memory().unwrap();
            let tmp = tempfile::tempdir().unwrap();
            let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
            store
                .put_cordon(&crate::store::Cordon {
                    machine: "a".into(),
                    reason: None,
                    since: chrono::Utc::now(),
                })
                .unwrap();
            let said =
                crate::leave::machine_cleanup(&paths, Some(&store), "a", None, true).unwrap();
            assert_eq!(said, ["would remove a's cordon"]);
            assert!(store.cordon("a").unwrap().is_some());
            crate::leave::machine_cleanup(&paths, Some(&store), "a", None, false).unwrap();
            assert!(store.cordon("a").unwrap().is_none());
        }
    }
}
