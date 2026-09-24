# pastor jobs and schedules implementation plan (plan 2 of 4)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `pastor serve` runs recurring jobs from `~/.config/pastor/jobs/*.toml`: on a schedule it asks a connector for items, drops the ones it has seen, renders a prompt per new item and queues a task, with a scheduler that runs beside the accept loop instead of inside it.

**Architecture:** A `Scheduler` tokio task owns the tick: it reloads job files that changed, starts due jobs (each run its own task, so a slow connector never blocks a tick), and runs the dispatch pass under one lock shared with `pastor run`. Job runs go through a small `ItemSource` seam whose only implementation here is the built-in `clock`; plan 3 plugs process connectors into the same seam. Seen keys, per-job cursor and last-run state live in two new SQLite tables. Dispatch claims a task with a conditional `UPDATE ... WHERE state = 'queued'` and `update_task` refuses to overwrite a row someone else changed. The readiness poll reads herdr's `launch_pending`/`interactive_ready` flags, and a machine whose requests work but whose event stream will not open is `polling`, not lost.

**Tech Stack:** Rust 2024, tokio, clap (derive), serde/serde_json, toml, rusqlite (bundled), chrono (`Local` for cron), tracing. No new runtime dependencies: the 5-field cron matcher and the `{{ a.b }}` substitution are each under 150 lines and the spec forbids template logic, so `croner`/`minijinja` would be weight without use. Dev: tempfile.

**Spec:** `docs/superpowers/specs/2026-09-23-pastor-design.md`, sections "Jobs and schedules", "Dispatch and tasks" (steps 1 and 4), "Files, config and systemd" (Reload), "Error handling", "Testing". `AGENTS.md` "Known gaps, parked for the next plans → Plan 2" lists the concurrency items folded in here.

**Not in this plan (stated so nobody looks for it):** process connectors, plugin manifests, `.env` loading, event hooks and `events.jsonl` (plan 3); hot reload of `flock.toml` and `pastor.toml` (a machine actor cannot yet be added or removed at runtime; job files do reload); `task retry|close|prune` and systemd (plan 4).

## Global Constraints

- Vocabulary is fixed by the spec: machine, flock, head, job, task, plugin, connector, agent. The job-side trait is named `ItemSource` in code only because `herdr::Connector` already means the transport; user-facing text says "connector".
- Job files: one per job, `~/.config/pastor/jobs/<name>.toml`. `name` defaults to the file stem and must equal it when given. Job names match `[a-z0-9][a-z0-9_.-]{0,63}` and `run` is reserved for one-off tasks (`tasks.job = 'run'`).
- `every` takes durations (`30s`, `5m`, `1h`, `1d`, same parser as `pastor.toml`). `cron` takes 5-field cron in the head's local time. Exactly one is required.
- Overdue on daemon start runs once. Missed runs are not replayed.
- A job never overlaps itself: a run due while the previous run is still going is skipped and logged. `pastor job run <name>` ignores schedule and overlap.
- Items carry a stable `key`. The seen-store is keyed by `(job, key)`. Seen keys are dropped. A run creates at most `max_tasks_per_run` tasks; the rest are logged and stay unseen. Duplicate key in one run: first wins.
- First run passes `since = now - backfill` and `cursor = null`. Later runs pass `since` = start of the last successful run and the last persisted cursor.
- Templates: `{{ item.* }}`, `{{ job.name }}`, `{{ task.id }}` in `prompt`, `branch`, `repo`. Substitution only, no logic. `task.id` renders as `t-<n>`.
- `connector.use = "clock"` is built in and emits one item per run with `key` = the run time (RFC 3339, seconds). Any other `use` makes the job `invalid` until plan 3 ships plugins; the error says so.
- Connector failure: `job.failed` event, cursor kept, backoff `min(1h, 60s * 2^(failures-1))`, reset on success.
- Queued tasks: retried each tick, oldest first; one warning per task after 1h queued.
- `pastor.toml` gains `request_timeout` (default `60s`) and `agent_ready_timeout` (default `30s`); the second must be shorter than the first, and neither may be zero.
- Channel states: `connecting`, `connected`, `reconnecting`, `polling`, `incompatible`. `polling` = requests answer but `events.subscribe` does not open; the machine is dispatched to and reconciled each `tick`.
- Readiness after `agent.start` (herdr 0.9.1 `AgentInfo`): `launch_pending` → keep waiting; `interactive_ready`, or status `working`/`blocked` → prompt; anything else (idle/done/unknown with neither flag) → the process exited, fail now; absent from `agent.list` → exited, fail now.
- Runtime CLI errors: JSON on stderr `{"code":"...","message":"..."}`, exit 1. Usage errors keep clap's text, exit 2.
- `make check` (fmt check, clippy `-D warnings`, full suite) before every commit. Nothing in the suite talks to a real herdr.
- Commits: conventional prefix, plain subject, body says why. No `Co-Authored-By` or other trailers (the git hook adds `Assisted-by:`). Work stays on `feat/core`'s successor branch `feat/jobs`, opened as a PR; never push `main`.
- pastor never closes panes, kills agents or removes worktrees on its own.

## Review Focus

1. A job file with both `every` and `cron`, or neither, or a `name` that does not match its file name, must be reported `invalid` in `pastor job list` and never run (Task 8 `exactly_one_schedule_and_name_must_match_stem`).
2. A connector that emits the same key twice in one run, or a key seen in an earlier run, must produce one task, never two, and a key beyond `max_tasks_per_run` must stay unseen for the next run (Task 10 `seen_keys_and_in_run_duplicates_create_one_task`, `max_tasks_per_run_defers_the_rest_unseen`).
3. A job whose connector is still running when it comes due again must be skipped and logged, not started twice (Task 11 `overlapping_run_is_skipped`).
4. A prompt naming an item field the connector did not emit must still produce a task, with that placeholder empty and a warning, not an item that fails every run forever (Task 10 `missing_item_field_renders_empty_and_warns`).
5. Two queued tasks and one free slot must end as one running and one queued, whether the passes are concurrent (a tick and a `pastor run`) or one pass places them back to back; the second case was observed over-dispatching on 2026-09-24 (Task 4 `live_count_is_current_when_dispatch_replies`, Task 11 `concurrent_dispatch_passes_do_not_over_dispatch`).
6. A job file edited into unparsable TOML while the daemon runs must keep the previous version running and show the parse error in `job list` (Task 11 `invalid_edit_keeps_previous_job_and_reports_error`).

---

## File structure

```
src/config/mod.rs        + request_timeout, agent_ready_timeout keys; Paths::jobs_dir
src/config/job.rs        NEW  JobFile (TOML shape), Job (validated), load_dir, set_enabled
src/schedule.rs          NEW  Schedule { Every, Cron }, CronExpr parser and next_after
src/template.rs          NEW  {{ a.b }} substitution and load-time placeholder check
src/connector/mod.rs     NEW  Item, RunInput, RunOutput, ItemSource trait, builtin lookup
src/connector/clock.rs   NEW  the built-in clock source
src/scheduler.rs         NEW  run_job (one job pass), Scheduler task, JobStatus, tick reports
src/store.rs             schema v2: seen + job_state; optimistic update_task; claim_task; insert_job_task
src/herdr/protocol.rs    AgentInfo.launch_pending
src/herdr/fake.rs        launch_pending/interactive_ready modelling; exit_agents_listed
src/dispatch.rs          readiness rule from the two flags
src/machine.rs           claim in SQL; live count before reply; Polling state; poll_every; PastorEvent.job
src/daemon.rs            Fleet (machines + dispatch lock), scheduler task, new IPC ops
src/ipc.rs               Tick, Reload, JobList, JobRun requests; Runs, Jobs responses
src/cli.rs               job and run-report table rows
src/main.rs              job list|enable|disable|run, tick, reload
tests/cli.rs             end-to-end clock job against fake-herdr
README.md, AGENTS.md, docs/superpowers/specs/2026-09-23-pastor-design.md, contrib/completions/
```

