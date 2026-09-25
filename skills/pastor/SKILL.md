---
name: pastor
description: "Drive pastor, a daemon that runs coding agents on a fleet of machines through herdr. Use when the user mentions pastor; when you have to run, watch or manage coding agents on other machines through pastor (dispatch a task, check its state, read its output, schedule a job, add a machine); or when you were yourself started by pastor as task t-N inside a herdr pane and need to know what is expected of you."
---

# pastor

pastor runs coding agents on machines the user owns. One machine runs the head, `pastor serve`, which talks over ssh to the flock: machines listed in `flock.toml`, each running a herdr server. A task is one agent in one herdr pane, optionally in its own git worktree; a job is a TOML file that queues tasks on a schedule.

## Learn the current CLI

The installed binary is the authority for syntax. This guide is for orientation; check a command's help before relying on a flag:

```bash
pastor --help
pastor task --help
pastor job --help
pastor machine --help
pastor events --help
pastor setup --help
```

Most read commands take `--json` (`pastor task list`, `pastor task show`, `pastor machine list`, `pastor job list`, `pastor events`). Use it, and read task ids, machines and states from the output instead of predicting them.

Runtime errors are one JSON object on stderr, `{"code": ..., "message": ...}`, with exit 1. A malformed command line is clap usage text with exit 2.

## Is a head running

```bash
pastor machine list
```

With a head running, it prints one row per machine: `NAME`, `CHANNEL` (`connected`, `polling`, `connecting`, `reconnecting`, `incompatible`), `HERDR` (the herdr version), `PASTOR` (the pastor installed there, `-` when unknown), `AGENTS` (live tasks over `max_agents`), `TAGS`, `ERROR`. Only `connected` and `polling` machines take tasks. With no head running, it says so on stderr and probes each machine directly instead: CHANNEL is then `probed` (herdr answered), `server down` (a local socket with no server), `unreachable` (ssh or transport failed) or `error` (herdr answered the ping with an error), and HERDR, PASTOR and AGENTS come from that probe. Report what it shows rather than starting `pastor serve` yourself, which is a long-running daemon.

```bash
pastor task list          # live tasks: queued, starting, running, blocked
pastor task list --all    # finished ones too: done, failed, stale, closed
pastor task list --blocked --json
```

`pastor task list` falls back to the database when the head is down and says so on stderr.

## Run a task

```bash
pastor task run "<prompt>" --machine pi-3 --agent claude \
  --agent-arg --model --agent-arg claude-opus-5-5 \
  --repo '~/work/api' --worktree --branch fix/flaky-ci --json
```

- `--machine M` pins the task. `--tag T` (repeatable) instead restricts it to machines that carry every given tag in `flock.toml`; it is a filter, not a label on the task. With neither, any machine with a free slot takes it.
- `--repo` is a path on the machine that runs the agent. Quote a leading `~` so your shell does not expand it. pastor checks that it is a directory on that machine before it creates anything (`test -d` over ssh); a missing repo fails the task with `repo <path> does not exist on <machine>`. A `command` machine cannot be checked.
- `--worktree` makes a git worktree of `--repo` for the task, on `--branch` or `pastor/t-N`. It needs `--repo` and the repo cloned on that machine.
- `--agent-arg` passes one argument to the agent and always takes the next word, dashes included. Repeat it, in order.
- `--timeout` bounds the task; past it the task goes `stale`.

pastor sends the prompt as is, and the agent knows nothing else. Write it so the agent can finish alone: say where it is (its own worktree and branch), what to change, what to commit and where to push, what report to write, and to print `DONE` as its last line.

Then watch it:

```bash
pastor task show t-12 --json   # one task, with its error and agent args
pastor task read t-12          # recent pane output; --lines N for more
pastor events --task t-12      # what happened to it, and when
```

`pastor task attach t-12` puts a human terminal into the agent's pane (ctrl+b q detaches). It needs a real terminal; as an agent, use `pastor task read`.

States:

- `queued`: accepted, no machine has a free slot or the tag yet.
- `starting`: pastor is creating the workspace and starting the agent.
- `running`: the agent is working.
- `blocked`: the agent is waiting on a permission prompt or question. Nobody answers it unless a human attaches.
- `done`: the agent went idle after pastor saw it work, and stayed idle for `settle` (10s by default). It means the agent stopped, not that the work is good; read the output.
- `stale`: the timeout passed without `done`. The agent is left running.
- `failed`: dispatch failed or the agent exited before it was done. `task show` has the error.
- `closed`: the pane of a done task went away.

