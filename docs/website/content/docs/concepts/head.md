---
title: head
summary: the one machine that keeps track
weight: 1
---
The head is the `pastor serve` process that owns your fleet. It holds the
queue, runs jobs and orchestrators on their schedule, starts agents on the
machines, and keeps the history. Everything else, the CLI included, asks it.

## what it keeps

| thing | where |
|---|---|
| tasks, their summaries, seen items, job state, saved trust | `~/.local/state/pastor/pastor.db` |
| every event, one JSON line each | `~/.local/state/pastor/events.jsonl` |
| its own log, when it runs in the background | `~/.local/state/pastor/serve.log` |
| the unix socket the CLI talks to | `~/.local/state/pastor/pastor.sock` |

It reads its config from `~/.config/pastor/`: `flock.toml`, `pastor.toml`,
`jobs/` and `orchestrators/`. It picks up edits on its next tick, every 10
seconds by default, and `pastor job reload` makes it read them now.

## how the CLI reaches it

The CLI talks to the head over that unix socket, on the same machine. The
head reaches other machines over ssh, one shared ssh connection per machine,
and asks each machine's herdr to open panes and start agents. It never
opens a network port. A CLI on another machine reaches it through ssh as
well: see [remote](../../deploy/remote/).

## start and stop it

```sh
pastor serve         # start it in the background; returns once it answers
pastor serve status  # head or headless, pid, version, service, log
pastor serve stop    # stop it; the agents keep running
```

`pastor serve --foreground` keeps it in the terminal, logging to stderr.
That is what a service runs: `pastor setup systemd` installs one that
starts at login and comes back after a crash (see
[as a service](../../deploy/service/)). `serve stop` refuses a head that
systemd or launchd runs, since they would start it again; stop it through
them.

Stopping the head does not stop your agents. They keep working in their
panes, and a head that starts again picks their tasks up from the store.

## when it is down

Read commands still work. They fall back to what the head left behind and
say so on stderr:

- `pastor task list`, `pastor task describe` and `pastor queue` read the store.
- `pastor machine list` probes each machine itself.
- `pastor job list` and `pastor orchestrator list` show the files and the
  last known state.
- `pastor events` reads the events file.

Commands that change work need the head: `pastor task run` fails with
"pastor serve is not running". `pastor tick` runs one pass on its own, but
the tasks it makes stay queued until a head starts.

A head that holds the socket but does not answer within two seconds is
treated as busy, not gone. Commands then stop with `head_unresponsive`
instead of working around it, so two schedulers never run side by side.

## one head, or a headless serve

A fleet has one head. On a machine whose CLI points at a head elsewhere,
`pastor serve` runs headless. It runs that machine's own jobs and hooks
and hands their items to the head. On a machine the head lists as
`pull = true`, it also takes tasks from the head and runs them there.
`pastor serve status` says which one runs. The [remote](../../deploy/remote/) page covers that setup.

Read on: [deploy solo](../../deploy/solo/) and
[as a service](../../deploy/service/) set a head up; its settings are in
[pastor.toml](../../reference/pastor-toml/), and its commands in the
[cli reference](../../reference/cli/).