Branch: `git checkout -b feat/jobs` from `feat/core` before Task 1 (PR #1 may still be open; plan 2's PR targets `feat/core` until #1 merges, then `main`).

---

### Task 1: Timeout config keys and the jobs directory

**Files:**
- Modify: `src/config/mod.rs` (struct `PastorConfig`, `load`, accessors, `impl Paths`, tests)
- Modify: `src/daemon.rs:33-37` (`MachineSettings` built in `Daemon::start`)

**Interfaces:**
- Produces:
  ```rust
  // src/config/mod.rs
  pub struct PastorConfig { pub tick: String, pub settle: String, pub reconcile_every: String,
                            pub request_timeout: String, pub agent_ready_timeout: String, pub defaults: Defaults }
  impl PastorConfig { pub fn request_timeout_duration(&self) -> Duration; pub fn agent_ready_timeout_duration(&self) -> Duration }
  impl Paths { pub fn jobs_dir(&self) -> PathBuf }   // <config_dir>/jobs
  ```

- [ ] **Step 1: Write the failing tests**

Add inside `mod tests` in `src/config/mod.rs`:

```rust
    #[test]
    fn timeout_keys_default_and_parse() {
        let cfg = PastorConfig::default();
        assert_eq!(cfg.request_timeout_duration(), Duration::from_secs(60));
        assert_eq!(cfg.agent_ready_timeout_duration(), Duration::from_secs(30));
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.toml");
        std::fs::write(&path, "request_timeout = \"90s\"\nagent_ready_timeout = \"45s\"\n").unwrap();
        let cfg = PastorConfig::load(&path).unwrap();
        assert_eq!(cfg.request_timeout_duration(), Duration::from_secs(90));
        assert_eq!(cfg.agent_ready_timeout_duration(), Duration::from_secs(45));
    }

    /// The readiness wait runs inside the request timeout that bounds a whole
    /// dispatch; a config that inverts them would report every slow agent as
    /// a wedged machine (see `MachineSettings::agent_ready_timeout`).
    #[test]
    fn ready_timeout_must_be_shorter_than_request_timeout_and_nonzero() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.toml");
        std::fs::write(&path, "request_timeout = \"30s\"\nagent_ready_timeout = \"30s\"\n").unwrap();
        let err = PastorConfig::load(&path).unwrap_err().to_string();
        assert!(err.contains("agent_ready_timeout"), "{err}");
        assert!(err.contains("shorter than request_timeout"), "{err}");
        std::fs::write(&path, "agent_ready_timeout = \"0s\"\n").unwrap();
        let err = PastorConfig::load(&path).unwrap_err().to_string();
        assert!(err.contains("must not be zero"), "{err}");
        std::fs::write(&path, "request_timeout = \"0s\"\n").unwrap();
        let err = PastorConfig::load(&path).unwrap_err().to_string();
        assert!(err.contains("request_timeout"), "{err}");
    }

    #[test]
    fn jobs_dir_lives_under_config() {
        let p = Paths::new("/tmp/c", "/tmp/s");
        assert_eq!(p.jobs_dir(), PathBuf::from("/tmp/c/jobs"));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib config::`
Expected: compile errors: no field `request_timeout`, no method `jobs_dir`.

- [ ] **Step 3: Implement**

In `src/config/mod.rs`, replace the `PastorConfig` struct and its `Default` with:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PastorConfig {
    pub tick: String,
    pub settle: String,
    pub reconcile_every: String,
    /// Bound on one herdr request, connect included. A machine that does not
    /// answer within it is treated as lost.
    pub request_timeout: String,
    /// How long dispatch waits between `agent.start` and a prompt herdr
    /// accepts. Runs inside `request_timeout`, so it must be shorter.
    pub agent_ready_timeout: String,
    pub defaults: Defaults,
}

impl Default for PastorConfig {
    fn default() -> Self {
        PastorConfig {
            tick: "10s".into(),
            settle: "10s".into(),
            reconcile_every: "60s".into(),
            request_timeout: "60s".into(),
            agent_ready_timeout: "30s".into(),
            defaults: Defaults::default(),
        }
    }
}
```

In `PastorConfig::load`, extend the validation list and add the ordering check after the loop:

```rust
        for (name, v, zero_ok) in [
            ("tick", &cfg.tick, false),
            ("settle", &cfg.settle, false),
            ("reconcile_every", &cfg.reconcile_every, false),
            ("request_timeout", &cfg.request_timeout, false),
            ("agent_ready_timeout", &cfg.agent_ready_timeout, false),
            ("defaults.timeout", &cfg.defaults.timeout, true),
        ] {
            let d = parse_duration(v)
                .map_err(|e| anyhow::anyhow!("{}: {name}: {e}", path.display()))?;
            if !zero_ok && d.is_zero() {
                anyhow::bail!("{}: {name}: must not be zero", path.display());
            }
        }
        if cfg.agent_ready_timeout_duration() >= cfg.request_timeout_duration() {
            anyhow::bail!(
                "{}: agent_ready_timeout must be shorter than request_timeout",
                path.display()
            );
        }
        Ok(cfg)
```

Add the accessors next to `timeout_duration`:

```rust
    pub fn request_timeout_duration(&self) -> Duration {
        duration_or_default(
            &self.request_timeout,
            &PastorConfig::default().request_timeout,
        )
    }
    pub fn agent_ready_timeout_duration(&self) -> Duration {
        duration_or_default(
            &self.agent_ready_timeout,
            &PastorConfig::default().agent_ready_timeout,
        )
    }
```

Add to `impl Paths`:

```rust
    /// One TOML file per job. Created by the user; pastor only reads and, for
    /// `job enable|disable`, rewrites one line of it.
    pub fn jobs_dir(&self) -> PathBuf {
        self.config_dir.join("jobs")
    }
```

In `src/daemon.rs` `Daemon::start`, wire the keys through:

```rust
        let settings = MachineSettings {
            settle: config.settle_duration(),
            reconcile_every: config.reconcile_duration(),
            request_timeout: config.request_timeout_duration(),
            agent_ready_timeout: config.agent_ready_timeout_duration(),
            ..Default::default()
        };
```

- [ ] **Step 4: Run the tests**

Run: `make check`
Expected: all green.

- [ ] **Step 5: Commit**

```bash
git add src/config/mod.rs src/daemon.rs
git commit -m "feat: request_timeout and agent_ready_timeout config keys" -m "Both bounds were hard-coded in MachineSettings; a slow fleet machine had no way to widen them. The ordering check lives in config load so a bad file fails at startup, not as a confusing 'request timed out' on the first dispatch."
```

---

### Task 2: Readiness from herdr's launch flags

**Files:**
- Modify: `src/herdr/protocol.rs:76-92` (`AgentInfo`)
- Modify: `src/herdr/fake.rs` (`State`, `agent.start`, `agent.prompt`, `agent.list`, new knob)
- Modify: `src/dispatch.rs:166-204` (`prompt_when_ready`), tests

**Interfaces:**
- Produces:
  ```rust
  // src/herdr/protocol.rs
  pub struct AgentInfo { /* existing */ #[serde(default)] pub launch_pending: bool, #[serde(default)] pub interactive_ready: bool }
  // src/herdr/fake.rs
  impl FakeHerdr { pub fn exit_agents_listed(&self, yes: bool) }  // started agents stay in agent.list, dead: no flags, idle
  ```
- Consumes: `dispatch::DispatchError::Task`, `FakeHerdr::set_ready_after`, `exit_agents_on_start`.

Why: herdr 0.9.1's `AgentInfo` (`src/api/schema/agents.rs`) reports `launch_pending` while the managed agent is in its pending or blocked-at-startup phase and `interactive_ready` once it is active. herdr's own `agent start --wait` (`src/cli/agent.rs`) decides with exactly these: `launch_pending` → wait; `interactive_ready` → ready; idle/done with neither → "agent process exited before becoming interactive". pastor's poll today (`agent_status != unknown`) prompts a dead-but-still-listed agent until the bound elapses.

- [ ] **Step 1: Write the failing tests**

In `src/herdr/fake.rs` `mod tests`, add:

```rust
    /// herdr keeps a pane whose managed agent died in `agent.list`, with neither
    /// launch flag set. The fake must model that shape, distinct from
    /// `exit_agents_on_start` (gone from the list altogether).
    #[tokio::test]
    async fn an_exited_agent_can_stay_listed_without_flags() {
        let fake = FakeHerdr::new();
        fake.exit_agents_listed(true);
        let created = fake.workspace_create(None, "t-1").await.unwrap();
        fake.agent_start("t-1", "claude", &created.root_pane.pane_id, &[])
            .await
            .unwrap();
        let list = fake.agent_list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert!(!list[0].launch_pending);
        assert!(!list[0].interactive_ready);
        assert_eq!(list[0].agent_status, AgentStatus::Idle);
        let err = fake.agent_prompt("t-1", "hi").await.unwrap_err();
        assert_eq!(err.code(), Some("agent_not_ready"));
    }

    #[tokio::test]
    async fn launch_flags_follow_the_ready_window() {
        let fake = FakeHerdr::new();
        fake.set_ready_after(Duration::from_millis(200));
        let created = fake.workspace_create(None, "t-1").await.unwrap();
        fake.agent_start("t-1", "claude", &created.root_pane.pane_id, &[])
            .await
            .unwrap();
        let a = &fake.agent_list().await.unwrap()[0];
        assert!(a.launch_pending && !a.interactive_ready, "{a:?}");
        tokio::time::sleep(Duration::from_millis(250)).await;
        let a = &fake.agent_list().await.unwrap()[0];
        assert!(!a.launch_pending && a.interactive_ready, "{a:?}");
    }
```

In `src/dispatch.rs` `mod tests`, add:

```rust
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
        assert!(message.contains("exited before becoming interactive"), "{message}");
        assert!(message.contains("pi-1"), "{message}");
        assert!(!err.is_transport());
        assert_eq!(t.state, TaskState::Failed);
        assert!(
            !fake.requests().iter().any(|r| r.method == "agent.prompt"),
            "a dead agent must not be prompted"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib herdr::fake:: dispatch::`
Expected: compile error, no method `exit_agents_listed`, no field `launch_pending`.

- [ ] **Step 3: Implement**

`src/herdr/protocol.rs`, in `AgentInfo` after `state_change_seq`:

```rust
    /// herdr 0.9.1: the managed agent is still in its pending phase (or blocked
    /// at startup); `agent.prompt` answers `agent_not_ready` while this is set.
    #[serde(default)]
    pub launch_pending: bool,
    /// herdr 0.9.1: the managed agent is active and accepts prompts.
    #[serde(default)]
    pub interactive_ready: bool,
```

(`interactive_ready` already exists; keep its position, add `launch_pending` next to it. Fix every `AgentInfo { .. }` literal in the crate: `src/herdr/fake.rs` `agent.start` and `agent.list`; `grep -rn "interactive_ready" src tests` finds them all.)

`src/herdr/fake.rs`:

In `State` add `exit_listed: bool,`. Add the knob next to `exit_agents_on_start`:

```rust
    /// The next started agents die at once but their pane stays in `agent.list`
    /// with neither `launch_pending` nor `interactive_ready`, as herdr reports a
    /// managed agent whose process exited before becoming interactive.
    pub fn exit_agents_listed(&self, yes: bool) {
        self.state.lock().unwrap().exit_listed = yes;
    }
```

In `handle`, `"agent.start"`: build `info` with `launch_pending: false, interactive_ready: true` and, before the `exit_on_start` check:

```rust
                if s.exit_listed {
                    let dead = AgentInfo {
                        interactive_ready: false,
                        launch_pending: false,
                        ..info.clone()
                    };
                    s.agents.insert(pane_id, dead.clone());
                    return Ok(json!({"type": "agent_started", "agent": dead, "argv": []}));
                }
```

In `"agent.prompt"`, after the `launching` check, refuse an agent that is not interactive:

```rust
                let Some(a) = s
                    .agents
                    .values_mut()
                    .find(|a| a.name.as_deref() == Some(target) || a.pane_id == target)
                else {
                    return Err(("agent_not_found".into(), target.into()));
                };
                if !a.interactive_ready {
                    return Err((
                        "agent_not_ready".into(),
                        format!("agent {target} is not an active named agent"),
                    ));
                }
```

In `"agent.list"`, the launching override becomes:

```rust
                            AgentInfo {
                                agent_status: AgentStatus::Unknown,
                                launch_pending: true,
                                interactive_ready: false,
                                ..a.clone()
                            }
```

`src/dispatch.rs`, replace the body of the loop in `prompt_when_ready`:

```rust
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
        if agent.launch_pending {
            // still launching: fall through to the wait below
        } else if can_prompt {
            match conn.agent_prompt(name, &task.prompt).await {
                Ok(_) => return Ok(DispatchOutcome::Running),
                // The agent is up and waiting for a human, not for us.
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
```

Check the existing tests still hold: `dispatch_waits_for_a_launching_agent` (flags flip after `ready_after`), `a_blocked_agent_blocks_the_task` (status blocked → `can_prompt` → `agent_blocked`), `an_agent_that_never_becomes_ready_fails_with_the_bound` (`launch_pending` for 30s → bound), `an_agent_that_exits_on_start_fails_immediately` (absent → exited).

Also update `tests/real_herdr.rs` if it constructs `AgentInfo` (it should not; `grep`).

- [ ] **Step 4: Run the tests**

Run: `make check`
Expected: green, including `make test-machine`-style timing tests (`cargo test --lib machine::` once is enough here).

- [ ] **Step 5: Commit**

```bash
git add src/herdr/protocol.rs src/herdr/fake.rs src/dispatch.rs
git commit -m "fix: read herdr's launch flags to tell launching from exited" -m "agent_status != unknown mistook a pane whose agent had died for one that was ready and prompted it until the 30s bound. herdr 0.9.1 reports launch_pending and interactive_ready, the pair its own 'agent start --wait' uses; a listed agent with neither is an exited process and fails at once."
```

---

### Task 3: Store: claim a task in SQL, refuse stale updates

**Files:**
- Modify: `src/store.rs` (`update_task`, new `claim_task`, new `Conflict`, tests)
- Modify: `src/machine.rs` (every `update_task(&t)` call site becomes `&mut`; `run_dispatch`)
- Modify: `src/daemon.rs` tests (none call `update_task`; verify with grep)

**Interfaces:**
- Produces:
  ```rust
  // src/store.rs
  #[derive(Debug, thiserror::Error)]
  #[error("task t-{id} changed underneath this update; reload it and apply again")]
  pub struct Conflict { pub id: i64 }
  impl Store {
      /// Optimistic: the row must still carry `t.updated_at`. On success `t.updated_at` is
      /// advanced to the value written. `Err` downcasts to `Conflict` when the row moved on.
      pub fn update_task(&self, t: &mut Task) -> anyhow::Result<()>;
      /// `queued` -> `starting` on `machine`, atomically. `None`: the task was not queued
      /// (already claimed, finished, or unknown).
      pub fn claim_task(&self, id: i64, machine: &str) -> anyhow::Result<Option<Task>>;
  }
  ```
- Consumes: `Task::agent_name_for`, `TaskState`.

Why: `update_task` wrote every column unconditionally, so two holders of the same row (today: an actor and a future `task close`; plan 4 adds the CLI as a writer) silently clobbered each other. The claim moves the one legal `queued -> starting` transition into a conditional `UPDATE`, so concurrent dispatch passes cannot both take a task. `Observed::DispatchStarting` and its tests stay: they document the rule the SQL enforces.

- [ ] **Step 1: Write the failing tests**

In `src/store.rs` `mod tests`, add:

```rust
    #[test]
    fn update_task_refuses_a_stale_copy() {
        let s = Store::open_in_memory().unwrap();
        let t = s.insert_task(new_task("run")).unwrap();
        let mut a = s.get_task(t.id).unwrap().unwrap();
        let mut b = s.get_task(t.id).unwrap().unwrap();
        a.state = TaskState::Running;
        s.update_task(&mut a).unwrap();
        assert!(
            a.updated_at > b.updated_at,
            "a successful update must advance the in-memory updated_at"
        );
        b.state = TaskState::Closed;
        let err = s.update_task(&mut b).unwrap_err();
        assert!(err.downcast_ref::<Conflict>().is_some(), "{err}");
        assert_eq!(
            s.get_task(t.id).unwrap().unwrap().state,
            TaskState::Running,
            "the stale write must not land"
        );
        // The fresh copy keeps working, and a second write on it too.
        a.state = TaskState::Done;
        s.update_task(&mut a).unwrap();
        a.state = TaskState::Closed;
        s.update_task(&mut a).unwrap();
        assert_eq!(s.get_task(t.id).unwrap().unwrap().state, TaskState::Closed);
    }

    #[test]
    fn update_task_still_reports_a_missing_row() {
        let s = Store::open_in_memory().unwrap();
        let mut t = s.insert_task(new_task("run")).unwrap();
        t.id = 99;
        let err = s.update_task(&mut t).unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
        assert!(err.downcast_ref::<Conflict>().is_none());
    }

    #[test]
    fn claim_task_is_exclusive() {
        let s = Store::open_in_memory().unwrap();
        let t = s.insert_task(new_task("run")).unwrap();
        let claimed = s.claim_task(t.id, "pi-3").unwrap().expect("first claim wins");
        assert_eq!(claimed.state, TaskState::Starting);
        assert_eq!(claimed.machine.as_deref(), Some("pi-3"));
        assert_eq!(claimed.agent_name.as_deref(), Some("t-1"));
        assert!(
            s.claim_task(t.id, "pi-1").unwrap().is_none(),
            "a second claim must find nothing to claim"
        );
        assert!(s.claim_task(99, "pi-1").unwrap().is_none());
        // A claimed copy is fresh: updating it must not conflict.
        let mut c = claimed;
        c.state = TaskState::Running;
        s.update_task(&mut c).unwrap();
    }
```

Change every existing `s.update_task(&x)` in the store tests to `s.update_task(&mut x)` (the bindings are already `mut`).

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib store::`
Expected: compile errors (`claim_task`, `Conflict`, `&mut`).

- [ ] **Step 3: Implement**

`src/store.rs`, above `impl Store`:

```rust
/// `update_task` found the row changed since this copy was read. The caller holds
/// stale data; reload and decide again rather than overwrite.
#[derive(Debug, thiserror::Error)]
#[error("task t-{id} changed underneath this update; reload it and apply again")]
pub struct Conflict {
    pub id: i64,
}
```

Replace `update_task`:

```rust
    /// Optimistic: the write lands only if the row still carries `t.updated_at`.
    /// On success `t.updated_at` is advanced to the value written, so the same
    /// copy can be updated again. A row that moved on is a `Conflict`; a row
    /// that is gone is a plain error.
    pub fn update_task(&self, t: &mut Task) -> anyhow::Result<()> {
        // Strictly later than the value being replaced, so two writes within one
        // clock tick still produce distinct stamps and the next check can tell
        // them apart.
        let now = Utc::now().max(t.updated_at + chrono::Duration::nanoseconds(1));
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE tasks SET machine = ?2, workspace_id = ?3, pane_id = ?4, agent_name = ?5, state = ?6, error = ?7,
                last_completion_seq = ?8, started_at = ?9, finished_at = ?10, updated_at = ?11, prompt = ?12, spec = ?13
             WHERE id = ?1 AND updated_at = ?14",
            params![
                t.id,
                t.machine,
                t.workspace_id,
                t.pane_id,
                t.agent_name,
                t.state.as_str(),
                t.error,
                t.last_completion_seq.map(|v| v as i64),
                t.started_at.map(|d| d.to_rfc3339()),
                t.finished_at.map(|d| d.to_rfc3339()),
                now.to_rfc3339(),
                t.prompt,
                serde_json::to_string(&t.spec)?,
                t.updated_at.to_rfc3339(),
            ],
        )?;
        if n == 1 {
            t.updated_at = now;
            return Ok(());
        }
        let exists: bool = conn.query_row(
            "SELECT COUNT(*) FROM tasks WHERE id = ?1",
            params![t.id],
            |r| r.get::<_, i64>(0),
        )? > 0;
        if exists {
            Err(Conflict { id: t.id }.into())
        } else {
            anyhow::bail!("task {} not found", t.id)
        }
    }

    /// The one transition dispatch is allowed to make on its own, done in SQL so
    /// concurrent dispatch passes cannot both take a task: `queued` -> `starting`
    /// on `machine`, with the agent name dispatch will use. `None` means the task
    /// was not queued any more (or never existed). This is `Observed::DispatchStarting`
    /// as a conditional UPDATE; `task::next_state` keeps the rule readable.
    pub fn claim_task(&self, id: i64, machine: &str) -> anyhow::Result<Option<Task>> {
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE tasks SET state = 'starting', machine = ?2, agent_name = ?3, error = NULL, updated_at = ?4
             WHERE id = ?1 AND state = 'queued'",
            params![id, machine, Task::agent_name_for(id), now],
        )?;
        drop(conn);
        if n == 0 {
            return Ok(None);
        }
        self.get_task(id)
    }
```

`src/machine.rs`: replace the start of `run_dispatch` (from `let mut task = match self.store.get_task(task_id)` through the `update_task(&task)` that persists `Starting`) with:

```rust
        // The claim is the `Queued -> Starting` transition done as a conditional
        // UPDATE: a task another pass already took, or that finished meanwhile,
        // is simply not claimable. It also persists `machine` and `agent_name`
        // before the first herdr call, so a daemon crash mid-dispatch leaves a
        // row `reconcile` can adopt by agent name instead of one that still reads
        // `Queued` and gets dispatched twice.
        let mut task = match self.store.claim_task(task_id, &self.name) {
            Ok(Some(t)) => t,
            Ok(None) => {
                return (
                    Err(anyhow::anyhow!(
                        "task t-{task_id} is not queued (already claimed, finished, or unknown)"
                    )),
                    false,
                );
            }
            Err(e) => return (Err(e), false),
        };
```

Then, still in `machine.rs`, make every remaining `self.store.update_task(&x)` take `&mut x`:
- `run_dispatch`: the two calls after the timeout (`&mut task`).
- `apply`: `fn apply(&mut self, mut task: Task, ...)` already binds `mut task`; use `&mut task`.
- `reconcile`: `let mut t = task;` branches use `&mut t`; the stale branch `self.store.update_task(&t)?` becomes `let mut t = task; t.state = TaskState::Stale; self.store.update_task(&mut t)?;` (it already is `let mut t`).
- Tests: `store.update_task(&t)` → `store.update_task(&mut t)` (the `t` bindings are `let mut t`).

Remove the now-unused `use crate::task::{Observed, ...}` only if the compiler says so (`Observed` is still used in `handle_event`/`reconcile`).

- [ ] **Step 4: Run the tests**

Run: `make check && make test-machine`
Expected: green. `reconcile_adopts_a_starting_task_by_agent_name` and `reconcile_fails_a_starting_task_with_no_agent` set up `Starting` rows by hand and are unaffected.

- [ ] **Step 5: Commit**

```bash
git add src/store.rs src/machine.rs
git commit -m "feat: claim tasks in SQL and refuse stale task updates" -m "update_task overwrote every column with whatever the caller held; a second writer (the CLI in plan 4, a concurrent pass today) would clobber state silently. The write now requires the row's updated_at to match and reports a Conflict otherwise. The queued->starting step is a conditional UPDATE so two dispatch passes cannot both take one task."
```

---

### Task 4: Machine actor: accurate live count, `polling` state, event shape

**Files:**
- Modify: `src/machine.rs` (`ChannelState`, `MachineSettings`, `PastorEvent`, `handle_command`, `Actor::run`, new `poll_until_subscribed`, tests)
- Modify: `src/daemon.rs` (`views()` health rule, the events log line, `MachineSettings` construction)

**Interfaces:**
- Produces:
  ```rust
  // src/machine.rs
  pub enum ChannelState { Connecting, Connected, Reconnecting, Polling, Incompatible }
  impl ChannelState { pub fn accepts_dispatch(&self) -> bool }   // Connected | Polling
  pub struct MachineSettings { /* existing */ pub poll_every: Duration }  // default 10s; daemon passes tick
  pub struct PastorEvent { pub kind: String, pub task_id: Option<i64>, pub machine: Option<String>, pub job: Option<String> }
  ```
- Consumes: `Store::claim_task` (Task 3), `PastorConfig::tick_duration`.

Why: (a) `handle_command` sent the dispatch reply before `refresh_live()`, so a caller that read `snapshot().live` right after could see the old count. Seen in the field on 2026-09-24 against fake-herdr: after a reconnect freed a `max_agents = 1` machine, one `dispatch_queued` pass placed t-3 and t-4 on it back to back and `flock list` showed `2/1`. Task 11 serialises passes and relies on the count being right the moment the reply arrives. (b) The spec's `polling` state: requests answer, the subscription does not. Today that machine bounces through `reconnecting` and `machine.lost` although every request works. (c) `PastorEvent.machine` was a required `String`; job events (Task 10) have no machine.

- [ ] **Step 1: Write the failing tests**

In `src/machine.rs` `mod tests`, change `FlakyEvents` so it can stop failing:

```rust
    /// Every request is served by the fake except `events.subscribe`, which gets
    /// a connection that closes without answering until `subscribes` reaches
    /// `fail_until`. Selecting by method, not by call parity: a connect attempt
    /// makes several ordinary calls (ping, reconcile's `agent.list`) before it
    /// subscribes, so a parity rule would fail one of those instead and never
    /// reach `open_events` at all.
    struct FlakyEvents {
        subscribes: Arc<std::sync::atomic::AtomicUsize>,
        fail_until: usize,
        fake: FakeHerdr,
    }
```

and in its `connect`, replace the `if req.method == "events.subscribe"` block with:

```rust
                    if req.method == "events.subscribe" {
                        let n = subscribes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if n < fail_until {
                            return; // dropping `writer` is the EOF the subscribe sees
                        }
                        // Past the failures: hand the subscription to the fake for real.
                        let mut stream = match fake
                            .subscribe(
                                req.params
                                    .get("subscriptions")
                                    .and_then(|v| v.as_array())
                                    .cloned()
                                    .unwrap_or_default(),
                            )
                            .await
                        {
                            Ok(s) => s,
                            Err(_) => return,
                        };
                        let ack = serde_json::json!({"id": req.id, "result": {"type": "subscription_started"}});
                        if writer.write_all(format!("{ack}\n").as_bytes()).await.is_err() {
                            return;
                        }
                        while let Ok(ev) = stream.next().await {
                            let line = serde_json::to_string(&ev).unwrap();
                            if writer.write_all(format!("{line}\n").as_bytes()).await.is_err() {
                                return;
                            }
                        }
                        return;
                    }
```

(`let fail_until = self.fail_until;` next to the other clones at the top of `connect`.) `event_stream_failures_back_off` passes `fail_until: usize::MAX`. Add:

```rust
    /// Requests work (ping, agent.list) but the event subscription will not open:
    /// the spec's `polling` state. The machine stays dispatchable, tasks are
    /// tracked by reconcile every `poll_every`, no `machine.lost` is announced,
    /// and the first successful subscribe returns it to `connected`.
    #[tokio::test]
    async fn a_machine_whose_events_will_not_open_polls_instead_of_dropping() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (events, mut rx) = broadcast::channel(64);
        let subscribes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut settings = settings();
        settings.poll_every = Duration::from_millis(100);
        let h = spawn_machine(
            "m".into(),
            2,
            vec![],
            Arc::new(FlakyEvents {
                subscribes: subscribes.clone(),
                fail_until: 3,
                fake: fake.clone(),
            }),
            store.clone(),
            settings,
            events,
        );
        wait_for("polling", || h.snapshot().channel == ChannelState::Polling).await;
        assert!(
            h.snapshot().error.as_deref().unwrap_or("").contains("events"),
            "{:?}",
            h.snapshot().error
        );

        // Dispatch works while polling.
        let t = h.dispatch(new_task(&store).id).await.unwrap();
        assert_eq!(t.state, TaskState::Running);
        // Without an event stream, only the poll can see this change.
        fake.set_status(t.pane_id.as_deref().unwrap(), AgentStatus::Blocked, None);
        wait_for("blocked via poll", || {
            state_of(&store, t.id) == TaskState::Blocked
        })
        .await;

        wait_for("connected once the subscribe succeeds", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        // Never lost: the machine answered every request throughout.
        while let Ok(ev) = rx.try_recv() {
            assert_ne!(ev.kind, "machine.lost", "{ev:?}");
        }
    }

    /// The reply to a dispatch must carry an already-refreshed live count: a
    /// caller that reads `snapshot().live` the moment `dispatch` returns is the
    /// serialised dispatch pass deciding whether the machine has room left.
    #[tokio::test]
    async fn live_count_is_current_when_dispatch_replies() {
        let fake = FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (h, _events) = spawn(&fake, &store);
        wait_for("connected", || {
            h.snapshot().channel == ChannelState::Connected
        })
        .await;
        for expected in 1..=2 {
            h.dispatch(new_task(&store).id).await.unwrap();
            assert_eq!(h.snapshot().live, expected, "live count lagged the reply");
        }
    }

    #[test]
    fn channel_states_that_accept_dispatch() {
        assert!(ChannelState::Connected.accepts_dispatch());
        assert!(ChannelState::Polling.accepts_dispatch());
        for s in [
            ChannelState::Connecting,
            ChannelState::Reconnecting,
            ChannelState::Incompatible,
        ] {
            assert!(!s.accepts_dispatch(), "{s}");
        }
        assert_eq!(ChannelState::Polling.to_string(), "polling");
    }
```

Update existing tests that build or read `PastorEvent.machine` (grep `ev.machine`): none assert on it today.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib machine::`
Expected: compile errors (`Polling`, `poll_every`, `accepts_dispatch`).

- [ ] **Step 3: Implement**

`src/machine.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChannelState {
    Connecting,
    Connected,
    Reconnecting,
    /// Requests answer but `events.subscribe` will not open: tasks are tracked
    /// by `agent.list` every `poll_every` until a subscribe succeeds.
    Polling,
    Incompatible,
}

impl ChannelState {
    /// May the dispatcher place a task here? Only states in which requests are
    /// known to answer.
    pub fn accepts_dispatch(&self) -> bool {
        matches!(self, ChannelState::Connected | ChannelState::Polling)
    }
}
```

Add `"polling"` to `Display`. In `MachineSettings` add:

```rust
    /// While `Polling`, how often `agent.list` reconciles. The daemon passes its
    /// `tick`, the spec's "each tick for a polling machine".
    pub poll_every: Duration,
```

with default `Duration::from_secs(10)` and `poll_every: Duration::from_millis(200)` in the test `settings()`.

`PastorEvent`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PastorEvent {
    pub kind: String,
    #[serde(default)]
    pub task_id: Option<i64>,
    #[serde(default)]
    pub machine: Option<String>,
    #[serde(default)]
    pub job: Option<String>,
}
```

`emit` sets `machine: Some(self.name.clone()), job: None`.

`handle_command`, `Dispatch` arm: move `self.refresh_live();` above `let _ = reply.send(result);`.

`Actor::run`: replace the `let mut events = match self.open_events().await { ... }` block with:

```rust
            let mut events = match self.open_events().await {
                Ok(s) => s,
                Err(err) => {
                    // ping and reconcile just succeeded: requests answer, only
                    // the subscription is missing. That is a machine to poll,
                    // not one to declare lost.
                    tracing::warn!(machine = %self.name, %err, "events will not open; polling");
                    self.set_channel(ChannelState::Polling, Some(format!("events: {err}")));
                    self.announce_connected();
                    self.refresh_live();
                    match self.poll_until_subscribed(&mut backoff).await {
                        Some(s) => s,
                        None => {
                            // A request failed while polling: the machine is gone
                            // after all. Fall through to the reconnect path.
                            self.set_channel(ChannelState::Reconnecting, Some("connection lost".into()));
                            self.announce_lost();
                            self.drain_commands_while_down(backoff).await;
                            backoff = (backoff * 2).min(self.settings.max_backoff);
                            continue;
                        }
                    }
                }
            };
```

Add the method after `open_events`:

```rust
    /// The `Polling` loop: serve commands, reconcile every `poll_every`, retry
    /// the subscription with backoff. `Some(stream)` once a subscribe succeeds;
    /// `None` when a request failed below the API, which means a full reconnect.
    async fn poll_until_subscribed(&mut self, backoff: &mut Duration) -> Option<EventStream> {
        let mut poll = tokio::time::interval(self.settings.poll_every);
        poll.tick().await; // fires immediately; we reconciled a moment ago
        let retry = tokio::time::sleep(*backoff);
        tokio::pin!(retry);
        loop {
            tokio::select! {
                cmd = self.rx.recv() => {
                    let Some(cmd) = cmd else { return None };
                    match self.handle_command(cmd).await {
                        // A resubscribe request is what the retry timer does anyway.
                        CommandOutcome::Nothing | CommandOutcome::Resubscribe => {}
                        CommandOutcome::Reconnect => return None,
                    }
                }
                _ = poll.tick() => {
                    if let Err(err) = self.reconcile().await {
                        tracing::warn!(machine = %self.name, %err, "poll reconcile failed");
                        return None;
                    }
                    if let Err(err) = self.confirm_pending_done().await {
                        tracing::warn!(machine = %self.name, %err, "settle check failed");
                        return None;
                    }
                }
                _ = &mut retry => {
                    match self.open_events().await {
                        Ok(s) => {
                            *backoff = self.settings.initial_backoff;
                            return Some(s);
                        }
                        Err(err) => {
                            *backoff = (*backoff * 2).min(self.settings.max_backoff);
                            tracing::debug!(machine = %self.name, %err, next_in = ?backoff, "subscribe still failing");
                            retry.as_mut().reset(tokio::time::Instant::now() + *backoff);
                        }
                    }
                }
            }
        }
    }
```

`drain_commands_while_down` is unchanged. In the connected loop, the `Resubscribe` failure arm keeps `break` (the reconnect path lands in polling on the next attempt if the subscribe still fails; a single failure does not need a special case).

`src/daemon.rs`:
- `Daemon::start`: add `poll_every: config.tick_duration(),` to the `MachineSettings`.
- `views()`: `healthy: s.channel.accepts_dispatch(),`.
- the events log line: `tracing::info!(kind = %ev.kind, task = ?ev.task_id, machine = ?ev.machine, job = ?ev.job, "pastor event")`.

- [ ] **Step 4: Run the tests**

Run: `make check && make test-machine`
Expected: green five times over. If `event_stream_failures_back_off` now counts fewer subscribes (polling retries at `backoff` rather than the whole connect cycle), the `(2..12)` window still holds: 50, 100, 200 ms gives 3 to 4 attempts in 300 ms.

- [ ] **Step 5: Commit**

```bash
git add src/machine.rs src/daemon.rs
git commit -m "feat: polling channel state and a live count that is current on reply" -m "A machine whose requests answer but whose events.subscribe fails was cycled through reconnecting and announced lost although nothing was lost. It now polls agent.list each tick and keeps taking tasks, as the spec's polling state says. The dispatch reply is sent after refresh_live so the serialised dispatch pass in the scheduler reads a count that already includes the task it just placed. PastorEvent gains a job field for the job.failed event."
```

---

### Task 5: Schedule: `every` and 5-field cron

**Files:**
- Create: `src/schedule.rs`
- Modify: `src/lib.rs` (add `pub mod schedule;`)

**Interfaces:**
- Produces:
  ```rust
  // src/schedule.rs
  pub enum Schedule { Every(Duration), Cron(CronExpr) }
  impl Schedule {
      pub fn from_fields(every: Option<&str>, cron: Option<&str>) -> Result<Schedule, String>;
      pub fn describe(&self) -> String;                                   // "every 5m" | "cron */5 9-18 * * 1-5"
      pub fn next_after(&self, last: DateTime<Utc>) -> Option<DateTime<Utc>>; // strictly after `last`; cron in Local
  }
  pub struct CronExpr { /* private */ }
  impl CronExpr {
      pub fn parse(text: &str) -> Result<CronExpr, String>;
      pub fn next_after(&self, after: DateTime<Local>) -> Option<DateTime<Local>>;
      pub fn next_after_in<Tz: TimeZone>(&self, after: DateTime<Tz>) -> Option<DateTime<Tz>>; // tests use Utc
  }
  pub fn describe_duration(d: Duration) -> String;                        // 300s -> "5m"
  ```
- Consumes: `config::parse_duration`.

- [ ] **Step 1: Write the failing tests**

Create `src/schedule.rs` with only the test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn every_is_last_plus_interval() {
        let s = Schedule::from_fields(Some("5m"), None).unwrap();
        assert_eq!(s, Schedule::Every(Duration::from_secs(300)));
        assert_eq!(
            s.next_after(utc("2026-09-24T10:00:00Z")),
            Some(utc("2026-09-24T10:05:00Z"))
        );
        assert_eq!(s.describe(), "every 5m");
        assert_eq!(
            Schedule::from_fields(Some("1d"), None).unwrap().describe(),
            "every 1d"
        );
    }

    #[test]
    fn exactly_one_of_every_and_cron() {
        assert!(Schedule::from_fields(None, None).unwrap_err().contains("every or cron"));
        assert!(
            Schedule::from_fields(Some("5m"), Some("* * * * *"))
                .unwrap_err()
                .contains("not both")
        );
        assert!(Schedule::from_fields(Some("0s"), None).unwrap_err().contains("zero"));
        assert!(Schedule::from_fields(Some("soon"), None).is_err());
    }

    // 2026-09-26 is a Saturday, 2026-09-28 a Monday.
    #[test]
    fn weekday_office_hours() {
        let c = CronExpr::parse("*/5 9-18 * * 1-5").unwrap();
        assert_eq!(
            c.next_after_in(utc("2026-09-26T10:03:00Z")),
            Some(utc("2026-09-28T09:00:00Z")),
            "Saturday rolls to Monday morning"
        );
        assert_eq!(
            c.next_after_in(utc("2026-09-28T09:00:00Z")),
            Some(utc("2026-09-28T09:05:00Z")),
            "strictly after: a run at 09:00 is not due again at 09:00"
        );
        assert_eq!(
            c.next_after_in(utc("2026-09-28T18:57:30Z")),
            Some(utc("2026-09-29T09:00:00Z"))
        );
        assert_eq!(
            Schedule::Cron(c).describe(),
            "cron */5 9-18 * * 1-5"
        );
    }

    #[test]
    fn leap_day_is_found_across_years() {
        let c = CronExpr::parse("0 0 29 2 *").unwrap();
        assert_eq!(
            c.next_after_in(utc("2026-03-01T00:00:00Z")),
            Some(utc("2028-02-29T00:00:00Z"))
        );
    }

    /// Vixie semantics: with both day-of-month and day-of-week restricted, a
    /// day matches either.
    #[test]
    fn day_of_month_or_day_of_week_when_both_are_set() {
        let c = CronExpr::parse("0 12 1 * 1").unwrap();
        assert_eq!(
            c.next_after_in(utc("2026-09-24T13:00:00Z")),
            Some(utc("2026-09-28T12:00:00Z")),
            "the Monday comes before the 1st"
        );
        assert_eq!(
            c.next_after_in(utc("2026-09-28T12:00:00Z")),
            Some(utc("2026-10-01T12:00:00Z")),
            "then the 1st, a Thursday"
        );
    }

    #[test]
    fn seven_is_sunday_and_lists_and_steps_parse() {
        let c = CronExpr::parse("0 0 * * 7").unwrap();
        assert_eq!(
            c.next_after_in(utc("2026-09-24T01:00:00Z")),
            Some(utc("2026-09-27T00:00:00Z"))
        );
        let c = CronExpr::parse("15,45 8-10/2 * 1,6 *").unwrap();
        assert_eq!(
            c.next_after_in(utc("2026-09-24T01:00:00Z")),
            Some(utc("2027-01-01T08:15:00Z"))
        );
        assert_eq!(
            c.next_after_in(utc("2027-01-01T08:15:00Z")),
            Some(utc("2027-01-01T08:45:00Z"))
        );
        assert_eq!(
            c.next_after_in(utc("2027-01-01T08:45:00Z")),
            Some(utc("2027-01-01T10:15:00Z"))
        );
    }

    #[test]
    fn rejects_malformed_expressions() {
        for bad in [
            "60 * * * *",
            "* 24 * * *",
            "* * 32 * *",
            "* * * 13 *",
            "* * * * 8",
            "* * * *",
            "* * * * * *",
            "*/0 * * * *",
            "5-1 * * * *",
            "a * * * *",
            "",
        ] {
            assert!(CronExpr::parse(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn describes_durations_in_their_largest_whole_unit() {
        assert_eq!(describe_duration(Duration::from_secs(45)), "45s");
        assert_eq!(describe_duration(Duration::from_secs(300)), "5m");
        assert_eq!(describe_duration(Duration::from_secs(7200)), "2h");
        assert_eq!(describe_duration(Duration::from_secs(90)), "90s");
        assert_eq!(describe_duration(Duration::from_secs(172800)), "2d");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib schedule::`
Expected: compile errors, nothing defined.

- [ ] **Step 3: Implement**

Prepend to `src/schedule.rs`:

```rust
//! When a job is due. `every` is a plain interval; `cron` is 5-field cron in the
//! head's local time, matched by a small walker rather than a crate: the spec's
//! subset (numbers, `*`, ranges, lists, steps) fits in a page and stays
//! readable next to its tests.

use std::time::Duration;

use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc};

use crate::config::parse_duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schedule {
    Every(Duration),
    Cron(CronExpr),
}

impl Schedule {
    /// From a job file's `every` / `cron` fields: exactly one must be set.
    pub fn from_fields(every: Option<&str>, cron: Option<&str>) -> Result<Schedule, String> {
        match (every, cron) {
            (Some(e), None) => {
                let d = parse_duration(e).map_err(|err| format!("every: {err}"))?;
                if d.is_zero() {
                    return Err("every: must not be zero".into());
                }
                Ok(Schedule::Every(d))
            }
            (None, Some(c)) => Ok(Schedule::Cron(CronExpr::parse(c)?)),
            (Some(_), Some(_)) => Err("set either every or cron, not both".into()),
            (None, None) => Err("set every or cron".into()),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Schedule::Every(d) => format!("every {}", describe_duration(*d)),
            Schedule::Cron(c) => format!("cron {}", c.source),
        }
    }

    /// The first instant strictly after `last` at which a run is due. Cron is
    /// evaluated in the head's local time zone and converted back.
    pub fn next_after(&self, last: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self {
            Schedule::Every(d) => Some(last + chrono::Duration::from_std(*d).ok()?),
            Schedule::Cron(c) => c
                .next_after(last.with_timezone(&Local))
                .map(|t| t.with_timezone(&Utc)),
        }
    }
}

/// `300s` reads better as `5m` in a table; whole units only, else seconds.
pub fn describe_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s > 0 && s % 86400 == 0 {
        format!("{}d", s / 86400)
    } else if s > 0 && s % 3600 == 0 {
        format!("{}h", s / 3600)
    } else if s > 0 && s % 60 == 0 {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    }
}

/// `minute hour day-of-month month day-of-week`. Each field: `*`, `n`, `a-b`,
/// `a-b/s`, `*/s`, or a comma list of those. Sunday is 0 or 7. Names are not
/// accepted. Day matching follows Vixie cron: if both day fields are restricted
/// a day matches when either does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronExpr {
    source: String,
    minute: Vec<bool>,
    hour: Vec<bool>,
    dom: Vec<bool>,
    month: Vec<bool>,
    dow: Vec<bool>,
    dom_any: bool,
    dow_any: bool,
}

impl CronExpr {
    pub fn parse(text: &str) -> Result<CronExpr, String> {
        let fields: Vec<&str> = text.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(format!(
                "cron {text:?}: expected 5 fields (minute hour day month weekday), got {}",
                fields.len()
            ));
        }
        let (minute, _) = parse_field(fields[0], 0, 59).map_err(|e| format!("cron minute {e}"))?;
        let (hour, _) = parse_field(fields[1], 0, 23).map_err(|e| format!("cron hour {e}"))?;
        let (dom, dom_any) = parse_field(fields[2], 1, 31).map_err(|e| format!("cron day {e}"))?;
        let (month, _) = parse_field(fields[3], 1, 12).map_err(|e| format!("cron month {e}"))?;
        let (mut dow, dow_any) = parse_field(fields[4], 0, 7).map_err(|e| format!("cron weekday {e}"))?;
        if dow[7] {
            dow[0] = true;
        }
        dow.truncate(7);
        Ok(CronExpr {
            source: fields.join(" "),
            minute,
            hour,
            dom,
            month,
            dow,
            dom_any,
            dow_any,
        })
    }

    pub fn next_after(&self, after: DateTime<Local>) -> Option<DateTime<Local>> {
        self.next_after_in(after)
    }

    /// Walk forward in the zone's wall-clock time, one unit at a time, skipping
    /// whole months, days and hours that cannot match, so a once-a-year
    /// expression is found in a few thousand steps rather than half a million.
    /// A wall-clock minute that does not exist (spring forward) is skipped; an
    /// ambiguous one (fall back) fires at its first occurrence.
    pub fn next_after_in<Tz: TimeZone>(&self, after: DateTime<Tz>) -> Option<DateTime<Tz>> {
        let tz = after.timezone();
        let mut t = after.naive_local().with_second(0)?.with_nanosecond(0)?
            + chrono::Duration::minutes(1);
        // Five years covers the sparsest 5-field expression (29 February).
        let limit = t + chrono::Duration::days(366 * 5);
        while t < limit {
            if !self.month[t.month() as usize] {
                t = start_of_next_month(t);
                continue;
            }
            if !self.day_matches(t.date()) {
                t = (t.date() + chrono::Duration::days(1)).and_hms_opt(0, 0, 0)?;
                continue;
            }
            if !self.hour[t.hour() as usize] {
                t = t.with_minute(0)? + chrono::Duration::hours(1);
                continue;
            }
            if !self.minute[t.minute() as usize] {
                t += chrono::Duration::minutes(1);
                continue;
            }
            match tz.from_local_datetime(&t).earliest() {
                Some(dt) => return Some(dt),
                None => t += chrono::Duration::minutes(1),
            }
        }
        None
    }

    fn day_matches(&self, d: NaiveDate) -> bool {
        let dom = self.dom[d.day() as usize];
        let dow = self.dow[d.weekday().num_days_from_sunday() as usize];
        match (self.dom_any, self.dow_any) {
            (true, true) => true,
            (false, true) => dom,
            (true, false) => dow,
            (false, false) => dom || dow,
        }
    }
}

fn start_of_next_month(t: NaiveDateTime) -> NaiveDateTime {
    let (y, m) = if t.month() == 12 {
        (t.year() + 1, 1)
    } else {
        (t.year(), t.month() + 1)
    };
    NaiveDate::from_ymd_opt(y, m, 1)
        .expect("month 1..=12 always exists")
        .and_hms_opt(0, 0, 0)
        .expect("midnight always exists")
}

/// One field into a membership table over `min..=max`, plus whether it was
/// unrestricted (`*` or `*/n`), which the day-matching rule needs.
fn parse_field(text: &str, min: u32, max: u32) -> Result<(Vec<bool>, bool), String> {
    if text.is_empty() {
        return Err("field is empty".into());
    }
    let mut set = vec![false; max as usize + 1];
    let any = text.starts_with('*');
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (
                r,
                Some(s.parse::<u32>().map_err(|_| format!("{part:?}: bad step"))?),
            ),
            None => (part, None),
        };
        if step == Some(0) {
            return Err(format!("{part:?}: step must be at least 1"));
        }
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (parse_num(a, min, max)?, parse_num(b, min, max)?)
        } else {
            let n = parse_num(range, min, max)?;
            (n, if step.is_some() { max } else { n })
        };
        if lo > hi {
            return Err(format!("{part:?}: range runs backwards"));
        }
        let mut v = lo;
        while v <= hi {
            set[v as usize] = true;
            v += step.unwrap_or(1);
        }
    }
    Ok((set, any))
}

fn parse_num(s: &str, min: u32, max: u32) -> Result<u32, String> {
    let n: u32 = s
        .parse()
        .map_err(|_| format!("{s:?}: expected a number between {min} and {max}"))?;
    if n < min || n > max {
        return Err(format!("{s:?}: must be between {min} and {max}"));
    }
    Ok(n)
}
```

Add `pub mod schedule;` to `src/lib.rs`.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib schedule:: && make check`
Expected: green. If `with_nanosecond` is flagged unused-import by clippy for `Timelike`, it is not: both `with_second` and `hour()` come from `Timelike`.

- [ ] **Step 5: Commit**

```bash
git add src/schedule.rs src/lib.rs
git commit -m "feat: job schedules: every and 5-field cron" -m "The walker skips whole months, days and hours that cannot match, so sparse expressions are cheap, and it steps in wall-clock time so a cron is what the head's clock says. Written in-crate instead of pulling a cron crate: the spec's subset is small and the Vixie day rule is easier to test here than to verify in a dependency."
```

---

### Task 6: Templates: `{{ item.title }}` substitution

**Files:**
- Create: `src/template.rs`
- Modify: `src/lib.rs` (add `pub mod template;`)

**Interfaces:**
- Produces:
  ```rust
  // src/template.rs
  pub struct Rendered { pub text: String, pub missing: Vec<String> }
  pub fn placeholders(template: &str) -> Result<Vec<String>, String>;      // every `{{ path }}`, in order; Err on syntax
  pub fn render(template: &str, ctx: &serde_json::Value) -> Result<Rendered, String>; // missing paths render "" and are listed
  ```

- [ ] **Step 1: Write the failing tests**

Create `src/template.rs` with the tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn substitutes_dotted_paths_and_leaves_the_rest_alone() {
        let ctx = json!({
            "item": {"key": "k1", "title": "login broken", "n": 3, "ok": true, "tags": ["a", "b"], "none": null},
            "job": {"name": "support"},
            "task": {"id": "t-7"}
        });
        let r = render(
            "[{{ job.name }}] {{item.title}} #{{ item.n }} {{ item.ok }} {{ item.tags }} '{{ item.none }}' {{ task.id }}\n{ not a placeholder }",
            &ctx,
        )
        .unwrap();
        assert_eq!(
            r.text,
            "[support] login broken #3 true [\"a\",\"b\"] '' t-7\n{ not a placeholder }"
        );
        assert!(r.missing.is_empty());
    }

    #[test]
    fn missing_paths_render_empty_and_are_reported() {
        let ctx = json!({"item": {"key": "k"}, "job": {"name": "j"}, "task": {"id": "t-1"}});
        let r = render("a {{ item.title }} b {{ item.meta.deep }} c", &ctx).unwrap();
        assert_eq!(r.text, "a  b  c");
        assert_eq!(r.missing, vec!["item.title", "item.meta.deep"]);
    }

    #[test]
    fn placeholders_lists_paths_and_rejects_bad_syntax() {
        assert_eq!(
            placeholders("{{ item.key }}/{{job.name}} {{ task.id }}").unwrap(),
            vec!["item.key", "job.name", "task.id"]
        );
        assert!(placeholders("no braces").unwrap().is_empty());
        assert!(placeholders("{{ item.key ").unwrap_err().contains("unterminated"));
        assert!(placeholders("{{ }}").unwrap_err().contains("bad placeholder"));
        assert!(placeholders("{{ item.a-b }}").unwrap_err().contains("bad placeholder"));
        assert!(placeholders("{{ item..key }}").unwrap_err().contains("bad placeholder"));
        assert!(placeholders("{{ item | upper }}").unwrap_err().contains("bad placeholder"));
        // render rejects the same input, so a bad template never half-renders
        assert!(render("{{ item.key ", &json!({})).is_err());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib template::`
Expected: compile errors.

- [ ] **Step 3: Implement**

Prepend to `src/template.rs`:

```rust
//! `{{ item.title }}` substitution for prompts, branches and repos. No logic,
//! no filters, no escapes: the spec asks for substitution only, and a template
//! engine would invite exactly the conditionals it rules out. A missing path
//! renders empty and is reported, so one odd item cannot fail a job forever.

use serde_json::Value;

pub struct Rendered {
    pub text: String,
    /// Paths that had no value in the context, in order of appearance.
    pub missing: Vec<String>,
}

/// Every `{{ path }}` in `template`, in order. Errors on an unterminated `{{`
/// or on a path that is not dotted identifiers, so a job file can be checked
/// at load time.
pub fn placeholders(template: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut rest = template;
    while let Some((_, path, after)) = next_placeholder(rest)? {
        out.push(path.to_string());
        rest = after;
    }
    Ok(out)
}

pub fn render(template: &str, ctx: &Value) -> Result<Rendered, String> {
    let mut text = String::with_capacity(template.len());
    let mut missing = Vec::new();
    let mut rest = template;
    while let Some((before, path, after)) = next_placeholder(rest)? {
        text.push_str(before);
        match lookup(ctx, path) {
            Some(v) => text.push_str(&scalar(v)),
            None => missing.push(path.to_string()),
        }
        rest = after;
    }
    text.push_str(rest);
    Ok(Rendered { text, missing })
}

/// `(literal before, path, rest after the closing braces)` for the next
/// placeholder, or `None` when there is no `{{` left.
fn next_placeholder(rest: &str) -> Result<Option<(&str, &str, &str)>, String> {
    let Some(start) = rest.find("{{") else {
        return Ok(None);
    };
    let after_open = &rest[start + 2..];
    let Some(end) = after_open.find("}}") else {
        let shown: String = rest[start..].chars().take(30).collect();
        return Err(format!("unterminated placeholder near {shown:?}"));
    };
    let path = after_open[..end].trim();
    let well_formed = !path.is_empty()
        && path.split('.').all(|seg| {
            !seg.is_empty()
                && seg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
    if !well_formed {
        return Err(format!(
            "bad placeholder {{{{ {path} }}}}: expected dotted names like item.title"
        ));
    }
    Ok(Some((&rest[..start], path, &after_open[end + 2..])))
}

fn lookup<'a>(ctx: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(ctx, |v, seg| v.get(seg))
}

/// Strings raw, null empty, everything else as compact JSON.
fn scalar(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}
```

Add `pub mod template;` to `src/lib.rs`.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib template:: && make check`
Expected: green.

- [ ] **Step 5: Commit**

```bash
git add src/template.rs src/lib.rs
git commit -m "feat: placeholder substitution for prompts, branches and repos" -m "Substitution only, as the spec says. A missing item field renders empty and is reported rather than failing: an item that fails to render would stay unseen and fail again every run."
```

---

### Task 7: Connector seam and the built-in clock

**Files:**
- Create: `src/connector/mod.rs`, `src/connector/clock.rs`
- Modify: `src/lib.rs` (add `pub mod connector;`)

**Interfaces:**
- Produces:
  ```rust
  // src/connector/mod.rs
  pub struct Item { pub key: String, pub fields: serde_json::Map<String, Value> }   // fields always include "key"
  impl Item { pub fn new(key: impl Into<String>, fields: Map<String, Value>) -> Item; pub fn as_value(&self) -> Value }
  pub struct RunInput { pub config: Value, pub cursor: Option<String>, pub since: DateTime<Utc>, pub now: DateTime<Utc> }
  pub struct RunOutput { pub items: Vec<Item>, pub cursor: Option<String>, pub logs: Vec<String> }
  pub type RunFuture<'a> = Pin<Box<dyn Future<Output = Result<RunOutput, String>> + Send + 'a>>;
  pub trait ItemSource: Send + Sync { fn id(&self) -> &str; fn run<'a>(&'a self, input: RunInput) -> RunFuture<'a>; }
  pub fn builtin(id: &str) -> Option<Arc<dyn ItemSource>>;   // "clock" only in this plan
  pub fn is_available(id: &str) -> bool;
  // src/connector/clock.rs
  pub struct Clock;
  ```

- [ ] **Step 1: Write the failing tests**

`src/connector/clock.rs`, tests only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[tokio::test]
    async fn one_item_per_run_keyed_by_the_run_time() {
        let now = Utc.with_ymd_and_hms(2026, 9, 24, 10, 0, 5).unwrap();
        let out = Clock
            .run(RunInput {
                config: serde_json::json!({}),
                cursor: None,
                since: now,
                now,
            })
            .await
            .unwrap();
        assert_eq!(out.items.len(), 1);
        let item = &out.items[0];
        assert_eq!(item.key, "2026-09-24T10:00:05Z");
        assert_eq!(item.fields["key"], "2026-09-24T10:00:05Z");
        assert_eq!(item.fields["at"], "2026-09-24T10:00:05Z");
        assert_eq!(item.fields["title"], "clock 2026-09-24T10:00:05Z");
        assert!(out.cursor.is_none());
    }
}
```

`src/connector/mod.rs`, tests only:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_clock_is_built_in() {
        assert!(is_available("clock"));
        assert_eq!(builtin("clock").unwrap().id(), "clock");
        assert!(!is_available("slack"));
        assert!(builtin("").is_none());
    }

    #[test]
    fn item_always_carries_its_key_in_fields() {
        let mut fields = serde_json::Map::new();
        fields.insert("title".into(), serde_json::Value::String("x".into()));
        let item = Item::new("k1", fields);
        assert_eq!(item.as_value()["key"], "k1");
        assert_eq!(item.as_value()["title"], "x");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib connector::`
Expected: compile errors.

- [ ] **Step 3: Implement**

`src/connector/mod.rs`:

```rust
//! The seam between the scheduler and whatever produces items. This plan ships
//! the built-in `clock`; plan 3 adds process connectors from plugins behind the
//! same trait. Named `ItemSource` in code only because `herdr::Connector` is
//! already the transport; wherever a user sees it, it is a connector.

pub mod clock;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value};

/// One thing a connector found. `key` is the stable identity the seen-store
/// uses; `fields` is the whole object (always including `key`) and is what
/// templates see as `item.*` and what `tasks.item` stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub key: String,
    pub fields: Map<String, Value>,
}

impl Item {
    pub fn new(key: impl Into<String>, mut fields: Map<String, Value>) -> Item {
        let key = key.into();
        fields.insert("key".into(), Value::String(key.clone()));
        Item { key, fields }
    }

    pub fn as_value(&self) -> Value {
        Value::Object(self.fields.clone())
    }
}

/// What a run receives; the same shape plan 3 will put on a plugin's stdin.
#[derive(Debug, Clone)]
pub struct RunInput {
    /// The job's `[connector]` table minus `use`.
    pub config: Value,
    pub cursor: Option<String>,
    /// Start of the last successful run, or `now - backfill` on the first.
    pub since: DateTime<Utc>,
    pub now: DateTime<Utc>,
}

#[derive(Debug, Clone, Default)]
pub struct RunOutput {
    pub items: Vec<Item>,
    /// Persisted only when the run succeeds; `None` keeps the previous cursor.
    pub cursor: Option<String>,
    pub logs: Vec<String>,
}

pub type RunFuture<'a> = Pin<Box<dyn Future<Output = Result<RunOutput, String>> + Send + 'a>>;

pub trait ItemSource: Send + Sync {
    fn id(&self) -> &str;
    fn run<'a>(&'a self, input: RunInput) -> RunFuture<'a>;
}

/// The connectors pastor ships inside the binary.
pub fn builtin(id: &str) -> Option<Arc<dyn ItemSource>> {
    match id {
        "clock" => Some(Arc::new(clock::Clock)),
        _ => None,
    }
}

pub fn is_available(id: &str) -> bool {
    builtin(id).is_some()
}
```

`src/connector/clock.rs`:

```rust
//! Schedule-only jobs: one item per run, keyed by the run time, so a job with
//! no external source still goes through the same seen-store and templates.

use chrono::{SecondsFormat, Utc};
use serde_json::{Map, Value};

use super::{Item, ItemSource, RunFuture, RunInput, RunOutput};

pub struct Clock;

impl ItemSource for Clock {
    fn id(&self) -> &str {
        "clock"
    }

    fn run<'a>(&'a self, input: RunInput) -> RunFuture<'a> {
        Box::pin(async move {
            let at = input.now.to_rfc3339_opts(SecondsFormat::Secs, true);
            let mut fields = Map::new();
            fields.insert("title".into(), Value::String(format!("clock {at}")));
            fields.insert("at".into(), Value::String(at.clone()));
            Ok(RunOutput {
                items: vec![Item::new(at, fields)],
                cursor: None,
                logs: Vec::new(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    // tests from Step 1
}
```

(`Utc` in the clock's imports is used by the test module only; keep the import inside `mod tests` if clippy complains.)

Add `pub mod connector;` to `src/lib.rs`.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib connector:: && make check`
Expected: green.

- [ ] **Step 5: Commit**

```bash
git add src/connector src/lib.rs
git commit -m "feat: connector seam with the built-in clock" -m "One trait the scheduler runs items through, so plan 3's process connectors slot in without touching the scheduler. The clock keys items by run time: a schedule-only job dedups and renders like any other."
```

---

### Task 8: Job files

**Files:**
- Create: `src/config/job.rs`
- Modify: `src/config/mod.rs` (add `pub mod job;`)

**Interfaces:**
- Produces:
  ```rust
  // src/config/job.rs
  pub struct JobFile { pub name: Option<String>, pub every: Option<String>, pub cron: Option<String>, pub enabled: bool,
                       pub connector: ConnectorTable, pub dispatch: DispatchTable }        // the TOML shape, serde only
  pub struct Job { pub name: String, pub schedule: Schedule, pub enabled: bool, pub connector: String,
                   pub connector_config: serde_json::Value, pub prompt: String, pub max_tasks_per_run: u32,
                   pub backfill: Duration, pub spec: DispatchSpec /* repo/branch unrendered */ }
  impl Job { pub fn parse(text: &str, stem: &str, defaults: &Defaults) -> Result<Job, String> }
  pub enum Loaded { Valid(Job), Invalid { name: String, error: String } }
  impl Loaded { pub fn name(&self) -> &str }
  pub fn load_dir(dir: &Path, defaults: &Defaults) -> anyhow::Result<Vec<Loaded>>;   // sorted by name; missing dir = empty
  pub fn load_file(path: &Path, stem: &str, defaults: &Defaults) -> Loaded;
  pub fn job_path(dir: &Path, name: &str) -> PathBuf;                                  // <dir>/<name>.toml
  pub fn set_enabled(path: &Path, enabled: bool) -> anyhow::Result<()>;                // rewrites one top-level line
  ```
- Consumes: `schedule::Schedule::from_fields`, `template::placeholders`, `connector::is_available`, `config::{Defaults, parse_duration}`, `task::DispatchSpec`.

- [ ] **Step 1: Write the failing tests**

Create `src/config/job.rs` with the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const SPEC_EXAMPLE: &str = r#"
name = "support-slack"
every = "5m"
enabled = true

[connector]
use = "clock"
channel = "C0123ABC"

[dispatch]
agent = "claude"
agent_args = []
repo = "~/work/support"
worktree = true
branch = "pastor/{{ item.key }}"
tags = ["fast"]
timeout = "2h"
max_tasks_per_run = 5
backfill = "0s"
prompt = """
New message in #support from {{ item.author }}:

{{ item.text }}

Investigate, fix if it is a bug, and write your answer to REPLY.md.
"""
"#;

    fn defaults() -> Defaults {
        Defaults::default()
    }

    #[test]
    fn parses_the_spec_example() {
        let job = Job::parse(SPEC_EXAMPLE, "support-slack", &defaults()).unwrap();
        assert_eq!(job.name, "support-slack");
        assert_eq!(job.schedule, Schedule::Every(Duration::from_secs(300)));
        assert!(job.enabled);
        assert_eq!(job.connector, "clock");
        assert_eq!(job.connector_config["channel"], "C0123ABC");
        assert!(job.connector_config.get("use").is_none(), "use is not config");
        assert_eq!(job.spec.agent, "claude");
        assert_eq!(job.spec.repo.as_deref(), Some("~/work/support"));
        assert!(job.spec.worktree);
        assert_eq!(job.spec.branch.as_deref(), Some("pastor/{{ item.key }}"));
        assert_eq!(job.spec.tags, vec!["fast"]);
        assert_eq!(job.spec.timeout_secs, 7200);
        assert_eq!(job.max_tasks_per_run, 5);
        assert_eq!(job.backfill, Duration::ZERO);
        assert!(job.prompt.contains("{{ item.author }}"));
    }

    #[test]
    fn a_connector_without_a_plugin_is_invalid_for_now() {
        let text = SPEC_EXAMPLE.replace("use = \"clock\"", "use = \"slack\"");
        let err = Job::parse(&text, "support-slack", &defaults()).unwrap_err();
        assert!(err.contains("slack"), "{err}");
        assert!(err.contains("not available"), "{err}");
    }

    #[test]
    fn exactly_one_schedule_and_name_must_match_stem() {
        let both = SPEC_EXAMPLE.replace("every = \"5m\"", "every = \"5m\"\ncron = \"* * * * *\"");
        assert!(Job::parse(&both, "support-slack", &defaults()).unwrap_err().contains("not both"));
        let neither = SPEC_EXAMPLE.replace("every = \"5m\"\n", "");
        assert!(Job::parse(&neither, "support-slack", &defaults()).unwrap_err().contains("every or cron"));
        let err = Job::parse(SPEC_EXAMPLE, "other", &defaults()).unwrap_err();
        assert!(err.contains("does not match the file name"), "{err}");
        // No name: the stem is the name.
        let unnamed = SPEC_EXAMPLE.replace("name = \"support-slack\"\n", "");
        assert_eq!(Job::parse(&unnamed, "anything-9", &defaults()).unwrap().name, "anything-9");
    }

    #[test]
    fn defaults_fill_agent_timeout_and_max_tasks() {
        let text = r#"
every = "1h"
[connector]
use = "clock"
[dispatch]
prompt = "tick {{ item.key }} for {{ job.name }} as {{ task.id }}"
"#;
        let d = Defaults {
            agent: "codex".into(),
            max_tasks_per_run: 2,
            timeout: "30m".into(),
        };
        let job = Job::parse(text, "hourly", &d).unwrap();
        assert_eq!(job.spec.agent, "codex");
        assert_eq!(job.max_tasks_per_run, 2);
        assert_eq!(job.spec.timeout_secs, 1800);
        assert!(job.enabled, "enabled defaults to true");
        assert!(!job.spec.worktree);
        assert_eq!(job.connector_config, serde_json::json!({}));
    }

    #[test]
    fn rejects_bad_names_templates_and_shapes() {
        let base = |name: &str, extra: &str| {
            format!(
                "name = \"{name}\"\nevery = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"p\"\n{extra}"
            )
        };
        for (name, needle) in [("run", "reserved"), ("Upper", "must match"), ("-x", "must match"), ("a b", "must match")] {
            let err = Job::parse(&base(name, ""), name, &defaults()).unwrap_err();
            assert!(err.contains(needle), "{name}: {err}");
        }
        let err = Job::parse(&base("ok", "branch = \"pastor/{{ job.nope }}\"\n"), "ok", &defaults()).unwrap_err();
        assert!(err.contains("dispatch.branch") && err.contains("job.nope"), "{err}");
        let err = Job::parse(&base("ok", "repo = \"{{ item.repo \"\n"), "ok", &defaults()).unwrap_err();
        assert!(err.contains("dispatch.repo") && err.contains("unterminated"), "{err}");
        let err = Job::parse(&base("ok", "worktree = true\n"), "ok", &defaults()).unwrap_err();
        assert!(err.contains("needs dispatch.repo"), "{err}");
        let err = Job::parse(&base("ok", "max_tasks_per_run = 0\n"), "ok", &defaults()).unwrap_err();
        assert!(err.contains("max_tasks_per_run"), "{err}");
        let err = Job::parse(&base("ok", "colour = \"blue\"\n"), "ok", &defaults()).unwrap_err();
        assert!(err.contains("colour"), "unknown keys must be reported: {err}");
        let err = Job::parse("every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"  \"\n", "ok", &defaults()).unwrap_err();
        assert!(err.contains("prompt is required"), "{err}");
    }

    #[test]
    fn load_dir_sorts_and_reports_invalid_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("jobs");
        assert!(load_dir(&dir, &defaults()).unwrap().is_empty(), "missing dir is no jobs");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(job_path(&dir, "zeta"), "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"z\"\n").unwrap();
        std::fs::write(job_path(&dir, "alpha"), "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"a\"\n").unwrap();
        std::fs::write(job_path(&dir, "broken"), "every = \"1h\"\n[connector\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "ignored").unwrap();
        let loaded = load_dir(&dir, &defaults()).unwrap();
        assert_eq!(
            loaded.iter().map(Loaded::name).collect::<Vec<_>>(),
            vec!["alpha", "broken", "zeta"]
        );
        let Loaded::Invalid { error, .. } = &loaded[1] else {
            panic!("broken must be invalid")
        };
        assert!(!error.is_empty());
        assert!(matches!(&loaded[0], Loaded::Valid(j) if j.prompt == "a"));
    }

    #[test]
    fn set_enabled_rewrites_one_line_and_keeps_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("j.toml");
        let original = "# my job\nname = \"j\"\nevery = \"1h\"   # hourly\nenabled = true\n\n[connector]\nuse = \"clock\"\n\n[dispatch]\nprompt = \"p\"\nenabled_looking = 1\n";
        std::fs::write(&path, original).unwrap();
        set_enabled(&path, false).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            original.replace("enabled = true", "enabled = false"),
            "only the top-level enabled line changes"
        );
        assert!(Job::parse(&text, "j", &defaults()).is_ok());
        assert!(!Job::parse(&text, "j", &defaults()).unwrap().enabled);

        // Absent: inserted before the first table so it stays top-level.
        let without = "name = \"k\"\nevery = \"1h\"\n\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"p\"\n";
        std::fs::write(&path, without).unwrap();
        set_enabled(&path, false).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "name = \"k\"\nevery = \"1h\"\nenabled = false\n\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"p\"\n");
        set_enabled(&path, true).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("enabled = true\n"));

        // A file that would not parse after the edit is left untouched.
        std::fs::write(&path, "every = \"1h\"\n[connector\n").unwrap();
        assert!(set_enabled(&path, true).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "every = \"1h\"\n[connector\n");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib config::job::`
Expected: compile errors.

- [ ] **Step 3: Implement**

Prepend to `src/config/job.rs`:

```rust
//! One job per TOML file in `~/.config/pastor/jobs/`. `JobFile` is the file's
//! shape and nothing else; `Job` is what survives validation and is what the
//! scheduler runs. Validation happens here, at load, so `job list` can show a
//! broken file as `invalid` with its reason instead of a run failing later.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;
use serde_json::Value;

use crate::config::{Defaults, parse_duration};
use crate::schedule::Schedule;
use crate::task::DispatchSpec;
use crate::template;

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobFile {
    pub name: Option<String>,
    pub every: Option<String>,
    pub cron: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub connector: ConnectorTable,
    pub dispatch: DispatchTable,
}

/// `use` names the connector; every other key is passed to it as config.
#[derive(Debug, Deserialize)]
pub struct ConnectorTable {
    #[serde(rename = "use")]
    pub use_: String,
    #[serde(flatten)]
    pub config: toml::Table,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DispatchTable {
    pub agent: Option<String>,
    pub agent_args: Vec<String>,
    pub repo: Option<String>,
    pub worktree: bool,
    pub branch: Option<String>,
    pub tags: Vec<String>,
    pub machine: Option<String>,
    pub timeout: Option<String>,
    pub max_tasks_per_run: Option<u32>,
    pub backfill: Option<String>,
    pub prompt: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub name: String,
    pub schedule: Schedule,
    pub enabled: bool,
    pub connector: String,
    /// The `[connector]` table minus `use`, as JSON for the connector's stdin.
    pub connector_config: Value,
    /// Unrendered; `{{ item.* }}`, `{{ job.name }}`, `{{ task.id }}` allowed.
    pub prompt: String,
    pub max_tasks_per_run: u32,
    pub backfill: Duration,
    /// `repo` and `branch` are unrendered templates too; the scheduler renders
    /// a copy per task.
    pub spec: DispatchSpec,
}

impl Job {
    /// Parse and validate one file's text. `stem` is the file name without
    /// `.toml`: it is the job's name, and a `name` key must agree with it.
    pub fn parse(text: &str, stem: &str, defaults: &Defaults) -> Result<Job, String> {
        let file: JobFile = toml::from_str(text).map_err(|e| e.to_string())?;
        let name = file.name.clone().unwrap_or_else(|| stem.to_string());
        if name != stem {
            return Err(format!(
                "name {name:?} does not match the file name {stem:?}"
            ));
        }
        check_name(&name)?;
        let schedule = Schedule::from_fields(file.every.as_deref(), file.cron.as_deref())?;
        if file.connector.use_.is_empty() {
            return Err("connector.use is required".into());
        }
        if !crate::connector::is_available(&file.connector.use_) {
            return Err(format!(
                "connector {:?} is not available (only the built-in clock exists until plugins ship)",
                file.connector.use_
            ));
        }
        let d = file.dispatch;
        if d.prompt.trim().is_empty() {
            return Err("dispatch.prompt is required".into());
        }
        if d.worktree && d.repo.is_none() {
            return Err("dispatch.worktree = true needs dispatch.repo".into());
        }
        for (field, text) in [
            ("prompt", Some(d.prompt.as_str())),
            ("branch", d.branch.as_deref()),
            ("repo", d.repo.as_deref()),
        ] {
            let Some(text) = text else { continue };
            for path in template::placeholders(text).map_err(|e| format!("dispatch.{field}: {e}"))? {
                let known = path.starts_with("item.") || path == "job.name" || path == "task.id";
                if !known {
                    return Err(format!(
                        "dispatch.{field}: unknown placeholder {{{{ {path} }}}}; use item.*, job.name or task.id"
                    ));
                }
            }
        }
        let timeout = match d.timeout.as_deref() {
            Some(t) => parse_duration(t).map_err(|e| format!("dispatch.timeout: {e}"))?,
            None => parse_duration(&defaults.timeout)
                .map_err(|e| format!("defaults.timeout: {e}"))?,
        };
        let backfill = match d.backfill.as_deref() {
            Some(b) => parse_duration(b).map_err(|e| format!("dispatch.backfill: {e}"))?,
            None => Duration::ZERO,
        };
        let max_tasks_per_run = d.max_tasks_per_run.unwrap_or(defaults.max_tasks_per_run);
        if max_tasks_per_run == 0 {
            return Err("dispatch.max_tasks_per_run must be at least 1".into());
        }
        let connector_config =
            serde_json::to_value(&file.connector.config).map_err(|e| e.to_string())?;
        Ok(Job {
            name,
            schedule,
            enabled: file.enabled,
            connector: file.connector.use_,
            connector_config,
            prompt: d.prompt,
            max_tasks_per_run,
            backfill,
            spec: DispatchSpec {
                agent: d.agent.unwrap_or_else(|| defaults.agent.clone()),
                agent_args: d.agent_args,
                repo: d.repo,
                worktree: d.worktree,
                branch: d.branch,
                machine: d.machine,
                tags: d.tags,
                timeout_secs: timeout.as_secs(),
            },
        })
    }
}

/// Job names appear in `tasks.job`, in `{{ job.name }}` and, from plan 3, as a
/// directory under the state dir, so they are kept to a safe alphabet. `run`
/// is what one-off tasks carry in `tasks.job`.
fn check_name(name: &str) -> Result<(), String> {
    let first_ok = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let rest_ok = name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_.-".contains(c));
    if !(first_ok && rest_ok && name.len() <= 64) {
        return Err(format!(
            "job name {name:?} must match [a-z0-9][a-z0-9_.-]{{0,63}}"
        ));
    }
    if name == "run" {
        return Err("job name \"run\" is reserved for one-off tasks".into());
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub enum Loaded {
    Valid(Job),
    Invalid { name: String, error: String },
}

impl Loaded {
    pub fn name(&self) -> &str {
        match self {
            Loaded::Valid(j) => &j.name,
            Loaded::Invalid { name, .. } => name,
        }
    }
}

pub fn job_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.toml"))
}

/// Every `*.toml` in `dir`, sorted by name, each valid or invalid with its
/// reason. A missing directory is simply no jobs.
pub fn load_dir(dir: &Path, defaults: &Defaults) -> anyhow::Result<Vec<Loaded>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).with_context(|| format!("read {}", dir.display())),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        out.push(load_file(&path, stem, defaults));
    }
    out.sort_by(|a, b| a.name().cmp(b.name()));
    Ok(out)
}

pub fn load_file(path: &Path, stem: &str, defaults: &Defaults) -> Loaded {
    let parsed = std::fs::read_to_string(path)
        .map_err(|e| e.to_string())
        .and_then(|text| Job::parse(&text, stem, defaults));
    match parsed {
        Ok(job) => Loaded::Valid(job),
        Err(error) => Loaded::Invalid {
            name: stem.to_string(),
            error,
        },
    }
}

/// `pastor job enable|disable`: rewrite the top-level `enabled` line (or insert
/// one before the first table) and nothing else, so comments and layout the
/// user wrote survive. The result must still parse or the file is left alone.
pub fn set_enabled(path: &Path, enabled: bool) -> anyhow::Result<()> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let line = format!("enabled = {enabled}");
    let mut out: Vec<String> = Vec::new();
    let mut replaced = false;
    let mut top_level = true;
    for l in text.lines() {
        let t = l.trim_start();
        if t.starts_with('[') {
            top_level = false;
        }
        let is_enabled_key = t
            .strip_prefix("enabled")
            .is_some_and(|rest| rest.trim_start().starts_with('='));
        if top_level && !replaced && is_enabled_key {
            out.push(line.clone());
            replaced = true;
        } else {
            out.push(l.to_string());
        }
    }
    if !replaced {
        let at = out
            .iter()
            .position(|l| l.trim_start().starts_with('['))
            .unwrap_or(out.len());
        out.insert(at, line);
    }
    let mut new_text = out.join("\n");
    if text.ends_with('\n') || !text.is_empty() {
        new_text.push('\n');
    }
    toml::from_str::<JobFile>(&new_text)
        .with_context(|| format!("{} would not parse after the edit", path.display()))?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, &new_text).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}
```

Add `pub mod job;` at the top of `src/config/mod.rs` next to `pub mod flock;`.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib config::job:: && make check`
Expected: green. If `toml::from_str` with `#[serde(flatten)]` into `toml::Table` rejects the spec example, replace the flatten with a manual split: deserialize `[connector]` as `toml::Table`, `remove("use")`, and error if absent. Keep the tests as they are.

- [ ] **Step 5: Commit**

```bash
git add src/config/job.rs src/config/mod.rs
git commit -m "feat: job files: parse, validate, list and toggle" -m "Validation is complete at load so a broken file shows as invalid with its reason in job list. enable/disable rewrites one line instead of round-tripping the TOML, which would drop the user's comments."
```

---

### Task 9: Store: seen keys and job state (schema v2)

**Files:**
- Modify: `src/store.rs` (`SCHEMA_VERSION`, `init`, new `JobState`, new methods, tests)

**Interfaces:**
- Produces:
  ```rust
  // src/store.rs
  pub struct JobState { pub name: String, pub last_run_at: Option<DateTime<Utc>>, pub last_ok_at: Option<DateTime<Utc>>,
                        pub last_result: Option<String>, pub last_error: Option<String>, pub cursor: Option<String>,
                        pub failures: u32, pub backoff_until: Option<DateTime<Utc>> }   // Default, Serialize, Deserialize, Clone, PartialEq
  impl Store {
      pub fn job_state(&self, name: &str) -> anyhow::Result<Option<JobState>>;
      pub fn job_states(&self) -> anyhow::Result<Vec<JobState>>;
      pub fn save_job_state(&self, s: &JobState) -> anyhow::Result<()>;      // insert or replace
      pub fn is_seen(&self, job: &str, key: &str) -> anyhow::Result<bool>;
      /// Insert a queued task for `item` (which must carry a string `key`), render it with its id,
      /// and record (job, key) as seen, all in one transaction. A key already seen is an error
      /// and nothing is written.
      pub fn insert_job_task(&self, job: &str, item: &Value,
          render: impl FnOnce(i64) -> Result<(String, DispatchSpec), String>) -> anyhow::Result<Task>;
  }
  ```

- [ ] **Step 1: Write the failing tests**

Add to `src/store.rs` `mod tests`:

```rust
    #[test]
    fn job_state_round_trips_and_lists() {
        let s = Store::open_in_memory().unwrap();
        assert!(s.job_state("a").unwrap().is_none());
        let now = Utc::now();
        let st = JobState {
            name: "a".into(),
            last_run_at: Some(now),
            last_ok_at: Some(now),
            last_result: Some("ok: 1 items, 1 tasks".into()),
            last_error: None,
            cursor: Some("c1".into()),
            failures: 0,
            backoff_until: None,
        };
        s.save_job_state(&st).unwrap();
        let got = s.job_state("a").unwrap().unwrap();
        assert_eq!(got.cursor.as_deref(), Some("c1"));
        assert_eq!(got.last_run_at.unwrap().timestamp_millis(), now.timestamp_millis());
        let failed = JobState {
            failures: 2,
            backoff_until: Some(now),
            last_error: Some("boom".into()),
            ..st.clone()
        };
        s.save_job_state(&failed).unwrap();
        assert_eq!(s.job_state("a").unwrap().unwrap().failures, 2, "replace, not duplicate");
        s.save_job_state(&JobState { name: "b".into(), ..Default::default() }).unwrap();
        assert_eq!(
            s.job_states().unwrap().iter().map(|j| j.name.clone()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn insert_job_task_renders_with_its_id_and_marks_seen() {
        let s = Store::open_in_memory().unwrap();
        let item = serde_json::json!({"key": "k1", "title": "t"});
        assert!(!s.is_seen("j", "k1").unwrap());
        let t = s
            .insert_job_task("j", &item, |id| {
                Ok((
                    format!("prompt for t-{id}"),
                    DispatchSpec {
                        branch: Some(format!("pastor/t-{id}")),
                        ..spec()
                    },
                ))
            })
            .unwrap();
        assert_eq!(t.job, "j");
        assert_eq!(t.state, TaskState::Queued);
        assert_eq!(t.prompt, format!("prompt for t-{}", t.id));
        assert_eq!(t.spec.branch.as_deref(), Some(format!("pastor/t-{}", t.id).as_str()));
        assert_eq!(t.item["key"], "k1");
        assert!(s.is_seen("j", "k1").unwrap());
        assert!(!s.is_seen("other", "k1").unwrap(), "seen is per job");

        // The same key again: refused, nothing written.
        let before = s.list_tasks(&TaskFilter::default()).unwrap().len();
        assert!(s.insert_job_task("j", &item, |_| Ok(("x".into(), spec()))).is_err());
        assert_eq!(s.list_tasks(&TaskFilter::default()).unwrap().len(), before);

        // A render failure rolls the whole thing back: no task, key still unseen.
        let item2 = serde_json::json!({"key": "k2"});
        let err = s
            .insert_job_task("j", &item2, |_| Err("nope".into()))
            .unwrap_err();
        assert!(err.to_string().contains("nope"), "{err}");
        assert_eq!(s.list_tasks(&TaskFilter::default()).unwrap().len(), before);
        assert!(!s.is_seen("j", "k2").unwrap());

        // No string key: refused up front.
        assert!(s.insert_job_task("j", &serde_json::json!({"title": "no key"}), |_| Ok(("x".into(), spec()))).is_err());
    }

    #[test]
    fn schema_v1_databases_are_migrated_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pastor.db");
        {
            let s = Store::open(&path).unwrap();
            s.insert_task(new_task("run")).unwrap();
            s.execute_raw("DROP TABLE seen; DROP TABLE job_state; UPDATE meta SET value = '1' WHERE key = 'schema_version'");
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.list_tasks(&TaskFilter::default()).unwrap().len(), 1, "data survives");
        assert!(!s.is_seen("j", "k").unwrap(), "the new tables exist");
        let v: String = s.meta("schema_version").unwrap().unwrap();
        assert_eq!(v, "2");
    }
```

Add to the store a small test-visible accessor next to `execute_raw`:

```rust
    #[cfg(test)]
    pub(crate) fn meta(&self, key: &str) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| r.get(0))
            .optional()?)
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib store::`
Expected: compile errors.

- [ ] **Step 3: Implement**

`src/store.rs`: `const SCHEMA_VERSION: i64 = 2;`. In `init`, extend the `execute_batch` with the new tables (after the `tasks_machine` index):

```sql
             CREATE TABLE IF NOT EXISTS seen (
                job TEXT NOT NULL,
                key TEXT NOT NULL,
                task_id INTEGER,
                seen_at TEXT NOT NULL,
                PRIMARY KEY (job, key)
             );
             CREATE TABLE IF NOT EXISTS job_state (
                name TEXT PRIMARY KEY,
                last_run_at TEXT,
                last_ok_at TEXT,
                last_result TEXT,
                last_error TEXT,
                cursor TEXT,
                failures INTEGER NOT NULL DEFAULT 0,
                backoff_until TEXT
             );
```

In the version match, the `Some(v) if v < SCHEMA_VERSION` arm keeps its comment; the `CREATE TABLE IF NOT EXISTS` above already performed the v1→v2 migration, so the arm only bumps the version. Add a comment saying so:

```rust
            Some(v) if v < SCHEMA_VERSION => {
                // v1 -> v2 added `seen` and `job_state`; both are created above
                // with IF NOT EXISTS, so there is nothing left to do but record
                // it. Future migrations that alter existing tables go here, one
                // `if v < N` block each, before the version bump.
```

Add the type and methods:

```rust
/// Per-job bookkeeping the scheduler needs across restarts.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct JobState {
    pub name: String,
    /// Start of the last attempt, successful or not; drives the schedule.
    pub last_run_at: Option<DateTime<Utc>>,
    /// Start of the last successful run; the next run's `since`.
    pub last_ok_at: Option<DateTime<Utc>>,
    pub last_result: Option<String>,
    pub last_error: Option<String>,
    pub cursor: Option<String>,
    pub failures: u32,
    pub backoff_until: Option<DateTime<Utc>>,
}
```

```rust
    pub fn job_state(&self, name: &str) -> anyhow::Result<Option<JobState>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT * FROM job_state WHERE name = ?1",
                params![name],
                row_to_job_state,
            )
            .optional()?)
    }

    pub fn job_states(&self) -> anyhow::Result<Vec<JobState>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT * FROM job_state ORDER BY name")?;
        let rows = stmt.query_map([], row_to_job_state)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn save_job_state(&self, s: &JobState) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO job_state (name, last_run_at, last_ok_at, last_result, last_error, cursor, failures, backoff_until)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                s.name,
                s.last_run_at.map(|d| d.to_rfc3339()),
                s.last_ok_at.map(|d| d.to_rfc3339()),
                s.last_result,
                s.last_error,
                s.cursor,
                s.failures as i64,
                s.backoff_until.map(|d| d.to_rfc3339()),
            ],
        )?;
        Ok(())
    }

    pub fn is_seen(&self, job: &str, key: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM seen WHERE job = ?1 AND key = ?2",
            params![job, key],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Insert a queued task for `item`, render its prompt and spec with the id
    /// it was given, and record `(job, key)` as seen: one transaction, so no
    /// reader ever sees an unrendered task and a render failure leaves the key
    /// unseen. A key already in `seen` violates the primary key and nothing is
    /// written.
    pub fn insert_job_task(
        &self,
        job: &str,
        item: &Value,
        render: impl FnOnce(i64) -> Result<(String, DispatchSpec), String>,
    ) -> anyhow::Result<Task> {
        let key = item
            .get("key")
            .and_then(Value::as_str)
            .context("item has no string key")?
            .to_string();
        let now = Utc::now().to_rfc3339();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO tasks (job, item, prompt, spec, state, created_at, updated_at) VALUES (?1, ?2, '', '{}', 'queued', ?3, ?3)",
            params![job, serde_json::to_string(item)?, now],
        )?;
        let id = tx.last_insert_rowid();
        let (prompt, spec) =
            render(id).map_err(|e| anyhow::anyhow!("render task t-{id} for job {job}: {e}"))?;
        tx.execute(
            "UPDATE tasks SET prompt = ?2, spec = ?3 WHERE id = ?1",
            params![id, prompt, serde_json::to_string(&spec)?],
        )?;
        tx.execute(
            "INSERT INTO seen (job, key, task_id, seen_at) VALUES (?1, ?2, ?3, ?4)",
            params![job, key, id, now],
        )?;
        tx.commit()?;
        drop(conn);
        self.get_task(id)?.context("task vanished after insert")
    }
```

and the row mapper next to `row_to_task`:

```rust
fn row_to_job_state(row: &Row<'_>) -> rusqlite::Result<JobState> {
    let parse_dt = |s: Option<String>| -> rusqlite::Result<Option<DateTime<Utc>>> {
        s.as_deref()
            .map(|s| {
                DateTime::parse_from_rfc3339(s)
                    .map(|d| d.with_timezone(&Utc))
                    .map_err(conversion_failure)
            })
            .transpose()
    };
    Ok(JobState {
        name: row.get("name")?,
        last_run_at: parse_dt(row.get("last_run_at")?)?,
        last_ok_at: parse_dt(row.get("last_ok_at")?)?,
        last_result: row.get("last_result")?,
        last_error: row.get("last_error")?,
        cursor: row.get("cursor")?,
        failures: row.get::<_, i64>("failures")? as u32,
        backoff_until: parse_dt(row.get("backoff_until")?)?,
    })
}
```

Note `'{}'` as the placeholder spec is never visible: the `UPDATE` in the same transaction replaces it before commit, and `render` failing rolls the insert back when `tx` drops.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib store:: && make check`
Expected: green.

- [ ] **Step 5: Commit**

```bash
git add src/store.rs
git commit -m "feat: seen keys and job state in the store (schema v2)" -m "insert_job_task renders with the task id inside the insert's transaction: the id is the only way to fill {{ task.id }}, and an unrendered row must never be visible to a dispatch pass. The seen row rides in the same transaction so a render failure leaves the key for the next run."
```

---

### Task 10: One job run: items in, tasks out

**Files:**
- Create: `src/scheduler.rs` (the `run_job` half; Task 11 adds the loop to the same file)
- Modify: `src/lib.rs` (add `pub mod scheduler;`)

**Interfaces:**
- Produces:
  ```rust
  // src/scheduler.rs
  pub const QUEUED_WARN_AFTER: Duration;                       // 1h
  pub enum RunOutcome { Ran, DryRun, Failed, Skipped, NotDue, Disabled, Invalid, Unknown }  // serde snake_case
  pub struct JobRunReport { pub job: String, pub outcome: RunOutcome, pub items: usize, pub created: Vec<String>,
                            pub skipped_seen: usize, pub deferred: usize, pub error: Option<String> }
  impl JobRunReport { pub fn new(job: &str, outcome: RunOutcome) -> JobRunReport }
  pub fn backoff_for(failures: u32) -> Duration;                // min(1h, 60s * 2^(failures-1))
  pub fn render_task(job: &Job, item: &Value, id: i64) -> Result<(String, DispatchSpec), String>;
  pub async fn run_job(store: &Store, job: &Job, source: &dyn ItemSource, events: &broadcast::Sender<PastorEvent>,
                       now: DateTime<Utc>, dry_run: bool) -> JobRunReport;
  ```
- Consumes: `Store::{job_state, save_job_state, is_seen, insert_job_task}`, `JobState`, `Job`, `ItemSource`, `RunInput`, `template::render`, `PastorEvent`.

- [ ] **Step 1: Write the failing tests**

Create `src/scheduler.rs` with:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector::{Item, RunFuture, RunOutput};
    use crate::schedule::Schedule;
    use crate::store::TaskFilter;
    use crate::task::TaskState;
    use serde_json::{Map, Value, json};
    use std::sync::Mutex;

    /// A connector with a script: the items to emit, the cursor to return, or
    /// an error, plus a record of what it was asked.
    pub(super) struct Scripted {
        pub items: Mutex<Vec<Item>>,
        pub cursor: Mutex<Option<String>>,
        pub fail: Mutex<Option<String>>,
        pub inputs: Mutex<Vec<RunInput>>,
    }

    impl Scripted {
        pub fn with_keys(keys: &[&str]) -> Scripted {
            Scripted {
                items: Mutex::new(keys.iter().map(|k| item(k)).collect()),
                cursor: Mutex::new(None),
                fail: Mutex::new(None),
                inputs: Mutex::new(Vec::new()),
            }
        }
    }

    pub(super) fn item(key: &str) -> Item {
        let mut fields = Map::new();
        fields.insert("title".into(), Value::String(format!("title of {key}")));
        Item::new(key, fields)
    }

    impl ItemSource for Scripted {
        fn id(&self) -> &str {
            "scripted"
        }
        fn run<'a>(&'a self, input: RunInput) -> RunFuture<'a> {
            Box::pin(async move {
                self.inputs.lock().unwrap().push(input);
                if let Some(err) = self.fail.lock().unwrap().clone() {
                    return Err(err);
                }
                Ok(RunOutput {
                    items: self.items.lock().unwrap().clone(),
                    cursor: self.cursor.lock().unwrap().clone(),
                    logs: vec!["scripted ran".into()],
                })
            })
        }
    }

    pub(super) fn job(name: &str) -> Job {
        Job {
            name: name.into(),
            schedule: Schedule::Every(Duration::from_secs(60)),
            enabled: true,
            connector: "scripted".into(),
            connector_config: json!({"channel": "C1"}),
            prompt: "{{ job.name }}: {{ item.title }} ({{ task.id }})".into(),
            max_tasks_per_run: 5,
            backfill: Duration::from_secs(600),
            spec: DispatchSpec {
                agent: "claude".into(),
                agent_args: vec![],
                repo: Some("/srv/{{ job.name }}".into()),
                worktree: true,
                branch: Some("pastor/{{ item.key }}".into()),
                machine: None,
                tags: vec![],
                timeout_secs: 60,
            },
        }
    }

    fn events() -> (broadcast::Sender<PastorEvent>, broadcast::Receiver<PastorEvent>) {
        broadcast::channel(16)
    }

    #[tokio::test]
    async fn creates_one_task_per_new_item_with_rendered_templates() {
        let store = Store::open_in_memory().unwrap();
        let src = Scripted::with_keys(&["k1", "k2"]);
        *src.cursor.lock().unwrap() = Some("c1".into());
        let (tx, _rx) = events();
        let now = Utc::now();
        let report = run_job(&store, &job("j"), &src, &tx, now, false).await;
        assert_eq!(report.outcome, RunOutcome::Ran);
        assert_eq!(report.items, 2);
        assert_eq!(report.created, vec!["t-1", "t-2"]);
        assert_eq!(report.skipped_seen, 0);
        assert_eq!(report.deferred, 0);
        assert!(report.error.is_none());

        let tasks = store.list_tasks(&TaskFilter::default()).unwrap();
        assert_eq!(tasks.len(), 2);
        let t1 = tasks.iter().find(|t| t.id == 1).unwrap();
        assert_eq!(t1.job, "j");
        assert_eq!(t1.state, TaskState::Queued);
        assert_eq!(t1.prompt, "j: title of k1 (t-1)");
        assert_eq!(t1.spec.repo.as_deref(), Some("/srv/j"));
        assert_eq!(t1.spec.branch.as_deref(), Some("pastor/k1"));
        assert!(t1.spec.worktree);
        assert_eq!(t1.item["key"], "k1");
        assert!(store.is_seen("j", "k1").unwrap() && store.is_seen("j", "k2").unwrap());

        let state = store.job_state("j").unwrap().unwrap();
        assert_eq!(state.cursor.as_deref(), Some("c1"));
        assert_eq!(state.failures, 0);
        assert_eq!(state.last_result.as_deref(), Some("ok: 2 items, 2 tasks"));
        assert!(state.last_run_at.is_some() && state.last_ok_at.is_some());

        // First run: since = now - backfill, cursor = null, config passed through.
        let input = &src.inputs.lock().unwrap()[0];
        assert_eq!(input.since, now - chrono::Duration::seconds(600));
        assert!(input.cursor.is_none());
        assert_eq!(input.config["channel"], "C1");

        // Second run: since = last ok run, cursor = the persisted one.
        let later = now + chrono::Duration::seconds(60);
        run_job(&store, &job("j"), &src, &tx, later, false).await;
        let input = &src.inputs.lock().unwrap()[1];
        assert_eq!(input.since, state.last_ok_at.unwrap());
        assert_eq!(input.cursor.as_deref(), Some("c1"));
    }

    #[tokio::test]
    async fn seen_keys_and_in_run_duplicates_create_one_task() {
        let store = Store::open_in_memory().unwrap();
        let (tx, _rx) = events();
        let src = Scripted::with_keys(&["k1"]);
        run_job(&store, &job("j"), &src, &tx, Utc::now(), false).await;
        *src.items.lock().unwrap() = vec![item("k1"), item("k1"), item("k2"), item("k2")];
        let report = run_job(&store, &job("j"), &src, &tx, Utc::now(), false).await;
        assert_eq!(report.items, 4);
        assert_eq!(report.created, vec!["t-2"], "k2 once; k1 was seen, repeats collapse");
        assert_eq!(report.skipped_seen, 1);
        assert_eq!(store.list_tasks(&TaskFilter::default()).unwrap().len(), 2);
        // Seen is per job: another job sees k1 as new.
        let report = run_job(&store, &job("other"), &Scripted::with_keys(&["k1"]), &tx, Utc::now(), false).await;
        assert_eq!(report.created.len(), 1);
    }

    #[tokio::test]
    async fn max_tasks_per_run_defers_the_rest_unseen() {
        let store = Store::open_in_memory().unwrap();
        let (tx, _rx) = events();
        let mut j = job("j");
        j.max_tasks_per_run = 2;
        let src = Scripted::with_keys(&["k1", "k2", "k3", "k4"]);
        let report = run_job(&store, &j, &src, &tx, Utc::now(), false).await;
        assert_eq!(report.created, vec!["t-1", "t-2"]);
        assert_eq!(report.deferred, 2);
        assert!(!store.is_seen("j", "k3").unwrap(), "deferred items stay unseen");
        let report = run_job(&store, &j, &src, &tx, Utc::now(), false).await;
        assert_eq!(report.created, vec!["t-3", "t-4"]);
        assert_eq!(report.skipped_seen, 2);
        assert_eq!(report.deferred, 0);
    }

    #[tokio::test]
    async fn missing_item_field_renders_empty_and_warns() {
        let store = Store::open_in_memory().unwrap();
        let (tx, _rx) = events();
        let mut j = job("j");
        j.prompt = "[{{ item.title }}] {{ item.author }}!".into();
        let src = Scripted::with_keys(&["k1"]);
        let report = run_job(&store, &j, &src, &tx, Utc::now(), false).await;
        assert_eq!(report.created, vec!["t-1"]);
        let t = store.get_task(1).unwrap().unwrap();
        assert_eq!(t.prompt, "[title of k1] !");
        assert!(store.is_seen("j", "k1").unwrap());
    }

    #[tokio::test]
    async fn a_failing_connector_backs_off_keeps_cursor_and_emits_job_failed() {
        let store = Store::open_in_memory().unwrap();
        let (tx, mut rx) = events();
        let src = Scripted::with_keys(&["k1"]);
        *src.cursor.lock().unwrap() = Some("c1".into());
        let t0 = Utc::now();
        run_job(&store, &job("j"), &src, &tx, t0, false).await;

        *src.fail.lock().unwrap() = Some("boom: 503 from upstream".into());
        let t1 = t0 + chrono::Duration::seconds(60);
        let report = run_job(&store, &job("j"), &src, &tx, t1, false).await;
        assert_eq!(report.outcome, RunOutcome::Failed);
        assert!(report.error.as_deref().unwrap().contains("boom"));
        let s = store.job_state("j").unwrap().unwrap();
        assert_eq!(s.failures, 1);
        assert_eq!(s.backoff_until, Some(t1 + chrono::Duration::seconds(60)));
        assert_eq!(s.cursor.as_deref(), Some("c1"), "cursor kept on failure");
        assert_eq!(s.last_ok_at, Some(t0), "since stays at the last success");
        assert_eq!(s.last_run_at, Some(t1));
        assert!(s.last_error.as_deref().unwrap().contains("boom"));
        assert!(s.last_result.as_deref().unwrap().starts_with("failed"));
        let ev = rx.try_recv().expect("job.failed emitted");
        assert_eq!(ev.kind, "job.failed");
        assert_eq!(ev.job.as_deref(), Some("j"));
        assert!(ev.machine.is_none() && ev.task_id.is_none());

        let t2 = t1 + chrono::Duration::seconds(120);
        run_job(&store, &job("j"), &src, &tx, t2, false).await;
        let s = store.job_state("j").unwrap().unwrap();
        assert_eq!(s.failures, 2);
        assert_eq!(s.backoff_until, Some(t2 + chrono::Duration::seconds(120)));

        *src.fail.lock().unwrap() = None;
        run_job(&store, &job("j"), &src, &tx, t2 + chrono::Duration::seconds(300), false).await;
        let s = store.job_state("j").unwrap().unwrap();
        assert_eq!(s.failures, 0);
        assert!(s.backoff_until.is_none() && s.last_error.is_none());
    }

    #[tokio::test]
    async fn dry_run_writes_nothing() {
        let store = Store::open_in_memory().unwrap();
        let (tx, mut rx) = events();
        let src = Scripted::with_keys(&["k1", "k2"]);
        let report = run_job(&store, &job("j"), &src, &tx, Utc::now(), true).await;
        assert_eq!(report.outcome, RunOutcome::DryRun);
        assert_eq!(report.created, vec!["k1", "k2"], "keys, since no task ids exist");
        assert!(store.list_tasks(&TaskFilter::default()).unwrap().is_empty());
        assert!(!store.is_seen("j", "k1").unwrap());
        assert!(store.job_state("j").unwrap().is_none());
        // A failing dry run does not back the job off either.
        *src.fail.lock().unwrap() = Some("boom".into());
        let report = run_job(&store, &job("j"), &src, &tx, Utc::now(), true).await;
        assert_eq!(report.outcome, RunOutcome::Failed);
        assert!(store.job_state("j").unwrap().is_none());
        assert!(rx.try_recv().is_err(), "no job.failed on a dry run");
    }

    #[test]
    fn backoff_doubles_from_a_minute_and_caps_at_an_hour() {
        assert_eq!(backoff_for(1), Duration::from_secs(60));
        assert_eq!(backoff_for(2), Duration::from_secs(120));
        assert_eq!(backoff_for(6), Duration::from_secs(1920));
        assert_eq!(backoff_for(7), Duration::from_secs(3600));
        assert_eq!(backoff_for(40), Duration::from_secs(3600));
        assert_eq!(backoff_for(0), Duration::from_secs(60), "defensive: never zero");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib scheduler::`
Expected: compile errors.

- [ ] **Step 3: Implement**

Prepend to `src/scheduler.rs`:

```rust
//! The tick. `run_job` is one job's pass: ask its connector, drop seen keys,
//! render and queue a task per new item, record the run. `Scheduler` (below,
//! Task 11) owns the loop that calls it.

use std::collections::HashSet;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

use crate::config::job::Job;
use crate::connector::{ItemSource, RunInput};
use crate::machine::PastorEvent;
use crate::store::{JobState, Store};
use crate::task::DispatchSpec;
use crate::template;

/// A task queued longer than this gets one warning in the log: no machine has
/// had capacity for an hour is worth a human's eye.
pub const QUEUED_WARN_AFTER: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    Ran,
    DryRun,
    Failed,
    /// Due, but the previous run was still going.
    Skipped,
    NotDue,
    Disabled,
    Invalid,
    Unknown,
}

impl std::fmt::Display for RunOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = serde_json::to_value(self).expect("unit variant");
        f.write_str(s.as_str().unwrap_or("?"))
    }
}

/// What one `run_job` did, for the log and for `pastor tick`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRunReport {
    pub job: String,
    pub outcome: RunOutcome,
    /// Items the connector emitted, before any filtering.
    pub items: usize,
    /// Task ids created (`t-3`); on a dry run, the item keys that would have.
    pub created: Vec<String>,
    pub skipped_seen: usize,
    /// New items beyond `max_tasks_per_run`, left unseen for the next run.
    pub deferred: usize,
    pub error: Option<String>,
}

impl JobRunReport {
    pub fn new(job: &str, outcome: RunOutcome) -> JobRunReport {
        JobRunReport {
            job: job.into(),
            outcome,
            items: 0,
            created: Vec::new(),
            skipped_seen: 0,
            deferred: 0,
            error: None,
        }
    }
}

/// Connector failures: a minute, doubling, capped at an hour. `failures` is the
/// count including this one.
pub fn backoff_for(failures: u32) -> Duration {
    let steps = failures.saturating_sub(1).min(6);
    Duration::from_secs(60u64 << steps).min(Duration::from_secs(3600))
}

/// Render prompt, repo and branch for one task. A placeholder with no value
/// renders empty and is logged: an item that failed to render would stay
/// unseen and fail again every run.
pub fn render_task(job: &Job, item: &Value, id: i64) -> Result<(String, DispatchSpec), String> {
    let ctx = serde_json::json!({
        "item": item,
        "job": {"name": job.name},
        "task": {"id": format!("t-{id}")},
    });
    let mut missing: Vec<String> = Vec::new();
    let mut render = |field: &str, text: &str| -> Result<String, String> {
        let r = template::render(text, &ctx).map_err(|e| format!("{field}: {e}"))?;
        missing.extend(r.missing.into_iter().map(|m| format!("{field}: {m}")));
        Ok(r.text)
    };
    let prompt = render("prompt", &job.prompt)?;
    let repo = job.spec.repo.as_deref().map(|r| render("repo", r)).transpose()?;
    let branch = job.spec.branch.as_deref().map(|b| render("branch", b)).transpose()?;
    if !missing.is_empty() {
        tracing::warn!(job = %job.name, task = %format!("t-{id}"), ?missing, "placeholders with no value rendered empty");
    }
    let spec = DispatchSpec {
        repo,
        branch,
        ..job.spec.clone()
    };
    Ok((prompt, spec))
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(120).collect()
}

/// One pass of one job. Never panics and never returns early without a
/// report; the report is what the log and `pastor tick` show. With `dry_run`
/// nothing is written: no tasks, no seen keys, no job state, no event.
pub async fn run_job(
    store: &Store,
    job: &Job,
    source: &dyn ItemSource,
    events: &broadcast::Sender<PastorEvent>,
    now: DateTime<Utc>,
    dry_run: bool,
) -> JobRunReport {
    let mut report = JobRunReport::new(
        &job.name,
        if dry_run { RunOutcome::DryRun } else { RunOutcome::Ran },
    );
    let mut state = match store.job_state(&job.name) {
        Ok(s) => s.unwrap_or_else(|| JobState {
            name: job.name.clone(),
            ..Default::default()
        }),
        Err(e) => {
            report.outcome = RunOutcome::Failed;
            report.error = Some(format!("job state: {e:#}"));
            return report;
        }
    };
    let backfill = chrono::Duration::from_std(job.backfill).unwrap_or_else(|_| chrono::Duration::zero());
    let input = RunInput {
        config: job.connector_config.clone(),
        cursor: state.cursor.clone(),
        since: state.last_ok_at.unwrap_or(now - backfill),
        now,
    };
    let output = match source.run(input).await {
        Ok(o) => o,
        Err(err) => {
            tracing::warn!(job = %job.name, %err, "connector failed");
            report.outcome = RunOutcome::Failed;
            report.error = Some(err.clone());
            if !dry_run {
                state.failures += 1;
                state.last_run_at = Some(now);
                let wait = chrono::Duration::from_std(backoff_for(state.failures))
                    .unwrap_or_else(|_| chrono::Duration::zero());
                state.backoff_until = Some(now + wait);
                state.last_result = Some(format!("failed ({}x): {}", state.failures, first_line(&err)));
                state.last_error = Some(err);
                if let Err(e) = store.save_job_state(&state) {
                    tracing::error!(job = %job.name, %e, "save job state");
                }
                let _ = events.send(PastorEvent {
                    kind: "job.failed".into(),
                    task_id: None,
                    machine: None,
                    job: Some(job.name.clone()),
                });
            }
            return report;
        }
    };
    for line in &output.logs {
        tracing::info!(job = %job.name, "{line}");
    }
    report.items = output.items.len();
    let mut in_run: HashSet<&str> = HashSet::new();
    for item in &output.items {
        if item.key.is_empty() {
            tracing::warn!(job = %job.name, "item without a key skipped");
            continue;
        }
        if !in_run.insert(item.key.as_str()) {
            continue; // duplicate key in one run: first wins
        }
        match store.is_seen(&job.name, &item.key) {
            Ok(true) => {
                report.skipped_seen += 1;
                continue;
            }
            Ok(false) => {}
            Err(e) => {
                report.outcome = RunOutcome::Failed;
                report.error = Some(format!("seen-store: {e:#}"));
                return report;
            }
        }
        if report.created.len() as u32 >= job.max_tasks_per_run {
            report.deferred += 1;
            continue;
        }
        if dry_run {
            report.created.push(item.key.clone());
            continue;
        }
        let value = item.as_value();
        match store.insert_job_task(&job.name, &value, |id| render_task(job, &value, id)) {
            Ok(t) => {
                tracing::info!(job = %job.name, task = %t.display_id(), key = %item.key, "task queued");
                report.created.push(t.display_id());
            }
            Err(e) => {
                tracing::error!(job = %job.name, key = %item.key, %e, "create task");
                report.error = Some(format!("{}: {e:#}", item.key));
            }
        }
    }
    if report.deferred > 0 {
        tracing::info!(
            job = %job.name,
            deferred = report.deferred,
            max = job.max_tasks_per_run,
            "max_tasks_per_run reached; the rest stay unseen for the next run"
        );
    }
    if !dry_run {
        state.failures = 0;
        state.backoff_until = None;
        state.last_run_at = Some(now);
        state.last_ok_at = Some(now);
        if output.cursor.is_some() {
            state.cursor = output.cursor;
        }
        state.last_result = Some(format!(
            "ok: {} items, {} tasks",
            report.items,
            report.created.len()
        ));
        state.last_error = None;
        if let Err(e) = store.save_job_state(&state) {
            tracing::error!(job = %job.name, %e, "save job state");
        }
    }
    report
}
```

Add `pub mod scheduler;` to `src/lib.rs`.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib scheduler:: && make check`
Expected: green.

- [ ] **Step 5: Commit**

```bash
git add src/scheduler.rs src/lib.rs
git commit -m "feat: one job run: connector items to queued tasks" -m "Dedup order matters: in-run duplicates first, then the seen-store, then the per-run cap, so a deferred item is never marked seen. A failed connector keeps its cursor and last-ok time and backs the job off; a dry run touches nothing."
```

---

### Task 11: Scheduler loop, the fleet lock and the daemon

**Files:**
- Modify: `src/scheduler.rs` (add `Scheduler`, `SchedulerHandle`, `JobStatus`, loop, tests)
- Modify: `src/daemon.rs` (new `Fleet`; `Daemon` fields; `start`, `run_with_listener`, `handle`; tests)
- Modify: `src/ipc.rs` (new requests and responses; round-trip test)

**Interfaces:**
- Produces:
  ```rust
  // src/daemon.rs
  pub struct Fleet { pub machines: Vec<MachineHandle>, /* store, dispatch_lock */ }
  impl Fleet {
      pub fn new(machines: Vec<MachineHandle>, store: Arc<Store>) -> Fleet;
      pub fn get(&self, name: &str) -> Option<&MachineHandle>;
      pub fn views(&self) -> Vec<MachineView>;
      pub async fn dispatch_queued(&self);          // serialised by dispatch_lock
  }
  impl Daemon { pub fn fleet(&self) -> Arc<Fleet>; pub fn scheduler(&self) -> SchedulerHandle }
  // src/scheduler.rs
  pub struct JobStatus { pub name: String, pub schedule: Option<String>, pub enabled: bool, pub connector: Option<String>,
                         pub error: Option<String>, pub last_run_at: Option<DateTime<Utc>>, pub last_result: Option<String>,
                         pub next_due: Option<DateTime<Utc>>, pub running: bool }
  pub struct Scheduler { /* private */ }
  impl Scheduler {
      pub fn new(paths: Paths, config: &PastorConfig, store: Arc<Store>, fleet: Arc<Fleet>, events: broadcast::Sender<PastorEvent>) -> Scheduler;
      pub fn standalone(paths: Paths, config: &PastorConfig, store: Arc<Store>) -> Scheduler;   // no machines, no listeners: `pastor tick`/`job list` offline
      pub fn spawn(self) -> SchedulerHandle;
      pub fn reload(&mut self) -> bool;                       // re-read jobs/ if its fingerprint changed
      pub fn statuses(&self, now: DateTime<Utc>) -> Vec<JobStatus>;
      pub async fn pass(&mut self, now: DateTime<Utc>);       // one tick
      pub async fn tick_now(&mut self, only: Option<&str>, dry_run: bool, now: DateTime<Utc>) -> Vec<JobRunReport>;
  }
  #[derive(Clone)] pub struct SchedulerHandle { /* mpsc */ }
  impl SchedulerHandle {
      pub async fn tick(&self, job: Option<String>, dry_run: bool) -> anyhow::Result<Vec<JobRunReport>>;
      pub async fn fire(&self, name: &str) -> anyhow::Result<Result<String, String>>;   // outer: scheduler gone; inner: unknown/invalid job
      pub async fn reload(&self) -> anyhow::Result<Vec<JobStatus>>;
      pub async fn job_list(&self) -> anyhow::Result<Vec<JobStatus>>;
  }
  // src/ipc.rs
  pub enum IpcRequest { /* existing */ Tick { job: Option<String>, dry_run: bool }, Reload, JobList, JobRun { name: String } }
  pub enum IpcResponse { /* existing */ Runs(Vec<JobRunReport>), Jobs(Vec<JobStatus>) }
  ```
- Consumes: Task 10's `run_job`, `Fleet` (this task), `config::job::{load_dir, Loaded, Job}`, `Store::{queued_tasks, job_states}`, `ChannelState::accepts_dispatch`.

- [ ] **Step 1: Write the failing tests**

`src/ipc.rs`: in `every_response_variant_round_trips_through_json`, add `IpcResponse::Runs(vec![])` and `IpcResponse::Jobs(vec![])` to the list, and in a new test:

```rust
    #[test]
    fn scheduler_requests_round_trip() {
        for req in [
            IpcRequest::Tick { job: Some("j".into()), dry_run: true },
            IpcRequest::Reload,
            IpcRequest::JobList,
            IpcRequest::JobRun { name: "j".into() },
        ] {
            let json = serde_json::to_string(&req).unwrap();
            let back: IpcRequest = serde_json::from_str(&json).unwrap();
            assert_eq!(format!("{req:?}"), format!("{back:?}"), "{json}");
        }
    }
```

`src/scheduler.rs` `mod tests`, add (the `Scripted` and `job` helpers from Task 10 are reused; `Slow` is new):

```rust
    use crate::daemon::Fleet;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Takes a while and counts its runs: for the overlap rule.
    struct Slow {
        runs: Arc<AtomicUsize>,
        hold: Duration,
    }
    impl ItemSource for Slow {
        fn id(&self) -> &str {
            "slow"
        }
        fn run<'a>(&'a self, _input: RunInput) -> RunFuture<'a> {
            Box::pin(async move {
                self.runs.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(self.hold).await;
                Ok(RunOutput::default())
            })
        }
    }

    fn scheduler_with(store: &Arc<Store>) -> (Scheduler, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        let config = PastorConfig {
            tick: "1s".into(),
            ..Default::default()
        };
        let fleet = Arc::new(Fleet::new(vec![], store.clone()));
        let (events, _) = broadcast::channel(16);
        (Scheduler::new(paths, &config, store.clone(), fleet, events), tmp)
    }

    fn write_job(paths: &Paths, name: &str, text: &str) {
        std::fs::create_dir_all(paths.jobs_dir()).unwrap();
        std::fs::write(crate::config::job::job_path(&paths.jobs_dir(), name), text).unwrap();
    }

    const CLOCK_JOB: &str = "every = \"5m\"\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"tick {{ item.key }}\"\n";

    #[tokio::test]
    async fn overlapping_run_is_skipped() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (mut s, _tmp) = scheduler_with(&store);
        let runs = Arc::new(AtomicUsize::new(0));
        let slow: Arc<dyn ItemSource> = Arc::new(Slow {
            runs: runs.clone(),
            hold: Duration::from_millis(300),
        });
        s.set_source_for_tests("slow", slow);
        let mut j = job("j");
        j.connector = "slow".into();
        j.schedule = Schedule::Every(Duration::from_secs(1));
        s.set_jobs_for_tests(vec![j]);

        let t0 = Utc::now();
        s.pass(t0).await;
        s.pass(t0 + chrono::Duration::seconds(5)).await; // due again, but still running
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(runs.load(Ordering::SeqCst), 1, "the second pass must not start a second run");
        assert!(s.statuses(t0)[0].running);

        tokio::time::sleep(Duration::from_millis(400)).await;
        s.pass(t0 + chrono::Duration::seconds(10)).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(runs.load(Ordering::SeqCst), 2, "once finished, the next due pass runs it");
    }

    #[tokio::test]
    async fn fire_ignores_schedule_overlap_and_enabled() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (mut s, _tmp) = scheduler_with(&store);
        let runs = Arc::new(AtomicUsize::new(0));
        s.set_source_for_tests(
            "slow",
            Arc::new(Slow {
                runs: runs.clone(),
                hold: Duration::from_millis(200),
            }),
        );
        let mut j = job("j");
        j.connector = "slow".into();
        j.enabled = false;
        s.set_jobs_for_tests(vec![j]);
        s.pass(Utc::now()).await;
        assert_eq!(runs.load(Ordering::SeqCst), 0, "disabled jobs never run on a pass");
        assert!(s.fire("j", Utc::now()).is_ok());
        assert!(s.fire("j", Utc::now()).is_ok(), "fire twice: overlap rule does not apply");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(runs.load(Ordering::SeqCst), 2);
        assert!(s.fire("nope", Utc::now()).unwrap_err().contains("no job"));
    }

    #[tokio::test]
    async fn invalid_edit_keeps_previous_job_and_reports_error() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (mut s, tmp) = scheduler_with(&store);
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        write_job(&paths, "a", CLOCK_JOB);
        assert!(s.reload(), "first load counts as a change");
        assert!(!s.reload(), "unchanged directory is not reloaded");
        let now = Utc::now();
        let st = &s.statuses(now)[0];
        assert_eq!(st.name, "a");
        assert!(st.error.is_none());
        assert_eq!(st.schedule.as_deref(), Some("every 5m"));

        // Break the file: the previous version keeps running, the error shows.
        std::thread::sleep(Duration::from_millis(20)); // mtime granularity
        write_job(&paths, "a", "every = \"5m\"\n[connector\n");
        assert!(s.reload());
        let st = &s.statuses(now)[0];
        assert!(st.error.is_some(), "{st:?}");
        assert_eq!(st.schedule.as_deref(), Some("every 5m"), "previous version kept");
        let reports = s.tick_now(Some("a"), true, now).await;
        assert_eq!(reports[0].outcome, RunOutcome::DryRun, "the kept version still runs");

        // A brand-new invalid file has nothing to keep.
        write_job(&paths, "b", "nonsense");
        assert!(s.reload());
        let sts = s.statuses(now);
        assert_eq!(sts.len(), 2);
        assert!(sts[1].schedule.is_none() && sts[1].error.is_some());
        let reports = s.tick_now(Some("b"), false, now).await;
        assert_eq!(reports[0].outcome, RunOutcome::Invalid);

        // Removing a file removes the job.
        std::fs::remove_file(crate::config::job::job_path(&paths.jobs_dir(), "a")).unwrap();
        assert!(s.reload());
        assert_eq!(s.statuses(now).len(), 1);
    }

    #[tokio::test]
    async fn overdue_on_start_runs_once_and_missed_runs_are_not_replayed() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (mut s, tmp) = scheduler_with(&store);
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        write_job(&paths, "a", CLOCK_JOB);
        let now = Utc::now();
        store
            .save_job_state(&JobState {
                name: "a".into(),
                last_run_at: Some(now - chrono::Duration::hours(3)),
                last_ok_at: Some(now - chrono::Duration::hours(3)),
                ..Default::default()
            })
            .unwrap();
        let reports = s.tick_now(None, false, now).await;
        assert_eq!(reports[0].outcome, RunOutcome::Ran, "three hours overdue: runs once");
        assert_eq!(reports[0].created.len(), 1);
        let reports = s.tick_now(None, false, now + chrono::Duration::seconds(1)).await;
        assert_eq!(reports[0].outcome, RunOutcome::NotDue, "not 36 times");
        let st = &s.statuses(now)[0];
        assert_eq!(st.next_due, Some(now + chrono::Duration::minutes(5)));
    }

    #[tokio::test]
    async fn a_new_every_job_runs_now_but_a_new_cron_job_waits() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (mut s, tmp) = scheduler_with(&store);
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        write_job(&paths, "e", CLOCK_JOB);
        write_job(&paths, "c", &CLOCK_JOB.replace("every = \"5m\"", "cron = \"0 0 1 1 *\""));
        let now = Utc::now();
        let reports = s.tick_now(None, false, now).await;
        let of = |name: &str| reports.iter().find(|r| r.job == name).unwrap();
        assert_eq!(of("e").outcome, RunOutcome::Ran);
        assert_eq!(of("c").outcome, RunOutcome::NotDue);
        let sts = s.statuses(now);
        let c = sts.iter().find(|j| j.name == "c").unwrap();
        assert!(c.next_due.unwrap() > now);
        assert_eq!(c.schedule.as_deref(), Some("cron 0 0 1 1 *"));
    }

    #[tokio::test]
    async fn backoff_gates_a_due_job() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (mut s, tmp) = scheduler_with(&store);
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        write_job(&paths, "a", CLOCK_JOB);
        let now = Utc::now();
        store
            .save_job_state(&JobState {
                name: "a".into(),
                failures: 1,
                backoff_until: Some(now + chrono::Duration::seconds(30)),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(s.tick_now(None, false, now).await[0].outcome, RunOutcome::NotDue);
        assert_eq!(
            s.tick_now(None, false, now + chrono::Duration::seconds(31)).await[0].outcome,
            RunOutcome::Ran
        );
        assert_eq!(
            s.tick_now(Some("a"), false, now).await[0].outcome,
            RunOutcome::Ran,
            "--job forces it regardless"
        );
    }

    #[tokio::test]
    async fn unknown_job_on_tick_is_reported() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (mut s, _tmp) = scheduler_with(&store);
        let reports = s.tick_now(Some("ghost"), false, Utc::now()).await;
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].outcome, RunOutcome::Unknown);
    }

    #[tokio::test]
    async fn queued_over_an_hour_is_warned_once() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (mut s, _tmp) = scheduler_with(&store);
        let t = store
            .insert_task(crate::store::NewTask {
                job: "run".into(),
                item: Value::Null,
                prompt: "p".into(),
                spec: job("j").spec,
            })
            .unwrap();
        let now = Utc::now();
        s.warn_long_queued(now);
        assert!(s.warned_queued.is_empty(), "fresh tasks are not warned about");
        let later = now + chrono::Duration::hours(2);
        s.warn_long_queued(later);
        s.warn_long_queued(later);
        assert_eq!(s.warned_queued.len(), 1);
        assert!(s.warned_queued.contains(&t.id));
    }
