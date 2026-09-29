---
title: queue and priority
summary: who runs next when machines are full
weight: 5
---
When no machine has room for a task, it waits in the queue. Each dispatch
pass walks the queue in order and starts every task that fits somewhere.
The queue is how you see what waits, why, and change what goes first.

## levels

Each task has a level: `low`, `normal`, `high` or `critical`. The queue is
ordered by level, highest first, then by position within the level, then
oldest first. A task that fits nowhere (its machines are full, or none has
its tags) is skipped, and the pass goes on. So a lower task can still start
on a machine where a higher one cannot.

A task gets its level from the first of: `--priority` on `task run` or the
job's `priority`, its flock, the machine it is pinned to, and `[defaults]`
in `pastor.toml`. With none it is `normal`.

```sh
pastor task run "Look at the failing deploy" --repo '~/src/app' --priority high
```

A queued task that has waited `age_after` (30 minutes unless its flock or
`[defaults]` in `pastor.toml` says otherwise; `never` turns it off) goes up
one level, and again after each further wait, so a `low` task behind a
steady stream of `normal` and `high` tasks still runs in the end. Ageing
stops at `high`: it never makes a task `critical`, so a steady stream of
`critical` tasks still goes first, and a task can wait behind it for as long
as it keeps coming; the WAITED column is how you notice. `pastor queue`
shows an aged task's original level, such as `high (was low)`.

A level also changes room: only a `critical` task may use a machine's
burst slot, past `max_agents` (see [machines](../machines/#slots)).

## pastor queue

```sh
pastor queue
```

```text
POS  TASK  LEVEL   WHERE             FROM         WAITED  WHY NOT YET
1    t-19  high    flock sandbox     job triage   4m      flock sandbox is full
2    t-17  normal  machine server-1  task run     1h      machine server-1 is full (3/3)
3    t-18  low     flock default     task run     2d      next pass: desk has room
```

| column | is |
|---|---|
| POS | its place in the whole queue |
| LEVEL | its level |
| WHERE | the machine it is pinned to, else its flock |
| FROM | `task run`, or the job that made it |
| WAITED | how long since it was queued |
| WHY NOT YET | what holds it: a full flock or machine, a machine not connected, missing tags, or `next pass: <machine> has room` |

WHY NOT YET plays a dispatch pass through, each task that fits taking its
slot. So a task behind the last free slot reads its flock as full.
`--flock` and `--machine` filter the list and keep each task's POS. With no
head running it shows the last queue the store knows.

## change the order

```sh
pastor queue move t-18 --top          # first, lifted to the first task's level
pastor queue move t-18 --before t-17  # just ahead of t-17
pastor queue move t-18 --to 2         # at POS 2
pastor task priority t-18 high        # another level, same place in it
```

A moved task takes the level of where it lands. Moved in front of a higher
task it is lifted to that level, and moved behind a lower one it is
lowered. The answer says where it is and what changed:

```text
t-18 is 1 of 3 in the queue; lifted from low to high
```

`queue move` and `task priority` work only on queued tasks. A paused task
cannot be moved, and while one is paused, the position `--to` counts can be
off from POS by the paused tasks ahead.

## preempt a low task

A `critical` task does not have to wait behind `low` work, but only when
you ask for it with `--preempt`:

```sh
pastor task run "Prod is down: find out why" --priority critical --preempt
```

When no machine has room for it, pastor looks for a machine it may run on
that would have room once a `low` task is gone. There it pauses the newest
running `low` Claude task: it presses `esc`, closes the pane, and keeps the
worktree. The critical task starts in that slot in the same pass.

`pastor task priority t-19 critical --preempt` sets the flag on a task that
already waits. `--preempt` below critical is refused, and a job's tasks keep
`preempt = true` only when they settle at critical.

The paused task goes first among the `low` tasks, pinned to its machine.
When that machine has room again, and is still in the task's flock with the
flock under its number there, the task resumes its own Claude session.
Only a running `low` Claude task with a recorded session is ever paused; a
`normal` task, another agent, or a blocked or done task never is.

Read on: [urgent work first](../../examples/urgent-first/) puts levels and
preempting to work; the commands are in the
[cli reference](../../reference/cli/#queue).
