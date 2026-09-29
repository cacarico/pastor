---
title: as a service
summary: keep the head and herdr running
weight: 4
aliases:
  - /docs/service/
---
Pick a service when the head, or herdr on a machine, should start at login
and come back after a crash, without a terminal kept open for it.

`pastor setup` installs a user service: systemd on Linux, launchd on macOS.
It never needs root. Without it, `pastor serve` runs the head in the
background until you `pastor serve stop` it or the machine restarts.

## what it installs

| command | installs | runs |
|---|---|---|
| `pastor setup systemd` | `~/.config/systemd/user/pastor.service` | `pastor serve --foreground` |
| `pastor setup systemd --herdr` | `~/.config/systemd/user/herdr.service` | `herdr server` |
| `pastor setup launchd` | `~/Library/LaunchAgents/pastor.serve.plist` | `pastor serve --foreground` |
| `pastor setup launchd --herdr` | `~/Library/LaunchAgents/pastor.herdr.plist` | `herdr server` |

Run the pastor one on the head. Run the herdr one on every machine that
runs agents, the head's own included; it is how a machine in a flock keeps its
herdr up.

The file points at the binary setup found: the pastor you ran, or the
`herdr` on your `PATH`. It copies your shell's `PATH`, so `ssh`, `herdr`
and the agents resolve as they do in a terminal. It leaves out entries that
are empty, relative, missing or writable by anyone, and names them. The
pastor unit also pins the config, state and data folders this shell uses.
Run setup again after you move a binary or change your `PATH`.

For the head, setup also makes the config and state folders `0700`, and
the head's socket and connector `.env` files `0600`.

## it asks first

Setup shows the file it will write, the binary it will run and the action,
then waits for you to type `yes`. `--yes` skips the prompt. It is needed
when stdin is not a terminal: in a script, in a task, or over
`ssh server-1 pastor setup systemd --herdr --yes`.

## linux

```sh
pastor setup systemd
```

With no other flag, it writes the unit, then runs
`systemctl --user enable --now`: enabled at login and started now.

| flag | does |
|---|---|
| `--yes` | skip the prompt |
| `--herdr` | install `herdr.service` instead of `pastor.service` |
| `--enable` | only enable the unit at login |
| `--start` | only start it now |
| `--enable --now` | enable it and start it now |
| `--stop` | stop it now |

A user service stops at logout unless lingering is on. Setup checks, and
prints `loginctl enable-linger` when it is off. Run that once on a machine
you do not stay logged in to.

## macos

```sh
pastor setup launchd
```

It writes the plist and loads it, which starts it. It takes the same
flags; `--stop` unloads it until the next login. Logs go to
`~/Library/Logs/pastor.serve.log` (or `pastor.herdr.log`).

## check, restart, logs

```sh
pastor serve status  # service: systemd
systemctl --user status pastor
systemctl --user restart pastor
journalctl --user -u pastor
```

The service logs to the journal (or the launchd log above), not to
`serve.log`. `pastor serve stop` refuses a head a service runs, since the
service would start it again: use `pastor setup systemd --stop` (or
`launchd --stop`).

Setup never restarts a running service, since restarting herdr stops every
agent it runs. When it updates a file, it keeps the old one as `.bak` and
tells you the running service still has the old one. Restart it yourself
when no agent would mind. On macOS a loaded agent keeps its old plist until
it is unloaded and loaded again:

```sh
pastor setup launchd --stop
pastor setup launchd --start
```

## a headless serve

On a machine with a head set elsewhere, `pastor setup systemd` installs the
same unit. `pastor serve --foreground` reads the head from `client.toml` and
runs headless: see [remote](../remote/#a-headless-serve).