```

`src/daemon.rs` `mod tests`: replace `d.views()` with `d.fleet().views()` and `d.dispatch_queued()` with `d.fleet().dispatch_queued()` in the existing tests, and add:

```rust
    /// Two passes at once (a tick and a `pastor run`) against one machine with
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
        assert_eq!(fake.agents().len(), 1, "one agent on a max_agents = 1 machine");
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
        assert!(list[0].prompt.ends_with(&format!(" {}", list[0].display_id())));
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
```

(`daemon()` in the tests returns the `TempDir`; the config it writes is in memory, so the jobs dir is created by the test. Add `use crate::scheduler::RunOutcome; use crate::store::NewTask;` to the test imports.)

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib scheduler:: daemon:: ipc::`
Expected: compile errors.

- [ ] **Step 3: Implement the IPC types**

`src/ipc.rs`:

```rust
use crate::scheduler::{JobRunReport, JobStatus};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum IpcRequest {
    Ping,
    Run { prompt: String, spec: DispatchSpec },
    List { filter: TaskFilter },
    TaskShow { id: i64 },
    TaskRead { id: i64, lines: u32 },
    FlockList,
    /// One scheduler pass now; `job` forces that job regardless of schedule.
    Tick { job: Option<String>, dry_run: bool },
    /// Re-read the jobs directory now.
    Reload,
    JobList,
    /// Fire a job now, ignoring schedule, overlap and `enabled`.
    JobRun { name: String },
}
```

