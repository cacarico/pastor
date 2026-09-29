---
title: flocks
summary: groups of machines that keep work apart
weight: 3
aliases:
  - /docs/flocks/
---
A flock is a named group of machines. Every task and job targets one flock,
and only that flock's machines take its work. Use flocks to keep work apart:
tasks that read outside text run on a sandbox machine with none of your
credentials, and everyday tasks never land there. Or share one machine
between projects so that no project takes every slot.

## flock.toml

```toml
# ~/.config/pastor/flock.toml
[[flock]]
name = "default"
default = true                         # tasks and jobs that name no flock go here
machines = { desk = 2, server-1 = 3 }  # desk runs at most 2 of default's tasks

[[flock]]
name = "sandbox"
machines = { sandbox-1 = 1 }
description = "Outside PRs and issues, on a machine with no credentials"

[[machine]]
name = "desk"
local = true
max_agents = 3

[[machine]]
name = "server-1"
ssh = "user@server-1"
max_agents = 3

[[machine]]
name = "sandbox-1"
ssh = "user@sandbox-1"
```

A flock's `machines` names the machines it may use, each with its number
there: at most how many of the flock's live tasks that machine runs. A
machine can be in many flocks, each with its own number.

## the default flock

Exactly one flock has `default = true`. A task or job that names no flock
goes there, and so does a machine that no flock lists. A file with no
`[[flock]]` at all is one flock named `default` that holds every machine,
so you only need flocks once you want to split work.

A task's flock is fixed when it is queued: its `--flock`, or the job's
`flock`; else the flock of the machine it is pinned to; else the default.

## a machine in many flocks

A task starts on a machine only when the machine has room for it and the
task's flock is under its number there. Job slots and burst never take a
flock past its number. A task whose flock is full waits, with a reason like
`flock sandbox is at 1 of 1 on sandbox-1`, and the next task in the queue goes.

A plain number is a hard ceiling. To let a busy project use slots that
quiet ones leave idle, give its flock a share and a max:

```toml
# ~/.config/pastor/flock.toml
[[flock]]
name = "code"
machines = { desk = { share = 2, max = 4 } }
```

Under its share the flock takes a free slot as usual. Between its share and
its max it takes one only while no flock that is under its share on that
machine has a task waiting. So a quiet project gets its share back as soon
as it has work, and nothing is held for it while it has none. The machine's
own room still caps everything. `flock list` shows this as `desk 1/2/4`
(live, share, max) and `machine list` as `code:2/4`.

## what a flock sets

A flock can carry every per-task setting that `[defaults]` in `pastor.toml`
has: `agent`, `agent_args`, `agents`, `model`, `profile`, `priority`,
`allow`, `deny`, `timeout`, `place`, `label` and `summary`. A task takes its
own flock's, so on a shared machine each project sets its own model,
permissions, priority and timeout.

```toml
# ~/.config/pastor/flock.toml
[[flock]]
name = "app"
machines = { server-1 = 3 }
model = "sonnet"     # a name from [models] in pastor.toml
profile = "develop"  # a permission profile
priority = "high"
timeout = "1h"
```

Each setting comes from the first layer that sets it: the task's own flags
or its job, then its flock, then the machine it runs on, then `[defaults]`.
Only `model`, `profile` and `priority` have a machine layer. `allow` and
`deny` add up across layers instead, and a deny always wins. The agent is
the one exception: the machine comes before the flock, because it knows
what is installed and logged in there. See
[agents and models](../agents-and-models/).

## manage them

```sh
pastor flock list --wide                  # machines, live agents, queued tasks, descriptions
pastor flock describe sandbox
pastor flock add lab server-1 --description "Test rigs"  # server-1 joins it
pastor flock join lab desk --max 2        # desk runs at most 2 of lab's tasks
pastor flock leave lab desk               # out of lab; out of its last flock, back in the default
pastor flock default set lab              # new tasks and jobs go to lab
pastor flock edit                         # saved only once valid
```

These commands edit `flock.toml` in place and keep its comments, and a
running head picks the change up at once. `flock join` keeps the machine in
its other flocks; without `--max` it keeps the number it has there, or takes
the machine's `max_agents`. Tasks already running keep running when a
machine leaves a flock.

An older file may put a machine in a flock with `flock = "sandbox"` on its
`[[machine]]`. That still works. The first `flock join`, `flock leave` or
`machine move` on that machine moves it into the flock's `machines`.

A flock is a routing rule, not a sandbox: every agent still runs as the
machine's user. See [profiles and trust](../profiles-and-trust/).

Read on: [untrusted work in a sandbox](../../examples/sandbox/) keeps
outside PRs on a machine of their own; every key is in
[flock.toml](../../reference/flock-toml/), and the commands in the
[cli reference](../../reference/cli/#flock).
