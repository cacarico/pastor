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
launch flags, and `job list|enable|disable|run`, `tick`, `reload`.

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
- A herdr error reply is an API error with a code, never a dead connection.
  Only EOF before a reply, spawn failure or a non-zero exit with no reply are
  transport failures, and only those make a machine `lost`.
- `remote-api-bridge` is a plain stdio forwarder to the local socket, so a
  bridge that exits with `command not found` means herdr is not on the PATH
  of a non-interactive shell on that machine.

## Conventions

- `make check` is the gate: fmt check, clippy with warnings as errors, the
  full suite. Run it before every commit. `make help` lists the rest.
- Nothing in the suite talks to a real herdr. `make smoke SESSION=s` runs the
  opt-in test against one on the same host; do it on a fleet machine before
  trusting a change to the transport or dispatch.
- Work on a branch, open a pull request, never push `main`.
- Commit messages: conventional prefix, plain subject, a body that explains
  the why. No `Co-Authored-By` or other trailers.
- Vocabulary is fixed by the spec: machine, flock, head, job, task, plugin,
  connector, agent. Agents are never renamed; hosts are not "sheep".
- Runtime CLI errors are JSON on stderr with a stable code and exit 1; clap
  usage errors stay plain text with exit 2.
- Rust edition 2024, toolchain from mise. No new runtime dependencies without
  a reason in the commit body.

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
- Adopted panes get no `agent_status` subscription until the next reconnect.
- `pastor machine status` does unbounded connect/ping/list on the CLI path.
- `pastor tick` runs jobs inline in the scheduler task so its report is
  complete; a process connector that takes a minute holds the scheduler for
  that minute. Move to spawned runs with a reply channel when plugins land.

Plan 4 (cleanup and lifecycle):

- A dispatch that fails after `agent.start` leaves a live agent that no task
  row points at; it is invisible to reconcile and to capacity accounting.
  Same for a daemon that is killed mid-dispatch before `starting` is
  persisted.
- A machine removed from the flock leaves its open tasks `running` forever.
- `apply` overwrites `finished_at` on `done -> closed`.
- `flock.toml` and `pastor.toml` do not reload; a machine added with `pastor
  machine add` needs a daemon restart. Job files do reload.
- `pastor run --worktree` without `--repo` is accepted by the CLI and queued,
  and only fails at dispatch. Reject it in the CLI (clap `requires`) and in
  `Run`.
- `machine add --command` is greedy (`num_args = 1..`): options placed after
  it are taken as part of the command. Put options before it, or add `--`.
- A repo that does not exist on the machine is not an error: herdr opened
  t-5's workspace at `$HOME` instead of the requested path. Check herdr's
  `workspace.create` behaviour and fail the task if the cwd is wrong.
- Claude Code's "trust this folder" dialog blocks every agent started in a
  folder it has not seen, on a fresh machine, until answered once per
  machine; pastor cannot answer it, so use `pastor attach` to answer it by
  hand, or document a one-time `claude` run per repo per machine.
- The Pis lack git and lingering, and herdr's server does not survive a
  reboot: start it with `herdr server`; a systemd user unit needs
  `loginctl enable-linger`.
- A hand edit of `flock.toml` leaves herdr's saved-machine list stale; only
  `machine add|remove --herdr` touches it. Candidate: `machine sync --herdr`,
  or reconciling the two lists on head start.
- `tasks.id` has no `AUTOINCREMENT`, so ids can be reused after a rollback or
  pruning; ids appear in branch names (`pastor/t-<n>`) and in
  `seen.task_id`.
- `pastor open` should detect a nested herdr and say so instead of herdr
  refusing to start.
- The whole dispatch pass runs under the fleet lock, so slow agent readiness
  delays `job list`, `tick`, `reload` and `run` too. Move readiness waits out
  of the lock.

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
~/.local/state/pastor/pastor.db   tasks, seen keys, job state (SQLite)
~/.local/state/pastor/pastor.sock daemon socket
~/.local/state/pastor/events.jsonl events log, rotated to events.jsonl.1
~/.local/state/pastor/ssh/        one ssh ControlMaster socket per machine
```

`PASTOR_CONFIG_DIR` and `PASTOR_STATE_DIR` override these; tests always set
them to temp dirs.
