---
title: connectors
summary: small programs that find work
weight: 7
aliases:
  - /docs/connectors/
---
A connector is a small program that finds work for a [job](../jobs/): it
turns an outside source, such as GitHub issues or a Kanban board, into
items, and each new item becomes a task. It can also report back where the
work came from when the task changes state.

## clock, and installing others

`clock` is built in. It hands over one item per run, keyed by the run time,
so the job's schedule alone decides when an agent starts. Every other
connector is installed from GitHub, as `owner/repo` or `owner/repo/subdir`:

```sh
pastor connector install cacarico/pastor-connectors/github-issues
pastor connector list                     # version, mode, hooks, missing secrets
pastor connector describe github-issues   # origin, commands, config keys, secrets, jobs
```

`install` shows what the connector will run and asks first; `--yes` skips
the question and `--ref` picks a branch, tag or commit. To work on a
connector of your own, `pastor connector link` uses a local directory in
place. A connector is a directory with a `pastor-connector.toml` manifest
and the commands it names.

## what its commands do

A connector can ship any mix of four commands. pastor runs each one as
your user, with the connector's `.env` in its environment.

| part | runs | for |
|---|---|---|
| `[connector]` | on each job run, or once as a long-lived stream | prints items as JSON lines, each with a `key` |
| `[[events]]` | on the events it lists, such as `task.blocked` | a hook: comment somewhere, send a notification |
| `[finish]` | once, when a task of its jobs ends `done` or `failed` | close the loop at the source, such as a comment on the issue |
| `[watch]` | on each `pastor watch` interval | lines pastor cannot know itself, such as the state of a PR |

The finish command gets the task, its branch, the pane's last lines and the
task's summary on stdin, so the comment can say how the work ended.

## use it in a job

A job names the connector by id, with the connector's own keys beside it:

```toml
# ~/.config/pastor/jobs/issues.toml
every = "10m"

[connector]
use = "github-issues"
repo = "owner/app"
label = "pastor"

[dispatch]
repo = "~/src/app"
worktree = true
branch = "pastor/issue-{{ item.key }}"
prompt = "Fix issue #{{ item.key }}: {{ item.title }}"
```

To see what a connector returns before you write the job, run it once. It
creates no tasks and saves no cursor:

```sh
pastor connector try github-issues --job issues --since 1h
```

A job whose connector is missing, or lacks a key the manifest marks
required, shows as invalid in `pastor job list`.

## secrets

Secrets and settings go in `~/.config/pastor/connectors/<id>/.env`, as
`KEY=value` lines. Every command of that connector gets them in its
environment, and the secrets its manifest declares are redacted from run
logs.

A connector is not sandboxed. It runs as the head's user and inherits the
head's environment and files, other connectors' `.env` files included.
Install only connectors you would run by hand.

Read on: [board cards](../../examples/board-cards/) hands Kanban cards to
agents through a connector; the job side is in
[job files](../../reference/job-files/), and the commands in the
[cli reference](../../reference/cli/#connector).
