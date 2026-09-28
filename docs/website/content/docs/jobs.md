---
title: jobs
summary: work that finds itself on a schedule
group: use
weight: 21
manual: how-it-works
---

A job is a schedule plus a connector that finds work, and a prompt for each
piece of work it finds. Use one when the same kind of task should start on
its own, every morning or for every new issue.

## a job file

A job is one TOML file in `~/.config/pastor/jobs/`, named after the file.
This one, `morning.toml`, wakes an agent every weekday at nine:

```toml
description = "Run the test suite every weekday morning"
cron = "0 9 * * 1-5"

[connector]
use = "clock"

[dispatch]
repo = "~/work/api"
prompt = "It is {{ item.key }}. Run the test suite and fix what broke."
```

`description` is optional: one line on what the job does, for `pastor job
list --wide` and `pastor job describe`. `every = "1h"` or `cron = "..."` says when; cron is in local time. The
built-in `clock` connector hands over one item per run, keyed by the run
time. Any other `use` names an installed connector.

## dispatch

`[dispatch]` takes the same things as `pastor task run`, plus a few of its own.

| key | does |
|---|---|
| `prompt` | the prompt template for each item |
| `description` | each task's description, a template like `prompt`; default `{{ item.title }}` |
| `repo`, `worktree`, `branch` | where the agent works, as on `task run` |
| `flock`, `machine`, `tags` | which machines take the tasks |
| `agent`, `agent_args` | the agent to start |
| `allow`, `deny` | tool patterns, added to the flock's and `[defaults]` |
| `max_tasks_per_run` | at most this many tasks per run; the rest wait for the next |
| `backfill` | on the first run, also take items from this far back |

Templates can use `{{ item.* }}`, `{{ job.name }}` and `{{ task.id }}`. An
item value in `repo` or `branch` must be one plain path component, and may
not be the first part of `branch`: use a fixed prefix such as
`pastor/{{ item.key }}`, so an item cannot point the agent at `main`.

## seen items and overlap

Each item has a key. pastor drops keys it has seen before, so one issue never
becomes two tasks; a pruned task's item stays seen. A job never overlaps
itself, and a connector that fails backs it off, one minute doubling to an hour.

## manage it

```sh
pastor job list  # schedule, enabled, last and next run, last result
pastor job list --wide  # and each job's description
pastor job describe morning
pastor tick --dry-run --job morning  # what a run would create, creating nothing
pastor job run morning  # fire it now, ignoring the schedule
pastor job edit morning
pastor job disable morning
pastor job reload  # re-read the files now, not at the next tick
```

A file that stops parsing keeps its last good version, and `job list` shows
the error. `job edit` saves the file only once it is valid.

More in the [manual](../manual/#how-it-works).
