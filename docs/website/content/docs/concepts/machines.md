---
title: machines
summary: where agents run
weight: 2
---
A machine is a computer that runs a herdr server, which the head can reach.
Each task runs on one machine, in a pane of that machine's herdr. The head
can be a machine too.

## how the head reaches one

A machine is a `[[machine]]` entry in `~/.config/pastor/flock.toml`, and it
is reached in one of these ways:

| kind | how | for |
|---|---|---|
| `local = true` | herdr's local socket, no ssh | the head's own machine |
| `ssh = "user@server-1"` | ssh, with keys and no password | any other machine |
| `command = [...]` | a program that speaks herdr's protocol on stdio | tests and development |
| `pull = true` | the machine's own headless `pastor serve` asks the head for work | a machine the head cannot reach |

`pastor machine add` writes the entry for you:

```sh
pastor machine add desk --local
pastor machine add server-1 user@server-1 --max-agents 3 --tag arm
pastor machine add laptop user@laptop --flock sandbox --herdr  # also in herdr's sidebar
```

Put ssh ports, keys and jump hosts in `~/.ssh/config` under a host alias,
and give pastor the alias. A machine's tags pick machines inside a flock: a
task run with `--tag arm` goes only to a machine that has every tag it asks
for. Setting up ssh and herdr on each machine is in
[flock](../../deploy/flock/); a pull machine is in
[remote](../../deploy/remote/).

## slots

Three numbers say how many agents a machine runs at once:

```toml
# ~/.config/pastor/flock.toml
[[machine]]
name = "server-1"
ssh = "user@server-1"
max_agents = 3  # default 2: slots every task can take
job_slots = 1   # default 1: extra slots only tasks from jobs take
burst = 1       # default 1: how far past max_agents a critical task may go
```

A task from a job takes a free job slot first, then a shared one. So a long
`task run` cannot keep a job's tasks from starting. A `critical` task that
finds the shared slots full may still start, up to `max_agents + burst`.
`0` turns job slots or burst off. With the defaults, `max_agents = 2` means
up to four agents: two shared, one job slot, one burst.

A task holds its slot while it is starting, running, blocked or stale, and
while it is done until its pane closes. An agent named like a task that no
task owns (an orphan) holds a slot too. Among the machines with room, the
one with the fewest live tasks takes the next task.

## machine list

```sh
pastor machine list
```

```text
pastor 0.8.0 on desk (herdr 0.9.1), 3 machines, desk is the head of the flock

NAME      HOST           FLOCKS     PROFILE  CHANNEL       HERDR  PASTOR  AGENTS     ORPHANS  TAGS  ERROR
desk      local          default:2  -        connected     0.9.1  0.8.0   1/2+1j+1b  -        -
server-1  user@server-1  default:3  develop  connected     0.9.1  0.8.0   2/3+1j+1b  -        arm
laptop    user@laptop    sandbox:1  review   reconnecting  0.9.1  0.8.0   0/2+1j+1b  -        -     connection lost
```

AGENTS is the machine's live tasks and orphans over its room: `max_agents`,
then `+1j` for job slots and `+1b` for burst. FLOCKS names the
[flocks](../flocks/) it is in, with its number in each. PROFILE is the
[permission profile](../profiles-and-trust/) its tasks get when they name
none. ORPHANS names orphaned agents; `pastor task close` closes one.

CHANNEL is the head's connection to the machine:

| channel | means |
|---|---|
| `connecting` | the first connection is being made |
| `connected` | requests answer and herdr's events stream in |
| `polling` | requests answer but the event stream will not open; tasks are checked every 10 seconds instead |
| `reconnecting` | the connection was lost; the head keeps trying, and ERROR says why |
| `incompatible` | its herdr is too old; update herdr there |

Only `connected` and `polling` machines take tasks. With no head running,
`machine list` probes each machine itself, and CHANNEL reads `probed`,
`server down`, `unreachable` or `error` instead.

## one machine in full

```sh
pastor machine describe server-1    # host, flocks, channel, versions, agents, recent errors
pastor machine list --wide          # adds each machine's description
pastor machine move laptop sandbox  # out of every flock, into sandbox
pastor machine remove laptop        # its tasks keep their rows
pastor machine open server-1        # the full herdr UI there
```

A machine can also set the agent, model, profile and priority of the tasks
it runs: see [agents and models](../agents-and-models/).

Read on: the [flock](../../deploy/flock/) and
[sandbox](../../examples/sandbox/) pages set machines up; the keys are in
[flock.toml](../../reference/flock-toml/), and the commands in the
[cli reference](../../reference/cli/#machine).
