use std::time::Duration;

use chrono::Utc;
use tokio::time::Instant;

use crate::herdr::{AgentStatus, CallError, Connector, ConnectorExt, HerdrError};
use crate::task::{DispatchSpec, Task, TaskState};

/// How often dispatch asks `agent.list` whether the agent it started is up yet.
const READY_POLL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone)]
pub struct MachineView {
    pub name: String,
    pub max_agents: u32,
    pub tags: Vec<String>,
    pub live: usize,
    pub healthy: bool,
}

/// Pinned machine wins. Otherwise: healthy, has every required tag, below capacity,
/// fewest live tasks. Ties keep flock order.
pub fn pick_machine(machines: &[MachineView], spec: &DispatchSpec) -> Option<String> {
    let fits = |m: &MachineView| {
        m.healthy
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
/// agent surfaces as a confusing "request timed out".
pub async fn dispatch(
    conn: &dyn Connector,
    task: &mut Task,
    ready_timeout: Duration,
) -> Result<DispatchOutcome, DispatchError> {
    let name = Task::agent_name_for(task.id);
    task.agent_name = Some(name.clone());
    task.state = TaskState::Starting;
    task.error = None;
    task.prompt_pending = false;

    let result = dispatch_steps(conn, task, &name, ready_timeout).await;
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
    ready_timeout: Duration,
) -> Result<DispatchOutcome, DispatchError> {
    let spec = task.spec.clone();
    let repo = match spec.repo.as_deref() {
        Some(repo) => Some(expand_home(conn, repo, task.machine.as_deref()).await?),
        None => None,
    };
    let created = if spec.worktree {
        let repo = repo
            .as_deref()
            .ok_or_else(|| HerdrError::Protocol("worktree = true needs repo".into()))?;
        let branch = spec
            .branch
            .clone()
            .unwrap_or_else(|| format!("pastor/{name}"));
        conn.worktree_create(repo, &branch, name).await?
    } else {
        conn.workspace_create(repo.as_deref(), name).await?
    };
    task.workspace_id = Some(created.workspace.workspace_id.clone());
    task.pane_id = Some(created.root_pane.pane_id.clone());

    // herdr's `agent.start` returns as soon as it has launched the agent in the
    // pane; it never reports `agent_not_ready` (its errors are about the name,
    // the kind and the pane). Readiness shows up afterwards, in `agent.list` and
    // in whether `agent.prompt` is accepted.
    conn.agent_start(
        name,
        &spec.agent,
        &created.root_pane.pane_id,
        &spec.agent_args,
    )
    .await?;

    prompt_when_ready(conn, task, name, ready_timeout).await
}

/// Expand a leading `~` in `repo` against the machine's home directory.
///
/// herdr takes `cwd` literally: `~/work` is a directory named `~` to it, and a
/// `cwd` that does not exist silently opens the pane somewhere else. Job files
/// and `pastor run --repo` both use `~` to mean the home on that machine, so
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

/// Poll `agent.list` until the agent herdr just started is up, then prompt it.
///
/// herdr answers `agent_not_ready` both while a managed agent is still launching
/// (transient) and once the agent is no longer the pane's foreground process —
/// an agent that exited, e.g. because its binary is not installed on that
/// machine. The two are told apart by whether the agent is still in
/// `agent.list`: gone means failed now, present-but-`unknown` means wait.
async fn prompt_when_ready(
    conn: &dyn Connector,
    task: &Task,
    name: &str,
    ready_timeout: Duration,
) -> Result<DispatchOutcome, DispatchError> {
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
        // herdr 0.9.1 reports readiness with two flags, the same ones its own
        // `agent start --wait` reads: `launch_pending` while the process is
        // coming up, `interactive_ready` once it accepts input. A listed agent
        // with neither, and not working or blocked, is a pane whose process
        // already exited: prompting it answers `agent_not_ready` forever.
        let can_prompt = agent.interactive_ready
            || matches!(
                agent.agent_status,
                AgentStatus::Working | AgentStatus::Blocked
            );
        if agent.agent_status == AgentStatus::Blocked {
            // Waiting for a human, usually on the agent's own startup question
            // (Claude's folder trust dialog). herdr keeps `launch_pending` set
            // meanwhile and refuses prompts with `agent_not_ready`, so waiting
            // here would only run out the bound. The machine sends the prompt
            // once the block clears (`Task::prompt_pending`).
            return Ok(DispatchOutcome::Blocked);
        } else if agent.launch_pending {
            // still launching: fall through to the wait below
        } else if can_prompt {
            match conn.agent_prompt(name, &task.prompt).await {
                Ok(_) => return Ok(DispatchOutcome::Running),
                // The agent is up and waiting for a human, not for us. herdr
                // did not send the prompt; the machine sends it after the block.
                Err(err) if err.code() == Some("agent_blocked") => {
                    return Ok(DispatchOutcome::Blocked);
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
        }
    }

    fn spec() -> DispatchSpec {
        DispatchSpec {
            agent: "claude".into(),
            agent_args: vec!["--model".into(), "opus".into()],
            repo: Some("/srv/app".into()),
            worktree: false,
            branch: None,
            machine: None,
            tags: vec![],
            timeout_secs: 60,
        }
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
            created_at: now,
            started_at: None,
            finished_at: None,
            updated_at: now,
        }
    }

    #[test]
    fn pick_machine_respects_capacity() {
        let ms = vec![
            mv("a", 1, 1, &[], true),
            mv("b", 2, 1, &[], true),
            mv("c", 2, 0, &[], true),
        ];
        assert_eq!(pick_machine(&ms, &spec()).as_deref(), Some("c"));
        let full = vec![mv("a", 1, 1, &[], true)];
        assert_eq!(pick_machine(&full, &spec()), None);
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
        let out = dispatch(&fake, &mut t, READY).await.unwrap();
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

    #[tokio::test]
    async fn dispatch_uses_worktree_when_asked() {
        let fake = FakeHerdr::new();
        let mut t = task(DispatchSpec {
            worktree: true,
            branch: Some("pastor/k1".into()),
            ..spec()
        });
        dispatch(&fake, &mut t, READY).await.unwrap();
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
                dispatch(&fake, &mut t, READY).await.unwrap();
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
            let err = dispatch(&fake, &mut t, READY).await.unwrap_err();
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
        let out = dispatch(&fake, &mut t, READY).await.unwrap();
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
        let err = dispatch(&fake, &mut t, READY).await.unwrap_err();
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
        let err = dispatch(&fake, &mut t, Duration::from_millis(200))
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
                    watcher.set_status(&a.pane_id, AgentStatus::Blocked, None);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let mut t = task(spec());
        let started = Instant::now();
        let out = dispatch(&fake, &mut t, READY).await.unwrap();
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
                    watcher.set_status(&a.pane_id, AgentStatus::Blocked, None);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let mut t = task(spec());
        let out = dispatch(&fake, &mut t, READY).await.unwrap();
        assert_eq!(out, DispatchOutcome::Blocked);
        assert_eq!(t.state, TaskState::Blocked);
        assert!(t.pane_id.is_some(), "pane is kept for inspection");
    }

    #[tokio::test]
    async fn start_failures_are_failed() {
        let fake = FakeHerdr::new();
        fake.set_start_behaviour(StartBehaviour::Fail("unsupported_agent_kind".into()));
        let mut t = task(spec());
        let err = dispatch(&fake, &mut t, READY).await.unwrap_err();
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
        let err = dispatch(&fake, &mut t, READY).await.unwrap_err();
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
        let err = dispatch(&fake, &mut t, READY).await.unwrap_err();
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
