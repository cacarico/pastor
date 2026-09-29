//! `pastor task attach` on a task whose pane is gone: reopen the agent's own
//! Claude session in a new pane on the task's machine.
//!
//! The pane is the user's, not the task's: the task's row is not touched,
//! and closing the pane leaves no trace on it. Its agent is named
//! `t-<id>-resume`, which reconcile does not take for an orphan (only
//! `t-<id>` counts), and a second attach goes back to it while it lives.

use crate::config::Agents;
use crate::dispatch::{DispatchError, expand_home, no_repo_dir};
use crate::herdr::{CallError, Connector, ConnectorExt};
use crate::task::Task;

/// Why a task cannot be reopened: a stable code for the CLI, and a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReopenError {
    pub code: &'static str,
    pub message: String,
}

impl ReopenError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        ReopenError {
            code,
            message: message.into(),
        }
    }
}

impl From<CallError> for ReopenError {
    fn from(err: CallError) -> Self {
        ReopenError::new("herdr_error", err.to_string())
    }
}

impl From<DispatchError> for ReopenError {
    fn from(err: DispatchError) -> Self {
        ReopenError::new("reopen_failed", err.to_string())
    }
}

/// The name of the agent that resumes task `id`'s session.
pub fn resume_agent_name(id: i64) -> String {
    format!("{}-resume", Task::agent_name_for(id))
}

/// Why `task` has nothing to reopen, before any machine is asked: no
/// session recorded. `None` when it has one.
pub fn why_not(task: &Task, agents: &Agents) -> Option<String> {
    if task.spec.session_id.is_some() {
        return None;
    }
    Some(if agents.kind(&task.spec.agent) == "claude" {
        "no Claude session was recorded for it to reopen".into()
    } else {
        "only Claude tasks can be reopened once their pane is gone".into()
    })
}

/// The agent `pastor task attach` should attach to for `task`, whose pane
/// pastor holds closed: its own agent while herdr still lists it, else an
/// earlier resume that is still open, else a new pane in the task's working
/// directory (the directory dispatch recorded on it for a task with no
/// repo, `task.spec.cwd`), with its agent definition's env, running
/// `claude --resume <session>`. A worktree removed at close is put back
/// first, at the same path on the same branch, since Claude files its
/// sessions by directory.
pub async fn reopen(
    conn: &dyn Connector,
    task: &Task,
    agents: &Agents,
) -> Result<String, ReopenError> {
    let id = task.display_id();
    let machine = task.machine.as_deref().unwrap_or("its machine");
    let Some(session) = task.spec.session_id.as_deref() else {
        let why = why_not(task, agents).unwrap_or_default();
        return Err(ReopenError::new("no_session", format!("{id}: {why}")));
    };
    if !crate::task::is_session_id(session) {
        return Err(ReopenError::new(
            "no_session",
            format!("{id}: its session {session:?} is not a session id"),
        ));
    }
    let name = resume_agent_name(task.id);
    let own = task.agent_name.clone().unwrap_or_else(|| id.clone());
    for agent in conn.agent_list().await? {
        match agent.name {
            Some(n) if n == own || n == name => return Ok(n),
            _ => {}
        }
    }

    let repo = match task.spec.repo.as_deref() {
        Some(repo) => Some(expand_home(conn, "repo", repo, Some(machine)).await?),
        None => None,
    };
    let cwd = match (&task.spec.checkout, &repo) {
        (Some(checkout), Some(repo)) => {
            if conn
                .dir_exists(&checkout.path)
                .await
                .map_err(CallError::from)?
                == Some(false)
            {
                match conn
                    .restore_worktree(repo, &checkout.path, &checkout.branch)
                    .await
                    .map_err(CallError::from)?
                {
                    Some(true) => {}
                    Some(false) => {
                        return Err(ReopenError::new(
                            "branch_gone",
                            format!(
                                "{id}: its worktree {} was removed and its branch {} is gone too, so there is no checkout to resume its session in",
                                checkout.path, checkout.branch
                            ),
                        ));
                    }
                    None => {
                        return Err(ReopenError::new(
                            "reopen_failed",
                            format!(
                                "{id}: could not re-create its worktree {} on branch {} on {machine}",
                                checkout.path, checkout.branch
                            ),
                        ));
                    }
                }
            }
            Some(checkout.path.clone())
        }
        (_, Some(repo)) => Some(repo.clone()),
        (_, None) => match task.spec.cwd.clone() {
            Some(cwd) => Some(cwd),
            // A task from before `cwd` was recorded: best effort, as at
            // dispatch, which may now answer differently.
            None => no_repo_dir(conn, Some(machine)).await?,
        },
    };
    if let Some(dir) = cwd.as_deref()
        && conn.dir_exists(dir).await.map_err(CallError::from)? == Some(false)
    {
        return Err(ReopenError::new(
            "reopen_failed",
            format!("{id}: its directory {dir} is gone from {machine}"),
        ));
    }

    let mut env = agents
        .0
        .get(&task.spec.agent)
        .map(|d| d.env.clone())
        .unwrap_or_default();
    for (key, value) in env.iter_mut() {
        if value == "~" || value.starts_with("~/") {
            *value = expand_home(conn, &format!("env {key}"), value, Some(machine)).await?;
        }
    }
    // Still an agent pastor started, as far as the head's fleet rules go.
    env.insert(crate::ipc::TASK_ENV.into(), own);
    let created = conn.workspace_create(cwd.as_deref(), &name, &env).await?;
    let args = ["--resume".to_string(), session.to_string()];
    conn.agent_start(&name, "claude", &created.root_pane.pane_id, &args)
        .await?;
    Ok(name)
}

