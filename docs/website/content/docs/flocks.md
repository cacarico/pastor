---
title: flocks
summary: named groups of machines
group: use
weight: 23
manual: flocks
---

A flock is a named group of machines. Every task and job targets one flock,
and only its machines take the work, so work and personal machines, logged in
to different accounts, never run each other's agents.

## flock.toml

```toml
# ~/.config/pastor/flock.toml
[[flock]]
name = "personal"
default = true  # tasks and jobs that name no flock go here

[[flock]]
name = "work"
agent = "claude"
agent_args = ["--model", "claude-sonnet-5"]

[[machine]]
name = "here"
local = true  # no flock: the default one

[[machine]]
name = "pi-1"
ssh = "user@pi-1"
flock = "work"
tags = ["arm"]
```

Exactly one flock has `default = true`. A file with no `[[flock]]` at all is
one flock named `default` that holds every machine.

## which agent

`agent` and `agent_args` on a flock set the agent its tasks run when they name
none. The same keys on a `[[machine]]` set it for that one machine. A task
takes the first of: its own `--agent`, its machine, its flock, `[defaults]` in
`pastor.toml`, then plain `claude`.

An agent can be a definition in `pastor.toml`, such as a second Claude
account:

```toml
[agents.claude-personal]
kind = "claude"
env = { CLAUDE_CONFIG_DIR = "~/.claude-personal" }
```

Then `agent = "claude-personal"` on a flock or machine runs Claude with that
config. `pastor task describe` shows which agent a task got and where from.

## manage them

```sh
pastor flock list
pastor flock describe work
pastor machine move pi-1 work  # tasks already on it stay there
pastor flock edit  # saved only once valid
```

## tags

A machine's `tags` pick machines inside a flock. A task run with `--tag`
goes only to a machine that carries every tag it asks for.

```sh
pastor task run "Build the image" --repo '~/work/api' --flock work --tag arm
```

More in the [manual](../manual/#flocks).
