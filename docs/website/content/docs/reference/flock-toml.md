---
title: flock.toml
summary: flocks and machines
weight: 3
---
`~/.config/pastor/flock.toml` lists the machines the head runs agents on,
as `[[machine]]` tables, and the flocks they form, as `[[flock]]` tables.
See [machines](../../concepts/machines/) and
[flocks](../../concepts/flocks/) for what they are.

## example

```toml
# ~/.config/pastor/flock.toml
[[flock]]
name = "work"
default = true
machines = { desk = 2, server-1 = { share = 2, max = 4 } }
profile = "develop"

[[flock]]
name = "personal"
machines = { laptop = 1, server-1 = 1 }
timeout = "1h"

[[machine]]
name = "desk"
local = true

[[machine]]
name = "server-1"
ssh = "user@server-1"
max_agents = 4
tags = ["gpu"]

[[machine]]
name = "laptop"
ssh = "user@laptop"
```

A file with no `[[flock]]` has one flock, `default`, holding every machine.
Once you declare flocks, exactly one has `default = true`. A machine that no
flock lists is in the default flock.

## machine

| key | default | does |
|---|---|---|
| `name` | required | what pastor calls it, in tasks, jobs and agent names |
| `ssh` | | reach it over ssh: `user@server-1`, or a Host from your ssh config |
| `local` | `false` | this machine itself, through herdr's local socket |
| `pull` | `false` | the head never connects; the machine's own headless `pastor serve` claims tasks and reports back |
| `command` | | developer option: an argv that speaks the herdr protocol on stdio |
| `session` | `"default"` | the herdr session agents run in |
| `max_agents` | `2` | how many tasks it runs at once; at least 1 |
| `job_slots` | `1` | extra slots, on top of `max_agents`, that only job tasks take; `0` for none |
| `burst` | `1` | how many past `max_agents` a critical task may start; `0` for none |
| `tags` | `[]` | labels a task's `--tag` or a job's `tags` can ask for |
| `agent`, `agent_args` | unset | the agent for tasks here that name none, before the flock's |
| `agents` | `{}` | the agent for a model of another kind, by kind, as in `[defaults]` |
| `model` | unset | a `[models]` name for tasks here that name none, after the flock's |
| `priority` | unset | the level of tasks pinned here that name none, after the flock's |
| `profile` | unset | the profile for tasks here that name none, after the flock's; also whether a task may ask for `unrestricted` here |
| `description` | unset | one line on what it is for, for `pastor machine list --wide` |
| `flock` | unset | the old way to join one flock, with the machine's own limits; `machines` on the flock is the current way |

Set exactly one of `ssh`, `local`, `pull` and `command`. Unknown keys in a
`[[machine]]` are ignored, so check the spelling. `model`, `agents` and
`profile` must exist in [pastor.toml](../pastor-toml/).

## flock

| key | default | does |
|---|---|---|
| `name` | required | the flock's name |
| `default` | `false` | tasks and jobs that name no flock go here; exactly one flock has it |
| `machines` | `{}` | the machines it may use, each with a number: `{ desk = 2 }` |
| `agent`, `agent_args` | unset | the agent for its tasks that name none; `agent_args = []` means none, not `[defaults]` |
| `agents` | `{}` | the agent for a model of another kind, by kind |
| `allow`, `deny` | `[]` | tool patterns added to `[defaults]` for its tasks |
| `model` | unset | a `[models]` name for its tasks that name none |
| `priority` | unset | the level of its tasks that name none |
| `profile` | unset | the permission profile for its tasks that name none |
| `timeout` | unset | how long its tasks may run, like `"2h"` |
| `place` | unset | where its tasks' panes go: `repo`, `own`, `pastor` or `pane:<workspace>` |
| `label` | unset | the workspace name template for its tasks |
| `summary` | unset | `ask`, `require` or `off` for its tasks |
| `description` | unset | one line on what it is for, for `pastor flock list --wide` |

Unknown keys in a `[[flock]]` are refused. Each key applies when the task or
job sets nothing; the order against the machine's and `[defaults]` is in the
[`task run` flags](../cli/#task).

A machine's number in `machines` is one of two forms:

| form | means |
|---|---|
| `desk = 2` | at most 2 of this flock's live tasks on `desk` |
| `desk = { share = 2, max = 4 }` | up to 2 as usual; from 2 up to 4 only while no task of a flock under its share waits there |

A number is at least 1, and `max` is never below `share`. Job slots and
burst never take a machine past a flock's number.

## what the commands write

`pastor machine` and `pastor flock` edit this file in place: comments, key
order and blank lines stay. Each edit is refused if the result would not
load. With a head running, the head edits its own file and reloads it.

| command | writes |
|---|---|
| `pastor machine add` | a new `[[machine]]` at the end; `job_slots` and `burst` only when not 1; `--flock` writes the machine's `flock` key |
| `pastor machine remove` | removes the `[[machine]]` and its entry in every flock's `machines` |
| `pastor machine move` | removes the machine from every flock and writes it into one flock's `machines` with its `max_agents` |
| `pastor flock add` | a new `[[flock]]`, with `default = true` and `description` when asked; each machine named joins with its `max_agents` |
| `pastor flock join` | the machine's entry in `machines`: `--max`, else its number there, else its `max_agents` |
| `pastor flock leave` | removes that entry, and `machines` when it is left empty |
| `pastor flock remove` | removes the `[[flock]]` |
| `pastor flock default set` | moves `default = true`; machines no flock lists get `flock = "<old default>"`, so they stay where they are |
| `pastor flock edit` | whatever you save in your editor, once it loads |

When a file with no `[[flock]]` gets its first flock, pastor first writes the
implicit one as `[[flock]] name = "default"`, so its machines keep their
flock. `pastor flock add --default` on such a file instead takes the
implicit flock's place and its machines, unless tasks are queued there.

The first time a machine with a `flock` key is joined, left or moved, the
key becomes an entry in that flock's `machines`, with the machine's
`max_agents`.
