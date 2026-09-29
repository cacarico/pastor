---
title: events
summary: what happened, and when
weight: 5
aliases:
  - /docs/events/
---
The head writes every task, job, machine and orchestrator event to a log,
one JSON record per line. Read it to see what happened overnight, or feed
it to a script. Connector hooks get the same records.

## read it

```sh
pastor events                # every record, oldest first
pastor events --task t-12    # one task's records
pastor events --follow       # keep printing new ones
pastor events --json         # the records as stored
```

| flag | does |
|---|---|
| `--task` | only records about this task, like `t-12` or `12` |
| `--follow` | keep printing records as they are written |
| `--json` | one JSON record per line, the shape hooks get on stdin |

```text
2026-09-29 09:00:04  task.queued        t-12  queued  job=morning
2026-09-29 09:00:09  task.running       t-12  server-1  running  job=morning
2026-09-29 09:41:30  task.done          t-12  server-1  done  job=morning
2026-09-29 10:02:11  machine.lost       laptop  reconnecting  connection refused
```

On the head's machine, `pastor events` reads the file, so it works with
`pastor serve` down. With a remote head it asks the head instead, and
`--follow` asks again every second.

For the changes you would act on, one line each, use
[`pastor watch`](../cli/#watch).

## the log

The log is `~/.local/state/pastor/events.jsonl`. Past 10 MiB it moves to
`events.jsonl.1`, replacing the one before, so at most two files are kept.
`pastor events` reads both, oldest first.

## event types

A task's state change is `task.` and the new state.

| type | when | `detail` |
|---|---|---|
| `task.queued` | a task was created: run, job, retry or orchestrator | |
| `task.running` | its agent took the prompt, or picked the work back up | |
| `task.blocked` | its agent waits on a person | `question`, when it ended its turn on one |
| `task.paused` | a critical task took its slot; it resumes later | |
| `task.done` | it finished | |
| `task.failed` | it failed | |
| `task.stale` | its timeout passed, or its pull machine was lost | |
| `task.closed` | its pane was closed | |
| `task.input` | someone ran `pastor task send` | `keys`, `text_len`, and `trust` with `--trust`; never the text |
| `task.trusted` | the head answered a folder-trust prompt from saved trust | `keys` |
| `job.failed` | a job run failed | |
| `connector.finish_failed` | a connector's finish command failed | `connector`, `reason` |
| `machine.lost` | a machine stopped answering; once per outage | |
| `machine.connected` | a lost machine answers again | |
| `agent.exhausted` | the head recorded an account (or one model of it) as out of usage | `account`, `model`, `agent`, `until`, `retry_at`, `hard`, `no_credit`, `what`, `line` |
| `agent.reset` | an exhausted account can be used again | `account`, `model`, `agent`, `retry_at`, `by` (`time` or `hand`) |

Orchestrator events all carry `orchestrator`, its name, in `detail`. See
[orchestrators](../../concepts/orchestrators/).

| type | when | more in `detail` |
|---|---|---|
| `orchestrator.started` | a run started an agent | scheduled: `lines`; session: `by` (`hours` or `hand`), `until` |
| `orchestrator.skipped` | a run was skipped | `reason`: `busy` or `post_pending` |
| `orchestrator.held` | a run was held back | `reason`: `max_orchestrators` (with `max`), `quota` (with `until`) or `restarts` (with `max`, `until`) |
| `orchestrator.quota` | its agent stopped on a usage limit | `until` |
| `orchestrator.failed` | a script failed, its agent could not start or failed, or its saved state could not be read | `stage` (`pre`, `agent`, `post` or `state`), `error`; `failures` on `pre` |
| `orchestrator.restarted` | a session's agent ended early and a new one took over | `after`, the old task; `restarts` in the last hour |
| `orchestrator.stopping` | a session's agent got its last message | `reason` (`hours` or `hand`), `grace` |
| `orchestrator.stopped` | the session ended | `reason` |

## a record

```json
{
  "seq": 812,
  "at": "2026-09-29T09:41:30.123Z",
  "type": "task.done",
  "task": {"id": 12, "job": "morning", "state": "done", "machine": "server-1", "...": "..."},
  "model": "sonnet",
  "job": "morning",
  "machine": null,
  "summary": {"round": 1, "outcome": "done", "text": "done: PR #42", "source": "agent", "at": "2026-09-29T09:41:29.900Z"}
}
```

| field | holds |
|---|---|
| `seq` | the record's number; it only grows, across restarts and rotations; `0` on old lines |
| `at` | when the head got the event, RFC 3339 UTC |
| `type` | one of the types above |
| `task` | the full task row, as `pastor task describe --json` prints it, on events about a task; else `null` |
| `model` | the `[models]` name the task runs; absent when it runs none |
| `job` | the job's name; `run` for a one-off task; `null` on events with no job |
| `machine` | on `machine.*` events, the machine's status as in `pastor machine list --json`; else `null` |
| `detail` | more, on the types that carry it; absent otherwise |
| `summary` | on `task.done` and `task.failed`: how the round ended, with `round`, `outcome`, `text`, `source` (`agent` or `pane`) and `at` |

`outcome` is `done`, `partial`, `blocked`, `nothing to do`, `unknown` or
`no summary`. See [how it ended](../../concepts/tasks/#how-it-ended).

Fields may be added; none are renamed or removed. A line that does not
parse is skipped.

## hooks

A connector can run a command on the head for the event types it names,
with the record on stdin: a notifier for `task.blocked`, say. A hook that
does not own a task's job gets the task without its item, prompt or summary
text. See [connectors](../../concepts/connectors/).
