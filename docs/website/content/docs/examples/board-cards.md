---
title: hand board cards to agents
summary: a tagged card becomes a branch
weight: 1
---
A Kanban board in an Obsidian vault is the to-do list. Tag a card `#agent`
in the `Ready` list and, within a minute, an agent works on it in its own
worktree, on a branch named after the card. The card moves across the board
by itself as the task runs, so the board always shows what the agents are
doing.

## what you need

- A board made with the Obsidian Kanban plugin, at `~/notes/Boards/App.md`,
  with the lists `Ready`, `In Progress`, `In Review`, `Failed` and `Done`.
- The `obsidian-kanban` connector. It is not built in: install or link it
  first ([connectors](../../concepts/connectors/)), then
  `pastor connector describe obsidian-kanban` lists its keys.
- The vault on the head's disk. The connector runs where the job file is,
  on the head, and reads the board there.
- `~/src/app` cloned on every machine that takes tasks, and on the head,
  where the connector's hook checks each push and opens the PR with `gh`.

## the job

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
max_tasks_per_run = 20
prompt = """
You are pastor task {{ task.id }}, started from the card "{{ item.title }}".

{{ item.body }}

Work on your own and do not ask questions. Do what the card asks, run the
tests, commit, and push with git push origin HEAD:kanban/{{ item.key }}.
Do not open a pull request and do not touch main.
"""
```

Each unchecked card in `Ready` that carries the tag is one item. Its key is
a hash of the card's text, so a card goes out once; edit the text and it is
a new card. `item.title` is the card without the tag. When the card links a
note (`[[Login redirect]]`), `item.body` is that note's text, so a card can
carry a long description. The branch has a fixed prefix, `kanban/`, so no
card can point an agent at `main`.

A run sends at most 5 tasks unless `max_tasks_per_run` says more; 20 sends
every tagged card at once. Cards past the free slots wait in the
[queue](../../concepts/queue/).

## try it first

Tag two cards, then ask for one run that creates nothing:

```sh
pastor connector try obsidian-kanban --job kanban  # the items the board gives
pastor tick --dry-run --job kanban  # what a run would create
```

```text
JOB     OUTCOME  ITEMS  CREATED                            SEEN  DEFERRED  ERROR
kanban  dry_run  2      2155db486f75d7bb 480467041d2eecb8  0     0
```

In a dry run, CREATED lists the item keys a real run would turn into tasks.
Leave the job to its schedule, or fire it now with `pastor job run kanban`.

## what you see

```sh
pastor task list
```

```text
ID    STATE    PRIORITY  MACHINE   FLOCK    AGENT   MODEL  JOB     AGE  NOTE
t-13  running  normal    server-1  default  claude  -      kanban  1m   Add a --json flag to the export command
t-12  running  normal    desk      default  claude  -      kanban  1m   Fix the login redirect after sign-out
```

The connector ships a hook that moves each card as its task changes. A
queued or running task puts its card in `In Progress`. A pushed branch puts
it in `In Review`, with the number of the PR the hook opened. A task that stops
without pushing puts it in `Failed` with the reason, and so does one whose
summary says `blocked` or `partial`. The hook drops the tag as it moves the
card, so a card is never sent twice.

```text
## In Progress

- [ ] Fix the login redirect after sign-out (t-12, agent working on desk)

## In Review

- [ ] Add a --json flag to the export command (t-13 pushed kanban/480467041d2eecb8, PR #43, outcome: done)
```

## cards that wait

A card can wait for others. Give the note it links a `depends-on` list in
its frontmatter:

```text
---
stage: todo
depends-on: ["[[Session cache]]"]
---
```

The card stays in `Ready`, tag and all, until every note in `depends-on`
says `stage: done`. You can tag a whole plan at once and the cards start in
order.

## next

- [jobs](../../concepts/jobs/) and [connectors](../../concepts/connectors/)
- [answer questions in a note](../answered-notes/): a second job on the same board
- [job files](../../reference/job-files/)
