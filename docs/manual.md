# pastor manual

How pastor works today, in full. The [README](../README.md) is the short
version.

## How it works

`pastor serve` runs on one machine, the head. herdr answers one request per
connection and then closes it, so pastor opens a connection per request: an
`ssh` running `herdr --session <s> remote-api-bridge`, which pipes herdr's
socket protocol over stdio. Those connections are cheap because all of a
machine's share one multiplexed ssh master
(`ControlMaster=auto`, `ControlPath=~/.local/state/pastor/ssh/<machine>-%C`,
`ControlPersist=600`), so only the first one authenticates. A long machine name
is shortened inside that socket name so it stays under the unix socket path
limit; `%C` is what tells machines apart. A master lingers for
up to 10 minutes after `pastor serve` exits; end one by hand with `ssh -O exit -o
ControlPath=~/.local/state/pastor/ssh/<machine>-%C <target>`. Alongside them each
machine keeps one long-lived connection for `events.subscribe`, the one thing
herdr holds open. Over these pastor creates a workspace, starts an agent named
after the task, waits for the agent to come up (herdr's `agent.start` returns
before it has), sends the prompt, and watches agent status events. herdr answers
`agent.start` with `agent_pane_busy` while a new pane's shell is still
starting, so pastor retries the start up to 5 times, 500ms apart, before it
fails the task. An agent that
never becomes ready within 30s fails the task; one that exits on start fails it
straight away, usually because the agent is not installed on that machine. An
agent that is blocked on its own startup question marks the task `blocked`;
herdr drops the prompt then, so pastor sends it once someone answers and the
agent leaves `blocked`. A task is `done` when its agent has gone idle (herdr's
`idle` or `done`) after working on the prompt and stays idle for `settle`.
pastor must have seen the agent `working` or `blocked` since the prompt went in,
from an event or from `agent.list`; `unknown` does not count. herdr also counts
each agent state change, and the count must have moved past its value when the
prompt went in and not moved again during the window. What pastor has seen is
stored with the task, so a daemon restart or a flock or settings reload does
not forget work it saw before. An agent that went idle without pastor ever
seeing it `working` or `blocked` (its whole working spell fell between two
reconciles while the event was lost) stays `running` until its task goes
`stale`: herdr's counter moves on `idle -> unknown -> idle` as well, so
without the activity pastor cannot tell finished work from a flicker. An agent whose process exits while it sits idle
between turns (someone typed `/exit` after the work) leaves its task `done`;
one that exits while starting, blocked or working fails it with "agent process
exited". Task state lives in SQLite under
`~/.local/state/pastor/`.

Before creating a workspace, pastor checks that the repo directory is
actually there: `test -d` over the same ssh master for an `ssh` machine,
directly on the filesystem for a `local` one, and skipped for a `command`
bridge, which says nothing about where it lands. herdr otherwise opens the
workspace in the shell's home with no error when the requested `cwd` is
missing, so a missing repo fails the task instead of silently working in
the wrong place.

The CLI talks to `pastor serve` over a unix socket (`pastor.sock`) with
newline-delimited JSON; each response is `{"kind": ..., "data": ...}`.
`pastor serve` refuses to start if a daemon already holds that socket, or if
something answers it but not a ping within 2 seconds — it only removes and
replaces a socket file whose connection is refused. Every command that talks
to or reloads the head pings it once, first, and acts on that answer
throughout. A head that holds the socket but does not answer the ping within
2 seconds stops the command (`head_unresponsive`) before it does anything:
it may be busy mid-request, and working as if no head ran would let `tick`
start a second scheduler next to it, or an edit or a prune go offline behind
it. `task list` and `task show`
read from `pastor serve` when it's running and fall back to the SQLite store
when it's not (`task list` says so on stderr); `task attach` always reads the store
directly, since it only needs the task's machine and agent name to hand off
to `ssh`/`herdr`.

`pastor machine list` opens with a line about the head, then lists the
machines:

```
pastor 0.4.0 on desk (herdr 0.9.1), 2 machines, desk is the head of the flock

NAME  HOST       FLOCK     CHANNEL    HERDR  PASTOR  AGENTS  ORPHANS  TAGS  ERROR
desk  local      personal  connected  0.9.1  0.4.0   0/2     -        -
pi-3  user@pi-3  work      connected  0.9.1  0.4.0   1/2     -        fast
```

