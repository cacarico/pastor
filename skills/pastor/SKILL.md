---
name: pastor
description: "Drive pastor, a daemon that runs coding agents on a fleet of machines through herdr. Use when the user mentions pastor; when you have to run, watch or manage coding agents on other machines through pastor (dispatch a task, check its state, read its output, schedule a job, add a machine); or when you were yourself started by pastor as task t-N inside a herdr pane and need to know what is expected of you."
---

# pastor

pastor runs coding agents on machines the user owns. One machine runs the head, `pastor serve`, which talks over ssh to the machines listed in `flock.toml`, each running a herdr server. Every machine is in one flock, a named group; a task goes only to machines of its flock. A task is one agent in one herdr pane, optionally in its own git worktree; a job is a TOML file that queues tasks on a schedule.

## Learn the current CLI

The installed binary is the authority for syntax. This guide is for orientation; check a command's help before relying on a flag:

```bash
pastor --help
pastor task --help
pastor job --help
pastor machine --help
pastor flock --help
pastor events --help
pastor setup --help
```

Most read commands take `--json` (`pastor task list`, `pastor task show`, `pastor machine list`, `pastor job list`, `pastor events`). Use it, and read task ids, machines and states from the output instead of predicting them.

Runtime errors are one JSON object on stderr, `{"code": ..., "message": ...}`, with exit 1. A malformed command line is clap usage text with exit 2.

## Is a head running

```bash
pastor machine list
```

With a head running, it opens with a line about the head (its pastor and herdr versions, its host, how many machines follow), then one row per machine: `NAME`, `HOST`, `FLOCK`, `CHANNEL` (`connected`, `polling`, `connecting`, `reconnecting`, `incompatible`), `HERDR` (the herdr version), `PASTOR` (the pastor installed there, `-` when unknown), `AGENTS` (live tasks over `max_agents`), `TAGS`, `ERROR`. Only `connected` and `polling` machines take tasks. With no head running, it says so on stderr and probes each machine directly instead: CHANNEL is then `probed` (herdr answered), `server down` (a local socket with no server), `unreachable` (ssh or transport failed) or `error` (herdr answered the ping with an error), and HERDR, PASTOR and AGENTS come from that probe. Report what it shows rather than starting `pastor serve` yourself, which is a long-running daemon.

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

