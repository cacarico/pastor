use std::time::Duration;

use chrono::Utc;
use tokio::time::Instant;

use crate::config::Agents;
use crate::herdr::{
    AgentInfo, AgentStatus, CallError, Connector, ConnectorExt, Created, Fetched, HerdrError,
};
pub use crate::machine::FlockSeat;
use crate::task::{Checkout, DispatchSpec, Place, Priority, Reopen, Task, TaskState, render_label};

/// How often dispatch asks `agent.list` whether the agent it started is up yet.
const READY_POLL: Duration = Duration::from_millis(500);

/// How many times dispatch tries `agent.start` on a pane herdr calls busy, and
/// how long it waits between tries. A new pane's shell can still be starting
/// a second after `workspace.create` returns; herdr then answers
/// `agent_pane_busy` and the same start works a moment later (t-42, t-50 on
/// 2026-09-25). herdr 0.9.1 has no request that says whether a pane's shell is
/// ready, so this is a timed retry. Five tries 500ms apart add at most 2s,
/// well inside `request_timeout`, which still bounds the whole dispatch.
const PANE_BUSY_ATTEMPTS: u32 = 5;
const PANE_BUSY_WAIT: Duration = Duration::from_millis(500);

#[derive(Debug, Clone)]
pub struct MachineView {
    pub name: String,
    pub max_agents: u32,
    /// Slots only job tasks take, on top of `max_agents`.
    pub job_slots: u32,
    /// How far past `max_agents` a critical task may go.
    pub burst: u32,
    pub tags: Vec<String>,
    /// The loads the machine takes, its own and its flocks'
    /// (`Flock::loads_of`), as the flock file stands now.
    pub loads: Vec<String>,
    /// Every pane-owning task and orphan on the machine.
    pub live: usize,
    /// How many of `live` come from a job (`Task::from_job`).
    pub live_jobs: usize,
    pub healthy: bool,
    /// A pull machine (`pull = true`): a pass never pushes to it, its own
    /// serve claims. Read by `pastor queue` alone, where `healthy` is
    /// whether it is connected: a task pinned to it waits on its slots and
    /// seat as its claims do, while a loose task never reads it as where
    /// it goes next, since whether its claims take loose tasks is its
    /// serve's own setting (`takes_flock_work`), which the head does not
    /// know.
    pub pull: bool,
    /// The head can place a task's workdir set there: a push machine it
    /// reaches with a shell (`workdir_task::takes_sets`). A task with a set
    /// goes nowhere else.
    pub workdir_sets: bool,
    /// The flocks the machine is in now, each with its number and how many
    /// of its live tasks run here.
    pub flocks: Vec<FlockSeat>,
    /// The flocks under their share here with a queued task waiting that
    /// this machine could take. A flock past its share here takes a slot
    /// only while this is empty (`MachineView::flock_may_take`).
    pub waiting_under_share: Vec<String>,
    /// Where the machine stands against its room floor.
    pub floor: FloorView,
}

/// A machine against its room floor (`config::Floor`), as dispatch reads
/// it. The default is no floor: never short, nothing unread.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FloorView {
    /// The first floor the machine is under, worded for the queue:
    /// `sauron has 1.2 GB free under /tmp`. Disk under `$HOME`, then
    /// `/tmp`, then memory (`floor_against`).
    pub short: Option<String>,
    /// Its floor is on, but a side of it cannot be read: no numbers yet, a
    /// probe that failed before any answered, no memory on macOS.
    pub unread: bool,
    /// The machine's flocks whose `unknown_room` there is `no_room`
    /// (`config::resolve_unknown_room`): an unread side holds their tasks.
    pub hold_unread: Vec<String>,
    /// The last `machine check` the head ran of it found that its agents
    /// cannot reach the head (`MachineStatus::reach`): a task placed there
    /// could not even say it is done.
    pub unreachable: bool,
    /// An operator cordoned it (`pastor machine cordon`): it takes no new
    /// task, whatever the claim, and `--now` is no way past that.
    pub cordoned: bool,
}

/// Where a machine named `name` with `room` (in KiB, as the probe reads
/// it) stands against `floor` (in bytes): the first side it is under, and
/// whether a side that is on cannot be read. No floor is never short.
pub fn floor_against(
    name: &str,
    room: Option<&crate::machine::Room>,
    floor: Option<crate::config::Floor>,
) -> (Option<String>, bool) {
    let Some(floor) = floor else {
        return (None, false);
    };
    let bytes = |kib: Option<u64>| kib.map(|k| k.saturating_mul(1024));
    let room = room.cloned().unwrap_or_default();
    let sides = [
        (floor.disk, bytes(room.home_free), "free under $HOME"),
        (floor.disk, bytes(room.tmp_free), "free under /tmp"),
        (floor.memory, bytes(room.mem_free), "of memory free"),
    ];
    let mut short = None;
    let mut unread = false;
    for (min, free, what) in sides {
        let Some(min) = min else { continue };
        match free {
            None => unread = true,
            Some(free) if free < min && short.is_none() => {
                short = Some(format!("{name} has {} {what}", size_label(free)));
            }
            Some(_) => {}
        }
    }
    (short, unread)
}

/// `bytes` as the queue says a size: `1.2 GB`, `512 MB`, `64 KB`, in
/// powers of 1024 like the floor's own sizes.
fn size_label(bytes: u64) -> String {
    const K: f64 = 1024.0;
    let b = bytes as f64;
    if b >= K * K * K {
        format!("{:.1} GB", b / (K * K * K))
    } else if b >= K * K {
        format!("{:.0} MB", b / (K * K))
    } else {
        format!("{:.0} KB", b / K)
    }
}

/// What a task may take on a machine: a job slot if it comes from a job,
/// burst if it is critical.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Claim {
    pub from_job: bool,
    pub critical: bool,
}

impl Claim {
    pub fn of(task: &Task) -> Claim {
        Claim {
            from_job: task.from_job(),
            critical: task.priority == Priority::Critical,
        }
    }
}

impl MachineView {
    /// `flock`'s seat here, `None` when the machine is not in it.
    pub fn seat(&self, flock: &str) -> Option<&FlockSeat> {
        self.flocks.iter().find(|f| f.name == flock)
    }

    pub fn in_flock(&self, flock: &str) -> bool {
        self.seat(flock).is_some()
    }

    /// Does the machine have every tag `spec` needs, take its load, and
    /// its workdir set if it has one? A task with no load goes to a machine
    /// whatever loads it takes.
    pub fn suits(&self, spec: &DispatchSpec) -> bool {
        self.has_tags(spec)
            && self.takes_load(spec)
            && (spec.workdir.is_none() || self.workdir_sets)
    }

    pub fn has_tags(&self, spec: &DispatchSpec) -> bool {
        spec.tags.iter().all(|t| self.tags.contains(t))
    }

    pub fn takes_load(&self, spec: &DispatchSpec) -> bool {
        spec.load.as_ref().is_none_or(|l| self.loads.contains(l))
    }

    /// May a task of `flock` take a slot here, as far as the flock's number
    /// goes? Under its share, yes; from its share up to its max, only while
    /// no flock under its share here has a task waiting; at its max, no.
    pub fn flock_may_take(&self, flock: &str) -> bool {
        self.seat(flock).is_some_and(|s| {
            s.has_room() && (s.under_share() || self.waiting_under_share.is_empty())
        })
    }

    /// Why its room floor keeps a task of `flock` off this machine, as the
    /// waiting note says it: it cannot reach the head, under its floor, or a side of it unread while
    /// `flock`'s `unknown_room` here is `no_room`. `None` when the floor
    /// lets it take one.
    pub fn floor_held(&self, flock: &str) -> Option<String> {
        if self.floor.cordoned {
            return Some(format!("{} is cordoned", self.name));
        }
        if self.floor.unreachable {
            return Some(format!("{} cannot reach the head", self.name));
        }
        if let Some(short) = &self.floor.short {
            return Some(short.clone());
        }
        (self.floor.unread && self.floor.hold_unread.iter().any(|f| f == flock))
            .then(|| format!("{}'s room cannot be read", self.name))
    }

    /// Count one more live task of `flock` here, as a dispatch would.
    pub fn take(&mut self, flock: &str, claim: Claim) {
        self.live += 1;
        if claim.from_job {
            self.live_jobs += 1;
        }
        if let Some(s) = self.flocks.iter_mut().find(|f| f.name == flock) {
            s.live += 1;
        }
    }

    /// Is there a slot for a task that claims `claim`? Up to `job_slots`
    /// live job tasks sit in job slots; every other live task counts
    /// against `max_agents`. A job task takes a free job slot, then a
    /// shared one; a critical task that finds the shared slots full may go
    /// up to `max_agents + burst`.
    pub fn has_room(&self, claim: Claim) -> bool {
        let in_job_slots = self.live_jobs.min(self.job_slots as usize);
        let shared = (self.live - in_job_slots) as u64;
        (claim.from_job && self.live_jobs < self.job_slots as usize)
            || shared < self.max_agents as u64
            || (claim.critical && shared < self.max_agents as u64 + self.burst as u64)
    }
}

/// Only machines in `flock`, the task's, qualify. Of those the pinned machine
/// wins. Otherwise: healthy, has every required tag and takes its load
/// (`MachineView::suits`), has room for `claim`
/// (`MachineView::has_room`) with `flock` under its number there, and past
/// its share only while no flock under its share waits there
/// (`MachineView::flock_may_take`), not held by its room floor
/// (`MachineView::floor_held`), fewest live tasks. Ties keep flock order.
pub fn pick_machine(
    machines: &[MachineView],
    flock: &str,
    spec: &DispatchSpec,
    claim: Claim,
) -> Option<String> {
    pick_machine_where(machines, flock, spec, claim, &|_| true)
}

/// `pick_machine` among the machines `accepts` takes by name, as a machine
/// of another flock is left out: one whose agent cannot run the task's model.
pub fn pick_machine_where(
    machines: &[MachineView],
    flock: &str,
    spec: &DispatchSpec,
    claim: Claim,
    accepts: &dyn Fn(&str) -> bool,
) -> Option<String> {
    let fits = |m: &MachineView| {
        m.healthy
            && m.has_room(claim)
            && m.flock_may_take(flock)
            && m.floor_held(flock).is_none()
            && m.suits(spec)
            && accepts(&m.name)
    };
    if let Some(pinned) = &spec.machine {
        return machines
            .iter()
            .find(|m| &m.name == pinned)
            .filter(|m| fits(m))
            .map(|m| m.name.clone());
    }
    machines
        .iter()
        .filter(|m| fits(m))
        .min_by_key(|m| m.live)
        .map(|m| m.name.clone())
}

/// Where a `task run --now` task (`DispatchSpec::now`) goes: its pinned
/// machine, if it is healthy, is not cordoned, is still in the task's
/// flock, has the task's tags and `accepts` it, however many tasks it runs.
/// `max_agents`, job slots, burst, the flock's number and the room floor
/// there are not looked at: the person who pinned it chose to run past
/// them, but not to run in another flock, nor in a flock at 0 there, which
/// is draining the machine, nor on a machine an operator cordoned.
/// `queue_run_as` checked membership before the row was inserted, under a
/// lock it releases before placement, so it is checked again here in case a
/// flock edit moved the machine in between.
pub fn now_machine(
    machines: &[MachineView],
    flock: &str,
    spec: &DispatchSpec,
    accepts: &dyn Fn(&str) -> bool,
) -> Option<String> {
    let pinned = spec.machine.as_ref()?;
    machines
        .iter()
        .find(|m| &m.name == pinned)
        .filter(|m| {
            m.healthy
                && !m.floor.cordoned
                && m.seat(flock).is_some_and(|s| !s.drained())
                && m.suits(spec)
                && accepts(&m.name)
        })
        .map(|m| m.name.clone())
}

/// Fill each view's `waiting_under_share` for placing a task of `flock`: on
/// a machine where `flock` is past its share and under its max, the other
/// flocks under their share there that have a task in `later` the machine
/// could take (queued, not pinned elsewhere, with its tags, room for its
/// claim, not held by the machine's room floor, one `accepts`). `later` is
/// the queue behind the task being
/// placed: those tasks still get their turn in this pass, while one ahead
/// of it already had its turn and did not take the machine. Elsewhere the
/// list is left empty, since it only matters past a share.
pub fn mark_waiting_under_share(
    views: &mut [MachineView],
    flock: &str,
    later: &[Task],
    default_flock: &str,
    accepts: &dyn Fn(&Task, &str) -> bool,
) {
    for v in views.iter_mut() {
        v.waiting_under_share.clear();
        if !v
            .seat(flock)
            .is_some_and(|s| s.has_room() && !s.under_share())
        {
            continue;
        }
        let mut waiting: Vec<String> = Vec::new();
        for t in later {
            let theirs = t.flock.as_deref().unwrap_or(default_flock);
            if t.state != TaskState::Queued
                || theirs == flock
                || waiting.iter().any(|w| w == theirs)
                || !v.seat(theirs).is_some_and(FlockSeat::under_share)
                || t.spec.machine.as_ref().is_some_and(|m| *m != v.name)
                || !v.suits(&t.spec)
                || !v.has_room(Claim::of(t))
                || v.floor_held(theirs).is_some()
                || !accepts(t, &v.name)
            {
                continue;
            }
            waiting.push(theirs.to_string());
        }
        v.waiting_under_share = waiting;
    }
}

