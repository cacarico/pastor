---
title: tasks
summary: one prompt, one agent, one machine
group: use
weight: 20
manual: how-it-works
---

A task sends one prompt to one agent. pastor picks a free machine, starts the
agent there in a herdr terminal, and tracks it until it finishes or needs you.

## run one

```sh
pastor task run "Fix the flaky test in ci.yml" --repo '~/work/api' --worktree
```

`--worktree` gives the agent its own git worktree, so two agents never edit
the same checkout.

| flag | does |
|---|---|
| `--repo` | the repo, as a path on the machine that runs the task |
| `--worktree` | a git worktree of its own, branched from `--repo` |
| `--machine` | run on this machine instead of any free one |
| `--flock` | only machines in this flock take it |
| `--agent` | the agent command: `claude`, `codex`, ... |
| `--timeout` | mark it stale after `30m`, `2h`, ... |
| `--priority` | `low`, `normal`, `high` or `critical`: higher levels leave the queue first |
| `--preempt` | critical only: on a full machine, pause a `low` Claude task and take its slot |
| `--summary` | `ask` (default), `require` or `off`: whether the prompt asks for a summary, and whether the task fails without one |
| `--description` | one line on what it is about; default: the prompt's first line |
| `--label` | the name of its herdr workspace; default: `{{ flock }}/{{ task.id }}` |

## name its workspace

herdr's sidebar shows each task's workspace as `personal/t-285`: its flock,
then the task. With many projects on one machine, that tells them apart.
Change it with a template, per task, job, flock or in `[defaults]`:

```sh
pastor task run "Fix the login page" --repo '~/work/web' --label '{{ machine }}/{{ task.id }}'
```

A template takes `{{ task.id }}`, `{{ flock }}`, `{{ machine }}`, `{{ job }}`
and `{{ item.key }}`. Only the workspace is renamed: the agent is still
`t-285`, and a task that joins a workspace leaves its name alone.
`pastor task describe` shows the label and where it came from.

## states

| state | means |
|---|---|
| `queued` | waiting for a machine with a free slot |
| `starting` | pastor is opening the terminal and starting the agent |
| `running` | the agent is working |
| `blocked` | the agent is waiting for you: a permission prompt or a question |
| `done` | the agent stopped after working |
| `stale` | it ran past its timeout; the agent is left running |
| `failed` | it could not start, or the agent exited before it was done |
| `closed` | finished for good |
| `paused` | a critical task took its slot; it resumes its session when there is room |

`done` means the agent stopped, not that the work is right. Read the output.

## wait your turn

When every machine is full, tasks wait in the queue, by level and then in
the order they came. `pastor queue` shows it in the order it will run, how
long each task has waited and why it has not started yet.

```sh
pastor queue
pastor queue move t-8 --top          # first, lifted to the first task's level
pastor queue move t-8 --before t-5   # just ahead of t-5
pastor task priority t-8 high        # another level, same place in it
```

A moved task takes the level of where it lands: in front of a higher task
it is lifted, behind a lower one it is lowered. Levels do not age, so a
`low` task can wait for ever; WAITED shows you.

A `critical` task started with `--preempt` does not wait behind `low`
work. On a full machine it pauses the newest running `low` Claude task
there: its agent is interrupted, its pane closed, its worktree kept. The
paused task goes first among the `low` ones and resumes its own session
on the same machine when a slot frees.

```sh
pastor task run --priority critical --preempt "prod is down: find out why"
```

## follow it

```sh
pastor task list
pastor task list --wide  # adds each task's RESULT and description
pastor task describe t-1
pastor events --follow
```

A PR fix round, run and followed this way, is in the [examples](../examples/).

## answer it

A blocked agent is waiting for you. Read what it asked, then answer, or
attach to its terminal and take over.

```sh
pastor task read t-1
pastor task send t-1 "yes, push it"
pastor task attach t-1  # ctrl+b q to leave
```

## how it ended

An agent says how its work went as it finishes. The first line of the
summary is the outcome: `done`, `partial`, `blocked` or `nothing to do`.

```sh
pastor task done --summary "done: pushed pastor/t-4, PR #31"
pastor task done --summary-file notes.md   # - reads stdin
```

Each round (the prompt, or a `task send` that reopened the task, up to
`done` or `failed`) keeps one summary, up to 2,000 characters. A round that
ends without one keeps `no summary` and the last lines of the pane instead.
`pastor task describe` shows the last round's, `--all-summaries` every
round's; `task list --wide` shows the outcome as RESULT, and `--json` has
`summary`. The `task.done` and `task.failed` events carry it, `pastor watch`
prints `outcome=` on their lines, and a connector's finish command gets it
on stdin.

pastor asks for it: every prompt it sends ends with a line asking for
`pastor task done --summary-file -` in that shape, and a `task send` that
reopens a done task asks again. The line is not stored in the task's
prompt. The `summary` setting changes that:

| value | does |
|---|---|
| `ask` | the default: the line is added |
| `require` | the line is added, and a summary is a condition of success: the agent's own bare `task done` is refused, and an agent that stops without one ends the task `failed` ("stopped without a summary"). Your `task done t-N` still passes. |
| `off` | nothing is added and nothing required |

Set it with `task run --summary`, in a job's `[dispatch]`, on a flock or in
`[defaults]`; the most specific wins, and `task describe` shows what a task
got.

## end it

pastor closes a done task's pane after 15 minutes (`close_done_after`).
Failed, stale and blocked tasks stay until you act.

```sh
pastor task close t-1 --remove-worktree
pastor task retry t-4
pastor task prune --done --older-than 3d
```