#[cfg(all(test, feature = "fake-herdr"))]
mod tests {
    use super::*;
    use crate::herdr::fake::FakeHerdr;
    use crate::task::{Checkout, DispatchSpec, TaskRole, TaskState};
    use chrono::Utc;

    const SESSION: &str = "0d5bd3a4-2f35-4e1c-9f59-7c1c3a7b8e21";

    fn closed(agent: &str, session: Option<&str>) -> Task {
        let now = Utc::now();
        Task {
            description: None,
            id: 4,
            job: "run".into(),
            item: serde_json::Value::Null,
            prompt: "p".into(),
            spec: DispatchSpec {
                agent: agent.into(),
                agent_args: vec![],
                allow: vec![],
                deny: vec![],
                repo: Some("~/src/app".into()),
                worktree: false,
                branch: None,
                machine: None,
                tags: vec![],
                timeout_secs: 60,
                checkout: None,
                reopen: None,
                agent_source: None,
                place: Default::default(),
                session_id: session.map(String::from),
                label: Default::default(),
                summary: Default::default(),
                cwd: None,
            },
            machine: Some("pi-1".into()),
            workspace_id: Some("w9".into()),
            pane_id: Some("w9:p1".into()),
            agent_name: Some("t-4".into()),
            role: TaskRole::Agent,
            state: TaskState::Closed,
            error: None,
            last_completion_seq: None,
            prompt_pending: false,
            activity_seen: false,
            ended: false,
            retry_of: None,
            flock: None,
            priority: Default::default(),
            priority_from: None,
            queue_pos: 0,
            pause: Default::default(),
            summary: None,
            created_at: now,
            started_at: None,
            finished_at: None,
            updated_at: now,
        }
    }

    fn personal() -> Agents {
        let mut agents = Agents::default();
        agents.0.insert(
            "claude-personal".into(),
            crate::config::AgentDef {
                kind: Some("claude".into()),
                env: [(
                    "CLAUDE_CONFIG_DIR".to_string(),
                    "~/.claude-personal".to_string(),
                )]
                .into(),
                ..Default::default()
            },
        );
        agents
    }

    /// A closed claude task: a pane in its directory, with its agent's env,
    /// running `claude --resume <session>`.
    #[tokio::test]
    async fn a_closed_claude_task_resumes_in_a_new_pane() {
        let fake = FakeHerdr::new();
        let t = closed("claude-personal", Some(SESSION));
        let name = reopen(&fake, &t, &personal()).await.unwrap();
        assert_eq!(name, "t-4-resume");
        let reqs = fake.requests();
        let ws = reqs
            .iter()
            .find(|r| r.method == "workspace.create")
            .unwrap();
        assert_eq!(ws.params["cwd"], "/home/fake/src/app");
        assert_eq!(ws.params["label"], "t-4-resume");
        assert_eq!(
            ws.params["env"],
            serde_json::json!({
                "CLAUDE_CONFIG_DIR": "/home/fake/.claude-personal",
                "PASTOR_TASK": "t-4",
            })
        );
        let start = reqs.iter().find(|r| r.method == "agent.start").unwrap();
        assert_eq!(start.params["name"], "t-4-resume");
        assert_eq!(start.params["kind"], "claude");
        assert_eq!(
            start.params["args"],
            serde_json::json!(["--resume", SESSION])
        );
        assert!(fake.restored().is_empty());

        // Attaching again goes back to the same pane.
        let again = reopen(&fake, &t, &personal()).await.unwrap();
        assert_eq!(again, "t-4-resume");
        let starts = fake
            .requests()
            .iter()
            .filter(|r| r.method == "agent.start")
            .count();
        assert_eq!(starts, 1);
    }

    /// A task with no repo ran in `~/pastor-tasks`, and Claude files its
    /// sessions by directory, so the resume opens there too, not wherever
    /// herdr's focused pane happens to be.
    #[tokio::test]
    async fn a_task_with_no_repo_resumes_in_the_no_repo_folder() {
        let fake = FakeHerdr::new();
        let mut t = closed("claude", Some(SESSION));
        t.spec.repo = None;
        reopen(&fake, &t, &Agents::default()).await.unwrap();
        let reqs = fake.requests();
        let ws = reqs
            .iter()
            .find(|r| r.method == "workspace.create")
            .unwrap();
        assert_eq!(ws.params["cwd"], "/home/fake/pastor-tasks");
    }

