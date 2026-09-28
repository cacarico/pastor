---
title: solo
summary: one machine is the head and the worker
weight: 1
---
Pick solo when one computer is enough: it runs the head and every agent,
with no ssh at all.

## set it up

```sh
pastor machine add here --local  # this machine, through herdr's local socket
pastor serve  # the head, in the background
pastor machine list
```

The machine is called `here`; any name works. `--local` means the head
talks to this machine's herdr directly, so herdr's server must run here. On
a laptop you can leave herdr open in a terminal; on a box that stays up, run
it as a service with `pastor setup systemd --herdr` (see
[as a service](../service/)).

`machine list` opens with a line about the head that ends in
`here is the head of the flock`, then one row for `here`. The head's own
machine always comes first.

[first run](../../start/first-run/) walks through a whole task on this
setup.

## how much it takes

`machine add` gives the machine room for 2 tasks at once, one extra slot
that only tasks from jobs take, and one more that only a critical task may
use. AGENTS in `machine list` shows it as `0/2+1j+1b`. Change it when you
add the machine:

```sh
pastor machine add here --local --max-agents 3 --job-slots 0 --burst 0
```

or later in `~/.config/pastor/flock.toml`. Tasks past the room wait in the
[queue](../../concepts/queue/).

## when solo is enough

- You run a few agents at a time, and one machine has the CPU, memory and
  accounts for them.
- The machine is on while the agents work. A laptop that sleeps pauses them
  all.
- Everything the agents need, repos and logins, is on this machine.

Jobs, orchestrators, flocks and profiles all work the same on one machine.
A work flock and a personal flock can both use `here`, each with its own
agent and login: see [work and personal](../../examples/work-and-personal/).

## grow into a fleet

Nothing you set up solo has to change. Add a machine the head reaches over
ssh, and the head spreads tasks over both:

```sh
pastor machine add server-1 user@server-1 --herdr
```

Your tasks, jobs and history stay where they are. The head keeps running on
this machine, which keeps taking tasks too. [fleet](../fleet/) covers ssh,
herdr on the new machine and the load. To move the head itself to an
always-on box later, see [move the head](../remote/#move-the-head).
