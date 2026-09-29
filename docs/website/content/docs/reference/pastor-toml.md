---
title: pastor.toml
summary: the head's settings and defaults
weight: 2
aliases:
  - /docs/config/
---
`~/.config/pastor/pastor.toml` holds the head's settings and the defaults
every task falls back to. Every key is optional; a missing file is all
defaults. Edit it with `pastor config edit`, which saves only a file that
loads, and a running head reloads it at once.

## example

```toml
# ~/.config/pastor/pastor.toml
close_done_after = "1h"
head_address = "user@desk"

[defaults]
profile = "develop"
timeout = "3h"

[agents.claude-sandbox]
kind = "claude"
env = { CLAUDE_CONFIG_DIR = "~/.claude-sandbox" }

[models.sonnet]
kind = "claude"
args = ["--model", "claude-sonnet-5"]

[profiles.ci]
extends = "develop"
allow = ["Bash(docker:*)"]
```

A duration is a whole number and one unit: `30s`, `5m`, `2h`, `1d`. An
unknown key, at the top level or in any table, is refused: the file does not
load, and the error names the key.

## top-level keys

| key | default | does |
|---|---|---|
| `tick` | `"10s"` | how often the scheduler and orchestrators run a pass; also how often a machine without an event stream is polled |
| `settle` | `"10s"` | how long a finished agent stays idle before its task is marked done |
| `reconcile_every` | `"60s"` | how often each machine's open tasks are checked against its live agents |
| `request_timeout` | `"60s"` | the bound on one herdr request, connect included; a machine that does not answer in time counts as lost |
| `agent_ready_timeout` | `"30s"` | from starting an agent to a prompt herdr accepts; must be shorter than `request_timeout` |
| `close_done_after` | `"5s"` | a done task's pane closes after this, so its machine slot frees; `"15m"` leaves time for `pastor task attach` to show its last screen; `"never"` keeps it |
| `close_failed_after` | `"5s"` | a failed or stale task's pane, or an orphan's, closes after this once its agent has stopped; the task stays failed and retryable, its worktree kept; `"never"` keeps them |
| `pull_lost_after` | `"10m"` | a pull machine that has not claimed or reported for this long counts as lost, and its starting and running tasks go stale |
| `agents_change_fleet` | `false` | `true` lets agents pastor started run, retry, send to and close tasks, run jobs, and edit machines, flocks and jobs |
| `max_orchestrators` | `1` | how many orchestrator agents run at once; each still takes a slot on the head's machine under its `max_agents` |
| `head_address` | unset | the ssh destination other machines reach the head by; agents on other machines get it as `PASTOR_HEAD` |

No duration here may be zero. `agents_change_fleet` guards against an agent
acting on its own; it is not a security boundary, since the agent runs as
your user. See [profiles and trust](../../concepts/profiles-and-trust/).

## defaults