and in `IpcResponse` add `Runs(Vec<JobRunReport>), Jobs(Vec<JobStatus>),` before `Error`.

- [ ] **Step 4: Implement `Fleet` and rewire `Daemon`**

`src/daemon.rs`:

```rust
use crate::scheduler::{Scheduler, SchedulerHandle};

/// The machines plus the one lock every dispatch pass takes. Shared by the
/// daemon (a `pastor run` dispatches inline) and the scheduler (each tick, and
/// after a job run queues tasks), so two passes never read the same capacity
/// snapshot and both fill the last slot.
pub struct Fleet {
    pub machines: Vec<MachineHandle>,
    store: Arc<Store>,
    dispatch_lock: tokio::sync::Mutex<()>,
}

impl Fleet {
    pub fn new(machines: Vec<MachineHandle>, store: Arc<Store>) -> Fleet {
        Fleet {
            machines,
            store,
            dispatch_lock: tokio::sync::Mutex::new(()),
        }
    }

    pub fn get(&self, name: &str) -> Option<&MachineHandle> {
        self.machines.iter().find(|h| h.name == name)
    }

    pub fn views(&self) -> Vec<MachineView> {
        self.machines
            .iter()
            .map(|m| {
                let s = m.snapshot();
                MachineView {
                    name: m.name.clone(),
                    max_agents: m.max_agents,
                    tags: m.tags.clone(),
                    live: s.live,
                    healthy: s.channel.accepts_dispatch(),
                }
            })
            .collect()
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
        for task in queued {
            let Some(name) = pick_machine(&self.views(), &task.spec) else {
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
```

