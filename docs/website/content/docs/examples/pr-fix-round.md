---
title: a PR fix round
summary: review comments fixed from one command
weight: 3
---
A reviewer left comments on PR `#42`. Instead of switching branches
yourself, one command starts an agent on them, in a worktree of its own, on
the model you pick. You carry on with your own work and step in only when
it asks for you.

## what you need

- `~/src/app` cloned on the machines that take tasks, with `gh` logged in there.
- A model named in `pastor.toml`. No model is built in, so `--model sonnet`
  needs this entry:

```toml
# ~/.config/pastor/pastor.toml
[models.sonnet]
kind = "claude"
args = ["--model", "sonnet"]
```

`kind` is the agent that can run it, and `args` go before the agent's own.
Set `model = "sonnet"` under `[defaults]`, on a flock or on a machine to
make it the default there.

## run it

```sh
pastor task run "Check out PR #42 with gh pr checkout 42, address every review comment, push" \
  --repo '~/src/app' --worktree --model sonnet
```

```text
ID    STATE   PRIORITY  MACHINE  FLOCK    AGENT   MODEL   JOB  AGE  NOTE
t-12  queued  normal    -        default  claude  sonnet  run  0s   Check out PR #42 with gh pr checkout 42, address every revie
```

Quote the `~`: the path is on the machine that runs the agent. The task
goes to the machine with the fewest live tasks that has room.

## follow it

```sh
pastor task list
```

```text
ID    STATE    PRIORITY  MACHINE   FLOCK    AGENT   MODEL   JOB  AGE  NOTE
t-12  blocked  normal    server-1  default  claude  sonnet  run  40s  Check out PR #42 with gh pr checkout 42, address every revie
```

`blocked` means the agent waits for you. The first time a worktree of this
repo opens on `server-1`, Claude asks whether to trust the folder. Accept it
once, and pastor answers it for this repo on that machine from then on:

```sh
pastor task send t-12 --trust
```

Then read what the agent is doing, the last 40 lines of its pane by default:

```sh
pastor task read t-12
```

```text
⏺ Three review comments on PR #42:
  1. rename parseOpts to parseOptions (src/cli.ts)
  2. handle an empty config file (src/config.ts)
  3. add a test for the redirect (test/login.test.ts)

⏺ 1 and 3 are done. For 2: should an empty config file be an error,
  or the same as no file?
```

## step in

Answer from where you are. The text is typed into the agent, then Enter:

```sh
pastor task send t-12 "Same as no file: fall back to the defaults"
```

```text
sent text and 1 key to t-12
```

Or sit in its terminal and take over for a while:

```sh
pastor task attach t-12  # ctrl+b q to leave
```

When the agent pushes and stops, the task is `done`. `pastor task describe t-12`
shows its summary. Close it with `pastor task close t-12 --remove-worktree`,
or leave it: pastor closes a done task's pane after 5 seconds.

## next

- [tasks](../../concepts/tasks/): states, answering, how a task ended
- [agents and models](../../concepts/agents-and-models/)
- [profiles and trust](../../concepts/profiles-and-trust/#folder-trust)
- [cli: task](../../reference/cli/#task) and [pastor.toml](../../reference/pastor-toml/)
