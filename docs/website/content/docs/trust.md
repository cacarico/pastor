---
title: trust model
summary: what an agent can reach, and what stops it
group: reference
weight: 43
manual: trust-model
---

An agent that pastor starts runs as the head's user on its machine, with that
user's files, keys, tokens and network. Read this before you give a flock
work from input you do not control.

## the risk

A prompt is not always yours. A job fills it from an issue or a message; a
repo holds READMEs and test output; the agent may fetch web pages. Any of
these can carry text written to steer the agent. What stands between that
text and the machine is the agent's own permission checks.

## permission prompts

pastor leaves the agent's permission mode alone. When the agent asks before
a tool, the task goes `blocked` until you answer. `allow` and `deny` lists
answer some of those questions in advance, and deny always wins.

```toml
# pastor.toml
[defaults]
allow = ["Read", "Edit", "Bash(git:*)"]
deny = ["WebFetch", "Bash(rm:*)"]
```

- Keep the prompts on, and make `allow` name only what a task needs.
- An `agent_args` entry that skips the agent's permissions removes every
  check, and no allow list applies. Use one only on machines you could wipe,
  never for jobs fed by outside input.
- Read a blocked task's pane before you answer it.

## agents and the fleet

pastor sets `PASTOR_TASK` in every agent's pane, and refuses commands from
there that change the fleet: running, retrying, closing or pruning tasks,
`send`, `attach`, `tick`, running or reloading jobs, connector changes, edits
of machines, flocks, jobs and `pastor.toml`, `serve`, `setup` and
`machine open`. Reads still work, and an agent may end its own task with
`pastor task done`. The refusal is `agent_refused`.

```toml
# pastor.toml
agents_change_fleet = true  # turn the refusal off
```

An orchestrator (`task run --role orchestrator`, or any agent an
[orchestrator](../orchestrators/) file starts) may also run, retry, send to
and close tasks, enable and disable jobs and keep its handover note; its pre
and post scripts (`PASTOR_ORCHESTRATOR`) get the same table. Only a person
starts an orchestrator.

This stops an agent acting on its own, not a determined one: it runs as the
same user and can unset the variable. Anything running as the head's user
controls the fleet, so do not give a `local = true` machine untrusted work.
A flock is a routing rule, not a sandbox.

## folder trust

An agent in a folder it has not seen stops at its folder-trust prompt, and a
worktree is always a new folder. Answer it once with `--trust`, and pastor
saves the repo as trusted on that machine. The head then answers that prompt
for the repo's later tasks there, and logs `task.trusted`.

```sh
pastor task read t-3
pastor task send t-3 --trust
pastor trust list
pastor trust remove pi-1 '~/work/api'  # its next task asks again
```

## what pastor closes

pastor closes only the panes and workspaces it created. A done task's pane
closes after `close_done_after`, and its worktree goes only if it is clean:
no uncommitted changes and no unpushed commits. Failed, stale and blocked
tasks stay until you act.

More in the [manual](../manual/#trust-model).