`Daemon` becomes:

```rust
pub struct Daemon {
    paths: Paths,
    config: PastorConfig,
    store: Arc<Store>,
    fleet: Arc<Fleet>,
    scheduler: SchedulerHandle,
    events: broadcast::Sender<PastorEvent>,
}
```

In `start`, after the machines are spawned:

```rust
        let fleet = Arc::new(Fleet::new(machines, store.clone()));
        let scheduler = Scheduler::new(
            paths.clone(),
            &config,
            store.clone(),
            fleet.clone(),
            events.clone(),
        )
        .spawn();
        Ok(Daemon { paths, config, store, fleet, scheduler, events })
```

Add accessors `pub fn fleet(&self) -> Arc<Fleet> { self.fleet.clone() }` and `pub fn scheduler(&self) -> SchedulerHandle { self.scheduler.clone() }`. Delete `Daemon::dispatch_queued` and `Daemon::views` (moved to `Fleet`). In `run_with_listener`, delete the `let mut tick = ...` line and the `_ = tick.tick() => ...` arm; `config` stays (the field is read by `start`; if clippy flags it dead, drop the field). In `handle`: `Run` checks `self.fleet.get(m).is_none()` for `unknown_machine` and calls `self.fleet.dispatch_queued().await`; `TaskRead` uses `self.fleet.get(m)`; `FlockList` maps `self.fleet.machines`. Add the arms:

