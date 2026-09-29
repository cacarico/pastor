---
title: job files
summary: one TOML file per job
weight: 4
---
A job is one TOML file in `~/.config/pastor/jobs/`, and the file name
without `.toml` is the job's name. See [jobs](../../concepts/jobs/) for how
jobs run; this page lists what a file may hold.

## example

```toml
# ~/.config/pastor/jobs/kanban.toml
description = "Hand #agent cards on the app board to agents"
every = "1m"

[connector]
use = "obsidian-kanban"
board = "~/notes/Boards/App.md"
list = "Ready"
tag = "#agent"

[dispatch]
repo = "~/src/app"
worktree = true
branch = "kanban/{{ item.key }}"
max_tasks_per_run = 3
prompt = """
You are pastor task {{ task.id }}, started from the card "{{ item.title }}".
Do what the card asks, test it, commit and push the branch.
"""
```

Everything in `[connector]` but `use` is the connector's own config. The
full walk-through is in [board cards](../../examples/board-cards/).

A file that does not load shows as `invalid` in `pastor job list`, with the
reason. Unknown keys are refused at the top level and in `[dispatch]`.

## top-level keys

| key | default | does |
|---|---|---|
| `every` | | run this often: `30s`, `15m`, `2h`, `1d`; not zero |
| `cron` | | run on a cron schedule in the head's local time |
| `enabled` | `true` | `false` stops scheduled runs; `pastor job enable` and `pastor job disable` rewrite this line |
| `description` | none | one line on what the job does, for `pastor job list --wide` and `pastor job describe` |
| `name` | the file name | if set, it must match the file name |
| `[connector]` | required | where the job finds its items |
| `[dispatch]` | required | the task each item becomes |

Set exactly one of `every` and `cron`. A name is lowercase letters, digits,
`_`, `.` and `-`, starting with a letter or digit, at most 64 characters,
and not `run`, which one-off tasks use.

`cron` takes five fields: minute, hour, day of month, month, day of week.
Each is `*`, `n`, `a-b`, `a-b/s`, `*/s` or a comma list of those. Sunday is
0 or 7, and names like `mon` are not accepted. When both day fields are set,
a day matches if either does.

## connector

| key | does |
|---|---|
| `use` | the connector's id: `clock`, built in, or one from `pastor connector install` |
| any other key | handed to the connector as its config; its manifest says which it needs |

The `clock` connector takes no config. It hands over one item per run,
keyed by the run time, with `key`, `at` and `title` fields. See
[connectors](../../concepts/connectors/) for the others.

The connector runs on the machine whose `jobs/` holds the file: the head, or
a machine running a headless `pastor serve`. `[dispatch] machine` only picks
where the agent runs.

## dispatch

Each key applies to every task the job queues. Where a key is left out, the
task's flock, its machine or `[defaults]` in
[pastor.toml](../pastor-toml/#defaults) fill it in, as for
[`pastor task run`](../cli/#task).

| key | default | does |
|---|---|---|
| `prompt` | required | the prompt template for each item |
| `description` | `"{{ item.title }}"` | each task's description, a template; empty falls back to the prompt's first line |
| `repo` | none | the repo the agent works in, a path on the machine that runs it; a template |
| `worktree` | `false` | a git worktree per task, branched from `repo`; needs `repo` |
| `branch` | `pastor/<task id>` | the worktree's branch; a template whose first part must be fixed |
| `machine` | none | run every task on this machine |
| `flock` | the machine's flock, else the default flock | only this flock's machines take the tasks |
| `tags` | `[]` | only a machine with all these tags takes a task |
| `agent` | the machine's, the flock's, `[defaults]` | the agent to start |
| `agent_args` | the machine's, the flock's, `[defaults]` | arguments for the agent; `[]` means none |
| `allow`, `deny` | `[]` | tool patterns added to the flock's and `[defaults]` |
| `model` | the flock's, the machine's, `[defaults]` | a `[models]` name, or a template of one; rendered empty, the next layer applies |
| `fallback` | the machine's, the flock's, `[defaults]` | `[models]` names the tasks may fall back to, each a template; entries rendered empty are dropped, and `[]` means none |
| `priority` | the flock's, the pinned machine's, `[defaults]`, normal | `low`, `normal`, `high`, `critical`, or a template of one; rendered empty, the next layer applies |
| `preempt` | `false` | a task that ends up critical may pause a low Claude task on a full machine |
| `profile` | the flock's, the machine's, `[defaults]` | a permission profile, by name; not a template |
| `summary` | the flock's, `[defaults]`, ask | `ask`, `require` or `off` |
| `timeout` | the flock's, `[defaults]` | mark a task stale after this long |
| `place` | the flock's, `[defaults]` | where a task's pane goes: `repo`, `own`, `pastor` or `pane:<workspace>` |
| `label` | the flock's, `[defaults]` | the workspace name template |
| `max_tasks_per_run` | `[defaults]`, 5 | at most this many tasks per run; the rest stay unseen for the next run |
| `backfill` | `"0s"` | on the first run, the connector's `since` points this far back |

`preempt = true` next to a fixed `priority` below critical makes the file
invalid. A task whose level comes out below critical loses the flag.

A rendered `priority` that is not a level refuses that item. A rendered
`model`, and each rendered `fallback` entry, must be a model name; whether
`[models]` has it is checked when the task is queued.

## templates

A template swaps `{{ path }}` for a value, nothing more: no conditions, no
filters. A string goes in as is, `null` as nothing, anything else as
compact JSON. A path the item lacks renders empty and is logged.

| variable | is | allowed in |
|---|---|---|
| `{{ item.<field> }}` | a field of the item, as the connector sent it | `prompt`, `description`, `repo`, `branch`, `model`, `fallback`, `priority` |
| `{{ item.key }}` | the item's key; every item has one | the same, and `label` |
| `{{ job.name }}` | the job's name | `prompt`, `description`, `repo`, `branch`, `model`, `fallback`, `priority` |
| `{{ task.id }}` | the task's id, like `t-12` | `prompt`, `repo`, `branch`, `label` |
| `{{ flock }}`, `{{ machine }}`, `{{ job }}` | the task's flock, machine and job | `label` only |

Any other placeholder makes the file invalid. Control characters in item
values are stripped before they reach the prompt.

## item values in repo and branch

An item comes from outside, such as an issue title, so pastor checks every
`{{ item.* }}` value before it goes into `repo` or `branch`. The value must
be one plain path component. It is refused when it:

- is empty or `.`, which covers a field the item lacks
- contains `/` or `\`
- contains `..` anywhere
- starts with `-`
- contains a control character

Then the rendered path as a whole must keep the template's number of path
components, and must not turn absolute when the template is not. An item
that fails is skipped and reported in the run's result; the job goes on
with the rest. Text you write yourself around the placeholder, like
`~/src/`, is not checked.

In `branch`, the first component may not hold an item value at all, so an
item can never name `main`. A file with `branch = "{{ item.key }}"` is
invalid; write `branch = "pastor/{{ item.key }}"`.
