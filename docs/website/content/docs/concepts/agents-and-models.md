---
title: agents and models
summary: which coding agent a task starts
weight: 8
---
The agent is the coding agent a task starts in its pane: `claude`, `codex`,
`opencode`, or any other agent herdr knows. The model is which model that
agent runs. Both can be set once, on a flock, a machine or in `[defaults]`,
so most tasks never name them.

## which agent a task starts

A task takes its agent from the first of these that names one:

1. `--agent` on `pastor task run`, or `agent` in a job's `[dispatch]`;
2. the machine it runs on, in `flock.toml`;
3. its flock, in `flock.toml`;
4. `[defaults]` in `pastor.toml`;
5. plain `claude`.

The machine comes before the flock because it knows what is installed and
logged in there. Arguments follow the agent they were written for:
`agent_args` on a layer apply only when that layer names no agent, or names
the one the task runs. So a flock's `--model` flag for Claude never reaches
a task that asked for codex.

A flock that names an agent also names its kind. A machine whose own agent
is of another kind runs its `agents` entry for the flock's kind instead. A
machine with no such entry is skipped for that flock's tasks, and a task
pinned there is refused (`agent_kind_missing`).

## named agents

`[agents.<name>]` in `pastor.toml` defines an agent by name. `kind` says
which herdr agent it starts, and `env` sets environment variables in its
pane. The usual case is a second Claude account, such as a low-limit one
for a sandbox machine that holds none of your credentials:

```toml
# ~/.config/pastor/pastor.toml
[agents.claude-sandbox]
kind = "claude"
env = { CLAUDE_CONFIG_DIR = "~/.claude-sandbox" }  # ~ is the task's machine's home
```

```toml
# fragment of ~/.config/pastor/flock.toml
[[flock]]
name = "sandbox"
machines = { sandbox-1 = 1 }
agent = "claude-sandbox"  # every task of sandbox runs it
```

herdr starts a `claude`, and `task list` shows `claude-sandbox`. A definition
gets the built-in settings of its kind, such as Claude's folder-trust keys
and tool flags. For an agent pastor has no built-ins for, the definition
can set `trust_keys`, `trust_marker`, `allow_flag` and `deny_flag`.

## named models

`[models.<name>]` names a model, so a task picks it by name instead of
repeating agent flags. Each has a `kind`, the agent kind that runs it, and
`args`, the flags that select it. No model is built in.

```toml
# ~/.config/pastor/pastor.toml
[models.sonnet]
kind = "claude"
args = ["--model", "claude-sonnet-5"]

[defaults]
model = "sonnet"
```

```sh
pastor task run "Tidy the README" --repo '~/src/app' --model sonnet
```

A task takes its model from the first of: `--model` or the job's `model`,
its flock, the machine it runs on, `[defaults]`. Here the flock comes
before the machine, unlike for the agent, so each project's flock sets its
model on a shared machine. With none, the task runs the agent's own
default. The model's `args` go first on the agent's command line, then the
task's `agent_args`. `--model` takes only a name; an unknown one is
refused with `unknown_model`.

`fallback = ["sonnet", "gpt"]` names, in order, the models a task may fall
back to when its own runs out, each finding its agent on the machine as
above. A new task whose model is on an exhausted account starts on the
first free one, and a running task moves down it when its model hits a
usage limit (see [usage limits](#usage-limits)). The list comes from `--fallback` or the job's `fallback`, then the
machine, then the flock, then `[defaults]`; the first list wins whole, and
`[]` means none. `pastor task run --no-fallback` gives one task none.

## usage limits

A usage limit belongs to an account. `account = "me-personal"` under an
agent's `[agents]` table says which login it uses; every machine whose agent
names the same account shares its limits. An agent with no account keeps a
limit to the machine it was seen on, since the same name can be another
login elsewhere. pastor never reads the account as a credential.

The head keeps a row per exhausted account (or one model of it) until its
reset, and a new task does not start on it: it takes the first free model
of its `fallback` list, or stays queued, and `pastor queue` says why, like
`waiting: me-personal exhausted until 03:00 (5-hour limit, seen by t-412)`.
A Claude task whose agent stops on a limit (`You've hit your limit ·
resets 3am`) goes `waiting`, not `done`: pastor closes its pane, keeps its
worktree, and resumes its session on the same machine at the reset.
`pastor task list` shows it as `waiting 03:00`. With a `fallback` list, it
waits only for a reset within `wait_under` (30 minutes); otherwise it goes
on under the next free model of its list in the same worktree: in the same
Claude session when the model runs on the same login (Opus to Sonnet), else
from its prompt with the end of the last agent's pane (Claude to opencode).
It stays on that model; new tasks start on the first choice. Where Claude shows its
limit picker instead, pastor picks "Stop and wait for limit to reset" by
its text and the task waits the same way; it never picks extra usage or an
upgrade, and a picker without "Stop and wait" is left `blocked` for you.

`pastor limit list` shows the rows, and `pastor limit clear <account>`
forgets one, which wakes the tasks waiting on it on the next pass. The
events are `agent.exhausted`, `agent.reset`, `task.limited`,
`task.waiting` and `task.agent_switched`, and
[`[limits]`](../../reference/pastor-toml/#limits) sets how long a limit
with no reset holds.

## a model of another kind

A model runs only on an agent of its kind. To run an opencode model on a
machine whose agent is Claude, say which agent runs that kind there with
`agents`, on a `[[machine]]`, a `[[flock]]` or in `[defaults]`:

```toml
# ~/.config/pastor/pastor.toml
[models.gpt]
kind = "opencode"
args = ["--model", "openai/gpt-5.5"]
```

```toml
# ~/.config/pastor/flock.toml
[[machine]]
name = "desk"
local = true
agents = { opencode = "opencode" }  # desk runs opencode models with opencode
```

`pastor task run --model gpt` then lands on `desk` and starts opencode,
while its Claude tasks still run Claude. A machine with no agent of the
model's kind is skipped. A task pinned to one, or one whose own `--agent`
is of another kind, is refused with `model_kind_mismatch`.

## see what a task got

`pastor task describe t-12` prints the agent, its args, the model and the
profile, each with where it came from, such as
`model: sonnet (from flock app)`. `task list` has AGENT and MODEL columns.

Read on: [untrusted work in a sandbox](../../examples/sandbox/) gives a
sandbox machine its own Claude account; the keys are in
[pastor.toml](../../reference/pastor-toml/) and
[flock.toml](../../reference/flock-toml/), and the flags in the
[cli reference](../../reference/cli/#task).