/// Why `flock`'s number holds its task off `v`, as the waiting note says
/// it: at its max, or past its share while another flock waits under its
/// share there. `None` when the number lets it take a slot.
pub fn flock_held(v: &MachineView, flock: &str) -> Option<String> {
    let seat = v.seat(flock)?;
    let max = seat.max?;
    if seat.drained() {
        return Some(format!(
            "flock {flock} has 0 slots on {}: it starts no new tasks there ({} still running)",
            v.name, seat.live
        ));
    }
    if !seat.has_room() {
        let of = seat.number_label().unwrap_or_else(|| max.to_string());
        return Some(format!(
            "flock {flock} is at {} of {of} on {}",
            seat.live, v.name
        ));
    }
    if seat.under_share() || v.waiting_under_share.is_empty() {
        return None;
    }
    let others = &v.waiting_under_share;
    let noun = if others.len() == 1 { "flock" } else { "flocks" };
    Some(format!(
        "flock {flock} is past its share on {}, at {} of {}, while {noun} {} wait{} under {} share",
        v.name,
        seat.live,
        seat.number_label().unwrap_or_default(),
        others.join(", "),
        if others.len() == 1 { "s" } else { "" },
        if others.len() == 1 { "its" } else { "their" },
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchOutcome {
    Running,
    Blocked,
}

/// Why a dispatch did not reach `Running`.
#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    /// A herdr call failed. Only this one can mean the machine is gone.
    #[error(transparent)]
    Call(#[from] CallError),
    /// pastor's own verdict about this dispatch: the spec is unusable, the agent
    /// never came up, the agent exited. The task failed; the machine is fine.
    #[error("{0}")]
    Task(String),
    /// The agent exited before it took its prompt, or sits at its login
    /// screen. `tail` is the end of its pane as it was then, empty when it
    /// could not be read: an agent can die at its start on a usage limit,
    /// and only its pane says so (`limit::limit_in`).
    #[error("{message}")]
    Exited { message: String, tail: String },
}

impl From<HerdrError> for DispatchError {
    fn from(err: HerdrError) -> DispatchError {
        DispatchError::Call(CallError::Herdr(err))
    }
}

impl DispatchError {
    pub fn code(&self) -> Option<&str> {
        match self {
            DispatchError::Call(err) => err.code(),
            DispatchError::Task(_) | DispatchError::Exited { .. } => None,
        }
    }

    /// Did this fail because the machine is unreachable, rather than because
    /// this task could not be started on it?
    pub fn is_transport(&self) -> bool {
        matches!(self, DispatchError::Call(err) if err.is_transport())
    }
}

/// Create the workspace, start the agent, wait for it to come up, send the
/// prompt. Each step is its own herdr request on its own connection (see
/// `ConnectorExt`), so a dispatch is several round trips, not one session:
/// nothing but the task row ties them together, which is why `agent_name`
/// identifies the agent afterwards.
///
/// `ready_timeout` bounds the wait between `agent.start` and a prompt herdr
/// accepts; it must stay below the caller's per-request timeout, or a slow
/// agent surfaces as a confusing "request timed out". `agents` turns the
/// task's tool lists into the agent's own flags (`Agents::launch_args`).
/// `head`, when given, is where the agent reaches the head from its machine
/// (`ipc::HEAD_ENV`).
pub async fn dispatch(
    conn: &dyn Connector,
    task: &mut Task,
    agents: &Agents,
    head: Option<&str>,
    ready_timeout: Duration,
) -> Result<DispatchOutcome, DispatchError> {
    dispatch_with(
        conn,
        task,
        agents,
        head,
        ready_timeout,
        Provision::default(),
    )
    .await
}

/// What a machine's provisioners make sure of before a launch there
/// (`provision`): the machine's own permission profile, which the agy
/// provisioner writes into agy's settings for an agy task under a profile.
/// The default asks for nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct Provision<'a> {
    pub own: Option<&'a crate::config::agy::Policy>,
    /// The keys of the trust pastor has saved for the machine
    /// (`Store::trusted_repos`): before the agent starts, a Claude or agy
    /// task whose folder is among them gets it written into the harness's
    /// store (`trust_write`).
    pub trusted: &'a [String],
    /// How long after that write the store is read again; none by default.
    pub trust_recheck: Duration,
    /// The harness versions the machine's flock pins, and the manifests of
    /// those harnesses: an agent of a kind pinned to a version gets the env
    /// its manifest says keeps it from updating itself
    /// (`Manifest::update_env`), so the pin holds.
    pub pins: Option<&'a crate::harness::Pins>,
    pub harnesses: Option<&'a crate::harness_manifest::Harnesses>,
    /// The env layers under an agent's own on this machine (`[defaults]`,
    /// the task's flock, the machine): every agent's pane gets them, the
    /// agent's `[agents]` env over them key by key.
    pub env: Option<&'a crate::config::EnvLayers>,
    /// The startup dialogs pastor has already answered for the task
    /// (`Store::answered`): its prompt is not held for them again
    /// (`Agents::startup_markers`).
    pub answered: &'a [String],
    /// What places a task's workdir set in its folder before the agent
    /// starts (`workdir_task::Gate`); none on a machine that places none.
    pub workdirs: Option<&'a crate::bundle::workdir_task::GateHandle>,
    /// How long the place of a task's workdir set may take; `None` is no
    /// bound. Its own time, so a slow place fails the task `workdir_failed`
    /// rather than reading as a herdr that stopped answering.
    pub workdir_wait: Option<std::time::Duration>,
}

/// `dispatch` on a machine whose provisioners want `provision`.
pub async fn dispatch_with(
    conn: &dyn Connector,
    task: &mut Task,
    agents: &Agents,
    head: Option<&str>,
    ready_timeout: Duration,
    provision: Provision<'_>,
) -> Result<DispatchOutcome, DispatchError> {
    start(conn, task, agents, head, ready_timeout, false, provision).await
}

/// `dispatch` for a paused task (`TaskState::Paused`): the same steps, but
/// the agent starts on the session it was paused in (`claude --resume
/// <session>`, in place of a new `--session-id`), a worktree task goes back
/// to its own checkout (`worktree.open`, never a new one: its session is
/// filed under that directory) and the prompt is `RESUME_PROMPT`, since the
/// task's own is already in the conversation. A checkout that is gone, or
/// no session recorded, fails the task.
pub async fn resume(
    conn: &dyn Connector,
    task: &mut Task,
    agents: &Agents,
    head: Option<&str>,
    ready_timeout: Duration,
) -> Result<DispatchOutcome, DispatchError> {
    resume_with(
        conn,
        task,
        agents,
        head,
        ready_timeout,
        Provision::default(),
    )
    .await
}

/// `resume` on a machine whose provisioners want `provision`.
pub async fn resume_with(
    conn: &dyn Connector,
    task: &mut Task,
    agents: &Agents,
    head: Option<&str>,
    ready_timeout: Duration,
    provision: Provision<'_>,
) -> Result<DispatchOutcome, DispatchError> {
    start(conn, task, agents, head, ready_timeout, true, provision).await
}

async fn start(
    conn: &dyn Connector,
    task: &mut Task,
    agents: &Agents,
    head: Option<&str>,
    ready_timeout: Duration,
    resume: bool,
    provision: Provision<'_>,
) -> Result<DispatchOutcome, DispatchError> {
    let name = Task::agent_name_for(task.id);
    task.agent_name = Some(name.clone());
    task.state = TaskState::Starting;
    task.error = None;
    task.prompt_pending = false;
    task.activity_seen = false;
    task.spec.start_prompt = None;

    let result = dispatch_steps(
        conn,
        task,
        &name,
        agents,
        head,
        ready_timeout,
        resume,
        provision,
    )
    .await;
    match &result {
        Ok(DispatchOutcome::Running) => {
            task.state = TaskState::Running;
            task.started_at = Some(Utc::now());
        }
        Ok(DispatchOutcome::Blocked) => {
            task.state = TaskState::Blocked;
            task.started_at = Some(Utc::now());
            // herdr rejected the input rather than queueing it; see
            // `Task::prompt_pending`.
            task.prompt_pending = true;
            task.error = Some("agent blocked during startup; answer its prompt".into());
        }
        Err(err) => {
            task.state = TaskState::Failed;
            task.error = Some(err.to_string());
            task.finished_at = Some(Utc::now());
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_steps(
    conn: &dyn Connector,
    task: &mut Task,
    name: &str,
    agents: &Agents,
    head: Option<&str>,
    ready_timeout: Duration,
    resume: bool,
    provision: Provision<'_>,
) -> Result<DispatchOutcome, DispatchError> {
    // Each dispatch says afresh what its fetch found.
    task.spec.fetch.warning = None;
    let spec = task.spec.clone();
    // A waiting task goes on after a usage limit (`TaskState::Waiting`): with
    // its session when it has one, else from its prompt again, in the same
    // checkout.
    let limited = resume && task.waiting_until.take().is_some();
    let restart = limited && spec.session_id.is_none();
    // What the round that stopped on the limit left; one that went to
    // another model says how that model learns what it did.
    let stop = if limited {
        task.spec.rounds.stop.take()
    } else {
        None
    };
    let moved = stop.as_deref().filter(|s| s.handover.is_some()).map(|s| {
        let from = crate::task::agent_and_model(&s.agent, s.model.as_deref());
        (from, s.tail.clone())
    });
    // agy reads its permission rules only from its settings on the machine,
    // and can write back a copy from before pastor's edit at any time, so
    // the agy provisioner makes them say what the machine's own profile says
    // before every launch, not once. First, so what checks those rules next
    // reads them as written.
    if agents.agy_profile(&spec) {
        let machine = task.machine.as_deref().unwrap_or("this machine");
        crate::provision::agy::reconcile(conn, provision.own, machine)
            .await
            .map_err(|e| match e {
                crate::provision::agy::ReconcileError::Transport(e) => CallError::from(e).into(),
                crate::provision::agy::ReconcileError::Refused(message) => {
                    DispatchError::Task(message)
                }
            })?;
    }
    // Before anything is made on the machine: a task whose agent cannot take
    // its tool lists (an `[agents]` edit since it was queued) leaves nothing
    // behind.
    let under = provision
        .env
        .map(|layers| layers.merged(task.flock.as_deref()))
        .unwrap_or_default();
    let mut launch = agents
        .launch_over(&spec, &under)
        .map_err(|e| DispatchError::Task(e.message))?;
    // opencode merges `OPENCODE_PERMISSION` over its own config's rules. Its
    // managed config, and on macOS its managed preferences, are merged after
    // `OPENCODE_CONFIG_CONTENT`, so pastor cannot neutralise rules set there,
    // and the task refuses rather than run under both. A machine that cannot
    // tell (a `command` one) goes ahead.
    if agents.opencode_profile(&spec) {
        if let Some(hits) = conn
            .opencode_permission_rules()
            .await
            .map_err(CallError::from)?
            && !hits.is_empty()
        {
            return Err(DispatchError::Task(format!(
                "{}: the opencode config on {} has permission rules of its own ({}), which \
                 opencode would merge with profile {}'s; move them out of its opencode config, \
                 or run without a profile",
                crate::config::opencode::OPENCODE_PERMISSIONS_CONFLICT,
                task.machine.as_deref().unwrap_or("this machine"),
                crate::config::opencode::describe_config_check_hits(&hits),
                spec.profile().unwrap_or_default(),
            )));
        }
        // The machine's own global config and `~/.opencode` are merged
        // before `OPENCODE_CONFIG_CONTENT`, so pastor neutralises them
        // instead: every top-level permission key found there is denied in
        // `OPENCODE_PERMISSION`, under every profile. What that cannot
        // reach (rules under an agent or a mode, a file pastor could not
        // parse, more config than it reads) leaves it unable to say what
        // would get through, so the task refuses the same way.
        match conn
            .opencode_config_permission_keys()
            .await
            .map_err(CallError::from)?
        {
            crate::config::opencode::MachineDenyKeys::Conflict(why) => {
                return Err(DispatchError::Task(format!(
                    "{}: the opencode config on {} {why}; move the rules out of its opencode \
                     config, or run without a profile",
                    crate::config::opencode::OPENCODE_PERMISSIONS_CONFLICT,
                    task.machine.as_deref().unwrap_or("this machine"),
                )));
            }
            crate::config::opencode::MachineDenyKeys::Keys(keys) if !keys.is_empty() => {
                launch.env.insert(
                    crate::config::opencode::PERMISSION_ENV.into(),
                    agents.opencode_permission_json(&spec, &keys),
                );
            }
            crate::config::opencode::MachineDenyKeys::Keys(_)
            | crate::config::opencode::MachineDenyKeys::None => {}
        }
    }
    // agy's rules are in its settings on the machine, and under the flags
    // pastor adds everything they do not deny runs: so what the task denies
    // has to be denied there, or the task does not start. A machine that
    // cannot be asked refuses too, unlike opencode's check: going ahead
    // would mean the skip flag with nothing verified behind it.
    if agents.agy_profile(&spec) && !spec.deny.is_empty() {
        let machine = task.machine.as_deref().unwrap_or("this machine");
        let profile = spec.profile().unwrap_or_default();
        let required: Vec<String> = spec
            .deny
            .iter()
            .filter_map(|p| crate::config::agy::required_rule(p))
            .collect();
        let why = match conn.agy_rules().await.map_err(CallError::from)? {
            None => Some(format!(
                "agy's permission rules on {machine} cannot be read (the machine takes no shell \
                 command), so pastor cannot check that profile {profile} holds there"
            )),
            Some(out) => match crate::config::agy::parse_denies(&out) {
                Err(e) => Some(format!(
                    "agy's permission rules on {machine} could not be read: {e}"
                )),
                Ok(denies) => {
                    let missing = crate::config::agy::missing(&required, &denies);
                    (!missing.is_empty()).then(|| {
                        format!(
                            "agy on {machine} does not deny what profile {profile} denies; add to \
                             \"permissions\".\"deny\" in {}:\n{}",
                            crate::config::agy::GLOBAL_SETTINGS,
                            missing.join("\n")
                        )
                    })
                }
            },
        };
        if let Some(why) = why {
            return Err(DispatchError::Task(format!(
                "{}: {why}",
                crate::config::agy::AGY_PERMISSIONS_MISSING
            )));
        }
    }
    // A Claude agent starts on a session pastor names, so `task attach` can
    // resume it once the pane is gone; last, after the tool flags. An agy
    // agent has no such flag, but is resumed the same way once pastor has
    // read its conversation id back from herdr (`Actor::capture_agent_session`).
    // The task records it only once `agent.start` succeeds
    // (`finish_dispatch`): a task that failed before then never had that
    // conversation to resume. A paused task goes back to the session it
    // recorded instead, by whichever flag its kind resumes on
    // (`task::resume_flag`); a kind with none, or a task with no session
    // recorded for it (an agy task on a machine with no hook installed),
    // falls back to starting fresh.
    let mut session = None;
    if resume && !restart {
        let flag = crate::task::resume_flag(&launch.kind).ok_or_else(|| {
            DispatchError::Task(format!(
                "{name}: a {} agent cannot resume a session",
                launch.kind
            ))
        })?;
        let id = spec
            .session_id
            .clone()
            .filter(|id| crate::task::is_resumable_session(&launch.kind, id))
            .ok_or_else(|| {
                DispatchError::Task(format!("{name} has no session recorded to resume"))
            })?;
        launch.args.extend([flag.to_string(), id.clone()]);
        session = Some(id);
    } else {
        task.spec.session_id = None;
        if launch.kind == "claude"
            && !crate::task::picks_session(&launch.args)
            && let Some(id) = crate::task::new_session_id()
        {
            launch.args.extend(["--session-id".to_string(), id.clone()]);
            session = Some(id);
        }
    }
    let repo = match spec.repo.as_deref() {
        Some(repo) => Some(expand_home(conn, "repo", repo, task.machine.as_deref()).await?),
        None => None,
    };
    let mut env = launch.env.clone();
    for (key, value) in env.iter_mut() {
        if value == "~" || value.starts_with("~/") {
            *value =
                expand_home(conn, &format!("env {key}"), value, task.machine.as_deref()).await?;
        }
    }
    // A harness its machine pins to a version must not update itself; over
    // the `[agents]` env, so the pin holds.
    if let (Some(pins), Some(harnesses)) = (provision.pins, provision.harnesses)
        && pins
            .get(&launch.kind)
            .is_some_and(|pin| pin != crate::harness::LATEST)
        && let Some(m) = harnesses.get(&launch.kind)
    {
        env.extend(m.update_env.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    // The mark the head refuses fleet changes by (`ipc::TASK_ENV`). Last,
    // so an `[agents]` env cannot clear it.
    env.insert(crate::ipc::TASK_ENV.into(), name.to_string());
    // Where an agent off the head's machine reaches the head; the same
    // reason puts it last.
    if let Some(head) = head {
        env.insert(crate::ipc::HEAD_ENV.into(), head.to_string());
    }
    if let Some(dir) = repo.as_deref() {
        check_repo_exists(conn, dir, task.machine.as_deref()).await?;
    }
    if spec.worktree && repo.is_none() {
        return Err(HerdrError::Protocol("worktree = true needs repo".into()).into());
    }
    let host = host_workspace(conn, &spec, repo.as_deref(), task.machine.as_deref()).await?;
    // A profiled opencode agent reads no project config, so the repo's
    // instruction files go in by path, from where it works.
    let opencode = agents.opencode_profile(&spec);
    let pane_env = async |cwd: Option<&str>| -> Result<_, DispatchError> {
        let mut env = env.clone();
        if opencode && let Some(cwd) = cwd {
            env.insert(
                crate::config::opencode::CONFIG_CONTENT_ENV.into(),
                crate::config::opencode::instructions_content(&instruction_files(conn, cwd).await?),
            );
        }
        Ok(env)
    };
    let dir = match repo.clone() {
        Some(repo) => Some(repo),
        // A resume goes back to the directory recorded at the first dispatch
        // (as `reopen` does), rather than asking `no_repo_dir` again and
        // getting a different answer once the folder has since become
        // makeable: the session is filed under the old directory, and the
        // agent needs to be started there to find it.
        None => match task.spec.cwd.clone() {
            Some(cwd) if resume => Some(cwd),
            _ => no_repo_dir(conn, task.machine.as_deref()).await?,
        },
    };
    if repo.is_none() {
        task.spec.cwd = dir.clone();
    }
    task.spec.label.name = None;
    task.spec.label.note = None;
    let pane_id = match (host, repo.as_deref()) {
        // A pane of the task's own in a workspace someone else has: only
        // that pane is recorded, so closing the task closes only it. The
        // workspace keeps its label.
        (Some(host), _) => {
            task.spec.label.name = host.label.clone();
            task.spec.label.note = Some(crate::task::JOINED_WORKSPACE.into());
            // A worktree is still made on disk, and the agent works in it;
            // only a workspace herdr just opened on it goes, with its one
            // pane. One that was already showing the checkout
            // (`already_open`) is someone else's and stays as it was.
            let mut worktree = None;
            if spec.worktree
                && let Some(repo) = repo.as_deref()
            {
                // Its workspace goes in a moment, so its label is the
                // agent's name, never the template.
                let (created, branch) = open_worktree(
                    conn,
                    &spec,
                    repo,
                    name,
                    name,
                    resume,
                    &mut task.spec.fetch.warning,
                )
                .await?;
                task.spec.checkout = find_checkout(conn, repo, branch, &created).await?;
                worktree = Some(created);
            }
            let cwd = match worktree {
                Some(_) => task.spec.checkout.as_ref().map(|c| c.path.clone()),
                None => dir.clone(),
            };
            let pane = conn
                .pane_split(
                    &host.pane_id,
                    cwd.as_deref(),
                    &pane_env(cwd.as_deref()).await?,
                )
                .await?;
            task.workspace_id = Some(host.workspace_id);
            task.pane_id = Some(pane.pane_id.clone());
            if let Some(created) = worktree.filter(|c| !c.already_open) {
                conn.pane_close(&created.root_pane.pane_id).await?;
            }
            pane.pane_id
        }
        (None, Some(repo)) if spec.worktree => {
            let label = workspace_label(task, name);
            let (created, branch) = open_worktree(
                conn,
                &spec,
                repo,
                name,
                &label,
                resume,
                &mut task.spec.fetch.warning,
            )
            .await?;
            // `worktree.open` answering `already_open` means the checkout
            // was already in a workspace another task opened: that
            // workspace's label is unchanged by this task, so its own
            // label, not the template just rendered, is what gets recorded.
            if created.already_open {
                task.spec.label.name = created.workspace.label.clone();
                task.spec.label.note = Some(crate::task::JOINED_WORKSPACE.into());
            }
            task.workspace_id = Some(created.workspace.workspace_id.clone());
            task.pane_id = Some(created.root_pane.pane_id.clone());
            task.spec.checkout = find_checkout(conn, repo, branch, &created).await?;
            // `worktree.create` and `worktree.open` take no env, and every
            // agent has one (`TASK_ENV`), so the agent gets a pane split off
            // the worktree's with it, and the pane without it goes: a task
            // keeps one pane, whose close ends the workspace. Unless
            // `worktree.open` answered a workspace already showing the
            // checkout: its root pane is someone else's, and stays.
            let root = created.root_pane.pane_id;
            let cwd = task.spec.checkout.as_ref().map(|c| c.path.clone());
            let pane = conn
                .pane_split(&root, cwd.as_deref(), &pane_env(cwd.as_deref()).await?)
                .await?;
            task.pane_id = Some(pane.pane_id.clone());
            if !created.already_open {
                conn.pane_close(&root).await?;
            }
            pane.pane_id
        }
        (None, _) => {
            let label = workspace_label(task, name);
            let created = conn
                .workspace_create(dir.as_deref(), &label, &pane_env(dir.as_deref()).await?)
                .await?;
            task.workspace_id = Some(created.workspace.workspace_id.clone());
            task.pane_id = Some(created.root_pane.pane_id.clone());
            created.root_pane.pane_id
        }
    };
    // A folder pastor already trusts is trusted in the harness's own store
    // before the agent can ask about it.
    let folder = task
        .spec
        .checkout
        .as_ref()
        .map(|c| c.path.clone())
        .or_else(|| dir.clone());
    crate::trust_write::before_start(
        conn,
        provision.trusted,
        provision.trust_recheck,
        task,
        &launch,
        folder.as_deref(),
    )
    .await;
    // The task's workdir set, in the folder the agent starts in, after the
    // trust (agy loads a folder's hooks only once it is trusted) and before
    // the agent, which reads it as it starts.
    place_workdir(
        task,
        &launch.kind,
        folder.as_deref(),
        provision.workdirs,
        provision.workdir_wait,
    )
    .await?;
    // A resumed session already has the line asking for a summary.
    let to = crate::task::agent_and_model(&task.spec.agent, task.model());
    let prompt = if let Some((from, tail)) = moved.as_ref().filter(|_| restart) {
        format!(
            "{}\n\n{}",
            crate::task::prompt_to_send(task).trim_end(),
            crate::task::limit_handover(from, tail)
        )
    } else if let Some((from, _)) = &moved {
        crate::task::switch_prompt(from, &to)
    } else if restart {
        format!(
            "{}\n\n{}",
            crate::task::prompt_to_send(task).trim_end(),
            crate::task::LIMIT_HANDOVER
        )
    } else if limited {
        crate::task::LIMIT_RESUME_PROMPT.to_string()
    } else if resume {
        crate::task::RESUME_PROMPT.to_string()
    } else {
        crate::task::prompt_to_send(task)
    };
    // Kept for a delivery held at a startup dialog, or sent again: it must
    // be this prompt, not the task's own a resumed session already ran.
    task.spec.start_prompt = (prompt != crate::task::prompt_to_send(task)).then(|| prompt.clone());
    finish_dispatch(FinishDispatch {
        conn,
        task,
        name,
        launch: &launch,
        session,
        pane_id: &pane_id,
        prompt: &prompt,
        agents,
        answered: provision.answered,
        ready_timeout,
    })
    .await
}

/// Place `task`'s stored workdir bundles, then its own workdir set, in
/// `folder` through `gate`, the set with the task's profile in it when it
/// names none, and record where. Fails the start (`workdir_failed`) when
/// one cannot be placed: the agent would start without what it gives.
///
/// A stored bundle never decides where a task runs, so on a machine that
/// places no sets, and for a kind of agent pastor places none for, the
/// bundles are skipped instead, with why on each (the actor raises
/// `workdir.skipped`). A bundle with nothing for the agent's kind places
/// nothing.
async fn place_workdir(
    task: &mut Task,
    kind: &str,
    folder: Option<&str>,
    gate: Option<&crate::bundle::workdir_task::GateHandle>,
    wait: Option<std::time::Duration>,
) -> Result<(), DispatchError> {
    use crate::bundle::workdir_push::WORKDIR_FAILED;
    use crate::bundle::workdir_task::Placing;
    let stored = !task.spec.workdir_bundles.is_empty();
    if task.spec.workdir.is_none() && !stored {
        return Ok(());
    }
    let machine = task.machine.clone().unwrap_or_default();
    let fail = |why: String| DispatchError::Task(format!("{WORKDIR_FAILED}: {why}"));
    let own = match task.spec.workdir.as_deref() {
        Some(w) => Some(
            crate::bundle::workdir_task::settle(&w.set, kind, task.spec.profile())
                .map_err(|r| fail(format!("{} ({})", r.message, r.code)))?,
        ),
        None => None,
    };
    let gate = gate.and_then(|g| g.0.clone());
    // Every task places a stored bundle under the bundle's own id, so in a
    // folder tasks share each start would replace the last one's version,
    // taking out files another agent there still reads. Only a checkout
    // pastor opened for this task is its own.
    let own_folder = task
        .spec
        .checkout
        .as_ref()
        .is_some_and(|c| !c.already_open && folder == Some(c.path.as_str()));
    let skip = match &gate {
        _ if !stored => None,
        None => Some(format!(
            "{machine} does not place workdir sets: pastor places them only on push machines"
        )),
        Some(_) if !crate::bundle::workdir_stored::places(kind) => Some(format!(
            "pastor places no workdir sets for {kind} agents yet"
        )),
        Some(_) if !own_folder => Some(crate::bundle::workdir_task::shared_folder(
            folder.unwrap_or("none"),
        )),
        Some(_) => None,
    };
    if let Some(why) = &skip {
        for b in &mut task.spec.workdir_bundles {
            b.skipped = Some(why.clone());
        }
    }
    let Some(gate) = gate else {
        if own.is_none() {
            return Ok(());
        }
        return Err(fail(format!(
            "{machine} does not place workdir sets: pastor places a task's set on push machines, and on pull machines whose pastor places them"
        )));
    };
    let mut all: Vec<Placing> = Vec::new();
    // A set the head built when a pull machine claimed the task is for the
    // kind it was claimed for; an agent of another kind leaves it out.
    let built_for = |b: &crate::task::WorkdirBundle| {
        b.set
            .as_ref()
            .and_then(|d| d.get("kind"))
            .and_then(|k| k.as_str())
            .map(str::to_string)
    };
    if skip.is_none() {
        for b in &task.spec.workdir_bundles {
            match (&b.set, built_for(b)) {
                (Some(doc), Some(k)) if k == kind => all.push(Placing::Set(doc.clone())),
                (Some(_), _) => {}
                (None, _) => all.push(Placing::Bundle {
                    id: b.id.clone(),
                    kind: kind.to_string(),
                    flock: task.flock.clone(),
                }),
            }
        }
    }
    let render = task.spec.workdir.as_deref().and_then(|w| w.render.clone());
    all.extend(own.map(|set| match render {
        Some(render) => Placing::Rendered { set, render },
        None => Placing::Set(set),
    }));
    if all.is_empty() {
        return Ok(());
    }
    let Some(folder) = folder else {
        return Err(fail("the task has no folder to place its set in".into()));
    };
    crate::bundle::workdir_task::place_within(gate, &machine, folder, all, wait)
        .await
        .map_err(DispatchError::Task)?;
    if skip.is_none() {
        for b in &mut task.spec.workdir_bundles {
            b.skipped = match built_for(b) {
                Some(k) if k != kind => Some(format!(
                    "its set was built for {k} agents when the task was claimed, and its agent is {kind}"
                )),
                _ => None,
            };
        }
    }
    if let Some(w) = task.spec.workdir.as_mut() {
        w.placed_in = Some(folder.to_string());
    }
    Ok(())
}

/// `finish_dispatch`'s arguments, grouped.
struct FinishDispatch<'a> {
    conn: &'a dyn Connector,
    task: &'a mut Task,
    name: &'a str,
    launch: &'a crate::config::Launch,
    session: Option<String>,
    pane_id: &'a str,
    prompt: &'a str,
    agents: &'a Agents,
    answered: &'a [String],
    ready_timeout: Duration,
}

/// Start the agent in its pane, wait for it to come up and give it `prompt`.
async fn finish_dispatch(ask: FinishDispatch<'_>) -> Result<DispatchOutcome, DispatchError> {
    let FinishDispatch {
        conn,
        task,
        name,
        launch,
        session,
        pane_id,
        prompt,
        agents,
        answered,
        ready_timeout,
    } = ask;
    // herdr's `agent.start` returns as soon as it has launched the agent in the
    // pane; it never reports `agent_not_ready` (its errors are about the name,
    // the kind and the pane). Readiness shows up afterwards, in `agent.list` and
    // in whether `agent.prompt` is accepted.
    start_agent(conn, name, &launch.kind, &launch.args, pane_id).await?;
    task.spec.session_id = session;

    let screens = Screens {
        hold: agents.startup_markers(&task.spec.agent, answered),
        login: agents.login_markers(&task.spec.agent),
        kind: launch.kind.clone(),
    };
    let inbox = Inbox::of(agents, &task.spec);
    let (outcome, prompted) = prompt_when_ready(
        conn,
        task,
        name,
        prompt,
        &screens,
        inbox.as_ref(),
        ready_timeout,
    )
    .await?;
    // The baseline a completion must move past, and whether the agent was
    // already at work when the prompt went in; see
    // `task::completed_since_prompt`.
    if let Some(Prompted { agent, activity }) = prompted {
        task.last_completion_seq = Some(agent.state_change_seq);
        task.activity_seen = activity;
        // An agy conversation id its hook already reported, the same as
        // `Actor::capture_agent_session` reads from `agent.list`.
        if let Some(session) = crate::task::reported_session(&launch.kind, &agent) {
            task.spec.session_id = Some(session.to_string());
        }
    }
    Ok(outcome)
}

/// A workspace the task's pane joins rather than one it makes, and the pane
/// to split it off.
struct Host {
    workspace_id: String,
    pane_id: String,
    /// The workspace's label, which the task leaves as it is.
    label: Option<String>,
}

/// The label of the workspace a task makes, recorded on it: its template
/// rendered (`task::render_label`), or `name` with the reason noted when
/// that is refused. Only the workspace is named so; the agent keeps `name`,
/// since pastor finds its agents by it.
fn workspace_label(task: &mut Task, name: &str) -> String {
    let label = match render_label(task.spec.label.template.as_deref(), task) {
        Ok(label) => label,
        Err(why) => {
            tracing::warn!(
                task = name,
                template = task.spec.label.template.as_deref().unwrap_or(crate::task::DEFAULT_LABEL),
                %why,
                "label refused; the workspace is named after the task"
            );
            task.spec.label.note = Some(format!("fell back to {name}: {why}"));
            name.to_string()
        }
    };
    task.spec.label.name = Some(label.clone());
    label
}

/// The label of the workspace `place = "pastor"` shares.
const PASTOR_WORKSPACE: &str = "pastor";

/// Where `spec.place` puts the task's pane, when that is a workspace the
/// task does not make: `None` means a workspace (or worktree) of its own.
///
/// - `repo`: the workspace already showing a task's repo, for a task
///   without a worktree; herdr reports a workspace's directory only when it
///   is a git checkout, compared without trailing slashes.
/// - `pastor`: the machine's workspace labelled `pastor`, made on first use.
/// - `pane:<label>`: the workspace with that label; refused when there is
///   none, before anything is made.
///
/// Only `repo` ever answers `None` for a workspace that closes midway.
async fn host_workspace(
    conn: &dyn Connector,
    spec: &DispatchSpec,
    repo: Option<&str>,
    machine: Option<&str>,
) -> Result<Option<Host>, DispatchError> {
    let labelled = |label: &str, list: &[crate::herdr::WorkspaceInfo]| {
        list.iter()
            .find(|w| w.label.as_deref() == Some(label))
            .map(|w| (w.workspace_id.clone(), w.label.clone()))
    };
    // A workspace can close between `workspace.list` and `pane.list`. Only
    // `repo` may then fall back to a workspace of the task's own; a named
    // one is looked up once more (`pastor` is made again if it is gone),
    // and still gone fails the task rather than put it somewhere else.
    for _ in 0..2 {
        let (workspace, label) = match &spec.place {
            Place::Own => return Ok(None),
            Place::Repo => {
                let Some(repo) = repo.filter(|_| !spec.worktree) else {
                    return Ok(None);
                };
                let showing = conn.workspace_list().await?.into_iter().find(|w| {
                    w.worktree
                        .as_ref()
                        .is_some_and(|c| same_dir(&c.checkout_path, repo))
                });
                match showing {
                    Some(w) => (w.workspace_id, w.label),
                    None => return Ok(None),
                }
            }
            Place::Pastor => match labelled(PASTOR_WORKSPACE, &conn.workspace_list().await?) {
                Some(ws) => ws,
                None => {
                    let created = conn
                        .workspace_create(None, PASTOR_WORKSPACE, &Default::default())
                        .await?;
                    return Ok(Some(Host {
                        workspace_id: created.workspace.workspace_id,
                        pane_id: created.root_pane.pane_id,
                        label: Some(PASTOR_WORKSPACE.into()),
                    }));
                }
            },
            Place::Pane(label) => match labelled(label, &conn.workspace_list().await?) {
                Some(ws) => ws,
                None => {
                    return Err(DispatchError::Task(format!(
                        "place pane:{label}: no workspace named {label} on {}",
                        machine.unwrap_or("this machine")
                    )));
                }
            },
        };
        // A workspace always has a pane while it is open.
        match conn.pane_list(&workspace).await {
            Ok(panes) => {
                if let Some(p) = panes.into_iter().next() {
                    return Ok(Some(Host {
                        workspace_id: workspace,
                        pane_id: p.pane_id,
                        label,
                    }));
                }
            }
            Err(err) if err.code() == Some("workspace_not_found") => {}
            Err(err) => return Err(err.into()),
        }
        if spec.place == Place::Repo {
            return Ok(None);
        }
    }
    Err(DispatchError::Task(format!(
        "place {}: its workspace closed while pastor placed the task on {}",
        spec.place,
        machine.unwrap_or("this machine")
    )))
}

/// Do two paths name the same directory? herdr reports a checkout's path
/// without a trailing slash; a `--repo` may carry one.
pub(crate) fn same_dir(a: &str, b: &str) -> bool {
    let trim = |p: &str| {
        let t = p.trim_end_matches('/');
        if t.is_empty() { "/" } else { t }.to_string()
    };
    trim(a) == trim(b)
}

/// Fetch the remote `repo`'s base comes from before a worktree is made off
/// it (`Connector::fetch_base`). Whatever goes wrong is a warning to put on
/// the task, never an error: the worktree is made from what the repo has.
/// `None` when it was fetched, there was no remote, or the machine cannot
/// be asked.
async fn fetch_base(conn: &dyn Connector, repo: &str) -> Option<String> {
    let why = match conn.fetch_base(repo).await {
        Ok(None | Some(Fetched::Done(_) | Fetched::NoRemote)) => return None,
        Ok(Some(Fetched::Failed(remote, why))) => format!("could not fetch {remote}: {why}"),
        Err(err) => format!("could not fetch: {}", err.message),
    };
    tracing::warn!(%repo, %why, "fetch before a worktree: going on from the repo as it is");
    Some(format!("{why}; the worktree was made from {repo} as it is"))
}

/// The workspace of a worktree task, labelled `label`, and the branch it is
/// on: the checkout a retry may reopen (`reopenable`), or else a new
/// worktree on the task's branch, `pastor/<name>` by default, made after
/// the repo's remote is fetched unless the task said `--no-fetch`; a fetch
/// that failed leaves its reason in `fetch_warning`.
async fn open_worktree(
    conn: &dyn Connector,
    spec: &DispatchSpec,
    repo: &str,
    name: &str,
    label: &str,
    resume: bool,
    fetch_warning: &mut Option<String>,
) -> Result<(Created, String), DispatchError> {
    // A paused task's own checkout, kept when it was paused: only it will
    // do, since Claude files the session under that directory.
    if resume && let Some(checkout) = spec.checkout.as_deref() {
        return match conn.worktree_open(repo, &checkout.branch, label).await {
            Ok(created) => Ok((created, checkout.branch.clone())),
            Err(err) if err.code() == Some("worktree_not_found") => {
                Err(DispatchError::Task(format!(
                    "its worktree {} on branch {} is gone, so its session has nowhere to resume",
                    checkout.path, checkout.branch
                )))
            }
            Err(err) => Err(err.into()),
        };
    }
    if let Some(reopen) = reopenable(conn, spec, repo).await? {
        let created = conn.worktree_open(repo, &reopen.branch, label).await?;
        return Ok((created, reopen.branch.clone()));
    }
    if let Some(reopen) = restorable(conn, spec, repo).await? {
        let created = conn.worktree_open(repo, &reopen.branch, label).await?;
        return Ok((created, reopen.branch.clone()));
    }
    let branch = spec
        .branch
        .clone()
        .unwrap_or_else(|| format!("pastor/{name}"));
    // A new worktree is the one place the base matters; a retry of the same
    // task attempts it afresh.
    *fetch_warning = None;
    if !spec.fetch.skip {
        *fetch_warning = fetch_base(conn, repo).await;
    }
    Ok((conn.worktree_create(repo, &branch, label).await?, branch))
}

/// The one rule for reopening a checkout. A retry goes back to the checkout
/// of the failed task it retries (`DispatchSpec::reopen`, which
/// `Store::insert_retry` sets only from the checkout that task's own
/// dispatch recorded) only while it is on disk at the same path, on the same
/// branch, that task's agent is gone and no agent is listed in the
/// workspace showing the checkout: the failed task's work is there, and
/// nobody else is at work in it. The machine actor has already dropped
/// `reopen` when a task's agent works in the checkout from a workspace not
/// showing it (`Actor::keep_occupied_checkout`). Anything else is `None` and gets a new
/// branch and worktree, since the checkout may now be another task's, the
/// old agent may still be editing it, or an earlier retry of the same task
/// may have reopened it.
async fn reopenable<'a>(
    conn: &dyn Connector,
    spec: &'a DispatchSpec,
    repo: &str,
) -> Result<Option<&'a Reopen>, DispatchError> {
    let Some(reopen) = spec.reopen.as_ref() else {
        return Ok(None);
    };
    let worktrees = conn.worktree_list(repo).await?;
    let Some(checkout) = worktrees
        .iter()
        .find(|w| w.path == reopen.path && w.branch.as_deref() == Some(reopen.branch.as_str()))
    else {
        return Ok(None);
    };
    // Any agent in the checkout's workspace, not only the old task's: a
    // retry that reopened it earlier has an agent there under its own name.
    let workspace = checkout.open_workspace_id.as_deref();
    let in_use = conn.agent_list().await?.iter().any(|a| {
        a.name.as_deref() == Some(reopen.agent.as_str())
            || Some(a.workspace_id.as_str()) == workspace
    });
    Ok((!in_use).then_some(reopen))
}

/// A retry of a task that asked for its worktree to go
/// (`DispatchSpec::removes_worktree`), whose checkout auto-close removed
/// while its branch stayed: the checkout is put back on that branch at its
/// old path (`Connector::restore_worktree`), so the retry goes on from the
/// failed task's commits rather than from a fresh `pastor/t-<id>`. `None`,
/// a new branch, when no checkout of the branch is gone to put back, the
/// branch is gone too, or the machine cannot tell.
async fn restorable<'a>(
    conn: &dyn Connector,
    spec: &'a DispatchSpec,
    repo: &str,
) -> Result<Option<&'a Reopen>, DispatchError> {
    let Some(reopen) = spec.reopen.as_ref().filter(|_| spec.removes_worktree()) else {
        return Ok(None);
    };
    // Still checked out somewhere (reopenable found it in use, or moved):
    // git would refuse a second checkout of the branch.
    if conn
        .worktree_list(repo)
        .await?
        .iter()
        .any(|w| same_dir(&w.path, &reopen.path) || w.branch.as_deref() == Some(&reopen.branch))
    {
        return Ok(None);
    }
    let restored = conn
        .restore_worktree(repo, &reopen.path, &reopen.branch)
        .await
        .map_err(CallError::from)?;
    Ok((restored == Some(true)).then_some(reopen))
}

/// The checkout herdr just made or reopened for this task, recorded so that
/// a retry knows it is this task's own, and whether its workspace was
/// already open (so closing never removes it). Taken from `worktree.list`, the
/// listing `reopenable` compares against, by the workspace showing it.
async fn find_checkout(
    conn: &dyn Connector,
    repo: &str,
    branch: String,
    created: &Created,
) -> Result<Option<Box<Checkout>>, DispatchError> {
    let workspace = created.workspace.workspace_id.as_str();
    Ok(conn
        .worktree_list(repo)
        .await?
        .into_iter()
        .find(|w| w.open_workspace_id.as_deref() == Some(workspace))
        .map(|w| {
            Box::new(Checkout {
                branch: w.branch.unwrap_or(branch),
                path: w.path,
                already_open: created.already_open,
            })
        }))
}

/// `agent.start`, retried while herdr says the pane is busy: its shell has not
/// finished starting yet (see `PANE_BUSY_ATTEMPTS`). Matched on the code, not
/// the message. Every other error fails at once.
async fn start_agent(
    conn: &dyn Connector,
    name: &str,
    agent: &str,
    args: &[String],
    pane_id: &str,
) -> Result<(), DispatchError> {
    let mut attempt = 1;
    loop {
        match conn.agent_start(name, agent, pane_id, args).await {
            Ok(_) => return Ok(()),
            Err(err) if err.code() == Some("agent_pane_busy") => {
                if attempt >= PANE_BUSY_ATTEMPTS {
                    // Keep the code so the error still reads as herdr's; add
                    // how long pastor tried.
                    let message = match err {
                        CallError::Herdr(HerdrError::Api { message, .. }) => message,
                        other => other.to_string(),
                    };
                    return Err(HerdrError::Api {
                        code: "agent_pane_busy".into(),
                        message: format!("{message} after {attempt} attempts"),
                    }
                    .into());
                }
                tracing::debug!(
                    agent = name,
                    pane = pane_id,
                    attempt,
                    %err,
                    "pane busy, retrying agent.start"
                );
                attempt += 1;
                tokio::time::sleep(PANE_BUSY_WAIT).await;
            }
            Err(err) => return Err(err.into()),
        }
    }
}

/// Where a task with no repo starts, under the machine's home.
pub const NO_REPO_DIR: &str = "pastor-tasks";

/// Where a task with no repo starts: herdr opens a pane with no `cwd`
/// wherever its focused pane is, which can be anyone's checkout (one such task opened
/// in a work repo). Not the home itself either: Claude Code never saves
/// folder trust for the home, so it asked at every start. So
/// `~/pastor-tasks`, made when missing, where Claude asks once per machine.
/// A folder that cannot be made falls back to the home; a machine that
/// cannot tell its home leaves it to herdr, as before.
pub(crate) async fn no_repo_dir(
    conn: &dyn Connector,
    machine: Option<&str>,
) -> Result<Option<String>, DispatchError> {
    let Some(home) = conn.home_dir().await.map_err(CallError::from)? else {
        return Ok(None);
    };
    let dir = format!("{}/{NO_REPO_DIR}", home.trim_end_matches('/'));
    match conn.ensure_dir(&dir).await.map_err(CallError::from)? {
        Some(true) => Ok(Some(dir)),
        _ => {
            tracing::warn!(
                machine = machine.unwrap_or("this machine"),
                %dir,
                "cannot make the folder for tasks with no repo; starting in the home"
            );
            Ok(Some(home))
        }
    }
}

/// Expand a leading `~` in `repo` against the machine's home directory.
///
/// herdr takes `cwd` literally: `~/work` is a directory named `~` to it, and a
/// `cwd` that does not exist silently opens the pane somewhere else. Job files
/// and `pastor task run --repo` both use `~` to mean the home on that machine, so
/// pastor resolves it there before asking herdr.
pub(crate) async fn expand_home(
    conn: &dyn Connector,
    what: &str,
    repo: &str,
    machine: Option<&str>,
) -> Result<String, DispatchError> {
    let rest = match repo.strip_prefix('~') {
        None => return Ok(repo.to_string()),
        Some(rest) if rest.is_empty() || rest.starts_with('/') => rest,
        Some(_) => {
            return Err(DispatchError::Task(format!(
                "{what} {repo}: only ~ and ~/ are expanded, not ~user; use an absolute path"
            )));
        }
    };
    let machine = machine.unwrap_or("this machine");
    match conn.home_dir().await.map_err(CallError::from)? {
        // A bare `~` is the home as reported, `/` included; only a suffix
        // needs the trailing slash dropped to avoid `//`.
        Some(home) if rest.is_empty() => Ok(home),
        Some(home) => Ok(format!("{}{rest}", home.trim_end_matches('/'))),
        None => Err(DispatchError::Task(format!(
            "{what} {repo}: pastor cannot tell the home directory on {machine}; \
             use an absolute path"
        ))),
    }
}

/// herdr opens a workspace whose `cwd` does not exist in the shell's home
/// instead, with no error, and the agent then works on the wrong tree (t-5,
/// 2026-09-24). Refuse before creating anything. A machine that cannot answer
/// (`None`: a `command` bridge, a shell that printed nothing) goes ahead, as
/// before.
/// The instruction file a profiled opencode task working in `dir` gets back:
/// the first of `opencode::instruction_candidates` that is there, as opencode
/// itself would pick, none when none is. Every candidate when the machine
/// cannot say, since a file that is not there matches nothing.
async fn instruction_files(conn: &dyn Connector, dir: &str) -> Result<Vec<String>, DispatchError> {
    let candidates = crate::config::opencode::instruction_candidates(dir);
    for file in &candidates {
        match conn.file_exists(file).await.map_err(CallError::from)? {
            Some(true) => return Ok(vec![file.clone()]),
            Some(false) => {}
            None => return Ok(candidates),
        }
    }
    Ok(vec![])
}

async fn check_repo_exists(
    conn: &dyn Connector,
    repo: &str,
    machine: Option<&str>,
) -> Result<(), DispatchError> {
    match conn.dir_exists(repo).await.map_err(CallError::from)? {
        Some(false) => Err(DispatchError::Task(format!(
            "repo {repo} does not exist on {}",
            machine.unwrap_or("this machine")
        ))),
        Some(true) | None => Ok(()),
    }
}

/// What sending a task's prompts through its Claude session's inbox socket
/// takes (`Connector::inbox_send`): its agent delivers that way
/// (`Agents::delivery`) and pastor knows its session.
pub(crate) struct Inbox {
    config_dir: Option<String>,
    session: String,
    mode: crate::inbox::Mode,
}

impl Inbox {
    /// `None` when `spec`'s prompts are typed.
    pub(crate) fn of(agents: &Agents, spec: &DispatchSpec) -> Option<Inbox> {
        if agents.delivery(&spec.agent) != crate::config::Delivery::Socket {
            return None;
        }
        let session = spec
            .session_id
            .clone()
            .filter(|s| crate::task::is_session_id(s))?;
        // Under a profile Claude starts with `dontAsk`, which prompts; only
        // its own args can make it bypass.
        let mode = if crate::config::claude_bypasses(&spec.agent_args) {
            crate::inbox::Mode::Bypass
        } else {
            crate::inbox::Mode::Prompting
        };
        Some(Inbox {
            config_dir: crate::usage::config_dir(agents, &spec.agent).map(str::to_string),
            session,
            mode,
        })
    }

    /// Send `text` to the session; whether it took it. Anything else is
    /// logged with the task, and the caller types the text instead.
    pub(crate) async fn send(&self, conn: &dyn Connector, task: &str, text: &str) -> bool {
        let sent = conn
            .inbox_send(self.config_dir.as_deref(), &self.session, self.mode, text)
            .await;
        match sent {
            Ok(Some(out)) if out.accepted() => {
                tracing::info!(task, "prompt sent through the inbox socket");
                true
            }
            Ok(Some(out)) => {
                tracing::info!(task, result = out.result, detail = ?out.detail, "the inbox socket did not take the prompt; typing it");
                false
            }
            Ok(None) => {
                tracing::info!(
                    task,
                    "no inbox socket send on this machine; typing the prompt"
                );
                false
            }
            Err(err) => {
                tracing::warn!(task, %err, "inbox socket send failed; typing the prompt");
                false
            }
        }
    }
}

/// The agent as a prompt went in, and whether it was at work by then. A
/// prompt typed with `agent.prompt` has the reply, which is newer than the
/// prompt; one sent through the socket has the `agent.list` read just
/// before, and no work since it is seen yet.
struct Prompted {
    agent: AgentInfo,
    activity: bool,
}

/// What `prompt_when_ready` reads a starting agent's pane for.
struct Screens {
    /// Startup questions herdr reads idle and ready
    /// (`Agents::startup_markers`).
    hold: Vec<String>,
    /// The agent's login screen (`Agents::login_markers`).
    login: Vec<String>,
    /// The agent's kind, for `limit::signed_out_in`.
    kind: String,
}

/// Poll `agent.list` until the agent herdr just started is up, then prompt it.
///
/// herdr answers `agent_not_ready` both while a managed agent is still launching
/// (transient) and once the agent is no longer the pane's foreground process —
/// an agent that exited, e.g. because its binary is not installed on that
/// machine. The two are told apart by whether the agent is still in
/// `agent.list`: gone means failed now, present-but-`unknown` means wait.
/// Present-and-`blocked` is a third case, checked before either: herdr answers
/// that with `agent_blocked`, not `agent_not_ready`, and dispatch returns
/// `Blocked` without prompting or waiting. So is an agent whose pane shows
/// one of `hold` (`Agents::startup_markers`): a startup question herdr reads
/// idle and ready (Codex's folder trust, any of agy's, Claude's before herdr
/// has noticed it), which a prompt would answer and be lost in. With `inbox`, a ready agent is sent the prompt through its
/// session's socket first, and typed only when that does not take it. An
/// agent at its login screen, blocked or ready (`limit::signed_out_in`), is
/// `DispatchError::Exited` with that screen as its tail: it is signed out,
/// and the machine holds the task as it holds one on a usage limit.
async fn prompt_when_ready(
    conn: &dyn Connector,
    task: &Task,
    name: &str,
    prompt: &str,
    screens: &Screens,
    inbox: Option<&Inbox>,
    ready_timeout: Duration,
) -> Result<(DispatchOutcome, Option<Prompted>), DispatchError> {
    let hold = &screens.hold;
    let machine = task.machine.as_deref().unwrap_or("that machine");
    let deadline = Instant::now() + ready_timeout;
    loop {
        let agents = conn.agent_list().await?;
        let Some(agent) = agents.iter().find(|a| a.name.as_deref() == Some(name)) else {
            return Err(exited(
                conn,
                task,
                format!(
                    "agent {name} exited before accepting a prompt (is `{}` installed on {machine}?)",
                    task.spec.agent
                ),
            )
            .await);
        };
        let ready = agent.agent_status == AgentStatus::Blocked
            || (!agent.launch_pending
                && (agent.interactive_ready || agent.agent_status == AgentStatus::Working));
        if ready && !screens.login.is_empty() {
            // A pane that cannot be read is taken as signed in.
            let screen = conn.agent_read(name, 100).await.unwrap_or_default();
            if crate::limit::signed_out_in(&screens.kind, &screen, &screens.login).is_some() {
                return Err(DispatchError::Exited {
                    message: format!(
                        "agent {name} is signed out on {machine}: it showed its login screen"
                    ),
                    tail: screen,
                });
            }
        }
        if agent.agent_status == AgentStatus::Blocked {
            // Waiting for a human, usually on the agent's own startup question
            // (Claude's folder trust dialog). herdr checks `blocked` before
            // `launch_pending`, answering `agent_blocked` even while the agent
            // is still launching, so waiting here would only run out the
            // bound. The machine sends the prompt once the block clears
            // (`Task::prompt_pending`).
            return Ok((DispatchOutcome::Blocked, None));
        }
        // herdr 0.9.1 reports readiness with two flags, the same ones its own
        // `agent start --wait` reads: `launch_pending` while the process is
        // coming up, `interactive_ready` once it accepts input. A listed agent
        // with neither, not blocked (handled above) and not working, is a
        // pane whose process already exited: prompting it answers
        // `agent_not_ready` forever.
        let can_prompt = agent.interactive_ready || agent.agent_status == AgentStatus::Working;
        if agent.launch_pending {
            // still launching: fall through to the wait below
        } else if can_prompt {
            if !hold.is_empty() {
                // A pane that cannot be read is prompted as before.
                let screen = conn.agent_read(name, 100).await.unwrap_or_default();
                if crate::config::shows_startup_question(&screen, hold) {
                    return Ok((DispatchOutcome::Blocked, None));
                }
            }
            if let Some(inbox) = inbox
                && inbox.send(conn, &task.display_id(), prompt).await
            {
                let agent = agent.clone();
                let prompted = Prompted {
                    agent,
                    activity: false,
                };
                return Ok((DispatchOutcome::Running, Some(prompted)));
            }
            match conn.agent_prompt(name, prompt).await {
                // The reply carries the agent's `state_change_seq` and status
                // as the prompt went in.
                Ok(agent) => {
                    let activity = agent.agent_status.is_activity();
                    return Ok((DispatchOutcome::Running, Some(Prompted { agent, activity })));
                }
                // The agent is up and waiting for a human, not for us. herdr
                // did not send the prompt; the machine sends it after the block.
                Err(err) if err.code() == Some("agent_blocked") => {
                    return Ok((DispatchOutcome::Blocked, None));
                }
                // Raced the flag: herdr still refuses. Keep waiting inside the bound.
                Err(err) if err.code() == Some("agent_not_ready") => {}
                Err(err) => return Err(err.into()),
            }
        } else {
            return Err(exited(
                conn,
                task,
                format!(
                    "agent {name} exited before becoming interactive (is `{}` installed on {machine}?)",
                    task.spec.agent
                ),
            )
            .await);
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(DispatchError::Task(format!(
                "agent {name} not ready after {ready_timeout:?}; \
                 a live agent {name} may remain on {machine}; close it in herdr"
            )));
        }
        tokio::time::sleep(READY_POLL.min(deadline - now)).await;
    }
}

/// How many lines of an agent's pane `exited` keeps.
const EXIT_TAIL_LINES: u32 = 30;

/// `DispatchError::Exited` with `message` and the end of `task`'s pane,
/// read by its pane id since the agent is gone. A pane that cannot be read
/// leaves the tail empty: the task fails as it would have anyway.
async fn exited(conn: &dyn Connector, task: &Task, message: String) -> DispatchError {
    let tail = match task.pane_id.as_deref() {
        Some(pane) => conn
            .agent_read(pane, EXIT_TAIL_LINES)
            .await
            .unwrap_or_default(),
        None => String::new(),
    };
    DispatchError::Exited { message, tail }
}

#[cfg(all(test, feature = "fake-herdr"))]
mod tests {
    use super::*;
    use crate::config::flock::FlockNumber;
    use crate::herdr::AgentStatus;
    use crate::herdr::fake::{FakeHerdr, StartBehaviour};
    use crate::task::WorkspaceLabel;
    use serde_json::Value;
    use std::sync::Arc;

    /// Generous next to every `ready_after` these tests use, so only the test
    /// that means to hit the bound does.
    const READY: Duration = Duration::from_secs(5);

    fn mv(name: &str, max: u32, live: usize, tags: &[&str], healthy: bool) -> MachineView {
        MachineView {
            name: name.into(),
            max_agents: max,
            job_slots: 0,
            burst: 0,
            tags: tags.iter().map(|s| s.to_string()).collect(),
            loads: vec![],
            live,
            live_jobs: 0,
            healthy,
            pull: false,
            workdir_sets: true,
            flocks: vec![seat("default", None, live)],
            waiting_under_share: vec![],
            floor: Default::default(),
        }
    }

    fn seat(name: &str, max: Option<u32>, live: usize) -> FlockSeat {
        FlockSeat {
            name: name.into(),
            share: None,
            max,
            live,
        }
    }

    /// One more live task of `flock`, counted on `live`, on `live_jobs`
    /// when the claim comes from a job, and on that flock's own seat; for
    /// a flock the machine is not in, `live` still counts it and no seat
    /// changes.
    #[test]
    fn take_counts_the_task_on_live_and_its_seat() {
        let mut m = MachineView {
            flocks: vec![seat("work", Some(4), 0), seat("home", Some(4), 0)],
            ..mv("desk", 4, 0, &[], true)
        };
        m.take("work", RUN);
        assert_eq!(m.live, 1);
        assert_eq!(m.live_jobs, 0, "not from a job");
        assert_eq!(m.seat("work").unwrap().live, 1);
        assert_eq!(
            m.seat("home").unwrap().live,
            0,
            "the other flock is untouched"
        );

        m.take("work", JOB);
        assert_eq!(m.live, 2);
        assert_eq!(m.live_jobs, 1, "a job claim counts here too");
        assert_eq!(m.seat("work").unwrap().live, 2);

        m.take("play", RUN);
        assert_eq!(m.live, 3, "live counts every task, seated or not");
        assert_eq!(
            m.seat("play"),
            None,
            "no seat for a flock not on the machine"
        );
    }

    /// `later`'s tasks join `waiting_under_share` only for a machine where
    /// `flock` is past its share and under its max, and only when each is
    /// itself queued, of a different flock under its own share here, with
    /// no pin elsewhere, no tag the machine lacks, room for its claim,
    /// and `accepts` it; a flock already in the list does not join twice.
    #[test]
    fn mark_waiting_under_share_filters_each_later_task() {
        let work = |live: usize| FlockSeat::new("work", Some(FlockNumber::split(1, 3)), live);
        let desk = |work_live: usize, home_live: usize| MachineView {
            flocks: vec![work(work_live), seat("home", Some(2), home_live)],
            tags: vec!["arm".into()],
            ..mv("desk", 10, work_live + home_live, &[], true)
        };
        let home_task = |over: fn(&mut Task)| {
            let mut t = Task {
                flock: Some("home".into()),
                ..task(spec())
            };
            over(&mut t);
            t
        };
        let accepts_all = |_: &Task, _: &str| true;
        let run =
            |views: &mut [MachineView], later: &[Task], accepts: &dyn Fn(&Task, &str) -> bool| {
                mark_waiting_under_share(views, "work", later, "default", accepts);
            };

        // Under its share, no waiting is computed even for a task that
        // would otherwise qualify.
        let mut views = vec![desk(0, 1)];
        run(&mut views, &[home_task(|_| {})], &accepts_all);
        assert!(views[0].waiting_under_share.is_empty(), "under its share");

        // At its max, none either.
        let mut views = vec![desk(3, 1)];
        run(&mut views, &[home_task(|_| {})], &accepts_all);
        assert!(views[0].waiting_under_share.is_empty(), "at its max");

        // Past its share, under its max: a plain queued task of another
        // flock under its own share here joins the list.
        let mut views = vec![desk(1, 1)];
        run(&mut views, &[home_task(|_| {})], &accepts_all);
        assert_eq!(views[0].waiting_under_share, vec!["home".to_string()]);

        // The flock being placed never joins, whatever else is true.
        let mut views = vec![desk(1, 1)];
        let same = Task {
            flock: Some("work".into()),
            ..task(spec())
        };
        run(&mut views, &[same], &accepts_all);
        assert!(views[0].waiting_under_share.is_empty(), "its own flock");

        // A flock already in the list does not join twice.
        let mut views = vec![desk(1, 1)];
        let twice = [home_task(|_| {}), home_task(|_| {})];
        run(&mut views, &twice, &accepts_all);
        assert_eq!(views[0].waiting_under_share, vec!["home".to_string()]);

        // Not queued.
        let mut views = vec![desk(1, 1)];
        let not_queued = home_task(|t| t.state = TaskState::Running);
        run(&mut views, &[not_queued], &accepts_all);
        assert!(views[0].waiting_under_share.is_empty(), "not queued");

        // Its own seat is not under share.
        let mut views = vec![desk(1, 2)];
        run(&mut views, &[home_task(|_| {})], &accepts_all);
        assert!(
            views[0].waiting_under_share.is_empty(),
            "home is at its share"
        );

        // Pinned to another machine.
        let mut views = vec![desk(1, 1)];
        let pinned = home_task(|t| t.spec.machine = Some("elsewhere".into()));
        run(&mut views, &[pinned], &accepts_all);
        assert!(views[0].waiting_under_share.is_empty(), "pinned elsewhere");

        // Asks for a tag the machine lacks.
        let mut views = vec![desk(1, 1)];
        let tagged = home_task(|t| t.spec.tags = vec!["gpu".into()]);
        run(&mut views, &[tagged], &accepts_all);
        assert!(
            views[0].waiting_under_share.is_empty(),
            "tag the machine lacks"
        );

        // No room for its claim: the machine's own limit is already met.
        let full = MachineView {
            max_agents: 2,
            ..desk(1, 1)
        };
        let mut views = vec![full];
        run(&mut views, &[home_task(|_| {})], &accepts_all);
        assert!(
            views[0].waiting_under_share.is_empty(),
            "the machine has no room"
        );

        // `accepts` refuses it.
        let mut views = vec![desk(1, 1)];
        run(&mut views, &[home_task(|_| {})], &|_, _| false);
        assert!(
            views[0].waiting_under_share.is_empty(),
            "accepts refused it"
        );
    }

    /// A later task of a flock the room floor holds off never joins
    /// `waiting_under_share`: mixed `unknown_room` policies (`work`'s room
    /// counts as room here, `home`'s does not) must not leave both flocks
    /// queued across passes when the machine's room is simply unread.
    #[test]
    fn mark_waiting_under_share_excludes_a_floor_held_later_task() {
        let work = |live: usize| FlockSeat::new("work", Some(FlockNumber::split(1, 3)), live);
        let desk = |work_live: usize, home_live: usize| MachineView {
            flocks: vec![work(work_live), seat("home", Some(2), home_live)],
            floor: FloorView {
                short: None,
                unread: true,
                hold_unread: vec!["home".to_string()],
                unreachable: false,
                cordoned: false,
            },
            ..mv("desk", 10, work_live + home_live, &[], true)
        };
        let home_task = Task {
            flock: Some("home".into()),
            ..task(spec())
        };
        let mut views = vec![desk(1, 1)];
        mark_waiting_under_share(&mut views, "work", &[home_task], "default", &|_, _| true);
        assert!(
            views[0].waiting_under_share.is_empty(),
            "home's unread room holds it off, so it never waits under work's share"
        );
    }

    /// `flock_held` clears a seat under its share on its own, whatever
    /// `waiting_under_share` says: that list only matters to a seat past
    /// its share (`mark_waiting_under_share` never fills it otherwise, but
    /// `flock_held` reads it as given).
    #[test]
    fn flock_held_clears_a_seat_under_share_even_with_others_waiting() {
        let v = MachineView {
            flocks: vec![seat("work", Some(4), 1)],
            waiting_under_share: vec!["home".to_string()],
            ..mv("desk", 4, 1, &[], true)
        };
        assert_eq!(
            flock_held(&v, "work"),
            None,
            "under its share holds nothing off"
        );
    }

    /// A flock at 0 on a machine starts nothing there, pinned, `--now` or
    /// not, however idle the machine; the reason says it has no slots, even
    /// with the machine full; the flock's other machines and other flocks
    /// still take tasks.
    #[test]
    fn a_flock_at_0_starts_nothing_on_the_machine() {
        let drained = MachineView {
            flocks: vec![seat("work", Some(0), 1), seat("home", Some(2), 0)],
            ..mv("desk", 4, 1, &[], true)
        };
        let lab = MachineView {
            flocks: vec![seat("work", Some(2), 1)],
            ..mv("lab", 2, 1, &[], true)
        };
        let ms = vec![drained.clone(), lab];
        let pinned = DispatchSpec {
            machine: Some("desk".into()),
            ..spec()
        };
        assert_eq!(
            pick_machine(&ms[..1], "work", &spec(), Claim::default()),
            None
        );
        assert_eq!(pick_machine(&ms, "work", &pinned, Claim::default()), None);
        assert_eq!(
            pick_machine(&ms, "work", &spec(), Claim::default()).as_deref(),
            Some("lab")
        );
        assert_eq!(
            pick_machine(&ms, "home", &pinned, Claim::default()).as_deref(),
            Some("desk")
        );
        let now = DispatchSpec {
            now: true,
            ..pinned.clone()
        };
        assert_eq!(now_machine(&ms, "work", &now, &|_| true), None);
        assert_eq!(
            flock_held(&drained, "work").as_deref(),
            Some("flock work has 0 slots on desk: it starts no new tasks there (1 still running)")
        );
    }

    /// Only the task's flock takes it, whatever the others have free.
    #[test]
    fn pick_machine_keeps_to_the_tasks_flock() {
        let ms = vec![
            mv("home-1", 2, 1, &[], true),
            MachineView {
                flocks: vec![seat("work", None, 0)],
                ..mv("work-1", 2, 0, &[], true)
            },
        ];
        assert_eq!(
            pick_machine(&ms, "default", &spec(), Claim::default()).as_deref(),
            Some("home-1")
        );
        assert_eq!(
            pick_machine(&ms, "work", &spec(), Claim::default()).as_deref(),
            Some("work-1")
        );
        assert_eq!(pick_machine(&ms, "play", &spec(), Claim::default()), None);
        let pinned_elsewhere = DispatchSpec {
            machine: Some("work-1".into()),
            ..spec()
        };
        assert_eq!(
            pick_machine(&ms, "default", &pinned_elsewhere, Claim::default()),
            None,
            "a pinned machine that moved to another flock takes nothing"
        );
    }

    /// A machine in two flocks takes a flock's task only while that flock is
    /// under its number there, whatever room the machine has; the other
    /// flock is not held back by it.
    #[test]
    fn pick_machine_keeps_a_flock_to_its_number() {
        let desk = MachineView {
            flocks: vec![seat("home", Some(3), 1), seat("work", Some(1), 1)],
            ..mv("desk", 4, 2, &[], true)
        };
        let ms = vec![desk.clone()];
        assert_eq!(pick_machine(&ms, "work", &spec(), Claim::default()), None);
        assert_eq!(
            pick_machine(&ms, "home", &spec(), Claim::default()).as_deref(),
            Some("desk")
        );
        let pinned = DispatchSpec {
            machine: Some("desk".into()),
            ..spec()
        };
        assert_eq!(pick_machine(&ms, "work", &pinned, Claim::default()), None);
        // Job slots and burst never pass the flock's number.
        let slack = vec![MachineView {
            job_slots: 2,
            burst: 2,
            ..desk.clone()
        }];
        for claim in [JOB, CRITICAL_RUN, CRITICAL_JOB] {
            assert_eq!(pick_machine(&slack, "work", &spec(), claim), None);
        }
        // Under its number, the flock goes to whichever member has room.
        let other = MachineView {
            flocks: vec![seat("work", Some(2), 0)],
            ..mv("lab", 2, 1, &[], true)
        };
        assert_eq!(
            pick_machine(&[desk, other], "work", &spec(), Claim::default()).as_deref(),
            Some("lab")
        );
        // With no number of its own, a flock has the machine's limits.
        let old = vec![MachineView {
            job_slots: 1,
            ..mv("old", 1, 1, &[], true)
        }];
        assert_eq!(
            pick_machine(&old, "default", &spec(), JOB).as_deref(),
            Some("old")
        );
    }

    /// A flock with a share and a max: under its share it takes a slot as
    /// usual; past it, only while no flock under its share waits there; at
    /// its max, never. The machine's own room holds all of them.
    #[test]
    fn pick_machine_lets_a_flock_past_its_share_only_on_an_idle_machine() {
        let split = |live| FlockSeat::new("work", Some(FlockNumber::split(1, 3)), live);
        let desk = |work_live: usize, waiting: &[&str]| MachineView {
            flocks: vec![seat("home", Some(2), 0), split(work_live)],
            waiting_under_share: waiting.iter().map(|w| w.to_string()).collect(),
            ..mv("desk", 4, work_live, &[], true)
        };
        let pick = |m: MachineView| pick_machine(&[m], "work", &spec(), Claim::default());
        // Under its share, a waiting task of another flock does not stop it.
        assert_eq!(pick(desk(0, &["home"])).as_deref(), Some("desk"));
        // Past its share on an idle machine it takes the slot.
        assert_eq!(pick(desk(1, &[])).as_deref(), Some("desk"));
        assert_eq!(pick(desk(2, &[])).as_deref(), Some("desk"));
        // Past its share while home waits under its share, it does not.
        assert_eq!(pick(desk(1, &["home"])), None);
        // Held at its max, however idle the machine.
        assert_eq!(pick(desk(3, &[])), None);
        // The machine's own room caps everything.
        let full = MachineView {
            max_agents: 2,
            ..desk(2, &[])
        };
        assert_eq!(pick(full), None);
        // Job slots and burst never pass the max.
        let slack = MachineView {
            job_slots: 2,
            burst: 2,
            ..desk(3, &[])
        };
        for claim in [JOB, CRITICAL_RUN, CRITICAL_JOB] {
            assert_eq!(
                pick_machine(std::slice::from_ref(&slack), "work", &spec(), claim),
                None
            );
        }
        // The plain number stays a hard ceiling.
        let plain = MachineView {
            flocks: vec![seat("work", Some(1), 1)],
            ..mv("desk", 4, 1, &[], true)
        };
        assert_eq!(pick(plain), None);
    }

    fn spec() -> DispatchSpec {
        DispatchSpec {
            now: false,
            agent: "claude".into(),
            agent_args: vec!["--model".into(), "opus".into()],
            allow: vec![],
            deny: vec![],
            repo: Some("/srv/app".into()),
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

    /// herdr opens a workspace whose cwd is missing in the shell's home
    /// instead, silently (t-5 on 2026-09-24). A repo the machine says is not
    /// a directory fails the task before anything is created.
    #[tokio::test]
    async fn a_repo_missing_on_the_machine_fails_before_herdr_is_asked() {
        for worktree in [false, true] {
            let fake = FakeHerdr::new();
            fake.set_missing_dir("/srv/app");
            let mut t = task(DispatchSpec { worktree, ..spec() });
            let err = dispatch(&fake, &mut t, &Agents::default(), None, READY)
                .await
                .unwrap_err();
            assert!(!err.is_transport(), "the machine is fine: {err}");
            assert_eq!(t.state, TaskState::Failed);
            assert_eq!(
                t.error.as_deref(),
                Some("repo /srv/app does not exist on pi-1")
            );
            assert!(
                !fake
                    .requests()
                    .iter()
                    .any(|r| r.method.ends_with(".create")),
                "nothing was created"
            );
            assert_eq!(t.spec.session_id, None, "no session to resume");
        }
    }

    /// The check runs on the expanded path, so `~/x` is looked up as
    /// `/home/fake/x`.
    #[tokio::test]
    async fn the_repo_check_sees_the_expanded_path() {
        let fake = FakeHerdr::new();
        fake.set_missing_dir("/home/fake/gone");
        let mut t = task(DispatchSpec {
            repo: Some("~/gone".into()),
            ..spec()
        });
        let err = dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("/home/fake/gone does not exist"),
            "{err}"
        );
    }

    /// `args` with the `--session-id` dispatch gave `t` after them.
    fn with_session(t: &Task, args: Value) -> Value {
        let mut args = args.as_array().unwrap().clone();
        let id = t
            .spec
            .session_id
            .clone()
            .expect("a claude task records a session");
        args.extend([Value::from("--session-id"), Value::from(id)]);
        Value::Array(args)
    }

    fn task(spec: DispatchSpec) -> Task {
        let now = Utc::now();
        Task {
            description: None,
            id: 7,
            job: "run".into(),
            item: Value::Null,
            prompt: "line one\n\"two\" {{ three }}".into(),
            spec,
            machine: Some("pi-1".into()),
            workspace_id: None,
            pane_id: None,
            agent_name: None,
            state: TaskState::Queued,
            error: None,
            last_completion_seq: None,
            prompt_pending: false,
            activity_seen: false,
            ended: false,
            retry_of: None,
            priority: Default::default(),
            priority_from: None,
            queue_pos: 0,
            aged_from: None,
            aged_at: None,
            pause: Default::default(),
            waiting_until: None,
            usage: None,
            summary: None,
            created_at: now,
            started_at: None,
            finished_at: None,
            updated_at: now,
            flock: Some("home".into()),
            role: Default::default(),
        }
    }

    #[test]
    fn pick_machine_respects_capacity() {
        let ms = vec![
            mv("a", 1, 1, &[], true),
            mv("b", 2, 1, &[], true),
            mv("c", 2, 0, &[], true),
        ];
        assert_eq!(
            pick_machine(&ms, "default", &spec(), Claim::default()).as_deref(),
            Some("c")
        );
        let full = vec![mv("a", 1, 1, &[], true)];
        assert_eq!(
            pick_machine(&full, "default", &spec(), Claim::default()),
            None
        );
    }

    /// A cordoned machine takes no task, whatever the claim, a pinned one
    /// included, and the reason names it; unlike the room floor, `--now`
    /// does not look past it.
    #[test]
    fn pick_machine_skips_a_cordoned_machine() {
        let cordoned = MachineView {
            job_slots: 1,
            burst: 1,
            floor: FloorView {
                cordoned: true,
                ..Default::default()
            },
            ..mv("a", 2, 0, &[], true)
        };
        for claim in [RUN, JOB, CRITICAL_RUN, CRITICAL_JOB] {
            assert_eq!(
                pick_machine(std::slice::from_ref(&cordoned), "default", &spec(), claim),
                None,
                "{claim:?}"
            );
        }
        assert_eq!(
            cordoned.floor_held("default").as_deref(),
            Some("a is cordoned")
        );
        let ms = vec![cordoned.clone(), mv("b", 2, 1, &[], true)];
        assert_eq!(
            pick_machine(&ms, "default", &spec(), RUN).as_deref(),
            Some("b"),
            "the other machine, though it is busier"
        );
        let pinned = DispatchSpec {
            machine: Some("a".into()),
            ..spec()
        };
        assert_eq!(pick_machine(&ms, "default", &pinned, RUN), None);
        let now = DispatchSpec {
            now: true,
            ..pinned
        };
        assert_eq!(now_machine(&ms, "default", &now, &|_| true), None);
        // Uncordoned, it is placed again.
        let lifted = MachineView {
            floor: FloorView::default(),
            ..cordoned
        };
        assert_eq!(
            pick_machine(std::slice::from_ref(&lifted), "default", &spec(), RUN).as_deref(),
            Some("a")
        );
        assert_eq!(
            now_machine(&[lifted], "default", &now, &|_| true).as_deref(),
            Some("a")
        );
    }

    /// A machine its last check found cannot reach the head is passed over
    /// for every claim, and the reason names it; `--now` looks past it.
    #[test]
    fn pick_machine_skips_a_machine_that_cannot_reach_the_head() {
        let cut_off = MachineView {
            job_slots: 1,
            burst: 1,
            floor: FloorView {
                unreachable: true,
                ..Default::default()
            },
            ..mv("a", 2, 0, &[], true)
        };
        for claim in [RUN, JOB, CRITICAL_RUN, CRITICAL_JOB] {
            assert_eq!(
                pick_machine(std::slice::from_ref(&cut_off), "default", &spec(), claim),
                None,
                "{claim:?}"
            );
        }
        assert_eq!(
            cut_off.floor_held("default").as_deref(),
            Some("a cannot reach the head")
        );
        let ms = vec![cut_off.clone(), mv("b", 2, 1, &[], true)];
        assert_eq!(
            pick_machine(&ms, "default", &spec(), RUN).as_deref(),
            Some("b")
        );
        let pinned = DispatchSpec {
            machine: Some("a".into()),
            now: true,
            ..spec()
        };
        assert_eq!(
            now_machine(&ms, "default", &pinned, &|_| true).as_deref(),
            Some("a")
        );
    }

    const GIB: u64 = 1024 * 1024 * 1024;

    /// A room as the probe reads it, in KiB.
    fn room_kib(home: Option<u64>, tmp: Option<u64>, mem: Option<u64>) -> crate::machine::Room {
        crate::machine::Room {
            home_free: home,
            tmp_free: tmp,
            mem_free: mem,
            ..Default::default()
        }
    }

    fn floor(disk: Option<u64>, memory: Option<u64>) -> Option<crate::config::Floor> {
        Some(crate::config::Floor { disk, memory })
    }

    /// The first side under its floor is named, `$HOME` before `/tmp`
    /// before memory, with the room converted from KiB; a side that is on
    /// and cannot be read is unread; a side that is off is never read;
    /// with no floor nothing is short or unread, whatever the room.
    #[test]
    fn floor_against_names_the_first_side_short_and_what_is_unread() {
        let kib = |gib: f64| Some((gib * 1024.0 * 1024.0) as u64);
        let on = floor(Some(4 * GIB), Some(GIB));
        let plenty = room_kib(kib(50.0), kib(10.0), kib(8.0));
        assert_eq!(floor_against("m", Some(&plenty), on), (None, false));
        assert_eq!(
            floor_against("sauron", Some(&room_kib(kib(50.0), kib(1.2), kib(8.0))), on),
            (Some("sauron has 1.2 GB free under /tmp".into()), false)
        );
        assert_eq!(
            floor_against("m", Some(&room_kib(kib(2.0), kib(1.0), kib(0.5))), on),
            (Some("m has 2.0 GB free under $HOME".into()), false),
            "$HOME first"
        );
        assert_eq!(
            floor_against("m", Some(&room_kib(kib(50.0), kib(10.0), kib(0.5))), on),
            (Some("m has 512 MB of memory free".into()), false)
        );
        // Memory on macOS: unread while the memory floor is on, not once
        // it is off.
        let mac = room_kib(kib(50.0), kib(10.0), None);
        assert_eq!(floor_against("m", Some(&mac), on), (None, true));
        assert_eq!(
            floor_against("m", Some(&mac), floor(Some(4 * GIB), None)),
            (None, false)
        );
        // No numbers yet, or a probe that failed before any answered.
        assert_eq!(floor_against("m", None, on), (None, true));
        let failed = crate::machine::Room {
            error: Some("ssh: connection reset".into()),
            ..Default::default()
        };
        assert_eq!(floor_against("m", Some(&failed), on), (None, true));
        // A failed probe over numbers last read keeps them.
        let stale = crate::machine::Room {
            error: Some("timed out".into()),
            ..room_kib(kib(50.0), kib(1.0), kib(8.0))
        };
        assert_eq!(
            floor_against("m", Some(&stale), on).0.as_deref(),
            Some("m has 1.0 GB free under /tmp")
        );
        // Short on one side and unread on another reads both.
        assert_eq!(
            floor_against("m", Some(&room_kib(kib(1.0), None, kib(8.0))), on),
            (Some("m has 1.0 GB free under $HOME".into()), true)
        );
        // No floor: never short.
        let empty = room_kib(Some(0), Some(0), Some(0));
        assert_eq!(floor_against("m", Some(&empty), None), (None, false));
        assert_eq!(floor_against("m", None, None), (None, false));
    }

    /// A machine under its floor takes no task, whatever the claim: plain,
    /// job slot or burst. An unread side counts as room unless the task's
    /// flock holds it (`unknown_room = "no_room"`). `--now` looks at none
    /// of it.
    #[test]
    fn pick_machine_skips_a_machine_held_by_its_floor() {
        let short = MachineView {
            job_slots: 1,
            burst: 1,
            floor: FloorView {
                short: Some("a has 1.0 GB free under /tmp".into()),
                ..Default::default()
            },
            ..mv("a", 2, 0, &[], true)
        };
        for claim in [RUN, JOB, CRITICAL_RUN, CRITICAL_JOB] {
            assert_eq!(
                pick_machine(std::slice::from_ref(&short), "default", &spec(), claim),
                None,
                "{claim:?}"
            );
        }
        let pinned = DispatchSpec {
            machine: Some("a".into()),
            ..spec()
        };
        assert_eq!(
            pick_machine(std::slice::from_ref(&short), "default", &pinned, RUN),
            None
        );
        let ms = vec![short.clone(), mv("b", 2, 1, &[], true)];
        assert_eq!(
            pick_machine(&ms, "default", &spec(), RUN).as_deref(),
            Some("b"),
            "the busier machine with room"
        );
        assert_eq!(
            short.floor_held("default").as_deref(),
            Some("a has 1.0 GB free under /tmp")
        );

        let unread = |hold: &[&str]| MachineView {
            floor: FloorView {
                unread: true,
                hold_unread: hold.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
            ..mv("a", 2, 0, &[], true)
        };
        assert_eq!(
            pick_machine(&[unread(&[])], "default", &spec(), RUN).as_deref(),
            Some("a"),
            "an unread room counts as room by default"
        );
        assert_eq!(
            pick_machine(&[unread(&["default"])], "default", &spec(), RUN),
            None
        );
        assert_eq!(
            unread(&["default"]).floor_held("default").as_deref(),
            Some("a's room cannot be read")
        );
        assert_eq!(
            unread(&["work"]).floor_held("default"),
            None,
            "another flock's hold"
        );

        let now = DispatchSpec {
            now: true,
            ..pinned
        };
        assert_eq!(
            now_machine(&[short], "default", &now, &|_| true).as_deref(),
            Some("a")
        );
        assert_eq!(
            now_machine(&[unread(&["default"])], "default", &now, &|_| true).as_deref(),
            Some("a")
        );
    }

    /// `mv` with job slots and burst, and how many of `live` are job tasks.
    fn slots(max: u32, job_slots: u32, burst: u32, live: usize, live_jobs: usize) -> MachineView {
        MachineView {
            job_slots,
            burst,
            live_jobs,
            ..mv("m", max, live, &[], true)
        }
    }

    const RUN: Claim = Claim {
        from_job: false,
        critical: false,
    };
    const JOB: Claim = Claim {
        from_job: true,
        critical: false,
    };
    const CRITICAL_RUN: Claim = Claim {
        from_job: false,
        critical: true,
    };
    const CRITICAL_JOB: Claim = Claim {
        from_job: true,
        critical: true,
    };

    #[test]
    fn job_slots_alone_are_for_job_tasks() {
        // Two `task run` tasks fill max_agents = 2; the job slot is free.
        let m = slots(2, 1, 0, 2, 0);
        assert!(m.has_room(JOB), "a job task takes the free job slot");
        assert!(
            !m.has_room(RUN),
            "a normal task is still held at max_agents"
        );
        assert!(!m.has_room(CRITICAL_RUN), "no burst");
        // The job slot taken, a second job task finds no room.
        assert!(!slots(2, 1, 0, 3, 1).has_room(JOB));
        // A job task in the job slot leaves the shared slots to others.
        assert!(slots(2, 1, 0, 2, 1).has_room(RUN));
        // Job tasks past the job slots count against max_agents.
        assert!(!slots(2, 1, 0, 3, 3).has_room(RUN));
        assert!(!slots(2, 1, 0, 3, 3).has_room(JOB));
        assert!(slots(2, 1, 0, 2, 2).has_room(JOB), "a shared slot is free");
    }

    #[test]
    fn burst_alone_is_for_critical_tasks() {
        let m = slots(2, 0, 1, 2, 0);
        assert!(
            m.has_room(CRITICAL_RUN),
            "critical goes one past max_agents"
        );
        assert!(
            !m.has_room(RUN),
            "a normal task is still held at max_agents"
        );
        assert!(!m.has_room(JOB), "no job slots");
        assert!(m.has_room(CRITICAL_JOB), "a critical job task bursts too");
        assert!(
            !slots(2, 0, 1, 3, 0).has_room(CRITICAL_RUN),
            "burst used up"
        );
        assert!(slots(2, 0, 2, 3, 0).has_room(CRITICAL_RUN));
    }

    #[test]
    fn job_slots_and_burst_together() {
        // Shared slots full, job slot free, burst free.
        let m = slots(2, 1, 1, 2, 0);
        assert!(m.has_room(JOB));
        assert!(m.has_room(CRITICAL_RUN));
        assert!(!m.has_room(RUN));
        // A job task in its slot does not use up the burst.
        let m = slots(2, 1, 1, 3, 1);
        assert!(m.has_room(CRITICAL_RUN), "burst counts outside job slots");
        assert!(!m.has_room(JOB), "job slot taken, shared slots full");
        assert!(m.has_room(CRITICAL_JOB), "then burst");
        // Everything taken.
        let m = slots(2, 1, 1, 4, 1);
        assert!(!m.has_room(CRITICAL_JOB));
        assert!(!m.has_room(CRITICAL_RUN));
    }

    #[test]
    fn zero_turns_job_slots_and_burst_off() {
        let m = slots(2, 0, 0, 2, 0);
        for claim in [RUN, JOB, CRITICAL_RUN, CRITICAL_JOB] {
            assert!(!m.has_room(claim), "{claim:?}");
        }
        let m = slots(2, 0, 0, 1, 1);
        for claim in [RUN, JOB, CRITICAL_RUN, CRITICAL_JOB] {
            assert!(m.has_room(claim), "{claim:?}");
        }
    }

    #[test]
    fn critical_job_task_takes_a_job_slot_first() {
        // Job slot free, shared slots free: the critical job task still
        // fits without burst, and once it runs it sits in the job slot, so
        // the shared slots stay open for a normal task.
        let before = slots(1, 1, 1, 0, 0);
        assert!(before.has_room(CRITICAL_JOB));
        let after = slots(1, 1, 1, 1, 1);
        assert!(after.has_room(RUN), "the shared slot is still free");
        assert!(after.has_room(CRITICAL_RUN));
        // Shared slot and job slot full: burst is the last step.
        let full = slots(1, 1, 1, 2, 1);
        assert!(!full.has_room(RUN));
        assert!(!full.has_room(JOB));
        assert!(full.has_room(CRITICAL_JOB));
    }

    #[test]
    fn pick_machine_takes_the_fewest_live_among_those_with_room() {
        let ms = vec![
            // Fewest live, but full for a normal task.
            MachineView {
                name: "a".into(),
                ..slots(1, 1, 0, 1, 0)
            },
            MachineView {
                name: "b".into(),
                ..slots(3, 0, 0, 2, 0)
            },
            MachineView {
                name: "c".into(),
                ..slots(3, 0, 0, 3, 0)
            },
        ];
        assert_eq!(
            pick_machine(&ms, "default", &spec(), RUN).as_deref(),
            Some("b")
        );
        assert_eq!(
            pick_machine(&ms, "default", &spec(), JOB).as_deref(),
            Some("a"),
            "a's job slot is room for a job task, and a has fewest live"
        );
    }

    /// `task run --now` takes its pinned machine whatever its count, even
    /// past `max_agents`, burst and the flock's number there, so long as it
    /// is healthy, has the task's tags and `accepts` it.
    #[test]
    fn now_machine_takes_the_pin_past_its_limits() {
        let full = MachineView {
            flocks: vec![seat("default", Some(1), 3)],
            ..mv("a", 2, 3, &["fast"], true)
        };
        let ms = vec![full, mv("b", 2, 0, &[], false)];
        let now = |machine: &str, tags: &[&str]| DispatchSpec {
            machine: Some(machine.into()),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            now: true,
            ..spec()
        };
        assert_eq!(
            pick_machine(&ms, "default", &now("a", &[]), Claim::default()),
            None,
            "the queue would wait"
        );
        assert_eq!(
            now_machine(&ms, "default", &now("a", &["fast"]), &|_| true).as_deref(),
            Some("a")
        );
        assert_eq!(
            now_machine(&ms, "default", &now("b", &[]), &|_| true),
            None,
            "unhealthy"
        );
        assert_eq!(
            now_machine(&ms, "default", &now("a", &["gpu"]), &|_| true),
            None,
            "tags"
        );
        assert_eq!(
            now_machine(&ms, "default", &now("a", &[]), &|_| false),
            None,
            "refused"
        );
        assert_eq!(
            now_machine(&ms, "default", &now("zzz", &[]), &|_| true),
            None
        );
        let unpinned = DispatchSpec {
            machine: None,
            ..now("a", &[])
        };
        assert_eq!(
            now_machine(&ms, "default", &unpinned, &|_| true),
            None,
            "needs a pin"
        );
    }

    /// A `--now` task still needs its pinned machine to be in its flock: a
    /// flock edit between `queue_run_as` (which checked membership before
    /// the row was inserted) and placement must not start it in a flock it
    /// no longer belongs to (GPT review on #103, finding 2).
    #[test]
    fn now_machine_requires_the_pin_still_be_in_the_flock() {
        let a = MachineView {
            flocks: vec![seat("work", Some(1), 3)],
            ..mv("a", 2, 3, &["fast"], true)
        };
        let ms = vec![a];
        let now = DispatchSpec {
            machine: Some("a".into()),
            now: true,
            ..spec()
        };
        assert_eq!(
            now_machine(&ms, "work", &now, &|_| true).as_deref(),
            Some("a"),
            "a is in work"
        );
        assert_eq!(
            now_machine(&ms, "default", &now, &|_| true),
            None,
            "a moved out of default, or was never in it"
        );
    }

    #[test]
    fn pick_machine_honours_pin_tags_and_health() {
        let ms = vec![
            mv("a", 2, 0, &["fast"], true),
            mv("b", 2, 0, &[], false),
            mv("c", 2, 0, &["fast", "gpu"], true),
        ];
        assert_eq!(
            pick_machine(
                &ms,
                "default",
                &DispatchSpec {
                    machine: Some("c".into()),
                    ..spec()
                },
                Claim::default(),
            )
            .as_deref(),
            Some("c")
        );
        assert_eq!(
            pick_machine(
                &ms,
                "default",
                &DispatchSpec {
                    machine: Some("b".into()),
                    ..spec()
                },
                Claim::default(),
            ),
            None,
            "pinned but unhealthy"
        );
        assert_eq!(
            pick_machine(
                &ms,
                "default",
                &DispatchSpec {
                    machine: Some("zzz".into()),
                    ..spec()
                },
                Claim::default(),
            ),
            None,
            "pinned but unknown"
        );
        assert_eq!(
            pick_machine(
                &ms,
                "default",
                &DispatchSpec {
                    tags: vec!["gpu".into()],
                    ..spec()
                },
                Claim::default(),
            )
            .as_deref(),
            Some("c")
        );
        assert_eq!(
            pick_machine(
                &ms,
                "default",
                &DispatchSpec {
                    tags: vec!["fast".into()],
                    ..spec()
                },
                Claim::default(),
            )
            .as_deref(),
            Some("a"),
            "flock order breaks ties"
        );
        assert_eq!(
            pick_machine(
                &ms,
                "default",
                &DispatchSpec {
                    tags: vec!["nope".into()],
                    ..spec()
                },
                Claim::default(),
            ),
            None
        );
    }

    /// A task with a load goes only to a machine that takes it, pinned,
    /// loose or `--now`; one with no load goes to any machine, one that
    /// takes loads too.
    #[test]
    fn pick_machine_keeps_a_load_to_the_machines_that_take_it() {
        let takes = |name: &str, loads: &[&str]| MachineView {
            loads: loads.iter().map(|l| l.to_string()).collect(),
            ..mv(name, 2, 0, &[], true)
        };
        // `a` has fewer live tasks, so a task free to go anywhere picks it.
        let ms = vec![
            takes("a", &[]),
            MachineView {
                live: 1,
                ..takes("b", &["rust", "light"])
            },
        ];
        let load = |load: Option<&str>, machine: Option<&str>| DispatchSpec {
            load: load.map(str::to_string),
            machine: machine.map(str::to_string),
            ..spec()
        };
        let pick = |s: &DispatchSpec| pick_machine(&ms, "default", s, Claim::default());
        assert_eq!(pick(&load(Some("rust"), None)).as_deref(), Some("b"));
        assert_eq!(pick(&load(Some("light"), None)).as_deref(), Some("b"));
        assert_eq!(pick(&load(Some("gpu"), None)), None, "nothing takes it");
        assert_eq!(
            pick(&load(None, None)).as_deref(),
            Some("a"),
            "no load: fewest live, as before"
        );
        assert_eq!(
            pick(&load(Some("rust"), Some("a"))),
            None,
            "pinned elsewhere"
        );
        assert_eq!(pick(&load(Some("rust"), Some("b"))).as_deref(), Some("b"));
        let now = |machine: &str| DispatchSpec {
            now: true,
            ..load(Some("rust"), Some(machine))
        };
        assert_eq!(now_machine(&ms, "default", &now("a"), &|_| true), None);
        assert_eq!(
            now_machine(&ms, "default", &now("b"), &|_| true).as_deref(),
            Some("b")
        );
    }

    #[tokio::test]
    async fn dispatch_sends_prompt_verbatim() {
        let fake = FakeHerdr::new();
        let mut t = task(spec());
        let out = dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        assert_eq!(out, DispatchOutcome::Running);
        assert_eq!(t.state, TaskState::Running);
        assert_eq!(t.agent_name.as_deref(), Some("t-7"));
        assert_eq!(t.pane_id.as_deref(), Some("w1:p1"));
        assert_eq!(t.workspace_id.as_deref(), Some("w1"));
        assert!(t.started_at.is_some());
        let reqs = fake.requests();
        let ws = reqs
            .iter()
            .find(|r| r.method == "workspace.create")
            .unwrap();
        assert_eq!(ws.params["cwd"], "/srv/app");
        assert_eq!(ws.params["label"], "home/t-7");
        let start = reqs.iter().find(|r| r.method == "agent.start").unwrap();
        assert_eq!(start.params["kind"], "claude");
        assert_eq!(
            start.params["args"],
            with_session(
                &t,
                serde_json::json!([
                    "--model",
                    "opus",
                    "--allowedTools",
                    "Bash(pastor task done:*)"
                ])
            )
        );
        let prompt = reqs.iter().find(|r| r.method == "agent.prompt").unwrap();
        assert_eq!(prompt.params["target"], "t-7");
        // The prompt as stored, then the line asking for a summary.
        assert_eq!(
            prompt.params["text"],
            format!(
                "line one\n\"two\" {{{{ three }}}}\n\n{}",
                crate::task::SUMMARY_ASK
            )
        );
        assert_eq!(t.prompt, "line one\n\"two\" {{ three }}", "not stored");
    }

    /// A Claude task's prompt goes through its session's inbox socket when
    /// the session takes it, and is not typed. The baseline is the
    /// sequence `agent.list` showed just before the send, and the agent
    /// was not seen at work yet: its own move to working is.
    #[tokio::test]
    async fn a_claude_prompt_goes_through_the_inbox_socket() {
        let fake = FakeHerdr::new();
        fake.set_inbox_result(Some("accepted"));
        let mut t = task(spec());
        let out = dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        assert_eq!(out, DispatchOutcome::Running);
        let reqs = fake.requests();
        assert!(!reqs.iter().any(|r| r.method == "agent.prompt"), "typed");
        let sends = fake.inbox_sends();
        assert_eq!(sends.len(), 1);
        assert_eq!(
            Some(sends[0].session.as_str()),
            t.spec.session_id.as_deref()
        );
        assert_eq!(sends[0].mode, crate::inbox::Mode::Prompting);
        assert_eq!(sends[0].config_dir, None);
        assert_eq!(
            sends[0].text,
            format!(
                "line one\n\"two\" {{{{ three }}}}\n\n{}",
                crate::task::SUMMARY_ASK
            )
        );
        let agent = &fake.agents()[0];
        assert_eq!(agent.agent_status, AgentStatus::Working);
        assert!(
            t.last_completion_seq < Some(agent.state_change_seq),
            "the baseline is from before the send: {:?} vs {}",
            t.last_completion_seq,
            agent.state_change_seq
        );
        assert!(!t.activity_seen);
    }

    /// Anything but `accepted`, and a machine that cannot send that way,
    /// types the prompt as before.
    #[tokio::test]
    async fn a_socket_that_does_not_take_the_prompt_falls_back_to_typing() {
        for result in [
            None,
            Some("no-session"),
            Some("held"),
            Some("dropped"),
            Some("refused"),
            Some("expired"),
            Some("denied"),
        ] {
            let fake = FakeHerdr::new();
            fake.set_inbox_result(result);
            let mut t = task(spec());
            let out = dispatch(&fake, &mut t, &Agents::default(), None, READY)
                .await
                .unwrap();
            assert_eq!(out, DispatchOutcome::Running, "{result:?}");
            assert_eq!(fake.inbox_sends().len(), 1, "{result:?}");
            let reqs = fake.requests();
            let prompt = reqs.iter().find(|r| r.method == "agent.prompt");
            assert!(prompt.is_some(), "{result:?}: typed");
            assert!(t.activity_seen, "{result:?}: the prompt reply was working");
        }
    }

    /// `delivery = "type"` never tries the socket; another kind has none.
    #[tokio::test]
    async fn a_typed_agent_never_tries_the_socket() {
        let fake = FakeHerdr::new();
        fake.set_inbox_result(Some("accepted"));
        let agents: Agents = toml::from_str("[claude]\ndelivery = \"type\"\n").unwrap();
        let mut t = task(spec());
        dispatch(&fake, &mut t, &agents, None, READY).await.unwrap();
        assert!(fake.inbox_sends().is_empty());
        assert!(fake.requests().iter().any(|r| r.method == "agent.prompt"));
    }

    /// A session launched bypassing permissions holds a message unless its
    /// sender says bypass; the agent's own config dir is where its session
    /// is found.
    #[tokio::test]
    async fn a_bypassing_session_is_sent_as_bypass_in_its_config_dir() {
        let fake = FakeHerdr::new();
        fake.set_inbox_result(Some("accepted"));
        let agents: Agents =
            toml::from_str("[claude]\nenv = { CLAUDE_CONFIG_DIR = \"~/.claude-alt\" }\n").unwrap();
        let mut t = task(DispatchSpec {
            agent_args: vec!["--dangerously-skip-permissions".into()],
            ..spec()
        });
        dispatch(&fake, &mut t, &agents, None, READY).await.unwrap();
        let sends = fake.inbox_sends();
        assert_eq!(sends[0].mode, crate::inbox::Mode::Bypass);
        assert_eq!(sends[0].config_dir.as_deref(), Some("~/.claude-alt"));
    }

    /// A blocked agent is not sent anything, socket or not: the machine
    /// sends the prompt once the block clears.
    #[tokio::test]
    async fn a_blocked_agent_gets_no_socket_send() {
        let fake = FakeHerdr::new();
        fake.set_inbox_result(Some("accepted"));
        fake.set_ready_after(Duration::from_millis(150));
        let watcher = fake.clone();
        tokio::spawn(async move {
            loop {
                if let Some(a) = watcher.agents().first() {
                    watcher.set_status(&a.pane_id, AgentStatus::Blocked);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let mut t = task(spec());
        let out = dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        assert_eq!(out, DispatchOutcome::Blocked);
        assert!(fake.inbox_sends().is_empty());
    }

    /// `summary = "off"` sends the prompt alone; `require` adds the
    /// warning after the line that asks.
    #[tokio::test]
    async fn dispatch_asks_for_a_summary_as_the_task_is_set() {
        use crate::task::{SUMMARY_REQUIRE, SummaryMode};
        let sent = |mode: SummaryMode| async move {
            let fake = FakeHerdr::new();
            let mut t = task(DispatchSpec {
                summary: mode,
                ..spec()
            });
            dispatch(&fake, &mut t, &Agents::default(), None, READY)
                .await
                .unwrap();
            let reqs = fake.requests();
            let prompt = reqs.iter().find(|r| r.method == "agent.prompt").unwrap();
            prompt.params["text"].as_str().unwrap().to_string()
        };
        assert_eq!(
            sent(SummaryMode::Off).await,
            "line one\n\"two\" {{ three }}"
        );
        let required = sent(SummaryMode::Require).await;
        assert!(required.ends_with(SUMMARY_REQUIRE), "{required}");
    }

    /// The workspace takes the label template rendered; the agent, which
    /// pastor finds by name, and the default branch stay `t-N`.
    #[tokio::test]
    async fn the_workspace_takes_the_label_and_the_agent_stays_t_n() {
        for worktree in [false, true] {
            let fake = FakeHerdr::new();
            let mut t = task(DispatchSpec {
                worktree,
                label: WorkspaceLabel {
                    template: Some("{{ machine }}:{{ task.id }}".into()),
                    from: Some("task run".into()),
                    ..Default::default()
                },
                ..spec()
            });
            dispatch(&fake, &mut t, &Agents::default(), None, READY)
                .await
                .unwrap();
            let reqs = fake.requests();
            let method = if worktree {
                "worktree.create"
            } else {
                "workspace.create"
            };
            let create = reqs.iter().find(|r| r.method == method).unwrap();
            assert_eq!(create.params["label"], "pi-1:t-7");
            if worktree {
                assert_eq!(create.params["branch"], "pastor/t-7");
            }
            let start = reqs.iter().find(|r| r.method == "agent.start").unwrap();
            assert_eq!(start.params["name"], "t-7");
            assert_eq!(t.agent_name.as_deref(), Some("t-7"));
            assert_eq!(t.spec.label.name.as_deref(), Some("pi-1:t-7"));
            assert_eq!(t.spec.label.note, None);
            assert_eq!(t.spec.label.from.as_deref(), Some("task run"));
        }
    }

    /// A label that renders empty or with a control character (an item's
    /// key can hold one) names the workspace `t-N`, and says why.
    #[tokio::test]
    async fn a_label_herdr_should_not_show_falls_back_to_t_n() {
        for key in ["a\u{1b}[2Jb", ""] {
            let fake = FakeHerdr::new();
            let mut t = task(DispatchSpec {
                label: WorkspaceLabel {
                    template: Some("{{ item.key }}".into()),
                    from: Some("job j".into()),
                    ..Default::default()
                },
                ..spec()
            });
            t.job = "j".into();
            t.item = serde_json::json!({ "key": key });
            dispatch(&fake, &mut t, &Agents::default(), None, READY)
                .await
                .unwrap();
            let create = fake
                .requests()
                .into_iter()
                .find(|r| r.method == "workspace.create")
                .unwrap();
            assert_eq!(create.params["label"], "t-7", "{key:?}");
            assert_eq!(t.spec.label.name.as_deref(), Some("t-7"));
            let note = t.spec.label.note.clone().unwrap();
            assert!(note.starts_with("fell back to t-7"), "{note}");
        }
    }

    /// A task that joins a workspace leaves its label alone, and records
    /// the one it joined.
    #[tokio::test]
    async fn a_joined_workspace_keeps_its_label() {
        let fake = FakeHerdr::new();
        fake.open_user_workspace("app", Some("/srv/app"));
        let mut t = task(spec());
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        assert!(!methods(&fake).iter().any(|m| m.ends_with(".create")));
        assert_eq!(t.spec.label.name.as_deref(), Some("app"));
        assert_eq!(t.spec.label.note.as_deref(), Some("joined workspace"));

        let mut shared = task(DispatchSpec {
            place: Place::Pastor,
            ..spec()
        });
        dispatch(&fake, &mut shared, &Agents::default(), None, READY)
            .await
            .unwrap();
        assert_eq!(shared.spec.label.name.as_deref(), Some("pastor"));
        assert_eq!(shared.spec.label.note.as_deref(), Some("joined workspace"));
    }

    /// A claude task starts on a session of its own, `--session-id` after
    /// every other arg, and the task records it so attach can resume it
    /// once the pane is gone.
    #[tokio::test]
    async fn a_claude_task_starts_on_a_session_it_records() {
        let fake = FakeHerdr::new();
        let mut t = task(DispatchSpec {
            deny: vec!["WebFetch".into()],
            ..spec()
        });
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        let id = t.spec.session_id.clone().expect("a session id");
        assert!(crate::task::is_session_id(&id), "{id}");
        let reqs = fake.requests();
        let start = reqs.iter().find(|r| r.method == "agent.start").unwrap();
        assert_eq!(
            start.params["args"],
            serde_json::json!([
                "--model",
                "opus",
                "--allowedTools",
                "Bash(pastor task done:*)",
                "--disallowedTools",
                "WebFetch",
                "--session-id",
                id
            ])
        );
        // Another task, another session.
        let mut u = task(spec());
        u.id = 8;
        dispatch(&FakeHerdr::new(), &mut u, &Agents::default(), None, READY)
            .await
            .unwrap();
        assert_ne!(u.spec.session_id.as_deref(), Some(id.as_str()));
    }

    /// Agent args that already pick a session keep it, and pastor records
    /// none; an agent of another kind gets no flag it would not know.
    #[tokio::test]
    async fn a_session_the_args_pick_or_another_kind_gets_no_session_id() {
        for args in [
            vec!["--session-id", "0d5bd3a4-2f35-4e1c-9f59-7c1c3a7b8e21"],
            vec!["--session-id=0d5bd3a4-2f35-4e1c-9f59-7c1c3a7b8e21"],
            vec!["--resume", "0d5bd3a4-2f35-4e1c-9f59-7c1c3a7b8e21"],
            vec!["-r", "0d5bd3a4-2f35-4e1c-9f59-7c1c3a7b8e21"],
            vec!["--continue"],
            vec!["-c"],
            vec!["--resume", "x", "--fork-session"],
        ] {
            let fake = FakeHerdr::new();
            let args: Vec<String> = args.into_iter().map(String::from).collect();
            let mut t = task(DispatchSpec {
                agent_args: args.clone(),
                ..spec()
            });
            // A stale id copied from somewhere else is dropped, too.
            t.spec.session_id = Some("stale".into());
            dispatch(&fake, &mut t, &Agents::default(), None, READY)
                .await
                .unwrap();
            assert_eq!(t.spec.session_id, None, "{args:?}");
            let reqs = fake.requests();
            let start = reqs.iter().find(|r| r.method == "agent.start").unwrap();
            // Their own, then the one tool every Claude task is allowed.
            let mut want = args.clone();
            want.extend([
                "--allowedTools".to_string(),
                "Bash(pastor task done:*)".into(),
            ]);
            assert_eq!(start.params["args"], serde_json::json!(want));
        }
        let fake = FakeHerdr::new();
        let mut t = task(DispatchSpec {
            agent: "codex".into(),
            agent_args: vec![],
            ..spec()
        });
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        assert_eq!(t.spec.session_id, None);
        let reqs = fake.requests();
        let start = reqs.iter().find(|r| r.method == "agent.start").unwrap();
        assert_eq!(start.params["args"], serde_json::json!([]));
    }

    /// The tool lists reach herdr as the agent's own flags, after its args.
    #[tokio::test]
    async fn dispatch_passes_the_tool_lists_as_the_agents_flags() {
        let fake = FakeHerdr::new();
        let mut t = task(DispatchSpec {
            allow: vec!["Bash(git:*)".into()],
            deny: vec!["WebFetch".into()],
            ..spec()
        });
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        let reqs = fake.requests();
        let start = reqs.iter().find(|r| r.method == "agent.start").unwrap();
        assert_eq!(
            start.params["args"],
            with_session(
                &t,
                serde_json::json!([
                    "--model",
                    "opus",
                    "--allowedTools",
                    "Bash(git:*)",
                    "--allowedTools",
                    "Bash(pastor task done:*)",
                    "--disallowedTools",
                    "WebFetch"
                ])
            )
        );
    }

    fn opencode_under(profile: Option<&str>) -> DispatchSpec {
        DispatchSpec {
            agent: "opencode".into(),
            agent_args: vec![],
            allow: vec!["Read".into()],
            deny: vec!["Edit".into()],
            agent_source: Some(Box::new(crate::task::AgentSource {
                ask: Default::default(),
                agent: "defaults".into(),
                agent_args: None,
                model: None,
                model_from: None,
                fallback: vec![],
                fallback_from: None,
                fallback_use: None,
                profile: profile.map(str::to_string),
                profile_from: Some("defaults".into()),
                timeout_from: None,
                place_from: None,
                lists_unapplied: None,
            })),
            ..spec()
        }
    }

    fn agy_under(profile: Option<&str>) -> DispatchSpec {
        DispatchSpec {
            agent: "agy".into(),
            ..opencode_under(profile)
        }
    }

    fn develop_deny() -> Vec<String> {
        develop_policy().deny
    }

    /// `develop` as a machine's own profile.
    fn develop_policy() -> crate::config::agy::Policy {
        crate::config::profile::Profiles::default()
            .resolve("develop")
            .unwrap()
            .agy_policy()
    }

    /// An agy task under `develop` whose deny list is `develop`'s, as
    /// `Profiles::apply` would settle it: what a develop machine may run.
    fn agy_develop() -> DispatchSpec {
        DispatchSpec {
            deny: develop_deny(),
            ..agy_under(Some("develop"))
        }
    }

    /// Dispatch `t` on `fake` whose machine's own profile is `own`; the
    /// error, if any.
    async fn dispatch_agy(
        fake: &FakeHerdr,
        t: &mut Task,
        own: &crate::config::agy::Policy,
    ) -> Option<String> {
        dispatch_with(
            fake,
            t,
            &Agents::default(),
            None,
            READY,
            Provision {
                own: Some(own),
                ..Default::default()
            },
        )
        .await
        .err()
        .map(|e| e.to_string())
    }

    fn agy_deny(fake: &FakeHerdr) -> serde_json::Value {
        let content = fake.agy_settings_now().unwrap().content.unwrap();
        serde_json::from_str::<serde_json::Value>(&content).unwrap()["permissions"]["deny"].clone()
    }

    /// Before a profiled agy launch, agy's settings get the deny rules of the
    /// machine's own profile (here `develop`), not the task's (a `review`
    /// task's `write_file(*)` would stop every edit on the machine, which is
    /// why settle keeps such a task off a `develop` machine). The check that
    /// follows then finds them covered.
    #[tokio::test]
    async fn a_profiled_agy_task_writes_the_machines_own_rules_first() {
        let fake = FakeHerdr::new();
        fake.set_agy_settings(Some(crate::provision::agy::AgySettings {
            content: None,
            link: false,
        }));
        let mut t = task(agy_develop());
        let err = dispatch_agy(&fake, &mut t, &develop_policy()).await;
        assert!(
            !err.clone()
                .unwrap_or_default()
                .contains("agy_permissions_missing"),
            "the provisioner refused: {err:?}"
        );
        assert_eq!(
            agy_deny(&fake),
            serde_json::json!([
                "command(rm -rf)",
                "command(sudo)",
                "command(git push --force)"
            ])
        );
        assert_eq!(fake.agy_writes().len(), 1);
        // Covered now: the next launch writes nothing.
        let mut t = task(agy_develop());
        dispatch_agy(&fake, &mut t, &develop_policy()).await;
        assert_eq!(fake.agy_writes().len(), 1);
    }

    /// The profile's lists replace the owner's `allow`, `deny` and `ask`
    /// whole, a stale allow and a rule agy saved from an "always allow"
    /// answer going with them; every other key keeps its value and place.
    #[tokio::test]
    async fn the_machines_profile_replaces_the_owners_rules_and_keeps_their_other_keys() {
        let fake = FakeHerdr::new();
        fake.set_agy_settings(Some(crate::provision::agy::AgySettings {
            content: Some(
                r#"{"theme":"dark","permissions":{"allow":["command(ls)"],"deny":["command(sudo)","command(shutdown)"],"ask":["command(git push)"]},"trustedWorkspaces":["/w"]}"#.into(),
            ),
            link: false,
        }));
        let mut t = task(agy_develop());
        dispatch_agy(&fake, &mut t, &develop_policy()).await;
        assert_eq!(
            agy_deny(&fake),
            serde_json::json!([
                "command(rm -rf)",
                "command(sudo)",
                "command(git push --force)"
            ])
        );
        let content = fake.agy_settings_now().unwrap().content.unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(
            v["permissions"]["allow"],
            serde_json::json!([
                "read_file(*)",
                "write_file(*)",
                "command(*)",
                "command(pastor task done)"
            ])
        );
        assert_eq!(v["permissions"]["ask"], serde_json::json!([]));
        assert_eq!(v["theme"], "dark");
        assert_eq!(v["trustedWorkspaces"], serde_json::json!(["/w"]));
        assert_eq!(v["toolPermission"], "always-proceed");
        assert_eq!(v["agentMode"], "accept-edits");
        assert!(content.find("theme").unwrap() < content.find("permissions").unwrap());
    }

    /// A machine whose agy settings cannot be read (a `command` machine) or
    /// parsed, or are a symlink that needs rules, refuses the task before
    /// anything is made there, with the rules to add by hand.
    #[tokio::test]
    async fn agy_settings_pastor_cannot_write_refuse_the_task() {
        let refused = async |settings| {
            let fake = FakeHerdr::new();
            fake.set_agy_settings(settings);
            let mut t = task(agy_develop());
            let err = dispatch_agy(&fake, &mut t, &develop_policy())
                .await
                .unwrap();
            assert!(err.contains("agy_permissions_missing"), "{err}");
            assert!(err.contains("command(sudo)"), "{err}");
            assert!(err.contains("pi-1"), "{err}");
            assert!(
                err.contains("~/.gemini/antigravity-cli/settings.json"),
                "{err}"
            );
            assert_eq!(t.state, TaskState::Failed);
            assert!(fake.requests().iter().all(|r| r.method != "agent.start"));
            assert!(fake.agy_writes().is_empty());
            err
        };
        let err = refused(None).await;
        assert!(err.contains("cannot read"), "{err}");
        let err = refused(Some(crate::provision::agy::AgySettings {
            content: Some("{ not json".into()),
            link: false,
        }))
        .await;
        assert!(err.contains("not JSON"), "{err}");
        let err = refused(Some(crate::provision::agy::AgySettings {
            content: Some("{}".into()),
            link: true,
        }))
        .await;
        assert!(err.contains("symlink"), "{err}");
        // A symlink that already says everything is fine as it is.
        let fake = FakeHerdr::new();
        fake.set_agy_settings(Some(crate::provision::agy::AgySettings {
            content: Some(
                r#"{"permissions":{"allow":["read_file(*)","write_file(*)","command(*)","command(pastor task done)"],"deny":["command(rm -rf)","command(sudo)","command(git push --force)"]},"toolPermission":"always-proceed","agentMode":"accept-edits"}"#.into(),
            ),
            link: true,
        }));
        let mut t = task(agy_develop());
        let err = dispatch_agy(&fake, &mut t, &develop_policy()).await;
        assert!(
            !err.clone()
                .unwrap_or_default()
                .contains("agy_permissions_missing"),
            "{err:?}"
        );
    }

    /// A profile's `[profiles.<name>.agy]` words reach the settings a
    /// dispatch writes. A profile that denies nothing never refuses a task
    /// over settings pastor cannot read or write (a launch under a profile
    /// approves every ask anyway); one that denies something does.
    #[tokio::test]
    async fn a_profiles_agy_words_are_written_and_an_open_profile_never_refuses() {
        let profiles: crate::config::profile::Profiles = toml::from_str(
            "[open]\nallow = [\"Bash\"]\n[open.agy]\nqueued_messages = \"send-immediately\"\n\
             allow_non_workspace_access = false\n",
        )
        .unwrap();
        let open = profiles.resolve("open").unwrap().agy_policy();
        let fake = FakeHerdr::new();
        fake.set_agy_settings(Some(crate::provision::agy::AgySettings {
            content: None,
            link: false,
        }));
        let mut t = task(agy_develop());
        dispatch_agy(&fake, &mut t, &open).await;
        let content = fake.agy_settings_now().unwrap().content.unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["queuedMessages"], "send-immediately");
        assert_eq!(v["allowNonWorkspaceAccess"], false);
        assert_eq!(
            v["permissions"]["allow"],
            serde_json::json!(["command(*)", "command(pastor task done)"])
        );
        assert_eq!(v["permissions"]["deny"], serde_json::json!([]));

        // Settings pastor cannot read: no deny rule missing, no refusal.
        let fake = FakeHerdr::new();
        fake.set_agy_settings(None);
        let mut t = task(DispatchSpec {
            deny: vec![],
            ..agy_under(Some("open"))
        });
        let err = dispatch_agy(&fake, &mut t, &open).await.unwrap_or_default();
        assert!(!err.contains("agy_permissions_missing"), "{err}");
        let mut t = task(agy_develop());
        let err = dispatch_agy(&fake, &mut t, &develop_policy())
            .await
            .unwrap();
        assert!(err.contains("agy_permissions_missing"), "{err}");
    }

    /// agy saving between pastor's read and write is read again and merged
    /// once; twice in a row refuses the task.
    #[tokio::test]
    async fn an_agy_save_during_the_write_is_merged_once() {
        let empty = || {
            Some(crate::provision::agy::AgySettings {
                content: None,
                link: false,
            })
        };
        let fake = FakeHerdr::new();
        fake.set_agy_settings(empty());
        fake.agy_saves_before_write(Some(r#"{"model":"x"}"#));
        let mut t = task(agy_develop());
        dispatch_agy(&fake, &mut t, &develop_policy()).await;
        let content = fake.agy_settings_now().unwrap().content.unwrap();
        assert!(content.contains("\"model\": \"x\""), "{content}");
        assert_eq!(agy_deny(&fake).as_array().unwrap().len(), 3);

        let fake = FakeHerdr::new();
        fake.set_agy_settings(empty());
        fake.agy_saves_before_write(Some("{}"));
        fake.agy_saves_before_write(Some(r#"{"a":1}"#));
        let mut t = task(agy_develop());
        let err = dispatch_agy(&fake, &mut t, &develop_policy())
            .await
            .unwrap();
        assert!(err.contains("agy_permissions_missing"), "{err}");
        assert!(err.contains("kept saving"), "{err}");
    }

    /// Nothing is written for a task that is not agy under a profile, or for
    /// an agent with `own_permissions`: a `command` machine does not refuse
    /// them. (A profiled agy task on a machine whose own profile denies
    /// nothing is still checked, and a `command` machine refuses that:
    /// `a_profiled_agy_task_needs_its_denies_in_the_machines_agy_settings`.)
    #[tokio::test]
    async fn the_agy_provisioner_leaves_other_tasks_alone() {
        let own = develop_policy();
        let mut claude = agy_develop();
        claude.agent = "claude".into();
        let own_permissions: Agents = toml::from_str("[agy]\nown_permissions = true\n").unwrap();
        for (spec, agents, own) in [
            (claude, Agents::default(), own.clone()),
            (agy_under(None), Agents::default(), own.clone()),
            (agy_develop(), own_permissions, own.clone()),
        ] {
            let fake = FakeHerdr::new();
            fake.set_agy_settings(None);
            let mut t = task(spec);
            let err = dispatch_with(
                &fake,
                &mut t,
                &agents,
                None,
                READY,
                Provision {
                    own: Some(&own),
                    ..Default::default()
                },
            )
            .await
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
            assert!(!err.contains("agy_permissions_missing"), "{err}");
        }
    }

    /// A profiled agy task starts with the flags that stop it asking when
    /// the machine's agy settings deny what it denies; with a rule missing,
    /// with settings pastor cannot read, and on a machine that takes no
    /// shell command, it fails before anything is made there. Without a
    /// profile nothing is checked.
    #[tokio::test]
    async fn a_profiled_agy_task_needs_its_denies_in_the_machines_agy_settings() {
        let fake = FakeHerdr::new();
        let mut t = task(agy_under(Some("review")));
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        let start = fake.requests();
        let start = start.iter().find(|r| r.method == "agent.start").unwrap();
        assert_eq!(start.params["kind"], "agy");
        assert_eq!(
            start.params["args"],
            serde_json::json!(["--dangerously-skip-permissions", "--mode", "accept-edits"])
        );

        let refused = |content: Option<&'static str>, says: &'static str| async move {
            let fake = FakeHerdr::new();
            fake.set_agy_settings(content.map(|c| crate::provision::agy::AgySettings {
                content: Some(c.into()),
                link: false,
            }));
            let mut t = task(agy_under(Some("review")));
            let err = dispatch(&fake, &mut t, &Agents::default(), None, READY)
                .await
                .unwrap_err();
            let text = err.to_string();
            assert!(text.contains("agy_permissions_missing"), "{text}");
            assert!(text.contains(says), "{text}");
            assert!(text.contains("pi-1"), "{text}");
            assert!(!err.is_transport());
            assert_eq!(t.state, TaskState::Failed);
            assert!(!fake.requests().iter().any(|r| r.method == "agent.start"));
        };
        refused(
            Some(r#"{"permissions":{"deny":["command(sudo)"]}}"#),
            "write_file(*)",
        )
        .await;
        refused(Some("{"), "settings.json").await;
        refused(None, "cannot be read").await;

        let fake = FakeHerdr::new();
        fake.set_agy_settings(None);
        let mut t = task(DispatchSpec {
            allow: vec![],
            deny: vec![],
            ..agy_under(None)
        });
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
    }

    /// A profiled opencode task starts with its rules in the pane's env.
    /// On a machine whose managed opencode config (or managed preferences)
    /// has permission rules it fails before anything is made there: those
    /// are merged after `OPENCODE_CONFIG_CONTENT`, so pastor cannot
    /// neutralise them, and opencode would merge them with the profile's.
    /// A machine that cannot tell goes ahead, and a task with no profile
    /// never asks.
    #[tokio::test]
    async fn a_profiled_opencode_task_refuses_a_machine_with_managed_rules() {
        let fake = FakeHerdr::new();
        let mut t = task(opencode_under(Some("review")));
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        let env = fake.pane_env(t.pane_id.as_deref().unwrap());
        assert_eq!(
            env["OPENCODE_PERMISSION"],
            crate::config::opencode::permission_json(&t.spec.allow, &t.spec.deny, false, &[])
        );
        assert_eq!(env["OPENCODE_CONFIG"], "");
        let start = fake.requests();
        let start = start.iter().find(|r| r.method == "agent.start").unwrap();
        assert_eq!(start.params["kind"], "opencode");
        assert_eq!(start.params["args"], serde_json::json!([]));

        let fake = FakeHerdr::new();
        fake.set_opencode_permissions(Some(vec![crate::config::opencode::ConfigCheckHit::Key {
            file: "/etc/opencode/opencode.json".into(),
            key: "permission".into(),
        }]));
        let mut t = task(opencode_under(Some("review")));
        let err = dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("opencode_permissions_conflict"),
            "{err}"
        );
        assert!(err.to_string().contains("pi-1"), "{err}");
        assert!(
            err.to_string()
                .contains("/etc/opencode/opencode.json (permission)"),
            "{err}"
        );
        assert!(!err.is_transport());
        assert_eq!(t.state, TaskState::Failed);
        assert!(fake.requests().is_empty(), "{:?}", fake.requests());

        fake.set_opencode_permissions(None);
        let mut t = task(opencode_under(Some("review")));
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();

        let fake = FakeHerdr::new();
        fake.set_opencode_permissions(Some(vec![
            crate::config::opencode::ConfigCheckHit::Unreadable {
                file: "/etc/opencode/opencode.json".into(),
            },
        ]));
        let mut t = task(DispatchSpec {
            allow: vec![],
            deny: vec![],
            ..opencode_under(None)
        });
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
    }

    /// A managed config file `CONFIG_CHECK_COMMAND` could not read refuses
    /// the task the same as one with rules in it, naming the file.
    #[tokio::test]
    async fn a_profiled_opencode_task_refuses_an_unreadable_managed_file() {
        let fake = FakeHerdr::new();
        fake.set_opencode_permissions(Some(vec![
            crate::config::opencode::ConfigCheckHit::Unreadable {
                file: "/etc/opencode/opencode.json".into(),
            },
        ]));
        let mut t = task(opencode_under(Some("review")));
        let err = dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("opencode_permissions_conflict"),
            "{err}"
        );
        assert!(
            err.to_string()
                .contains("/etc/opencode/opencode.json (unreadable)"),
            "{err}"
        );
        assert_eq!(t.state, TaskState::Failed);
    }

    /// A machine whose own global opencode config or `~/.opencode` has
    /// permission rules of its own no longer fails the task: every key
    /// pastor found there is denied in `OPENCODE_PERMISSION` instead, even
    /// under `unrestricted`, and the task runs. The env then has no open
    /// fallback of its own, which opencode would put after the machine's
    /// keys (`config::opencode::permission_json`): what opencode's defaults
    /// ask about is allowed by name instead.
    #[tokio::test]
    async fn a_profiled_opencode_task_denies_the_machines_own_keys_instead_of_failing() {
        let fake = FakeHerdr::new();
        fake.set_opencode_deny_keys(crate::config::opencode::MachineDenyKeys::Keys(vec![
            "bash".into(),
            "webfetch".into(),
        ]));
        let mut t = task(opencode_under(Some("unrestricted")));
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        let env = fake.pane_env(t.pane_id.as_deref().unwrap());
        let permission: serde_json::Value =
            serde_json::from_str(env["OPENCODE_PERMISSION"].as_str().unwrap()).unwrap();
        assert!(permission.get("*").is_none(), "{permission}");
        assert_eq!(permission["bash"], "deny", "{permission}");
        assert_eq!(permission["webfetch"], "deny", "{permission}");
        assert_eq!(permission["read"], "allow", "{permission}");
        assert_eq!(permission["external_directory"], "allow", "{permission}");
    }

    /// What `OPENCODE_PERMISSION` cannot neutralise on the machine's own
    /// global config or `~/.opencode` (a file pastor could not parse, rules
    /// under an agent, too much config) fails the task the same way a
    /// managed config's rules do, saying what was found.
    #[tokio::test]
    async fn a_profiled_opencode_task_refuses_a_machine_whose_own_config_it_cannot_neutralise() {
        let fake = FakeHerdr::new();
        fake.set_opencode_deny_keys(crate::config::opencode::MachineDenyKeys::Conflict(
            "has permission rules under agent.build (bash)".into(),
        ));
        let mut t = task(opencode_under(Some("review")));
        let err = dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("opencode_permissions_conflict"),
            "{err}"
        );
        assert!(
            err.to_string()
                .contains("on pi-1 has permission rules under agent.build (bash); move"),
            "{err}"
        );
        assert!(!err.is_transport());
        assert_eq!(t.state, TaskState::Failed);
        assert!(fake.requests().is_empty(), "{:?}", fake.requests());
    }

    /// A profiled opencode task runs with the repo's own opencode config
    /// off, so a checkout cannot add rules to the profile's; the repo's
    /// `AGENTS.md` or `CLAUDE.md`, which that also turns off, comes back
    /// by its path in the directory the agent works in, a worktree's too:
    /// the first one there, as opencode would pick. A task with no profile
    /// keeps the repo's config.
    #[tokio::test]
    async fn a_profiled_opencode_task_turns_the_repo_config_off_but_keeps_its_instructions() {
        let instructions = |env: &serde_json::Value| {
            let content: serde_json::Value =
                serde_json::from_str(env["OPENCODE_CONFIG_CONTENT"].as_str().unwrap()).unwrap();
            content
        };
        let fake = FakeHerdr::new();
        fake.set_file("/srv/app/AGENTS.md");
        fake.set_file("/srv/app/CLAUDE.md");
        let mut t = task(opencode_under(Some("review")));
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        let env = fake.pane_env(t.pane_id.as_deref().unwrap());
        assert_eq!(env["OPENCODE_DISABLE_PROJECT_CONFIG"], "1");
        assert_eq!(
            instructions(&env),
            serde_json::json!({"instructions": ["/srv/app/AGENTS.md"]}),
            "opencode reads only the first file there, AGENTS.md before CLAUDE.md"
        );

        let fake = FakeHerdr::new();
        let mut t = task(opencode_under(Some("review")));
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        let env = fake.pane_env(t.pane_id.as_deref().unwrap());
        assert_eq!(instructions(&env), serde_json::json!({"instructions": []}));

        let fake = FakeHerdr::new();
        fake.set_file("/fake/worktrees/pastor-k1/CLAUDE.md");
        let mut t = task(DispatchSpec {
            worktree: true,
            branch: Some("pastor/k1".into()),
            ..opencode_under(Some("review"))
        });
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        let env = fake.pane_env(t.pane_id.as_deref().unwrap());
        assert_eq!(env["OPENCODE_DISABLE_PROJECT_CONFIG"], "1");
        assert_eq!(
            instructions(&env),
            serde_json::json!({"instructions": ["/fake/worktrees/pastor-k1/CLAUDE.md"]})
        );

        let fake = FakeHerdr::new();
        let mut t = task(DispatchSpec {
            allow: vec![],
            deny: vec![],
            ..opencode_under(None)
        });
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        let env = fake.pane_env(t.pane_id.as_deref().unwrap());
        assert!(
            env.get("OPENCODE_DISABLE_PROJECT_CONFIG").is_none(),
            "{env}"
        );
        assert!(env.get("OPENCODE_CONFIG_CONTENT").is_none(), "{env}");
    }

    /// An agent that cannot take a list it was given fails the task before
    /// anything is made on the machine.
    #[tokio::test]
    async fn dispatch_refuses_a_tool_list_the_agent_has_no_flag_for() {
        let fake = FakeHerdr::new();
        let mut t = task(DispatchSpec {
            agent: "codex".into(),
            deny: vec!["WebFetch".into()],
            ..spec()
        });
        let err = dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("deny_flag"), "{err}");
        assert!(!err.is_transport());
        assert_eq!(t.state, TaskState::Failed);
        assert!(fake.requests().is_empty(), "{:?}", fake.requests());
    }

    /// `[agents.claude-alt] kind = "claude"` with an env: herdr starts
    /// a claude, with Claude's tool flags, in a pane that has the env, `~`
    /// expanded against the machine's home, a bare `~` too.
    fn personal() -> Agents {
        let mut agents = Agents::default();
        agents.0.insert(
            "claude-alt".into(),
            crate::config::AgentDef {
                kind: Some("claude".into()),
                env: [
                    ("CLAUDE_CONFIG_DIR".to_string(), "~/.claude-alt".to_string()),
                    ("PLAIN".to_string(), "a~b".to_string()),
                    ("BARE".to_string(), "~".to_string()),
                ]
                .into(),
                ..Default::default()
            },
        );
        agents
    }

    const CLAUDE_STORE: &str = "{\n  \"projects\": {},\n  \"userID\": \"abc\"\n}\n";

    /// Dispatch `t` with `trusted` as the trust pastor holds for the machine.
    async fn dispatch_trusting(fake: &FakeHerdr, t: &mut Task, agents: &Agents, trusted: &[&str]) {
        let trusted: Vec<String> = trusted.iter().map(|s| s.to_string()).collect();
        dispatch_with(
            fake,
            t,
            agents,
            None,
            READY,
            Provision {
                trusted: &trusted,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }

    /// A Claude task in a folder pastor trusts has it trusted in the agent
    /// home's store before the agent starts, so the dialog never shows; one
    /// in a folder nobody trusted, and one whose store is not there, are
    /// left as they were.
    #[tokio::test]
    async fn a_trusted_folder_is_written_into_claudes_store_before_the_start() {
        let fake = FakeHerdr::new();
        fake.set_home_file("~/.claude.json", Some(CLAUDE_STORE));
        let mut t = task(DispatchSpec {
            repo: Some("/srv/app".into()),
            ..spec()
        });
        dispatch_trusting(&fake, &mut t, &Agents::default(), &["/srv/app"]).await;
        let v: serde_json::Value =
            serde_json::from_str(&fake.home_file_now("~/.claude.json").unwrap()).unwrap();
        assert_eq!(v["projects"]["/srv/app"]["hasTrustDialogAccepted"], true);
        assert_eq!(v["userID"], "abc");
        assert!(fake.requests().iter().any(|r| r.method == "agent.start"));
        assert_eq!(fake.file_writes().len(), 1);

        // Not a folder pastor trusts.
        let fake = FakeHerdr::new();
        fake.set_home_file("~/.claude.json", Some(CLAUDE_STORE));
        let mut t = task(spec());
        dispatch_trusting(&fake, &mut t, &Agents::default(), &["/srv/other"]).await;
        assert_eq!(
            fake.home_file_now("~/.claude.json").as_deref(),
            Some(CLAUDE_STORE)
        );
        assert!(fake.file_writes().is_empty());

        // A home that has never run Claude has no store to add to.
        let fake = FakeHerdr::new();
        let mut t = task(spec());
        dispatch_trusting(&fake, &mut t, &Agents::default(), &["/srv/app"]).await;
        assert_eq!(fake.home_file_now("~/.claude.json"), None);
        assert!(fake.file_writes().is_empty());
    }

    /// The store is the one of the agent's config dir, and a task with no
    /// repo is trusted by the folder it starts in.
    #[tokio::test]
    async fn the_store_follows_the_agents_home_and_a_no_repo_task_its_folder() {
        let fake = FakeHerdr::new();
        let store = "~/.claude-alt/.claude.json";
        fake.set_home_file(store, Some(CLAUDE_STORE));
        let mut t = task(DispatchSpec {
            agent: "claude-alt".into(),
            repo: None,
            ..spec()
        });
        // The key pastor holds for such a task is its folder.
        dispatch_trusting(&fake, &mut t, &personal(), &["/home/fake/pastor-tasks"]).await;
        let v: serde_json::Value =
            serde_json::from_str(&fake.home_file_now(store).unwrap()).unwrap();
        assert_eq!(
            v["projects"]["/home/fake/pastor-tasks"]["hasTrustDialogAccepted"], true,
            "{v}"
        );
    }

    /// A worktree task starts in its checkout, a folder of its own: that is
    /// the folder written, on the strength of the trust held for its repo.
    #[tokio::test]
    async fn a_worktree_tasks_checkout_is_written_on_its_repos_trust() {
        let fake = FakeHerdr::new();
        fake.set_home_file("~/.claude.json", Some(CLAUDE_STORE));
        let mut t = task(DispatchSpec {
            repo: Some("/srv/app".into()),
            worktree: true,
            branch: Some("pastor/k1".into()),
            ..spec()
        });
        dispatch_trusting(&fake, &mut t, &Agents::default(), &["/srv/app"]).await;
        let v: serde_json::Value =
            serde_json::from_str(&fake.home_file_now("~/.claude.json").unwrap()).unwrap();
        let projects = v["projects"].as_object().unwrap();
        assert_eq!(
            projects.keys().collect::<Vec<_>>(),
            ["/fake/worktrees/pastor-k1"],
            "the checkout, not the repo: {v}"
        );
        assert_eq!(
            v["projects"]["/fake/worktrees/pastor-k1"]["hasTrustDialogAccepted"],
            true
        );
    }

    /// agy's trusted workspaces are written into its settings.
    #[tokio::test]
    async fn a_trusted_folder_is_a_trusted_workspace_of_agy() {
        let fake = FakeHerdr::new();
        fake.set_home_file(
            crate::provision::agy::SETTINGS_FILE,
            Some("{\"agentMode\": \"accept-edits\"}\n"),
        );
        let mut t = task(DispatchSpec {
            agent: "agy".into(),
            ..spec()
        });
        dispatch_trusting(&fake, &mut t, &Agents::default(), &["/srv/app"]).await;
        let v: serde_json::Value = serde_json::from_str(
            &fake
                .home_file_now(crate::provision::agy::SETTINGS_FILE)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(v["trustedWorkspaces"], serde_json::json!(["/srv/app"]));
        assert_eq!(v["agentMode"], "accept-edits");
    }

    /// Another kind has no store here, and a write that cannot be done never
    /// fails the task.
    #[tokio::test]
    async fn trust_that_cannot_be_written_does_not_fail_the_task() {
        let fake = FakeHerdr::new();
        fake.set_home_file("~/.claude.json", Some("{ nope"));
        let mut t = task(spec());
        dispatch_trusting(&fake, &mut t, &Agents::default(), &["/srv/app"]).await;
        assert!(fake.requests().iter().any(|r| r.method == "agent.start"));
        let fake = FakeHerdr::new();
        let mut t = task(DispatchSpec {
            agent: "codex".into(),
            ..spec()
        });
        dispatch_trusting(&fake, &mut t, &Agents::default(), &["/srv/app"]).await;
        assert!(fake.file_writes().is_empty());
    }

    #[tokio::test]
    async fn an_agent_definition_starts_its_kind_with_its_env() {
        let fake = FakeHerdr::new();
        let mut t = task(DispatchSpec {
            agent: "claude-alt".into(),
            deny: vec!["WebFetch".into()],
            ..spec()
        });
        dispatch(&fake, &mut t, &personal(), None, READY)
            .await
            .unwrap();
        let reqs = fake.requests();
        let ws = reqs
            .iter()
            .find(|r| r.method == "workspace.create")
            .unwrap();
        // The classic renderer is a built-in of the claude kind.
        let want = serde_json::json!({
            "BARE": "/home/fake",
            "CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN": "1",
            "CLAUDE_CONFIG_DIR": "/home/fake/.claude-alt",
            "PASTOR_TASK": "t-7",
            "PLAIN": "a~b",
        });
        assert_eq!(ws.params["env"], want);
        let start = reqs.iter().find(|r| r.method == "agent.start").unwrap();
        assert_eq!(start.params["kind"], "claude");
        assert_eq!(start.params["name"], "t-7");
        assert_eq!(
            start.params["args"],
            with_session(
                &t,
                serde_json::json!([
                    "--model",
                    "opus",
                    "--allowedTools",
                    "Bash(pastor task done:*)",
                    "--disallowedTools",
                    "WebFetch"
                ])
            )
        );
        assert_eq!(fake.pane_env(t.pane_id.as_deref().unwrap()), want);
        assert!(!reqs.iter().any(|r| r.method == "pane.split"));
    }

    /// A worktree gets no env from herdr, so the agent runs in a pane split
    /// off it with the env, and the pane without it is closed.
    #[tokio::test]
    async fn a_worktree_agent_gets_its_env_through_a_split_pane() {
        let fake = FakeHerdr::new();
        let mut t = task(DispatchSpec {
            agent: "claude-alt".into(),
            worktree: true,
            branch: Some("pastor/k1".into()),
            ..spec()
        });
        dispatch(&fake, &mut t, &personal(), None, READY)
            .await
            .unwrap();
        let reqs = fake.requests();
        let methods: Vec<&str> = reqs.iter().map(|r| r.method.as_str()).collect();
        let at = |m: &str| methods.iter().position(|x| *x == m).unwrap();
        assert!(at("worktree.create") < at("pane.split"), "{methods:?}");
        assert!(at("pane.split") < at("pane.close"), "{methods:?}");
        assert!(at("pane.close") < at("agent.start"), "{methods:?}");
        let split = &reqs[at("pane.split")];
        assert_eq!(split.params["target_pane_id"], "w1:p1");
        assert_eq!(split.params["cwd"], "/fake/worktrees/pastor-k1");
        assert_eq!(reqs[at("pane.close")].params["pane_id"], "w1:p1");
        let pane = t.pane_id.clone().unwrap();
        assert_ne!(pane, "w1:p1");
        assert_eq!(reqs[at("agent.start")].params["pane_id"], pane.as_str());
        assert_eq!(
            fake.pane_env(&pane)["CLAUDE_CONFIG_DIR"],
            "/home/fake/.claude-alt"
        );
        assert_eq!(t.workspace_id.as_deref(), Some("w1"));

        assert_eq!(fake.pane_env(&pane)["PASTOR_TASK"], "t-7");
    }

    /// Every agent pastor starts has `PASTOR_TASK` in its pane, so the head
    /// knows its requests for an agent's (`Daemon::handle_from`), even with
    /// no env of its own and in a worktree, which gets its env only through a
    /// split pane.
    #[tokio::test]
    async fn every_agent_pane_is_marked_with_its_task() {
        let fake = FakeHerdr::new();
        let mut t = task(spec());
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        assert_eq!(
            fake.pane_env(t.pane_id.as_deref().unwrap()),
            serde_json::json!({"CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN": "1", "PASTOR_TASK": "t-7"}),
            "PASTOR_TASK, and the claude kind's classic renderer"
        );

        let fake = FakeHerdr::new();
        let mut t = task(DispatchSpec {
            worktree: true,
            branch: Some("pastor/k2".into()),
            ..spec()
        });
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        let pane = t.pane_id.clone().unwrap();
        assert_ne!(pane, "w1:p1");
        assert_eq!(fake.pane_env(&pane)["PASTOR_TASK"], "t-7");
    }

    /// An agent of a kind its machine pins to a version gets the env its
    /// manifest says keeps it from updating itself; one pinned to `latest`,
    /// or not pinned, does not.
    #[tokio::test]
    async fn a_pinned_harness_is_kept_from_updating_itself() {
        let harnesses = crate::harness_manifest::Harnesses::builtin();
        for (pin, set) in [
            (Some("2.1.310"), true),
            (Some("latest"), false),
            (None, false),
        ] {
            let pins: crate::harness::Pins = pin
                .map(|p| ("claude".to_string(), p.to_string()))
                .into_iter()
                .collect();
            let fake = FakeHerdr::new();
            let mut t = task(spec());
            dispatch_with(
                &fake,
                &mut t,
                &Agents::default(),
                None,
                READY,
                Provision {
                    pins: Some(&pins),
                    harnesses: Some(harnesses),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            let env = fake.pane_env(t.pane_id.as_deref().unwrap());
            assert_eq!(env.get("DISABLE_AUTOUPDATER").is_some(), set, "{pin:?}");
        }
    }

    /// The env layers reach the pane: `[defaults]` under the task's flock's,
    /// under the machine's, under the agent's own, key by key; another
    /// flock's layer and a task with no flock get no flock layer; the marks
    /// pastor sets stay last.
    #[tokio::test]
    async fn env_layers_reach_the_pane_under_the_agents_own() {
        let layers = crate::config::EnvLayers {
            defaults: [("A", "d"), ("B", "d"), ("C", "d"), ("D", "d")]
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .into(),
            flocks: [
                (
                    "work".to_string(),
                    [("B", "work"), ("C", "work"), ("D", "work")]
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .into(),
                ),
                (
                    "play".to_string(),
                    [("B", "play")]
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .into(),
                ),
            ]
            .into(),
            machine: [("C", "machine"), ("D", "machine"), ("PASTOR_TASK", "mine")]
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .into(),
        };
        let mut agents = personal();
        agents
            .0
            .get_mut("claude-alt")
            .unwrap()
            .env
            .insert("D".into(), "agent".into());
        let pane_env = async |flock: Option<&str>| {
            let fake = FakeHerdr::new();
            let mut t = task(DispatchSpec {
                agent: "claude-alt".into(),
                ..spec()
            });
            t.flock = flock.map(str::to_string);
            dispatch_with(
                &fake,
                &mut t,
                &agents,
                None,
                READY,
                Provision {
                    env: Some(&layers),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            fake.pane_env(t.pane_id.as_deref().unwrap())
        };
        let env = pane_env(Some("work")).await;
        assert_eq!(env["A"], "d", "defaults");
        assert_eq!(env["B"], "work", "the flock over defaults");
        assert_eq!(env["C"], "machine", "the machine over the flock");
        assert_eq!(env["D"], "agent", "the agent over all");
        assert!(
            env["CLAUDE_CONFIG_DIR"].as_str().unwrap().starts_with('/'),
            "its own keys stay"
        );
        assert_eq!(env["PASTOR_TASK"], "t-7", "the mark stays last");
        assert_eq!(pane_env(Some("play")).await["B"], "play");
        assert_eq!(pane_env(None).await["B"], "d", "no flock, no flock layer");
        // Not given, nothing changes.
        let fake = FakeHerdr::new();
        let mut t = task(DispatchSpec {
            agent: "claude-alt".into(),
            ..spec()
        });
        dispatch(&fake, &mut t, &agents, None, READY).await.unwrap();
        assert!(
            fake.pane_env(t.pane_id.as_deref().unwrap())
                .get("A")
                .is_none()
        );
    }

    /// A worktree task's base is fetched from its repo's remote before the
    /// worktree is made; a task without a worktree, one that said `--no-fetch`
    /// and one that reopens a checkout fetch nothing.
    #[tokio::test]
    async fn the_base_is_fetched_before_a_new_worktree_is_made() {
        let fetched = |t: &DispatchSpec| {
            let t = t.clone();
            async move {
                let fake = FakeHerdr::new();
                let mut task = task(t);
                dispatch(&fake, &mut task, &Agents::default(), None, READY)
                    .await
                    .unwrap();
                (fake, task)
            }
        };
        let (fake, t) = fetched(&DispatchSpec {
            worktree: true,
            repo: Some("/r/app".into()),
            ..spec()
        })
        .await;
        assert_eq!(fake.fetches(), ["/r/app"]);
        let before = fake.fetched_after()[0];
        let create = fake
            .requests()
            .iter()
            .position(|r| r.method == "worktree.create")
            .unwrap();
        assert!(create >= before, "fetched first: {before} vs {create}");
        assert_eq!(t.spec.fetch.warning, None);

        for (why, s) in [
            (
                "--no-fetch",
                DispatchSpec {
                    worktree: true,
                    repo: Some("/r/app".into()),
                    fetch: crate::task::FetchNote {
                        skip: true,
                        warning: None,
                    },
                    ..spec()
                },
            ),
            (
                "no worktree",
                DispatchSpec {
                    worktree: false,
                    repo: Some("/r/app".into()),
                    ..spec()
                },
            ),
        ] {
            let (fake, _) = fetched(&s).await;
            assert!(fake.fetches().is_empty(), "{why}");
        }
    }

    /// A fetch that fails is a warning on the task, not a failure: the
    /// worktree is made from the repo as it is, the reason is in
    /// `fetch.warning`, and a dispatch that fetches fine clears it. A remote
    /// that cannot be reached, a machine that cannot be asked and a repo with
    /// no remote at all are all fine for the task.
    #[tokio::test]
    async fn a_failed_fetch_warns_and_the_worktree_is_made_anyway() {
        use crate::herdr::Fetched;
        let wt = || DispatchSpec {
            worktree: true,
            repo: Some("/r/app".into()),
            ..spec()
        };
        let fake = FakeHerdr::new();
        fake.set_fetch(Fetched::Failed(
            "origin".into(),
            "fatal: unable to access 'https://git.example/app.git'".into(),
        ));
        let mut t = task(wt());
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        assert_eq!(t.state, TaskState::Running);
        let why = t.spec.fetch.warning.clone().unwrap();
        assert!(why.contains("could not fetch origin"), "{why}");
        assert!(why.contains("unable to access"), "{why}");
        assert!(why.contains("made from /r/app as it is"), "{why}");
        assert!(
            fake.requests()
                .iter()
                .any(|r| r.method == "worktree.create"),
            "the worktree was made"
        );

        // The next dispatch of the task says afresh what its fetch found.
        let fake = FakeHerdr::new();
        fake.set_fetch(Fetched::Done("origin".into()));
        let mut again = task(DispatchSpec {
            fetch: t.spec.fetch.clone(),
            ..wt()
        });
        dispatch(&fake, &mut again, &Agents::default(), None, READY)
            .await
            .unwrap();
        assert_eq!(again.spec.fetch.warning, None);

        for answer in [Fetched::NoRemote] {
            let fake = FakeHerdr::new();
            fake.set_fetch(answer);
            let mut t = task(wt());
            dispatch(&fake, &mut t, &Agents::default(), None, READY)
                .await
                .unwrap();
            assert_eq!(t.spec.fetch.warning, None);
        }
    }

    /// A head address reaches the agent as `PASTOR_HEAD`, after the
    /// `[agents]` env so config cannot clear it; without one there is none.
    #[tokio::test]
    async fn the_head_address_goes_in_the_pane_env() {
        let fake = FakeHerdr::new();
        let mut t = task(DispatchSpec {
            agent: "claude-alt".into(),
            ..spec()
        });
        let mut agents = personal();
        agents
            .0
            .get_mut("claude-alt")
            .unwrap()
            .env
            .insert("PASTOR_HEAD".into(), "elsewhere".into());
        dispatch(&fake, &mut t, &agents, Some("user@head.example"), READY)
            .await
            .unwrap();
        let env = fake.pane_env(t.pane_id.as_deref().unwrap());
        assert_eq!(env["PASTOR_HEAD"], "user@head.example");
        assert_eq!(env["PASTOR_TASK"], "t-7");

        let fake = FakeHerdr::new();
        let mut t = task(spec());
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        let env = fake.pane_env(t.pane_id.as_deref().unwrap());
        assert!(env.get("PASTOR_HEAD").is_none(), "{env}");
    }

    #[tokio::test]
    async fn dispatch_uses_worktree_when_asked() {
        let fake = FakeHerdr::new();
        let mut t = task(DispatchSpec {
            worktree: true,
            branch: Some("pastor/k1".into()),
            ..spec()
        });
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        let wt = fake
            .requests()
            .into_iter()
            .find(|r| r.method == "worktree.create")
            .unwrap();
        assert_eq!(wt.params["cwd"], "/srv/app");
        assert_eq!(wt.params["branch"], "pastor/k1");
        assert_eq!(wt.params["label"], "home/t-7");
    }

    /// A retry's `worktree.open` can answer a workspace someone else already
    /// has open on the checkout (`already_open`): that workspace's own
    /// label, not the template this task just rendered, is what gets
    /// recorded, and the task is marked as having joined it.
    #[tokio::test]
    async fn a_reopened_worktree_already_open_elsewhere_keeps_its_own_label() {
        let fake = FakeHerdr::new();
        fake.worktree_create("/srv/app", "pastor/k1", "seed")
            .await
            .unwrap();
        let path = fake
            .worktree_list("/srv/app")
            .await
            .unwrap()
            .into_iter()
            .find(|w| w.branch.as_deref() == Some("pastor/k1"))
            .unwrap()
            .path;
        let mut t = task(DispatchSpec {
            worktree: true,
            reopen: Some(Box::new(Reopen {
                branch: "pastor/k1".into(),
                path,
                agent: "someone-else".into(),
            })),
            // A warning the task came with, from the dispatch of the task
            // it retries, is not this dispatch's.
            fetch: crate::task::FetchNote {
                skip: false,
                warning: Some("could not fetch origin: old".into()),
            },
            ..spec()
        });
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        assert_eq!(t.spec.fetch.warning, None);
        assert_eq!(t.spec.label.name.as_deref(), Some("seed"));
        assert_eq!(
            t.spec.label.note.as_deref(),
            Some(crate::task::JOINED_WORKSPACE)
        );
        assert!(
            fake.fetches().is_empty(),
            "a reopened checkout is not a new worktree"
        );
    }

    /// herdr takes `cwd` literally and opens the pane elsewhere when it does
    /// not exist, so `~` is expanded against the machine's home first, for a
    /// workspace and a worktree alike.
    #[tokio::test]
    async fn a_leading_tilde_is_the_machine_s_home() {
        for (home, repo, cwd) in [
            ("/home/fake", "~/work/app", "/home/fake/work/app"),
            ("/home/fake", "~", "/home/fake"),
            // root's home: a bare `~` must stay `/`, not become empty.
            ("/", "~", "/"),
            ("/", "~/work", "/work"),
        ] {
            for worktree in [false, true] {
                let fake = FakeHerdr::new();
                fake.set_home(Some(home));
                let mut t = task(DispatchSpec {
                    repo: Some(repo.into()),
                    worktree,
                    ..spec()
                });
                dispatch(&fake, &mut t, &Agents::default(), None, READY)
                    .await
                    .unwrap();
                let method = if worktree {
                    "worktree.create"
                } else {
                    "workspace.create"
                };
                let req = fake
                    .requests()
                    .into_iter()
                    .find(|r| r.method == method)
                    .unwrap();
                assert_eq!(
                    req.params["cwd"], cwd,
                    "{repo} with home {home} via {method}"
                );
                assert_eq!(
                    t.spec.repo.as_deref(),
                    Some(repo),
                    "the spec keeps what was asked"
                );
            }
        }
    }

    /// herdr opens a pane with no `cwd` wherever its focused pane is, which
    /// put a task with no repo in someone's work checkout.
    /// A task with no repo starts in `~/pastor-tasks`, made
    /// when missing, in a workspace of its own or a pane split into a shared
    /// one: not the home itself, which Claude Code never saves trust for, so
    /// it asked at every start. A machine that cannot tell its home
    /// leaves the choice to herdr, as before.
    #[tokio::test]
    async fn a_task_with_no_repo_starts_in_pastor_tasks() {
        for (home, cwd) in [
            (
                Some("/home/fake"),
                serde_json::json!("/home/fake/pastor-tasks"),
            ),
            (None, Value::Null),
        ] {
            for place in [Place::Repo, Place::Own, Place::Pastor] {
                let fake = FakeHerdr::new();
                fake.set_home(home);
                fake.set_missing_dir("/home/fake/pastor-tasks");
                let mut t = task(DispatchSpec {
                    repo: None,
                    place: place.clone(),
                    ..spec()
                });
                dispatch(&fake, &mut t, &Agents::default(), None, READY)
                    .await
                    .unwrap();
                let req = fake
                    .requests()
                    .into_iter()
                    .find(|r| {
                        r.method == "pane.split"
                            || (r.method == "workspace.create" && r.params["label"] != "pastor")
                    })
                    .unwrap();
                assert_eq!(req.params["cwd"], cwd, "{place:?} with home {home:?}");
                let made: Vec<String> = home
                    .map(|_| "/home/fake/pastor-tasks".to_string())
                    .into_iter()
                    .collect();
                assert_eq!(fake.made_dirs(), made, "{place:?} with home {home:?}");
                assert_eq!(
                    t.spec.cwd,
                    cwd.as_str().map(str::to_string),
                    "{place:?} with home {home:?}"
                );
            }
        }
    }

    /// A folder that cannot be made (a file in the way, no permission) is no
    /// reason to refuse the task: it starts in the home, as before this
    /// folder, and Claude asks for trust there. The home is also what gets
    /// recorded on the task, so `pastor task attach` resumes there too
    /// (`reopen::a_home_fallback_resumes_in_the_home_even_once_the_folder_can_be_made`),
    /// instead of asking `no_repo_dir` again and getting a different answer
    /// once the folder can be made.
    #[tokio::test]
    async fn a_task_with_no_repo_falls_back_to_home_when_the_folder_cannot_be_made() {
        let fake = FakeHerdr::new();
        fake.set_unmakeable_dir("/home/fake/pastor-tasks");
        let mut t = task(DispatchSpec {
            repo: None,
            place: Place::Own,
            ..spec()
        });
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        let req = fake
            .requests()
            .into_iter()
            .find(|r| r.method == "workspace.create" && r.params["label"] != "pastor")
            .unwrap();
        assert_eq!(req.params["cwd"], "/home/fake");
        assert_eq!(t.spec.cwd.as_deref(), Some("/home/fake"));
    }

    /// A paused task resumes in the directory recorded at its first
    /// dispatch, not a fresh answer from `no_repo_dir`: once
    /// `~/pastor-tasks` becomes makeable, asking again would move the agent
    /// away from the home its session was filed under.
    #[tokio::test]
    async fn a_paused_task_resumes_where_it_first_dispatched_even_once_the_folder_can_be_made() {
        let fake = FakeHerdr::new();
        fake.set_unmakeable_dir("/home/fake/pastor-tasks");
        let mut t = task(DispatchSpec {
            repo: None,
            place: Place::Own,
            ..spec()
        });
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        assert_eq!(t.spec.cwd.as_deref(), Some("/home/fake"));

        fake.clear_unmakeable_dir("/home/fake/pastor-tasks");
        let made_before = fake.made_dirs().len();
        resume(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        assert_eq!(
            fake.made_dirs().len(),
            made_before,
            "resume must not ask no_repo_dir again"
        );
        assert_eq!(t.spec.cwd.as_deref(), Some("/home/fake"));
        let req = fake
            .requests()
            .into_iter()
            .rev()
            .find(|r| r.method == "workspace.create" && r.params["label"] != "pastor")
            .unwrap();
        assert_eq!(req.params["cwd"], "/home/fake");
    }

    /// A waiting task goes on after its limit: with its session and the
    /// limit's resume line when it has one, else a new session with its
    /// prompt and the handover paragraph. Either way `waiting_until` goes.
    #[tokio::test]
    async fn a_waiting_task_resumes_its_session_or_starts_again_with_a_handover() {
        let started = |fake: &FakeHerdr| {
            let reqs = fake.requests();
            let start = reqs
                .iter()
                .rev()
                .find(|r| r.method == "agent.start")
                .unwrap();
            let prompt = reqs
                .iter()
                .rev()
                .find(|r| r.method == "agent.prompt")
                .unwrap();
            (
                start.params["args"].to_string(),
                prompt.params["text"].as_str().unwrap().to_string(),
            )
        };
        let session = "0d5bd3a4-2f35-4e1c-9f59-7c1c3a7b8e21";
        let fake = FakeHerdr::new();
        let mut t = task(DispatchSpec {
            session_id: Some(session.into()),
            ..spec()
        });
        t.waiting_until = Some(Utc::now());
        resume(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        let (args, prompt) = started(&fake);
        assert!(
            args.contains("--resume") && args.contains(session),
            "{args}"
        );
        assert_eq!(prompt, crate::task::LIMIT_RESUME_PROMPT);
        assert_eq!(t.waiting_until, None);
        assert_eq!(t.spec.session_id.as_deref(), Some(session));

        let fake = FakeHerdr::new();
        let mut t = task(spec());
        t.waiting_until = Some(Utc::now());
        resume(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        let (args, prompt) = started(&fake);
        assert!(args.contains("--session-id"), "{args}");
        assert!(prompt.starts_with("line one"), "{prompt}");
        assert!(prompt.ends_with(crate::task::LIMIT_HANDOVER), "{prompt}");
        assert_eq!(t.waiting_until, None);
        assert!(t.spec.session_id.is_some(), "a new session to resume next");
    }

    /// An agy task resumes a wait the same way a Claude one does, but on
    /// `--conversation`, the flag agy takes it on, and only when herdr
    /// reported one (`task::resume_flag`); with none recorded, an agy task
    /// gets no flag at all (agy has none to choose its own session with),
    /// and restarts from its prompt and the handover, same as today.
    #[tokio::test]
    async fn an_agy_waiting_task_resumes_its_conversation_or_falls_back_to_a_restart() {
        let started = |fake: &FakeHerdr| {
            let reqs = fake.requests();
            let start = reqs
                .iter()
                .rev()
                .find(|r| r.method == "agent.start")
                .unwrap();
            start.params["args"].clone()
        };
        let conversation = "9f1e2d3c-4b5a-6978-8f90-123456789abc";
        let fake = FakeHerdr::new();
        let mut t = task(DispatchSpec {
            agent: "agy".into(),
            agent_args: vec![],
            session_id: Some(conversation.into()),
            ..spec()
        });
        t.waiting_until = Some(Utc::now());
        resume(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        assert_eq!(
            started(&fake),
            serde_json::json!(["--conversation", conversation])
        );
        assert_eq!(t.spec.session_id.as_deref(), Some(conversation));

        let fake = FakeHerdr::new();
        let mut t = task(DispatchSpec {
            agent: "agy".into(),
            agent_args: vec![],
            ..spec()
        });
        t.waiting_until = Some(Utc::now());
        resume(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        assert_eq!(
            started(&fake),
            serde_json::json!([]),
            "no flag to choose one"
        );
        assert_eq!(t.spec.session_id, None, "nothing for agy to resume next");
    }

    /// A waiting task that moved to another model: in the same session with
    /// the switch line, or, with none, from its prompt with the handover and
    /// the last agent's pane. Either way the stop is taken.
    #[tokio::test]
    async fn a_task_moved_to_another_model_gets_its_handover() {
        let started = |fake: &FakeHerdr| {
            let reqs = fake.requests();
            let find = |m: &str| reqs.iter().rev().find(|r| r.method == m).unwrap().clone();
            (
                find("agent.start").params["args"].to_string(),
                find("agent.prompt").params["text"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            )
        };
        let stop = |handover| {
            Some(Box::new(crate::task::LimitStop {
                agent: "claude".into(),
                model: Some("opus".into()),
                why: "limit".into(),
                line: "You've hit your limit".into(),
                until: Utc::now(),
                tail: "● Half of it is done.\n● You've hit your limit".into(),
                then: crate::task::NextRound::Own,
                waited: None,
                handover: Some(handover),
            }))
        };
        let session = "0d5bd3a4-2f35-4e1c-9f59-7c1c3a7b8e21";
        let fake = FakeHerdr::new();
        let mut t = task(DispatchSpec {
            session_id: Some(session.into()),
            ..spec()
        });
        t.spec.rounds.stop = stop(crate::task::Handover::Session);
        t.waiting_until = Some(Utc::now());
        resume(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        let (args, prompt) = started(&fake);
        assert!(args.contains(session), "{args}");
        assert_eq!(
            prompt,
            crate::task::switch_prompt("claude (opus)", "claude")
        );
        assert_eq!(t.spec.rounds.stop, None);

        let fake = FakeHerdr::new();
        let mut t = task(spec());
        t.spec.rounds.stop = stop(crate::task::Handover::PaneTail);
        t.waiting_until = Some(Utc::now());
        resume(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        let (args, prompt) = started(&fake);
        assert!(args.contains("--session-id"), "{args}");
        assert!(prompt.starts_with("line one"), "{prompt}");
        assert!(
            prompt.ends_with(&crate::task::limit_handover(
                "claude (opus)",
                "● Half of it is done.\n● You've hit your limit"
            )),
            "{prompt}"
        );
        assert!(prompt.contains("git status"), "{prompt}");
        assert_eq!(t.spec.rounds.stop, None);
    }

    /// An agent that dies at its start leaves the end of its pane on the
    /// error, for a usage limit to be read from.
    #[tokio::test]
    async fn an_agent_that_exits_on_start_leaves_its_pane_tail() {
        for listed in [false, true] {
            let fake = FakeHerdr::new();
            if listed {
                fake.exit_agents_listed(true);
            } else {
                fake.exit_agents_on_start(true);
            }
            fake.set_exit_screen("You've hit your limit · resets 3am\n");
            let mut t = task(spec());
            let err = dispatch(&fake, &mut t, &Agents::default(), None, READY)
                .await
                .unwrap_err();
            let DispatchError::Exited { message, tail } = &err else {
                panic!("{err:?}");
            };
            assert!(message.contains("exited before"), "{message}");
            assert!(tail.contains("You've hit your limit"), "{tail}");
            assert_eq!(t.error.as_deref(), Some(message.as_str()));
        }
    }

    /// Only a resume keeps the directory recorded on the task: a fresh
    /// dispatch of a task with no repo asks `no_repo_dir`, whatever `cwd`
    /// the task already carries.
    #[tokio::test]
    async fn a_fresh_dispatch_with_no_repo_asks_again_whatever_cwd_it_carries() {
        let fake = FakeHerdr::new();
        let mut t = task(DispatchSpec {
            repo: None,
            place: Place::Own,
            cwd: Some("/home/fake/elsewhere".into()),
            ..spec()
        });
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        assert_eq!(t.spec.cwd.as_deref(), Some("/home/fake/pastor-tasks"));
        let req = fake
            .requests()
            .into_iter()
            .find(|r| r.method == "workspace.create" && r.params["label"] != "pastor")
            .unwrap();
        assert_eq!(req.params["cwd"], "/home/fake/pastor-tasks");
    }

    /// Where `~` cannot be resolved the task fails up front with a reason,
    /// instead of herdr quietly opening the pane in some other directory.
    #[tokio::test]
    async fn an_unresolvable_tilde_fails_the_task_before_herdr_is_asked() {
        for (home, repo, needle) in [
            (None, "~/work", "cannot tell the home directory"),
            (Some("/home/fake"), "~bob/work", "not ~user"),
        ] {
            let fake = FakeHerdr::new();
            fake.set_home(home);
            let mut t = task(DispatchSpec {
                repo: Some(repo.into()),
                ..spec()
            });
            let err = dispatch(&fake, &mut t, &Agents::default(), None, READY)
                .await
                .unwrap_err();
            assert!(!err.is_transport(), "the machine is fine: {err}");
            assert_eq!(t.state, TaskState::Failed);
            assert!(
                t.error.as_deref().unwrap().contains(needle),
                "{:?}",
                t.error
            );
            assert!(
                !fake
                    .requests()
                    .iter()
                    .any(|r| r.method == "workspace.create"),
                "nothing was created"
            );
        }
    }

    /// herdr reports a managed agent as `unknown` and refuses prompts while it
    /// is still launching. Dispatch waits that out instead of failing the task:
    /// this is the live defect (`agent_not_ready: agent t-1 is not an active
    /// named agent` on a perfectly healthy machine).
    #[tokio::test]
    async fn dispatch_waits_for_a_launching_agent() {
        let fake = FakeHerdr::new();
        fake.set_ready_after(Duration::from_millis(300));
        let mut t = task(spec());
        let out = dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        assert_eq!(out, DispatchOutcome::Running);
        assert_eq!(t.state, TaskState::Running);
        let methods: Vec<String> = fake.requests().into_iter().map(|r| r.method).collect();
        assert!(
            methods.iter().filter(|m| *m == "agent.list").count() >= 2,
            "expected repeated agent.list polling, got {methods:?}"
        );
        assert_eq!(
            methods.last().map(String::as_str),
            Some("agent.prompt"),
            "the prompt is sent last, once the agent is up: {methods:?}"
        );
    }

    /// The same `agent_not_ready` code with the opposite meaning: the agent is
    /// gone from `agent.list`, so it exited (the usual cause is the agent binary
    /// missing on that machine). Fail now, do not wait out the bound.
    #[tokio::test]
    async fn an_agent_that_exits_on_start_fails_immediately() {
        let fake = FakeHerdr::new();
        fake.exit_agents_on_start(true);
        let mut t = task(spec());
        t.machine = Some("pi-1".into());
        let started = Instant::now();
        let err = dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "waited {:?} for an agent that was already gone",
            started.elapsed()
        );
        let message = err.to_string();
        assert!(
            message.contains("exited before accepting a prompt"),
            "{message}"
        );
        assert!(message.contains("claude"), "{message}");
        assert!(message.contains("pi-1"), "{message}");
        assert!(!err.is_transport(), "a dead agent is not a dead machine");
        assert_eq!(t.state, TaskState::Failed);
        assert_eq!(t.error.as_deref(), Some(message.as_str()));
    }

    /// The agent never comes up within the bound. The task fails, the machine is
    /// untouched, and the message says an agent may still be sitting there.
    #[tokio::test]
    async fn an_agent_that_never_becomes_ready_fails_with_the_bound() {
        let fake = FakeHerdr::new();
        fake.set_ready_after(Duration::from_secs(30));
        let mut t = task(spec());
        t.machine = Some("pi-1".into());
        let err = dispatch(
            &fake,
            &mut t,
            &Agents::default(),
            None,
            Duration::from_millis(200),
        )
        .await
        .unwrap_err();
        let message = err.to_string();
        assert!(message.contains("not ready after"), "{message}");
        assert!(message.contains("may remain on pi-1"), "{message}");
        assert!(!err.is_transport(), "a slow agent is not a dead machine");
        assert_eq!(t.state, TaskState::Failed);
        assert!(t.pane_id.is_some(), "pane is kept for inspection");
    }

    /// An agent stuck on its startup question (Claude's folder trust dialog)
    /// is `blocked` with `launch_pending` still set, and herdr answers
    /// `agent_not_ready` to prompts until a human clears it. That is a blocked
    /// task, not an agent that never came up.
    #[tokio::test]
    async fn an_agent_blocked_while_launching_blocks_the_task() {
        let fake = FakeHerdr::new();
        // Launching for longer than the readiness bound: only the blocked
        // status can end dispatch in time.
        fake.set_ready_after(READY * 2);
        let watcher = fake.clone();
        tokio::spawn(async move {
            loop {
                if let Some(a) = watcher.agents().first() {
                    watcher.set_status(&a.pane_id, AgentStatus::Blocked);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let mut t = task(spec());
        let started = Instant::now();
        let out = dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        assert_eq!(out, DispatchOutcome::Blocked);
        assert!(started.elapsed() < READY, "waited out the readiness bound");
        assert_eq!(t.state, TaskState::Blocked);
        assert!(t.prompt_pending, "the agent has not seen the prompt");
        assert!(
            !fake.requests().iter().any(|r| r.method == "agent.prompt"),
            "a blocked agent is not prompted"
        );
    }

    /// An agent that is up but waiting for a human still blocks the task.
    #[tokio::test]
    async fn a_blocked_agent_blocks_the_task() {
        let fake = FakeHerdr::new();
        // The agent has to exist before it can be blocked, and dispatch has to
        // still be polling when it is: block it from a watcher during the
        // launch window.
        fake.set_ready_after(Duration::from_millis(150));
        let watcher = fake.clone();
        tokio::spawn(async move {
            loop {
                if let Some(a) = watcher.agents().first() {
                    watcher.set_status(&a.pane_id, AgentStatus::Blocked);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let mut t = task(spec());
        let out = dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        assert_eq!(out, DispatchOutcome::Blocked);
        assert_eq!(t.state, TaskState::Blocked);
        assert!(t.pane_id.is_some(), "pane is kept for inspection");
    }

    #[tokio::test]
    async fn start_failures_are_failed() {
        let fake = FakeHerdr::new();
        fake.set_start_behaviour(StartBehaviour::Fail("unsupported_agent_kind".into()));
        let mut t = task(spec());
        let err = dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some("unsupported_agent_kind"));
        assert!(
            !err.is_transport(),
            "a rejected start is not a dead machine"
        );
        assert_eq!(t.state, TaskState::Failed);
        assert!(
            t.error
                .as_deref()
                .unwrap()
                .contains("unsupported_agent_kind")
        );
        assert!(
            t.workspace_id.is_some(),
            "created workspace is recorded even on failure"
        );
        assert!(!fake.requests().iter().any(|r| r.method == "agent.prompt"));
        // Claude never ran, so there is no session for attach to resume.
        let start = fake
            .requests()
            .into_iter()
            .find(|r| r.method == "agent.start");
        assert!(
            start.unwrap().params["args"]
                .to_string()
                .contains("--session-id")
        );
        assert_eq!(t.spec.session_id, None);
    }

    /// herdr answers `agent_pane_busy` while the new pane's shell is still
    /// starting; on the real fleet the same dispatch works a moment later
    /// (t-42, t-50). Dispatch retries `agent.start` instead of failing.
    #[tokio::test]
    async fn a_busy_pane_is_retried_until_the_shell_is_up() {
        let fake = FakeHerdr::new();
        fake.set_pane_busy_for(2);
        let mut t = task(spec());
        let out = dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        assert_eq!(out, DispatchOutcome::Running);
        assert_eq!(t.state, TaskState::Running);
        let reqs = fake.requests();
        let starts = reqs.iter().filter(|r| r.method == "agent.start").count();
        assert_eq!(starts, 3, "two busy answers, then the start that worked");
        assert!(reqs.iter().any(|r| r.method == "agent.prompt"));
    }

    /// A pane that stays busy past the last attempt fails the task with
    /// herdr's own words and the attempt count, and nothing is prompted.
    #[tokio::test]
    async fn a_pane_busy_past_the_last_attempt_fails_the_task() {
        let fake = FakeHerdr::new();
        fake.set_pane_busy_for(PANE_BUSY_ATTEMPTS + 1);
        let mut t = task(spec());
        let err = dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some("agent_pane_busy"));
        assert!(!err.is_transport(), "a busy pane is not a dead machine");
        let message = err.to_string();
        assert!(
            message.contains("agent target pane w1:p1 is not an available shell"),
            "{message}"
        );
        assert!(
            message.ends_with(&format!("after {PANE_BUSY_ATTEMPTS} attempts")),
            "{message}"
        );
        assert_eq!(t.state, TaskState::Failed);
        assert_eq!(t.error.as_deref(), Some(message.as_str()));
        let reqs = fake.requests();
        let starts = reqs.iter().filter(|r| r.method == "agent.start").count();
        assert_eq!(starts, PANE_BUSY_ATTEMPTS as usize);
        assert!(!reqs.iter().any(|r| r.method == "agent.prompt"));
    }

    /// Only `agent_pane_busy` is retried; any other start error fails at once.
    #[tokio::test]
    async fn other_start_errors_are_not_retried() {
        let fake = FakeHerdr::new();
        fake.set_start_behaviour(StartBehaviour::Fail("agent_name_taken".into()));
        let mut t = task(spec());
        let err = dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some("agent_name_taken"));
        let starts = fake
            .requests()
            .iter()
            .filter(|r| r.method == "agent.start")
            .count();
        assert_eq!(starts, 1);
    }

    /// The pane is still listed but the agent process died before becoming
    /// interactive: neither launch flag set, status idle. herdr's own
    /// `agent start --wait` fails here; so must dispatch, at once, instead of
    /// prompting a corpse until the bound elapses.
    #[tokio::test]
    async fn an_agent_that_exits_but_stays_listed_fails_immediately() {
        let fake = FakeHerdr::new();
        fake.exit_agents_listed(true);
        let mut t = task(spec());
        t.machine = Some("pi-1".into());
        let started = Instant::now();
        let err = dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "waited {:?} for an agent that had already exited",
            started.elapsed()
        );
        let message = err.to_string();
        assert!(
            message.contains("exited before becoming interactive"),
            "{message}"
        );
        assert!(message.contains("pi-1"), "{message}");
        assert!(!err.is_transport());
        assert_eq!(t.state, TaskState::Failed);
        assert!(
            !fake.requests().iter().any(|r| r.method == "agent.prompt"),
            "a dead agent must not be prompted"
        );
    }

    #[tokio::test]
    async fn worktree_without_repo_fails_before_calling_herdr() {
        let fake = FakeHerdr::new();
        let mut t = task(DispatchSpec {
            worktree: true,
            repo: None,
            ..spec()
        });
        let err = dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                DispatchError::Call(CallError::Herdr(HerdrError::Protocol(_)))
            ),
            "{err:?}"
        );
        assert!(
            !err.is_transport(),
            "a task pastor itself rejects must not mark the machine lost"
        );
        assert_eq!(t.state, TaskState::Failed);
        assert!(fake.requests().is_empty());
    }
    fn methods(fake: &FakeHerdr) -> Vec<String> {
        fake.requests().into_iter().map(|r| r.method).collect()
    }

    /// A fix round in a PR's worktree: `--repo` is a checkout a workspace
    /// already shows, so the agent gets a pane there, not a workspace of its
    /// own. The pane is the task's; the workspace stays the user's.
    #[tokio::test]
    async fn place_repo_puts_a_task_in_the_workspace_showing_its_repo() {
        for repo in ["/srv/app", "/srv/app/"] {
            let fake = FakeHerdr::new();
            fake.open_user_workspace("other", Some("/srv/other"));
            let ws = fake.open_user_workspace("app", Some("/srv/app"));
            let mut t = task(DispatchSpec {
                repo: Some(repo.into()),
                ..spec()
            });
            dispatch(&fake, &mut t, &Agents::default(), None, READY)
                .await
                .unwrap();
            assert_eq!(t.state, TaskState::Running, "{repo}");
            assert_eq!(t.workspace_id.as_deref(), Some(ws.as_str()), "{repo}");
            let pane = t.pane_id.clone().unwrap();
            assert_eq!(fake.panes(&ws), vec![format!("{ws}:p1"), pane.clone()]);
            assert!(!methods(&fake).iter().any(|m| m.ends_with(".create")));
            let split = fake
                .requests()
                .into_iter()
                .find(|r| r.method == "pane.split")
                .unwrap();
            assert_eq!(split.params["target_pane_id"], format!("{ws}:p1"));
            assert_eq!(split.params["cwd"], repo);
            assert_eq!(fake.pane_env(&pane)["PASTOR_TASK"], "t-7");
        }
    }

    /// Nothing shows the repo, or there is no repo: a workspace of its own,
    /// as before.
    #[tokio::test]
    async fn place_repo_makes_a_workspace_when_none_shows_the_repo() {
        for repo in [Some("/srv/app"), None] {
            let fake = FakeHerdr::new();
            fake.open_user_workspace("other", Some("/srv/other"));
            let mut t = task(DispatchSpec {
                repo: repo.map(str::to_string),
                ..spec()
            });
            dispatch(&fake, &mut t, &Agents::default(), None, READY)
                .await
                .unwrap();
            let create = fake
                .requests()
                .into_iter()
                .find(|r| r.method == "workspace.create")
                .unwrap();
            assert_eq!(create.params["label"], "home/t-7");
            assert_eq!(t.workspace_id.as_deref(), Some("w2"), "{repo:?}");
            assert!(!methods(&fake).contains(&"pane.split".to_string()));
        }
    }

    /// A worktree task gets a new worktree, which herdr already shows
    /// under the repo's workspace, even with the repo open in one.
    #[tokio::test]
    async fn place_repo_keeps_a_worktree_task_in_its_worktree() {
        let fake = FakeHerdr::new();
        let ws = fake.open_user_workspace("app", Some("/srv/app"));
        let mut t = task(DispatchSpec {
            worktree: true,
            ..spec()
        });
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        assert!(methods(&fake).contains(&"worktree.create".to_string()));
        assert_ne!(t.workspace_id.as_deref(), Some(ws.as_str()));
        assert_eq!(fake.panes(&ws), vec![format!("{ws}:p1")]);
        assert!(t.spec.checkout.is_some());
    }

    /// `own`: a workspace named after the task, whatever already shows the
    /// repo.
    #[tokio::test]
    async fn place_own_always_makes_a_workspace() {
        let fake = FakeHerdr::new();
        let ws = fake.open_user_workspace("app", Some("/srv/app"));
        let mut t = task(DispatchSpec {
            place: Place::Own,
            ..spec()
        });
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        assert_ne!(t.workspace_id.as_deref(), Some(ws.as_str()));
        assert_eq!(fake.panes(&ws), vec![format!("{ws}:p1")]);
        assert!(!methods(&fake).contains(&"workspace.list".to_string()));
        let create = fake
            .requests()
            .into_iter()
            .find(|r| r.method == "workspace.create")
            .unwrap();
        assert_eq!(create.params["label"], "home/t-7");
    }

    /// `pastor`: the first task makes the machine's `pastor` workspace, the
    /// next one joins it. Each gets its own pane; the workspace's first pane
    /// stays, so no task's close ends it.
    #[tokio::test]
    async fn place_pastor_shares_one_workspace_made_on_first_use() {
        let fake = FakeHerdr::new();
        let mut one = task(DispatchSpec {
            place: Place::Pastor,
            ..spec()
        });
        dispatch(&fake, &mut one, &Agents::default(), None, READY)
            .await
            .unwrap();
        let ws = one.workspace_id.clone().unwrap();
        let mut two = task(DispatchSpec {
            place: Place::Pastor,
            repo: None,
            ..spec()
        });
        two.id = 8;
        dispatch(&fake, &mut two, &Agents::default(), None, READY)
            .await
            .unwrap();
        assert_eq!(two.workspace_id.as_deref(), Some(ws.as_str()));
        let creates: Vec<_> = fake
            .requests()
            .into_iter()
            .filter(|r| r.method == "workspace.create")
            .collect();
        assert_eq!(creates.len(), 1);
        assert_eq!(creates[0].params["label"], "pastor");
        assert_eq!(creates[0].params["cwd"], serde_json::Value::Null);
        assert_eq!(
            fake.panes(&ws),
            vec![
                format!("{ws}:p1"),
                one.pane_id.clone().unwrap(),
                two.pane_id.clone().unwrap()
            ]
        );
        assert_eq!(
            fake.pane_env(one.pane_id.as_deref().unwrap())["PASTOR_TASK"],
            "t-7"
        );
        assert_eq!(
            fake.pane_env(two.pane_id.as_deref().unwrap())["PASTOR_TASK"],
            "t-8"
        );
    }

    /// A worktree task placed in `pastor` still gets its worktree on disk,
    /// recorded as its checkout; the workspace herdr opened on it goes, and
    /// the agent works in the checkout from a pane in `pastor`.
    #[tokio::test]
    async fn place_pastor_still_makes_the_worktree() {
        let fake = FakeHerdr::new();
        let mut t = task(DispatchSpec {
            place: Place::Pastor,
            worktree: true,
            ..spec()
        });
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        let checkout = t.spec.checkout.clone().expect("checkout recorded");
        assert_eq!(checkout.branch, "pastor/t-7");
        let ws = t.workspace_id.clone().unwrap();
        assert_eq!(
            fake.workspaces(),
            vec![ws.clone()],
            "the worktree's workspace is closed"
        );
        let split = fake
            .requests()
            .into_iter()
            .find(|r| r.method == "pane.split")
            .unwrap();
        assert_eq!(split.params["cwd"], checkout.path.as_str());
        assert!(fake.panes(&ws).contains(t.pane_id.as_ref().unwrap()));
        assert_eq!(fake.worktree_list("/srv/app").await.unwrap().len(), 1);
    }

    /// `pane:<workspace>`: a pane in that workspace, or refused before
    /// anything is made when the machine has none by that name.
    #[tokio::test]
    async fn place_pane_uses_the_named_workspace_or_refuses() {
        let fake = FakeHerdr::new();
        let ws = fake.open_user_workspace("work", None);
        let mut t = task(DispatchSpec {
            place: Place::Pane("work".into()),
            ..spec()
        });
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        assert_eq!(t.workspace_id.as_deref(), Some(ws.as_str()));
        assert_eq!(fake.panes(&ws).len(), 2);

        for worktree in [false, true] {
            let fake = FakeHerdr::new();
            fake.open_user_workspace("work", None);
            let mut t = task(DispatchSpec {
                place: Place::Pane("play".into()),
                worktree,
                ..spec()
            });
            let err = dispatch(&fake, &mut t, &Agents::default(), None, READY)
                .await
                .unwrap_err();
            assert!(!err.is_transport());
            assert_eq!(t.state, TaskState::Failed);
            assert!(
                err.to_string().contains("no workspace named play on pi-1"),
                "{err}"
            );
            let made = methods(&fake);
            assert!(
                !made
                    .iter()
                    .any(|m| m.ends_with(".create") || m == "pane.split"),
                "{made:?}"
            );
        }
    }

    /// A named workspace that closes between `workspace.list` and
    /// `pane.list` is looked up once more, and never swapped for a
    /// workspace of the task's own: a blip passes, a workspace gone for
    /// good or still gone the second time fails the task.
    #[tokio::test]
    async fn place_pane_never_falls_back_to_a_workspace_of_its_own() {
        // A blip: the second look finds it.
        let fake = FakeHerdr::new();
        let ws = fake.open_user_workspace("work", None);
        fake.vanish_on_pane_list(1, false);
        let mut t = task(DispatchSpec {
            place: Place::Pane("work".into()),
            ..spec()
        });
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        assert_eq!(t.workspace_id.as_deref(), Some(ws.as_str()));

        for (n, close, why) in [
            (1, true, "no workspace named work"),
            (2, false, "closed while"),
        ] {
            let fake = FakeHerdr::new();
            fake.open_user_workspace("work", None);
            fake.vanish_on_pane_list(n, close);
            let mut t = task(DispatchSpec {
                place: Place::Pane("work".into()),
                ..spec()
            });
            let err = dispatch(&fake, &mut t, &Agents::default(), None, READY)
                .await
                .unwrap_err();
            assert!(!err.is_transport());
            assert_eq!(t.state, TaskState::Failed);
            assert!(err.to_string().contains(why), "{err}");
            let made = methods(&fake);
            assert!(
                !made
                    .iter()
                    .any(|m| m.ends_with(".create") || m == "pane.split"),
                "{made:?}"
            );
        }
    }

    /// Only `workspace_not_found` from `pane.list` means the workspace
    /// closed and is worth a second look; any other failure fails the
    /// task as it is, with no second `pane.list`.
    #[tokio::test]
    async fn a_pane_list_failure_other_than_not_found_is_not_retried() {
        let fake = FakeHerdr::new();
        fake.open_user_workspace("work", None);
        fake.set_malformed_reply("pane.list");
        let mut t = task(DispatchSpec {
            place: Place::Pane("work".into()),
            ..spec()
        });
        let err = dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap_err();
        assert!(!err.to_string().contains("closed while"), "{err}");
        let made = methods(&fake);
        assert_eq!(
            made.iter().filter(|m| *m == "pane.list").count(),
            1,
            "{made:?}"
        );
        assert!(!made.iter().any(|m| m == "agent.start"), "{made:?}");
    }

    /// A paused worktree task resumes in its own checkout through
    /// `worktree.open`, never a new one. A checkout that is gone fails the
    /// task with a reason; any other `worktree.open` failure fails it as
    /// it is.
    #[tokio::test]
    async fn a_worktree_task_resumes_only_in_its_own_checkout() {
        let worktree_task = || {
            task(DispatchSpec {
                worktree: true,
                branch: Some("pastor/k1".into()),
                ..spec()
            })
        };

        let fake = FakeHerdr::new();
        let mut t = worktree_task();
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        assert_eq!(t.spec.checkout.as_ref().unwrap().branch, "pastor/k1");
        let before = methods(&fake).len();
        resume(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        let again = methods(&fake).split_off(before);
        assert!(again.iter().any(|m| m == "worktree.open"), "{again:?}");
        assert!(!again.iter().any(|m| m == "worktree.create"), "{again:?}");

        // Gone: herdr answers `worktree_not_found`.
        let fake = FakeHerdr::new();
        let mut t = worktree_task();
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        t.spec.checkout.as_mut().unwrap().branch = "pastor/gone".into();
        let err = resume(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("nowhere to resume"), "{err}");

        // Any other failure is not reported as a gone checkout.
        let fake = FakeHerdr::new();
        let mut t = worktree_task();
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        fake.set_malformed_reply("worktree.open");
        let err = resume(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap_err();
        assert!(!err.to_string().contains("nowhere to resume"), "{err}");
    }

    /// A gate that records each place, with whether herdr had started an
    /// agent yet, and answers `answer`.
    struct Places {
        fake: FakeHerdr,
        answer: Result<(), String>,
        calls: std::sync::Mutex<Vec<(String, String, Value, bool)>>,
    }

    impl crate::bundle::workdir_task::Gate for Places {
        /// A set's document, or for a bundle `{"bundle": id, "kind": kind,
        /// "flock": flock}`; a bundle named `empty` has nothing to place.
        fn place<'a>(
            &'a self,
            machine: &'a str,
            dir: &'a str,
            what: &'a crate::bundle::workdir_task::Placing,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, String>> + Send + 'a>>
        {
            use crate::bundle::workdir_task::Placing;
            let started = self
                .fake
                .requests()
                .iter()
                .any(|r| r.method == "agent.start");
            let (seen, placed) = match what {
                Placing::Set(set) => (set.clone(), true),
                Placing::Rendered { set, render } => {
                    let mut seen = set.clone();
                    seen["rendered"] = render.profile.clone().into();
                    (seen, true)
                }
                Placing::Bundle { id, kind, flock } => (
                    serde_json::json!({"bundle": id, "kind": kind, "flock": flock}),
                    id != "empty",
                ),
            };
            self.calls
                .lock()
                .unwrap()
                .push((machine.to_string(), dir.to_string(), seen, started));
            let answer = self.answer.clone().map(|()| placed);
            Box::pin(async move { answer })
        }

        fn forget(&self, _machine: &str, _dir: &str) {}
    }

    fn places(fake: &FakeHerdr, answer: Result<(), String>) -> Arc<Places> {
        Arc::new(Places {
            fake: fake.clone(),
            answer,
            calls: Default::default(),
        })
    }

    fn with_set(spec: DispatchSpec) -> DispatchSpec {
        DispatchSpec {
            workdir: Some(Box::new(crate::task::TaskWorkdir {
                set: serde_json::json!({"id": "reviewer", "kind": "claude", "files": [
                    {"path": ".claude/agents/checker.md", "content": "Check.\n"}]}),
                placed_in: None,
                render: None,
            })),
            ..spec
        }
    }

    async fn dispatch_placing(
        fake: &FakeHerdr,
        t: &mut Task,
        gate: &Arc<Places>,
        resume_it: bool,
    ) -> Result<DispatchOutcome, DispatchError> {
        let handle = crate::bundle::workdir_task::GateHandle(Some(
            gate.clone() as Arc<dyn crate::bundle::workdir_task::Gate>
        ));
        let provision = Provision {
            workdirs: Some(&handle),
            ..Default::default()
        };
        if resume_it {
            resume_with(fake, t, &Agents::default(), None, READY, provision).await
        } else {
            dispatch_with(fake, t, &Agents::default(), None, READY, provision).await
        }
    }

    /// A task's set goes into the folder its agent starts in, before the
    /// agent starts, and again on each start: here a dispatch, then a
    /// resume. The place is recorded on the task.
    #[tokio::test]
    async fn a_tasks_workdir_set_is_placed_before_each_start() {
        let fake = FakeHerdr::new();
        let gate = places(&fake, Ok(()));
        let mut t = task(with_set(spec()));
        dispatch_placing(&fake, &mut t, &gate, false).await.unwrap();
        {
            let calls = gate.calls.lock().unwrap();
            assert_eq!(calls.len(), 1);
            let (machine, dir, set, started) = &calls[0];
            assert_eq!(machine, "pi-1");
            assert_eq!(dir, "/srv/app");
            assert_eq!(set["id"], "reviewer");
            assert!(!started, "placed before agent.start");
        }
        assert_eq!(
            t.spec.workdir.as_ref().unwrap().placed_in.as_deref(),
            Some("/srv/app")
        );
        dispatch_placing(&fake, &mut t, &gate, true).await.unwrap();
        assert_eq!(gate.calls.lock().unwrap().len(), 2, "again on the resume");
    }

    /// A set that names no profile takes the task's; a set for another kind
    /// fails the start, as does a gate that refuses or a machine with none.
    /// Nothing starts then.
    #[tokio::test]
    async fn a_workdir_set_that_cannot_be_placed_fails_the_start() {
        let fake = FakeHerdr::new();
        let gate = places(&fake, Ok(()));
        let mut profiled = with_set(spec());
        profiled.agent_source = Some(Box::new(crate::task::AgentSource {
            profile: Some("review".into()),
            ..Default::default()
        }));
        let mut t = task(profiled);
        dispatch_placing(&fake, &mut t, &gate, false).await.unwrap();
        assert_eq!(gate.calls.lock().unwrap()[0].2["profile"], "review");

        let refused = |answer: Result<(), String>, spec: DispatchSpec| async move {
            let fake = FakeHerdr::new();
            let gate = places(&fake, answer);
            let mut t = task(spec);
            let err = dispatch_placing(&fake, &mut t, &gate, false)
                .await
                .unwrap_err()
                .to_string();
            assert_eq!(t.state, TaskState::Failed);
            assert!(
                !fake.requests().iter().any(|r| r.method == "agent.start"),
                "no agent: {err}"
            );
            err
        };
        let err = refused(Err("workdir_failed: conflict x".into()), with_set(spec())).await;
        assert_eq!(err, "workdir_failed: conflict x");
        let mut agy = with_set(spec());
        agy.workdir.as_mut().unwrap().set["kind"] = "agy".into();
        let err = refused(Ok(()), agy).await;
        assert!(err.starts_with("workdir_failed: "), "{err}");
        assert!(err.contains("workdir_kind_mismatch"), "{err}");

        // A machine with no gate places nothing, and says so.
        let fake = FakeHerdr::new();
        let mut t = task(with_set(spec()));
        let err = dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not place workdir sets"), "{err}");
    }

    fn with_bundles(spec: DispatchSpec, ids: &[&str]) -> DispatchSpec {
        DispatchSpec {
            workdir_bundles: ids
                .iter()
                .map(|id| crate::task::WorkdirBundle::named(id))
                .collect(),
            ..spec
        }
    }

    /// A worktree task's spec: its checkout is its own folder.
    fn in_worktree(spec: DispatchSpec, branch: &str) -> DispatchSpec {
        DispatchSpec {
            worktree: true,
            branch: Some(branch.into()),
            ..spec
        }
    }

    /// A task's stored workdir bundles go into its own worktree first, in
    /// list order and for its agent's kind and flock, then its own set, all
    /// before the agent starts. A bundle with nothing for the kind places
    /// nothing and is not a skip.
    #[tokio::test]
    async fn a_tasks_workdir_bundles_are_placed_before_its_own_set() {
        let fake = FakeHerdr::new();
        let gate = places(&fake, Ok(()));
        let mut t = task(with_bundles(
            in_worktree(with_set(spec()), "pastor/k1"),
            &["repo-notes", "empty"],
        ));
        t.flock = Some("ops".into());
        dispatch_placing(&fake, &mut t, &gate, false).await.unwrap();
        let calls = gate.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 3, "{calls:?}");
        assert_eq!(
            calls[0].2,
            serde_json::json!({"bundle": "repo-notes", "kind": "claude", "flock": "ops"})
        );
        assert_eq!(calls[1].2["bundle"], "empty");
        assert_eq!(calls[2].2["id"], "reviewer");
        assert!(
            calls
                .iter()
                .all(|c| c.1 == "/fake/worktrees/pastor-k1" && !c.3),
            "{calls:?}"
        );
        assert!(t.spec.workdir_bundles.iter().all(|b| b.skipped.is_none()));

        // Bundles alone, with no set of its own.
        let fake = FakeHerdr::new();
        let gate = places(&fake, Ok(()));
        let mut t = task(with_bundles(
            in_worktree(spec(), "pastor/k2"),
            &["repo-notes"],
        ));
        dispatch_placing(&fake, &mut t, &gate, false).await.unwrap();
        assert_eq!(gate.calls.lock().unwrap().len(), 1);
    }

    /// A stored bundle that cannot be placed, such as one that clashes with
    /// another set in the folder, fails the start like the task's own set:
    /// nothing after it is placed, and no agent starts.
    #[tokio::test]
    async fn a_workdir_bundle_that_cannot_be_placed_fails_the_start() {
        let fake = FakeHerdr::new();
        let gate = places(
            &fake,
            Err("workdir_failed: workdir_conflict .claude/agents/checker.md".into()),
        );
        let mut t = task(with_bundles(
            in_worktree(with_set(spec()), "pastor/k1"),
            &["repo-notes"],
        ));
        let err = dispatch_placing(&fake, &mut t, &gate, false)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("workdir_conflict"), "{err}");
        assert_eq!(gate.calls.lock().unwrap().len(), 1, "stopped at the first");
        assert_eq!(t.state, TaskState::Failed);
        assert!(!fake.requests().iter().any(|r| r.method == "agent.start"));
    }

    /// A stored bundle never decides where a task runs: on a machine that
    /// places no sets, or for an agent kind pastor places none for, the
    /// task starts without its bundles, each marked skipped with why.
    #[tokio::test]
    async fn workdir_bundles_are_skipped_where_no_set_is_placed() {
        let fake = FakeHerdr::new();
        let mut t = task(with_bundles(spec(), &["repo-notes"]));
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        let why = t.spec.workdir_bundles[0].skipped.clone().unwrap();
        assert!(why.contains("does not place workdir sets"), "{why}");
        assert!(fake.requests().iter().any(|r| r.method == "agent.start"));

        let fake = FakeHerdr::new();
        let gate = places(&fake, Ok(()));
        let mut t = task(with_bundles(
            DispatchSpec {
                agent: "codex".into(),
                ..spec()
            },
            &["repo-notes"],
        ));
        dispatch_placing(&fake, &mut t, &gate, false).await.unwrap();
        assert!(gate.calls.lock().unwrap().is_empty());
        let why = t.spec.workdir_bundles[0].skipped.clone().unwrap();
        assert!(why.contains("no workdir sets for codex agents"), "{why}");
    }

    /// Stored bundles go in a task's own worktree only. Every task of a
    /// flock places a bundle under the bundle's own id, so in a folder
    /// tasks share (a repo task with no worktree, the no-repo folder, a
    /// checkout pastor did not open) each start would replace the last
    /// one's version: a claude task's files would go when an agy task
    /// started beside it. There they are skipped with why, and the task's
    /// own set is still placed.
    #[tokio::test]
    async fn workdir_bundles_go_only_in_the_tasks_own_worktree() {
        let fake = FakeHerdr::new();
        let gate = places(&fake, Ok(()));
        let mut claude = task(with_bundles(with_set(spec()), &["repo-notes"]));
        let mut agy = task(with_bundles(
            DispatchSpec {
                agent: "agy".into(),
                ..spec()
            },
            &["repo-notes"],
        ));
        agy.id = 8;
        dispatch_placing(&fake, &mut claude, &gate, false)
            .await
            .unwrap();
        dispatch_placing(&fake, &mut agy, &gate, false)
            .await
            .unwrap();
        let calls = gate.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1, "only claude's own set: {calls:?}");
        assert_eq!(calls[0].2["id"], "reviewer");
        for t in [&claude, &agy] {
            let why = t.spec.workdir_bundles[0].skipped.clone().unwrap();
            assert!(why.starts_with("shared folder (/srv/app)"), "{why}");
        }

        // A task with no repo starts in the folder every such task shares.
        let fake = FakeHerdr::new();
        let gate = places(&fake, Ok(()));
        let mut t = task(with_bundles(
            DispatchSpec {
                repo: None,
                ..spec()
            },
            &["repo-notes"],
        ));
        dispatch_placing(&fake, &mut t, &gate, false).await.unwrap();
        assert!(gate.calls.lock().unwrap().is_empty());
        let why = t.spec.workdir_bundles[0].skipped.clone().unwrap();
        assert!(why.starts_with("shared folder ("), "{why}");

        // A checkout pastor did not open is someone else's folder too.
        let fake = FakeHerdr::new();
        let gate = places(&fake, Ok(()));
        fake.worktree_create("/srv/app", "pastor/k1", "seed")
            .await
            .unwrap();
        let path = fake
            .worktree_list("/srv/app")
            .await
            .unwrap()
            .into_iter()
            .find(|w| w.branch.as_deref() == Some("pastor/k1"))
            .unwrap()
            .path;
        let mut t = task(with_bundles(
            DispatchSpec {
                worktree: true,
                reopen: Some(Box::new(Reopen {
                    branch: "pastor/k1".into(),
                    path: path.clone(),
                    agent: "someone-else".into(),
                })),
                ..spec()
            },
            &["repo-notes"],
        ));
        dispatch_placing(&fake, &mut t, &gate, false).await.unwrap();
        assert!(t.spec.checkout.as_ref().unwrap().already_open);
        assert!(gate.calls.lock().unwrap().is_empty());
        let why = t.spec.workdir_bundles[0].skipped.clone().unwrap();
        assert!(why.starts_with(&format!("shared folder ({path})")), "{why}");

        // Two worktree tasks each get the bundle in their own checkout.
        let fake = FakeHerdr::new();
        let gate = places(&fake, Ok(()));
        let mut one = task(with_bundles(
            in_worktree(spec(), "pastor/k1"),
            &["repo-notes"],
        ));
        let mut two = task(with_bundles(
            in_worktree(
                DispatchSpec {
                    agent: "agy".into(),
                    ..spec()
                },
                "pastor/k2",
            ),
            &["repo-notes"],
        ));
        two.id = 8;
        dispatch_placing(&fake, &mut one, &gate, false)
            .await
            .unwrap();
        dispatch_placing(&fake, &mut two, &gate, false)
            .await
            .unwrap();
        let dirs: Vec<String> = gate
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.1.clone())
            .collect();
        assert_eq!(
            dirs,
            ["/fake/worktrees/pastor-k1", "/fake/worktrees/pastor-k2"]
        );
    }

    /// A task a pull machine claimed carries what the head built for it:
    /// each stored bundle's set goes to the gate as it is, not by id, and
    /// its own set with the head's rendering of its profile. A set built
    /// for another kind than the agent's is left out, and says why.
    #[tokio::test]
    async fn a_claimed_task_places_the_sets_the_head_built() {
        let built = |kind: &str| {
            serde_json::json!({"id": "repo-notes", "kind": kind, "files": [
                {"path": ".claude/agents/notes.md", "content": "Notes.\n"}]})
        };
        let claimed = |kind: &str| {
            let mut spec =
                with_bundles(in_worktree(with_set(spec()), "pastor/k1"), &["repo-notes"]);
            spec.workdir_bundles[0].set = Some(built(kind));
            spec.agent_source = Some(Box::new(crate::task::AgentSource {
                profile: Some("review".into()),
                ..Default::default()
            }));
            let config = crate::config::PastorConfig::default();
            let profile = config.profiles.resolve("review").unwrap();
            spec.workdir.as_mut().unwrap().render = Some(Box::new(
                crate::profile_render::render(&config, &profile, "claude", None).unwrap(),
            ));
            spec
        };
        let fake = FakeHerdr::new();
        let gate = places(&fake, Ok(()));
        let mut t = task(claimed("claude"));
        dispatch_placing(&fake, &mut t, &gate, false).await.unwrap();
        let calls = gate.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert_eq!(calls[0].2, built("claude"));
        assert_eq!(calls[1].2["id"], "reviewer");
        assert_eq!(calls[1].2["rendered"], "review");
        assert!(t.spec.workdir_bundles[0].skipped.is_none());

        let fake = FakeHerdr::new();
        let gate = places(&fake, Ok(()));
        let mut t = task(claimed("agy"));
        dispatch_placing(&fake, &mut t, &gate, false).await.unwrap();
        let calls = gate.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1, "only its own set: {calls:?}");
        let why = t.spec.workdir_bundles[0].skipped.clone().unwrap();
        assert!(why.contains("built for agy agents"), "{why}");
    }

    /// A gate whose place never answers.
    struct Hangs;

    impl crate::bundle::workdir_task::Gate for Hangs {
        fn place<'a>(
            &'a self,
            _machine: &'a str,
            _dir: &'a str,
            _what: &'a crate::bundle::workdir_task::Placing,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, String>> + Send + 'a>>
        {
            Box::pin(std::future::pending())
        }

        fn forget(&self, _machine: &str, _dir: &str) {}
    }

    /// A place that does not answer within its own bound fails the start
    /// `workdir_failed`, a task error and not a herdr that stopped
    /// answering, and no agent starts.
    #[tokio::test]
    async fn a_place_that_does_not_answer_in_time_fails_the_start() {
        let fake = FakeHerdr::new();
        let handle = crate::bundle::workdir_task::GateHandle(Some(Arc::new(Hangs)));
        let provision = Provision {
            workdirs: Some(&handle),
            workdir_wait: Some(Duration::from_millis(50)),
            ..Default::default()
        };
        let mut t = task(with_set(spec()));
        let err = dispatch_with(&fake, &mut t, &Agents::default(), None, READY, provision)
            .await
            .unwrap_err();
        assert!(!err.is_transport(), "{err}");
        let err = err.to_string();
        assert!(err.starts_with("workdir_failed: "), "{err}");
        assert!(err.contains("did not answer within"), "{err}");
        assert_eq!(t.state, TaskState::Failed);
        assert!(!fake.requests().iter().any(|r| r.method == "agent.start"));
    }

    /// A task with no set asks the gate nothing.
    #[tokio::test]
    async fn a_task_with_no_set_places_nothing() {
        let fake = FakeHerdr::new();
        let gate = places(&fake, Err("not asked".into()));
        let mut t = task(spec());
        dispatch_placing(&fake, &mut t, &gate, false).await.unwrap();
        assert!(gate.calls.lock().unwrap().is_empty());
    }

    /// A machine the head cannot place a set on does not suit a task with
    /// one.
    #[test]
    fn a_task_with_a_set_suits_only_machines_that_take_sets() {
        let mut m = mv("a", 2, 0, &[], true);
        let with = with_set(spec());
        assert!(m.suits(&with));
        m.workdir_sets = false;
        assert!(!m.suits(&with));
        assert!(m.suits(&spec()), "a task with none goes anywhere");
    }
}
