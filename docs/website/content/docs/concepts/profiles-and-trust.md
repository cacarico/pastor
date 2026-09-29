---
title: profiles and trust
summary: what an agent may do, and where
weight: 9
aliases:
  - /docs/trust/
---
An agent that pastor starts runs as the machine's user, with that user's
files, keys, tokens and network. A prompt is not always yours: a job fills
it from an issue, a repo holds READMEs and test output, and the agent may
fetch web pages. Any of these can carry text written to steer the agent.
This page covers what pastor gives you to limit that.

## permission profiles

A profile is a named pair of tool lists, `allow` and `deny`, in Claude
Code's patterns. Three are built in:

| profile | allows | denies |
|---|---|---|
| `review` | `Read`, `Glob`, `Grep`, and `git status`, `diff`, `log`, `show`, `blame` | `Edit`, `Write`, `NotebookEdit`, `git push`, and the destructive ones |
| `develop` | the read tools, `Edit`, `Write`, `NotebookEdit`, `Bash` | the destructive ones: `rm -rf`, `sudo`, `git push --force` |
| `unrestricted` | everything `develop` allows, plus `WebFetch` and `WebSearch` | nothing |

Add your own under `[profiles]`, or replace a built-in one of the same name.
`extends` starts from another profile, and a deny anywhere in the chain
always wins, so a profile can narrow the one it extends but never lift its
deny.

```toml
# ~/.config/pastor/pastor.toml
[profiles.ci]
description = "develop, plus docker"
extends = "develop"
allow = ["Bash(docker:*)"]

[defaults]
profile = "review"
```

```sh
pastor profile list          # name, where it comes from, what it extends
pastor profile describe ci   # the chain and the lists it adds up to
pastor task run "Add a Dockerfile" --repo '~/src/app' --profile ci
```

A task takes its profile from the first of: `--profile` or the job's
`profile`, its flock, the machine it runs on, `[defaults]`. With none it
runs no profile.

## what a profile does to the agent

Under a profile, a Claude agent starts with `--permission-mode dontAsk`
and the profile's lists. It never stops at a permission prompt: a tool the
lists and its own settings do not allow is refused, not asked about. So a
profiled task runs through without you, and the profile is the whole of
what it may do. Agent args that pick a permission mode themselves are
refused while a profile applies (`profile_args_conflict`).

An opencode agent gets the lists as opencode permissions in its pane's
environment. Any other agent gets them through its `allow_flag` and
`deny_flag` and keeps its own mode.

`unrestricted` denies nothing, so a task may ask for it only on a machine
whose own profile is `unrestricted` too. That choice belongs to whoever
owns the machine, in `flock.toml` or `pastor.toml`, not to a run flag or a
job file.

## without a profile

With no profile, pastor leaves the agent's permission mode alone. When
Claude asks before a tool, the task goes `blocked` until you answer it.
`allow` and `deny` lists in `[defaults]`, on a flock or in a job answer some
of those questions in advance. The lists add up across layers, and a deny
always wins.

```toml
# ~/.config/pastor/pastor.toml
[defaults]
allow = ["Read", "Edit", "Bash(git:*)"]
deny = ["WebFetch", "Bash(rm:*)"]
```

An `agent_args` entry such as `--dangerously-skip-permissions` removes every
check, and no list applies. Keep one to machines you could wipe, and never
use it for jobs fed by outside input. Read a blocked task's pane before you
answer it.

## agents and the flock

pastor sets `PASTOR_TASK` in every agent's pane. From there, the agent may
read everything and end its own task with `pastor task done`. Every command
that changes something is refused with `agent_refused`: running or closing
tasks, editing machines, flocks, jobs or config, `serve`, and the rest.
`agents_change_fleet = true` in `pastor.toml` turns the refusal off.

An [orchestrator](../orchestrators/) may also run, retry, send to and close
tasks, and enable and disable jobs. Only a person starts one.

This stops an agent acting on its own, not a determined one: it runs as the
same user and can unset the variable. A flock is a routing rule, not a
sandbox, so do not give a `local = true` machine untrusted work.

## folder trust

An agent in a folder it has not seen stops at its folder-trust prompt, and
a worktree is always a new folder. Answer it once with `--trust`: pastor
presses the agent's trust keys and saves the repo as trusted on that
machine. From then on the head answers that prompt for the repo's tasks
there, worktrees included, and logs `task.trusted`.

```sh
pastor task send t-12 --trust
pastor trust add server-1 '~/src/app'     # trust it before any task asks
pastor trust list
pastor trust remove server-1 '~/src/app'  # its next task asks again
```

## what pastor closes

pastor closes only the panes and workspaces it created. A done task's pane
closes after `close_done_after`, and its worktree goes only if it is clean:
no uncommitted changes and no unpushed commits. Failed, stale and blocked
tasks stay until you act.

Read on: [first run](../../start/first-run/) answers a folder-trust
prompt; profiles and lists are keys in
[pastor.toml](../../reference/pastor-toml/), and the commands in the
[cli reference](../../reference/cli/#trust).
