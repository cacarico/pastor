---
title: answer questions in a note
summary: an agent carries out your answer
weight: 2
---
Agents often stop at a decision only you can make. Here the question goes
in a note with a card on the board. You answer in the note when you have a
minute and move the card to `Answered`. The next run starts an agent that
reads your answers and carries them out, and the card moves to `Done`.

## what you need

- The board and connector from [hand board cards to agents](../board-cards/),
  with one more list, `Answered`.
- A question note linked from each card: `- [ ] Pick a cache for sessions [[Session cache]]`.
- The vault on `desk`, the head. The job pins its tasks there, since no
  other machine has the notes.

## the job

A second job on the same board. It reads `Answered` and takes every card
there: `tag = ""` turns the tag filter off. Leave `tag` out and the
connector falls back to `#agent`, and your answered cards never go out.

```toml
# ~/.config/pastor/jobs/answered.toml
description = "Carry out the answers on the app board"
every = "1m"

[connector]
use = "obsidian-kanban"
board = "~/notes/Boards/App.md"
list = "Answered"
tag = ""

[dispatch]
machine = "desk"
max_tasks_per_run = 3
timeout = "45m"
prompt = """
You are pastor task {{ task.id }}. The card "{{ item.title }}" was moved to
Answered: its questions have answers now. Nobody is watching; do not ask
questions.

The note:

{{ item.body }}

Act on every answer. Carry a decision into the notes it touches, and add a
card tagged #agent to Ready for work an agent can do alone. If an answer
raises a new question, add it to the same note with options and a
recommendation, and put a new card for it in To decide. Under each answer,
say what you did with it. The notes are in ~/notes. Do not move this card.
"""
```

The job has no `repo`: the agent starts in `~/pastor-tasks` on `desk` and
reaches the notes by path. The connector's hook moves the card to
`In Progress` while the task runs. With no repo there is no branch to
review, so the hook moves it on to `Done` when the task finishes, or to
`Failed` if it fails.

The key is still a hash of the card's text. A second round on the same note
needs new card text, such as `Pick a cache for sessions, round 2 [[Session cache]]`.

## what you see

Move a card to `Answered` and check before the minute is up:

```sh
pastor tick --dry-run --job answered
pastor task list --job answered
```

```text
JOB       OUTCOME  ITEMS  CREATED           SEEN  DEFERRED  ERROR
answered  dry_run  1      35c10ffc1bae3a76  0     0

ID    STATE    PRIORITY  MACHINE  FLOCK    AGENT   MODEL  JOB       AGE  NOTE
t-17  running  normal    desk     default  claude  -      answered  20s  Pick a cache for sessions [[Session cache]]
```

When it ends, the card reads
`Pick a cache for sessions [[Session cache]] (t-17 finished, no repo to review, outcome: done)`
in `Done`, and the note has a line under each answer.

## next

- [hand board cards to agents](../board-cards/): the first job on this board
- [jobs](../../concepts/jobs/) and [tasks](../../concepts/tasks/#how-it-ended)
- [job files](../../reference/job-files/)
