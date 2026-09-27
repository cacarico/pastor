---
title: concepts
summary: head, machines, flocks, tasks, jobs
group: start
weight: 12
manual: how-it-works
---

pastor has a handful of parts. Knowing what each one is makes the commands
and the config files easy to read.

## the parts

| part | is |
|---|---|
| head | the one machine that runs `pastor serve`: the queue, the schedule, the history |
| machine | a computer that takes tasks, running herdr; the head can be one |
| flock | a named group of machines; every machine is in exactly one |
| task | one agent, one prompt, in a repo or its own git worktree |
| agent | the coding agent a task starts, such as `claude` or `codex` |
| job | a TOML file: a schedule plus a connector, turning work into tasks |
| connector | a small program that finds work: `clock` is built in, others install from GitHub |

## how they fit

```text
you ─► pastor CLI ─┐
                   ├─► head: pastor serve ──ssh + herdr──► machine ──► agent in a pane
jobs ──────────────┘   queue · schedule · history
```

The CLI talks to the head over a unix socket. The head reaches each machine
over ssh and asks its herdr to open a pane and start the agent. The head
never opens a network port.

## flocks keep work apart

A task or a job targets one flock, and only that flock's machines take its
tasks. Use them to keep work and personal agents on the right accounts. A
flock, or a single machine, can name its own agent. One flock is the
default, for tasks that name none.

## where things live

| file | holds |
|---|---|
| `~/.config/pastor/flock.toml` | flocks and machines |
| `~/.config/pastor/pastor.toml` | the head's settings and task defaults |
| `~/.config/pastor/jobs/<name>.toml` | one job per file |
| `~/.local/state/pastor/pastor.db` | tasks, seen items, job state |

More in the [manual](../manual/#how-it-works).
