# Working on pastor

Read this first, then `README.md` (how it works today) and the design spec
on the `docs` branch, `docs/superpowers/specs/2026-09-23-pastor-design.md`
(what it must become). The spec is the source of truth for design; the code
is the source of truth for behaviour. When they disagree, fix one and say
which.

## Status

Plans 1 (core) and 2 (jobs and schedules) are implemented: plan 1 on
`feat/core` (PR #1), plan 2 on `feat/jobs`. Plan 2 added job files, the
schedule (`every`/`cron`), the built-in `clock` connector behind the
`ItemSource` seam, the seen-store and per-job state (schema v2), templates,
the scheduler as its own task with one dispatch lock, SQL task claims and
optimistic `update_task`, the `polling` channel state, readiness from herdr's
launch flags, and `task run|list`, `job list|enable|disable|run|reload`, `tick`.

Plans 3 and 4 are not written yet:

- Plan 3: plugins and the connector protocol (process connectors behind
  `connector::ItemSource`), event hooks, the events log, `pastor events`.
- Plan 4: systemd unit, `task retry|close|prune`, cleanup of orphaned
  workspaces and agents, hot reload of `flock.toml` and `pastor.toml`.

`docs/superpowers/plans/` on the `docs` branch holds the executed plans; they
are history, not reference. Do not copy code from them.

## herdr facts that shaped the code

Verified against herdr 0.9.1 (protocol 22) and its source. The spec records
them too; they are repeated here because getting them wrong cost a day.

- herdr's API server answers one request per socket connection and closes.
  Only `events.subscribe` stays open. pastor opens a connection per request
  over one multiplexed ssh master (`ControlMaster=auto`, ControlPath under
  the state dir, `ControlPersist=600`) plus one long-lived events connection.
- `agent.start` returns before the agent process is up. `agent.prompt`
  answers `agent_not_ready` while it launches, and also when the agent has
  exited. pastor polls `agent.list` until the agent is ready (30s bound)
  and fails fast if it vanishes or exits.
- An agent stuck on its own startup question (Claude's folder trust dialog)
  shows `agent_status: blocked` with `launch_pending: true`; herdr's
  `agent.prompt` checks `blocked` before `launch_pending`, so it answers
  `agent_blocked`. pastor marks the task `blocked` without prompting, and
  once the block clears the agent goes straight to active and the pending
  prompt is sent.
- There is no `completion_seq` in herdr 0.9.1. `agent.list` entries carry
  `agent_status` and `state_change_seq`, a server-wide counter stamped on an
  agent each time its detected state (idle, working, blocked, unknown)
  changes. `done` is idle after a working or blocked spell that nobody has
  looked at yet; once someone focuses the pane it reads `idle`, with no new
  sequence. Subscription events carry the status only, no sequence. So pastor
  takes the sequence from the `agent.prompt` reply as the task's baseline
  (column `last_completion_seq`, name kept for the schema) and calls a task
  done when pastor has seen the agent `working` or `blocked` since the prompt
  (event or `agent.list`) and, after `settle`, `agent.list` shows it idle or
  done at a sequence past that baseline and unchanged since it was first seen
  idle. The sequence alone is not proof of work: `unknown` bumps it too, so
  `idle -> unknown -> idle` would pass. herdr's own `agent.prompt --wait` makes
  the same two checks (`prompt_activity_statuses`, then
  `after_state_change_seq`). The activity flag lives in the machine actor's
  memory, not the store (a column would need a schema bump); after a restart
  an agent found working or blocked counts again, one found idle stays running
  until stale. Unreleased herdr adds `completion_seq` in the same sequence;
  pastor prefers it when present, with no activity needed.
- A herdr error reply is an API error with a code, never a dead connection.
  Only EOF before a reply, spawn failure or a non-zero exit with no reply are
  transport failures, and only those make a machine `lost`.
- `pane.close {pane_id}` closes the pane and its agent, and closing a
  workspace's last pane closes the workspace. `worktree.remove
  {workspace_id, force}` deletes the checkout and closes its workspace, so
  `task close --remove-worktree` calls it instead of `pane.close`, never
  after. Codes: `pane_not_found`, `dirty_worktree_requires_force`,
  `not_linked_worktree` (a plain workspace), `workspace_not_found` (an
  unknown id). `make smoke` checks them.
- `remote-api-bridge` is a plain stdio forwarder to the local socket, so a
  bridge that exits with `command not found` means herdr is not on the PATH
  of a non-interactive shell on that machine.

## Conventions

- `make check` is the gate: fmt check, clippy with warnings as errors, the
  full suite. Run it before every commit. `make help` lists the rest.
- CI (`.github/workflows/ci.yml`) runs `make check` and `make test-machine`
  on every pull request and on pushes to `main`.
- Nothing in the suite talks to a real herdr. `make smoke SESSION=s` runs the
  opt-in test against one on the same host; do it on a fleet machine before
  trusting a change to the transport or dispatch.
- Work on a branch, open a pull request, never push `main`.
- Commit messages: conventional prefix, plain subject, a body that explains
  the why. No `Co-Authored-By` or other trailers.
- `skills/pastor/SKILL.md` is built into the binary. Change it with the CLI:
  a unit test fails when it names a command or flag that does not exist.
- Vocabulary is fixed by the spec: machine, flock, head, job, task, plugin,
  connector, agent. Agents are never renamed; hosts are not "sheep".
- Runtime CLI errors are JSON on stderr with a stable code and exit 1; clap
  usage errors stay plain text with exit 2.
- Rust edition 2024, toolchain from mise. No new runtime dependencies without
  a reason in the commit body.
- Releases are tagged `vX.Y.Z` on `main` with a signed tag. `CHANGELOG.md`
  gets one section per release.

## Known gaps, parked for the next plans

From the whole-branch review and the live tour of the real fleet and the
fake, both on 2026-09-24. None of them blocks plan 1 or plan 2; each is a
design decision for the plan named.

Plan 2 (scheduler and concurrency): every item here was folded into plan 2 on
this branch — the scheduler is its own task with one dispatch lock, tasks are
claimed in SQL, `update_task` is optimistic, `request_timeout` and
`agent_ready_timeout` are config keys, readiness reads herdr's
`launch_pending`/`interactive_ready`, and the `polling` channel state exists.
What plan 2 left behind is below.

Plan 3 (events and plugins):

- A 30s unread events socket can overrun herdr's retained history; pastor
  reconnects and reconciles, at the cost of a `machine.lost` blip.
- `pastor machine list` without a head does unbounded connect/ping/list on the
  CLI path, one machine at a time.
- A stream connector starts on its job's first run, not at daemon start,
  and `pastor job reload` (which every `plugin install|link|uninstall|unlink`
  sends) rebuilds the catalog, restarting every stream connector.
- `only_own` decides ownership by reading the task's job file for
  `connector.use`; a job file edited or removed after its tasks were made
  changes who owns them.

Plan 4 (cleanup and lifecycle):

- A machine removed from the flock leaves its open tasks `running` until
  someone runs `task close`, which closes such a row without herdr.
- `flock.toml` and `pastor.toml` do not reload; a machine added with `pastor
  machine add` needs a daemon restart. Job files do reload.
- `machine add --command` is greedy (`num_args = 1..`): options placed after
  it are taken as part of the command. Put options before it, or add `--`.
- Claude Code's "trust this folder" dialog blocks every agent started in a
  folder it has not seen, on a fresh machine, until answered once per
  machine; pastor cannot answer it, so use `pastor task attach` to answer it by
  hand, or document a one-time `claude` run per repo per machine.
- The Pis lack git and lingering, and herdr's server does not survive a
  reboot: start it with `herdr server`; a systemd user unit needs
  `loginctl enable-linger`.
- A hand edit of `flock.toml` leaves herdr's saved-machine list stale; only
  `machine add|remove --herdr` touches it. Candidate: `machine sync --herdr`,
  or reconciling the two lists on head start.
- `tasks.id` has no `AUTOINCREMENT`, so an id can be reused after a rolled
  back insert of the newest task; ids appear in agent names, branch names
  (`pastor/t-<n>`) and `seen.task_id`. `task prune` never deletes the newest
  row for this reason. The real fix is a table rebuild in a later schema.
- `pastor open` should detect a nested herdr and say so instead of herdr
  refusing to start.
- The whole dispatch pass runs under the fleet lock, so slow agent readiness
  delays `job list`, `tick`, `job reload` and `task run` too. Move readiness waits out
  of the lock.
- Orphans (agents named `t-N` that no open task owns) are found only by
  reconcile, so they appear up to `reconcile_every` late, and the rule
  assumes one pastor owns the `t-N` names on each herdr. `task close t-N`
  for an orphan with no row finds it through the machines' last reconcile.
- A retry copies the rendered spec, so a job whose branch template does not
  use the task id (`pastor/{{ item.key }}`) retries onto the same branch; if
  the old worktree is still there, `worktree.create` fails. Close the old task
  with `--remove-worktree` first, or re-render from the job file on retry.
- `task close --remove-worktree` never sends `force`; a dirty checkout is an
  error until someone commits or cleans it. A `--force` would be a separate
  decision.
- `tests/transport.rs` `command_transport_talks_to_fake_herdr` failed once
  under a loaded `make check` (the stdio fake-herdr closed before replying)
  and passed on every rerun.

Not yet assigned a plan:

- Cron minutes that do not exist on a spring-forward day are skipped;
  Vixie cron runs them instead.
- Proposed: herdr owns machine identity, and flock entries reference herdr's
  saved-machine labels and carry only pastor's extra fields. A small plan of
  its own, after plan 2.

## Where things live

```
~/.config/pastor/pastor.toml      tick, settle, reconcile_every, defaults
~/.config/pastor/flock.toml       machines
~/.config/pastor/jobs/<name>.toml one job per file
~/.local/state/pastor/pastor.db   tasks (schema 3: retry_of), seen keys, job state (SQLite)
~/.local/state/pastor/pastor.sock daemon socket
~/.local/state/pastor/events.jsonl events log, rotated to events.jsonl.1
~/.local/state/pastor/ssh/        one ssh ControlMaster socket per machine
~/.config/systemd/user/*.service  from `pastor setup systemd [--herdr]`
~/.config/pastor/plugins/<id>/.env   plugin secrets and settings
~/.local/share/pastor/plugins/<id>/  plugin checkouts or links (PASTOR_DATA_DIR)
~/.local/state/pastor/plugins/<job>/ plugin scratch per job (PASTOR_PLUGIN_STATE_DIR)
~/.local/state/pastor/plugins/@<id>/ plugin scratch for hooks and runs with no job
~/.local/state/pastor/runs/<job>/    connector run logs, 256 KiB each, newest 20 kept
~/.local/state/pastor/runs/@<id>/    hook logs (and `plugin` runs with no job)
skills/pastor/SKILL.md            agent skill, in the repo; `pastor --skill` prints it
```

`PASTOR_CONFIG_DIR`, `PASTOR_STATE_DIR` and `PASTOR_DATA_DIR` override these;
tests always set them to temp dirs (`Paths::new` puts the data dir under the
state dir).

In the code: `task retry|close|prune` are `src/task_cli.rs` (CLI), the
`TaskRetry|TaskClose|TaskPrune` arms in `Daemon::handle`,
`Store::{insert_retry, close_task, prune}` and `MachineCommand::Close` in
the actor; orphan detection is `machine::orphan_agents`, used by reconcile
and by the head-less probe in `machine list`.
