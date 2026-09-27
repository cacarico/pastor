---
title: config files
summary: what pastor reads and writes, and where
group: reference
weight: 41
manual: files
---

pastor keeps its settings in TOML files under `~/.config/pastor/` and its
runtime state under `~/.local/state/pastor/`. Read this to know which file
to edit, or what to back up.

## config

| path | holds |
|---|---|
| `~/.config/pastor/pastor.toml` | the head's settings and the task defaults; every key optional |
| `~/.config/pastor/flock.toml` | flocks and machines |
| `~/.config/pastor/jobs/<name>.toml` | one job per file |
| `~/.config/pastor/client.toml` | this CLI's `[head]`, from `pastor head set` |
| `~/.config/pastor/connectors/<id>/.env` | a connector's secrets and settings |

Edit them with `pastor config edit`, `pastor flock edit` and
`pastor job edit`. Each checks the file before it saves it, and a running
head reloads it at once.

## state and data

| path | holds |
|---|---|
| `~/.local/state/pastor/pastor.db` | tasks, seen items, job state, trusted repos |
| `~/.local/state/pastor/pastor.sock` | the head's socket |
| `~/.local/state/pastor/events.jsonl` | the events log, and `events.jsonl.1` before it |
| `~/.local/state/pastor/ssh/` | ssh ControlMaster sockets, per machine and for a remote head |
| `~/.local/state/pastor/runs/` | captured connector and hook output, capped and pruned |
| `~/.local/share/pastor/connectors/<id>/` | installed connectors; a symlink for a linked one |

`pastor setup` writes the service files: `~/.config/systemd/user/` on Linux,
`~/Library/LaunchAgents/` on macOS.

## environment

| variable | does |
|---|---|
| `PASTOR_CONFIG_DIR` | overrides `~/.config/pastor` |
| `PASTOR_STATE_DIR` | overrides `~/.local/state/pastor` |
| `PASTOR_DATA_DIR` | overrides `~/.local/share/pastor` |
| `PASTOR_HEAD` | the head to use for this shell, over `client.toml` |
| `PASTOR_CONNECTOR_GIT_BASE` | where `pastor connector install` clones from; default `https://github.com` |

## pastor.toml

These are the defaults. Leave a key out to keep its default.

```toml
tick = "10s"                 # scheduler pass
settle = "10s"               # a finished agent stays idle this long before its task is done
reconcile_every = "60s"
request_timeout = "60s"      # one herdr request, connect included
agent_ready_timeout = "30s"  # agent start to an accepted prompt
close_done_after = "15m"     # a done task's pane closes after this; "never" keeps it
agents_change_fleet = false  # true lets agents pastor started run tasks and edit the fleet

[defaults]                   # for run flags, job keys and flock keys left out
agent = "claude"
agent_args = []
allow = []                   # tool patterns the agent may use unasked
deny = []                    # tool patterns it must never use; wins over allow
max_tasks_per_run = 5
timeout = "2h"
place = "repo"               # where a task's pane goes: repo, own, pastor or pane:<workspace>
```

`[agents.<name>]` tables define agents by name, such as a second Claude
account with its own `env`.

More in the [manual](../manual/#files).