- `--flock F` sends the task to that flock's machines only. Without it the task goes to the flock of `--machine`, else the default flock (`pastor flock list` marks it). `--machine` in another flock than `--flock` is refused with `flock_mismatch`.
- `--machine M` pins the task. `--tag T` (repeatable) instead restricts it to machines that carry every given tag in `flock.toml`; it is a filter, not a label on the task. With neither, any machine of the task's flock with a free slot takes it.
- `--repo` is a path on the machine that runs the agent. Quote a leading `~` so your shell does not expand it. pastor checks that it is a directory on that machine before it creates anything (`test -d` over ssh); a missing repo fails the task with `repo <path> does not exist on <machine>`. A `command` machine cannot be checked.
- `--worktree` makes a git worktree of `--repo` for the task, on `--branch` or `pastor/t-N`. It needs `--repo` and the repo cloned on that machine.
- `--agent-arg` passes one argument to the agent and always takes the next word, dashes included. Repeat it, in order.
- Without `--agent` and `--agent-arg`, the task takes the `agent` and `agent_args` of the machine it runs on from `flock.toml`, then its flock's, then `[defaults]` in `pastor.toml`, then `claude`. Args follow the agent they were written for: a flock's args for codex never reach a task run with `--agent claude`. `pastor task show t-N` prints what the task resolved to and where each came from; an unpinned task settles its agent again when it is placed on a machine.
- An agent name can be a definition in `pastor.toml`: `[agents.claude-personal]` with `kind = "claude"` and `env = { CLAUDE_CONFIG_DIR = "~/.claude-personal" }` runs Claude on another account. `--agent claude-personal` (or a flock's `agent`) picks it; herdr starts the `kind`, with the env set on the task's pane, and Claude's trust keys and tool flags follow the kind.
- Tool permissions: the agent keeps its own permission mode. `allow` and `deny` lists of tool patterns (`"Bash(git:*)"`) in `[defaults]`, a `[[flock]]` entry or a job's `[dispatch]` add up, and deny wins over allow; pastor passes them as the agent's own flags (`--allowedTools`, `--disallowedTools` for Claude). An agent with no such flags refuses tasks that carry a list (`agent_tools_unsupported`). Never add `--dangerously-skip-permissions` or similar to `--agent-arg` on your own: a prompt-injected agent would then act as the machine's user with nothing to stop it. Leave that decision to the user.
- `--timeout` bounds the task; past it the task goes `stale`.
- `--prompt-file PATH` takes the prompt from a file on the machine running the CLI (`-` is stdin) in place of the argument; give exactly one of the two. Use it for a long prompt: quotes, backticks and `$` need no escaping, and trailing newlines are dropped. An unreadable file fails with `prompt_file_unreadable`, an empty one with `prompt_file_empty`.

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
- `blocked`: the agent is waiting on a permission prompt or question. Nobody answers it unless someone sends input (`pastor task send`) or attaches.
- `done`: the agent went idle after pastor saw it work, and stayed idle for `settle` (10s by default). It means the agent stopped, not that the work is good; read the output.
- `stale`: the timeout passed without `done`. The agent is left running.
- `failed`: dispatch failed or the agent exited before it was done. `task show` has the error.
- `closed`: finished for good, by pastor after the grace period or by `task close`. Usually the pane is gone, but a task that never reached a machine, or whose machine left the flock, is closed as a row only: no pane was closed and its worktree may still be on disk.

pastor closes a done task's pane after `close_done_after` (`pastor.toml`, default `15m`; `never` disables it): a worktree pastor created is removed if it is clean, kept with a note on the task if it is not, and the task then shows as `closed`. Failed, stale and blocked tasks are never closed on their own; use `pastor task retry` or `pastor task close`. The check runs on each reconcile while the machine is connected; an agent herdr shows working or blocked again at that moment is left alone, and its task goes back to `running` or `blocked`.

## Closing and retrying tasks

```bash
pastor task retry t-4                      # queues a copy of a failed or stale task, new id, retry_of t-4
pastor task close t-4                      # closes the pane if there is one, marks the task closed
pastor task close t-4 --remove-worktree    # removes the worktree too; refused if it has uncommitted changes
pastor task prune --done --older-than 3d   # deletes finished rows; --failed and --closed add those states
```

`retry` re-dispatches the task's job, item, prompt and dispatch settings as a new task; the old agent may still be running under its own id. Retrying a failed worktree task reuses its branch and, if still on disk, its checkout. `close` also closes an orphaned agent, a `t-N` pane with no open task. `prune` never deletes a row whose worktree may still be on disk; it names the ones it keeps, and `task close t-N --remove-worktree` clears them so the next prune takes them. A pruned task's item stays seen, so a job never queues it again.

## Answering a blocked task

```bash
pastor task read t-12                       # see what it is asking first
pastor task send t-12 "yes, go on"          # types the text, then Enter; --no-enter leaves Enter out
pastor task send t-12 --key esc             # named keys, in order; repeat --key
pastor task send t-12 --trust               # accept the folder-trust prompt, and trust that repo on that machine
pastor trust list                           # saved (machine, repo) pairs; pastor trust remove <machine> <repo>
```

Only starting, running and blocked tasks take input (`task_not_live` otherwise). Read the pane before you answer; never send what a human should decide. Once a repo is trusted on a machine, the head answers the trust prompt of its later tasks there by itself, once per task (`task.trusted`), worktrees included. `--trust` answers only a task blocked on its startup prompt (`not_at_trust_prompt` otherwise) and needs `trust_keys` for the agent (Claude has them built in); `no_trust_keys` otherwise.

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

`[dispatch]` takes the same things as `pastor task run`, plus tool lists: `agent`, `agent_args`, `allow`, `deny`, `repo`, `worktree`, `branch`, `tags`, `flock`, `machine`, `timeout`, plus `max_tasks_per_run` and the `prompt` template.

```bash
pastor job list            # schedule, enabled, last and next run, errors
pastor job run hourly      # fire now, ignoring the schedule
pastor job enable hourly
pastor job disable hourly
pastor job reload          # re-read the files now instead of at the next tick
pastor tick --dry-run --job hourly   # what a run would create, creating nothing
```

The head picks up job file edits by itself. A file that stops parsing keeps its last good version and shows the error in `pastor job list`. It also re-reads `flock.toml` and `pastor.toml`, so `pastor machine add`, `pastor machine remove`, `pastor machine move`, the `pastor flock` commands and config edits need no restart (`pastor tick` reloads them too); one of those that stops parsing also keeps its last good version, but reports it in the daemon log only (`journalctl --user -u pastor`), not in `pastor job list`.

## The fleet

`~/.config/pastor/flock.toml` holds one `[[machine]]` per machine: `name`, exactly one of `ssh = "user@host"`, `local = true` or `command = [...]` (for tests), `session` (the herdr session, default `default`), `max_agents` (default 2), `tags`, `flock`, and optionally the `agent` and `agent_args` its tasks get when they name none, before the flock's. `[[flock]]` entries (`name`, `default = true` on one of them, and optionally the `agent` and `agent_args` its tasks and jobs get when they name none, and `allow` and `deny` tool lists added to theirs) declare the flocks; a machine with no `flock` is in the default one, and a file with no `[[flock]]` has a single flock named `default`.

```bash
pastor machine add pi-3 user@pi-3 --max-agents 2 --tag arm --herdr
pastor machine add here --local
pastor machine add pi-5 user@pi-5 --flock work
pastor machine move pi-3 work   # new tasks only; tasks already on it stay
pastor machine remove pi-3 --herdr
pastor machine list        # connects to each machine now: ssh, herdr version, agents
pastor flock list          # each flock: default, machines, live agents, queued tasks
pastor flock add work [--default]
pastor flock default work  # new tasks and jobs go there; machines stay put
pastor flock remove work   # refused while it has machines or queued tasks, or is the default
```

These commands edit `flock.toml` in place, keeping its comments.

Every machine needs a herdr server running, and the head needs passwordless ssh to it. Start herdr with `herdr server`, or better as a user service: `pastor setup systemd --herdr --yes` on that machine. The head itself runs as a service with `pastor setup systemd --yes`. Setup needs `--yes` when stdin is not a terminal. Ask the user before installing services.

## Events

```bash
pastor events                  # every task, job and machine event, oldest first
pastor events --task t-12 --json
timeout 60 pastor events --follow   # --follow never returns on its own; always bound it
```

It reads the log file, so it works with the head down.

## When pastor dispatched you

You are a pastor task when `PASTOR_TASK=t-N` is set (or, from an older pastor, `HERDR_ENV=1` is set, your herdr agent and workspace are named `t-N`), and the prompt reads like a self-contained work order. Then:

- Work only in the directory you started in: your worktree and branch. Never touch other worktrees, branches or panes.
- Commit and push exactly as the prompt says, and write the report it asks for.
- Do not ask questions. Nobody is watching; a permission prompt or a question leaves the task `blocked` until a human happens to attach. If something is missing, say so in your report and stop.
- Print `DONE` as your last line when finished, then go idle.
- Do not close your pane or exit to clean up. That is the user's job.
- Do not run, send to, attach to, retry, close or prune tasks, tick (not even `--dry-run`), run or reload jobs, install, link, uninstall or unlink connectors, edit machines, flocks or jobs, or run `pastor serve`, `pastor setup` or `pastor open`. pastor refuses these from your pane with `agent_refused` unless the user set `agents_change_fleet = true`; do not work around it. Reading (`task list`, `show`, `read`) is fine.

## When something goes wrong

- A task stays `queued`: no connected machine of its flock carries all its tags or has a free slot. Compare `pastor machine list` (FLOCK, TAGS) with the task's flock and tags. The head logs a warning after an hour, and at once for a task pinned to a machine that has moved to another flock.
- `failed` with `agent_pane_busy`: herdr refused to start the agent because the pane was not at an idle shell prompt.
- `failed` with "agent t-N not found": the agent's pane vanished before it was done (closed by hand, herdr restarted, or the agent crashed).
- `failed` soon after start: the agent is usually not installed, or not on the PATH herdr sees on that machine.
- `blocked` right after start: often Claude's "trust this folder" dialog on a repo that machine has not seen. Check with `pastor task read`, then `pastor task send t-N --trust` answers it and saves the repo as trusted, so its next tasks on that machine go through on their own.
- A machine `polling`: herdr answers requests but its event stream will not open. It still takes tasks; pastor checks them every tick and keeps trying to subscribe.
- A machine `reconnecting`: herdr is unreachable. Its tasks are reconciled when it comes back.
- A `timeout` error from the CLI: the head may still carry the request out. Check `pastor task list` before sending it again, or you may queue a duplicate.