The line names the head's pastor version, its hostname, the version of the
`herdr` on its PATH (`-` when there is none) and how many machines follow.
When the head is itself a machine (a `local` one) the line ends by naming it,
and its row comes first; a head that runs no agents gets the line without
that ending. With no head running, the line is replaced by the notice on
stderr that the machines were probed directly. `--flock F` lists only that
flock's machines, and the line counts those.
HOST is the ssh target, `local`, or the program a `command` machine runs.
FLOCK is the flock the machine is in (see Flocks). PASTOR is the pastor
installed on the machine:
over the ssh master, pastor runs `pastor --version` in a shell that has
`~/.cargo/bin` and `~/.local/bin` on its PATH, since ssh's non-login shell
often lacks them. A `local` machine is the head's own pastor; a `command`
machine, one with no pastor, or one that gives an odd answer shows `-`. The
head asks once each time it connects to a machine, so an upgrade shows after
the next reconnect.
With `pastor serve` running, CHANNEL is the head's live channel state and
AGENTS counts pastor's tasks and orphans (see below) against `max_agents`. Without it, the command
probes each machine itself (a ping and an `agent.list`, one at a time, with no
time limit, plus the pastor version for a machine that answered); CHANNEL
reads one of four values: `probed` (the ping answered — an old protocol or a
failed `agent.list` still counts as `probed`, with the reason in ERROR),
`server down` (a local endpoint's own socket has nothing listening),
`unreachable` (any other transport failure), or `error` (the ping itself came
back with a non-transport API error). AGENTS counts every agent herdr reports,
and stderr says so. `--json` prints
`{"head": {...}, "machines": [...]}`, with `pastor_version` on the head and
on each machine (`null` when unknown), `flock` on each machine, and `channel`
one of the same four probe values (or the head's live channel state when
`pastor serve` is running).

Jobs are one TOML file each in `~/.config/pastor/jobs/`. On every `tick` the
daemon re-reads files that changed (a file that stops parsing keeps its last
good version and shows the error in `pastor job list`), asks each due job's
connector for items, drops keys it has seen before, renders `prompt`, `repo`
and `branch` with `{{ item.* }}`, `{{ job.name }}` and `{{ task.id }}`, and
queues one task per new item up to `max_tasks_per_run`. The rest stay unseen
for the next run. A job never overlaps itself; `pastor job run <name>` fires
one regardless, and it starts once a run already going has finished. `every = "5m"` or `cron = "*/5 9-18 * * 1-5"` (local time)
says when. The built-in connector is `clock`, one item per run keyed by the
run time; any other connector is a plugin (see Plugins), and a job that names
one that is not installed is `invalid`.
A failed connector backs the job off, one minute doubling to an hour, and
keeps its cursor.

A machine whose requests answer but whose event subscription will not open is
`polling`: it still takes tasks and is reconciled every `tick`. Two dispatch
passes never run at once, and a task moves from `queued` to `starting` with a
conditional update, so a machine is never given more than `max_agents`.

`pastor task list` shows live tasks only: queued, starting, running and blocked.
Finished ones (done, failed, stale, closed) appear with `--all`, and an empty
default list says so on stderr. `--blocked` and `--done` narrow to just that
state, `--job`, `--flock` and `--machine` narrow whichever set is shown, and
`--json` prints the same selection. FLOCK is the flock the task targets. `pastor task read t-1` fetches recent output from the
task's pane over the machine channel. `pastor open pi-3` execs the full herdr
UI against a flock machine (`herdr --remote` for an SSH one, `herdr` directly
for a local one) instead of showing pastor's own view; herdr refuses to start
inside one of its own panes, so run it from a plain terminal. pastor's flock and
herdr's saved machines are separate lists on purpose: `machine add --herdr` and
`machine remove --herdr` keep them in step by running `herdr machine add|remove`
for you, and without the flag `machine add` prints the command instead. `remove`
matches herdr's entry by label only; when none matches but the same host is
saved under another label, it prints that entry's remove command rather than
guessing.

A task that reaches `done` is closed by pastor after `close_done_after`
(`pastor.toml`, default `15m`; `never` disables it): the pane closes, a
worktree pastor created is removed if it is clean and kept with a note if it
is not, and the task shows as `closed`. Failed and stale tasks are left for
`pastor task retry`; blocked tasks need their prompt answered
(`pastor task send` or `pastor task attach`) or `pastor task close`. None of them is closed on its
own. The grace period keeps the pane there for `pastor task attach`; the
check runs with each reconcile, while the machine is connected. An agent
that herdr shows working or blocked again
at that moment is left alone, and its task goes back to running or blocked.

`pastor task send t-3 "yes, go on"` types into the pane of a live task
(starting, running or blocked) and presses Enter; `--no-enter` leaves Enter
out, and each `--key K` presses one named key after the text, in order
(`--key esc`, `--key Down --key Enter`; herdr's key names). It goes through
the head to the task's machine; anything else answers `task_not_live`. Each
send is a `task.input` event recording the key names and the length of the
text, never the text, which may be a secret.

An agent started in a folder it has not seen stops at its folder-trust
prompt, and a worktree is always a new folder. `pastor task send t-3
--trust` presses the agent's trust keys (`trust_keys` under `[agents.<name>]`
in `pastor.toml`; Claude's are built in as `Down`, `Enter`) and saves the
task's machine and repo, the `--repo` as given, so every worktree of that
repo counts. It answers only a task blocked on its startup prompt; any other
task answers `not_at_trust_prompt`, and nothing is sent or saved. An agent
without trust keys answers `no_trust_keys`, and a task
without `--repo` gets the keys but nothing is saved. From then on, when a task
of a saved repo is blocked during startup on that machine, the head presses
the trust keys itself, once per task, and emits `task.trusted`; a task still
blocked after that is left for a human. `pastor trust list [--json]` shows
the saved pairs and `pastor trust remove <machine> <repo>` forgets one; both
work with `pastor serve` down.

Everything else pastor closes only when asked; three commands do it, all
with `--json`. `pastor task retry t-4` queues a new task
copying a failed or stale one (job, item, prompt and dispatch settings, with
`retry_of` pointing back and a "retry of t-4" note in `task list`) and dispatches it
at once. It gets a new id because the old agent `t-4`, named after the task, may
still be running. The retry of a failed worktree task goes back to that
task's checkout, on its branch, with herdr's `worktree.open` instead of
`worktree.create` (which git would refuse with "already exists"), but only
when pastor knows the checkout is that task's own (its dispatch made it and
recorded where), it is still on disk at the same path, the old agent is
gone and no agent is in the checkout's workspace (an earlier retry of the
same task may have reopened it). Any other retry, a stale task's included (its agent may still be
working), gets a branch of its own (`pastor/t-<new id>`, even when the task
named one) and a new worktree. `pastor task close
t-4` closes the task's pane (and the agent in it) and marks it `closed`; a
queued task only has its row closed. `--remove-worktree` removes the task's
worktree instead, which closes its workspace, pane included. herdr refuses a
checkout with uncommitted or untracked files; the task is then left as it was,
so commit or clean up and run it again. `pastor task prune --done --older-than
3d` deletes done tasks that finished more than three days ago; `--failed` and
`--closed` add those states. A pruned task's item stays seen, so a job never
queues it again, and the newest task is always kept so its id is never handed
out twice. Prune also keeps a worktree task whose checkout may still be on
disk: a plain close only closes the pane, and the row is then the only record
of that checkout. It names each one it keeps; `task close --remove-worktree`
on it removes the worktree (or, when herdr has already lost the workspace,
says to run `git worktree remove`) and clears the recorded workspace, and the
next prune takes the row. Prune works without `pastor serve`, but not behind one that holds
the socket and does not answer (`head_unresponsive`): that head may still be
writing tasks. Retry and close need it.
`task run --worktree` needs `--repo`.

