use chrono::Utc;

use crate::herdr::{CallError, Connector, ConnectorExt, HerdrError};
use crate::task::{DispatchSpec, Task, TaskState};

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

/// Create the workspace, start the agent, send the prompt. Each step is its own
/// herdr request on its own connection (see `ConnectorExt`), so a dispatch is
/// three round trips, not one session: nothing but the task row ties them
/// together, which is why `agent_name` identifies the agent afterwards.
pub async fn dispatch(conn: &dyn Connector, task: &mut Task) -> Result<DispatchOutcome, CallError> {
    let name = Task::agent_name_for(task.id);
    task.agent_name = Some(name.clone());
    task.state = TaskState::Starting;
    task.error = None;

    let result = dispatch_steps(conn, task, &name).await;
    match &result {
        Ok(DispatchOutcome::Running) => {
            task.state = TaskState::Running;
            task.started_at = Some(Utc::now());
        }
        Ok(DispatchOutcome::Blocked) => {
            task.state = TaskState::Blocked;
            task.started_at = Some(Utc::now());
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
) -> Result<DispatchOutcome, CallError> {
    let spec = task.spec.clone();
    let created = if spec.worktree {
        let repo = spec
            .repo
            .as_deref()
            .ok_or_else(|| HerdrError::Protocol("worktree = true needs repo".into()))?;
        let branch = spec
            .branch
            .clone()
            .unwrap_or_else(|| format!("pastor/{name}"));
        conn.worktree_create(repo, &branch, name).await?
    } else {
        conn.workspace_create(spec.repo.as_deref(), name).await?
    };
    task.workspace_id = Some(created.workspace.workspace_id.clone());
    task.pane_id = Some(created.root_pane.pane_id.clone());

    match conn
        .agent_start(
            name,
            &spec.agent,
            &created.root_pane.pane_id,
            &spec.agent_args,
        )
        .await
    {
        Ok(_) => {}
        Err(err) if err.code() == Some("agent_not_ready") => return Ok(DispatchOutcome::Blocked),
        Err(err) => return Err(err),
    }
    conn.agent_prompt(name, &task.prompt).await?;
    Ok(DispatchOutcome::Running)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::AgentStatus;
    use crate::herdr::fake::{FakeHerdr, StartBehaviour};
    use serde_json::Value;

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
        let out = dispatch(&fake, &mut t).await.unwrap();
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
        dispatch(&fake, &mut t).await.unwrap();
        let wt = fake
            .requests()
            .into_iter()
            .find(|r| r.method == "worktree.create")
            .unwrap();
        assert_eq!(wt.params["cwd"], "/srv/app");
        assert_eq!(wt.params["branch"], "pastor/k1");
        assert_eq!(wt.params["label"], "t-7");
    }

    #[tokio::test]
    async fn not_ready_is_blocked_and_failures_are_failed() {
        let fake = FakeHerdr::new();
        fake.set_start_behaviour(StartBehaviour::NotReady);
        let mut t = task(spec());
        assert_eq!(
            dispatch(&fake, &mut t).await.unwrap(),
            DispatchOutcome::Blocked
        );
        assert_eq!(t.state, TaskState::Blocked);
        assert!(t.pane_id.is_some(), "pane is kept for inspection");
        assert!(!fake.requests().iter().any(|r| r.method == "agent.prompt"));

        fake.set_start_behaviour(StartBehaviour::Fail("unsupported_agent_kind".into()));
        let mut t = task(spec());
        let err = dispatch(&fake, &mut t).await.unwrap_err();
        assert_eq!(err.code(), Some("unsupported_agent_kind"));
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
    }

    #[tokio::test]
    async fn worktree_without_repo_fails_before_calling_herdr() {
        let fake = FakeHerdr::new();
        let mut t = task(DispatchSpec {
            worktree: true,
            repo: None,
            ..spec()
        });
        let err = dispatch(&fake, &mut t).await.unwrap_err();
        assert!(
            matches!(err, CallError::Herdr(HerdrError::Protocol(_))),
            "{err:?}"
        );
        assert_eq!(t.state, TaskState::Failed);
        assert!(fake.requests().is_empty());
        let _ = AgentStatus::Idle;
    }
}
