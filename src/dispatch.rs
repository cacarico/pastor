use std::time::Duration;

use chrono::Utc;
use tokio::time::Instant;

use crate::config::Agents;
use crate::herdr::{
    AgentInfo, AgentStatus, CallError, Connector, ConnectorExt, Created, HerdrError,
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
    /// Every pane-owning task and orphan on the machine.
    pub live: usize,
    /// How many of `live` come from a job (`Task::from_job`).
    pub live_jobs: usize,
    pub healthy: bool,
    /// The flocks the machine is in now, each with its number and how many
    /// of its live tasks run here.
    pub flocks: Vec<FlockSeat>,
    /// The flocks under their share here with a queued task waiting that
    /// this machine could take. A flock past its share here takes a slot
    /// only while this is empty (`MachineView::flock_may_take`).
    pub waiting_under_share: Vec<String>,
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

    /// Is `flock` under its number here? Job slots and burst never pass it.
    pub fn flock_has_room(&self, flock: &str) -> bool {
        self.seat(flock).is_some_and(FlockSeat::has_room)
    }

    /// May a task of `flock` take a slot here, as far as the flock's number
    /// goes? Under its share, yes; from its share up to its max, only while
    /// no flock under its share here has a task waiting; at its max, no.
    pub fn flock_may_take(&self, flock: &str) -> bool {
        self.seat(flock).is_some_and(|s| {
            s.has_room() && (s.under_share() || self.waiting_under_share.is_empty())
        })
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
/// wins. Otherwise: healthy, has every required tag, has room for `claim`
/// (`MachineView::has_room`) with `flock` under its number there, and past
/// its share only while no flock under its share waits there
/// (`MachineView::flock_may_take`), fewest live tasks. Ties keep flock order.
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
            && spec.tags.iter().all(|t| m.tags.contains(t))
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

/// Fill each view's `waiting_under_share` for placing a task of `flock`: on
/// a machine where `flock` is past its share and under its max, the other
/// flocks under their share there that have a task in `later` the machine
/// could take (queued, not pinned elsewhere, with its tags, room for its
/// claim, one `accepts`). `later` is the queue behind the task being
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
                || !t.spec.tags.iter().all(|tag| v.tags.contains(tag))
                || !v.has_room(Claim::of(t))
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
            DispatchError::Task(_) => None,
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
    start(conn, task, agents, head, ready_timeout, false).await
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
    start(conn, task, agents, head, ready_timeout, true).await
}

async fn start(
    conn: &dyn Connector,
    task: &mut Task,
    agents: &Agents,
    head: Option<&str>,
    ready_timeout: Duration,
    resume: bool,
) -> Result<DispatchOutcome, DispatchError> {
    let name = Task::agent_name_for(task.id);
    task.agent_name = Some(name.clone());
    task.state = TaskState::Starting;
    task.error = None;
    task.prompt_pending = false;
    task.activity_seen = false;

    let result = dispatch_steps(conn, task, &name, agents, head, ready_timeout, resume).await;
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

async fn dispatch_steps(
    conn: &dyn Connector,
    task: &mut Task,
    name: &str,
    agents: &Agents,
    head: Option<&str>,
    ready_timeout: Duration,
    resume: bool,
) -> Result<DispatchOutcome, DispatchError> {
    let spec = task.spec.clone();
    // Before anything is made on the machine: a task whose agent cannot take
    // its tool lists (an `[agents]` edit since it was queued) leaves nothing
    // behind.
    let mut launch = agents
        .launch(&spec)
        .map_err(|e| DispatchError::Task(e.message))?;
    // opencode merges `OPENCODE_PERMISSION` over its own config's rules, so a
    // machine with rules of its own would run the task under both. A machine
    // that cannot tell (a `command` one) goes ahead.
    if agents.opencode_profile(&spec)
        && conn
            .opencode_permission_rules()
            .await
            .map_err(CallError::from)?
            == Some(true)
    {
        return Err(DispatchError::Task(format!(
            "{}: the opencode config on {} has permission rules of its own, which opencode would \
             merge with profile {}'s; move them out of its opencode config, or run without a profile",
            crate::config::opencode::OPENCODE_PERMISSIONS_CONFLICT,
            task.machine.as_deref().unwrap_or("this machine"),
            spec.profile().unwrap_or_default(),
        )));
    }
    // A Claude agent starts on a session pastor names, so `task attach` can
    // resume it once the pane is gone; last, after the tool flags. The task
    // records it only once `agent.start` succeeds (`finish_dispatch`): a
    // task that failed before then never had that conversation to resume.
    // A paused task goes back to the session it recorded instead.
    let mut session = None;
    if resume {
        let id = spec
            .session_id
            .clone()
            .filter(|id| crate::task::is_session_id(id))
            .ok_or_else(|| {
                DispatchError::Task(format!("{name} has no Claude session recorded to resume"))
            })?;
        launch.args.extend(["--resume".to_string(), id.clone()]);
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
        None => no_repo_dir(conn, task.machine.as_deref()).await?,
    };
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
                let (created, branch) =
                    open_worktree(conn, &spec, repo, name, name, resume).await?;
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
            let (created, branch) = open_worktree(conn, &spec, repo, name, &label, resume).await?;
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
    // A resumed session already has the line asking for a summary.
    let prompt = if resume {
        crate::task::RESUME_PROMPT.to_string()
    } else {
        crate::task::prompt_to_send(task)
    };
    finish_dispatch(
        conn,
        task,
        name,
        &launch,
        session,
        &pane_id,
        &prompt,
        ready_timeout,
    )
    .await
}

/// Start the agent in its pane, wait for it to come up and give it `prompt`.
#[allow(clippy::too_many_arguments)]
async fn finish_dispatch(
    conn: &dyn Connector,
    task: &mut Task,
    name: &str,
    launch: &crate::config::Launch,
    session: Option<String>,
    pane_id: &str,
    prompt: &str,
    ready_timeout: Duration,
) -> Result<DispatchOutcome, DispatchError> {
    // herdr's `agent.start` returns as soon as it has launched the agent in the
    // pane; it never reports `agent_not_ready` (its errors are about the name,
    // the kind and the pane). Readiness shows up afterwards, in `agent.list` and
    // in whether `agent.prompt` is accepted.
    start_agent(conn, name, &launch.kind, &launch.args, pane_id).await?;
    task.spec.session_id = session;

    let (outcome, prompted) = prompt_when_ready(conn, task, name, prompt, ready_timeout).await?;
    // The baseline a completion must move past, and whether the agent was
    // already at work when the prompt went in; see
    // `task::completed_since_prompt`.
    if let Some(agent) = prompted {
        task.last_completion_seq = Some(agent.state_change_seq);
        task.activity_seen = agent.agent_status.is_activity();
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

/// The workspace of a worktree task, labelled `label`, and the branch it is
/// on: the checkout a retry may reopen (`reopenable`), or else a new
/// worktree on the task's branch, `pastor/<name>` by default.
async fn open_worktree(
    conn: &dyn Connector,
    spec: &DispatchSpec,
    repo: &str,
    name: &str,
    label: &str,
    resume: bool,
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
    let branch = spec
        .branch
        .clone()
        .unwrap_or_else(|| format!("pastor/{name}"));
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
/// wherever its focused pane is, which can be anyone's checkout (t-284 opened
/// in a work repo). Not the home itself either: Claude Code never saves
/// folder trust for the home, so it asked at every start (t-358). So
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

/// Poll `agent.list` until the agent herdr just started is up, then prompt it.
///
/// herdr answers `agent_not_ready` both while a managed agent is still launching
/// (transient) and once the agent is no longer the pane's foreground process —
/// an agent that exited, e.g. because its binary is not installed on that
/// machine. The two are told apart by whether the agent is still in
/// `agent.list`: gone means failed now, present-but-`unknown` means wait.
/// Present-and-`blocked` is a third case, checked before either: herdr answers
/// that with `agent_blocked`, not `agent_not_ready`, and dispatch returns
/// `Blocked` without prompting or waiting.
async fn prompt_when_ready(
    conn: &dyn Connector,
    task: &Task,
    name: &str,
    prompt: &str,
    ready_timeout: Duration,
) -> Result<(DispatchOutcome, Option<AgentInfo>), DispatchError> {
    let machine = task.machine.as_deref().unwrap_or("that machine");
    let deadline = Instant::now() + ready_timeout;
    loop {
        let agents = conn.agent_list().await?;
        let Some(agent) = agents.iter().find(|a| a.name.as_deref() == Some(name)) else {
            return Err(DispatchError::Task(format!(
                "agent {name} exited before accepting a prompt (is `{}` installed on {machine}?)",
                task.spec.agent
            )));
        };
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
            match conn.agent_prompt(name, prompt).await {
                // The reply carries the agent's `state_change_seq` and status
                // as the prompt went in.
                Ok(agent) => return Ok((DispatchOutcome::Running, Some(agent))),
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
            return Err(DispatchError::Task(format!(
                "agent {name} exited before becoming interactive (is `{}` installed on {machine}?)",
                task.spec.agent
            )));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::flock::FlockNumber;
    use crate::herdr::AgentStatus;
    use crate::herdr::fake::{FakeHerdr, StartBehaviour};
    use crate::task::WorkspaceLabel;
    use serde_json::Value;

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
            live,
            live_jobs: 0,
            healthy,
            flocks: vec![seat("default", None, live)],
            waiting_under_share: vec![],
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
            pause: Default::default(),
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
            with_session(&t, serde_json::json!(["--model", "opus"]))
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
            assert_eq!(start.params["args"], serde_json::json!(args));
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
                profile: profile.map(str::to_string),
                profile_from: Some("defaults".into()),
                timeout_from: None,
                place_from: None,
            })),
            ..spec()
        }
    }

    /// A profiled opencode task starts with its rules in the pane's env.
    /// On a machine whose own opencode config has permission rules it fails
    /// before anything is made there: opencode would merge them with the
    /// profile's. A machine that cannot tell goes ahead, and a task with no
    /// profile never asks.
    #[tokio::test]
    async fn a_profiled_opencode_task_refuses_a_machine_with_its_own_rules() {
        let fake = FakeHerdr::new();
        let mut t = task(opencode_under(Some("review")));
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        let env = fake.pane_env(t.pane_id.as_deref().unwrap());
        assert_eq!(
            env["OPENCODE_PERMISSION"],
            crate::config::opencode::permission_json(&t.spec.allow, &t.spec.deny, false)
        );
        assert_eq!(env["OPENCODE_CONFIG"], "");
        let start = fake.requests();
        let start = start.iter().find(|r| r.method == "agent.start").unwrap();
        assert_eq!(start.params["kind"], "opencode");
        assert_eq!(start.params["args"], serde_json::json!([]));

        let fake = FakeHerdr::new();
        fake.set_opencode_permissions(Some(true));
        let mut t = task(opencode_under(Some("review")));
        let err = dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("opencode_permissions_conflict"),
            "{err}"
        );
        assert!(err.to_string().contains("pi-1"), "{err}");
        assert!(!err.is_transport());
        assert_eq!(t.state, TaskState::Failed);
        assert!(fake.requests().is_empty(), "{:?}", fake.requests());

        fake.set_opencode_permissions(None);
        let mut t = task(opencode_under(Some("review")));
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();

        let fake = FakeHerdr::new();
        fake.set_opencode_permissions(Some(true));
        let mut t = task(DispatchSpec {
            allow: vec![],
            deny: vec![],
            ..opencode_under(None)
        });
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
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

    /// `[agents.claude-personal] kind = "claude"` with an env: herdr starts
    /// a claude, with Claude's tool flags, in a pane that has the env, `~`
    /// expanded against the machine's home.
    fn personal() -> Agents {
        let mut agents = Agents::default();
        agents.0.insert(
            "claude-personal".into(),
            crate::config::AgentDef {
                kind: Some("claude".into()),
                env: [
                    (
                        "CLAUDE_CONFIG_DIR".to_string(),
                        "~/.claude-personal".to_string(),
                    ),
                    ("PLAIN".to_string(), "a~b".to_string()),
                ]
                .into(),
                ..Default::default()
            },
        );
        agents
    }

    #[tokio::test]
    async fn an_agent_definition_starts_its_kind_with_its_env() {
        let fake = FakeHerdr::new();
        let mut t = task(DispatchSpec {
            agent: "claude-personal".into(),
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
        let want = serde_json::json!({
            "CLAUDE_CONFIG_DIR": "/home/fake/.claude-personal",
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
                serde_json::json!(["--model", "opus", "--disallowedTools", "WebFetch"])
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
            agent: "claude-personal".into(),
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
            "/home/fake/.claude-personal"
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
            serde_json::json!({"PASTOR_TASK": "t-7"})
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

    /// A head address reaches the agent as `PASTOR_HEAD`, after the
    /// `[agents]` env so config cannot clear it; without one there is none.
    #[tokio::test]
    async fn the_head_address_goes_in_the_pane_env() {
        let fake = FakeHerdr::new();
        let mut t = task(DispatchSpec {
            agent: "claude-personal".into(),
            ..spec()
        });
        let mut agents = personal();
        agents
            .0
            .get_mut("claude-personal")
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
            ..spec()
        });
        dispatch(&fake, &mut t, &Agents::default(), None, READY)
            .await
            .unwrap();
        assert_eq!(t.spec.label.name.as_deref(), Some("seed"));
        assert_eq!(
            t.spec.label.note.as_deref(),
            Some(crate::task::JOINED_WORKSPACE)
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
    /// put t-284, a task with no repo, in someone's work checkout
    /// (2026-09-28). A task with no repo starts in `~/pastor-tasks`, made
    /// when missing, in a workspace of its own or a pane split into a shared
    /// one: not the home itself, which Claude Code never saves trust for, so
    /// it asked at every start (t-358). A machine that cannot tell its home
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
            }
        }
    }

    /// A folder that cannot be made (a file in the way, no permission) is no
    /// reason to refuse the task: it starts in the home, as before this
    /// folder, and Claude asks for trust there.
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
}
