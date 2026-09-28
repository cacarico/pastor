---
title: cli
summary: every command, one line each
weight: 1
aliases:
  - /docs/cli/
---
`pastor` is one binary with a command per area. This page lists every
command and the flags you reach for most. `--help` after any command lists
all of its flags.

## shared behaviour

- Every `list` and `describe` takes `--json`, and so do `pastor queue`,
  `pastor serve status` and `pastor head show`. Scripts and agents should
  read ids and states from it rather than parse the tables.
- The commands that change something and answer with a result take `--json`
  too: `task run`, `retry`, `priority`, `close`, `prune`, `send`, `done`,
  `queue move` and `tick`.
- `-w, --wide` on `task list`, `machine list`, `flock list`, `job list`,
  `orchestrator list` and `connector list` adds a DESCRIPTION column, cut to
  the terminal's width. On `task list` it adds RESULT too: how each task's
  last round ended.
- `--head <DEST>` on any command uses the head at that ssh destination for
  this one command. See [a remote head](../../deploy/remote/).
- A runtime error is one JSON object on stderr, `{"code": ..., "message": ...}`,
  with exit 1. A malformed command line prints usage text with exit 2.
- `pastor --skill` prints the guide for coding agents that drive pastor.

A task is named `t-12` or just `12`. A machine is named as `flock.toml`
names it. A job or orchestrator is its file name without `.toml`.

## task

One-off work and every task a job or orchestrator started. See
[tasks](../../concepts/tasks/).

| command | does |
|---|---|
| `pastor task run` | create a one-off task and queue it; the prompt is the argument or `--prompt-file` |
| `pastor task list` | live tasks (queued, starting, running, blocked, paused); orchestrators get a table of their own first |
| `pastor task describe` | one task in full: state, machine, agent, prompt, error, summary |
| `pastor task read` | the last lines of a task's pane |
| `pastor task attach` | attach to a task's agent terminal (ctrl+b q detaches); a closed Claude task's session reopens in a new pane |
| `pastor task send` | type text or press keys in a live task's agent |
| `pastor task retry` | re-dispatch a failed or stale task as a new task |
| `pastor task priority` | put a queued task at another level: low, normal, high or critical |
| `pastor task done` | mark a task done; an agent may end its own, with a summary |
| `pastor task close` | close a task's pane, or an orphaned agent |
| `pastor task prune` | delete old finished tasks; their items stay seen |

`pastor task run` flags. Each default is the first layer that sets it.

| flag | does | default |
|---|---|---|
| `--repo` | the repo the agent works in, a path on the machine that runs it | `~/pastor-tasks` |
| `--machine` | run it on this machine | any free one |
| `--flock` | only this flock's machines take it | the machine's flock, else the default flock |
| `--worktree`, `--branch` | a git worktree per task, on this branch; needs `--repo` | no worktree; branch `pastor/<task id>` |
| `--tag` | only a machine with this tag; repeat for more, it needs them all | none |
| `--agent` | the agent to start, like `claude` or `codex` | the machine's, the flock's, `[defaults]`, `claude` |
| `--agent-arg` | one argument for the agent; repeat, in order | the machine's, the flock's, `[defaults]` |
| `--model` | a `[models]` name | the flock's, the machine's, `[defaults]`, none |
| `--profile` | a permission profile | the flock's, the machine's, `[defaults]`, none |
| `--priority` | low, normal, high or critical | the flock's, the pinned machine's, `[defaults]`, normal |
| `--preempt` | a critical task may pause a low Claude task to take its slot | off |
| `--timeout` | mark it stale after this long (`30m`, `2h`) | the flock's, `[defaults]`, `2h` |
| `--place` | where its pane goes: repo, own, pastor or `pane:<workspace>` | the flock's, `[defaults]`, repo |
| `--label` | the workspace name template | the flock's, `[defaults]`, `{{ flock }}/{{ task.id }}` |
| `--summary` | `ask`, `require` or `off` | the flock's, `[defaults]`, ask |
| `--description` | one line on what it is about | the prompt's first line |
| `--role` | `agent` or `orchestrator`: what it may change through the head | agent |
| `--prompt-file` | read the prompt from a file, or `-` for stdin | |

Other task flags:

| command | flag | does |
|---|---|---|
| `pastor task list` | `--all`, `--done`, `--blocked` | every task, only done ones, only blocked ones |
| `pastor task list` | `--job`, `--flock`, `--machine` | only tasks from this job, flock or machine |
| `pastor task describe` | `--all-summaries` | every round's summary, not only the last |
| `pastor task read` | `--lines` | how many lines from the bottom; default 40 |
| `pastor task send` | `--key` | press a named key (Enter, Down, esc, ctrl+c) after the text; repeat for more |
| `pastor task send` | `--no-enter` | type the text without Enter |
| `pastor task send` | `--trust` | accept the folder-trust prompt and trust the repo on that machine from now on |
| `pastor task retry` | `--place` | put the new task's pane elsewhere |
| `pastor task priority` | `--preempt` | with critical: may pause a low Claude task |
| `pastor task done` | `--summary`, `--summary-file` | what was done; its first line is the outcome |
| `pastor task close` | `--remove-worktree` | remove the worktree too; refused with uncommitted changes |
| `pastor task prune` | `--older-than` with `--done`, `--failed` or `--closed` | which finished tasks to delete |

## queue

See [the queue](../../concepts/queue/).

| command | does |
|---|---|
| `pastor queue` | queued tasks in the order they will start, and why each waits; `--flock` or `--machine` narrow it |
| `pastor queue move` | move a queued task with `--top`, `--before`, `--after` or `--to`; it takes the level of where it lands |

## job

See [jobs](../../concepts/jobs/) and [job files](../job-files/).

| command | does |
|---|---|
| `pastor job list` | every job file: schedule, enabled, last run, next run, last result |
| `pastor job describe` | one job in full: schedule, connector, dispatch, last runs, recent tasks |
| `pastor job run` | fire a job now, ignoring its schedule and `enabled` |
| `pastor job enable` | set `enabled = true` in the file |
| `pastor job disable` | set `enabled = false` in the file |
| `pastor job edit` | open a job file in `$VISUAL` or `$EDITOR`; saved only once valid |
| `pastor job reload` | re-read the job files, `flock.toml` and `pastor.toml` now |

With a head on another machine, a job whose file is on this machine is this
machine's; `job list` shows the head's jobs, then this machine's.

## machine

See [machines](../../concepts/machines/) and [flock.toml](../flock-toml/).

| command | does |
|---|---|
| `pastor machine add` | add a machine to `flock.toml`: over ssh, `--local`, or by `--command` |
| `pastor machine remove` | remove a machine; its tasks keep their rows |
| `pastor machine move` | take a machine out of every flock and put it in one |
| `pastor machine list` | a line about the head, then each machine |
| `pastor machine describe` | one machine in full: versions, agents, recent errors |
| `pastor machine open` | open the full herdr UI on a machine |
| `pastor machine authorized-key` | print the `authorized_keys` line that lets a machine's agents reach this head; needs `--key` |

`pastor machine add` flags:

| flag | does | default |
|---|---|---|
| `--local` | this machine, through herdr's local socket | ssh |
| `--max-agents` | how many tasks it runs at once | 2 |
| `--job-slots` | extra slots only job tasks take; 0 for none | 1 |
| `--burst` | how many past `--max-agents` a critical task may start; 0 for none | 1 |
| `--tag` | a label `task run --tag` can ask for; repeat for more | none |
| `--flock` | the flock it joins | the default flock |
| `--session` | the herdr session agents run in | `default` |
| `--description` | one line on what it is for | none |
| `--herdr` | also save it in herdr's sidebar | off |

`pastor machine remove --herdr` also removes herdr's saved machine.

## flock

See [flocks](../../concepts/flocks/).

| command | does |
|---|---|
| `pastor flock list` | every flock: default or not, its machines, live agents, queued tasks |
| `pastor flock describe` | one flock in full |
| `pastor flock add` | declare a flock, with the machines named; `--default` makes new work go to it |
| `pastor flock join` | put a machine in a flock, or change its number there with `--max` |
| `pastor flock leave` | take a machine out of a flock; out of its last one it is in the default flock |
| `pastor flock remove` | remove a flock; refused while it has machines or queued tasks, or is the default |
| `pastor flock default show` | print the default flock |
| `pastor flock default set` | make another flock the default; machines stay where they are |
| `pastor flock edit` | open `flock.toml` in `$VISUAL` or `$EDITOR`; saved only once valid |

## orchestrator

See [orchestrators](../../concepts/orchestrators/).

| command | does |
|---|---|
| `pastor orchestrator list` | every orchestrator file: kind, state, schedule, last and next run, its last agent |
| `pastor orchestrator describe` | one in full: settings, note, last runs with the lines each pre script printed, recent events |
| `pastor orchestrator run` | run a scheduled one now, ignoring its schedule and `enabled`; skipped while its last agent works |
| `pastor orchestrator start` | start a session one now, inside its hours or not; it stops at the next `hours.stop` |
| `pastor orchestrator stop` | stop a session one: a last message, then its agent is closed after `stop_grace` |
| `pastor orchestrator enable` | enable an orchestrator file |
| `pastor orchestrator disable` | disable one; an agent it started keeps running |
| `pastor orchestrator note` | keep the handover note every agent it starts gets (4 KiB at most); `--name` picks the orchestrator |