    /// Dispatch fell back to the home directory because `~/pastor-tasks`
    /// could not be made, and recorded that on the task. Even though the
    /// folder can be made now, the resume must still open in the home,
    /// where Claude actually filed the session, not wherever `no_repo_dir`
    /// would put a fresh task today.
    #[tokio::test]
    async fn a_home_fallback_resumes_in_the_home_even_once_the_folder_can_be_made() {
        let fake = FakeHerdr::new();
        let mut t = closed("claude", Some(SESSION));
        t.spec.repo = None;
        t.spec.cwd = Some("/home/fake".into());
        reopen(&fake, &t, &Agents::default()).await.unwrap();
        let reqs = fake.requests();
        let ws = reqs
            .iter()
            .find(|r| r.method == "workspace.create")
            .unwrap();
        assert_eq!(ws.params["cwd"], "/home/fake");
    }

    /// While the task's own agent is still listed, attach goes to it.
    #[tokio::test]
    async fn a_live_agent_is_attached_as_it_is() {
        let fake = FakeHerdr::new();
        let created = fake
            .connect()
            .call(
                "workspace.create",
                serde_json::json!({"cwd": null, "label": "t-4"}),
            )
            .await
            .unwrap();
        let pane = created["root_pane"]["pane_id"].as_str().unwrap();
        fake.connect()
            .call(
                "agent.start",
                serde_json::json!({"name": "t-4", "kind": "claude", "pane_id": pane, "args": []}),
            )
            .await
            .unwrap();
        let t = closed("claude", Some(SESSION));
        assert_eq!(reopen(&fake, &t, &Agents::default()).await.unwrap(), "t-4");
        let starts = fake
            .requests()
            .iter()
            .filter(|r| r.method == "agent.start")
            .count();
        assert_eq!(starts, 1, "nothing new started");
    }

    /// A worktree removed at close is put back at its path, on its branch,
    /// before the session is resumed there.
    #[tokio::test]
    async fn a_removed_worktree_is_recreated_first() {
        let fake = FakeHerdr::new();
        fake.set_missing_dir("/wt/app-t-4");
        let mut t = closed("claude", Some(SESSION));
        t.spec.worktree = true;
        t.spec.checkout = Some(Box::new(Checkout {
            branch: "pastor/t-4".into(),
            path: "/wt/app-t-4".into(),
            already_open: false,
        }));
        reopen(&fake, &t, &Agents::default()).await.unwrap();
        assert_eq!(
            fake.restored(),
            vec![(
                "/home/fake/src/app".to_string(),
                "/wt/app-t-4".to_string(),
                "pastor/t-4".to_string()
            )]
        );
        let reqs = fake.requests();
        let ws = reqs
            .iter()
            .find(|r| r.method == "workspace.create")
            .unwrap();
        assert_eq!(ws.params["cwd"], "/wt/app-t-4");

        // A worktree still on disk is left as it is.
        let fake = FakeHerdr::new();
        reopen(&fake, &t, &Agents::default()).await.unwrap();
        assert!(fake.restored().is_empty());
    }

    /// With its branch gone too there is nothing to resume in: it says so,
    /// and opens nothing.
    #[tokio::test]
    async fn a_gone_branch_stops_the_reopen() {
        let fake = FakeHerdr::new();
        fake.set_missing_dir("/wt/app-t-4");
        fake.set_gone_branch("pastor/t-4");
        let mut t = closed("claude", Some(SESSION));
        t.spec.worktree = true;
        t.spec.checkout = Some(Box::new(Checkout {
            branch: "pastor/t-4".into(),
            path: "/wt/app-t-4".into(),
            already_open: false,
        }));
        let err = reopen(&fake, &t, &Agents::default()).await.unwrap_err();
        assert_eq!(err.code, "branch_gone");
        assert!(err.message.contains("pastor/t-4 is gone"), "{err:?}");
        assert!(
            !fake
                .requests()
                .iter()
                .any(|r| r.method.ends_with(".create") || r.method == "agent.start")
        );
    }

    /// An agent with no session support gets a hint that only Claude tasks
    /// can be reopened, before any machine is asked.
    #[tokio::test]
    async fn another_kind_says_only_claude_tasks_reopen() {
        let t = closed("opencode", None);
        let why = why_not(&t, &Agents::default()).unwrap();
        assert!(why.contains("only Claude tasks"), "{why}");
        let fake = FakeHerdr::new();
        let err = reopen(&fake, &t, &Agents::default()).await.unwrap_err();
        assert_eq!(err.code, "no_session");
        assert!(fake.requests().is_empty());

        let t = closed("claude", None);
        let why = why_not(&t, &Agents::default()).unwrap();
        assert!(why.contains("no Claude session"), "{why}");
        assert_eq!(
            why_not(&closed("claude", Some(SESSION)), &Agents::default()),
            None
        );
    }

    /// A recorded session goes into a command line on the machine, so one
    /// that is not a UUID is refused.
    #[tokio::test]
    async fn a_session_that_is_not_a_uuid_is_refused() {
        let fake = FakeHerdr::new();
        let t = closed("claude", Some("x; rm -rf ~"));
        let err = reopen(&fake, &t, &Agents::default()).await.unwrap_err();
        assert_eq!(err.code, "no_session");
        assert!(fake.requests().is_empty());
    }
}
