---
title: remote head
summary: drive a head that runs on another machine
group: run
weight: 31
manual: a-head-on-another-machine
---

The CLI can talk to a head that runs on another machine. Use it when
`pastor serve` runs on an always-on box such as `pi-1` and you want to run
and follow tasks from your laptop.

## set it

```sh
pastor head set user@pi-1  # pings the head, then saves it
pastor head set user@pi-1 --pastor '~/.local/bin/pastor'  # pastor is not on its PATH over ssh
pastor head show
pastor head unset  # back to this machine's head
```

`head set` saves nothing if the ping fails. `--force` saves it anyway.

| way | scope |
|---|---|
| `~/.config/pastor/client.toml` | every command, written by `pastor head set` |
| `PASTOR_HEAD=user@pi-1` | one shell, over the file |
| `--head user@pi-1` | one command, over both |

```toml
# ~/.config/pastor/client.toml
[head]
ssh = "user@pi-1"               # an ssh destination, as ssh takes it
pastor = "~/.local/bin/pastor"  # optional: pastor's path on the head
```

## what it needs

The CLI reaches the head over ssh, never a network port. Each request runs
`pastor bridge` on the head, one request line in, one reply line back.

- key-based ssh to the head: it runs in `BatchMode`, so a password or an
  unknown host key fails at once;
- pastor on the head, on a non-interactive shell's PATH or named with
  `--pastor`;
- `pastor serve` running there.

Errors: `head_unreachable` when ssh fails, `no_head` when nothing runs there,
`head_too_old` when that pastor is older. It never falls back to this
machine's files.

## what goes to it

The `task` commands, `machine list`, `tick`, `job list`, `job run` and
`job reload` go to the remote head. `completions`, `setup`, `head`, `bridge`,
`connector` and `task attach` stay local: connectors are this machine's, and
attach goes to the machine directly. Every other command would read this
machine's files, so it fails with `remote_head_unsupported`; run it on the
head.
`pastor serve` refuses to start while a remote head is set.

## agents on other machines

An agent on `pi-1` reaches the head the same way, with a key that can do only
what an agent should: list its flock's tasks, and read or end the tasks on
its own machine. Make a key without a passphrase on `pi-1`, copy the `.pub`
file to the head, and print its `authorized_keys` line there:

```sh
pastor machine authorized-key pi-1 --key pastor_head.pub
```

Append the line to the head user's `~/.ssh/authorized_keys`. It forces
`pastor bridge --agent --machine pi-1`, which answers anything else with
`not_allowed_for_agent`. Give each machine its own key.

Adding a machine over ssh and reading its load is in the [examples](../examples/).
More in the [manual](../manual/#a-head-on-another-machine).