`[defaults]` fills in what a task's run flags, its job file, its flock and
its machine leave out. Which of those layers comes first differs per key:
see the [`task run` flags](../cli/#task) and
[agents and models](../../concepts/agents-and-models/).

| key | default | does |
|---|---|---|
| `agent` | `"claude"` | the agent tasks start |
| `agent_args` | `[]` | arguments for that agent |
| `allow` | `[]` | tool patterns the agent may use without asking, like `"Bash(git:*)"` |
| `deny` | `[]` | tool patterns it must never use; wins over `allow` |
| `model` | unset | a `[models]` name; unset runs no model |
| `fallback` | unset | `[models]` names tasks may fall back to, in order; unset or `[]` is none |
| `priority` | unset | `low`, `normal`, `high` or `critical`; unset is `normal` |
| `agents` | `{}` | the agent for a model of another kind than `agent`'s, by kind: `{ opencode = "opencode" }` |
| `profile` | unset | a permission profile; unset runs none |
| `max_tasks_per_run` | `5` | at most this many tasks per job run; a job can set its own |
| `timeout` | `"2h"` | a task still running after this goes stale |
| `place` | `"repo"` | where a task's pane goes: `repo`, `own`, `pastor` or `pane:<workspace>` |
| `label` | unset | the task's workspace name template; unset is `"{{ flock }}/{{ task.id }}"` |
| `summary` | unset | `ask`, `require` or `off`; unset is `ask` |
| `keep_pane` | unset | `true` keeps a task's pane once it ends, until `pastor task close`; unset is no |

`allow` and `deny` add up across layers instead of replacing each other:
`[defaults]`, then the flock, then the task or job. A pattern denied in any
layer is dropped from `allow`.

## agents

`[agents.<name>]` defines an agent by name, such as a second Claude account
with its own config dir. A task, job, flock or machine names it with
`agent`. Every key is optional.

| key | default | does |
|---|---|---|
| `kind` | the table's name | the herdr agent kind to start, like `claude` or `codex`; the built-in trust keys and tool flags follow it |
| `env` | `{}` | environment for the task's pane; a value starting with `~/` is expanded on the machine that runs it |
| `trust_keys` | `["Down", "Enter"]` for Claude, else none | the keys that accept the folder-trust prompt; `[]` for none |
| `trust_marker` | `"Yes, I trust this folder"` for Claude, else none | text only the trust prompt shows; saved trust presses the keys only while the pane shows it |
| `allow_flag` | `"--allowedTools"` for Claude | the flag put before each `allow` pattern; an agent with none refuses tasks that carry an allow list |
| `deny_flag` | `"--disallowedTools"` for Claude | the same, for `deny` |
| `account` | none | a label for the login the agent uses: every machine whose agent names the same account shares its usage limits; unset, a limit holds only on the machine it was seen on |

## models

`[models.<name>]` names a model, which a task picks with `model`. No model
is built in, so `--model sonnet` fails until you add `[models.sonnet]`. A
name is lowercase letters, digits, `_`, `.` and `-`.

| key | default | does |
|---|---|---|
| `kind` | required | the agent kind that runs it, like `claude` |
| `args` | required, may be `[]` | put before the task's `agent_args`, like `["--model", "claude-sonnet-5"]` |

## profiles

`[profiles.<name>]` defines a permission profile beside the three built-in
ones. A Claude agent under a profile gets its lists and never asks. See
[profiles and trust](../../concepts/profiles-and-trust/).

| key | default | does |
|---|---|---|
| `description` | none | one line for `pastor profile list` |
| `extends` | none | another profile, built in or not, whose lists come first |
| `allow` | `[]` | tool patterns added to what it extends |
| `deny` | `[]` | tool patterns added too; wins over any allow |

| built in | allows | denies |
|---|---|---|
| `review` | reading, searching, and read-only git (`status`, `diff`, `log`, `show`, `blame`) | edits, `git push`, `rm -rf`, `sudo` |
| `develop` | reading, searching, edits and `Bash` | `rm -rf`, `sudo`, `git push --force` |
| `unrestricted` | every tool, the web included | nothing |

A table with a built-in name replaces that profile. A task may ask for
`unrestricted` only on a machine whose own profile is `unrestricted`.
`pastor profile describe` prints a profile's full lists.

## watch

`[[watch.connector]]` lists the connectors whose `[watch]` command
`pastor watch` runs each interval. None by default; `--connector` replaces
the list for one run.

| key | default | does |
|---|---|---|
| `name` | required | the connector's id, as `pastor connector list` shows it |

## limits

`[limits]` says how the head treats an account that ran out of usage. See
[usage limits](../../concepts/agents-and-models/#usage-limits); `pastor
limit list` shows the accounts it holds back.

| key | default | does |
|---|---|---|
| `wait_under` | `"1h"` | a limited task waits for a reset closer than this, and falls back past it; `"0s"` never waits |
| `rate_retries` | `3` | a task stopped on a 429 or 529 is sent on this many times before it counts as limited |
| `rate_backoff` | `"1m"` | the wait before the first of those; each next one doubles it |
| `unknown_reset_wait` | `"1h"` | how long a limit whose message names no reset holds |
| `retry_after_no_credit` | `"6h"` | how long a limit for no credit holds |
| `handover_lines` | `100` | the pane lines a task moving to another model hands to it |

## shepherd

`[shepherd]` is read by a headless `pastor serve`, one on a machine whose
CLI uses a head elsewhere. It matters when the head lists this machine as
`pull = true` in its [flock.toml](../flock-toml/).

| key | default | does |
|---|---|---|
| `machine` | the hostname | the name this machine has in the head's `flock.toml` |
| `takes_flock_work` | `false` | take any task the head would place here, not only those pinned to it |
| `command` | unset | developer option: an argv that speaks the herdr protocol, in place of this machine's herdr |

`PASTOR_SHEPHERD_FLOCK_WORK=1` (or `true`) in the serve's environment does
what `takes_flock_work = true` does.

## files

| path | holds |
|---|---|
| `~/.config/pastor/pastor.toml` | this page |
| `~/.config/pastor/flock.toml` | flocks and machines; see [flock.toml](../flock-toml/) |
| `~/.config/pastor/jobs/<name>.toml` | one job per file; see [job files](../job-files/) |
| `~/.config/pastor/orchestrators/<name>.toml` | one orchestrator per file, and an optional `.env` for its scripts |
| `~/.config/pastor/client.toml` | this CLI's head, from `pastor head set` |
| `~/.config/pastor/connectors/<id>/.env` | a connector's secrets and settings |
| `~/.local/state/pastor/pastor.db` | tasks, summaries, seen items, job state, saved trust |
| `~/.local/state/pastor/shepherd.db` | a headless serve's job state, seen items and pulled tasks |
| `~/.local/state/pastor/pastor.sock` | the head's socket |
| `~/.local/state/pastor/events.jsonl` | the events log; see [events](../events/) |
| `~/.local/state/pastor/serve.log`, `serve.json` | a background serve's log, and its pid, service and log path |
| `~/.local/state/pastor/watch/<name>.json` | a `pastor watch` cursor |
| `~/.local/state/pastor/orchestrators/<name>/` | an orchestrator's state, note, scratch dir and run logs |
| `~/.local/state/pastor/connectors/<job>/` | a job's connector scratch |
| `~/.local/state/pastor/runs/` | captured connector and hook output, capped and pruned |
| `~/.local/state/pastor/ssh/` | ssh ControlMaster sockets, per machine and for a remote head |
| `~/.local/share/pastor/connectors/<id>/` | installed connectors; a symlink for a linked one |

`pastor setup` writes the service files: `~/.config/systemd/user/` on
Linux, `~/Library/LaunchAgents/` on macOS.

## environment

| variable | does |
|---|---|
| `PASTOR_CONFIG_DIR` | replaces `~/.config/pastor` |
| `PASTOR_STATE_DIR` | replaces `~/.local/state/pastor` |
| `PASTOR_DATA_DIR` | replaces `~/.local/share/pastor` |
| `PASTOR_HEAD` | the head this shell's CLI uses, over `client.toml` |
| `PASTOR_CONNECTOR_GIT_BASE` | where `pastor connector install` clones from; default `https://github.com` |
