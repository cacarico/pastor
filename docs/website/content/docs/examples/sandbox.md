---
title: untrusted work in a sandbox
summary: outside PRs and issues, away from your credentials
weight: 6
---
Some tasks make an agent read text a stranger wrote: a PR from an outside
contributor, a new issue, a log someone pasted. That text can carry a
prompt injection, words written to steer the agent. On a machine that
holds your push keys, your `gh` login and your main agent account, a
steered agent can use all of them. So run that work on a machine where
there is nothing to steal.

## what you need

- Your everyday machines, `desk` (the head) and `server-1`, with their real
  keys and logins. Nothing changes there.
- One throwaway machine or VM, `sandbox-1`, that you could wipe tomorrow.
  It has herdr, and it has none of your credentials: no ssh key that can
  push, no `gh` login, no cloud tokens.
- A fresh clone over https, which needs no key to fetch a public repo:
  `git clone https://github.com/owner/app ~/src/app` on `sandbox-1`.
- A second, low-limit agent account, logged in on `sandbox-1` only:
  `CLAUDE_CONFIG_DIR=~/.claude-sandbox claude`, once. If a steered agent
  burns through it, it hits that account's limit, not yours.

The head reaches `sandbox-1` over ssh, as it reaches any machine. That key
lives on the head, not on `sandbox-1`.

## the sandbox agent

```toml
# ~/.config/pastor/pastor.toml
[agents.claude-sandbox]
kind = "claude"
env = { CLAUDE_CONFIG_DIR = "~/.claude-sandbox" }
```

pastor sets `env` in the task's pane, and `~/` means the home of the
machine that runs the task. `kind = "claude"` keeps Claude's trust keys and
tool flags.

## the flocks

```toml
# ~/.config/pastor/flock.toml
[[flock]]
name = "default"
default = true
machines = { desk = 2, server-1 = 3 }

[[flock]]
name = "sandbox"
description = "Outside PRs and issues, on a machine with no credentials"
machines = { sandbox-1 = 1 }
agent = "claude-sandbox"
profile = "review"
allow = ["Bash(git fetch:*)", "Bash(pastor task done:*)"]
deny = [
  "Bash(git push:*)",
  "Bash(gh:*)",
  "Bash(curl:*)",
  "Bash(wget:*)",
  "Bash(ssh:*)",
  "WebFetch",
  "WebSearch",
]

[[machine]]
name = "desk"
local = true

[[machine]]
name = "server-1"
ssh = "user@server-1"
max_agents = 3

[[machine]]
name = "sandbox-1"
ssh = "user@sandbox-1"
```

A task that names no flock goes to `default`, so everyday tasks never land
on `sandbox-1`. A `sandbox` task runs only on `sandbox-1`, and starts
`claude-sandbox` under the built-in `review` profile. `review` lets the
agent read files and run `git status`, `diff`, `log`, `show` and `blame`,
and denies edits and `git push`. Under a profile, Claude runs with
`--permission-mode dontAsk`: a tool the lists do not allow is refused, not
asked about.

`allow` adds two commands `review` lacks: `git fetch`, to get the PR's
commits, and `pastor task done`, so the agent can hand in its summary.

`deny` is a second layer. A task can ask for another profile with
`--profile`, but denies add up across `[defaults]`, the flock and the task,
and nothing lifts one. So a `sandbox` task run with `--profile develop` is
still refused `git push`, `gh`, `curl`, `wget`, `ssh` and the web tools.

Trust the clone once, so the first task does not stop at Claude's folder
prompt:

```sh
pastor trust add sandbox-1 '~/src/app'
```

## run it

```sh
pastor task run "Read PR #42 from an outside contributor and list what it changes and anything risky" --flock sandbox --repo '~/src/app'
pastor task list --flock sandbox
```

```text
ID    STATE    PRIORITY  MACHINE    FLOCK    AGENT           MODEL  JOB  AGE  NOTE
t-31  running  normal    sandbox-1  sandbox  claude-sandbox  -      run  1m   Read PR #42 from an outside contributor and list what it cha
```

The agent fetches the PR (GitHub serves `pull/42/head` over https with no
login for a public repo), diffs it and reads the code around it. Watch it,
then read what it found:

```sh
pastor task read t-31 --lines 80  # the pane's last 80 lines
pastor task describe t-31         # the summary, the agent, the profile
```

`task describe` shows `summary: done (round 1, from the agent, ...)` when
the agent handed one in, and the pane's last lines when it did not. The
agent can report. It cannot push, and your accounts are not on the machine
for it to reach.

## every new issue

A job sends its tasks to the sandbox with `flock` in `[dispatch]`. The
connector runs on the head, with the head's GitHub token; only the prompt
goes to `sandbox-1`.

```toml
# ~/.config/pastor/jobs/new-issues.toml
every = "10m"

[connector]
use = "github-issues"
repo = "owner/app"

[dispatch]
repo = "~/src/app"
flock = "sandbox"
prompt = """
Issue #{{ item.key }}, "{{ item.title }}", came from outside. Read the code
it points at and say where the problem likely is and what a fix would
change. Change nothing.
"""
```

Leave `agent` and `profile` out of the job: a job's own come before the
flock's.

## what this protects, and what it does not

pastor keeps these tasks on `sandbox-1` and starts them with the agent,
profile and lists you set. That is all pastor does. The isolation comes
from what `sandbox-1` does not have.

- The tool lists are Claude Code's patterns. They stop the obvious
  commands, but a steered agent may find another way to the network. Treat
  them as a second layer, not the wall.
- A task can name another agent with `--agent`. On `sandbox-1` there is no
  other login for it to use.
- The report-back key from
  [flock](../../deploy/flock/#let-agents-report-back) is fine on
  `sandbox-1`: it only runs `pastor bridge --agent --machine sandbox-1`,
  which lists its flocks' tasks and reads or ends the tasks on its own
  machine. Without it, pastor still marks the task done and keeps the
  pane's last lines.
- The summary and the pane are text from an agent that read a stranger's
  text. Read them as such, and think before a connector posts them
  anywhere.
- A flock is a routing rule. A `sandbox` flock on your own desk protects
  nothing.

## next

- [flocks](../../concepts/flocks/) and [profiles and trust](../../concepts/profiles-and-trust/)
- [agents and models](../../concepts/agents-and-models/)
- [flock.toml](../../reference/flock-toml/)
