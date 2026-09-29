---
title: flock
summary: a head plus machines over ssh
weight: 2
---
Pick a flock when one machine is not enough: the head runs on one box and
spreads agents over the others it reaches by ssh.

## what each machine needs

The head connects out to every machine; nothing connects in to the head.
Each machine needs:

- **ssh from the head without a prompt.** pastor runs ssh in batch mode, so
  a password, a key passphrase or an unknown host key fails at once. Use a
  key with no passphrase (or an ssh-agent the head can reach), and connect
  once by hand to accept the host key. `user@server-1` can also be a `Host`
  from your ssh config.
- **herdr, with its server running.** pastor asks it to open panes and
  start agents. To keep it up across logouts and reboots, install pastor on
  the machine and run `pastor setup systemd --herdr` there (`setup launchd
  --herdr` on macOS). Restarting herdr stops its agents, so set it up
  before you give it work.
- **the agents and the repos.** `claude`, `codex` or `opencode`, logged in,
  and the repos a task's `--repo` names, at that path on that machine.

pastor on the machine is optional. With it, `machine list` shows its version
and its agents can report back to the head (below).

Check from the head that ssh gets in without a prompt and finds herdr on
the PATH a non-interactive shell gets:

```sh
ssh -o BatchMode=yes user@server-1 herdr --version
```

## add machines

On the head, add the head's own machine, then the others:

```sh
pastor machine add desk --local
pastor machine add server-1 user@server-1 --max-agents 3 --herdr
pastor machine list
```

`--max-agents 3` lets `server-1` run three tasks at once. `--herdr` also
saves it in herdr's sidebar on the head, so you can open it from there;
without the flag, `machine add` prints the `herdr machine add` command for
you. A running head picks the change up at once.

## read the load

```text
pastor 0.8.0 on desk (herdr 0.9.1), 2 machines, desk is the head of the flock

NAME      HOST           FLOCKS   PROFILE  CHANNEL    HERDR  PASTOR  AGENTS     ORPHANS  TAGS  ERROR
desk      local          default  -        connected  0.9.1  0.8.0   1/2+1j+1b  -        -
server-1  user@server-1  default  -        connected  0.9.1  0.8.0   2/3+1j+1b  -        -
```

AGENTS is the load: live agents against the machine's room. `2/3+1j+1b`
means two live, room for three, plus one slot only job tasks take and one
only a critical task may use. So `--max-agents 3` can mean five agents at
a busy moment; pass `--job-slots 0 --burst 0` for a hard three.

Each task goes to the machine with the fewest live tasks that still has
room. Only `connected` and `polling` machines take tasks. ERROR says why
one is not, such as an ssh failure or a herdr too old to talk to. ORPHANS
names agents called like a task (`t-12`) that no open task owns, such as
one left by a dispatch that failed halfway. They count against the room
until `pastor task close t-12` closes them. `pastor machine describe
server-1` shows one machine in full.

## tags

A tag marks what a machine has. Give it when you add the machine, and a
task that asks for tags only goes to a machine with all of them:

```sh
pastor machine add server-1 user@server-1 --tag gpu
pastor task run "Retrain the model on the new data" --repo '~/src/app' --tag gpu
```

For a machine you already added, set `tags = ["gpu"]` on its
`[[machine]]` in `~/.config/pastor/flock.toml` (`pastor flock edit` opens
it and checks it before saving).

## flocks, move and remove

Every machine starts in the default flock. To keep some tasks on some
machines, such as outside PRs on a sandbox with no credentials, make more
flocks (see [flocks](../../concepts/flocks/)) and put each machine where
it belongs:

```sh
pastor machine move server-1 sandbox  # out of every flock, into sandbox
pastor machine remove server-1  # out of flock.toml
pastor machine remove server-1 --herdr  # and out of herdr's sidebar
```

Tasks already on a machine stay there when you move it. A removed
machine's tasks keep their rows; a live one shows its machine as
`server-1 (removed)` in `task list`. `pastor flock join` puts a machine in
one more flock without taking it out of the others.

## let agents report back

An agent tells the head it is finished by running `pastor task done`. On
the head's own machine that goes over the local socket. On `server-1` it
goes over ssh, with a key that can only do what an agent should: list its
flocks' tasks, and read or end the tasks on its own machine.

1. On `server-1`, as the user the agents run as, make a key with no
   passphrase and make ssh use it for the head. Copy the `.pub` file to the
   head.
2. On the head, print the `authorized_keys` line for it:

   ```sh
   pastor machine authorized-key server-1 --key pastor_head.pub
   ```

3. Append the line to the head user's `~/.ssh/authorized_keys`. It forces
   `pastor bridge --agent --machine server-1`, which refuses anything else
   with `not_allowed_for_agent`.
4. In the head's `pastor.toml`, set `head_address` to the ssh destination
   the machines reach the head by. Agents off the head's machine get it as
   `PASTOR_HEAD`.

Give each machine its own key. Without any of this, pastor still sees the
agent go idle and marks the task `done`, keeping the pane's last lines in
place of a summary. A task run with `--summary require` fails instead.

## a machine the head cannot reach

A laptop behind a home router, or one that comes and goes, can take tasks
the other way round: its own `pastor serve` asks the head for work. See
[remote](../remote/#a-laptop-that-takes-tasks).
