---
name: pastor
description: "Drive pastor, a daemon that runs coding agents on a fleet of machines through herdr. Use when the user mentions pastor; when you have to run, watch or manage coding agents on other machines through pastor (dispatch a task, check its state, read its output, schedule a job, add a machine); or when you were yourself started by pastor as task t-N inside a herdr pane and need to know what is expected of you."
---

# pastor

pastor runs coding agents on machines the user owns. One machine runs the head, `pastor serve`, which talks over ssh to the machines listed in `flock.toml`, each running a herdr server. Every machine is in one or more flocks, named groups; a task goes only to machines of its flock. A task is one agent in one herdr pane, optionally in its own git worktree; a job is a TOML file that queues tasks on a schedule.

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

Most read commands take `--json` (`pastor task list`, `pastor task describe`, `pastor machine list`, `pastor job list`, `pastor events`, and every `describe`). Use it, and read task ids, machines and states from the output instead of predicting them.

Runtime errors are one JSON object on stderr, `{"code": ..., "message": ...}`, with exit 1. A malformed command line is clap usage text with exit 2.

## Is a head running

```bash
pastor machine list
```

With a head running, it opens with a line about the head (its pastor and herdr versions, its host, how many machines follow), then one row per machine:

| Column | Shows |
|---|---|
| `NAME`, `HOST` | the machine, and how it is reached (`user@host`, `local`) |
| `FLOCKS` | each flock with the machine's number there, `work:2,home:1` |
| `PROFILE` | the machine's permission profile |
| `CHANNEL` | `connected`, `polling`, `connecting`, `reconnecting`, `incompatible`; only `connected` and `polling` machines take tasks |
| `HERDR`, `PASTOR` | the herdr version, and the pastor installed there (`-` when unknown) |
| `AGENTS` | live tasks over the machine's room: `max_agents`, then `+1j` for job slots and `+1b` for burst when set |
| `ORPHANS` | agents named `t-N` that no open task owns |
| `TAGS`, `ERROR` | its tags, and its error when it has one |

With no head running, it says so on stderr and probes each machine directly instead, except pull machines, whose CHANNEL reads `not probed`. For the others CHANNEL is then `probed` (herdr answered), `server down` (a local socket with no server), `unreachable` (ssh or transport failed) or `error` (herdr answered the ping with an error), and HERDR, PASTOR and AGENTS come from that probe. Report what it shows rather than starting `pastor serve` yourself, which starts a long-running daemon; `pastor serve status` (`--json`) says whether one runs on this machine, with its pid, version and service.

