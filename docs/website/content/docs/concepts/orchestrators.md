---
title: orchestrators
summary: agents that watch other agents
weight: 10
aliases:
  - /docs/orchestrators/
---
An orchestrator is an agent that drives the others: it merges what is
ready, sends fixes and answers what is blocked. pastor runs orchestrators
from files, so a night of work keeps moving without anyone starting,
feeding or restarting one.

Each file in `~/.config/pastor/orchestrators/` is one orchestrator, named
after the file, and says its `kind`:

- `scheduled`: a pre script runs on a schedule and does the mechanical work
  itself. pastor starts an agent only when the script prints something
  that needs judgment. Most runs start no agent, and each agent is short.
- `session`: one agent kept running through set hours, for nights you want
  something watching all the time.

## a scheduled orchestrator

```toml
# ~/.config/pastor/orchestrators/merge.toml
kind = "scheduled"
cron = "*/5 22-23,0-7 * * *"  # or every = "5m"
pre = ["./merge-pre.sh"]      # relative to this file
post = ["./merge-post.sh"]    # optional: runs once the agent ends
model = "sonnet"
skill = "orchestrating-pastor"
prompt = "Decide what to do with each line below."
```

A run goes like this:

1. If the last run's agent still works, or its post script has not run
   yet, the run is skipped.
2. The pre script runs on the head, with `PASTOR_ORCHESTRATOR` set to the
   orchestrator's name and a scratch dir, kept between runs, in
   `PASTOR_ORCHESTRATOR_STATE_DIR`. It merges, rebases and sends fixes on
   its own, and prints one line on stdout for each thing it cannot decide.
3. No lines: the run is over, and no agent starts. A script that fails, or
   runs past `timeout` (default `5m`), backs off, one minute doubling to an
   hour.
4. With lines, one agent starts with the `orchestrator` role, the prompt,
   the skill, the handover note and every line.
5. When that agent ends `done`, `failed` or `stale`, the post script runs
   once, with the agent's end state, its summary and the lines on stdin.

## a session orchestrator

```toml
# ~/.config/pastor/orchestrators/night.toml
kind = "session"
hours = { start = "22:00", stop = "08:00" }  # local time; may cross midnight
stop_grace = "5m"
skill = "orchestrating-pastor"
prompt = "You are the night orchestrator."
```

- The head starts it at `hours.start`, or at once if it starts inside the
  hours. The agent begins with `pastor watch --now` and keeps watching.
- At `hours.stop` the agent gets a last message, and `stop_grace` later it
  is closed.
- If it dies, goes stale or ends its turn early, the head starts another
  with the handover note, at most three times an hour. After a quota error
  it waits for the reset the message names.

## where it runs

Everything an orchestrator runs, its scripts and its agent, runs on the
head's own machine: the `local = true` one in `flock.toml`, which must
exist. The agent takes one of that machine's slots like any task.
`max_orchestrators` in `pastor.toml` (default 1) caps how many orchestrator
agents run at once on top of that. A session holds its place from start to
stop; a scheduled run that finds the cap reached starts no agent, and the
next run tries again.

`prompt` is required. `repo` gives the agent a repo to work in, in a
worktree of its own; without it the agent starts in `~/pastor-tasks`, like
any task with no repo. `enabled` and `description` work as on a job.

## what it may do

An orchestrator's agent, and its pre and post scripts, may run, retry, send
to and close tasks, enable and disable jobs, keep its own handover note,
and read everything. Anything else is refused: adding machines, editing
files, starting other orchestrators. Only a person starts an orchestrator,
from a file or with `pastor task run --role orchestrator`.

The refusal guards against mistakes, not against a determined script:
everything runs as the head's user. See
[profiles and trust](../profiles-and-trust/#agents-and-the-flock).

## commands

```sh
pastor orchestrator list                 # kind, state, schedule, last and next run
pastor orchestrator describe merge       # settings, note, last runs with their lines
pastor orchestrator run merge            # one scheduled run now
pastor orchestrator start night          # a session now, to its next stop
pastor orchestrator stop night           # last message, grace, close
pastor orchestrator disable merge        # no more runs; a running agent keeps going
pastor orchestrator note --name merge "merged #42; #43 waits on review"
```

Orchestrator tasks show in `pastor task list` in a table of their own,
above the other tasks.

Read on: [keep PRs moving overnight](../../examples/overnight/) is a whole
night watch, pre script included, and
[let your agent drive pastor](../../examples/agent-drives/) covers the
skill; the commands are in the [cli reference](../../reference/cli/#orchestrator).
