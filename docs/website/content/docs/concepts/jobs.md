---
title: jobs
summary: work that finds itself on a schedule
weight: 6
aliases:
  - /docs/jobs/
---
A job is a schedule, a [connector](../connectors/) that finds work, and a
prompt template for each piece of work it finds. Use one when the same kind
of task should start on its own: every weekday morning, or for every new
issue with a label.

## a job file

A job is one TOML file in `~/.config/pastor/jobs/`, named after the file.
This one wakes an agent every weekday at nine:

```toml
# ~/.config/pastor/jobs/morning.toml
description = "Run the test suite every weekday morning"
cron = "0 9 * * 1-5"  # local time; or every = "1h"

[connector]
use = "clock"

[dispatch]
repo = "~/src/app"
prompt = "It is {{ item.key }}. Run the test suite and fix what broke."
```

`every` or `cron` says when, exactly one of them. `[connector]` names the
connector and carries its own keys. The built-in `clock` hands over one item
per run, keyed by the run time, so the schedule alone decides when an agent
starts. `[dispatch]` says what each item becomes.

A new `every` job runs at once; a new `cron` job waits for its first time.
Runs missed while the head was down are not caught up: an overdue job runs
once, and its schedule goes on from there.

## dispatch

`[dispatch]` takes what `pastor task run` takes, as keys: `repo`,
`worktree`, `branch`, `flock`, `machine`, `tags`, `agent`, `model`,
`profile`, `priority`, `timeout` and more. Its own keys include:

| key | does |
|---|---|
| `prompt` | the prompt template for each item; required |
| `description` | each task's description; default `{{ item.title }}` |
| `max_tasks_per_run` | at most this many tasks per run (default 5); the rest wait for the next run |
| `backfill` | on the first run, also take items from this far back |

Templates use `{{ item.* }}`, any field the connector sends, and
`{{ job.name }}`. `prompt`, `repo` and `branch` may also use
`{{ task.id }}`. An item value put in `repo` or `branch` must be one plain
path component, and may not be the first part of `branch`. Use a fixed
prefix such as `pastor/{{ item.key }}`, so an item can never point an agent
at `main`.

## seen items

Each item has a key. pastor drops keys it has seen before, so one issue
never becomes two tasks. A pruned task's item stays seen. An item past
`max_tasks_per_run` stays unseen, and the next run takes it.

## overlap and backoff

A job never overlaps itself. `pastor job run` fires one now, ignoring the
schedule, and it starts once a run already going has finished. A connector
that fails backs its job off, one minute doubling to an hour, and keeps its
cursor, so nothing is lost while it is down.

## where it runs

The connector runs on the machine whose `jobs/` holds the file: usually the
head. `[dispatch] machine` picks only where the agent runs. A job that must
read files on another machine lives in that machine's `jobs/`, under a
headless `pastor serve` (see [remote](../../deploy/remote/)).

## manage it

```sh
pastor job list                        # schedule, enabled, last and next run, last result
pastor job describe morning            # settings, last runs, recent tasks
pastor tick --dry-run --job morning    # what a run would create, creating nothing
pastor job run morning                 # fire it now
pastor job edit morning                # saved only once valid
pastor job disable morning
pastor job reload                      # re-read the files now, not at the next tick
```

A file that stops parsing keeps its last good version, and `job list`
shows the error. A job whose connector is missing, or lacks a required key,
shows as invalid.

Read on: [a job every morning](../../examples/morning-job/) and
[board cards](../../examples/board-cards/) are whole jobs to copy; every
key is in [job files](../../reference/job-files/), and the commands in the
[cli reference](../../reference/cli/#job).