```bash
pastor task list          # live tasks: queued, starting, running, blocked, paused
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
- `--place` says where the agent's pane goes on the machine's herdr. `repo` (the default): a worktree task in its new worktree, a task whose `--repo` a workspace already shows (a fix round in a pull request's worktree) in a new pane there, anything else in its own workspace. `own`: always its own workspace. `pastor`: a pane in the machine's `pastor` workspace, made on first use. `pane:<workspace>`: a pane in the workspace with that label, refused if the machine has none. A job sets it with `place` in `[dispatch]`, a flock with `place` on its `[[flock]]` entry, `pastor.toml` with `place` under `[defaults]`, in that order. Closing a task closes only its own pane, never a workspace it joined.
- `--label TEMPLATE` names the workspace pastor makes for the task (its own, or its worktree's), with `{{ task.id }}`, `{{ flock }}`, `{{ machine }}`, `{{ job }}` (empty for `task run`) and `{{ item.key }}`. Without it the task takes the `label` of its job's `[dispatch]`, then its flock, then `[defaults]`, then `{{ flock }}/{{ task.id }}`, such as `personal/t-285`. Leading and trailing spaces and slashes are dropped; a label that renders empty or with a control character falls back to `t-N`. Only the workspace is named so: the agent stays `t-N`, and a task that joins a workspace leaves its label alone. `task describe` shows the label and where it came from.
- `--agent-arg` passes one argument to the agent and always takes the next word, dashes included. Repeat it, in order.
- `--model NAME` runs a model from `[models.<name>]` in `pastor.toml` (its `kind` and `args`, which go before the agent's). It takes a name, never raw args: those go in `--agent-arg`.
  - Without it: the `model` of the task's flock, then its machine, then `[defaults]`, then none.
  - An unknown name is `unknown_model`; a model of another kind than the agent is `model_kind_mismatch`.
  - An unpinned task only goes to machines whose agent has the model's kind. A model of another kind than the default agent's runs on the agent a machine, flock or `[defaults]` names for that kind in `agents = { <kind> = "<agent>" }`; a machine with none never gets it.
- `--fallback sonnet,gpt` names the `[models]` the task may fall back to, in order; `--no-fallback` names none. Without either: the `fallback` of its machine, then its flock, then `[defaults]`, then none; the first list wins whole, and `[]` means none. An unknown name is `unknown_model`. Nothing switches models yet; `task describe` shows the list.
- `--priority LEVEL` (`low`, `normal`, `high`, `critical`) orders the queue when machines are full: by level, highest first, then by position, then age.
  - Without it: the `priority` of the task's flock, then the machine it is pinned to, then `[defaults]`, then `normal`. Another word is `unknown_priority`.
  - `pastor task priority t-N LEVEL` changes a queued task's level (`not_queued` once a machine took it); `task retry` keeps it.
  - `pastor queue` lists queued tasks in start order, with how long each waited and why it has not started (`--flock`, `--machine`, `--json`). `pastor queue move t-N` with `--top`, `--before t-M`, `--after t-M` or `--to N` moves one, at the level of where it lands.
- `--preempt` (only on a `critical` task, else `preempt_needs_critical`): when no machine has room, pause the newest running `low` Claude task on a machine it may use and start in its slot.
  - The paused task's agent is interrupted and its pane closed, its worktree kept. It goes `paused`, first among `low` tasks and pinned to its machine, and resumes its own session (`claude --resume`) there when a slot frees.
  - A normal task, an opencode task, a done one or one resumed in the last 10 minutes is never paused.
  - `pastor task priority t-N critical --preempt` sets it on a queued task (the same command without it drops it); a job sets `preempt = true` under `[dispatch]`.
- Without `--agent` and `--agent-arg`, the task takes the `agent` and `agent_args` of its machine in `flock.toml`, then its flock's, then `[defaults]` in `pastor.toml`, then `claude`. The agent is the one setting where the machine comes before the flock.
  - Args follow the agent they were written for: a flock's args for codex never reach a task run with `--agent claude`.
  - A flock's `agent` also sets the kind its tasks run: a machine whose own agent is of another kind runs its `agents` entry for that kind, and one with none is skipped (pinned there: `agent_kind_missing`).
  - An unpinned task settles its agent again when placed on a machine. `pastor task describe t-N` prints what the task resolved to and where each came from.
- An agent name can be a definition in `pastor.toml`: `[agents.claude-personal]` with `kind = "claude"` and `env = { CLAUDE_CONFIG_DIR = "~/.claude-personal" }` runs Claude on another account. `--agent claude-personal` (or a flock's `agent`) picks it; herdr starts the `kind` with the env set on the task's pane, and trust keys and tool flags follow the kind.
- `--profile NAME` runs the task under a permission profile: `review`, `develop`, `unrestricted`, or `[profiles.<name>]` in `pastor.toml` (`pastor profile list`). Pick the narrowest that does the job: `review` for reading, `develop` for changing a checkout.
  - Without it: the `profile` of the task's flock, then its machine, then `[defaults]`, then none. An unknown name is `unknown_profile`.
  - The profile's allow and deny go before the task's own. A Claude agent starts with `--permission-mode dontAsk`: it never stops at a permission prompt, and is refused what the lists and its settings do not allow. Agent args that pick a permission mode are refused (`profile_args_conflict`).
  - An opencode agent gets the lists as `OPENCODE_PERMISSION` in its pane, denying what they do not allow (`unrestricted` allows it); a machine whose own opencode config has permission rules fails the task (`opencode_permissions_conflict`).
  - `unrestricted` runs only on a machine whose own profile (its `profile`, else its flock's, else `[defaults]`) is `unrestricted` (`profile_not_allowed`); a flock's profile never lifts a machine's.
- Tool permissions: without a profile the agent keeps its own permission mode. `allow` and `deny` lists of tool patterns (`"Bash(git:*)"`) in `[defaults]`, a `[[flock]]` entry or a job's `[dispatch]` add up, and deny wins over allow; pastor passes them as the agent's own flags (`--allowedTools`, `--disallowedTools` for Claude). An agent with no such flags refuses tasks that carry a list (`agent_tools_unsupported`). Never add `--dangerously-skip-permissions` or similar to `--agent-arg` on your own: a prompt-injected agent would then act as the machine's user with nothing to stop it. Leave that decision to the user.
- `--timeout` bounds the task while it runs; past it the task goes `stale`. A blocked task waits for a person instead and does not time out; the clock restarts once the block clears, so each uninterrupted running spell gets the full timeout, not what was left of the last one. Without it the task takes its job's `timeout`, then its flock's, then `[defaults]`.
- `--description TEXT` gives the task one line on what it is about, shown by `pastor task list --wide`, `task describe` and in `--json`; without it the prompt's first line stands in. Jobs, flocks and machines carry one too (`description` in their files); `--wide` on any `list` adds the column.
- `--summary MODE` (`ask`, `require`, `off`) says whether the prompt asks the agent for a summary at the end, and with `require` fails a task that stops without one. Without it the task takes its job's, then its flock's, then `[defaults]`, then `ask`. `pastor task describe` shows the summary, and `task list --wide` its first line as RESULT.
- `--role orchestrator` starts a task that may also run, retry, send to and close tasks and enable and disable jobs. Only a person may start one: from a task's pane it is `role_refused`.
- `--prompt-file PATH` takes the prompt from a file on the machine running the CLI (`-` is stdin) in place of the argument; give exactly one of the two. Use it for a long prompt: quotes, backticks and `$` need no escaping, and trailing newlines are dropped. An unreadable file fails with `prompt_file_unreadable`, an empty one with `prompt_file_empty`.

pastor sends the prompt as is, and the agent knows nothing else. Write it so the agent can finish alone: say where it is (its own worktree and branch), what to change, what to commit and where to push, what report to write, and to print `DONE` as its last line.

Then watch it:

```bash
pastor task describe t-12 --json   # one task, with its error and agent args
pastor task read t-12          # recent pane output; --lines N for more
pastor events --task t-12      # what happened to it, and when
```

`pastor task attach t-12` puts a human terminal into the agent's pane (ctrl+b q detaches). On a closed or failed Claude task whose pane is gone, it reopens the agent's own session (`claude --resume`) in a new pane on the task's machine, re-creating a removed worktree first; the task itself does not change. Other agents cannot be reopened. It needs a real terminal; as an agent, use `pastor task read`.

States:

- `queued`: accepted, no machine has a free slot or the tag yet.
- `starting`: pastor is creating the workspace and starting the agent.
- `running`: the agent is working.
- `blocked`: the agent is waiting on a permission prompt or question, or ended its turn on a question (the task's error reads `agent asked: ...`). Nobody answers it unless someone sends input (`pastor task send`) or attaches.
- `done`: the agent went idle after pastor saw it work, and stayed idle for `settle` (10s by default). It means the agent stopped, not that the work is good; read the output.
- `stale`: the timeout passed without `done` during a single uninterrupted running spell, or the task's pull machine went silent for `pull_lost_after` (`pastor.toml`, default `10m`). A task waits out `blocked` indefinitely, and the clock restarts from a fresh full timeout each time a block clears, so time spent blocked never counts toward this. The agent is left running.
- `paused`: a critical `--preempt` task took its slot; its pane is gone, its worktree kept, and it resumes its session when its machine has room. `task send` and `task attach` refuse it (`task_not_live`, `task_paused`); `task close` closes the row (`--remove-worktree` removes the kept worktree too).
- `failed`: dispatch failed or the agent exited before it was done. `task describe` has the error.
- `closed`: finished for good, by pastor after the grace period or by `task close`. Usually the pane is gone, but a task that never reached a machine, or whose machine left the flock, is closed as a row only: no pane was closed and its worktree may still be on disk.

pastor closes a done task's pane after `close_done_after` (`pastor.toml`, default `5s`; `never` disables it): a worktree pastor created is removed if it is clean (no uncommitted changes and no commits on no remote), kept with a note on the task if it is not, and the task then shows as `closed`. Failed and stale tasks stay for `pastor task retry`, but once their agent has stopped pastor closes their pane after `close_failed_after` (default `5s`, `never` keeps it; a stale one turns `failed`), and an orphaned `t-N` agent's too; blocked tasks are never closed on their own. The check runs after each reconcile while the machine is connected, and in between at the shorter of `close_done_after` and `close_failed_after` (one set to `never` left out) when that is shorter than `reconcile_every`; an agent herdr shows working or blocked again at that moment is left alone, and its task goes back to `running` or `blocked`.

## Closing and retrying tasks

```bash
pastor task retry t-4                      # queues a copy of a failed or stale task, new id, retry_of t-4
pastor task retry t-4 --place own          # the same, with the copy's pane placed elsewhere
pastor task close t-4                      # closes the pane if there is one, marks the task closed
pastor task done t-4                       # marks a task with a pane done; its pane closes after close_done_after
pastor task describe t-4 --all-summaries   # how each round of the task ended
pastor task close t-4 --remove-worktree    # removes the worktree too; refused if it has uncommitted changes
pastor task close t-4 t-5 t-6              # several: one line each, the rest go on past a failure, exit 1 (close_failed) if any failed
pastor task prune --done --older-than 3d   # deletes finished rows; --failed and --closed add those states
```

`retry` re-dispatches the task's job, item, prompt and dispatch settings as a new task; the old agent may still be running under its own id. Retrying a failed worktree task goes back to its checkout and branch while that checkout is still on disk and no agent is in it; otherwise, and always for a stale task, the retry gets a new branch (`pastor/t-<new id>`) and a new worktree. `close` also closes an orphaned agent, a `t-N` pane with no open task. `done` is how an agent ends its own task (with no task given it ends `PASTOR_TASK`'s): the task is `done` at once and stays so while the agent finishes its turn, and auto-close takes its pane once the agent is idle; `task send` to it makes it run again. `prune` never deletes a row whose worktree may still be on disk; it names the ones it keeps, and `task close t-N --remove-worktree` clears them so the next prune takes them. A pruned task's item stays seen, so a job never queues it again.

## Answering a blocked task

```bash
pastor task read t-12                       # see what it is asking first
pastor task send t-12 "yes, go on"          # types the text, then Enter; --no-enter leaves Enter out
pastor task send t-12 --key esc             # named keys, in order; repeat --key
pastor task send t-12 --trust               # accept the folder-trust prompt, and trust that repo on that machine
pastor trust list                           # saved (machine, repo) pairs
pastor trust add <machine> <repo>           # trust a repo on a machine; remove undoes it
pastor profile list                         # permission profiles; pastor profile describe <name> shows its allow and deny
```

Only starting, running and blocked tasks take input, and done ones whose pane is still open (`task_not_live` otherwise); a done task sent input goes back to running, so `pastor task send t-12 "commit and push"` finishes work an agent left undone. Read the pane before you answer; never send what a human should decide. Once a repo is trusted on a machine, the head answers the trust prompt of its later tasks there by itself, once per task (`task.trusted`), worktrees included, and only while the pane shows that prompt; a task blocked on any other dialog is left for you. `--trust` answers only a task blocked on its startup prompt (`not_at_trust_prompt` otherwise) and needs `trust_keys` for the agent (Claude has them built in); `no_trust_keys` otherwise.

## Jobs

A job is one TOML file under `~/.config/pastor/jobs/`, named after the file:

```toml
every = "1h"                 # or cron = "*/30 9-18 * * 1-5", local time
[connector]
use = "clock"                # built in; others come from pastor connector install
[dispatch]
repo = "~/work/api"
tags = ["arm"]
prompt = "It is {{ item.key }}. Run the suite and fix what broke. Task {{ task.id }}."
```

`[dispatch]` takes the same things as `pastor task run`, plus tool lists: `agent`, `agent_args`, `model` (a name, or a template like `"{{ item.model }}"`; empty falls through to the flock's), `priority` (a level, or a template like `"{{ item.priority }}"`; empty falls through), `preempt` (its tasks that settle at critical may pause a low one), `summary`, `profile`, `allow`, `deny`, `repo`, `worktree`, `branch`, `tags`, `flock`, `machine`, `timeout`, `place`, `label`, plus `max_tasks_per_run`, `backfill` (how far back the first run looks, such as `"7d"`; without it the first run sees only items from then on), the `prompt` template and a `description` template for each task (default `"{{ item.title }}"`).

```bash
pastor job list            # schedule, enabled, last and next run, errors
pastor job describe hourly --json   # one job: connector, dispatch, last runs and errors, recent tasks
pastor connector describe github-issues --json   # one connector: origin, commands, config, missing secrets, jobs using it
pastor job run hourly      # fire now, ignoring the schedule
pastor job enable hourly
pastor job disable hourly
pastor job reload          # re-read the files now instead of at the next tick
pastor tick --dry-run --job hourly   # what a run would create, creating nothing
```

The head picks up job file edits by itself. A file that stops parsing keeps its last good version and shows the error in `pastor job list`. It also re-reads `flock.toml` and `pastor.toml`, so `pastor machine add`, `pastor machine remove`, `pastor machine move`, the `pastor flock` commands and config edits need no restart (`pastor tick` reloads them too); one of those that stops parsing also keeps its last good version, but reports it in the daemon log only (`~/.local/state/pastor/serve.log`, or `journalctl --user -u pastor` under systemd), not in `pastor job list`.

## The fleet

`~/.config/pastor/flock.toml` holds one `[[machine]]` per machine:

| Key | Meaning |
|---|---|
| `name` | the machine's name |
| `ssh = "user@host"`, `local = true`, `pull = true` or `command = [...]` | exactly one: how the head reaches it. `pull` is a machine whose own headless `pastor serve` takes its tasks from the head; `command` is for tests |
| `session` | the herdr session, default `default` |
| `max_agents` | how many live tasks it runs, default 2 |
| `job_slots` | default 1: extra slots only tasks from jobs take, a free one first |
| `burst` | default 1: how many past `max_agents` a `critical` task may start (setting either it or `job_slots` to `0` disables only that one) |
| `tags`, `flock` | its tags, and a flock it joins |
| `agent`, `agent_args` | what its tasks get when they name none, before the flock's |
| `model`, `priority`, `profile` | the same, after the flock's |

`[[flock]]` entries declare the flocks:

| Key | Meaning |
|---|---|
| `name` | the flock's name |
| `default = true` | on one of them |
| `machines = { desk = 2 }` | the machines it may use, with at most how many of its live tasks each runs |
| `machines = { desk = { share = 2, max = 4 } }` | under its share the flock takes a free slot as usual; between share and max only while no task of a flock under its share there is waiting |
| `agent`, `agent_args`, `agents`, `model`, `profile`, `priority`, `timeout`, `place`, `label`, `summary` | optional, as in `[defaults]`: what its tasks and jobs get when they name none (`label` names the workspace) |
| `allow`, `deny` | tool lists added to theirs |

A task gets its own flock's settings; they come before the machine's for everything but the agent. A machine's own `flock` key also puts it in that flock, with no number but the machine's limits; a machine nothing places is in the default one, and a file with no `[[flock]]` has a single flock named `default`. A task starts on a machine only when the machine has room and the task's flock is under its number there (job slots and burst never pass it).

```bash
pastor machine add pi-3 user@pi-3 --max-agents 2 --tag arm --herdr
pastor machine add here --local
pastor machine add pi-5 user@pi-5 --flock work
pastor machine move pi-3 work   # leave every flock, join work; new tasks only, tasks already on it stay
pastor machine remove pi-3 --herdr
pastor machine list        # connects to each machine now: ssh, herdr version, agents
pastor flock list          # each flock: default, machines as `pi-3 1/2` (live/number, `1/2/4` for live/share/max), live agents, queued tasks
pastor flock add work [pi-3 ...] [--default]   # the machines named join it
pastor flock join work pi-3 --max 2   # pi-3 also in work, at most 2 of its tasks; again with --max changes it
pastor flock leave work pi-3   # out of its last flock, pi-3 is back in the default one
pastor flock default show      # print the default flock
pastor flock default set work  # new tasks and jobs go there; machines stay put
pastor flock remove work   # refused while it has machines or queued tasks, or is the default
pastor machine describe pi-3 --json   # one machine: channel, versions, its tasks, recent errors
pastor flock describe work --json     # one flock: default, agent, machines, live tasks
```

`pastor job edit hourly`, `pastor flock edit` and `pastor config edit` open the file in `$VISUAL` or `$EDITOR` and save it only once it is valid, reloading a running head. They are for a human at a terminal: an agent without one would wait on the editor, so edit the file directly and run `pastor job reload`, or use the commands above.

These commands edit `flock.toml` in place, keeping its comments.

Before a new machine takes work unattended, go through this once:

1. herdr, pastor and the agents must be on the PATH of a non-interactive ssh command. The head runs `ssh user@host sh -c 'herdr … remote-api-bridge'`, and many `~/.bashrc` files return early when not interactive (Debian's does), so put the PATH line before that check. Check each name on its own, since POSIX `command -v` takes one: `ssh user@host 'command -v claude && command -v herdr && command -v pastor'`.
2. Every repo the flock's jobs and tasks use must exist at the same path on the machine. pastor does not clone. Clone each one before the machine joins a flock whose jobs use it.
3. Save trust for each repo right after cloning: `pastor trust add <machine> <repo>`. The head then answers Claude's folder-trust prompt for that repo's tasks and worktrees on that machine. `pastor trust list` shows the pairs.
4. Log the agent in, then finish its first-run setup once. For Claude, `claude auth login` saves credentials, but the first interactive start still stops on first-run screens (theme, login confirmation) and the first task sits `blocked`. Finish them with `pastor task attach t-N` and detach with ctrl+b q. `~/.claude.json` has `hasCompletedOnboarding: true` once it is done.
5. Prove it with a task pinned to the machine: `pastor task run --machine <machine> --repo <repo> --worktree "Reply with the single word ready. Do not run any commands."`, then `pastor task read t-N` and `pastor task close t-N --remove-worktree` so the test checkout does not stay on disk.

Every machine needs a herdr server running, and the head needs passwordless ssh to it. Start herdr with `herdr server`, or better as a user service: `pastor setup systemd --herdr --yes` on that machine. The head itself runs as a service with `pastor setup systemd --yes`. On macOS use `pastor setup launchd` with the same flags. Setup needs `--yes` when stdin is not a terminal. Ask the user before installing services.

## Events

```bash
pastor events                  # every task, job and machine event, oldest first
pastor events --task t-12 --json
timeout 60 pastor events --follow   # --follow never returns on its own; always bound it
```

On the head's machine it reads the log file, so it works with the head down; with a remote head set it asks the head.

To wait on the fleet rather than read its history, use `pastor watch`: one line per change to act on (`TASK t-12 failed ...`, `JOB nightly failing: ...`, `HEAD down: ...`, and the lines of connectors with a `[watch]` command).

```bash
pastor watch --now                         # what needs attention now: blocked, done, failed, stale tasks, failing jobs
timeout 1800 pastor watch --name night     # never returns on its own; bound it and run it again with the same --name
pastor watch --name night --json --interval 2m --connector prs
```

A watcher keeps a cursor under its `--name`, so one started again repeats nothing; `--reset` starts it over at the end of the log. It only reads, so it works from a pastor task too.

## A head on another machine

The CLI can drive a head on another machine over ssh. `pastor head set user@pi-1` saves it in `~/.config/pastor/client.toml`, `PASTOR_HEAD=<dest>` overrides that for one shell, and `--head <dest>` for one command. Most commands then go to that head, except `machine authorized-key` (`remote_head_unsupported`); the ones that stay local on purpose, `completions`, `setup`, `head`, `bridge`, `connector` and `config edit --local`; `task attach` and `machine open`, which still ask the head for the task or machine but then run here, against the machine itself; and `serve status`/`serve stop`, which always act on this machine's own serve, head or headless. `pastor serve` on such a machine runs headless: this machine's jobs and hooks, and the tasks the head hands it when it is a `pull = true` machine there.

## When pastor dispatched you

You are a pastor task when `PASTOR_TASK=t-N` is set (or, from an older pastor, `HERDR_ENV=1` is set, your herdr agent is named `t-N`), and the prompt reads like a self-contained work order. Then:

- Work only in the directory you started in: your worktree and branch. Never touch other worktrees, branches or panes.
- Commit and push exactly as the prompt says, and write the report it asks for.
- Do not ask questions. Nobody is watching; a permission prompt or a question leaves the task `blocked` until a human happens to attach. If something is missing, say so in your report and stop.
- When finished, run `pastor task done --summary "<outcome>: <what you did>"` (it ends your own task, from `PASTOR_TASK`; `--summary-file -` reads a longer one from stdin), print `DONE` as your last line, then go idle. The summary's first line starts with the outcome: `done`, `partial`, `blocked` or `nothing to do`; say what you pushed or why you stopped. pastor marks the task `done` at once and closes your pane after `close_done_after`, freeing the machine's slot. If your prompt ends by saying pastor fails the task without a summary, a bare `pastor task done` is refused (`summary_required`) and stopping without one fails the task: send one, whatever the outcome.
- Do not close your pane or exit to clean up; `pastor task done` is how you say you are finished.
- Do not run, send to, attach to, retry, reprioritize, move in the queue, close or prune tasks, tick (not even `--dry-run`), run, reload, enable, disable or edit jobs, change orchestrators, add or remove trusted repos, install, link, uninstall or unlink connectors, edit machines, flocks or pastor.toml, or run `pastor serve`, `pastor setup` or `pastor machine open`. pastor refuses these from your pane with `agent_refused` unless the user set `agents_change_fleet = true`; do not work around it. `pastor task done` for your own task is the one exception; for any other task it is refused too. Reading (`task list`, `queue`, `read`, `events`, `watch`, `serve status`, and `describe` for tasks, jobs, machines, flocks and connectors) is fine; `pastor serve stop` is refused like `pastor serve`.
- If `pastor task describe $PASTOR_TASK` says `role: orchestrator`, a person started you to coordinate: you may also `pastor task run`, `task retry`, `task send` and `task close`, and `pastor job disable` a failing job and `pastor job enable` it again. Everything else above is still refused, `task prune` included, and you may never start another orchestrator (`--role orchestrator` is `role_refused` from any task).
- If an orchestrator file started you (your prompt ends with the lines its pre script printed), handle each line, then leave the next run a short handover note with `pastor orchestrator note "<what you did, what waits>"` (4 KiB at most; it is your orchestrator's, so no `--name`), and end with `pastor task done --summary-file -`. `pastor orchestrator describe <name>` shows the last runs and their lines.
- If a session orchestrator started you (your prompt asks you to begin with `pastor watch --now`), run it, act on each line, then keep watching with `pastor watch` in the foreground; ending your turn ends your agent, and pastor starts another in your place. Keep the handover note current as you go (`pastor orchestrator note "..."`). When pastor types that your session ends, write the note and end your turn: you are closed after the grace.

## When something goes wrong

- A task stays `queued`: no connected machine of its flock carries all its tags or has a free slot, or its flock is at its number on each machine with room (its error then says `flock <name> is at N of N on <machine>`), or its flock is past its share where another flock under its share waits (`flock <name> is past its share on <machine>, ...`). Compare `pastor machine list` (FLOCKS, TAGS) with the task's flock and tags. The head logs a warning after an hour, and at once for a task pinned to a machine that has moved to another flock.
- `failed` with `agent_pane_busy`: herdr refused to start the agent because the pane was not at an idle shell prompt.
- `failed` with "agent t-N not found": the agent's pane vanished before it was done (closed by hand, herdr restarted, or the agent crashed).
- `failed` soon after start: the agent is usually not installed, or not on the PATH herdr sees on that machine.
- `blocked` right after start: often Claude's "trust this folder" dialog on a repo that machine has not seen. Check with `pastor task read`, then `pastor task send t-N --trust` answers it and saves the repo as trusted, so its next tasks on that machine go through on their own.
- `failed` at once with `repo <path> does not exist on <machine>`: the repo is not cloned there. pastor does not clone.
- A task with no `--repo` starts in `~/pastor-tasks`, where Claude asks for trust once per machine. `pastor trust add` cannot cover it, since the task has no repo. Answer it once with `pastor task send t-N --trust`, or give tasks a `--repo`.
- A prompt the agent does not take (typed while a trust prompt was answered by hand) is sent again up to twice; after that the task is `blocked` with an error saying so, and the agent sits idle at an empty input. Send the prompt again with `pastor task send t-N "<prompt>"` (the task id comes first).
- A machine `polling`: herdr answers requests but its event stream will not open. It still takes tasks; pastor checks them every tick and keeps trying to subscribe.
- A machine `reconnecting`: herdr is unreachable. Its tasks are reconciled when it comes back. Stuck there with `herdr: not found`: herdr is not on the PATH of a non-interactive ssh command on that machine (see the checklist under The fleet).
- A `timeout` error from the CLI: the head may still carry the request out. Check `pastor task list` before sending it again, or you may queue a duplicate.