An agent named like a task (`t-N`) that no open task owns is an orphan: a
dispatch that failed after the agent started, a daemon killed mid-dispatch, a
failed task whose agent never exited, a pruned row. Orphans still hold a pane,
so they count toward `max_agents`. `pastor task list` prints a line for each under
its table, unless `--blocked`, `--done` or `--job` narrows it (an orphan has no
state or job; `--machine` still applies), `machine list` names them in an ORPHANS column,
and `pastor task close t-N` closes one, with or without a row. pastor finds
them when it reconciles (every `reconcile_every`). It assumes it is the only
pastor naming agents `t-N` on each herdr.

## Flocks

A flock is a named group of machines. Every machine is in exactly one, and
every task and job targets one: only that flock's machines take its tasks.
Flocks keep kinds of work apart, such as work and personal machines that run
agents on different accounts. A flock can also say which agent its tasks run.

```toml
# flock.toml
[[flock]]
name = "personal"
default = true            # tasks and jobs that name no flock go here

[[flock]]
name = "work"
agent = "claude"          # optional: the agent for this flock's tasks
agent_args = ["--model", "claude-sonnet-5"]

[[machine]]
name = "desk"
local = true              # no `flock`: the default flock

[[machine]]
name = "pi-3"
ssh = "user@pi-3"
flock = "work"
```

`[[flock]]` entries declare the flocks, so a flock can have no machines yet.
Names are unique and exactly one has `default = true`. A machine naming an
undeclared flock, two defaults, or none makes the file fail to load, like any
other bad flock file: `pastor serve` refuses to start on it, `pastor tick`
and `pastor job list` without a head refuse it too, and a running head keeps
the previous version. A file with no `[[flock]]` entry at all is
one flock named `default` holding every machine, so files from before flocks
load unchanged.

```
pastor flock list                       NAME, DEFAULT, MACHINES, AGENTS, QUEUED (--json)
pastor flock add <name> [--default]
pastor flock remove <name>              refused while it has machines or queued tasks, or is the default
pastor flock default <name>             new tasks and jobs go to <name>
pastor machine add ... [--flock F]      default: the default flock
pastor machine move <name> <flock>
pastor machine list [--flock F]
```

These commands edit `flock.toml` in place: comments, order and layout that
the edit does not touch stay as they were, and a running head picks the
change up at once, as it does for `machine add|remove`. The first `flock add`
on a file with no `[[flock]]` entry writes the implicit flock down as
`default` first. `flock default` writes the old default flock onto every
machine that named none, so changing where new work goes moves no machine.
AGENTS in `flock list` needs a running head and is `-` without one.

