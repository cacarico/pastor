---
title: examples
summary: setups taken from real use, to copy
group: use
weight: 24
manual: how-it-works
---

Five setups from how pastor is used every day, with the names swapped for
placeholders: `desk` is the head, `server-1` and `laptop` take tasks,
`~/notes` is a notes vault and `~/src/app` the repo the agents work on.

## a Kanban board that hands cards to agents

A board in an Obsidian vault is the to-do list. A card tagged `#agent` in
the `Ready` list becomes a task in its own worktree, one branch per card.

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
prompt = """
You are pastor task {{ task.id }}, started from the card "{{ item.title }}".
Do what the card asks, test it, commit and push the branch.
"""
```

The card moves itself: to In Progress when its task starts, In Review when
it is done, Failed when it fails. A card whose `depends-on` names another
card stays where it is until that one is done. Test the job before it runs
for real:

```sh
pastor tick --dry-run --job kanban
```

## answer questions in a note, let an agent carry them out

A second job on the same connector reads the `Answered` list, with no tag:
every card there is a question someone answered, and the agent does what
the answer says.

```toml
# ~/.config/pastor/jobs/answered.toml
description = "Carry out the answers on the app board"
every = "1m"

[connector]
use = "obsidian-kanban"
board = "~/notes/Boards/App.md"
list = "Answered"

[dispatch]
repo = "~/src/app"
worktree = true
branch = "answered/{{ item.key }}"
prompt = "Read the card \"{{ item.title }}\" and its answer, and carry it out."
```

## a PR fix round from one command

Review comments on PR `#42`: one command starts an agent on them, in a
worktree of its own, on a named model from `[models]` in `pastor.toml`.

```sh
pastor task run "Check out PR #42 with gh pr checkout 42, address every review comment, push" \
  --repo '~/src/app' --worktree --model sonnet
```

Then follow it, read what it did, and step in if it asks:

```sh
pastor task list
pastor task read t-12
pastor task attach t-12  # ctrl+b q to leave
```

## two accounts on one head

A personal and a work Claude account, each with its own machines, so the
two never run each other's agents. The work agent is Claude with its own
login:

```toml
# ~/.config/pastor/pastor.toml
[agents.claude-work]
kind = "claude"
env = { CLAUDE_CONFIG_DIR = "~/.claude-work" }
```

```toml
# ~/.config/pastor/flock.toml
[[flock]]
name = "personal"
default = true
agent = "claude"

[[flock]]
name = "work"
agent = "claude-work"

[[machine]]
name = "desk"
local = true
flock = "personal"

[[machine]]
name = "laptop"
ssh = "user@laptop"
flock = "work"
```

A task names the flock it belongs to; one that names none goes to
`personal`:

```sh
pastor task run "Fix the login redirect" --repo '~/src/app' --worktree --flock work
```

## agents on another machine over ssh

The head runs agents on itself and on `server-1`, which it reaches over ssh
with a key and no password. Each task goes to the one with the fewest live
tasks that still has room:

```sh
pastor machine add server-1 user@server-1 --max-agents 3 --herdr
pastor machine list
```

```text
pastor 0.5.0 on desk (herdr 0.9.1), 2 machines, desk is the head of the flock

NAME      HOST           FLOCKS    PROFILE  CHANNEL    HERDR  PASTOR  AGENTS     ORPHANS  TAGS  ERROR
desk      local          personal  -        connected  0.9.1  0.5.0   1/2+1j+1b  -        -
server-1  user@server-1  personal  -        connected  0.9.1  0.5.0   2/3+1j+1b  -        -
```

AGENTS is the load: live tasks against the room each machine has. To run the
CLI from the laptop against the head on `desk`, see [remote head](../remote-head/).

More in the [manual](../manual/#how-it-works).
