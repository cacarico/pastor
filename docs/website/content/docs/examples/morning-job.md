---
title: a job every morning
summary: a schedule that starts an agent
weight: 5
---
Some work has no trigger but the time. Every weekday at seven, an agent
looks at what the agents did since yesterday and writes a morning note in
the vault, so the day starts with a page to read instead of a pile of
terminals. The built-in `clock` connector makes one item per run, and the
schedule alone decides when the agent starts.

## what you need

- The vault on `desk`, the head. The job pins its task there.
- Nothing to install: `clock` is built in.

## the job

```toml
# ~/.config/pastor/jobs/morning.toml
description = "Write the morning note from yesterday's tasks and PRs"
cron = "0 7 * * 1-5"

[connector]
use = "clock"

[dispatch]
machine = "desk"
timeout = "30m"
prompt = """
You are pastor task {{ task.id }}. It is {{ item.key }}. Nobody is watching;
do not ask questions.

Write today's note at ~/notes/Daily/morning.md:
1. From pastor task list --all --json, the tasks since yesterday morning:
   what finished, what failed and why, what is blocked on a person.
2. From gh pr list --repo owner/app, the PRs waiting on a review.
3. At the top, the three things that need a person first.
"""
```

`cron` is the usual five fields, in the head's local time: minute 0, hour 7,
Monday to Friday. `every = "12h"` is the other way to say when. The item's
key is the run time in UTC, `2026-09-30T05:00:00Z`, and `{{ item.key }}`
puts it in the prompt. The job has no `repo`, so the agent starts in
`~/pastor-tasks` and reaches the notes by path.

## try it first

See what a run would create, then fire one now instead of waiting for
tomorrow:

```sh
pastor tick --dry-run --job morning
pastor job run morning
```

```text
JOB      OUTCOME  ITEMS  CREATED               SEEN  DEFERRED  ERROR
morning  dry_run  1      2026-09-29T16:04:12Z  0     0
```

```text
started job morning
```

`job run` ignores the schedule and `enabled`, so it also tries a job you
have disabled. A job never overlaps itself: fired while a run is going, it
waits for that one.

## what you see

```sh
pastor job list
pastor task list --job morning
```

```text
NAME     SCHEDULE          ENABLED  FLOCK    CONNECTOR        LAST RUN  NEXT    RESULT
kanban   every 1m          yes      default  obsidian-kanban  40s ago   in 20s  ok: 0 items, 0 tasks
morning  cron 0 7 * * 1-5  yes      default  clock            30s ago   in 14h  ok: 1 items, 1 tasks
```

```text
ID    STATE    PRIORITY  MACHINE  FLOCK    AGENT   MODEL  JOB      AGE  NOTE
t-24  running  normal    desk     default  claude  -      morning  25s  clock 2026-09-29T16:05:02Z
```

RESULT is the last run: how many items the connector gave and how many
tasks they became. A file with a mistake shows `invalid:` and the reason
there, and the job keeps its last good version until you fix it. A clock
task's NOTE is its item's title, the run time.

`pastor job disable morning` stops it for a holiday; `pastor job enable morning`
brings it back.

## next

- [jobs](../../concepts/jobs/) and [connectors](../../concepts/connectors/)
- [job files](../../reference/job-files/) and [cli: job](../../reference/cli/#job)
- [let your agent drive pastor](../agent-drives/): what the morning agent may read