## connector

See [connectors](../../concepts/connectors/).

| command | does |
|---|---|
| `pastor connector install` | install one from GitHub, `owner/repo` or `owner/repo/subdir`; `--ref` for a branch, tag or commit |
| `pastor connector link` | use one from a local directory, in place, while you develop it |
| `pastor connector uninstall` | remove an installed one; its `.env` and state stay |
| `pastor connector unlink` | remove a linked one; the directory stays |
| `pastor connector list` | connectors: version, hooks, missing secrets |
| `pastor connector describe` | one in full: manifest, origin, commands, config, secrets, jobs |
| `pastor connector try` | run one once for `--job` and print its items; creates no tasks and saves no cursor. `pastor connector try prs watch` runs its `[watch]` command instead |

`pastor connector try --since` sets how far back `since` points; the default
is the job's `backfill`, else `0s`.

## trust

Folder trust: the repos whose trust prompt pastor answers, per machine. See
[profiles and trust](../../concepts/profiles-and-trust/).

| command | does |
|---|---|
| `pastor trust list` | every saved trust: machine, repo, when |
| `pastor trust add` | trust a repo on a machine ahead of its first task |
| `pastor trust remove` | forget a saved trust; the repo's next task asks again |

## profile

Permission profiles, built in and from `[profiles]` in
[pastor.toml](../pastor-toml/#profiles).

| command | does |
|---|---|
| `pastor profile list` | every profile: name, where it comes from, what it extends |
| `pastor profile describe` | one profile with its `extends` followed: the allow and deny lists it adds up to |

## head

Which head this CLI talks to. See [a remote head](../../deploy/remote/).

| command | does |
|---|---|
| `pastor head set` | use the head on another machine, over ssh; checked with one ping first (`--force` skips it, `--pastor` gives pastor's path there) |
| `pastor head show` | print the head this CLI uses |
| `pastor head unset` | use this machine's head again |

## events

`pastor events` prints the events log: `--task` for one task, `--follow` to
keep printing, `--json` for the records as stored. See [events](../events/).

## watch

`pastor watch` prints one line per change you would act on: a task that is
blocked, done, failed or stale, a failing job, the head going down, a
connector's lines. It keeps a cursor, so a watcher started again with the
same name repeats nothing.

| flag | does |
|---|---|
| `--now` | print what needs attention now and exit |
| `--name` | the watcher's name, for its cursor; default `default` |
| `--reset` | forget the cursor and start from the end of the log |
| `--all` | every task state change; with `--now`, every live task too |
| `--interval` | how often to look; default `1m` |
| `--connector` | run this connector's `[watch]` command; repeatable, replaces `[[watch.connector]]` |
| `--json` | one JSON record per line: `kind`, the line's fields, and `line` |

## serve

| command | does |
|---|---|
| `pastor serve` | start the head in the background and return once it answers; `-f, --foreground` keeps it in this terminal |
| `pastor serve status` | whether a serve runs here, head or headless: pid, version, service, log |
| `pastor serve stop` | stop the serve running here; agents keep running |

With a head set on another machine, `pastor serve` runs headless. See
[the head](../../concepts/head/).

## setup

Install pastor, or herdr with `--herdr`, as a user service. See
[run it as a service](../../deploy/service/).

| command | does |
|---|---|
| `pastor setup systemd` | install `pastor.service` (or `herdr.service`), then enable and start it |
| `pastor setup launchd` | install the `pastor.serve` (or `pastor.herdr`) launch agent on macOS, then load it |

`--enable`, `--start`, `--enable --now` and `--stop` pick another action.
`-y, --yes` skips the prompt; a script or a task needs it.

## config

| command | does |
|---|---|
| `pastor config edit` | open the head's `pastor.toml` in `$VISUAL` or `$EDITOR`; saved only once valid. `--local` edits this machine's |

See [pastor.toml](../pastor-toml/).

## tick

`pastor tick` runs one scheduler pass now and reports what it did. `--job`
runs only that job, due or not. `--dry-run` runs the connectors and shows
what would be created, writing nothing.

## other commands

| command | does |
|---|---|
| `pastor completions` | print a completion script for bash, elvish, fish, powershell or zsh |
| `pastor bridge` | pass request lines from stdin to this machine's head; what a remote CLI runs over ssh |