```rust
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
```

`serve()` log line: `machines = daemon.fleet.machines.len()`.

- [ ] **Step 5: Implement the `Scheduler`**

Append to the non-test part of `src/scheduler.rs` (add the imports it needs at the top: `std::collections::HashMap`, `std::path::PathBuf`, `std::sync::Arc`, `std::time::SystemTime`, `tokio::sync::{mpsc, oneshot}`, `tokio::task::JoinHandle`, `crate::config::job::{Loaded, load_dir}`, `crate::config::{Defaults, PastorConfig, Paths}`, `crate::connector`, `crate::daemon::Fleet`, `crate::schedule::Schedule`):

```rust
/// What `pastor job list` shows for one job file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobStatus {
    pub name: String,
    /// `None` when the file never parsed (nothing to describe).
    pub schedule: Option<String>,
    pub enabled: bool,
    pub connector: Option<String>,
    /// The current file's problem. With `schedule` also set, the previous
    /// good version is what runs.
    pub error: Option<String>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_result: Option<String>,
    pub next_due: Option<DateTime<Utc>>,
    pub running: bool,
}

/// A job as the scheduler holds it: the last good parse, plus the current
/// file's error if it stopped parsing. A file that never parsed has no `job`.
#[derive(Debug, Clone)]
struct Entry {
    job: Option<Job>,
    error: Option<String>,
}

enum Due {
    Now,
    At(DateTime<Utc>),
    Never,
}

pub enum SchedulerCommand {
    Tick {
        job: Option<String>,
        dry_run: bool,
        reply: oneshot::Sender<Vec<JobRunReport>>,
    },
    Fire {
        name: String,
        reply: oneshot::Sender<Result<String, String>>,
    },
    Reload {
        reply: oneshot::Sender<Vec<JobStatus>>,
    },
    JobList {
        reply: oneshot::Sender<Vec<JobStatus>>,
    },
}

#[derive(Clone)]
pub struct SchedulerHandle {
    tx: mpsc::Sender<SchedulerCommand>,
}

impl SchedulerHandle {
    async fn send<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<T>) -> SchedulerCommand,
    ) -> anyhow::Result<T> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(make(reply))
            .await
            .map_err(|_| anyhow::anyhow!("scheduler is gone"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("scheduler dropped the request"))
    }
    pub async fn tick(&self, job: Option<String>, dry_run: bool) -> anyhow::Result<Vec<JobRunReport>> {
        self.send(|reply| SchedulerCommand::Tick { job, dry_run, reply }).await
    }
    pub async fn fire(&self, name: &str) -> anyhow::Result<Result<String, String>> {
        let name = name.to_string();
        self.send(|reply| SchedulerCommand::Fire { name, reply }).await
    }
    pub async fn reload(&self) -> anyhow::Result<Vec<JobStatus>> {
        self.send(|reply| SchedulerCommand::Reload { reply }).await
    }
    pub async fn job_list(&self) -> anyhow::Result<Vec<JobStatus>> {
        self.send(|reply| SchedulerCommand::JobList { reply }).await
    }
}

type Resolver = Box<dyn Fn(&str) -> Option<Arc<dyn ItemSource>> + Send + Sync>;

pub struct Scheduler {
    paths: Paths,
    defaults: Defaults,
    tick: Duration,
    store: Arc<Store>,
    fleet: Arc<Fleet>,
    events: broadcast::Sender<PastorEvent>,
    /// Connector id -> source. `connector::builtin` outside tests.
    resolve: Resolver,
    entries: HashMap<String, Entry>,
    /// (file name, mtime, size) of every job file at the last load; `None`
    /// until the first.
    fingerprint: Option<Vec<(PathBuf, Option<SystemTime>, u64)>>,
    /// Runs in progress: a job may appear more than once only through `fire`.
    in_flight: Vec<(String, JoinHandle<JobRunReport>)>,
    /// When each job was first loaded; a cron job that never ran is due at its
    /// first occurrence after this.
    first_seen: HashMap<String, DateTime<Utc>>,
    warned_queued: HashSet<i64>,
}

impl Scheduler {
    pub fn new(
        paths: Paths,
        config: &PastorConfig,
        store: Arc<Store>,
        fleet: Arc<Fleet>,
        events: broadcast::Sender<PastorEvent>,
    ) -> Scheduler {
        Scheduler {
            paths,
            defaults: config.defaults.clone(),
            tick: config.tick_duration(),
            store,
            fleet,
            events,
            resolve: Box::new(connector::builtin),
            entries: HashMap::new(),
            fingerprint: None,
            in_flight: Vec::new(),
            first_seen: HashMap::new(),
            warned_queued: HashSet::new(),
        }
    }

    /// For the CLI when no daemon runs: no machines to dispatch to, nobody
    /// listening for events. Tasks it queues wait for the next `pastor serve`.
    pub fn standalone(paths: Paths, config: &PastorConfig, store: Arc<Store>) -> Scheduler {
        let fleet = Arc::new(Fleet::new(Vec::new(), store.clone()));
        let (events, _) = broadcast::channel(1);
        Scheduler::new(paths, config, store, fleet, events)
    }

    pub fn spawn(self) -> SchedulerHandle {
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(self.run(rx));
        SchedulerHandle { tx }
    }

    async fn run(mut self, mut rx: mpsc::Receiver<SchedulerCommand>) {
        let mut tick = tokio::time::interval(self.tick);
        loop {
            tokio::select! {
                _ = tick.tick() => self.pass(Utc::now()).await,
                cmd = rx.recv() => {
                    let Some(cmd) = cmd else { return };
                    match cmd {
                        SchedulerCommand::Tick { job, dry_run, reply } => {
                            let reports = self.tick_now(job.as_deref(), dry_run, Utc::now()).await;
                            let _ = reply.send(reports);
                        }
                        SchedulerCommand::Fire { name, reply } => {
                            self.reload();
                            let _ = reply.send(self.fire(&name, Utc::now()));
                        }
                        SchedulerCommand::Reload { reply } => {
                            self.reload();
                            let _ = reply.send(self.statuses(Utc::now()));
                        }
                        SchedulerCommand::JobList { reply } => {
                            self.reload();
                            let _ = reply.send(self.statuses(Utc::now()));
                        }
                    }
                }
            }
        }
    }

    /// Re-read the jobs directory if any file was added, removed or touched.
    /// A file that stopped parsing keeps its last good version and carries the
    /// error; a removed file removes the job. Returns whether anything was
    /// reloaded.
    pub fn reload(&mut self) -> bool {
        let dir = self.paths.jobs_dir();
        let fp = fingerprint(&dir);
        if self.fingerprint.as_ref() == Some(&fp) {
            return false;
        }
        self.fingerprint = Some(fp);
        let loaded = match load_dir(&dir, &self.defaults) {
            Ok(l) => l,
            Err(err) => {
                tracing::error!(%err, "read jobs directory");
                return true;
            }
        };
        let now = Utc::now();
        let mut next: HashMap<String, Entry> = HashMap::new();
        for l in loaded {
            let name = l.name().to_string();
            self.first_seen.entry(name.clone()).or_insert(now);
            let entry = match l {
                Loaded::Valid(job) => Entry { job: Some(job), error: None },
                Loaded::Invalid { error, .. } => {
                    let previous = self.entries.get(&name).and_then(|e| e.job.clone());
                    match &previous {
                        Some(_) => tracing::warn!(job = %name, %error, "job file invalid; previous version kept"),
                        None => tracing::warn!(job = %name, %error, "job file invalid"),
                    }
                    Entry { job: previous, error: Some(error) }
                }
            };
            next.insert(name, entry);
        }
        for gone in self.entries.keys().filter(|k| !next.contains_key(*k)) {
            tracing::info!(job = %gone, "job file removed");
        }
        self.entries = next;
        true
    }

    fn states(&self) -> HashMap<String, JobState> {
        match self.store.job_states() {
            Ok(v) => v.into_iter().map(|s| (s.name.clone(), s)).collect(),
            Err(err) => {
                tracing::error!(%err, "read job states");
                HashMap::new()
            }
        }
    }

    fn is_running(&self, name: &str) -> bool {
        self.in_flight.iter().any(|(n, _)| n == name)
    }

    fn due_of(&self, job: &Job, state: Option<&JobState>, now: DateTime<Utc>) -> Due {
        if !job.enabled {
            return Due::Never;
        }
        if let Some(until) = state.and_then(|s| s.backoff_until)
            && now < until
        {
            return Due::At(until);
        }
        let last = state.and_then(|s| s.last_run_at);
        let next = match (&job.schedule, last) {
            // A new interval job runs at once (backfill says how far back it looks).
            (Schedule::Every(_), None) => return Due::Now,
            // A new cron job waits for its first occurrence; nothing was missed.
            (Schedule::Cron(_), None) => {
                let from = self.first_seen.get(&job.name).copied().unwrap_or(now);
                job.schedule.next_after(from)
            }
            // Overdue (daemon was down, or the tick is late) is simply due: it
            // runs once and the next occurrence is computed from now.
            (_, Some(last)) => job.schedule.next_after(last),
        };
        match next {
            Some(t) if t <= now => Due::Now,
            Some(t) => Due::At(t),
            None => Due::Never,
        }
    }

    pub fn statuses(&self, now: DateTime<Utc>) -> Vec<JobStatus> {
        let states = self.states();
        let mut out: Vec<JobStatus> = self
            .entries
            .iter()
            .map(|(name, e)| {
                let state = states.get(name);
                let next_due = e.job.as_ref().and_then(|j| match self.due_of(j, state, now) {
                    Due::Now => Some(now),
                    Due::At(t) => Some(t),
                    Due::Never => None,
                });
                JobStatus {
                    name: name.clone(),
                    schedule: e.job.as_ref().map(|j| j.schedule.describe()),
                    enabled: e.job.as_ref().is_some_and(|j| j.enabled),
                    connector: e.job.as_ref().map(|j| j.connector.clone()),
                    error: e.error.clone(),
                    last_run_at: state.and_then(|s| s.last_run_at),
                    last_result: state.and_then(|s| s.last_result.clone()),
                    next_due,
                    running: self.is_running(name),
                }
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// One tick: reload, reap finished runs, start due jobs, dispatch, warn.
    pub async fn pass(&mut self, now: DateTime<Utc>) {
        self.reload();
        self.reap().await;
        let states = self.states();
        let due: Vec<Job> = self
            .entries
            .values()
            .filter_map(|e| e.job.clone())
            .filter(|j| matches!(self.due_of(j, states.get(&j.name), now), Due::Now))
            .collect();
        for job in due {
            if self.is_running(&job.name) {
                tracing::warn!(job = %job.name, "due, but the previous run is still going; skipped");
                continue;
            }
            if let Err(reason) = self.start_run(job.clone(), now) {
                tracing::warn!(job = %job.name, %reason, "run not started");
            }
        }
        self.fleet.dispatch_queued().await;
        self.warn_long_queued(now);
    }

    /// `pastor job run`: now, regardless of schedule, overlap and `enabled`.
    pub fn fire(&mut self, name: &str, now: DateTime<Utc>) -> Result<String, String> {
        let job = self
            .entries
            .get(name)
            .and_then(|e| e.job.clone())
            .ok_or_else(|| format!("no job named {name:?} (or its file has never parsed)"))?;
        if self.is_running(name) {
            tracing::info!(job = name, "fired while a previous run is still going");
        }
        self.start_run(job, now)?;
        Ok(format!("started job {name}"))
    }

    fn start_run(&mut self, job: Job, now: DateTime<Utc>) -> Result<(), String> {
        let source = (self.resolve)(&job.connector)
            .ok_or_else(|| format!("connector {:?} is not available", job.connector))?;
        let store = self.store.clone();
        let fleet = self.fleet.clone();
        let events = self.events.clone();
        let name = job.name.clone();
        let handle = tokio::spawn(async move {
            let report = run_job(&store, &job, source.as_ref(), &events, now, false).await;
            if !report.created.is_empty() {
                // Do not wait for the next tick to place what this run queued.
                fleet.dispatch_queued().await;
            }
            report
        });
        self.in_flight.push((name, handle));
        Ok(())
    }

    /// Log the reports of runs that finished since the last pass.
    async fn reap(&mut self) {
        let mut still = Vec::new();
        for (name, handle) in self.in_flight.drain(..) {
            if !handle.is_finished() {
                still.push((name, handle));
                continue;
            }
            match handle.await {
                Ok(r) => tracing::info!(
                    job = %r.job, outcome = %r.outcome, items = r.items, created = r.created.len(),
                    seen = r.skipped_seen, deferred = r.deferred, error = ?r.error, "job run finished"
                ),
                Err(err) => tracing::error!(job = %name, %err, "job run panicked"),
            }
        }
        self.in_flight = still;
    }

    /// `pastor tick`: run due jobs (or the one named, forced) inline and report.
    /// Inline so the reports are complete when this returns; a long connector
    /// holds the scheduler for that long, which is acceptable for a debugging
    /// command.
    pub async fn tick_now(
        &mut self,
        only: Option<&str>,
        dry_run: bool,
        now: DateTime<Utc>,
    ) -> Vec<JobRunReport> {
        self.reload();
        self.reap().await;
        let states = self.states();
        let mut names: Vec<&String> = self.entries.keys().collect();
        names.sort();
        let mut reports = Vec::new();
        for name in names {
            if only.is_some_and(|o| o != name) {
                continue;
            }
            let entry = &self.entries[name];
            let Some(job) = entry.job.clone() else {
                let mut r = JobRunReport::new(name, RunOutcome::Invalid);
                r.error = entry.error.clone();
                reports.push(r);
                continue;
            };
            let forced = only.is_some();
            if !forced {
                match self.due_of(&job, states.get(name), now) {
                    Due::Now => {}
                    Due::Never if !job.enabled => {
                        reports.push(JobRunReport::new(name, RunOutcome::Disabled));
                        continue;
                    }
                    _ => {
                        reports.push(JobRunReport::new(name, RunOutcome::NotDue));
                        continue;
                    }
                }
                if self.is_running(name) {
                    reports.push(JobRunReport::new(name, RunOutcome::Skipped));
                    continue;
                }
            }
            let Some(source) = (self.resolve)(&job.connector) else {
                let mut r = JobRunReport::new(name, RunOutcome::Invalid);
                r.error = Some(format!("connector {:?} is not available", job.connector));
                reports.push(r);
                continue;
            };
            reports.push(run_job(&self.store, &job, source.as_ref(), &self.events, now, dry_run).await);
        }
        if let Some(o) = only
            && !reports.iter().any(|r| r.job == o)
        {
            let mut r = JobRunReport::new(o, RunOutcome::Unknown);
            r.error = Some(format!("no job named {o:?}"));
            reports.push(r);
        }
        if !dry_run {
            self.fleet.dispatch_queued().await;
        }
        reports
    }

    fn warn_long_queued(&mut self, now: DateTime<Utc>) {
        let Ok(queued) = self.store.queued_tasks() else {
            return;
        };
        let limit = chrono::Duration::from_std(QUEUED_WARN_AFTER).expect("1h fits");
        for t in queued {
            if now - t.created_at >= limit && self.warned_queued.insert(t.id) {
                tracing::warn!(
                    task = %t.display_id(),
                    job = %t.job,
                    queued_for = %crate::cli::age(t.created_at),
                    "no machine has had capacity; still queued"
                );
            }
        }
    }

    #[cfg(test)]
    fn set_source_for_tests(&mut self, id: &str, source: Arc<dyn ItemSource>) {
        let id = id.to_string();
        let previous = std::mem::replace(&mut self.resolve, Box::new(|_| None));
        self.resolve = Box::new(move |name| {
            if name == id {
                Some(source.clone())
            } else {
                previous(name)
            }
        });
    }

    #[cfg(test)]
    fn set_jobs_for_tests(&mut self, jobs: Vec<Job>) {
        let now = Utc::now();
        self.entries = jobs
            .into_iter()
            .map(|j| {
                self.first_seen.entry(j.name.clone()).or_insert(now);
                (j.name.clone(), Entry { job: Some(j), error: None })
            })
            .collect();
        // Pretend the directory was read, so `pass` does not overwrite these.
        self.fingerprint = Some(fingerprint(&self.paths.jobs_dir()));
    }
}

/// Cheap change detection for the jobs directory: names, mtimes and sizes.
fn fingerprint(dir: &std::path::Path) -> Vec<(PathBuf, Option<SystemTime>, u64)> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let md = entry.metadata().ok();
            out.push((
                path,
                md.as_ref().and_then(|m| m.modified().ok()),
                md.map(|m| m.len()).unwrap_or(0),
            ));
        }
    }
    out.sort();
    out
}
```

