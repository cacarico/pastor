use std::time::Duration;

use chrono::Utc;
use tokio::time::Instant;

use crate::config::Agents;
use crate::herdr::{
    AgentInfo, AgentStatus, CallError, Connector, ConnectorExt, Created, HerdrError,
};
use crate::task::{Checkout, DispatchSpec, Reopen, Task, TaskState};

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
    pub tags: Vec<String>,
    pub live: usize,
    pub healthy: bool,
    /// The flock the machine is in now.
    pub flock: String,
}

/// Only machines in `flock`, the task's, qualify. Of those the pinned machine
/// wins. Otherwise: healthy, has every required tag, below capacity, fewest
/// live tasks. Ties keep flock order.
pub fn pick_machine(machines: &[MachineView], flock: &str, spec: &DispatchSpec) -> Option<String> {
    let fits = |m: &MachineView| {
        m.flock == flock
            && m.healthy
            && (m.live as u64) < m.max_agents as u64
            && spec.tags.iter().all(|t| m.tags.contains(t))
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
pub async fn dispatch(
    conn: &dyn Connector,
    task: &mut Task,
    agents: &Agents,
    ready_timeout: Duration,
) -> Result<DispatchOutcome, DispatchError> {
    let name = Task::agent_name_for(task.id);
    task.agent_name = Some(name.clone());
    task.state = TaskState::Starting;
    task.error = None;
    task.prompt_pending = false;
    task.activity_seen = false;

    let result = dispatch_steps(conn, task, &name, agents, ready_timeout).await;
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
    ready_timeout: Duration,
) -> Result<DispatchOutcome, DispatchError> {
    let spec = task.spec.clone();
    // Before anything is made on the machine: a task whose agent cannot take
    // its tool lists (an `[agents]` edit since it was queued) leaves nothing
    // behind.
    let args = agents.launch_args(&spec).map_err(DispatchError::Task)?;
    let repo = match spec.repo.as_deref() {
        Some(repo) => Some(expand_home(conn, repo, task.machine.as_deref()).await?),
        None => None,
    };
    if let Some(dir) = repo.as_deref() {
        check_repo_exists(conn, dir, task.machine.as_deref()).await?;
    }
    let (created, branch) = if spec.worktree {
        let repo = repo
            .as_deref()
            .ok_or_else(|| HerdrError::Protocol("worktree = true needs repo".into()))?;
        let (created, branch) = open_worktree(conn, &spec, repo, name).await?;
        (created, Some(branch))
    } else {
        (conn.workspace_create(repo.as_deref(), name).await?, None)
    };
    task.workspace_id = Some(created.workspace.workspace_id.clone());
    task.pane_id = Some(created.root_pane.pane_id.clone());
    if let (Some(repo), Some(branch)) = (repo.as_deref(), branch) {
        task.spec.checkout = find_checkout(conn, repo, branch, &created).await?;
    }

    // herdr's `agent.start` returns as soon as it has launched the agent in the
    // pane; it never reports `agent_not_ready` (its errors are about the name,
    // the kind and the pane). Readiness shows up afterwards, in `agent.list` and
    // in whether `agent.prompt` is accepted.
    start_agent(conn, name, &spec.agent, &args, &created.root_pane.pane_id).await?;

    let (outcome, prompted) = prompt_when_ready(conn, task, name, ready_timeout).await?;
    // The baseline a completion must move past, and whether the agent was
    // already at work when the prompt went in; see
    // `task::completed_since_prompt`.
    if let Some(agent) = prompted {
        task.last_completion_seq = Some(agent.state_change_seq);
        task.activity_seen = agent.agent_status.is_activity();
    }
    Ok(outcome)
}

/// The workspace of a worktree task, and the branch it is on: the checkout
/// a retry may reopen (`reopenable`), or else a new worktree on the task's
/// branch, `pastor/<name>` by default.
async fn open_worktree(
    conn: &dyn Connector,
    spec: &DispatchSpec,
    repo: &str,
    name: &str,
) -> Result<(Created, String), DispatchError> {
    if let Some(reopen) = reopenable(conn, spec, repo).await? {
        let created = conn.worktree_open(repo, &reopen.branch, name).await?;
        return Ok((created, reopen.branch.clone()));
    }
    let branch = spec
        .branch
        .clone()
        .unwrap_or_else(|| format!("pastor/{name}"));
    Ok((conn.worktree_create(repo, &branch, name).await?, branch))
}

/// The one rule for reopening a checkout. A retry goes back to the checkout
/// of the failed task it retries (`DispatchSpec::reopen`, which
/// `Store::insert_retry` sets only from the checkout that task's own
/// dispatch recorded) only while it is on disk at the same path, on the same
/// branch, that task's agent is gone and no agent is listed in the
/// workspace showing the checkout: the failed task's work is there, and
/// nobody else is at work in it. Anything else is `None` and gets a new
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
/// a retry knows it is this task's own. Taken from `worktree.list`, the
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

/// Expand a leading `~` in `repo` against the machine's home directory.
///
/// herdr takes `cwd` literally: `~/work` is a directory named `~` to it, and a
/// `cwd` that does not exist silently opens the pane somewhere else. Job files
/// and `pastor task run --repo` both use `~` to mean the home on that machine, so
/// pastor resolves it there before asking herdr.
async fn expand_home(
    conn: &dyn Connector,
    repo: &str,
    machine: Option<&str>,
) -> Result<String, DispatchError> {
    let rest = match repo.strip_prefix('~') {
        None => return Ok(repo.to_string()),
        Some(rest) if rest.is_empty() || rest.starts_with('/') => rest,
        Some(_) => {
            return Err(DispatchError::Task(format!(
                "repo {repo}: only ~ and ~/ are expanded, not ~user; use an absolute path"
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
            "repo {repo}: pastor cannot tell the home directory on {machine}; \
             use an absolute path"
        ))),
    }
}

/// herdr opens a workspace whose `cwd` does not exist in the shell's home
/// instead, with no error, and the agent then works on the wrong tree (t-5,
/// 2026-09-24). Refuse before creating anything. A machine that cannot answer
/// (`None`: a `command` bridge, a shell that printed nothing) goes ahead, as
/// before.
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
            match conn.agent_prompt(name, &task.prompt).await {
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
    use crate::herdr::AgentStatus;
    use crate::herdr::fake::{FakeHerdr, StartBehaviour};
    use serde_json::Value;

    /// Generous next to every `ready_after` these tests use, so only the test
    /// that means to hit the bound does.
    const READY: Duration = Duration::from_secs(5);

    fn mv(name: &str, max: u32, live: usize, tags: &[&str], healthy: bool) -> MachineView {
        MachineView {
            name: name.into(),
            max_agents: max,
            tags: tags.iter().map(|s| s.to_string()).collect(),
            live,
            healthy,
            flock: "default".into(),
        }
    }

    /// Only the task's flock takes it, whatever the others have free.
    #[test]
    fn pick_machine_keeps_to_the_tasks_flock() {
        let ms = vec![
            mv("home-1", 2, 1, &[], true),
            MachineView {
                flock: "work".into(),
                ..mv("work-1", 2, 0, &[], true)
            },
        ];
        assert_eq!(
            pick_machine(&ms, "default", &spec()).as_deref(),
            Some("home-1")
        );
        assert_eq!(
            pick_machine(&ms, "work", &spec()).as_deref(),
            Some("work-1")
        );
        assert_eq!(pick_machine(&ms, "play", &spec()), None);
        let pinned_elsewhere = DispatchSpec {
            machine: Some("work-1".into()),
            ..spec()
        };
        assert_eq!(
            pick_machine(&ms, "default", &pinned_elsewhere),
            None,
            "a pinned machine that moved to another flock takes nothing"
        );
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
            let err = dispatch(&fake, &mut t, &Agents::default(), READY)
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
        let err = dispatch(&fake, &mut t, &Agents::default(), READY)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("/home/fake/gone does not exist"),
            "{err}"
        );
    }

    fn task(spec: DispatchSpec) -> Task {
        let now = Utc::now();
        Task {
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
            retry_of: None,
            created_at: now,
            started_at: None,
            finished_at: None,
            updated_at: now,
            flock: None,
        }
    }

    #[test]
    fn pick_machine_respects_capacity() {
        let ms = vec![
            mv("a", 1, 1, &[], true),
            mv("b", 2, 1, &[], true),
            mv("c", 2, 0, &[], true),
        ];
        assert_eq!(pick_machine(&ms, "default", &spec()).as_deref(), Some("c"));
        let full = vec![mv("a", 1, 1, &[], true)];
        assert_eq!(pick_machine(&full, "default", &spec()), None);
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
                }
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
                }
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
                }
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
                }
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
                }
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
                }
            ),
            None
        );
    }

    #[tokio::test]
    async fn dispatch_sends_prompt_verbatim() {
        let fake = FakeHerdr::new();
        let mut t = task(spec());
        let out = dispatch(&fake, &mut t, &Agents::default(), READY)
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
        assert_eq!(ws.params["label"], "t-7");
        let start = reqs.iter().find(|r| r.method == "agent.start").unwrap();
        assert_eq!(start.params["kind"], "claude");
        assert_eq!(start.params["args"], serde_json::json!(["--model", "opus"]));
        let prompt = reqs.iter().find(|r| r.method == "agent.prompt").unwrap();
        assert_eq!(prompt.params["target"], "t-7");
        assert_eq!(prompt.params["text"], "line one\n\"two\" {{ three }}");
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
        dispatch(&fake, &mut t, &Agents::default(), READY)
            .await
            .unwrap();
        let reqs = fake.requests();
        let start = reqs.iter().find(|r| r.method == "agent.start").unwrap();
        assert_eq!(
            start.params["args"],
            serde_json::json!([
                "--model",
                "opus",
                "--allowedTools",
                "Bash(git:*)",
                "--disallowedTools",
                "WebFetch"
            ])
        );
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
        let err = dispatch(&fake, &mut t, &Agents::default(), READY)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("deny_flag"), "{err}");
        assert!(!err.is_transport());
        assert_eq!(t.state, TaskState::Failed);
        assert!(fake.requests().is_empty(), "{:?}", fake.requests());
    }

    #[tokio::test]
    async fn dispatch_uses_worktree_when_asked() {
        let fake = FakeHerdr::new();
        let mut t = task(DispatchSpec {
            worktree: true,
            branch: Some("pastor/k1".into()),
            ..spec()
        });
        dispatch(&fake, &mut t, &Agents::default(), READY)
            .await
            .unwrap();
        let wt = fake
            .requests()
            .into_iter()
            .find(|r| r.method == "worktree.create")
            .unwrap();
        assert_eq!(wt.params["cwd"], "/srv/app");
        assert_eq!(wt.params["branch"], "pastor/k1");
        assert_eq!(wt.params["label"], "t-7");
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
                dispatch(&fake, &mut t, &Agents::default(), READY)
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
            let err = dispatch(&fake, &mut t, &Agents::default(), READY)
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
        let out = dispatch(&fake, &mut t, &Agents::default(), READY)
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
        let err = dispatch(&fake, &mut t, &Agents::default(), READY)
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
        let out = dispatch(&fake, &mut t, &Agents::default(), READY)
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
        let out = dispatch(&fake, &mut t, &Agents::default(), READY)
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
        let err = dispatch(&fake, &mut t, &Agents::default(), READY)
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
    }

    /// herdr answers `agent_pane_busy` while the new pane's shell is still
    /// starting; on the real fleet the same dispatch works a moment later
    /// (t-42, t-50). Dispatch retries `agent.start` instead of failing.
    #[tokio::test]
    async fn a_busy_pane_is_retried_until_the_shell_is_up() {
        let fake = FakeHerdr::new();
        fake.set_pane_busy_for(2);
        let mut t = task(spec());
        let out = dispatch(&fake, &mut t, &Agents::default(), READY)
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
        let err = dispatch(&fake, &mut t, &Agents::default(), READY)
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
        let err = dispatch(&fake, &mut t, &Agents::default(), READY)
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
        let err = dispatch(&fake, &mut t, &Agents::default(), READY)
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
        let err = dispatch(&fake, &mut t, &Agents::default(), READY)
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
}
