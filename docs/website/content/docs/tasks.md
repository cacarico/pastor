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

`done` means the agent stopped, not that the work is right. Read the output.

## follow it

```sh
pastor task list
pastor task describe t-1
pastor events --follow
```

## answer it

A blocked agent is waiting for you. Read what it asked, then answer, or
attach to its terminal and take over.

```sh
pastor task read t-1
pastor task send t-1 "yes, push it"
pastor task attach t-1  # ctrl+b q to leave
```

## end it

pastor closes a done task's pane after 15 minutes (`close_done_after`).
Failed, stale and blocked tasks stay until you act.

```sh
pastor task close t-1 --remove-worktree
pastor task retry t-4
pastor task prune --done --older-than 3d
```
