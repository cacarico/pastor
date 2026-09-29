---
title: tasks
summary: one prompt, one agent, one machine
weight: 4
aliases:
  - /docs/tasks/
---
A task sends one prompt to one agent. pastor picks a machine with room,
starts the agent there in a herdr pane, and tracks it until it finishes or
needs you. It is the unit everything else is built on: a job makes tasks,
and an orchestrator runs them.

## run one

```sh
pastor task run "Fix the flaky test in ci.yml" --repo '~/src/app' --worktree
```

Quote the `~`: `--repo` is a path on the machine that runs the agent, not on
yours. `--worktree` gives the agent its own git worktree, on a branch
`pastor/t-N`, so two agents never edit the same checkout. A task with no
repo starts in `~/pastor-tasks` on its machine.

| flag | does |
|---|---|
| `--repo` | the repo, as a path on the machine that runs the task |
| `--worktree` | a git worktree of its own, branched from `--repo` |
| `--flock` | only this flock's machines take it |
| `--machine` | run it on this machine instead of any with room |
| `--priority` | `low`, `normal`, `high` or `critical`; see [queue](../queue/) |

The agent, its model and its permission profile come from the flock, the
machine or `[defaults]` unless you pass `--agent`, `--model` or `--profile`
(see [agents and models](../agents-and-models/) and
[profiles and trust](../profiles-and-trust/)). Every flag is in the
[cli reference](../../reference/cli/#task).

herdr's sidebar shows the task's workspace as `default/t-12`, its flock and
then the task, so tasks of many projects on one machine are easy to tell
apart. `--label` changes that name.

## states

| state | means |
|---|---|
| `queued` | waiting for a machine with room; see [queue](../queue/) |
| `starting` | pastor is opening the pane and starting the agent |
| `running` | the agent is working |
| `blocked` | the agent waits for you: a permission prompt, a question, or folder trust |
| `done` | the agent stopped after working, or said it was done |
| `stale` | it ran past its timeout (2h by default), or its pull machine was lost; the agent is left running |
| `failed` | it could not start, or the agent exited before it was done |
| `paused` | a critical task took its slot; it resumes its session when there is room |
| `closed` | finished for good; its pane is gone |

`done` means the agent stopped, not that the work is right. Read what it
did.

## follow it

```sh
pastor task list             # live tasks: queued, starting, running, blocked, paused
pastor task list --all       # finished ones too
pastor task list --wide      # adds how each ended (RESULT) and its description
pastor task describe t-12    # one task in full: agent, model, profile and where each came from
pastor task read t-12        # the last lines of its pane
pastor events --follow       # every change, as it happens
pastor watch                 # one line per thing to act on
```

## answer it

A blocked agent is waiting for you. Read what it asked, then answer it, or
attach to its pane and take over.

```sh
pastor task read t-12
pastor task send t-12 "yes, push it"
pastor task send t-12 --trust  # accept its folder-trust prompt
pastor task attach t-12        # ctrl+b q to leave
```

`task send` also works on a `done` task whose pane is still open. It goes
back to `running`, with its context, so an agent that stopped too early can
be told to finish.

## how it ended

An agent says how its work went as it finishes. The first line of its
summary is the outcome: `done`, `partial`, `blocked` or `nothing to do`.

```sh
pastor task done --summary "done: pushed pastor/t-12, PR #42"
pastor task done --summary-file notes.md  # - reads stdin
```

pastor asks for it: each prompt it sends ends with a line asking the agent
to run `pastor task done --summary-file -`. Each round of a task (the
prompt, or a `task send` that reopened it) keeps one summary. A round that
ends without one keeps the last lines of the pane instead.
`pastor task describe` shows the last round's, `--all-summaries` every
round's. The same summary reaches `task list --wide`, the `task.done` and
`task.failed` events, `pastor watch` and a connector's finish command.

The `summary` setting, per task (`--summary`), job, flock or `[defaults]`,
says how hard pastor asks:

| value | does |
|---|---|
| `ask` | the default: the line is added to the prompt |
| `require` | the line is added, and the task fails if its agent stops without a summary |
| `off` | nothing is added and nothing required |

## end it

pastor closes a done task's pane after 5 seconds (`close_done_after` in
`pastor.toml`), and removes its worktree if it is clean: no uncommitted
changes and no unpushed commits. Nothing else closes on its own.

```sh
pastor task close t-12 --remove-worktree  # close it now, worktree too
pastor task retry t-14                    # a failed or stale task, again as a new task
pastor task prune --done --older-than 3d  # delete old finished rows; their items stay seen
```

A done task holds its machine's slot until its pane closes, and a stale one
until you close it. A failed task holds none. A closed Claude task can still
be reopened: `pastor task attach` resumes its session in a new pane.

Read on: [PR fix round](../../examples/pr-fix-round/) runs and follows a
task end to end; every flag is in the [cli reference](../../reference/cli/#task).
