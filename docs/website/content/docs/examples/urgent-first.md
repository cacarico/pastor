---
title: urgent work first
summary: priorities, the queue, preempting
weight: 4
---
On a busy day every slot is taken and new work waits. Priorities decide
what starts next when a slot frees, and the queue shows the order and why
each task still waits. When something is on fire, a `critical` task can
pause low work to start at once.

## what you need

A flock that is full. Here `desk` and `server-1` run 2 and 3 agents, and
every agent counts against `max_agents`: no job slots and no burst.

```toml
# ~/.config/pastor/flock.toml
[[machine]]
name = "desk"
local = true
max_agents = 2
job_slots = 0
burst = 0

[[machine]]
name = "server-1"
ssh = "user@server-1"
max_agents = 3
job_slots = 0
burst = 0
```

Both default to 1. With `burst = 1`, a critical task first takes one slot
past `max_agents`, and pauses nothing until that is gone too.

## jump the line

Levels are `low`, `normal`, `high` and `critical`. A task that names none
takes its flock's `priority`, else `[defaults]`, else `normal`. Dispatch takes higher levels first, then the order tasks came in.

```sh
pastor task run "Fix the checkout crash on Safari" --repo '~/src/app' --worktree --priority high
pastor queue
```

```text
POS  TASK  LEVEL   WHERE          FROM        WAITED  WHY NOT YET
1    t-21  high    flock default  task run    1m      flock default is full
2    t-18  normal  flock default  job kanban  6m      flock default is full
3    t-19  normal  flock default  task run    5m      flock default is full
4    t-16  low     flock default  task run    14m     flock default is full
```

POS is the order the tasks will start in. FROM says who asked: `task run`
or a job. Levels do not age, so a `low` task can wait for ever; WAITED shows
how long it has.

## reorder

Put a task first. It takes the level of where it lands, so `t-19` becomes
`high`:

```sh
pastor queue move t-19 --top
```

```text
t-19 is 1 of 4 in the queue; lifted from normal to high
```

`--before t-21`, `--after t-21` and `--to 2` place it elsewhere. To change
a task's level without moving it by hand, set it:

```sh
pastor task priority t-16 normal
```

```text
ID    STATE   PRIORITY  MACHINE  FLOCK    AGENT   MODEL  JOB  AGE  NOTE
t-16  queued  normal    -        default  claude  -      run  14m  Tidy up the README
```

## pause low work

Production is down. A `critical` task goes first, and with `--preempt` it
need not wait for a slot:

```sh
pastor task run "Prod login is down: find out why" --repo '~/src/app' --worktree --priority critical --preempt
```

With every slot full, it pauses the newest running `low` Claude task on a
machine and takes its slot. The paused agent is interrupted and its pane
closed; its worktree stays. With no such task to pause, the critical task
waits at the head of the queue.

```sh
pastor task list --machine server-1
```

```text
ID    STATE    PRIORITY  MACHINE   FLOCK    AGENT   MODEL  JOB  AGE  NOTE
t-22  running  critical  server-1  default  claude  -      run  1m   Prod login is down: find out why
t-17  running  normal    server-1  default  claude  -      run  25m  Update the Node version in CI
t-15  running  high      server-1  default  claude  -      run  40m  Upgrade the payment SDK
t-14  paused   low       server-1  default  claude  -      run  1h   Rename the old config keys
```

The paused task goes back in the queue, first among the `low` ones, and
waits for its own machine:

```text
POS  TASK  LEVEL   WHERE             FROM        WAITED  WHY NOT YET
1    t-19  high    flock default     task run    7m      flock default is full
2    t-21  high    flock default     task run    3m      flock default is full
3    t-16  normal  flock default     task run    16m     flock default is full
4    t-18  normal  flock default     job kanban  8m      flock default is full
5    t-14  low     machine server-1  task run    1h      paused for t-22; machine server-1 is full (3/3)
```

When a slot on `server-1` frees and nothing higher waits, `t-14` resumes its
own Claude session there. Only a `critical` task may preempt, and only a
`low` Claude task is ever paused. A paused task keeps its place: `queue move`
does not move it.

## next

- [queue and priority](../../concepts/queue/) and [machines](../../concepts/machines/)
- [cli: queue](../../reference/cli/#queue) and [flock.toml](../../reference/flock-toml/)
