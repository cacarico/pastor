# Working on pastor

Read this first, then `README.md` (how it works today) and
`docs/superpowers/specs/2026-09-23-pastor-design.md` (what it must become).
The spec is the source of truth for design; the code is the source of truth
for behaviour. When they disagree, fix one and say which.

## Status

Plan 1 of 4 (core) is implemented on `feat/core`, PR #1. It covers the flock
registry, the herdr client and transports, tasks and the SQLite store, one
actor per machine, the `pastor serve` daemon, the CLI, README, Makefile and
shell completions.

Plans 2 to 4 are not written yet:

- Plan 2: jobs and schedules (connector plugin + schedule + prompt), the
  seen-store, templates.
- Plan 3: plugins and the connector protocol, event hooks, the events log,
  `pastor events`.
- Plan 4: systemd unit, `task retry|close|prune`, cleanup of orphaned
  workspaces and agents.

`docs/superpowers/plans/2026-09-23-pastor-core.md` is the executed plan 1.
It is history: several snippets in it describe behaviour that was changed
after review (persistent request connections, prompting without a readiness
wait, `agent_not_ready` marking a task blocked, `list` hiding failed tasks).
Do not copy code from it.

## herdr facts that shaped the code

Verified against herdr 0.9.1 (protocol 22) and its source. The spec records
them too; they are repeated here because getting them wrong cost a day.

- herdr's API server answers one request per socket connection and closes.
  Only `events.subscribe` stays open. pastor opens a connection per request
  over one multiplexed ssh master (`ControlMaster=auto`, ControlPath under
  the state dir, `ControlPersist=600`) plus one long-lived events connection.
- `agent.start` returns before the agent process is up. `agent.prompt`
  answers `agent_not_ready` while it launches, and also when the agent has
  exited. pastor polls `agent.list` until the agent is ready (30s bound),
  fails fast if it vanishes, and treats `agent_blocked` as the only route to
  the `blocked` state.
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

From the whole-branch review and the first run against a real machine. None
of them blocks plan 1; each is a design decision for the plan named.

Plan 2 (scheduler and concurrency):

- `Daemon::run` awaits `dispatch_queued` inline in its `select!`, so a slow
  dispatch (up to the 30s readiness wait) holds the accept loop. The
  scheduler must run as its own task, and dispatch passes should be
  serialised or claim tasks with a conditional `UPDATE ... WHERE state =
  'queued'`.
- Capacity is a snapshot; concurrent `dispatch_queued` callers can
  over-dispatch past `max_agents`.
- `update_task` writes every column with no optimistic concurrency.
- `request_timeout` and `agent_ready_timeout` have no config keys.
- The readiness poll is `agent_status != unknown`; herdr's `AgentInfo`
  exposes `launch_pending`/`interactive_ready`, a plainer signal.
- The spec's "warn after 1h queued" and the `polling` channel state are not
  implemented.

Plan 3 (events):

- A 30s unread events socket can overrun herdr's retained history; pastor
  reconnects and reconciles, at the cost of a `machine.lost` blip.
- Adopted panes get no `agent_status` subscription until the next reconnect.
- `pastor flock status` does unbounded connect/ping/list on the CLI path.

Plan 4 (cleanup and lifecycle):

- A dispatch that fails after `agent.start` leaves a live agent that no task
  row points at; it is invisible to reconcile and to capacity accounting.
  Same for a daemon that is killed mid-dispatch before `starting` is
  persisted.
- A machine removed from the flock leaves its open tasks `running` forever.
- `apply` overwrites `finished_at` on `done -> closed`.

## Where things live

```
~/.config/pastor/pastor.toml      tick, settle, reconcile_every, defaults
~/.config/pastor/flock.toml       machines
~/.local/state/pastor/pastor.db   tasks (SQLite)
~/.local/state/pastor/pastor.sock daemon socket
~/.local/state/pastor/ssh/        one ssh ControlMaster socket per machine
```

`PASTOR_CONFIG_DIR` and `PASTOR_STATE_DIR` override these; tests always set
them to temp dirs.
