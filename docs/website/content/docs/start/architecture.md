---
title: architecture overview
summary: the parts and how they talk
weight: 3
aliases:
  - /docs/concepts/
---
pastor has a handful of parts. Knowing what each one is makes the commands
and the config files easy to read.

## the parts

| part | is |
|---|---|
| head | the `pastor serve` that holds the queue, the schedule and the history; one per fleet |
| machine | a computer that runs agents in its herdr; the head's own machine can be one |
| flock | a named group of machines; a machine can be in several, and one flock is the default |
| task | one prompt to one agent on one machine, in a repo or a git worktree of its own |
| agent | the coding agent a task starts: `claude`, `codex`, `opencode`, ... |
| job | a TOML file: a schedule plus a connector, turning each piece of work it finds into a task |
| connector | a small program that finds work: `clock` is built in, others install from GitHub |
| orchestrator | an agent the head starts to watch other agents: on a schedule, or kept running |

## how they fit

```text
you ──► pastor CLI ──unix socket──► head: pastor serve ──ssh + herdr──► machine ──► agent in a pane
                                    queue · schedule · history
                                      ▲
job files ────────────────────────────┘
```

The CLI talks to the head over a unix socket in the state folder. The head
reaches each machine over ssh, through one shared connection per machine,
and asks its herdr to open a pane and start the agent there. For its own
machine it uses herdr's local socket, with no ssh. It then follows each
agent's status through herdr until the agent finishes, asks something or
runs out of time.

The head never opens a network port. A CLI on another machine reaches it
the same way the head reaches machines: over ssh, running `pastor bridge`
on the head's machine.

A second machine can also run `pastor serve` with a head set elsewhere. It
then runs headless: its own jobs and hooks, and, as a pull machine, the
tasks the head gives it. There is still one head.

## where things live

| path | holds |
|---|---|
| `~/.config/pastor/flock.toml` | machines and flocks |
| `~/.config/pastor/pastor.toml` | the head's settings and the task defaults |
| `~/.config/pastor/jobs/<name>.toml` | one job per file |
| `~/.config/pastor/orchestrators/<name>.toml` | one orchestrator per file |
| `~/.config/pastor/client.toml` | which head this CLI uses, when it is elsewhere |
| `~/.local/state/pastor/pastor.db` | tasks, seen items, job state, saved trust |
| `~/.local/state/pastor/events.jsonl` | the events log |
| `~/.local/state/pastor/serve.log` | the log of a head started in the background |
| `~/.local/state/pastor/pastor.sock` | the socket the CLI talks to |
| `~/.local/share/pastor/connectors/` | installed connectors |

`PASTOR_CONFIG_DIR`, `PASTOR_STATE_DIR` and `PASTOR_DATA_DIR` move the three
folders; the XDG variables work too. The full list is in the
[pastor.toml reference](../../reference/pastor-toml/).

## where to read next

| to learn | read |
|---|---|
| what the head does and keeps | [head](../../concepts/head/) |
| how machines are reached and how much they take | [machines](../../concepts/machines/) |
| keeping work and personal agents apart | [flocks](../../concepts/flocks/) |
| a task's life, from queued to closed | [tasks](../../concepts/tasks/), [queue](../../concepts/queue/) |
| work that finds itself | [jobs](../../concepts/jobs/), [connectors](../../concepts/connectors/) |
| which agent and model a task gets | [agents and models](../../concepts/agents-and-models/) |
| what an agent may do | [profiles and trust](../../concepts/profiles-and-trust/) |
| agents that watch agents | [orchestrators](../../concepts/orchestrators/) |

And to lay it out on your machines:

| setup | when |
|---|---|
| [solo](../../deploy/solo/) | one machine is the head and runs every agent |
| [fleet](../../deploy/fleet/) | a head plus machines it reaches over ssh |
| [remote](../../deploy/remote/) | the CLI on a laptop, the head somewhere else |
| [as a service](../../deploy/service/) | keep the head and herdr running across reboots |
