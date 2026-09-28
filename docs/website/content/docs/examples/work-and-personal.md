---
title: work and personal apart
summary: two flocks, two accounts
weight: 6
---
One head runs your personal projects and your day job. Each side has its
own machines and its own Claude account, so a work task never runs on a
personal machine or bills the personal account, and the other way round.
Two flocks do it: a task goes only to machines of its flock, and each flock
starts its own agent.

## what you need

- A second Claude login for work. Claude keeps its login in
  `CLAUDE_CONFIG_DIR`, so the work account lives in `~/.claude-work` on
  `laptop`. Log in there once with `CLAUDE_CONFIG_DIR=~/.claude-work claude`.
- `~/src/app` cloned on the machines of each flock that works on it.

## the work agent

An agent under `[agents]` is a name for an agent kind with its own
settings. This one is Claude, started with the work login:

```toml
# ~/.config/pastor/pastor.toml
[agents.claude-work]
kind = "claude"
env = { CLAUDE_CONFIG_DIR = "~/.claude-work" }
```

pastor sets `env` in the task's pane, and a leading `~/` means the home of
the machine that runs the task. `kind = "claude"` keeps Claude's trust keys
and tool flags.

## the flocks

```toml
# ~/.config/pastor/flock.toml
[[flock]]
name = "personal"
default = true
machines = { desk = 2, server-1 = 3 }

[[flock]]
name = "work"
agent = "claude-work"
machines = { laptop = 2 }

[[machine]]
name = "desk"
local = true

[[machine]]
name = "server-1"
ssh = "user@server-1"
max_agents = 3

[[machine]]
name = "laptop"
ssh = "user@laptop"
```

`machines` names each flock's machines and at most how many of the flock's
tasks each runs. `personal` is the default: a task or job that names no
flock goes there, and gets plain `claude`. `work` tasks get `claude-work`.
A machine can be in both flocks, with a number in each; there too, the
task's flock picks the agent, and so the account.

## run on each side

```sh
pastor task run "Fix the login redirect" --repo '~/src/app' --worktree --flock work
pastor task run "Add paging to the orders page" --repo '~/src/app' --worktree  # personal
```

A job sends its tasks to a flock with `flock = "work"` in `[dispatch]`.
Leave `agent` out of the job: a job's agent overrides the flock's.

## what you see

```sh
pastor flock list
pastor task list
```

```text
NAME      DEFAULT  MACHINES                AGENTS  QUEUED
personal  yes      desk 0/2, server-1 1/3  1       0
work      no       laptop 1/2              1       0
```

```text
ID    STATE    PRIORITY  MACHINE   FLOCK     AGENT        MODEL  JOB  AGE  NOTE
t-26  running  normal    server-1  personal  claude       -      run  1m   Add paging to the orders page
t-25  running  normal    laptop    work      claude-work  -      run  2m   Fix the login redirect
```

MACHINES shows each machine's live tasks of that flock over its number
there. `pastor task list --flock work` and `pastor queue --flock work` show
one side only.

If one machine's plain `claude` is already logged in to the other account,
give that machine an agent of its own: `agent = "claude-home"` on its
`[[machine]]`, with a matching `[agents.claude-home]`. A machine's agent
comes before its flock's.

## next

- [flocks](../../concepts/flocks/) and [agents and models](../../concepts/agents-and-models/)
- [flock.toml](../../reference/flock-toml/) and [pastor.toml](../../reference/pastor-toml/)
- [a fleet over ssh](../../deploy/fleet/)
