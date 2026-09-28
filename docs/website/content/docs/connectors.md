---
title: connectors
summary: where a job's work comes from
group: use
weight: 22
manual: connectors
---

A connector is a small program that turns an outside source, such as GitHub
issues, into work items for a job. It can also run hooks when tasks change
state, to report back where the work came from.

## install one

`clock` is built in: one item per run, keyed by the run time, so the job's
schedule alone decides when an agent starts. Others install from GitHub, as
`owner/repo` or `owner/repo/subdir`:

```sh
pastor connector install cacarico/pastor-connectors/github-issues
pastor connector list  # version, mode, hooks, missing secrets
pastor connector describe github-issues
```

`install` shows what the connector will run and asks first; `--yes` skips
the question. A connector runs as your user on the head, so install only ones
you would run by hand. `describe` shows its origin, config keys, secrets and
the jobs that use it.

## use it in a job

A job names the connector by id, with the connector's own keys beside it:

```toml
# ~/.config/pastor/jobs/issues.toml
every = "10m"

[connector]
use = "github-issues"
repo = "owner/api"
label = "pastor"

[dispatch]
repo = "~/work/api"
worktree = true
branch = "pastor/issue-{{ item.key }}"
prompt = "Fix issue #{{ item.key }}: {{ item.title }}"
```

A job whose connector is missing, or lacks a required key, shows as invalid
in `pastor job list`. To see what a connector returns before you write the
job, run it once; it creates no tasks and saves nothing:

```sh
pastor connector try github-issues --job issues --since 1h
```

## hooks and finish

A connector can ship event hooks: commands the head runs on events such as
`task.done` or `task.blocked`, to comment somewhere or send a notification.
A `[finish]` command, if it has one, runs once when a task of its jobs ends
`done` or `failed`, to close the loop at the source: comment on the issue.
It gets the task, its branch, the pane's last lines and the task's
`summary` on stdin, so the comment can say how the work ended.

## secrets

Secrets and settings go in `~/.config/pastor/connectors/<id>/.env`, as
`KEY=value` lines. Every command of that connector gets them in its
environment, and the secrets its manifest declares are redacted from run
logs.

More in the [manual](../manual/#connectors).
