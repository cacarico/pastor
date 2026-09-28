---
title: events
summary: a log of every task, job and machine change
group: reference
weight: 42
manual: events
---

The head writes every task, job and machine event to a log, one JSON record
per line. Read it to see what happened overnight, or feed it to a script.

## read it

```sh
pastor events
pastor events --task t-3
pastor events --follow
pastor events --json
```

| flag | does |
|---|---|
| `--task` | only events about this task, like `t-3` or `3` |
| `--follow` | keep printing new events as they are written |
| `--json` | one record per line, as stored; the shape hooks get |

`pastor events` reads the file, not the head, so it works with
`pastor serve` down.

## the log

The log is `~/.local/state/pastor/events.jsonl`. Past 10 MiB it moves to
`events.jsonl.1`, replacing the one before, so at most two files are kept.
`pastor events` prints both, oldest first.

## kinds

| type | when |
|---|---|
| `task.queued`, `task.running`, `task.blocked` | a task changed state |
| `task.done`, `task.stale`, `task.failed`, `task.closed` | a task changed state |
| `task.input` | someone ran `pastor task send`; key names and text length, never the text |
| `task.trusted` | the head answered a folder-trust prompt |
| `job.failed` | a job run failed |
| `connector.finish_failed` | a connector's finish command failed |
| `machine.connected`, `machine.lost` | a machine's channel came up or went down |
| `orchestrator.started`, `orchestrator.skipped`, `orchestrator.held` | an orchestrator's run started an agent, was skipped while its agent works, or was held back by `max_orchestrators` or a quota |
| `orchestrator.quota`, `orchestrator.failed` | its agent stopped on a quota; its pre or post script failed |

## a record

| field | holds |
|---|---|
| `seq` | the record's number; grows across restarts and rotations, `0` when unnumbered |
| `at` | when the head got the event, RFC 3339 UTC |
| `type` | one of the kinds above |
| `task` | the full task row on `task.*` events, else `null` |
| `job` | the job name; `run` for a one-off task |
| `machine` | the machine's status on `machine.*` events, else `null` |
| `detail` | more, on the events that carry it |
| `summary` | on `task.done` and `task.failed`: how the round ended, its `outcome`, `text`, `source` (`agent` or `pane`), `round` and `at` |

Fields may be added; none are renamed or removed.

## hooks

A connector can run a command on the head for chosen event types, with the
record on stdin: a notifier for `task.blocked`, say. See event hooks in the
[manual](../manual/#event-hooks).

More in the [manual](../manual/#events).
