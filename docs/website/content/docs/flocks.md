---
title: flocks
summary: named groups of machines
group: use
weight: 23
manual: flocks
---

A flock is a named group of machines. Every task and job targets one flock,
and only its machines take the work, so work and personal machines, logged in
to different accounts, never run each other's agents. A machine can be in many
flocks, each with its own limit there, so one machine can be shared between
projects without one taking every slot.

## flock.toml

```toml
# ~/.config/pastor/flock.toml
[[flock]]
name = "personal"
default = true  # tasks and jobs that name no flock go here
machines = { here = 2 }  # here runs at most 2 of personal's tasks

[[flock]]
name = "work"
machines = { here = 1 }  # and at most 1 of work's
description = "Paid work, on the work account"  # optional, for --wide and describe
agent = "claude"
agent_args = ["--model", "claude-sonnet-5"]
summary = "require"  # optional: ask, require or off, before [defaults]
model = "sonnet"     # optional: before the machine's and [defaults]
profile = "develop"  # optional: likewise
priority = "high"    # optional: likewise
timeout = "1h"       # optional: before [defaults]
place = "pastor"     # optional: likewise

[[machine]]
name = "here"
local = true
max_agents = 3

[[machine]]
name = "pi-1"
ssh = "user@pi-1"
flock = "work"  # the old way: in work, with the machine's own limits
tags = ["arm"]
```

Exactly one flock has `default = true`. A file with no `[[flock]]` at all is
one flock named `default` that holds every machine, and a machine nothing
places is in the default flock.

A task starts on a machine only when the machine has room and the task's flock
is under its number there; job slots and burst never pass it. A task whose
flock is full waits, saying `flock work is at 1 of 1 on here`, and the next
task in the queue goes.

## what a flock sets

A flock carries every per-task setting `[defaults]` has: `agent`,
`agent_args`, `agents`, `model`, `profile`, `priority`, `allow`, `deny`,
`timeout`, `place`, `label` and `summary`. A task gets its own flock's. For
everything but the agent the flock comes before the machine: a task takes
each from its own flags or job, then its flock, then the machine, then
`[defaults]`. So on a machine shared by several projects, each project's flock
sets its model, permissions, priority and timeout. A machine's own `profile`
still decides whether a task may ask for `unrestricted` there.

## which agent

The agent is the exception: the machine knows what is installed and logged in
there, so it comes first. `agent` and `agent_args` on a flock set the agent
its tasks run when they name none. The same keys on a `[[machine]]` set it for
that one machine. A task takes the first of: its own `--agent`, its machine,
its flock, `[defaults]` in `pastor.toml`, then plain `claude`.

A flock that names an agent names its kind too. A machine whose agent is of
another kind runs its `agents` entry for the flock's kind; a machine with none
is skipped for the flock's tasks, and a task pinned there is refused
(`agent_kind_missing`).

An agent can be a definition in `pastor.toml`, such as a second Claude
account:

```toml
[agents.claude-personal]
kind = "claude"
env = { CLAUDE_CONFIG_DIR = "~/.claude-personal" }
```

Then `agent = "claude-personal"` on a flock or machine runs Claude with that
config. `pastor task describe` shows which agent a task got and where from.

## manage them

```sh
pastor flock list --wide  # with each flock's description
pastor flock add lab pi-1 --description "Test rigs"  # pi-1 joins it
pastor flock join work here --max 2  # here runs at most 2 of work's tasks
pastor flock leave work here  # out of its last flock, back in the default
pastor flock describe work
pastor machine move pi-1 work  # leave every flock, join work; tasks on it stay
pastor flock edit  # saved only once valid
```

`flock join` keeps the machine in its other flocks; without `--max` it keeps
the number it has there, or takes the machine's `max_agents`. The first
`join`, `leave` or `move` on a machine with the old `flock` key moves it into
that flock's `machines`, comments kept. `flock list` shows each machine with
the flock's number and live tasks (`here 1/2`), and `machine list` its flocks
(`personal:2,work:1`).

## room on a machine

`max_agents` (default 2) is how many tasks a machine runs at once. Two more
keys on a `[[machine]]` make room past it:

```toml
[[machine]]
name = "pi-1"
ssh = "user@pi-1"
max_agents = 2
job_slots = 1  # default 1: extra slots only tasks from jobs take
burst = 1      # default 1: how far past max_agents a critical task may go
```

A task from a job takes a free job slot first, then a shared one, so a long
`task run` cannot keep a job's tasks from starting. A `critical` task that
finds the shared slots full may still start, up to `max_agents + burst`
tasks outside job slots. `0` turns either off. Among the machines with room,
the one with the fewest live tasks takes the task. `machine list` shows the
room as `2+1j+1b`.

## tags

A machine's `tags` pick machines inside a flock. A task run with `--tag`
goes only to a machine that carries every tag it asks for.

```sh
pastor task run "Build the image" --repo '~/work/api' --flock work --tag arm
```

Two accounts on one head, with a flock each, is in the [examples](../examples/).
More in the [manual](../manual/#flocks).
