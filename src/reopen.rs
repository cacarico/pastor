//! `pastor task attach` on a task whose pane is gone: reopen the agent's own
//! session (Claude's `--resume`, agy's `--conversation`; see
//! `task::resume_flag`) in a new pane on the task's machine.
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
    Some(
        if crate::task::resume_flag(agents.kind(&task.spec.agent)).is_some() {
            "no session was recorded for it to reopen".into()
        } else {
            "only Claude or agy tasks can be reopened once their pane is gone".into()
        },
    )
}

/// Leave `task`'s stored workdir bundles out of its reopen on pull machine
/// `machine`, each marked skipped with why, and answer the ids it marked.
/// The head places nothing on a pull machine (its serve places what a
/// claim hands it), and a reopen is no claim. Its own set is not touched:
/// `reopen` still refuses one it cannot place.
pub fn leave_stored_out(task: &mut Task, machine: &str) -> Vec<String> {
    let why =
        format!("{machine} is a pull machine, where a reopen places no stored workdir bundles");
    task.spec
        .workdir_bundles
        .iter_mut()
        .filter(|b| b.skipped.is_none())
        .map(|b| {
            b.skipped = Some(why.clone());
            b.id.clone()
        })
        .collect()
}

/// The agent `pastor task attach` should attach to for `task`, whose pane
/// pastor holds closed: its own agent while herdr still lists it, else an
/// earlier resume that is still open, else a new pane in the task's working
/// directory (the directory dispatch recorded on it for a task with no
/// repo, `task.spec.cwd`), with its agent definition's env, running
/// `claude --resume <session>` or, for agy, `agy --conversation <id>`
/// (`task::resume_flag`). `under` is the env layers below the agent's own
/// (`EnvLayers::merged`: `[defaults]`, the task's flock, its machine), which
/// a dispatch gives the pane too. A worktree removed at close is put back first, at
/// the same path on the same branch, since each files its session by
/// directory. A task's workdir set is placed there through `workdirs`
/// before the agent starts, as at dispatch; one that cannot be placed
/// refuses the reopen (`workdir_failed`).
pub async fn reopen(
    conn: &dyn Connector,
    task: &Task,
    agents: &Agents,
    under: &std::collections::BTreeMap<String, String>,
    workdirs: Option<&dyn crate::bundle::workdir_task::Gate>,
) -> Result<String, ReopenError> {
    let id = task.display_id();
    let machine = task.machine.as_deref().unwrap_or("its machine");
    let kind = agents.kind(&task.spec.agent);
    let Some(flag) = crate::task::resume_flag(kind) else {
        let why = why_not(task, agents).unwrap_or_default();
        return Err(ReopenError::new("no_session", format!("{id}: {why}")));
    };
    let Some(session) = task.spec.session_id.as_deref() else {
        let why = why_not(task, agents).unwrap_or_default();
        return Err(ReopenError::new("no_session", format!("{id}: {why}")));
    };
    if !crate::task::is_resumable_session(kind, session) {
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

    // Its stored workdir bundles, as its last start placed them (one that
    // left them out, on a machine or for a kind that takes no sets, leaves
    // them out here too), then its own set. Only in its own worktree, as
    // at dispatch: in a folder tasks share, a place would replace another
    // task's version of the bundle.
    let own_folder = task
        .spec
        .checkout
        .as_ref()
        .is_some_and(|c| !c.already_open && cwd.as_deref() == Some(c.path.as_str()));
    let mut all = Vec::new();
    if crate::bundle::workdir_stored::places(kind) && own_folder {
        all.extend(
            task.spec
                .workdir_bundles
                .iter()
                .filter(|b| b.skipped.is_none())
                .map(|b| crate::bundle::workdir_task::Placing::Bundle {
                    id: b.id.clone(),
                    kind: kind.to_string(),
                    flock: task.flock.clone(),
                }),
        );
    }
    if let Some(w) = task.spec.workdir.as_deref() {
        use crate::bundle::workdir_push::WORKDIR_FAILED;
        let doc = crate::bundle::workdir_task::settle(&w.set, kind, task.spec.profile()).map_err(
            |r| {
                ReopenError::new(
                    WORKDIR_FAILED,
                    format!("{WORKDIR_FAILED}: {} ({})", r.message, r.code),
                )
            },
        )?;
        all.push(crate::bundle::workdir_task::Placing::Set(doc));
    }
    if !all.is_empty() {
        use crate::bundle::workdir_push::WORKDIR_FAILED;
        let (Some(gate), Some(dir)) = (workdirs, cwd.as_deref()) else {
            return Err(ReopenError::new(
                WORKDIR_FAILED,
                format!("{WORKDIR_FAILED}: {id}'s workdir set has nowhere to be placed from here"),
            ));
        };
        for what in &all {
            gate.place(machine, dir, what)
                .await
                .map_err(|why| ReopenError::new(WORKDIR_FAILED, why))?;
        }
    }

    let mut env = under.clone();
    if let Some(def) = agents.0.get(&task.spec.agent) {
        env.extend(def.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    }
    for (key, value) in env.iter_mut() {
        if value == "~" || value.starts_with("~/") {
            *value = expand_home(conn, &format!("env {key}"), value, Some(machine)).await?;
        }
    }
    // Still an agent pastor started, as far as the head's fleet rules go.
    env.insert(crate::ipc::TASK_ENV.into(), own);
    let created = conn.workspace_create(cwd.as_deref(), &name, &env).await?;
    let args = [flag.to_string(), session.to_string()];
    conn.agent_start(&name, kind, &created.root_pane.pane_id, &args)
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
                now: false,
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
        }
    }

    fn personal() -> Agents {
        let mut agents = Agents::default();
        agents.0.insert(
            "claude-alt".into(),
            crate::config::AgentDef {
                kind: Some("claude".into()),
                env: [("CLAUDE_CONFIG_DIR".to_string(), "~/.claude-alt".to_string())].into(),
                ..Default::default()
            },
        );
        agents
    }

    /// The env layers a dispatch gave the pane (`under`) are in the reopened
    /// pane too, the agent's own `env` over them; the task mark stays last.
    #[tokio::test]
    async fn a_reopened_pane_gets_the_env_layers_under_the_agents_own() {
        let fake = FakeHerdr::new();
        let t = closed("claude-alt", Some(SESSION));
        let under: std::collections::BTreeMap<String, String> = [
            ("BUILD_JOBS", "1"),
            ("CLAUDE_CONFIG_DIR", "/layer"),
            ("PASTOR_TASK", "mine"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .into();
        reopen(&fake, &t, &personal(), &under, None).await.unwrap();
        let reqs = fake.requests();
        let ws = reqs
            .iter()
            .find(|r| r.method == "workspace.create")
            .unwrap();
        assert_eq!(
            ws.params["env"],
            serde_json::json!({
                "BUILD_JOBS": "1",
                "CLAUDE_CONFIG_DIR": "/home/fake/.claude-alt",
                "PASTOR_TASK": "t-4",
            })
        );
    }

    /// A closed claude task: a pane in its directory, with its agent's env,
    /// running `claude --resume <session>`.
    #[tokio::test]
    async fn a_closed_claude_task_resumes_in_a_new_pane() {
        let fake = FakeHerdr::new();
        let t = closed("claude-alt", Some(SESSION));
        let name = reopen(&fake, &t, &personal(), &Default::default(), None)
            .await
            .unwrap();
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
                "CLAUDE_CONFIG_DIR": "/home/fake/.claude-alt",
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
        let again = reopen(&fake, &t, &personal(), &Default::default(), None)
            .await
            .unwrap();
        assert_eq!(again, "t-4-resume");
        let starts = fake
            .requests()
            .iter()
            .filter(|r| r.method == "agent.start")
            .count();
        assert_eq!(starts, 1);
    }

    /// A gate that records each place, with whether herdr had started an
    /// agent yet, and answers `answer`.
    struct Places {
        fake: FakeHerdr,
        answer: Result<(), String>,
        calls: std::sync::Mutex<Vec<(String, String, serde_json::Value, bool)>>,
    }

    impl crate::bundle::workdir_task::Gate for Places {
        /// A set's document, or for a bundle `{"bundle": id, "kind": kind,
        /// "flock": flock}`.
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
            let seen = match what {
                Placing::Set(set) | Placing::Rendered { set, .. } => set.clone(),
                Placing::Bundle { id, kind, flock } => {
                    serde_json::json!({"bundle": id, "kind": kind, "flock": flock})
                }
            };
            self.calls
                .lock()
                .unwrap()
                .push((machine.to_string(), dir.to_string(), seen, started));
            let answer = self.answer.clone().map(|()| true);
            Box::pin(async move { answer })
        }

        fn forget(&self, _machine: &str, _dir: &str) {}
    }

    /// A reopened task's workdir set is placed in the folder its agent
    /// resumes in, with the task's profile, before the agent starts; a set
    /// that cannot be placed refuses the reopen and starts no agent.
    #[tokio::test]
    async fn a_reopened_task_gets_its_workdir_set_before_its_agent() {
        let mut t = closed("claude", Some(SESSION));
        t.spec.workdir = Some(Box::new(crate::task::TaskWorkdir {
            set: serde_json::json!({"id": "reviewer", "kind": "claude", "files": [
                {"path": ".claude/agents/checker.md", "content": "Check.\n"}]}),
            placed_in: Some("/home/fake/src/app".into()),
            render: None,
        }));
        let fake = FakeHerdr::new();
        let gate = Places {
            fake: fake.clone(),
            answer: Ok(()),
            calls: Default::default(),
        };
        reopen(
            &fake,
            &t,
            &Agents::default(),
            &Default::default(),
            Some(&gate),
        )
        .await
        .unwrap();
        let calls = gate.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1, "{calls:?}");
        let (machine, dir, set, started) = &calls[0];
        assert_eq!(machine, "pi-1");
        assert_eq!(dir, "/home/fake/src/app");
        assert_eq!(set["id"], "reviewer");
        assert!(!started, "placed before agent.start");
        assert!(fake.requests().iter().any(|r| r.method == "agent.start"));

        let fake = FakeHerdr::new();
        let gate = Places {
            fake: fake.clone(),
            answer: Err("workdir_failed: the profile file is not pastor's".into()),
            calls: Default::default(),
        };
        let err = reopen(
            &fake,
            &t,
            &Agents::default(),
            &Default::default(),
            Some(&gate),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, crate::bundle::workdir_push::WORKDIR_FAILED);
        assert!(err.message.contains("not pastor's"), "{err:?}");
        assert!(!fake.requests().iter().any(|r| r.method == "agent.start"));

        // With no way to reach the head, the reopen is refused too.
        let fake = FakeHerdr::new();
        let err = reopen(&fake, &t, &Agents::default(), &Default::default(), None)
            .await
            .unwrap_err();
        assert_eq!(err.code, crate::bundle::workdir_push::WORKDIR_FAILED);
        assert!(!fake.requests().iter().any(|r| r.method == "agent.start"));
    }

    /// A pull machine's task reopens without its stored workdir bundles,
    /// each marked skipped, where it would otherwise be refused for having
    /// nowhere to place them from; one already left out keeps its why.
    #[tokio::test]
    async fn a_pull_machine_s_task_reopens_without_its_stored_bundles() {
        let mut t = closed("claude", Some(SESSION));
        t.spec.worktree = true;
        t.spec.checkout = Some(Box::new(Checkout {
            branch: "pastor/t-4".into(),
            path: "/wt/app-t-4".into(),
            already_open: false,
        }));
        t.spec.workdir_bundles = vec![
            crate::task::WorkdirBundle::named("repo-notes"),
            crate::task::WorkdirBundle {
                skipped: Some("not here".into()),
                ..crate::task::WorkdirBundle::named("left-out")
            },
        ];
        let fake = FakeHerdr::new();
        let refused = reopen(&fake, &t, &Agents::default(), &Default::default(), None)
            .await
            .unwrap_err();
        assert_eq!(refused.code, crate::bundle::workdir_push::WORKDIR_FAILED);

        assert_eq!(leave_stored_out(&mut t, "laptop"), ["repo-notes"]);
        let why = t.spec.workdir_bundles[0].skipped.clone().unwrap();
        assert!(why.starts_with("laptop is a pull machine"), "{why}");
        assert_eq!(
            t.spec.workdir_bundles[1].skipped.as_deref(),
            Some("not here")
        );
        let fake = FakeHerdr::new();
        reopen(&fake, &t, &Agents::default(), &Default::default(), None)
            .await
            .unwrap();
        assert!(fake.requests().iter().any(|r| r.method == "agent.start"));
    }

    /// A reopened task's stored workdir bundles go in its own worktree
    /// first, in its flock, then its own set; a bundle its last start left
    /// out is left out again.
    #[tokio::test]
    async fn a_reopened_task_gets_its_workdir_bundles_first() {
        let mut t = closed("claude", Some(SESSION));
        t.flock = Some("ops".into());
        t.spec.worktree = true;
        t.spec.checkout = Some(Box::new(Checkout {
            branch: "pastor/t-4".into(),
            path: "/wt/app-t-4".into(),
            already_open: false,
        }));
        t.spec.workdir_bundles = vec![
            crate::task::WorkdirBundle::named("repo-notes"),
            crate::task::WorkdirBundle {
                skipped: Some("not here".into()),
                ..crate::task::WorkdirBundle::named("left-out")
            },
        ];
        t.spec.workdir = Some(Box::new(crate::task::TaskWorkdir {
            set: serde_json::json!({"id": "reviewer", "kind": "claude", "files": [
                {"path": ".claude/agents/checker.md", "content": "Check.\n"}]}),
            placed_in: None,
            render: None,
        }));
        let fake = FakeHerdr::new();
        let gate = Places {
            fake: fake.clone(),
            answer: Ok(()),
            calls: Default::default(),
        };
        reopen(
            &fake,
            &t,
            &Agents::default(),
            &Default::default(),
            Some(&gate),
        )
        .await
        .unwrap();
        let calls = gate.calls.lock().unwrap().clone();
        let seen: Vec<&serde_json::Value> = calls.iter().map(|c| &c.2).collect();
        assert_eq!(
            seen,
            [
                &serde_json::json!({"bundle": "repo-notes", "kind": "claude", "flock": "ops"}),
                &calls[1].2
            ]
        );
        assert_eq!(calls[1].2["id"], "reviewer");
        assert!(calls.iter().all(|c| !c.3), "all before agent.start");
        assert!(calls.iter().all(|c| c.1 == "/wt/app-t-4"), "{calls:?}");

        // In the repo it shares with other tasks, only its own set goes in.
        t.spec.worktree = false;
        t.spec.checkout = None;
        let fake = FakeHerdr::new();
        let gate = Places {
            fake: fake.clone(),
            answer: Ok(()),
            calls: Default::default(),
        };
        reopen(
            &fake,
            &t,
            &Agents::default(),
            &Default::default(),
            Some(&gate),
        )
        .await
        .unwrap();
        let calls = gate.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0].2["id"], "reviewer");
    }

    /// A task with no repo ran in `~/pastor-tasks`, and Claude files its
    /// sessions by directory, so the resume opens there too, not wherever
    /// herdr's focused pane happens to be.
    #[tokio::test]
    async fn a_task_with_no_repo_resumes_in_the_no_repo_folder() {
        let fake = FakeHerdr::new();
        let mut t = closed("claude", Some(SESSION));
        t.spec.repo = None;
        reopen(&fake, &t, &Agents::default(), &Default::default(), None)
            .await
            .unwrap();
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
        reopen(&fake, &t, &Agents::default(), &Default::default(), None)
            .await
            .unwrap();
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
        assert_eq!(
            reopen(&fake, &t, &Agents::default(), &Default::default(), None)
                .await
                .unwrap(),
            "t-4"
        );
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
        reopen(&fake, &t, &Agents::default(), &Default::default(), None)
            .await
            .unwrap();
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
        reopen(&fake, &t, &Agents::default(), &Default::default(), None)
            .await
            .unwrap();
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
        let err = reopen(&fake, &t, &Agents::default(), &Default::default(), None)
            .await
            .unwrap_err();
        assert_eq!(err.code, "branch_gone");
        assert!(err.message.contains("pastor/t-4 is gone"), "{err:?}");
        assert!(
            !fake
                .requests()
                .iter()
                .any(|r| r.method.ends_with(".create") || r.method == "agent.start")
        );
    }

    /// An agent with no session support gets a hint that only Claude or agy
    /// tasks can be reopened, before any machine is asked.
    #[tokio::test]
    async fn another_kind_says_only_claude_or_agy_tasks_reopen() {
        let t = closed("opencode", None);
        let why = why_not(&t, &Agents::default()).unwrap();
        assert!(why.contains("only Claude or agy tasks"), "{why}");
        let fake = FakeHerdr::new();
        let err = reopen(&fake, &t, &Agents::default(), &Default::default(), None)
            .await
            .unwrap_err();
        assert_eq!(err.code, "no_session");
        assert!(fake.requests().is_empty());

        let t = closed("claude", None);
        let why = why_not(&t, &Agents::default()).unwrap();
        assert!(why.contains("no session was recorded"), "{why}");
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
        let err = reopen(&fake, &t, &Agents::default(), &Default::default(), None)
            .await
            .unwrap_err();
        assert_eq!(err.code, "no_session");
        assert!(fake.requests().is_empty());
    }

    /// An agy task's recorded conversation id (read from herdr's own hook,
    /// not chosen by pastor) resumes it with `agy --conversation <id>`,
    /// exactly as Claude's own session resumes with `--resume`.
    #[tokio::test]
    async fn a_closed_agy_task_resumes_with_its_conversation_id() {
        const CONVERSATION: &str = "9f1e2d3c-4b5a-6978-8f90-123456789abc";
        let fake = FakeHerdr::new();
        let t = closed("agy", Some(CONVERSATION));
        let name = reopen(&fake, &t, &Agents::default(), &Default::default(), None)
            .await
            .unwrap();
        assert_eq!(name, "t-4-resume");
        let reqs = fake.requests();
        let start = reqs.iter().find(|r| r.method == "agent.start").unwrap();
        assert_eq!(start.params["kind"], "agy");
        assert_eq!(
            start.params["args"],
            serde_json::json!(["--conversation", CONVERSATION])
        );
    }

    /// A machine with no agy hook installed never records a conversation
    /// id, so there is nothing to reopen: the same `no_session` an agy task
    /// gets as a Claude one with no session, not the "only Claude or agy"
    /// hint, which is for kinds pastor never resumes at all.
    #[tokio::test]
    async fn an_agy_task_with_no_recorded_conversation_has_nothing_to_reopen() {
        let t = closed("agy", None);
        let why = why_not(&t, &Agents::default()).unwrap();
        assert!(why.contains("no session was recorded"), "{why}");
        let fake = FakeHerdr::new();
        let err = reopen(&fake, &t, &Agents::default(), &Default::default(), None)
            .await
            .unwrap_err();
        assert_eq!(err.code, "no_session");
        assert!(fake.requests().is_empty());
    }
}
