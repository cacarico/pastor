use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::broadcast;

use crate::config::flock::{EditError, Flock, FlockDoc, MachineConfig, TaskFlockError};
use crate::config::{AgentChoice, AgentPick, Defaults, PastorConfig, Paths};
use crate::dispatch::{MachineView, pick_machine};
use crate::herdr::{Connector, Endpoint};
use crate::ipc::{DaemonProbe, IpcRequest, IpcResponse};
use crate::machine::{
    ActorStopped, MachineHandle, MachineSettings, OrphanClosed, PastorEvent, SendInput,
    SendRefused, ShutdownOutcome, spawn_machine,
};
use crate::scheduler::{ConfigFingerprint, Scheduler, SchedulerHandle};
use crate::store::{NewTask, RetryError, Store, TaskFilter};
use crate::task::{PANE_OWNING_STATES, Task, TaskState};

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

/// The flock `name` is in according to `flock`; a machine the file does not
/// have (a fixed fleet's, or one held until its actor ends) is in the default
/// flock, the only one a fixed fleet has.
fn flock_of(flock: &Flock, name: &str) -> String {
    flock
        .machine_flock(name)
        .unwrap_or(flock.default_flock())
        .to_string()
}

/// The part of a machine's entry its actor is built from. The flock is not:
/// it only decides which tasks the machine is offered, which dispatch reads
/// from the flock last applied, so moving a machine keeps its connection and
/// the tasks already on it.
fn actor_config(m: &MachineConfig) -> MachineConfig {
    MachineConfig {
        flock: None,
        ..m.clone()
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
    /// `[defaults]` as last applied (`set_defaults`): what a queued task's
    /// agent falls back to after its flock's (`resolve_agent`).
    defaults: RwLock<Defaults>,
    store: Arc<Store>,
    /// `None` for a fixed fleet (`Fleet::new`): tests and the daemon-less CLI.
    spawner: Option<Spawner>,
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
            })
            .collect();
        Fleet {
            members: RwLock::new(members),
            wanted: RwLock::default(),
            defaults: RwLock::default(),
            store,
            spawner: None,
            dispatch_lock: tokio::sync::Mutex::new(()),
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
            store,
            spawner: Some(Spawner { connect, events }),
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

    /// Every machine's status with the flock it is in (see `flock_of`), in
    /// flock order: what `machine list` and `machine.*` events report.
    pub fn statuses(&self) -> Vec<crate::machine::MachineStatus> {
        let wanted = self.flock();
        self.machines()
            .iter()
            .map(|h| crate::machine::MachineStatus {
                flock: Some(flock_of(&wanted, &h.name)),
                ..h.snapshot()
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

    /// Take `[defaults]` from `pastor.toml` as now loaded; the scheduler
    /// calls it at start and on every reload that reads the file.
    pub fn set_defaults(&self, defaults: Defaults) {
        *self.defaults.write().unwrap() = defaults;
    }

    /// The agent a task queued in `flock` gets, given what its run or job
    /// asked for (`Defaults::resolve_agent`), from the flock and defaults as
    /// they stand now.
    pub fn resolve_agent(&self, ask: &AgentChoice, flock: &str) -> AgentPick {
        let wanted = self.wanted.read().unwrap();
        self.defaults
            .read()
            .unwrap()
            .resolve_agent(ask, wanted.entry(flock))
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
            let want = (actor_config(m), settings.clone());
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
        let handle = spawn_machine(
            m.name.clone(),
            m.max_agents,
            m.tags.clone(),
            (spawner.connect)(m),
            self.store.clone(),
            settings.clone(),
            spawner.events.clone(),
        );
        Member {
            handle,
            spawned_from: Some((actor_config(m), settings.clone())),
            shutting_down: false,
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
                    tags: m.handle.tags.clone(),
                    live: s.live,
                    // An aborted actor answers nothing, and a dispatch to
                    // it would wait for as long as it stays wedged.
                    healthy: !m.shutting_down && s.channel.accepts_dispatch(),
                    flock: flock_of(&wanted, &m.handle.name),
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
    /// it, keeps the agent `spec` already carries.
    pub async fn queue_run(
        &self,
        prompt: String,
        mut spec: crate::task::DispatchSpec,
        flock: Option<&str>,
        ask: Option<&AgentChoice>,
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
            self.resolve_agent(ask, &flock).apply_to(&mut spec);
        }
        self.store
            .insert_task(NewTask {
                job: "run".into(),
                item: serde_json::Value::Null,
                prompt,
                spec,
                flock,
            })
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
        let _pass = self.dispatch_lock.lock().await;
        let flock = self.job_task_flock(job)?;
        let pick = self.resolve_agent(&job.agent, &flock);
        self.store.insert_job_task(&job.name, &flock, item, |id| {
            let (prompt, mut spec) = render(id)?;
            pick.apply_to(&mut spec);
            Ok((prompt, spec))
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

    /// `queue_task` for a retry of task `id` (`Store::insert_retry`). The
    /// copy keeps the original's pin and flock, so both get the same checks
    /// under the same lock: a failed task outlives `flock remove`, which only
    /// counts queued ones, and a copy in a removed flock would wait forever.
    /// A row that is missing or not retryable is left for `insert_retry` to
    /// name.
    pub async fn queue_retry(&self, id: i64) -> Result<Task, QueueError<RetryError>> {
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
        }
        self.store.insert_retry(id).map_err(QueueError::Store)
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
            let target = task.flock.as_deref().unwrap_or(flock.default_flock());
            let Some(name) = pick_machine(&self.views(), target, &task.spec) else {
                continue;
            };
            let Some(handle) = self.get(&name) else {
                continue;
            };
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
}

pub struct Daemon {
    paths: Paths,
    store: Arc<Store>,
    fleet: Arc<Fleet>,
    scheduler: SchedulerHandle,
    events: broadcast::Sender<PastorEvent>,
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
        // Plugin event hooks read the broadcast on their own, subscribed here
        // for the same reason as the log: before any actor can emit.
        let hooks_rx = events.subscribe();
        let connect = connect.unwrap_or_else(|| endpoint_factory(paths.clone()));
        let fleet = Arc::new(Fleet::managed(store.clone(), events.clone(), connect));
        fleet.apply_flock(&flock, &machine_settings(&config)).await;
        // Fresh set: nothing has been warned about yet, so every task on a
        // machine `flock` does not have is reported.
        warn_removed(&store, &flock, &mut HashSet::new());
        // Subscribed in `start`, before any actor runs, so the log sees the
        // first events too. The log holds the fleet weakly (see `spawn_log`),
        // so dropping the daemon still winds the tasks down.
        let lookup: Arc<dyn crate::events::MachineLookup> = fleet.clone();
        crate::events::spawn_log(
            paths.events_file(),
            crate::events::DEFAULT_MAX_BYTES,
            store.clone(),
            Some(Arc::downgrade(&lookup)),
            log_rx,
        );
        crate::hooks::spawn(
            paths.clone(),
            store.clone(),
            Some(Arc::downgrade(&lookup)),
            hooks_rx,
        );
        let scheduler = Scheduler::new(
            paths.clone(),
            &config,
            store.clone(),
            fleet.clone(),
            events.clone(),
        )
        .with_plugins()
        .with_config_baseline(on_disk)
        .spawn();
        Ok(Daemon {
            paths,
            store,
            fleet,
            scheduler,
            events,
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
    async fn bind_socket(socket: &std::path::Path) -> anyhow::Result<tokio::net::UnixListener> {
        if socket.exists() {
            // Staleness is a property of the connect, not of the reply: a live
            // daemon mid-request (e.g. `dispatch_queued` against a slow or wedged
            // herdr) can go a while without answering a ping, and a busy daemon
            // looks exactly like a wedged one from the outside. Only a refused (or
            // absent) connect means nothing is actually listening; anything else
            // must be left alone rather than unlinked and stolen.
            match crate::ipc::probe_daemon(socket).await {
                DaemonProbe::Running => anyhow::bail!(
                    "another pastor daemon is already running on {}",
                    socket.display()
                ),
                DaemonProbe::Unresponsive => anyhow::bail!(
                    "a daemon is listening on {} but did not respond within 2s; \
                     remove the socket file by hand only if that daemon is dead",
                    socket.display()
                ),
                DaemonProbe::NotRunning => std::fs::remove_file(socket)?,
            }
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
        let daemon = Arc::new(self);
        let mut extra_signals = ExtraSignals::new()?;
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (stream, _) = accepted?;
                    let d = daemon.clone();
                    tokio::spawn(async move {
                        let (r, mut w) = stream.into_split();
                        let mut line = String::new();
                        if BufReader::new(r).read_line(&mut line).await.is_err() { return; }
                        let resp = match serde_json::from_str::<IpcRequest>(line.trim()) {
                            Ok(req) => d.handle(req).await,
                            Err(err) => IpcResponse::error("invalid_request", err),
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

    pub async fn handle(&self, req: IpcRequest) -> IpcResponse {
        match req {
            IpcRequest::Ping => IpcResponse::Pong {
                version: env!("CARGO_PKG_VERSION").into(),
                protocol: crate::ipc::IPC_PROTOCOL,
            },
            IpcRequest::Run {
                prompt,
                spec,
                flock,
                agent,
            } => {
                // clap refuses this too; checked here as well so no other
                // client can queue a task dispatch can only fail.
                if spec.worktree && spec.repo.is_none() {
                    return IpcResponse::error(
                        "worktree_needs_repo",
                        "a worktree task needs a repo to branch from",
                    );
                }
                // `[defaults]` as pastor.toml reads now, not as of the last
                // tick: `task run` has always taken an edit at once. A file
                // that does not load leaves the last good one in use.
                if agent.is_some()
                    && let Ok(config) = PastorConfig::load_existing(&self.paths.config_file())
                {
                    self.fleet.set_defaults(config.defaults);
                }
                let task = match self
                    .fleet
                    .queue_run(prompt, spec, flock.as_deref(), agent.as_ref())
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
            IpcRequest::TaskShow { id } => match self.store.get_task(id) {
                Ok(Some(t)) => IpcResponse::Task(t),
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
            IpcRequest::Tick { job, dry_run } => match self.scheduler.tick(job, dry_run).await {
                Ok(runs) => IpcResponse::Runs(runs),
                Err(err) => IpcResponse::error("scheduler_error", err),
            },
            IpcRequest::Reload => match self.scheduler.reload().await {
                Ok(jobs) => IpcResponse::Jobs(jobs),
                Err(err) => IpcResponse::error("scheduler_error", err),
            },
            IpcRequest::JobList => match self.scheduler.job_list().await {
                Ok(jobs) => IpcResponse::Jobs(jobs),
                Err(err) => IpcResponse::error("scheduler_error", err),
            },
            IpcRequest::JobRun { name } => match self.scheduler.fire(&name).await {
                Ok(Ok(msg)) => IpcResponse::Text(msg),
                Ok(Err(reason)) => IpcResponse::error("job_not_found", reason),
                Err(err) => IpcResponse::error("scheduler_error", err),
            },
            IpcRequest::TaskRetry { id } => self.retry(id).await,
            IpcRequest::TaskClose {
                id,
                remove_worktree,
            } => self.close(id, remove_worktree).await,
            IpcRequest::TaskSend { id, input } => self.send(id, input).await,
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
        }
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

    /// `TaskRetry`: a new queued row copying `id` (see `Store::insert_retry`),
    /// dispatched at once like a `Run`. Answers the new row as it stands after
    /// the dispatch pass.
    async fn retry(&self, id: i64) -> IpcResponse {
        // The store checks the state and copies in one statement; its error
        // says which check failed, so a row pruned by a concurrent request is
        // `task_not_found` and a storage failure is `store_error`.
        let task = match self.fleet.queue_retry(id).await {
            Ok(t) => t,
            Err(QueueError::UnknownMachine(m)) => {
                return IpcResponse::error(
                    "unknown_machine",
                    format!("t-{id} is pinned to machine {m}, which is not in the flock"),
                );
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
            if let Some(m) = t.machine.clone() {
                break m;
            }
            let was = t.state;
            let closed = if was == TaskState::Queued {
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
                });
            }
            return IpcResponse::Task(closed);
        };
        // A removed machine held only until its old actor ends counts as
        // gone: that actor answers nothing and no replacement will come.
        let handle = self
            .fleet
            .get(&machine)
            .filter(|_| self.fleet.in_flock(&machine));
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
            name: name.into(),
            local: false,
            ssh: None,
            command: Some(vec!["fake".into()]),
            session: "default".into(),
            max_agents: max,
            tags: vec![],
            flock: None,
        }
    }

    fn spec() -> DispatchSpec {
        DispatchSpec {
            agent: "claude".into(),
            agent_args: vec![],
            repo: None,
            worktree: false,
            branch: None,
            machine: None,
            tags: vec![],
            timeout_secs: 60,
            checkout: None,
            reopen: None,
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
        while d.fleet().views().iter().any(|v| !v.healthy) {
            assert!(Instant::now() < deadline, "machines never connected");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        (d, tmp)
    }

    #[tokio::test]
    async fn run_dispatches_immediately() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let mut events = d.subscribe();
        let resp = d
            .handle(IpcRequest::Run {
                prompt: "hi".into(),
                spec: spec(),
                flock: None,
                agent: None,
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
                prompt: "1".into(),
                spec: spec(),
                flock: None,
                agent: None,
            })
            .await
        else {
            panic!()
        };
        assert_eq!(first.state, TaskState::Running);
        let IpcResponse::Task(second) = d
            .handle(IpcRequest::Run {
                prompt: "2".into(),
                spec: spec(),
                flock: None,
                agent: None,
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

    #[tokio::test]
    async fn pinned_unknown_machine_and_bad_ids_are_errors() {
        let (d, _tmp) = daemon(&[("a", 2, FakeHerdr::new())]).await;
        let resp = d
            .handle(IpcRequest::Run {
                prompt: "x".into(),
                spec: DispatchSpec {
                    machine: Some("zzz".into()),
                    ..spec()
                },
                flock: None,
                agent: None,
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
                prompt: "x".into(),
                spec: DispatchSpec {
                    machine: Some("b".into()),
                    ..spec()
                },
                flock: None,
                agent: None,
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
            prompt: "x".into(),
            spec: DispatchSpec {
                machine: machine.map(Into::into),
                ..spec()
            },
            flock: flock.map(Into::into),
            agent: None,
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
            prompt: "x".into(),
            spec: spec(),
            flock: Some(flock.into()),
            agent,
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
            error_code(d.handle(IpcRequest::TaskRetry { id: t.id }).await),
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
                        .queue_run("x".into(), spec(), Some("spare"), None)
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
        assert_eq!(h.flock, "work");
        let resp = d.handle(run_in(Some("home"), None)).await;
        let IpcResponse::Task(t) = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(t.state, TaskState::Queued, "home has no machine left");
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
                prompt: "x".into(),
                spec: DispatchSpec {
                    machine: Some("b".into()),
                    ..spec()
                },
                flock: None,
                agent: None,
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
            error_code(d.handle(IpcRequest::TaskRetry { id: failed.id }).await),
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
            error_code(d.handle(IpcRequest::TaskRetry { id: running.id }).await),
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
                prompt: "hi".into(),
                spec: spec(),
                flock: None,
                agent: None,
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
            },
        );
        let settings = machine_settings(&config);
        assert_eq!(
            settings.agents.trust_keys("codex"),
            Some(vec!["Enter".into()])
        );
        assert_ne!(settings, machine_settings(&test_config()));
    }

    #[tokio::test]
    async fn task_send_reaches_a_live_task_and_refuses_the_rest() {
        let fake = FakeHerdr::new();
        let (d, _tmp) = daemon(&[("a", 2, fake.clone())]).await;
        let IpcResponse::Task(t) = d
            .handle(IpcRequest::Run {
                prompt: "hi".into(),
                spec: spec(),
                flock: None,
                agent: None,
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
        let resp = d.handle(IpcRequest::TaskRetry { id: failed.id }).await;
        let IpcResponse::Task(t) = resp else {
            panic!("{resp:?}")
        };
        assert_ne!(t.id, failed.id);
        assert_eq!(t.retry_of, Some(failed.id));
        assert_eq!(t.state, TaskState::Running, "dispatched right away");
        let ev = events.try_recv().unwrap();
        assert_eq!((ev.kind.as_str(), ev.task_id), ("task.queued", Some(t.id)));

        assert_eq!(
            error_code(d.handle(IpcRequest::TaskRetry { id: t.id }).await),
            "not_retryable"
        );
        assert_eq!(
            error_code(d.handle(IpcRequest::TaskRetry { id: 99 }).await),
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
            error_code(d.handle(IpcRequest::TaskRetry { id: failed.id }).await),
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
                prompt: "x".into(),
                spec: spec(),
                flock: None,
                agent: None,
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
                prompt: "x".into(),
                spec: spec(),
                flock: None,
                agent: None,
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
        let ws = fake.workspace_create(None, "t-42").await.unwrap();
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
                prompt: "x".into(),
                spec: DispatchSpec {
                    worktree: true,
                    ..spec()
                },
                flock: None,
                agent: None,
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
}
