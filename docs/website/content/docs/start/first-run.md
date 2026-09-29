---
title: first run
summary: one machine, one task, one answer
weight: 2
aliases:
  - /docs/first-run/
---
A first session on one machine: it is the head and it runs the agent too.
You need pastor, herdr with its server running, `claude`, and a git repo on
this machine (`~/src/app` below). No second box, no ssh.

## add this machine

```sh
pastor machine add here --local
```

This writes `~/.config/pastor/flock.toml` with one machine, `here`, reached
through herdr's local socket. It lands in the default flock and takes two
tasks at once, plus one slot for jobs.

## start the head

```sh
pastor serve
```

The head starts in the background and the command returns once it answers.
It logs to `~/.local/state/pastor/serve.log`. `pastor serve status` shows it
and `pastor serve stop` stops it. To have it start at login instead, see
[as a service](../../deploy/service/).

```sh
pastor machine list
```

```text
pastor 0.8.0 on desk (herdr 0.9.1), 1 machine, here is the head of the flock

NAME  HOST   FLOCKS   PROFILE  CHANNEL    HERDR  PASTOR  AGENTS     ORPHANS  TAGS  ERROR
here  local  default  -        connected  0.9.1  0.8.0   0/2+1j+1b  -        -
```

The first line is the head: its version, its host (`desk`) and its herdr.
Then one row per machine. `connected` means the head reaches its herdr and
can place tasks there. AGENTS reads `0/2+1j+1b`: no agents live, room for
two, plus one job slot and one burst slot for critical tasks.

## run a task

```sh
pastor task run "Fix the flaky test in ci.yml" --repo '~/src/app' --worktree
```

```text
ID   STATE   PRIORITY  MACHINE  FLOCK    AGENT   MODEL  JOB  AGE  NOTE
t-1  queued  normal    -        default  claude  -      run  0s   Fix the flaky test in ci.yml
```

`--repo` is a path on the machine that runs the agent, so quote the `~` and
let that machine expand it. `--worktree` gives the agent a git worktree of
its own, branched from the repo, so your checkout stays as it is. With no
`--agent`, pastor starts `claude`. A moment later the task has a machine.

## watch it

```sh
pastor task list
```

```text
ID   STATE    PRIORITY  MACHINE  FLOCK    AGENT   MODEL  JOB  AGE  NOTE
t-1  blocked  normal    here     default  claude  -      run  14s  Fix the flaky test in ci.yml
```

`blocked` means the agent waits for you. Read its screen:

```sh
pastor task read t-1  # the last 40 lines of its pane
```

Claude asks whether you trust the files in this folder. The worktree is a
folder it has never seen, so it asks before it does anything. pastor holds
the prompt back until that question is answered.

## answer it

```sh
pastor task send t-1 --trust
```

`--trust` presses the keys that accept Claude's prompt and saves the repo as
trusted on this machine. The next task in `~/src/app` here is answered for
you, and `pastor trust list` shows what is saved. pastor then sends the
prompt, and the task goes `running`.

Other questions get text: `pastor task send t-1 "yes, go ahead"`. To sit in
the terminal yourself, `pastor task attach t-1`, and `ctrl+b q` to leave.

## read the result

When the agent is finished it runs `pastor task done` with a short summary,
because pastor adds that request to the prompt by default. The task goes `done`.

```sh
pastor task read t-1  # what the agent printed last
pastor task describe t-1  # everything, the summary near the end
```

```text
...
summary:    done (round 1, from the agent, 1m ago)
  done
  Fixed the race in the retry test: it now waits for the mock server.
  One commit on pastor/t-1, not pushed.
prompt:
  Fix the flaky test in ci.yml
  ...
```

The first line of a summary is its outcome: `done`, `partial`, `blocked` or
`nothing to do`. An agent that stops without one gets `no summary`, and
pastor keeps the pane's last lines instead. `done` means the agent stopped,
not that the work is right: read the diff before you trust it.

## see it closed

`pastor task list` shows live tasks only, so the finished task drops out of
it. A `done` task keeps its pane for 5 seconds (`close_done_after`; set it
longer to attach and look). Then pastor closes the pane and the task shows
`closed`. Add `--all` to see it:

```sh
pastor task list --all
```

```text
ID   STATE   PRIORITY  MACHINE  FLOCK    AGENT   MODEL  JOB  AGE  NOTE
t-1  closed  normal    here     default  claude  -      run  22m  worktree kept: commits on no remote on branch pastor/t-1 of ~/src/app; ...
```

pastor removes a worktree only when it is clean. This one holds a commit no
remote has, so it stays, and the note says where. Push or merge the branch,
then remove the checkout with `git worktree remove`. To close a task yourself,
run `pastor task close t-1`.

More on states, retries and summaries in [tasks](../../concepts/tasks/).

Running commands by hand is cool, but pastor is even more powerful in the hands of agents.

[Let your agent drive pastor](../../examples/agent-drives/).
