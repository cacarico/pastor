---
title: let your agent drive pastor
summary: the skill, JSON, and orchestrators
weight: 8
---
Running commands by hand is cool, but pastor is even more powerful in the hands of agents.

Ask the coding agent in your own terminal what your flocks are doing, and it
reads the tasks, sums them up and answers the one that waits on you. The
agents pastor starts use the same CLI to report how their work went. This
page shows what to give your agent and where pastor draws the line.

## give it the skill

pastor carries a guide for coding agents: what pastor is, how to run and
follow tasks, what each state means, and what a task agent is expected to
do. `pastor --skill` prints the copy that matches the installed version.
Save it where your agent loads skills, and again after each upgrade:

```sh
mkdir -p ~/.claude/skills/pastor
pastor --skill > ~/.claude/skills/pastor/SKILL.md
```

An agent that meets pastor without it gets a pointer anyway: `pastor --help`
ends with one.

## read with JSON

Tables are for people. Every `list` and `describe` takes `--json`, and so do
`pastor queue`, `pastor events` and `pastor serve status`, so an agent reads
ids and states instead of guessing them:

```sh
pastor task list --blocked --json
pastor task describe t-12 --json
pastor queue --json
```

A command that fails prints one JSON object on stderr and exits 1, so the
agent can act on the code:

```text
{"code":"unknown_model","message":"..."}
```

## agents pastor started

Every agent pastor starts has `PASTOR_TASK` set in its pane, and pastor
treats commands from there as that task's.

| an agent may | an agent may not |
|---|---|
| read: `task list`, `describe`, `read`, `queue`, `events`, `machine list`, `job list` | run, retry, send to, attach to or close tasks |
| end its own task: `pastor task done --summary ...` | change a task's priority or place in the queue |
| | tick (dry runs too), run or reload jobs, edit machines, flocks, jobs or `pastor.toml` |
| | install or link connectors, start or set up a head |

A refused command fails with `agent_refused`, and the message lists what is
off limits. `agents_change_fleet = true` in `pastor.toml` lifts the rule.
It guards against an agent's mistakes, not against a determined one: the
agent runs as your user. See [profiles and trust](../../concepts/profiles-and-trust/#agents-and-the-flock).

## an orchestrator agent

An agent started with the role `orchestrator` may do more: run, retry, send
to and close tasks, enable and disable jobs, and keep its handover note.
Only a person starts one, never a task:

```sh
pastor task run --role orchestrator "Watch the flocks until 8: retry what fails once, answer what blocks, leave a note"
```

An [orchestrator file](../../concepts/orchestrators/) starts them on a
schedule instead; [keep PRs moving overnight](../overnight/) is one.

## a session

Your agent, in your own terminal, asked what waits on you:

```text
> Anything in pastor waiting on me?

⏺ Bash(pastor task list --blocked --json | jq -r '.[] | "t-\(.id) \(.machine) \(.description)"')
  ⎿  t-12 server-1 Check out PR #42 with gh pr checkout 42, address every review comment, push

⏺ Bash(pastor task read t-12 --lines 15)
  ⎿  ⏺ 1 and 3 are done. For 2: should an empty config file be an error,
       or the same as no file?

⏺ t-12 is fixing PR #42 and asks one thing: should an empty config file be
  an error, or the same as no file?

> same as no file

⏺ Bash(pastor task send t-12 "Same as no file: fall back to the defaults" --json)
  ⎿  {"message":"sent text and 1 key to t-12"}
```

And the agent of `t-12` itself, which pastor started, as it finishes:

```text
⏺ Bash(pastor task run "Also bump the lockfile" --repo '~/src/app')
  ⎿  Error: {"code":"agent_refused","message":"t-12 is an agent pastor started, and agents may not change the fleet (run, send to, attach to, ...); set agents_change_fleet = true in pastor.toml to allow it"}

⏺ Bash(pastor task done --summary "done: fixed the 3 review comments on PR #42 and pushed")
  ⎿  ID    STATE  PRIORITY  MACHINE   FLOCK    AGENT   MODEL   JOB  AGE  NOTE
     t-12  done   normal    server-1  default  claude  sonnet  run  12m  Check out PR #42 with gh pr checkout 42, address every revie
```

The summary's first line is the outcome: `done`, `partial`, `blocked` or
`nothing to do`. `pastor task describe t-12` shows it, and so does the
`task.done` event.

## next

- [tasks](../../concepts/tasks/#how-it-ended) and [orchestrators](../../concepts/orchestrators/)
- [profiles and trust](../../concepts/profiles-and-trust/)
- [cli](../../reference/cli/#shared-behaviour) and [events](../../reference/events/)