Note on `warned_queued` in the test: the test reads `s.warned_queued` directly; it is a private field of the same module, which the test module can see. `set_jobs_for_tests` calls `fingerprint` on a directory that may not exist: that returns an empty vector, which is fine.

- [ ] **Step 6: Run the tests**

Run: `make check && make test-machine`
Expected: green. Timing-sensitive tests here (`overlapping_run_is_skipped`) sleep generously relative to the 300 ms hold; if the suite is flaky under load, widen the hold to 600 ms and the waits with it rather than tightening assertions.

- [ ] **Step 7: Commit**

```bash
git add src/scheduler.rs src/daemon.rs src/ipc.rs
git commit -m "feat: scheduler task with reload, overlap rule and one dispatch lock" -m "The tick moves out of the accept loop into its own task; a dispatch that waits 30s on agent readiness no longer holds every CLI request. Job runs are spawned so a slow connector cannot hold a tick either. The dispatch pass takes one lock shared with pastor run, and the actor refreshes its live count before replying, which together close the over-dispatch window. Job files reload when the directory changes, keeping the last good version of a file that stops parsing."
```

---

### Task 12: CLI: `job`, `tick`, `reload`, and the end-to-end run

**Files:**
- Modify: `src/main.rs` (`Command`, new arg structs, handlers)
- Modify: `src/cli.rs` (job and run-report rows)
- Modify: `tests/cli.rs` (`start_with_jobs`, new test)
- Modify: `contrib/completions/pastor.bash`, `contrib/completions/pastor.fish` (regenerated)

**Interfaces:**
- Produces:
  ```rust
  // src/cli.rs
  pub const JOB_HEADER: [&str; 7];   // NAME SCHEDULE ENABLED CONNECTOR LAST RUN NEXT RESULT
  pub fn job_rows(jobs: &[JobStatus]) -> Vec<Vec<String>>;
  pub const RUN_HEADER: [&str; 7];   // JOB OUTCOME ITEMS CREATED SEEN DEFERRED ERROR
  pub fn run_rows(runs: &[JobRunReport]) -> Vec<Vec<String>>;
  pub fn in_(at: DateTime<Utc>) -> String;                     // "in 4m", "now", "12s ago"
  ```
  CLI surface:
  ```
  pastor tick [--dry-run] [--job NAME] [--json]
  pastor reload
  pastor job list [--json]
  pastor job enable <name> | disable <name>
  pastor job run <name>
  ```
- Consumes: `Scheduler::standalone`, `scheduler::{JobStatus, JobRunReport, RunOutcome}`, `config::job::{job_path, set_enabled}`, `IpcRequest::{Tick, Reload, JobList, JobRun}`.

- [ ] **Step 1: Write the failing tests**

`src/cli.rs` `mod tests`:

```rust
    #[test]
    fn job_rows_show_errors_over_results_and_relative_next() {
        use crate::scheduler::JobStatus;
        let now = chrono::Utc::now();
        let ok = JobStatus {
            name: "a".into(),
            schedule: Some("every 5m".into()),
            enabled: true,
            connector: Some("clock".into()),
            error: None,
            last_run_at: Some(now - chrono::Duration::seconds(90)),
            last_result: Some("ok: 1 items, 1 tasks".into()),
            next_due: Some(now + chrono::Duration::seconds(210)),
            running: false,
        };
        let broken = JobStatus {
            name: "b".into(),
            schedule: None,
            enabled: false,
            connector: None,
            error: Some("expected `]`".into()),
            last_run_at: None,
            last_result: None,
            next_due: None,
            running: true,
        };
        let rows = job_rows(&[ok, broken]);
        assert_eq!(rows[0], vec!["a", "every 5m", "yes", "clock", "1m ago", "in 3m", "ok: 1 items, 1 tasks"]);
        assert_eq!(rows[1], vec!["b", "-", "no", "-", "never", "-", "invalid: expected `]` (running)"]);
    }

    #[test]
    fn run_rows_summarise_a_report() {
        use crate::scheduler::{JobRunReport, RunOutcome};
        let mut r = JobRunReport::new("a", RunOutcome::Ran);
        r.items = 3;
        r.created = vec!["t-1".into(), "t-2".into()];
        r.skipped_seen = 1;
        let rows = run_rows(&[r, JobRunReport::new("b", RunOutcome::NotDue)]);
        assert_eq!(rows[0], vec!["a", "ran", "3", "t-1 t-2", "1", "0", ""]);
        assert_eq!(rows[1], vec!["b", "not_due", "0", "", "0", "0", ""]);
    }

    #[test]
    fn relative_times() {
        let now = chrono::Utc::now();
        assert_eq!(in_(now + chrono::Duration::seconds(250)), "in 4m");
        assert_eq!(in_(now - chrono::Duration::seconds(12)), "12s ago");
        assert_eq!(in_(now), "now");
    }
```

