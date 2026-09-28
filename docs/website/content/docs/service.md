---
title: run as a service
summary: keep the head up after you log out
group: run
weight: 30
manual: run-under-systemd
---

`pastor setup` installs the head as a user service, so it starts at login
and comes back after a crash. It uses systemd on Linux and launchd on macOS,
and never needs root. Without one, `pastor serve` runs the head in the
background until you `pastor serve stop` it or the machine restarts.

The service runs `pastor serve --foreground`, which logs to the journal (or
launchd's log) instead of `serve.log`. A unit written by an older pastor, with
a bare `pastor serve`, stays in the foreground too, since pastor sees that
systemd or launchd started it; re-run `pastor setup` to bring it up to date.

## linux

```sh
pastor setup systemd
```

It writes `~/.config/systemd/user/pastor.service`, shows what it will do,
and waits for you to type `yes`. Then it runs
`systemctl --user enable --now`. The unit points at the pastor it found and
copies your shell's `PATH`, so `ssh`, `herdr` and the agents resolve as they
do in a terminal. Run it again after you move a binary.

| flag | does |
|---|---|
| `--yes` | skip the prompt; needed when stdin is not a terminal |
| `--herdr` | install `herdr.service` instead, for each flock machine |
| `--enable` | enable the unit at login |
| `--start` | start it now |
| `--stop` | stop it now |

A user service stops at logout unless lingering is on. Setup checks, and
prints `loginctl enable-linger` when it is off.

## macos

```sh
pastor setup launchd
```

It writes `~/Library/LaunchAgents/pastor.serve.plist` and loads it, which
starts it. It takes the same flags. Logs go to
`~/Library/Logs/pastor.serve.log`.

## check, restart, logs

```sh
pastor serve status          # service: systemd
systemctl --user status pastor
systemctl --user restart pastor
journalctl --user -u pastor
pastor machine list
```

`pastor serve stop` refuses a head a service runs, since the service would
start it again: use `pastor setup systemd --stop` (or `launchd --stop`).

Setup never restarts a running service, since restarting herdr stops its
agents. After a change, restart it yourself. A unit that differs from what
setup would write is kept as `pastor.service.bak`.

On macOS a loaded agent keeps its old plist until it is unloaded and loaded
again:

```sh
pastor setup launchd --stop
pastor setup launchd --start
```

More in the [manual](../manual/#run-under-systemd).
