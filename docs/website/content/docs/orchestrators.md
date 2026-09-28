---
title: orchestrators
summary: an agent that drives the others, started when there is judgment to do or kept running through set hours
group: use
weight: 22
manual: orchestrators
---

An orchestrator is an agent that drives the others: it merges what is ready,
sends fixes and answers what is blocked. pastor runs orchestrators from files,
so a night of work keeps moving without anyone starting, feeding or
restarting one.

## a scheduled orchestrator

One TOML file in `~/.config/pastor/orchestrators/`, named after the file. It
says its `kind`. A `scheduled` one runs a pre script on a schedule; the script
does the mechanical work itself and prints one line for each thing that needs
judgment. Only then does pastor start an agent, with every line in its prompt.

```toml
# ~/.config/pastor/orchestrators/merge.toml
kind = "scheduled"
cron = "*/5 22-23,0-7 * * *"     # or every = "5m"
pre = ["./merge-pre.sh"]         # relative to this file
post = ["./merge-post.sh"]       # optional: runs once the agent ends
timeout = "5m"                   # for pre and post each
model = "sonnet"
skill = "orchestrating-pastor"
prompt = "Decide what to do with each line below."
```

| key | does |
|---|---|
| `kind` | `scheduled` or `session`; required, and a key of the other kind makes the file invalid |
| `every`, `cron` | when the pre script runs (scheduled, exactly one) |
| `pre` | the script that looks and acts first (scheduled, required) |
| `post` | a script that gets the agent's end state, summary and the lines (scheduled) |
| `timeout` | how long each script may run; default `5m` |
| `model`, `skill`, `prompt` | the agent's model, the skill it is told to use, and its prompt |
| `repo` | the repo its agents work in, each in a worktree; default: the home directory |
| `enabled`, `description` | as on a job |
| `hours`, `stop_grace` | when a `session` runs (local `HH:MM`, may cross midnight), and how long its agent gets after the last message; default `5m` |

## a run

1. If the last run's agent still works, or its post script has not run yet,
   the run is skipped.
2. The pre script runs on the head, with `PASTOR_ORCHESTRATOR` set to the
   orchestrator's name and a scratch dir in `PASTOR_ORCHESTRATOR_STATE_DIR`.
   No lines: done, no agent. A failing script, or one printing more than
   64 KiB of lines, backs off, one minute doubling to an hour.
3. With lines, one agent starts on the head's own machine with the role
   `orchestrator`, the handover note and every line.
4. When that agent ends `done`, `failed` or `stale`, the post script runs once.

At most `max_orchestrators` orchestrator agents run at once (`1` by default,
in pastor.toml, outside `max_agents`); a run past it starts none. An agent that
stops on a quota error holds the next one back until the quota resets.

## a session orchestrator

A `session` orchestrator is one agent kept running through set hours, for
nights you want something watching all the time.

```toml
# ~/.config/pastor/orchestrators/night.toml
kind = "session"
hours = { start = "22:00", stop = "08:00" }
stop_grace = "5m"
skill = "orchestrating-pastor"
prompt = "You are the night orchestrator."
```

- The head starts it at `hours.start`, or at once if it finds itself inside
  the hours: the prompt, the skill, the handover note, and `pastor watch
  --now` to begin with.
- At `hours.stop` the agent gets a last message, and `stop_grace` later it is
  closed.
- If it dies, goes stale or ends its turn early, the head starts another with
  the note, at most three times an hour. On a quota error it waits for the
  reset read from the message, then restarts.
- It holds its `max_orchestrators` slot from start to stop. A scheduled run
  while it runs does its pre script and is held; a session due while a
  scheduled agent works starts once that agent ends.
- `pastor orchestrator start night` starts it now, `pastor orchestrator stop
  night` stops it until its next hours.

## the role

Every agent an orchestrator starts may run, retry, send to and close tasks,
enable and disable jobs, and keep its handover note, besides reading. The pre
and post scripts get the same table: a script's `pastor task run` works, its
`pastor machine add` is refused. It guards against mistakes, not against a
determined script.

## commands

```sh
pastor orchestrator list                 # kind, state, schedule, last and next run
pastor orchestrator describe merge       # settings, note, last runs with their lines
pastor orchestrator run merge            # one run now
pastor orchestrator start night          # a session now, to its next stop
pastor orchestrator stop night           # last message, grace, close
pastor orchestrator disable merge
pastor orchestrator note --name merge "merged #31; #32 waits on review"
```

More in the [manual](../manual/#orchestrators).
