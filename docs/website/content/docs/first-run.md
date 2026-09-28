---
title: first run
summary: two machines, one task, one answer
group: start
weight: 11
manual: try-it
---

A guided first session: add two machines, start the head, run one task and
answer it. It assumes pastor and herdr are installed.

## add machines

The head can take tasks too. Add it as a local machine, then any machine
you can ssh to without a password.

```sh
pastor machine add here --local
pastor machine add pi-1 user@pi-1 --max-agents 2
```

`--herdr` also saves the machine in herdr's sidebar. Each machine runs the
herdr server; `pastor setup systemd --herdr` on it keeps one running.

## start the head

```sh
pastor setup systemd  # setup launchd on macOS
pastor machine list
```

Setup shows what it will install and asks before it does. `machine list`
opens with a line about the head, then one row per machine. Only
`connected` and `polling` machines take tasks.

## run a task

```sh
pastor task run "Fix the flaky test in ci.yml" --repo '~/work/api' --worktree
```

Quote the `~`: `--repo` is a path on the machine that runs the agent, and
your shell would expand it to this machine's home. `--worktree` gives the
agent a git worktree of its own.

## check on it

```sh
pastor task list
pastor task read t-1
pastor events --follow
```

## answer it

A task that shows `blocked` is waiting for you. A worktree is a folder the
agent has not seen, so Claude stops first at its folder-trust prompt.

```sh
pastor task send t-1 --trust  # accept it, and trust this repo on its machine from now on
pastor task send t-1 "yes, go ahead"
pastor task attach t-1  # sit in its terminal; ctrl+b q detaches
```

`done` means the agent stopped, not that the work is right. Read the output
before you trust it.

More in the [manual](../manual/#try-it).
