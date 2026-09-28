---
title: cli
summary: every command on one page
group: reference
weight: 40
---

`pastor` is one binary with a command per area: tasks, machines, flocks, jobs
and so on. This page lists them; `--help` after any command lists its flags.

## shared behaviour

- Most read commands take `--json`: `pastor task list`, `pastor machine list`,
  `pastor job list`, `pastor events` and every `describe`. Scripts and agents
  should read ids and states from it rather than guess them.
- `--head` uses the head at an ssh destination for one command (see
  [remote head](../remote-head/)).
- A runtime error is one JSON object on stderr, `{"code": ..., "message": ...}`,
  with exit 1. A malformed command line prints usage text with exit 2.
- `pastor --skill` prints the guide for coding agents that drive pastor.

## task

| command | does |
|---|---|
| `pastor task run` | create a one-off task and dispatch it |
| `pastor task list` | live tasks across the flock; `--all` adds finished ones |
| `pastor task describe` | one task in full: state, machine, agent, prompt, error |
| `pastor task read` | recent output from a task's pane |
| `pastor task attach` | attach to a task's agent terminal (ctrl+b q detaches) |
| `pastor task send` | type text or press keys in a live task's agent |
| `pastor task retry` | re-dispatch a failed or stale task as a new task |
| `pastor task priority` | put a queued task at another level: low, normal, high or critical |
| `pastor task done` | mark a task done; an agent may end its own |
| `pastor task close` | close a task's pane, and its worktree with `--remove-worktree` |
| `pastor task prune` | delete old finished tasks; their items stay seen |

## queue

| command | does |
|---|---|
| `pastor queue` | queued tasks in the order they will start: level, where, from, waited, why not yet |
| `pastor queue move` | put a queued task `--top`, `--before`, `--after` or `--to` a place; it takes that place's level |

## machine

| command | does |
|---|---|
| `pastor machine add` | add a machine to flock.toml: over ssh, local or by a command |
| `pastor machine remove` | remove a machine; tasks on it keep their rows |
| `pastor machine move` | put a machine in another flock |
| `pastor machine list` | a line about the head, then each machine |
| `pastor machine describe` | one machine in full: versions, agents, recent errors |
| `pastor machine open` | open the full herdr UI on a machine |
| `pastor machine authorized-key` | print the line that lets a machine's agents reach this head |

## flock

| command | does |
|---|---|
| `pastor flock list` | every flock: default or not, machines, live agents, queued tasks |
| `pastor flock add` | declare a flock; `--default` makes new work go to it |
| `pastor flock remove` | remove a flock; refused while it has machines or queued tasks |
| `pastor flock default` | the flock new tasks and jobs go to |
| `pastor flock describe` | one flock in full |
| `pastor flock edit` | open flock.toml in your editor; saved only once valid |

## job

| command | does |
|---|---|
| `pastor job list` | every job file: schedule, enabled, last and next run |
| `pastor job enable` | enable a job file |
| `pastor job disable` | disable a job file |
| `pastor job run` | fire a job now, ignoring its schedule and `enabled` |
| `pastor job reload` | re-read job files, flock.toml and pastor.toml now |
| `pastor job describe` | one job in full: connector, dispatch, last runs, tasks |
| `pastor job edit` | open a job file in your editor; saved only once valid |

## connector

| command | does |
|---|---|
| `pastor connector install` | install a connector from GitHub: `owner/repo` or `owner/repo/subdir` |
| `pastor connector link` | use a connector from a local directory, in place |
| `pastor connector uninstall` | remove an installed connector; its .env and state stay |
| `pastor connector unlink` | remove a linked connector; the directory stays |
| `pastor connector list` | connectors: version, hooks, missing secrets |
| `pastor connector describe` | one connector in full |
| `pastor connector try` | run a connector once for a job and print its items |

## head and trust

| command | does |
|---|---|
| `pastor head set` | use the head on another machine, over ssh |
| `pastor head show` | print the head this CLI uses |
| `pastor head unset` | use this machine's head again |
| `pastor trust list` | every saved folder trust: machine, repo, when |
| `pastor trust remove` | forget a saved trust; the repo's next task asks again |

## service and config

| command | does |
|---|---|
| `pastor setup systemd` | install a systemd user unit for pastor, or herdr with `--herdr` |
| `pastor setup launchd` | install a launchd user agent on macOS |
| `pastor config edit` | open pastor.toml in your editor; saved only once valid |

## single commands

| command | does |
|---|---|
| `pastor serve` | run the head: scheduler, machine channels, dispatch |
| `pastor tick` | run one scheduler pass now and report it; `--dry-run` writes nothing |
| `pastor events` | show the events log |
| `pastor completions` | print a shell completion script |
| `pastor bridge` | pass request lines from stdin to this machine's head |
