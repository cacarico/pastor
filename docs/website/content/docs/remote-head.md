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

The `task` commands, `queue`, `events`, `tick`, `job reload` and the head's
jobs go to the remote head, and so do the `flock` commands, `machine
list|add|remove|move|describe`, `trust`, `profile` and `config edit`. They
print what they would print on the head itself. `flock edit` and `config
edit` open the head's file here and send it back to be checked and saved.

`completions`, `setup`, `head`, `bridge` and `connector` stay local:
connectors are this machine's. `task attach` and `machine open` go to the
machine directly, but ask the head for the task and its flock.toml. `config
edit --local` edits this machine's pastor.toml, which its headless serve
reads. `machine authorized-key` runs on the head only; here it fails with
`remote_head_unsupported`, naming the head.

With a head set, `pastor serve` runs headless: it runs this machine's own jobs
and hooks for the head.

## move the head

To move the head from `laptop` to `pi-1`: stop it on `laptop`; copy
`flock.toml`, `pastor.toml`, `jobs/` and `pastor.db` to `pi-1`; there, make
`pi-1` `local = true` and `laptop` `pull = true` in flock.toml and set
`head_address`; run `pastor setup systemd` on `pi-1`, then `pastor head set
user@head.example` and `pastor setup systemd` on `laptop`; give the other
machines their locked keys. The steps in full are in the
[manual](../manual/#moving-the-head).

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