pastor never closes panes and never removes worktrees on its own in this version. Finished tasks keep their pane and worktree until a human cleans them up. Do not do it for them unless asked.

## Jobs

A job is one TOML file under `~/.config/pastor/jobs/`, named after the file:

```toml
every = "1h"                 # or cron = "*/30 9-18 * * 1-5", local time
[connector]
use = "clock"                # the only connector in this version
[dispatch]
repo = "~/work/api"
tags = ["arm"]
prompt = "It is {{ item.key }}. Run the suite and fix what broke. Task {{ task.id }}."
```

`[dispatch]` takes the same things as `pastor task run`: `agent`, `agent_args`, `repo`, `worktree`, `branch`, `tags`, `machine`, `timeout`, plus `max_tasks_per_run` and the `prompt` template.

```bash
pastor job list            # schedule, enabled, last and next run, errors
pastor job run hourly      # fire now, ignoring the schedule
pastor job enable hourly
pastor job disable hourly
pastor job reload          # re-read the files now instead of at the next tick
pastor tick --dry-run --job hourly   # what a run would create, creating nothing
```

The head picks up job file edits by itself. A file that stops parsing keeps its last good version and shows the error in `pastor job list`. It also re-reads `flock.toml` and `pastor.toml`, so `pastor machine add`, `pastor machine remove` and config edits need no restart (`pastor tick` reloads them too); one of those that stops parsing also keeps its last good version, but reports it in the daemon log only (`journalctl --user -u pastor`), not in `pastor job list`.

## The fleet

`~/.config/pastor/flock.toml` holds one `[[machine]]` per machine: `name`, exactly one of `ssh = "user@host"`, `local = true` or `command = [...]` (for tests), `session` (the herdr session, default `default`), `max_agents` (default 2) and `tags`.

```bash
pastor machine add pi-3 fleet@pi-3 --max-agents 2 --tag arm --herdr
pastor machine add here --local
pastor machine remove pi-3 --herdr
pastor machine list        # connects to each machine now: ssh, herdr version, agents
```

Every machine needs a herdr server running, and the head needs passwordless ssh to it. Start herdr with `herdr server`, or better as a user service: `pastor setup systemd --herdr --yes` on that machine. The head itself runs as a service with `pastor setup systemd --yes`. Setup needs `--yes` when stdin is not a terminal. Ask the user before installing services.

## Events

```bash
pastor events                  # every task, job and machine event, oldest first
pastor events --task t-12 --json
timeout 60 pastor events --follow   # --follow never returns on its own; always bound it
```

It reads the log file, so it works with the head down.

## When pastor dispatched you

You are a pastor task when `HERDR_ENV=1` is set, your herdr agent and workspace are named `t-N`, and the prompt reads like a self-contained work order. Then:

- Work only in the directory you started in: your worktree and branch. Never touch other worktrees, branches or panes.
- Commit and push exactly as the prompt says, and write the report it asks for.
- Do not ask questions. Nobody is watching; a permission prompt or a question leaves the task `blocked` until a human happens to attach. If something is missing, say so in your report and stop.
- Print `DONE` as your last line when finished, then go idle.
- Do not close your pane or exit to clean up. That is the user's job.

## When something goes wrong

- A task stays `queued`: no connected machine carries all its tags or has a free slot. Compare `pastor machine list` with the task's tags. The head logs a warning after an hour.
- `failed` with `agent_pane_busy`: herdr refused to start the agent because the pane was not at an idle shell prompt.
- `failed` with "agent t-N not found": the agent's pane vanished before it was done (closed by hand, herdr restarted, or the agent crashed).
- `failed` soon after start: the agent is usually not installed, or not on the PATH herdr sees on that machine.
- `blocked` right after start: often Claude's "trust this folder" dialog on a repo that machine has not seen. A human has to answer it once with `pastor task attach`.
- A machine `polling`: herdr answers requests but its event stream will not open. It still takes tasks; pastor checks them every tick and keeps trying to subscribe.
- A machine `reconnecting`: herdr is unreachable. Its tasks are reconciled when it comes back.
- A `timeout` error from the CLI: the head may still carry the request out. Check `pastor task list` before sending it again, or you may queue a duplicate.