A task's flock is fixed when it is created: `--flock` on `pastor task run`, or
`flock` under a job's `[dispatch]`; else the flock of the machine it is pinned
to (`--machine`, a job's `machine`); else the default flock. `--machine` with
a `--flock` the machine is not in is refused (`flock_mismatch`), as is a flock
that does not exist (`unknown_flock`). A job whose flock does not fit, or
whose pinned machine is not in the flock, fails its run before the connector
is asked for anything, with the reason in `pastor job list`. A flock or a
pinned machine removed while the connector runs gets none of that run's tasks:
each item fails, and the cursor holds for the next run. `pastor task retry` keeps the flock of the task it copies, and is refused
(`unknown_flock`) once that flock has been removed. A head
started from a pastor before flocks would ignore `--flock` and read
`flock.toml` as one flock, so while flocks are in play every command that
talks to or reloads the head asks its protocol first and refuses an old one
(`head_too_old`): restart `pastor serve` after an upgrade. Flocks are in play
when the command takes `--flock`, edits `flock.toml` (`flock add|default|remove`,
`machine add|remove|move`), or `flock.toml` declares named flocks, since then
no `--flock` means the default flock rather than every machine. A head that
is listening but does not answer is refused whether flocks are in play or not
(`head_unresponsive`); only a head that is not running at all is passed by.

### A flock's agent

`agent` and `agent_args` under a `[[flock]]` entry are the agent its tasks and
jobs run when they name none. Each task settles its agent when it is queued,
from the first of these that says:

1. `--agent` and `--agent-arg` on `pastor task run`, or `agent` and
   `agent_args` under a job's `[dispatch]`;
2. the task's flock, in `flock.toml`;
3. `[defaults]` in `pastor.toml`;
4. the built-in: `claude` with no args.

The agent and its args are looked up on their own, with one rule: args follow
the agent they were written for. A layer's `agent_args` only apply when that
layer names no `agent`, or names the one the task runs. So with the `work`
flock above, `pastor task run --flock work --agent codex` runs codex without
`--model claude-sonnet-5`, and `[defaults] agent_args` (written for
`[defaults] agent`) do not reach a flock that runs another agent. An
`agent_args = []` is a choice, not a gap: it stops the lookup with no args.

`pastor task show` prints the agent and args a task resolved to; the task keeps
them, so a later edit of `flock.toml` or `pastor.toml` changes only tasks
queued after it. `pastor task run` makes the head apply an edit of
`pastor.toml` or `flock.toml` first; for a job, an edit reaches the head on
its next tick, or at once with `pastor job reload`.

### Tool allow and deny lists

pastor leaves the agent's own permission mode alone: Claude still asks before
it runs a tool its settings do not already allow, and a task that waits on
such a question goes `blocked` until someone answers it with `pastor task
send`. To answer some of those questions in advance, give pastor lists of tool
patterns, in the agent's own syntax:

```toml
# pastor.toml
[defaults]
allow = ["Read", "Edit", "Bash(git:*)", "Bash(cargo test:*)"]
deny = ["WebFetch", "Bash(rm:*)"]
```

```toml
# flock.toml
[[flock]]
name = "work"
allow = ["Bash(make:*)"]
deny = ["Bash(git push:*)"]
```

```toml
# a job file
[dispatch]
allow = ["Bash(gh pr view:*)"]
prompt = "..."
```

`allow` lists tools the agent may use without asking; `deny` lists tools it
must never use. The lists add up: a task gets `[defaults]`, then its flock's,
then its job's, without repeats. **Deny wins**: a pattern in any `deny` is
dropped from `allow`, and is passed to the agent as a deny as well, so a flock
or a job can widen what `[defaults]` allows but never lift a deny from a
broader layer. Patterns are compared as written; Claude itself also applies a
deny over an allow that overlaps it. An empty pattern, or one that starts with
`-`, fails the file's load: agent flags belong in `agent_args`.

When it starts the agent, pastor turns the lists into the agent's own flags,
after `agent_args`, one flag per pattern. For `claude` those are
`--allowedTools` and `--disallowedTools`, so a task in the `work` flock above
starts as `claude --allowedTools Read --allowedTools Edit ... --disallowedTools
WebFetch ... --disallowedTools 'Bash(git push:*)'`. Another
agent names its flags in pastor.toml:

```toml
[agents.my-agent]
allow_flag = "--allow-tool"
deny_flag = "--deny-tool"
```

A task whose agent has no flag for a list it carries is refused rather than
started without it (`agent_tools_unsupported` from `pastor task run` and
`pastor task retry`; a job records the error for that item). `pastor task show`
prints a task's `allow` and `deny`. Since a head from before these lists would
start the agent without them, every command that can make it queue a task
(`pastor task run`, `pastor task retry`, `pastor tick` without `--dry-run`,
`pastor job run`) refuses one (`head_too_old`): restart `pastor serve` after
an upgrade.

#### Arguments that turn permissions off

`agent_args` is passed through as written, so it can also carry
`--dangerously-skip-permissions`, `--permission-mode bypassPermissions` or an
agent's equivalent. pastor allows this, but **it is dangerous**: with it the
agent asks nothing and no allow list applies. Whatever reads the prompt, the
repo or a web page can then steer an agent that runs every command it is told
to, as the head's user on that machine, with that user's files, keys and
network. See [Trust model](#trust-model) before you set one, prefer a narrow
`allow` list, and keep such args in a flock of disposable machines rather than
in `[defaults]`.

### Agent definitions

`[agents.<name>]` in pastor.toml defines an agent by name. Tasks, jobs and
flocks name it like any other agent; `kind` says which herdr agent it starts,
and `env` sets environment variables for its pane. The usual case is a second
Claude account on the same machines:

```toml
# pastor.toml
[agents.claude-personal]
kind = "claude"
env = { CLAUDE_CONFIG_DIR = "~/.claude-personal" }
```

```toml
# flock.toml: every task of the personal flock runs it
[[flock]]
name = "personal"
agent = "claude-personal"
```

`pastor task run --agent claude-personal` or `agent = "claude-personal"` in
a job does the same for one task. herdr starts a `claude` (the `kind`; without
one, the name itself), and `task show` and `task list` keep the name
`claude-personal`. Built-in settings follow the kind, not the name: the
definition gets Claude's trust keys and its `--allowedTools` and
`--disallowedTools` flags unless it sets `trust_keys`, `allow_flag` or
`deny_flag` of its own. It does not inherit what `[agents.claude]` sets.

`env` values are passed as written, except that a value of `~` or one that
starts with `~/` is expanded against the home of the machine the task runs
on, as `--repo` is; a `command` machine cannot report a home, so give those
absolute paths. Keys must be variable names (letters, digits and `_`, not
starting with a digit). pastor sets the env when it creates the task's pane:
`workspace.create` takes it directly. herdr's `worktree.create` and
`worktree.open` take none, so for a `--worktree` task pastor splits a pane off
the worktree's, with the env and the checkout as its directory, closes the
pane without it, and starts the agent in the new one. A task without env keeps
herdr's own pane either way.

The head checks a task's agent against pastor.toml when it queues it, and
`pastor task run` makes the head apply an edit of pastor.toml or flock.toml
first, so a definition you have just added is the one its machine starts.

`pastor machine move` changes the flock of tasks dispatched after it; tasks
already on the machine keep running there, and its connection stays up. A
queued task pinned to a machine that has moved to another flock stays queued
for its own flock, and the head logs a warning once.

## Events

`pastor serve` appends every task, job and machine event to
`~/.local/state/pastor/events.jsonl`, one JSON record per line. When the next
line would take the file past 10 MiB it is moved to `events.jsonl.1`
(replacing the previous one) and a new file started, so the log keeps at most
two generations. `pastor events` prints both, oldest first; `--task t-3` keeps
one task's records, `--json` prints the records as stored, and `--follow`
keeps printing as new ones are written. It reads the file, not the daemon, so
it works with `pastor serve` down.

A record, which is also what plugin event hooks get on stdin:

```json
{
  "at": "2026-09-24T10:15:02.123Z",
  "type": "task.done",
  "task": {"id": 3, "job": "triage", "item": {"key": "...", "title": "..."},
           "prompt": "...", "spec": {"agent": "claude", "...": "..."},
           "machine": "pi-3", "state": "done", "error": null, "...": "..."},
  "job": "triage",
  "machine": null
}
```

- `at`: when the daemon received the event, RFC 3339 UTC.
- `type`: `task.queued|running|blocked|done|stale|failed|closed`,
  `task.input` (`pastor task send`), `task.trusted` (the head answered a
  trust prompt), `job.failed`, `machine.connected`, `machine.lost`.
- `task`: the full task row (the same object as `pastor task show --json`) at
  that moment, on `task.*` events; `null` otherwise or if the row is gone.
  `task.flock` is the task's flock, so a hook can route work and personal
  notifications apart.
- `job`: the job name. For a task event it is the task's `job` (`run` for a
  one-off `pastor task run` task); for `job.failed`, the job that failed.
- `machine`: on `machine.*` events, the machine's status as an entry of
  `machines` in `pastor machine list --json` shows it (`name`, `host`,
  `endpoint`, `channel`, `herdr_version`, `pastor_version`, `protocol`,
  `error`, `live`, `max_agents`, `tags`, `flock`); `null` on other events.
  A task's machine is `task.machine`.
- `detail`: only on events that carry more, and absent otherwise. On
  `task.input`, `keys` (the key names pressed, Enter included), `text_len`
  (the length of any text) and `trust` (sent by `--trust`); on
  `task.trusted`, `keys`.

Fields may be added; none will be renamed or removed. Unreadable lines (a
torn write, a hand edit) are skipped.

Runtime errors print JSON on stderr with a stable `code` and exit 1; a
malformed command line gets clap's plain usage text and exit 2.

Each machine needs herdr 0.9 or newer (protocol 22 or newer) with its server
running, and SSH access from the head without a passphrase prompt (a key in
ssh-agent won't be there for a service; use a dedicated key or Tailscale SSH).

## Try it

```bash
make install                         # pastor into ~/.cargo/bin
pastor machine add pi-3 user@pi-3 --max-agents 2 --herdr   # --herdr also saves it in herdr's sidebar
pastor machine add here --local
pastor flock add work                # a second flock; the machines above stay in `default`
pastor machine move pi-3 work
pastor machine list                  # a line about the head, then each machine: host, flock, channel, herdr, pastor, agents
pastor setup systemd                 # confirm, then install and enable --now; or `pastor serve &`
pastor task run "Fix the flaky test in ci.yml" --repo '~/work/api' --machine pi-3
pastor task run "Review the open PR" --agent-arg=--model --agent-arg=claude-opus-5-5
pastor task run "Triage the inbox" --flock work   # only work machines take it
pastor task run --prompt-file ./prompt.md --repo '~/work/api'   # a long prompt, no shell quoting
mkdir -p ~/.config/pastor/jobs
cat > ~/.config/pastor/jobs/hourly.toml <<'EOF'
every = "1h"
[connector]
use = "clock"
[dispatch]
repo = "~/work/api"
prompt = "It is {{ item.key }}. Run the test suite and fix what broke. Task {{ task.id }}."
EOF
pastor job list                      # picked up at the next tick
pastor tick --dry-run --job hourly   # what a run would create, without creating it
pastor job run hourly                # fire it now
pastor task list --job hourly        # live tasks only
pastor job disable hourly
pastor task list --all               # finished tasks too
pastor task read t-1                 # recent pane output, without attaching
pastor task retry t-4                # a failed or stale task again, as a new task
pastor task close t-1 --remove-worktree   # close its pane and remove its worktree
pastor task prune --done --closed --older-than 7d
pastor task attach t-1               # lands in the agent's pane; ctrl+b q detaches
pastor open pi-3                     # the full herdr UI on that machine
pastor events --follow               # task, job and machine events as they happen
```

`--repo` and a job's `repo` are paths on the machine that runs the agent. A
leading `~` means that machine's home: pastor asks an ssh machine for `$HOME`
and uses its own for a local one, because herdr takes the path literally and
opens the pane somewhere else when it does not exist. Quote it, or your shell
expands it to the head's home first. A `command` machine cannot report a home,
and neither can one whose shell has no absolute `$HOME`; give those absolute
paths.

`pastor task run` takes the prompt as its argument or, instead, `--prompt-file
PATH`, and `--repo`, `--flock`, `--machine`, `--agent`, `--agent-arg`,
`--worktree`, `--branch` (with `--worktree`), `--tag` (repeatable),
`--timeout` and `--json`. `--agent-arg` hands one argument to the agent,
through herdr's `agent.start`; repeat it for more, in order. It always takes the
next word as its value, even one that starts with a dash, so
`--agent-arg --model --agent-arg claude-opus-5-5` and
`--agent-arg=--model --agent-arg=claude-opus-5-5` mean the same thing; the
`=` form just reads more clearly. There is no single-string form: pastor would
have to split it on spaces, and that breaks any argument that contains one. A
job file's `agent_args` does the same for its tasks. When neither says
anything, the flock's `agent_args` apply, then `[defaults] agent_args` in
pastor.toml (see [A flock's agent](#a-flocks-agent)); a job file that sets
`agent_args = []` opts out of both. `pastor task show t-1` prints the agent and
args a task was started with.

`--prompt-file` reads the prompt from a file on the machine that runs the CLI,
not on the agent's machine; `-` reads standard input. It spares a long prompt
the shell's quoting, so `pastor task run --prompt-file - <<'EOF'` takes quotes,
backticks and dollar signs as they are. Give exactly one of the argument and
the flag; both, or neither, is a usage error (exit 2). Newlines at the end of
the file are dropped, as a prompt typed on the command line has none; the rest
is sent unchanged. A file that cannot be read (missing, a directory, not UTF-8)
fails with `prompt_file_unreadable`, and one with nothing but whitespace with
`prompt_file_empty`; in both cases nothing is dispatched.

Without a real herdr, a fake one speaks the same protocol. It comes in the same
two pieces the real thing does, because state has to outlive a single request:
a server, and a bridge per request.

```bash
FAKE_HERDR_AUTO_DONE_MS=500 fake-herdr --listen /tmp/fake-herdr.sock &
pastor machine add fake --command "fake-herdr --connect /tmp/fake-herdr.sock"
pastor serve
```

## Run under systemd

`pastor setup systemd` installs `contrib/systemd/pastor.service` to
`~/.config/systemd/user/`, shows what it will do, and only continues after you
type `yes`. `--yes` (`-y`) skips the prompt; it is required when stdin is not a
terminal (a script, a task, `ssh host pastor setup systemd --yes`), where setup
fails at once rather than wait for an answer. With no action flag it runs `systemctl --user enable --now` on the
unit. `--enable`, `--start`, `--enable --now`, `--enable --start` and `--stop`
map to the same `systemctl --user` actions after the unit is written.
`pastor setup systemd --herdr` does the same with `herdr.service` (the herdr
server) and belongs on every machine in the flock. Both units always restart
(`Restart=always`, so a head killed by a stray signal comes back) and log to
the journal (`journalctl --user -u pastor`); `systemctl --user stop` still
stops one for good, since systemd does not restart after an explicit stop.
Setup points `ExecStart`
at the binary it finds (the running pastor, or `herdr` on PATH) and copies your
shell's `PATH` into the unit, so `ssh`, `herdr` and the agents resolve under
systemd the way they do in a terminal; re-run it after moving a binary. A unit
that differs from what setup would write is kept as `<unit>.service.bak`, and a
running service is not restarted, since restarting herdr stops its agents: run
`systemctl --user restart pastor` (or `herdr`) yourself.

A user service stops at logout unless lingering is on. Setup checks
`loginctl show-user` and prints `loginctl enable-linger` when it is off.
For `pastor.service` it also sets the config and state dirs to 0700 and the
socket and plugin `.env` files to 0600, and says what it changed.

## Plugins

A plugin is a directory with a `pastor-plugin.toml` and the commands it
names. It can provide a connector (where a job's items come from), event
hooks, or both. `tests/fixtures/plugin/` has small working examples (`echo`
and `stream` connectors, a `notify` hook).

```
~/.local/share/pastor/plugins/<id>/     the plugin: a checkout, or a symlink for `plugin link`
~/.config/pastor/plugins/<id>/.env      secrets and settings, written by you
~/.local/state/pastor/plugins/<job>/    the job's scratch directory, owned by pastor
~/.local/state/pastor/runs/<job>/       run logs, one <ts>.log per run
```

`PASTOR_DATA_DIR` overrides the first. The directory name is the plugin's id
and must equal `id` in the manifest.

### The manifest

```toml
id = "slack"                         # required: [a-z0-9][a-z0-9_.-]{0,63}
name = "Slack"                       # optional, defaults to the id
version = "0.1.0"                    # required: major.minor.patch, numbers only
min_pastor_version = "0.1.0"         # optional: refuse to load on an older pastor
description = "Watch a channel, report back in thread"

[connector]
mode = "poll"                        # poll (the default) or stream
command = ["bash", "poll.sh"]        # argv, run in the plugin's directory
timeout = "60s"                      # the default; bounds one poll run

[connector.config.channel]           # what a job's [connector] table may carry
required = true
description = "Channel ID to watch"

[secrets.SLACK_BOT_TOKEN]            # names only; the values live in .env
description = "Bot token with channels:history and chat:write"

[[events]]
on = ["task.done", "task.blocked", "task.failed"]
only_own = true                      # false by default
command = ["bash", "report.sh"]
timeout = "60s"                      # the default
```

The manifest is checked when pastor discovers the plugin, and a key it does
not know is an error. A plugin needs a `[connector]`, at least one `[[events]]`
hook, or both. Each `command` is an argv array with a program in its first
place; a relative program with a slash (`./poll`) means the plugin's own file.
An `on` entry is an event type like `task.done`, and timeouts may not be
zero. The id `clock` belongs to the built-in connector. A plugin that fails
any of this, or asks for a newer pastor than the one running, is listed by
`plugin list` as invalid with the reason, and a job that uses it is invalid
too.

`config` and `secrets` are declarations, with no schema language beyond
`required` and `description`: pastor checks that a job's `[connector]` table
has every `required` key and passes the rest through untouched, and
`plugin list` reports the secrets the `.env` leaves unset or empty. Secret
names must look like environment variables (`[A-Z_][A-Z0-9_]*`).

### Connector protocol

To run a connector, pastor starts its `command` in the plugin's directory
with the plugin's `.env` in its environment plus `PASTOR_PLUGIN_ID`,
`PASTOR_JOB`, `PASTOR_CONFIG_DIR`, `PASTOR_STATE_DIR` and
`PASTOR_PLUGIN_STATE_DIR` (the job's scratch directory, created 0700). It
writes one JSON line to stdin, then closes it:

```json
{"config": {"channel": "C0123ABC"}, "cursor": "1727000000.000100", "since": "2026-09-23T09:00:00Z"}
```

`config` is the job's `[connector]` table without `use`. `cursor` is what the
connector last reported, or `null` before it has reported one. `since` is the
start of the job's last successful run, or on the first run now minus the
job's `[dispatch] backfill`; it is an RFC 3339 UTC time.

The connector answers in JSON lines on stdout, one object per line with a
`type`:

```json
{"type":"item","key":"1727000123.000200","title":"login broken on safari","body":"...","url":"https://...","author":"ana"}
{"type":"log","level":"info","message":"fetched 12 messages"}
{"type":"cursor","value":"1727000123.000200"}
```

- `item` needs a `key`, a non-empty string that stays the same for the same
  source object: pastor drops keys it has seen before, so it is what stops one
  message becoming two tasks. Every other field is kept and reachable in a
  job's templates as `{{ item.<field> }}` (`title`, `body` and `url` are
  conventions, not requirements; a nested object is reached with dots).
- `cursor` carries a string `value` for the connector to be handed back next
  time. The last one in a run wins.
- `log` carries a `message` and an optional `level` (`info` by default). Pastor
  logs it, and `plugin run` prints it.
- A blank line is ignored. Any other line (not JSON, not an object, an item
  without a key, an unknown `type`) is skipped, noted in the run log, and the
  rest of the output is used.

A poll connector exits when it is done. Exit 0 succeeds: its items become
tasks and its cursor is saved, once every new item has been dealt with: a run
that defers items (`max_tasks_per_run`) or fails to insert one keeps the old
cursor and `since`, and the items that landed are not made twice. An item
pastor rejects (an item field used in the job's `repo` or `branch` template
whose value is unsafe there) is reported in the job's last error and skipped;
it does not hold the cursor back, since a retry cannot fix it. A non-zero exit, a timeout (the whole
process group is killed) or a program that will not start fails the run: its
items and cursor are discarded, `job.failed` is emitted, the job backs off,
and the error names the run log. What the connector writes to stderr goes to
that log.

A stream connector gets the same handshake once, when it is started, and
answers in the same lines at any time; it owns its own sockets and pastor
proxies nothing. If it exits it is started again with backoff (1s doubling to
5 minutes; a run that stayed up a minute starts it over). Pastor hands the
restarted process, in its handshake, the newest cursor the connector emitted
before it exited, whether or not a job run has saved it yet, or the job's
saved cursor if it has emitted none since the daemon started. How a job run takes a stream's output is
under "Using a plugin in a job".

### Secrets and the `.env` file

Secrets and settings go in `~/.config/pastor/plugins/<id>/.env`, which every
command of the plugin (connector and hooks) gets in its environment. The
format is the common dotenv subset: `KEY=value` lines, an optional `export `,
`#` comments, single quotes for a literal value and double quotes for one with
`\n`, `\"` and `\\` escapes, no interpolation; the last of a repeated key
wins. A line that does not parse makes every command of that plugin fail
naming the line, and `plugin list` shows the error. `pastor setup systemd`
sets these files to 0600.

Only what the manifest declares under `[secrets]` is treated as secret. What
a run writes to stderr lands in `~/.local/state/pastor/runs/<job>/<ts>.log`
(a hook's stdout goes to its log too); in those logs, in the connector's `log`
records and in the reason a failed run reports, the value of every declared
secret is replaced by `[redacted:NAME]` (a value shorter than four characters
is left alone, since hiding it would mangle the log and protect nothing). Redaction works line by line, so a
declared secret may not contain a line break (a double-quoted `\n`): pastor
refuses such a `.env` and names the variable. Each log is cut at 256 KiB, and
each run directory keeps its newest 20: `runs/<job>/` for a job's connector
runs, and `runs/@<id>/` for all of a plugin's hook runs together. A stdout or stderr line longer than 256 KiB is
cut there and the rest of it dropped.

### Plugin commands

```bash
pastor plugin install owner/repo/plugins/slack        # owner/repo[/subdir], --ref, --yes
pastor plugin link ~/src/my-plugin                    # use a working copy in place
pastor plugin list [--json]                           # version, connector, hooks, missing secrets
pastor plugin run slack --job support --since 1h      # run the connector once, print its items
pastor plugin uninstall slack                         # or unlink, for a linked one
```

`install` clones the repository from GitHub with `git` (`PASTOR_PLUGIN_GIT_BASE`
points it at a mirror), checks out `--ref` if given, validates the manifest and
shows what the plugin will run (its connector and hook commands and its
secrets) before asking to continue. `--yes` skips the question, and it is
required when stdin is not a terminal. `link` puts a symlink to a directory of
yours in the plugins directory, for developing one. Both print the secrets
still unset in the `.env`. `uninstall` removes a checkout and `unlink` a
link (the directory itself stays); the `.env` and the state directory are
kept, and jobs that use the plugin are invalid until it is back.

`list` shows one row per plugin: id, version, connector mode, number of hooks,
whether it is installed or linked, and `ok`, the secrets still missing, or why
it is invalid.

`run` runs the connector once for a job and dispatches nothing. It uses the
`[connector]` table of `~/.config/pastor/jobs/<job>.toml` if that file exists,
and an empty config only when there is no such file, so you can try a plugin
before writing the job. A job file that exists is never ignored: one that
names a different connector, or that is invalid, is an error. The cursor is `null`, and `since` is `--since` before now, by
default the job's `backfill`, or zero. Items are printed to stdout as JSON
lines; logs and the summary go to stderr, and the run log is written as for any
run. Nothing is saved: no cursor, no tasks. A stream connector is collected for
its `timeout` and then stopped.

`install`, `link`, `uninstall` and `unlink` tell a running daemon to reload,
so a plugin is usable without a restart (this also restarts stream
connectors). A daemon that is running but does not answer gets a warning to
run `pastor job reload` yourself.

### Using a plugin in a job

A job uses a plugin's connector by its id (`[connector] use = "slack"`, with
the plugin's own keys beside it);
`pastor serve` and `pastor tick` check the job's connector table against the
keys the manifest marks `required`, and `job list` shows a job whose plugin is
missing or unhappy as invalid, with the reason. A poll connector runs once per
job run and must finish within its `timeout` (60s by default); a stream
connector is started once, restarted with backoff when it exits (and stopped
when its job file is removed, disabled or moved to another connector), and each job
run takes what it emitted since the last; the stream holds a batch until a
run has persisted it, so a dry run or a failed insert hands it out again.
The run that starts a stream waits, up to its `timeout`, to see the process
come up, so a missing program or a broken `.env` fails that first run.
A stream lives in `pastor serve`: `pastor tick` with no daemon running is a
one-pass process that would kill the stream on exit, so it reports a stream
job as failed, saying it needs `pastor serve`, and leaves it alone. Item fields that a job puts into
`repo` or `branch` may not contain `/`, `\`, `..`, a leading `-` or control
characters; such an item is skipped and reported.

### Event hooks

A plugin's `[[events]]` hooks run on the head for every event whose type is
in `on` (`task.queued`, `task.done`, `task.blocked`, `task.failed`,
`job.failed`, `machine.lost`, ... as in `pastor events`). The hook gets the
event record, the same JSON `pastor events --json` prints, on stdin, and the
same environment as the connector (`PASTOR_JOB` is the task's job, and unset
for an event about no job, such as `machine.lost`). With `only_own = true` it
only hears about tasks and jobs whose connector is this plugin, found by
reading `connector.use` in the job's file; one-off `pastor task run` tasks
belong to no plugin, and an event about no job passes.

```toml
[[events]]
on = ["task.done", "task.blocked"]
only_own = true
command = ["sh", "report.sh"]
timeout = "60s"                     # the default
```

Hooks of different plugins run at the same time; one plugin's hooks run one
after another, in event order, from a queue that holds 256 events; when a
plugin's hooks fall that far behind, the oldest waiting events are dropped
and logged. A hook that fails or times out is logged and not retried. Its output goes to `~/.local/state/pastor/runs/@<id>/`, redacted
like connector logs.

## Trust model

pastor gives an agent what the head's user has on each machine. It reaches a
machine over ssh as the user in `ssh = "user@host"` (or runs locally as the
head's own user), and herdr starts the agent in a pane of that user's
session. The agent can do what that user can do there: read and write their
files, use their ssh keys, git credentials and API tokens, reach what their
network reaches, including other machines they can log in to.

The prompt is the agent's input, and it is not always yours. A job's prompt
is filled from connector items (an issue, a message, a page); a repo holds
READMEs, comments and test output; the agent may fetch web pages. Any of
these can carry instructions written to steer the agent: prompt injection.
What stands between such text and the machine is the agent's own permission
checks: its settings, the `allow` and `deny` lists pastor passes, and the
question it asks before anything else, which parks the task as `blocked`
until a human answers.

So:

- Keep the agent's permission prompts on. An `allow` list should name the
  tools a task needs, not everything; put what must never happen in `deny`.
- `--dangerously-skip-permissions` and its kin in `agent_args` remove the
  checks entirely (see [Arguments that turn permissions
  off](#arguments-that-turn-permissions-off)). A prompt-injected agent then
  acts as the head's user on that machine with nothing to stop it. Use them
  only on machines you could wipe, with no credentials worth stealing, and
  never for jobs fed by input you do not control.
- Answer a blocked task only after reading its pane (`pastor task attach`),
  and treat `pastor task send --trust` the same way: it trusts the repo for
  every later task on that machine.
- Give each flock its own machines and accounts when work and personal data
  must not meet; a flock is a routing rule, not a sandbox.

## Files

```
~/.config/pastor/pastor.toml      tick, settle, reconcile_every, request_timeout, agent_ready_timeout, close_done_after, defaults, agents (all optional)
~/.config/pastor/flock.toml       flocks and machines
~/.config/pastor/jobs/<name>.toml one job per file
~/.local/state/pastor/pastor.db   tasks (schema 6, with retry_of, flock, trust_sent and activity_seen), seen keys, job state, trusted repos
~/.local/state/pastor/pastor.sock daemon socket
~/.local/state/pastor/events.jsonl events log (and events.jsonl.1, the previous one)
~/.local/state/pastor/ssh/        one ssh ControlMaster socket per machine and host
~/.config/systemd/user/{pastor,herdr}.service   written by `pastor setup systemd`
~/.config/pastor/plugins/<id>/.env   a plugin's secrets and settings
~/.local/share/pastor/plugins/<id>/  installed plugins (a symlink for a linked one)
~/.local/state/pastor/plugins/<job>/ a job's connector scratch
~/.local/state/pastor/runs/<job>/    captured connector output, capped and pruned
~/.local/state/pastor/runs/@<id>/    captured hook output (and job-less `plugin` runs)
```

`PASTOR_CONFIG_DIR`, `PASTOR_STATE_DIR` and `PASTOR_DATA_DIR` override the
locations. `PASTOR_PLUGIN_GIT_BASE` (default `https://github.com`) is where
`plugin install` clones `owner/repo` from.

```toml
# pastor.toml, every key optional; these are the defaults
tick = "10s"                 # scheduler pass
settle = "10s"               # a finished agent stays idle this long before its task is done
reconcile_every = "60s"
request_timeout = "60s"      # one herdr request, connect included
agent_ready_timeout = "30s"  # agent.start to an accepted prompt; below request_timeout
close_done_after = "15m"     # a done task's pane closes after this; "never" keeps it
[defaults]                   # for run flags, job keys and flock keys that are left out
agent = "claude"
agent_args = []              # e.g. ["--model", "claude-opus-5-5"]
allow = []                   # tool patterns the agent may use unasked, e.g. ["Bash(git:*)"]
deny = []                    # tool patterns it must never use; wins over allow
max_tasks_per_run = 5
timeout = "2h"
[agents.claude]              # one table per agent that needs one
kind = "claude"                  # the herdr agent it starts; default: the table's name
env = {}                         # env for its pane, e.g. { CLAUDE_CONFIG_DIR = "~/.claude-personal" }
trust_keys = ["Down", "Enter"]   # accept its folder-trust prompt; [] for none
allow_flag = "--allowedTools"    # the flag before each allow pattern
deny_flag = "--disallowedTools"  # the flag before each deny pattern
```

## Shell completions

`pastor completions <shell>` prints a completion script generated from the
command definitions, so it always matches the installed binary. Ready-made
copies for bash and fish live in `contrib/completions/`.

`make install` writes both files after installing the binary, honouring
`$XDG_CONFIG_HOME` and `$XDG_DATA_HOME`. If you installed with plain `cargo
install`, or want completions for another shell such as zsh, run:

```bash
pastor completions fish > ~/.config/fish/completions/pastor.fish
pastor completions bash > ~/.local/share/bash-completion/completions/pastor
```

## Skill for agents

`skills/pastor/SKILL.md` is a guide for coding agents: what pastor is, how to
run and watch tasks, what each state means, and what an agent that pastor
dispatched is expected to do. The same file is built into the binary, so
`pastor --skill` prints the copy that matches the installed version, and
`pastor --help` ends with a pointer to it.

It follows the Agent Skills layout, so a symlink makes it available to an
agent, either for your user or for one project:

```bash
mkdir -p ~/.claude/skills
ln -s ~/ghq/github.com/cacarico/pastor/skills/pastor ~/.claude/skills/pastor   # for you
mkdir -p .claude/skills
ln -s ~/ghq/github.com/cacarico/pastor/skills/pastor .claude/skills/pastor     # for one project
```

An agent that pastor dispatched runs on a flock machine, where this checkout
may not exist; if pastor is installed there, it can run `pastor --skill`.
A unit test checks that every command and flag the skill names exists, so a
CLI change that breaks it fails `make check`.

## Development

The Makefile is the list of things you can run here; `make help` prints it.

```bash
make check            # fmt check, clippy with warnings as errors, full test suite
make test             # unit tests plus an end-to-end run against fake-herdr
make test-machine     # the machine actor tests five times, to catch timing flakes
make smoke SESSION=s  # opt-in test against a real herdr running session s on this host
make build            # debug build of both binaries; cargo run -- --help works from there
```

`make check` is what a pull request has to pass. Nothing in the suite talks to
a real herdr, so run `make smoke` on a fleet machine before trusting it there.
