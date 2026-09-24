# pastor design

Date: 2026-09-23
Status: approved in conversation, awaiting written review

pastor runs coding agents on always-on machines you own, so they can pick up
tasks from Slack, Asana or a schedule while your laptop is closed. It sits on
top of herdr: herdr owns the terminals and the agents, pastor owns the
schedule, the fleet and the bookkeeping. When you open the laptop you attach
to the panes through herdr and see what the agents did.

## Goals

- Agents take tasks without a laptop being open. Sessions live on the
  machines, never on the client.
- Recurring jobs watch external sources (Slack channel, Asana project, GitHub
  issues) or a clock, and turn new items into agent tasks.
- One place to see every task across machines, and to find the ones that
  need a human.
- Vocabulary and CLI shape match herdr so herdr users feel at home.
- Community can add sources and notifications without touching pastor.

## Non-goals

- Replacing or wrapping the herdr viewer. `herdr --remote` and herdr's saved
  machines are the UI.
- Being an agent runtime or sandbox. pastor only calls herdr.
- Provisioning hosts, installing herdr remotely, or distributing credentials.
  Explicitly a later phase.
- Desktop notifications. herdr already does those when the laptop is open.
- High availability. One head; if it is down, scheduling pauses.

## Prior art, in one paragraph

Nothing combines a fleet of owned always-on hosts, scheduled and event
triggers whose unit of work is a long-lived coding-agent session with real
lifecycle state, and one place to see and unblock that work. Closest are
Fleet (macOS, Claude only), background-agents (rented cloud sandboxes), cyrus
(single host, tracker-driven, no cron), agent-deck (TUI with remote
instances, no scheduler) and Kestra (orchestrator, no fleet or attach).
Anthropic's routines cap at one hour; owned hardware has no such cap. The
defensible core is small: fleet registry, trigger-to-`agent.start`
dispatcher, dedup, and a cross-host blocked inbox.

## Vocabulary

Reuse herdr's words where they exist. Add only what herdr lacks.

