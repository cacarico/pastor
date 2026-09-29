---
title: remote
summary: the CLI here, the head somewhere else
weight: 3
aliases:
  - /docs/remote-head/
---
Pick remote when the head runs on an always-on box, such as `server-1`, and
you want to run and follow tasks from your laptop.

## set it

```sh
pastor head set user@server-1  # pings the head, then saves it
pastor head set user@server-1 --pastor '~/.local/bin/pastor'  # pastor is not on its PATH over ssh
pastor head show
pastor head unset  # back to this machine's head
```

`head set` saves nothing if the ping fails; `--force` saves it anyway.
There are three ways to name the head, and the narrower one wins:

| way | scope |
|---|---|
| `~/.config/pastor/client.toml` | every command, written by `pastor head set` |
| `PASTOR_HEAD=user@server-1` | one shell, over the file |
| `--head user@server-1` | one command, over both |

```toml
# ~/.config/pastor/client.toml
[head]
ssh = "user@server-1"           # an ssh destination, as ssh takes it
pastor = "~/.local/bin/pastor"  # optional: pastor's path on the head
```

## what it needs

The CLI reaches the head over ssh, never a network port. Each request runs
`pastor bridge` on the head: one request line in, one reply line back. The
connections share one ssh master, so only the first one logs in.

- key-based ssh to the head. It runs in batch mode, so a password, a
  passphrase prompt or an unknown host key fails at once.
- pastor on the head, on a non-interactive shell's PATH or named with
  `--pastor`.
- `pastor serve` running there.

The errors say which is missing: `head_unreachable` when ssh fails,
`no_head` when nothing runs there, `head_too_old` when that pastor is older
than the feature needs. A command never falls back to this machine's files.

## what goes to it

Almost everything goes to the head and prints what it would print there:
`task`, `queue`, `watch`, `events`, `tick`, `machine`, `flock`, `trust`,
`profile`, `config edit` and `orchestrator`. `flock edit` and `config edit`
open the head's file here and send it back to be checked and saved.

`job` commands go to wherever the job file is. A job whose file is in this
machine's `~/.config/pastor/jobs/` is this machine's; every other name is
the head's. `job list` shows both, in two tables.

These stay on this machine:

| command | why |
|---|---|
| `completions`, `setup`, `head`, `bridge` | they are about this machine |
| `connector` | connectors are installed per machine |
| `serve status`, `serve stop` | they act on this machine's serve |
| `config edit --local` | this machine's own pastor.toml |
| `task attach`, `machine open` | they open the machine directly, after asking the head where |

`machine authorized-key` works on the head only. Here it fails with
`remote_head_unsupported`, naming the head to run it on.

## a headless serve

With a head set, `pastor serve` on this machine does not start a second
head. It runs headless: it runs the jobs in this machine's `jobs/` and hands
the work they find to the head, and it runs this machine's connector hooks
on the head's events. When the head lists this machine as `pull = true`, it
also runs the tasks the head gives it: see
[a laptop that takes tasks](#a-laptop-that-takes-tasks). `pastor serve
status` says `headless`, and `pastor setup systemd` installs it as a service
the same way.

## a laptop that takes tasks

The head cannot reach a laptop behind a home router, or one that comes and
goes. It can still take tasks, as a pull machine: its headless serve asks the head
for work and runs it on its own herdr.

In the head's flock.toml (`pastor flock edit` from anywhere), give the
laptop `pull = true` in place of `ssh` or `local`:

```toml
# ~/.config/pastor/flock.toml, on the head
[[machine]]
name = "laptop"
pull = true
```

On the laptop, with its head set, start the headless serve:

```sh
pastor serve  # or pastor setup systemd, to keep it running
```

By default it takes only the tasks pinned to it, such as
`pastor task run "Tidy the notes" --machine laptop`. With
`takes_flock_work = true` under `[shepherd]` in the laptop's pastor.toml
(`pastor config edit --local`), it takes any task its flocks would place
there. `[shepherd] machine` names it when its name in flock.toml is not its
hostname. The laptop keeps its copy of those tasks in `shepherd.db`, in its
state folder. A pull
machine the head has not heard from in 10 minutes (`pull_lost_after`) is
counted lost, and its starting and running tasks go `stale`.

## move the head

To move the head from `laptop` to `server-1`, and keep `laptop` as a pull
machine:

1. On `laptop`, stop the head: `pastor serve stop`, or its service with
   `pastor setup systemd --stop`.
2. Copy `flock.toml`, `pastor.toml`, `jobs/`, `orchestrators/` and each
   `connectors/<id>/.env` from `~/.config/pastor/`, and `pastor.db`,
   `orchestrators/` and, for the history, `events.jsonl` from
   `~/.local/state/pastor/`, to the same places on `server-1`. Without
   `orchestrators/` in both, the new head has no orchestrators, or starts
   them with no state or note. Install the connectors the jobs use there.
   Delete the job files from `laptop`, or they run there as well.
3. On `server-1`, in the copied flock.toml, make `server-1` `local = true`
   and `laptop` `pull = true`. In pastor.toml, set `head_address` to the
   ssh destination the machines reach `server-1` by.
4. On `server-1`, run `pastor setup systemd`, which starts the head. It
   needs key-based ssh to every `ssh` machine, as `laptop` had.
5. On `laptop`, run `pastor head set user@server-1`, then
   `pastor setup systemd` again: it now installs the headless serve.
6. Give each machine whose agents report back its own locked key on
   `server-1` (see [let agents report back](../flock/#let-agents-report-back)).

From `laptop`, `pastor machine list` then names `user@server-1` as the head
and `pastor task list --all` shows the old tasks. `laptop`'s own `pastor.db`
is no longer read; delete it once the move works.