`src/main.rs` `mod tests`:

```rust
    #[test]
    fn job_and_tick_commands_parse() {
        for args in [
            vec!["pastor", "tick"],
            vec!["pastor", "tick", "--dry-run", "--job", "a", "--json"],
            vec!["pastor", "reload"],
            vec!["pastor", "job", "list", "--json"],
            vec!["pastor", "job", "enable", "a"],
            vec!["pastor", "job", "disable", "a"],
            vec!["pastor", "job", "run", "a"],
        ] {
            if let Err(e) = Cli::try_parse_from(&args) {
                panic!("{args:?}: {e}");
            }
        }
        assert_eq!(
            Cli::try_parse_from(["pastor", "job", "enable"]).unwrap_err().exit_code(),
            2
        );
    }
```

`tests/cli.rs`: refactor `start()` into `start_with_jobs(jobs: &[(&str, &str)]) -> Env` that writes each `(name, text)` to `config/jobs/<name>.toml` before spawning `pastor serve`, with `start()` calling it with `&[]`. Add:

```rust
#[test]
fn clock_job_creates_tasks_end_to_end() {
    let env = start_with_jobs(&[(
        "tick",
        "every = \"1s\"\n[connector]\nuse = \"clock\"\n[dispatch]\nrepo = \"/tmp\"\nprompt = \"clock {{ item.key }} for {{ job.name }} as {{ task.id }}\"\n",
    )]);
    let deadline = Instant::now() + Duration::from_secs(10);
    let tasks: Vec<serde_json::Value> = loop {
        let out = env.cmd(&["list", "--job", "tick", "--json"]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let tasks: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
        if !tasks.is_empty() {
            break tasks;
        }
        assert!(Instant::now() < deadline, "the clock job never queued a task");
        std::thread::sleep(Duration::from_millis(100));
    };
    let t = &tasks[tasks.len() - 1];
    assert_eq!(t["job"], "tick");
    let prompt = t["prompt"].as_str().unwrap();
    assert!(prompt.starts_with("clock 20"), "{prompt}");
    assert!(prompt.contains(" for tick as t-"), "{prompt}");
    assert_eq!(t["item"]["key"], prompt.split(' ').nth(1).unwrap());

    let out = env.cmd(&["job", "list"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("tick") && text.contains("every 1s") && text.contains("ok:"), "{text}");

    let out = env.cmd(&["tick", "--dry-run", "--job", "tick", "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let runs: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(runs[0]["outcome"], "dry_run");
    assert_eq!(runs[0]["created"].as_array().unwrap().len(), 1);

    let out = env.cmd(&["job", "disable", "tick"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let file = std::fs::read_to_string(env.config.join("jobs/tick.toml")).unwrap();
    assert!(file.contains("enabled = false\n"), "{file}");
    let out = env.cmd(&["job", "list", "--json"]);
    let jobs: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(jobs[0]["enabled"], false, "disable reloads the daemon at once");

    let out = env.cmd(&["job", "run", "tick"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("started"));

    let out = env.cmd(&["job", "run", "ghost"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("job_not_found"));
}

#[test]
fn tick_without_daemon_queues_tasks_for_later() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(config.join("jobs")).unwrap();
    std::fs::write(
        config.join("jobs/tick.toml"),
        "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"p {{ task.id }}\"\n",
    )
    .unwrap();
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .output()
            .unwrap()
    };
    let out = run(&["tick", "--json"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not running"));
    let runs: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(runs[0]["outcome"], "ran");
    let out = run(&["list", "--json"]);
    let tasks: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["state"], "queued");
    assert_eq!(tasks[0]["prompt"], "p t-1");
    let out = run(&["job", "list"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("ok: 1 items, 1 tasks"));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib cli:: && cargo test --test cli`
Expected: compile errors in the unit tests; the CLI tests fail on unknown subcommands.

- [ ] **Step 3: Implement `src/cli.rs`**

```rust
use crate::scheduler::{JobRunReport, JobStatus};

/// "in 4m" for the future, "12s ago" for the past, "now" within a second.
pub fn in_(at: chrono::DateTime<Utc>) -> String {
    let delta = at - Utc::now();
    if delta.num_seconds().abs() < 1 {
        return "now".into();
    }
    if delta > chrono::Duration::zero() {
        format!("in {}", age(Utc::now() - delta))
    } else {
        format!("{} ago", age(at))
    }
}

pub const JOB_HEADER: [&str; 7] = ["NAME", "SCHEDULE", "ENABLED", "CONNECTOR", "LAST RUN", "NEXT", "RESULT"];

pub fn job_rows(jobs: &[JobStatus]) -> Vec<Vec<String>> {
    jobs.iter()
        .map(|j| {
            let mut result = match &j.error {
                Some(e) => format!("invalid: {e}"),
                None => j.last_result.clone().unwrap_or_default(),
            };
            if j.running {
                result.push_str(" (running)");
            }
            vec![
                j.name.clone(),
                j.schedule.clone().unwrap_or_else(|| "-".into()),
                if j.enabled { "yes" } else { "no" }.into(),
                j.connector.clone().unwrap_or_else(|| "-".into()),
                j.last_run_at.map(|t| format!("{} ago", age(t))).unwrap_or_else(|| "never".into()),
                j.next_due.map(in_).unwrap_or_else(|| "-".into()),
                result.trim().to_string(),
            ]
        })
        .collect()
}

pub const RUN_HEADER: [&str; 7] = ["JOB", "OUTCOME", "ITEMS", "CREATED", "SEEN", "DEFERRED", "ERROR"];

pub fn run_rows(runs: &[JobRunReport]) -> Vec<Vec<String>> {
    runs.iter()
        .map(|r| {
            vec![
                r.job.clone(),
                r.outcome.to_string(),
                r.items.to_string(),
                r.created.join(" "),
                r.skipped_seen.to_string(),
                r.deferred.to_string(),
                r.error.clone().unwrap_or_default(),
            ]
        })
        .collect()
}
```

(`age()` rounds down to whole units, so "1m ago" for 90 s and "in 3m" for 210 s, as the test expects.)

- [ ] **Step 4: Implement `src/main.rs`**

Add to `Command`:

```rust
    /// Run one scheduler pass now and report what it did
    Tick(TickArgs),
    /// Re-read the job files now instead of at the next tick
    Reload,
    /// Manage jobs (files in ~/.config/pastor/jobs/)
    Job {
        #[command(subcommand)]
        cmd: JobCmd,
    },
```

```rust
#[derive(Args)]
struct TickArgs {
    /// Run connectors and show what would be created; write nothing
    #[arg(long)]
    dry_run: bool,
    /// Only this job, and run it whether or not it is due
    #[arg(long)]
    job: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum JobCmd {
    /// Every job file: schedule, enabled, last run, next run, last result
    List {
        #[arg(long)]
        json: bool,
    },
    Enable {
        name: String,
    },
    Disable {
        name: String,
    },
    /// Fire a job now, ignoring its schedule, the overlap rule and `enabled`
    Run {
        name: String,
    },
}
```

Dispatch in `main`: `Command::Tick(a) => tick(&paths, a).await, Command::Reload => reload(&paths).await, Command::Job { cmd } => job(&paths, cmd).await,`. Handlers:

```rust
use std::sync::Arc;
use pastor::config::job::{job_path, set_enabled};
use pastor::scheduler::{JobRunReport, JobStatus, Scheduler};

fn print_runs(runs: &[JobRunReport], json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(runs)?);
    } else if runs.is_empty() {
        println!("no jobs");
    } else {
        println!(
            "{}",
            pastor::cli::table(&pastor::cli::RUN_HEADER, &pastor::cli::run_rows(runs))
        );
    }
    Ok(())
}

fn print_jobs(jobs: &[JobStatus], json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(jobs)?);
    } else if jobs.is_empty() {
        println!("no jobs");
    } else {
        println!(
            "{}",
            pastor::cli::table(&pastor::cli::JOB_HEADER, &pastor::cli::job_rows(jobs))
        );
    }
    Ok(())
}

/// Offline scheduler over the same store, for `tick` and `job list` when
/// `pastor serve` is down. Tasks it queues wait for the daemon.
fn standalone(paths: &Paths) -> anyhow::Result<Scheduler> {
    let config = PastorConfig::load(&paths.config_file())?;
    paths.ensure()?;
    let store = Arc::new(Store::open(&paths.db_file())?);
    Ok(Scheduler::standalone(paths.clone(), &config, store))
}

async fn tick(paths: &Paths, a: TickArgs) -> anyhow::Result<()> {
    let runs = if daemon_running(&paths.socket_file()).await {
        let IpcResponse::Runs(runs) = ask(
            paths,
            IpcRequest::Tick {
                job: a.job,
                dry_run: a.dry_run,
            },
        )
        .await?
        else {
            unreachable!()
        };
        runs
    } else {
        eprintln!(
            "pastor serve is not running; running the pass here (new tasks stay queued until it starts)"
        );
        let mut s = standalone(paths)?;
        s.tick_now(a.job.as_deref(), a.dry_run, chrono::Utc::now())
            .await
    };
    print_runs(&runs, a.json)
}

async fn reload(paths: &Paths) -> anyhow::Result<()> {
    let IpcResponse::Jobs(jobs) = ask(paths, IpcRequest::Reload).await? else {
        unreachable!()
    };
    print_jobs(&jobs, false)
}

async fn job(paths: &Paths, cmd: JobCmd) -> anyhow::Result<()> {
    match cmd {
        JobCmd::List { json } => {
            let jobs = if daemon_running(&paths.socket_file()).await {
                let IpcResponse::Jobs(jobs) = ask(paths, IpcRequest::JobList).await? else {
                    unreachable!()
                };
                jobs
            } else {
                eprintln!("pastor serve is not running; showing the job files and the last known state");
                let mut s = standalone(paths)?;
                s.reload();
                s.statuses(chrono::Utc::now())
            };
            print_jobs(&jobs, json)?;
        }
        JobCmd::Enable { name } => toggle(paths, &name, true).await?,
        JobCmd::Disable { name } => toggle(paths, &name, false).await?,
        JobCmd::Run { name } => {
            let IpcResponse::Text(msg) = ask(paths, IpcRequest::JobRun { name }).await? else {
                unreachable!()
            };
            println!("{msg}");
        }
    }
    Ok(())
}

async fn toggle(paths: &Paths, name: &str, enabled: bool) -> anyhow::Result<()> {
    let path = job_path(&paths.jobs_dir(), name);
    if !path.exists() {
        fail("job_not_found", &format!("no job file {}", path.display()));
    }
    set_enabled(&path, enabled)?;
    let verb = if enabled { "enabled" } else { "disabled" };
    if daemon_running(&paths.socket_file()).await {
        ask(paths, IpcRequest::Reload).await?;
        println!("{verb} {name}");
    } else {
        println!("{verb} {name}; applies when pastor serve starts");
    }
    Ok(())
}
```

- [ ] **Step 5: Regenerate completions, run everything**

Run: `make completions && make check`
Expected: green; `git status` shows both completion scripts changed.

- [ ] **Step 6: Commit**

```bash
git add src/main.rs src/cli.rs tests/cli.rs contrib/completions
git commit -m "feat: job list/enable/disable/run, tick and reload commands" -m "tick and job list go through the daemon when it is up and run against the store when it is not, the same split list already makes; the spec's line about tick refusing to run beside the daemon is superseded, it never applied to run either. enable/disable edit the file and ask a running daemon to reload so the change shows at once."
```

---
### Task 13: README, AGENTS.md, spec corrections, branch wrap-up

**Files:**
- Modify: `README.md` (Status, "How it works", "Try it", "Files")
- Modify: `AGENTS.md` (Status, parked gaps, "Where things live")
- Modify: `docs/superpowers/specs/2026-09-23-pastor-design.md` (four corrections)

- [ ] **Step 1: README**

Replace the `Status:` paragraph with:

```
Status: core and jobs. One-off tasks and scheduled jobs work end to end with
the built-in `clock` connector. Connector plugins, event hooks and systemd
setup are the next milestones; see
`docs/superpowers/specs/2026-09-23-pastor-design.md`.
```

Append to "How it works", after the paragraph on the CLI socket:

```
Jobs are one TOML file each in `~/.config/pastor/jobs/`. On every `tick` the
daemon re-reads files that changed (a file that stops parsing keeps its last
good version and shows the error in `pastor job list`), asks each due job's
connector for items, drops keys it has seen before, renders `prompt`, `repo`
and `branch` with `{{ item.* }}`, `{{ job.name }}` and `{{ task.id }}`, and
queues one task per new item up to `max_tasks_per_run`. The rest stay unseen
for the next run. A job never overlaps itself; `pastor job run <name>` fires
one regardless. `every = "5m"` or `cron = "*/5 9-18 * * 1-5"` (local time)
says when. The only connector today is `clock`, one item per run keyed by the
run time; jobs that name another connector are `invalid` until plugins ship.
A failed connector backs the job off, one minute doubling to an hour, and
keeps its cursor.

A machine whose requests answer but whose event subscription will not open is
`polling`: it still takes tasks and is reconciled every `tick`. Two dispatch
passes never run at once, and a task moves from `queued` to `starting` with a
conditional update, so a machine is never given more than `max_agents`.
```

In "Try it", after `pastor run ...`, add:

```bash
mkdir -p ~/.config/pastor/jobs
cat > ~/.config/pastor/jobs/hourly.toml <<'EOF'
every = "1h"
[connector]
use = "clock"
[dispatch]
repo = "~/work/api"
prompt = "It is {{ item.key }}. Run the test suite and fix what broke. Task {{ task.id }}."
EOF
pastor job list                      # picked up at the next tick
pastor tick --dry-run --job hourly   # what a run would create, without creating it
pastor job run hourly                # fire it now
pastor list --job hourly
pastor job disable hourly
```

In "Files", add the line `~/.config/pastor/jobs/<name>.toml  one job per file` after `flock.toml`, and extend the `pastor.toml` line to `tick, settle, reconcile_every, request_timeout, agent_ready_timeout, defaults (all optional)`.

- [ ] **Step 2: AGENTS.md**

Status becomes:

```
Plans 1 (core) and 2 (jobs and schedules) are implemented: plan 1 on
`feat/core` (PR #1), plan 2 on `feat/jobs`. Plan 2 added job files, the
schedule (`every`/`cron`), the built-in `clock` connector behind the
`ItemSource` seam, the seen-store and per-job state (schema v2), templates,
the scheduler as its own task with one dispatch lock, SQL task claims and
optimistic `update_task`, the `polling` channel state, readiness from herdr's
launch flags, and `job list|enable|disable|run`, `tick`, `reload`.

Plans 3 and 4 are not written yet:

- Plan 3: plugins and the connector protocol (process connectors behind
  `connector::ItemSource`), event hooks, the events log, `pastor events`.
- Plan 4: systemd unit, `task retry|close|prune`, cleanup of orphaned
  workspaces and agents, hot reload of `flock.toml` and `pastor.toml`.

`docs/superpowers/plans/` holds the executed plans; they are history, not
reference. Do not copy code from them.
```

Replace the "Plan 2 (scheduler and concurrency)" list with one line saying every item was folded into plan 2, and add what plan 2 left behind, observed on 2026-09-24 against the real fleet and the fake:

```
Plan 3 (events and plugins):

- (existing three items unchanged)
- `pastor tick` runs jobs inline in the scheduler task so its report is
  complete; a process connector that takes a minute holds the scheduler for
  that minute. Move to spawned runs with a reply channel when plugins land.

Plan 4 (cleanup and lifecycle):

- (existing three items unchanged)
- `flock.toml` and `pastor.toml` do not reload; a machine added with
  `flock add` needs a daemon restart. Job files do reload.
- `pastor run --worktree` without `--repo` is accepted and queued, and only
  fails at dispatch. Reject it in the CLI (clap `requires`) and in `Run`.
- `flock add --command` is greedy (`num_args = 1..`): options after it are
  taken as part of the command. Document `--` or move `--command` last.
- A `repo` that does not exist on the machine is not an error: herdr opened
  t-5's workspace at `$HOME` instead of `/home/cacarico/some/repo`. Check
  herdr's `workspace.create` behaviour and fail the task if the cwd is wrong.
- Claude Code's "trust this folder" dialog blocks every agent started in a
  folder it has not seen (t-4, t-5 on cberry). pastor cannot answer it;
  document the one-time `claude` run per repo per machine, or pass the
  trust flag if the agent has one.
```

In "Where things live", add `~/.config/pastor/jobs/<name>.toml   one job per file` and change the `pastor.db` line to `tasks, seen keys, job state (SQLite)`.

- [ ] **Step 3: Spec corrections**

In `docs/superpowers/specs/2026-09-23-pastor-design.md`:

1. Dispatch step 4 becomes: "Wait for readiness: poll `agent.list` for the agent named `t-<id>` within a 30s bound (`agent_ready_timeout`, below `request_timeout`). herdr 0.9.1 reports `launch_pending` while the agent is coming up and `interactive_ready` once it accepts input; an agent listed with neither, and not `working` or `blocked`, has exited and the task fails at once, as does one missing from the list."
2. "Files, config and systemd": in the `pastor.toml` example add `request_timeout = "60s"` and `agent_ready_timeout = "30s"`; replace "`pastor tick` and `pastor run` refuse to run while the daemon holds the store." with "`pastor tick` and `job list` go through the daemon when it runs and read the store directly when it does not, the same split as `list`; `pastor run` and `job run` need the daemon."
3. "The flock": after the channel state list add "`polling`: requests answer but `events.subscribe` will not open; the machine takes tasks and is reconciled each `tick` until a subscribe succeeds."
4. "Jobs and schedules": after "First run passes `since = now - backfill` and `cursor = null`." add "Later runs pass `since` = start of the last successful run and the last persisted cursor. `job run` also ignores `enabled`."

- [ ] **Step 4: Gate, commit, PR**

Run: `make check && make test-machine`
Expected: green.

```bash
git add README.md AGENTS.md docs/superpowers/specs/2026-09-23-pastor-design.md
git commit -m "docs: jobs in the README, plan 2 status and parked gaps in AGENTS.md, spec corrections" -m "The spec's tick-refuses-beside-the-daemon line never matched run or list; the code's split is now written down. The parked list carries what the 2026-09-24 run against the real fleet found."
git push -u origin feat/jobs
gh pr create --base feat/core --title "pastor jobs: schedules, clock connector, seen-store, scheduler task" --body-file - <<'EOF'
Plan 2 of 4: `docs/superpowers/plans/2026-09-24-pastor-jobs.md`.

- job files, `every`/`cron`, templates, the built-in `clock` connector behind `ItemSource`
- seen-store and per-job state (schema v2, migrated in place)
- scheduler as its own task; one dispatch lock; SQL task claim; optimistic `update_task`
- `polling` channel state; readiness from herdr's `launch_pending`/`interactive_ready`
- `pastor job list|enable|disable|run`, `pastor tick [--dry-run] [--job]`, `pastor reload`

Test: `make check`, `make test-machine`; fake-herdr end to end in `tests/cli.rs`.
EOF
```

(If PR #1 has merged by then, base on `main` instead.)

---

## Self-review

**Spec coverage** (sections "Jobs and schedules", "Dispatch and tasks" steps 1 and 4, "Reload", "Error handling", and the plan 2 list in AGENTS.md):

| requirement | task |
|---|---|
| one file per job, `name`, `every`/`cron` exactly one, `enabled` | 8 |
| `[connector] use` + config passthrough | 7, 8, 10 |
| `[dispatch]` fields, defaults from `pastor.toml` | 8 |
| durations for `every`; 5-field local cron | 5 |
| overdue on start runs once; missed runs not replayed | 11 (`overdue_on_start_runs_once...`) |
| a job never overlaps itself; `job run` ignores schedule and overlap | 11 |
| stable `key`; seen-store by (job, key); `max_tasks_per_run`; rest unseen; duplicate key first wins | 9, 10 |
| first run `since = now - backfill`, `cursor = null` | 10 |
| templates: `item.*`, `job.name`, `task.id`; substitution only | 6, 8, 10 |
| built-in `clock` connector | 7 |
| `pastor run` one-off unchanged | 11 (`Run` through `Fleet`) |
| `job list` (name, schedule, enabled, last run, last result), `enable|disable`, `job run` | 11, 12 |
| `pastor tick [--dry-run] [--job]` | 11, 12 |
| `pastor reload`; job edits apply next tick; bad file reported, previous kept | 11, 12 |
| connector failure: `job.failed`, cursor kept, backoff 2x to 1h, reset on success | 10 |
| queued: retried each tick oldest first, warn after 1h | 11 |
| `polling` channel state; reconcile each tick when polling | 4 |
| readiness from `launch_pending`/`interactive_ready` (parked item) | 2 |
| scheduler in its own task; serialised dispatch; SQL claim (parked) | 3, 4, 11 |
| capacity over-dispatch (parked; reproduced 2026-09-24) | 4, 11 |
| `update_task` optimistic concurrency (parked) | 3 |
| `request_timeout` / `agent_ready_timeout` config keys (parked) | 1 |
| `events.jsonl`, hooks, plugins, `.env` | plan 3, stated in the header |
| flock / pastor.toml hot reload | plan 4, stated in the header and AGENTS.md |

Gap found and fixed during review: `since` on later runs was unspecified; the plan defines it as the last successful run's start and Task 13 writes that into the spec.

**Placeholder scan:** no TBD/TODO; every step with code shows the code; Task 8 Step 4's fallback for `flatten` names a concrete alternative rather than deferring.

**Type consistency:** `Store::update_task(&mut Task)` (3) is what 4 and 9's callers use; `Store::claim_task -> Option<Task>` (3) matches 4's `run_dispatch`; `JobState` fields (9) match 10's reads and 11's `states()`; `JobRunReport`/`RunOutcome`/`JobStatus` (10, 11) match 12's rows and `ipc.rs`; `Fleet::{new, get, views, dispatch_queued}` (11) match `Scheduler` and `main.rs`; `PastorEvent { machine: Option<String>, job: Option<String> }` (4) matches 10's `job.failed`; `AgentInfo.launch_pending` (2) is set in both fake arms; `ChannelState::accepts_dispatch` (4) is used by `Fleet::views`; `Scheduler::{standalone, reload, statuses, tick_now}` (11) are what `main.rs` (12) calls; `template::{render, placeholders}` (6) are what 8 and 10 use; `Schedule::{from_fields, describe, next_after}` (5) are what 8 and 11 use.

**Review Focus:** all six lines name the task and test that pins them (1→8, 2→10, 3→11, 4→10, 5→4 and 11, 6→11).