| word | meaning |
|---|---|
| machine | one host running a herdr server (herdr's word) |
| flock | the set of machines pastor dispatches to; `pastor machine add|remove|list|status` manages its members |
| head | the one machine running `pastor serve`; otherwise an ordinary machine |
| job | a recurring definition: schedule, connector, prompt, placement |
| task | one dispatched unit: an item, the machine it landed on, the agent running it |
| plugin | an installable directory that provides a connector, event hooks, or both |
| connector | the part of a plugin that turns an external source into items |
| agent | herdr's agent, never renamed |

Machines are not called sheep. In herdr's metaphor the agents are the herd,
and output that mixed sheep with agents would read wrong. The theme lives in
the name and in `flock`.

## Architecture

Language: Rust. One static binary, cross-compiled for aarch64 Pis. Likely
crates: tokio, clap, serde, toml, rusqlite (bundled), minijinja, a cron
parser, notify for config watching. Plugins are argv processes in any
language.

`pastor serve` is one long-running process on the head, started by systemd,
made of four cooperating parts sharing one SQLite store.

1. **Scheduler loop.** Wakes every `tick`, finds due jobs, runs their
   connectors, filters items through the seen-store, hands new items to the
   dispatcher. `pastor tick` runs one pass of the same code and exits, for
   debugging.
2. **Connector runner.** Spawns poll connectors per due run and keeps stream
   connectors alive with restart backoff. Connectors only print items; they
   never touch herdr.
3. **Machine channels.** herdr's API server answers exactly one request per
   connection and then closes it (`src/api/server.rs`), so a channel is not
   one long-lived stream. Correction to the original design: pastor opens a
   connection per request — an `ssh` running `herdr --session <s>
   remote-api-bridge`, which pipes stdio to that machine's herdr socket —
   and keeps one long-lived connection for `events.subscribe`, the one
   method herdr does hold open. The per-request connections are cheap because
   every `ssh` shares one `ControlMaster` per machine
   (`ControlPath = <state_dir>/ssh/<machine>-%C`, where ssh's `%C` hashes the
   destination so a retargeted machine cannot reuse the old host's master;
   `ControlPersist=600`, so a master outlives `pastor serve` by up to ten
   minutes and `ssh -O exit` ends one by hand),
   so only the first pays a handshake. pastor speaks herdr's
   newline-delimited JSON protocol for every call: `workspace.create`,
   `worktree.create`, `agent.start`, `agent.prompt`, `agent.list`,
   `agent.read`, `events.subscribe`. A `local = true` machine connects to the
   socket path directly, once per request, the same way. When a request fails
   below the API, or the event stream ends: reconnect with backoff, poll that
   machine each tick until it returns. pastor does not use `herdr --machine`
   and does not depend on the head's herdr client catalog.
4. **Dispatcher.** Picks a machine, creates a place, starts the agent, sends
   the prompt, records the task. State after that comes from events.

Stopping `pastor serve` stops scheduling and dispatch only. Agents keep
running because herdr owns them.

herdr facts this design relies on, verified against herdr 0.9.1 source:
`remote-api-bridge` exists as an internal subcommand and is what herdr's own
client uses, one process per request; the API server serves one request per
connection, and herdr's own client (`src/api/client.rs`, `request_value`)
connects per request for the same reason; the socket API has `events.subscribe` with
`pane.agent_status_changed` and `pane.closed`; agent states are `idle`,
`working`, `blocked`, `done`, `unknown`; `completion_seq` marks real
completed work; agent names must match `[a-z][a-z0-9_-]{0,31}`; `agent.start`
needs an existing idle shell pane; `agent.prompt` on a blocked agent returns
`agent_blocked` without sending input. Saved machines and `--machine` arrived
in herdr 0.9.0, so every machine needs herdr 0.9 or newer.

## The flock

pastor owns its registry in `~/.config/pastor/flock.toml`:

```toml
[[machine]]
name = "pi-1"
local = true
max_agents = 2

[[machine]]
name = "pi-3"
ssh = "fleet@pi-3"        # any ssh target or ssh_config host
session = "default"       # herdr session on that machine
max_agents = 3
tags = ["fast"]
```

- `name` is pastor's handle and must be unique. `ssh` and `local` are
  mutually exclusive; exactly one is required.
- `session` defaults to `default`. Tasks land in the machine's default
  session on purpose, so `herdr --remote pi-3` shows them next to hand-started
  work.
- `max_agents` caps concurrent pastor tasks on the machine. `tags` are free
  strings that jobs can require.
- herdr's own saved-machine catalog is a viewer concern. `pastor machine add`
  prints the matching `herdr machine add` command as a hint and does nothing
  with it.

Each machine needs herdr 0.9+, its server always running (a systemd user
service with lingering, unit shipped by pastor), and SSH access from the
head. Nothing else about the host is pastor's business.

Commands:

```
pastor machine add <name> <ssh-target> [--session S] [--max-agents N] [--tag T]...
pastor machine add <name> --local [--max-agents N] [--tag T]...
pastor machine remove <name>
pastor machine list        name, target, enabled, channel state, tasks live/max
pastor machine status [name]   fresh check: ssh, herdr version, server up, protocol
```

Channel state is one of `connected`, `reconnecting`, `polling`,
`incompatible`, `server down`. `polling`: requests answer but
`events.subscribe` will not open; the machine takes tasks and is reconciled
each `tick` until a subscribe succeeds.

## Jobs and schedules

One file per job in `~/.config/pastor/jobs/<name>.toml`.

```toml
name = "support-slack"
every = "5m"                         # or cron = "*/5 9-18 * * 1-5"
enabled = true

[connector]
use = "slack"                        # plugin id
channel = "C0123ABC"                 # rest is passed to the connector as config

[dispatch]
agent = "claude"                     # herdr agent kind
agent_args = []                      # passed after `--` to the agent
repo = "~/work/support"              # cwd on the machine
worktree = true                      # worktree per task on `branch`
branch = "pastor/{{ item.key }}"
tags = ["fast"]                      # or machine = "pi-3"
timeout = "2h"
max_tasks_per_run = 5
backfill = "0s"                      # first run: how far back `since` points
prompt = """
New message in #support from {{ item.author }}:

{{ item.text }}

Investigate, fix if it is a bug, and write your answer to REPLY.md.
"""
```

- `every` takes durations (`30s`, `5m`, `1h`). `cron` takes 5-field cron in
  the head's local time. Exactly one is required.
- Overdue on daemon start runs once. Missed runs are not replayed.
- A job never overlaps itself. If its connector is still running when the
  next run is due, that run is skipped and logged.
- Items carry a stable `key`. The seen-store is keyed by (job, key). Seen
  keys are dropped. A run creates at most `max_tasks_per_run` tasks; the rest
  are logged and remain unseen so the next run picks them up.
- First run passes `since = now - backfill` and `cursor = null`. Later runs
  pass `since` = start of the last successful run and the last persisted
  cursor. `job run` also ignores `enabled`.
- Templates use `{{ item.* }}`, `{{ job.name }}`, `{{ task.id }}` in
  `prompt`, `branch` and `repo`. Substitution only, no logic.
- `connector.use = "clock"` is built in and emits one item per run with
  `key` set to the run time, for schedule-only jobs.
- `pastor task run "<prompt>" [--repo P] [--machine M] [--agent K]
  [--worktree]` creates a one-off task with no job file. `pastor run` stays as
  a compatibility alias.

Commands:

```
pastor job list                     name, schedule, enabled, last run, last result
pastor job enable|disable <name>
pastor job run <name>               fire now, ignore schedule and overlap rule
pastor task run "<prompt>" [...]    one-off task
```

## Plugins

A plugin is a directory with `pastor-plugin.toml` and commands. It may
provide a connector, event hooks, or both. The format mirrors herdr's
plugin manifest: TOML manifest, argv commands, context by env, data by JSON.

Layout:

```
~/.local/share/pastor/plugins/<id>/     managed checkout
~/.config/pastor/plugins/<id>/.env      secrets and settings, user-edited
~/.local/state/pastor/plugins/<job>/    per-job cursor and scratch, pastor-owned
```

Manifest:

```toml
id = "slack"
name = "Slack"
version = "0.1.0"
min_pastor_version = "0.1.0"
description = "Watch a channel, report back in thread, DM on blocked"

[connector]
mode = "poll"                        # poll | stream
command = ["bash", "poll.sh"]
timeout = "60s"

[connector.config.channel]
required = true
description = "Channel ID to watch"

[secrets.SLACK_BOT_TOKEN]
description = "Bot token with channels:history and chat:write"

[[events]]
on = ["task.done", "task.blocked", "task.failed"]
only_own = true                      # only tasks whose item came from this plugin
command = ["bash", "report.sh"]

[[events]]
on = ["task.blocked", "machine.lost"]
command = ["bash", "dm-me.sh"]
```

`config` and `secrets` are declarations: pastor validates a job's connector
table against `config` at load time and reports missing secrets in
`plugin list`. There is no schema language beyond `required` and
`description`.

### Connector invocation

pastor runs the command with the plugin directory as cwd, the `.env` loaded
into its environment, and `PASTOR_PLUGIN_ID`, `PASTOR_JOB`,
`PASTOR_CONFIG_DIR`, `PASTOR_STATE_DIR` set. stdin receives one JSON object:

```json
{"config": {"channel": "C0123ABC"}, "cursor": "1727000000.000100", "since": "2026-09-23T09:00:00Z"}
```

stdout is JSON lines:

```json
{"type":"item","key":"1727000123.000200","title":"login broken on safari","body":"...","url":"https://...","author":"ana"}
{"type":"log","level":"info","message":"fetched 12 messages"}
{"type":"cursor","value":"1727000123.000200"}
```

- `key` is required and must be stable for the same source object.
  `title`, `body`, `url` are conventions. Other fields are allowed and
  reachable in templates.
- Exit 0: cursor persisted. Non-zero or timeout: cursor not advanced,
  stderr captured, `job.failed` emitted, backoff applied.
- Malformed lines are skipped and logged; the rest of the output is used.
- Stream mode: started once at daemon start, kept alive with backoff, same
  handshake, items and cursor lines accepted at any time. The connector owns
  its own port or socket; pastor proxies nothing.

### Event hooks

Each matching `[[events]]` hook runs on the head with the event JSON on
stdin and the same env as a connector. Hooks run concurrently across
plugins, sequentially within one, with a timeout (default 60s). A failed
hook is logged and never retried. `only_own = true` limits task events to
tasks whose item came from this plugin's connector.

Commands:

```
pastor plugin install owner/repo[/subdir] [--ref REF] [--yes]
pastor plugin link <path>
pastor plugin uninstall|unlink <id>
pastor plugin list                          id, version, connector?, hooks, missing secrets
pastor plugin run <id> --job NAME [--since 1h]   run connector once, print items, dispatch nothing
```

First-party plugins live in `plugins/` in the pastor repo as ordinary
installable plugins and serve as the reference examples: `slack`, `asana`,
`github-issues`, `ntfy`.

## Dispatch and tasks

A task row holds: id, job (or `run` for one-offs), item JSON, machine,
workspace id, pane id, agent name, state, created/started/finished
timestamps, error. Ids are `t-<n>` and double as the herdr agent name.

States:

| state | meaning |
|---|---|
| queued | accepted, no machine had capacity |
| starting | herdr calls in flight |
| running | agent `working` |
| blocked | agent `blocked`, needs a human |
| done | `completion_seq` advanced and the agent stayed idle for `settle` |
| stale | `timeout` passed without done; agent left running |
| failed | dispatch failed, or the agent process exited |
| closed | pane gone, by you or by `task close` |

`done` means the agent stopped and is waiting, not that the work is good.
The pane stays open.

Dispatch steps, each its own request on its own connection:

1. Pick a machine: pinned `machine` wins; else filter by `tags`, drop
   machines at `max_agents`, with a down channel, or `incompatible`; take the
   one with the fewest live tasks. None: stay `queued`, retry each tick,
   oldest first, warn after 1h.
2. Make a place: `worktree.create` (repo, branch, label = task id) when
   `worktree = true`, else `workspace.create` with cwd = repo. Use the
   returned root pane. One workspace per task.
3. `agent.start` with name = task id, kind, pane, `agent_args`. Correction to
   the original design: herdr's `agent.start` returns as soon as it has
   launched the agent, before the agent is up, and it never answers
   `agent_not_ready` (its errors are about the name, the kind and the pane).
4. Wait for readiness: poll `agent.list` for the agent named `t-<id>` within a
   30s bound (`agent_ready_timeout`, below `request_timeout`). herdr 0.9.1
   reports `launch_pending` while the agent is coming up and
   `interactive_ready` once it accepts input; an agent listed with neither,
   and not `working` or `blocked`, has exited and the task fails at once, as
   does one missing from the list.
5. `agent.prompt` with the rendered prompt, no wait. herdr answers
   `agent_not_ready` both while the agent is still launching and once the agent
   is no longer the pane's foreground process; the two are told apart by
   `agent.list`. Still launching: keep waiting inside the same bound. Gone from
   `agent.list`: the agent exited (usually it is not installed on that machine)
   and the task fails now. Bound elapsed: the task fails, and the message says
   a live agent may still be sitting on that machine. `agent_blocked` marks the
   task `blocked`, not failed, and records the prompt as pending: herdr
   rejected it without sending input. When the agent next reports a status
   other than `blocked` or `unknown` (an event, or reconcile), the machine
   sends the prompt and marks the task `running`; `agent_blocked` or
   `agent_not_ready` there leaves it pending, any other API error fails the
   task.
6. Record ids, mark `running`.

A failing step marks the task `failed` with herdr's error code and message.
Created things are left in place. No automatic dispatch retry.

Tracking: each machine holds one long-lived connection subscribed to
`pane.agent_status_changed` and `pane.closed`; events match tasks by pane id. On reconnect, and each tick
for a polling machine, `agent.list` reconciles. Pane gone means `closed`.
`timeout` elapsed means `stale`, nothing is killed.

Commands:

```
pastor task list [--job NAME] [--machine M] [--blocked|--done|--all]   hides closed by default
pastor task show t-123
pastor task retry t-123                     re-dispatch a failed or stale task
pastor task close t-123 [--remove-worktree]
pastor task prune --done --older-than 3d
pastor task attach t-123                    ssh -t <machine> herdr --session <s> agent attach t-123
pastor open <machine>                       herdr --remote <ssh-target> [--session S]
```

`pastor list` and `pastor attach` stay as compatibility aliases for the nested
task commands.

pastor never closes panes or removes worktrees on its own.

From the laptop in this version, the pastor CLI is reached over SSH:
`ssh pi-1 pastor task list` and `ssh -t pi-1 pastor task attach t-123` (the
head then hops to the task's machine). The top-level aliases also work.
`pastor open` reads `flock.toml` locally, so on the laptop it needs a copy of
that file or you use `herdr --remote` directly. A `--host` flag that does the
ssh-exec for you is listed under Later.

## Events and notifications

Events emitted by the daemon, each a JSON object with timestamp, type and
the task, job or machine record:

| event | when |
|---|---|
| task.queued, task.running | dispatch progress: queued when the row is inserted, running when the agent is up (no `task.started`: the launch moment is not persisted, so there is no record to attach) |
| task.blocked | agent needs a human |
| task.done | completed work, after settle |
| task.stale, task.failed, task.closed | as defined above |
| job.failed | connector non-zero or timeout |
| machine.lost, machine.connected | channel dropped past backoff, or returned |

Noise control: `task.done` only when `completion_seq` advances and the agent
stays idle for `settle` (default 10s). `task.blocked` once per blocked
episode. `machine.lost` only after the first reconnect attempt fails.

Destinations:

1. `~/.local/state/pastor/events.jsonl`, rotated by size, read by
   `pastor events [--follow] [--task t-123]`.
2. Plugin event hooks.
3. Nothing else built in. `ntfy` and the Slack DM hook cover phone and
   laptop.

On the laptop, herdr's own badge, toast and sound cover blocked agents when
its saved machines are connected.

`pastor task list --blocked` shows task, machine, time blocked, and the last lines
of the pane fetched with `agent.read` on demand.

## Files, config and systemd

Config in `~/.config/pastor/`: `pastor.toml`, `flock.toml`, `jobs/`,
`plugins/<id>/.env`.

```toml
# pastor.toml, every key optional
tick = "10s"
settle = "10s"
request_timeout = "60s"
agent_ready_timeout = "30s"
[defaults]
agent = "claude"
max_tasks_per_run = 5
timeout = "2h"
```

State in `~/.local/state/pastor/`: `pastor.db` (SQLite: tasks, seen items,
cursors, job last-run, machine last-seen), `events.jsonl`,
`runs/<job>/<ts>.log` (captured connector and hook output, capped and
pruned), `pastor.sock`. Managed plugin checkouts in
`~/.local/share/pastor/plugins/`.

CLI to daemon: newline-delimited JSON over `pastor.sock`. Responses are
adjacently tagged (`{"kind": ..., "data": ...}`), not internally tagged,
because serde_json can't serialize an internally tagged newtype variant that
holds a sequence or a string. When the daemon is down, read-only commands
fall back to the database and say so. `pastor tick` and `job list` go
through the daemon when it runs and read the store directly when it does
not, the same split as `task list`; `task run` and `job run` need the daemon.

Reload: the daemon watches the config directory. Job and flock edits apply on
the next tick; a file that fails to parse is reported and the previous
version kept. `pastor job reload` forces it. Plugins change only through
`plugin install|link|uninstall|unlink`.

systemd, user units shipped in `contrib/systemd/`, installed to
`~/.config/systemd/user/` without sudo:

```
pastor setup systemd [--enable] [--start|--stop] [--now]
pastor setup systemd --herdr [--enable] [--start|--stop] [--now]
```

No action flag means `enable --now`, the same as the original `enable + start`
contract. `--enable`, `--start`, `--enable --start`, `--enable --now` and
`--stop` map to the matching `systemctl --user` actions. Before writing a unit
or running `systemctl`, pastor prints the target unit, install directory,
`ExecStart` and action, and continues only when stdin answers exactly `yes`.

Both use `Restart=on-failure` and `After=network-online.target`, log to
stdout for journald, and need `loginctl enable-linger`, which pastor checks
and prints the command for.

SSH from a service has no login session. Supported: a passphrase-less key
restricted to the fleet user, or Tailscale SSH. pastor uses the `ssh` binary
from PATH and honours `~/.ssh/config`.

Permissions: config and state dirs 0700, socket and `.env` 0600. Secrets
only in `.env`; pastor redacts their values from logs by name.

Versioning: database schema version with forward migrations on start;
socket protocol version checked by the CLI.

## Error handling

| failure | behaviour |
|---|---|
| job file won't parse | logged, previous kept, `invalid` in `job list` |
| connector non-zero or timeout | `job.failed`, cursor kept, backoff 2x up to 1h, reset on success |
| connector bad JSON line | skipped and logged |
| duplicate key in one run | first wins |
| no machine with capacity | `queued`, retried each tick, warn after 1h |
| herdr call fails in dispatch | `failed` with herdr code, partial state kept, no auto retry |
| machine channel drops | `machine.lost` after backoff, polling fallback, reconcile on return |
| herdr protocol unsupported | machine `incompatible`, no dispatch, version in `machine status` |
| hook fails | logged, never retried |
| daemon crashes | systemd restarts; reconcile open tasks on start |
| head reboots | same; overdue jobs run once |
| database locked or corrupt | refuse to start with path and error; overwrite nothing |

Commitments: pastor never destroys anything on error. Retries happen only
where the source of truth is external and cheap to re-read (connectors,
channels). Dispatch retries only on command.

CLI errors: runtime errors are JSON on stderr with a stable `code` and exit 1. Usage errors keep clap's plain text and exit 2, as herdr's own CLI does.

## Testing

- Unit tests per module, no network: schedule maths, dedup and seen-store,
  templates, task state transitions from herdr event sequences including
  flapping, machine selection, config parse and reload, manifest validation.
- A fake herdr: in-process, speaks the socket protocol over a pipe, answers
  the methods pastor uses, scripted to emit status sequences, return
  `agent_not_ready` or `agent_blocked`, or drop the connection. Its shapes are
  validated against herdr's published JSON schema, vendored from a pinned
  herdr version and refreshed by a test.
- Connector and hook protocol tests with real child processes from a shell
  fixture that emits items, a cursor, a bad line, and fails on demand. Same
  for `.env` loading and redaction.
- One opt-in integration test against a real `herdr server` in a named test
  session on the developer machine, exercising `pastor run` on a `local`
  machine. It needs a real agent kind installed locally, named by an env
  var, and checks pane creation, agent name and the state transitions.
- A manual checklist for the fleet path: `machine add`, `machine status`,
  `pastor run` to a remote machine, close the laptop, `pastor attach` from a
  phone, blocked notification arrives.

## CLI summary

```
pastor serve
pastor tick [--dry-run] [--job NAME]
pastor job reload
pastor task run "<prompt>" [--repo P] [--machine M] [--agent K] [--worktree]
pastor task list [--job NAME] [--machine M] [--blocked|--done|--all]
pastor task attach <task>
pastor open <machine>
pastor task show|retry|close|prune ...
pastor machine add|remove|list|status ...
pastor job list|enable|disable|run ...
pastor plugin install|link|uninstall|unlink|list|run ...
pastor events [--follow] [--task T]
pastor setup systemd [--herdr] [--enable] [--start|--stop] [--now]
```

`pastor run`, `pastor list` and `pastor attach` are compatibility aliases for
the nested task commands.

## Later

- Provisioning: install herdr on a machine, copy credentials agents need.
- Fleet-wide `.env` distribution for plugins that must run off the head.
- Running connectors on machines other than the head.
- A `--host` flag so the laptop's pastor CLI can ssh-exec the head's.
