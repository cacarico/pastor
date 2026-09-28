# pastor manual

How pastor works today, in full. The [README](../README.md) is the short
version.

## How it works

`pastor serve` runs on one machine, the head. herdr answers one request per
connection and then closes it, so pastor opens a connection per request: an
`ssh` running `herdr --session <s> remote-api-bridge`, which pipes herdr's
socket protocol over stdio. Every command pastor runs over ssh (this one, the
probes below, `task attach`) goes as `sh -c '<command>'`, so a remote login
shell that is not POSIX, such as fish, only parses one quoted word; sh parses
the rest. For the same reason a machine's `session` may not contain a
backslash, and a repo path with one needs a POSIX login shell. Those connections are cheap because all of a
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
without the activity pastor cannot tell finished work from a flicker. herdr
shows an agent that stopped to ask something as idle too, so before it calls a
task `done` pastor reads the last 100 lines of the pane: when the agent's last
message (Claude's `●` block) ends in `?`, the task goes `blocked` instead, with
`agent asked: <question>` as its error, and stays there until the agent moves
again; answer it with `pastor task send`. Agents that draw their messages
without that marker are never read as asking. An agent whose process exits while it sits idle
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
it. `task list` and `task describe`
read from `pastor serve` when it's running and fall back to the SQLite store
when it's not (`task list` says so on stderr); `task attach` always reads the store
directly, since it only needs the task's machine and agent name to hand off
to `ssh`/`herdr`.

`pastor machine list` opens with a line about the head, then lists the
machines:

```text
pastor 0.4.0 on desk (herdr 0.9.1), 2 machines, desk is the head of the flock

NAME  HOST       FLOCK     PROFILE  CHANNEL    HERDR  PASTOR  AGENTS  ORPHANS  TAGS  ERROR
desk  local      personal  -        connected  0.9.1  0.4.0   0/2     -        -
pi-3  user@pi-3  work      develop  connected  0.9.1  0.4.0   1/2     -        fast
```

The line names the head's pastor version, its hostname, the version of the
`herdr` on its PATH (`-` when there is none) and how many machines follow.
When the head is itself a machine (a `local` one) the line ends by naming it,
and its row comes first; a head that runs no agents gets the line without
that ending. With no head running, the line is replaced by the notice on
stderr that the machines were probed directly. `--flock F` lists only that
flock's machines, and the line counts those.
HOST is the ssh target, `local`, or the program a `command` machine runs.
FLOCK is the flock the machine is in (see Flocks). PROFILE is the
permission profile a task there runs under when it names none: the
machine's own, else its flock's, else `[defaults]` (see [Permission
profiles](#permission-profiles)); `profile` in `--json`. PASTOR is the pastor
installed on the machine:
over the ssh master, pastor runs `pastor --version` in a shell that has
`~/.cargo/bin` and `~/.local/bin` on its PATH, since ssh's non-login shell
often lacks them. A `local` machine is the head's own pastor; a `command`
machine, one with no pastor, or one that gives an odd answer shows `-`. The
head asks each time it connects to a machine, and again after a reconcile
once ten minutes have passed since it last asked: on the reconcile tick
(`reconcile_every`, 60s by default) while connected, on the `tick` while
polling. With the defaults an upgrade shows within about eleven minutes
without restarting the head; a longer `reconcile_every` delays it to the first
reconcile past the ten minutes. A probe that gets no answer keeps the version
last read. `machine list` itself never probes
while the head is running.
With `pastor serve` running, CHANNEL is the head's live channel state and
AGENTS counts pastor's tasks and orphans (see below) against the machine's
room, `max_agents` followed by `+<n>j` for job slots and `+<n>b` for burst
when they are set (see Room on a machine). Without it, the command
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
for the next run. Item text comes from outside, so an item's control
characters, other than newline and tab, are dropped before its values go
into the prompt, which is typed into the agent's terminal. An item value put
into `repo` or `branch` must be one plain path component: not empty (a missing
field renders empty), not `.` or `..`, no `/` or `\`, no leading `-` and no
control characters. An item that breaks this is skipped and shown in the job's
last result; the job's cursor still moves. An item value may not appear in the first
component of `branch`: the job fixes a prefix such as `pastor/{{ item.key }}`,
so an item cannot name an existing branch like `main` and have the agent
commit to it. A job file that breaks this is `invalid`. A job never overlaps itself; `pastor job run <name>` fires
one regardless, and it starts once a run already going has finished. `every = "5m"` or `cron = "*/5 9-18 * * 1-5"` (local time)
says when. The built-in connector is `clock`, one item per run keyed by the
run time; any other is an installed connector (see Connectors), and a job
that names one that is not installed is `invalid`.
A failed connector backs the job off, one minute doubling to an hour, and
keeps its cursor.

A job can also run on another machine and hand its items to the head, which
keeps the `seen` keys so no item is queued twice. The IPC request
`job_submit` carries the job's name, its `[dispatch]` table, its prompt and
the items (each with its `key`); the head checks the table as it would a job
file's (`invalid_dispatch` with the same error), and queues each item with the
same rendering, path checks and `max_tasks_per_run` cap as its own jobs. It
answers the tasks queued, the keys skipped as seen, and each refused item with
its reason (`max_tasks_per_run` for those past the cap; they stay unseen). A
name the head has a job file for is `job_name_taken`; submitters of one name
share its seen keys. It needs a head of IPC protocol 7 (`head_too_old`
otherwise). A headless serve (below) sends it for each run of its jobs.

A machine whose requests answer but whose event subscription will not open is
`polling`: it still takes tasks and is reconciled every `tick`. Two dispatch
passes never run at once, and a task moves from `queued` to `starting` with a
conditional update, so a machine is never given more than its room (see Room
on a machine).

`pastor task list` shows live tasks only: queued, starting, running and blocked.
Finished ones (done, failed, stale, closed) appear with `--all`, and an empty
default list says so on stderr. `--blocked` and `--done` narrow to just that
state, `--job`, `--flock` and `--machine` narrow whichever set is shown, and
`--json` prints the same selection. FLOCK is the flock the task targets. `pastor task read t-1` fetches recent output from the
task's pane over the machine channel. Text that came from an item or a pane is
printed with its control characters escaped (`\x1b`, `\r`, ...), so none of
it can move the cursor, retitle the terminal or set the clipboard: the NOTE
column of `task list` (an item's title, cut to 60 characters), every field and
the prompt of `task describe`, and `task read`. `--json` prints it raw. `pastor machine open pi-3` execs the full herdr
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
worktree pastor created is removed if it is clean, and the task shows as
`closed`. Clean means no uncommitted changes and no commits that are on no
remote (`git rev-list HEAD --not --remotes` is empty; a repo with no remote
at all always has some). A worktree that is not clean is kept, with a note
on the task naming the branch, for you to save and remove with `git worktree
remove`. herdr never deletes the branch, even for a worktree it removes. Failed and stale tasks are left for
`pastor task retry`; blocked tasks need their prompt answered
(`pastor task send` or `pastor task attach`) or `pastor task close`. None of them is closed on its
own. The grace period keeps the pane there for `pastor task attach` (a
Claude task can be reopened after it anyway; see [Reopening a finished
task](#reopening-a-finished-task)); the
check runs with each reconcile, while the machine is connected. An agent
that herdr shows working or blocked again
at that moment is left alone, and its task goes back to running or blocked.

An agent says it is finished with `pastor task done` from its own pane (the
task defaults to `PASTOR_TASK`; a human may name any task, `pastor task done
t-3`). The task goes `done` at once, however herdr reads the agent, and stays
done while the agent finishes the turn it said so in: a trailing question or
a turn still at work no longer keeps it `blocked` or `running`, and the
completion sequence check does not hold its close. Auto-close then closes its
pane once the agent is idle and `close_done_after` has passed, which frees the
machine's slot. `pastor task send` to such a task gives it more to do, and it
runs again as any done task does. `task describe --json` prints `"ended": true`.

`pastor task send t-3 "yes, go on"` types into the pane of a live task
(starting, running, blocked, or done with its pane still open) and presses
Enter; `--no-enter` leaves Enter out, and each `--key K` presses one named
key after the text, in order (`--key esc`, `--key Down --key Enter`; herdr's
key names). It goes through the head to the task's machine; anything else
answers `task_not_live`. Each send is a `task.input` event recording the key
names and the length of the text, never the text, which may be a secret.

A done task that is sent input goes back to running (`task.running`), so an
agent marked done with its work unfinished can be told to finish in the same
pane, context and all. Its next turn marks it done again; the idle it was
done at does not.

An agent started in a folder it has not seen stops at its folder-trust
prompt, and a worktree is always a new folder. `pastor task send t-3
--trust` presses the agent's trust keys (`trust_keys` under `[agents.<name>]`
in `pastor.toml`; Claude's are built in as `Down`, `Enter`) and saves the
task's machine and repo, the `--repo` as given, so every worktree of that
repo counts. It answers only a task blocked on its startup prompt; any other
task answers `not_at_trust_prompt`, and nothing is sent or saved. An agent
without trust keys answers `no_trust_keys`, and a task
without `--repo` gets the keys but nothing is saved. From then on, when a task
of a saved repo is blocked during startup on that machine, the head reads its
pane and, if it shows the agent's trust prompt, presses the trust keys itself,
once per task, and emits `task.trusted`; a task still blocked after that is
left for a human. The prompt is known by its `trust_marker` (Claude's is built
in as its "Yes, I trust this folder" option), so a task blocked on another
dialog the same keys would accept, such as Claude's bypass-permissions
warning, is left for a human too. The marker must be in the prompt at the
bottom of the pane (after the last output, and after any menu above the last
one), since the read includes scrollback where an earlier trust prompt can
linger above a later dialog. An agent with no marker (`trust_marker =
""`, or one that is not Claude and sets none) gets the keys without the check. Either way the task's prompt goes in
once, `settle` after the trust keys: Claude redraws for a moment after the
dialog and loses what is typed then, though herdr takes it.
`pastor trust list [--json]` shows
the saved pairs, `pastor trust add <machine> <repo>` saves one without a
blocked task, and `pastor trust remove <machine> <repo>` forgets one. With
`pastor serve` running they go through it, so the table is the head's; with
it down they read and write the store directly. `add` and `remove` change the
fleet: an agent pastor started is refused them unless `agents_change_fleet`
is on.

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
gone, no agent is in the checkout's workspace (an earlier retry of the
same task may have reopened it) and no other task's agent works in the
checkout from elsewhere (found by its path, as for removing a worktree,
below). Any other retry, a stale task's included (its agent may still be
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

### Where a task's pane goes

`place` decides where on its machine's herdr a task's agent gets its pane.
It is set like every dispatch setting: `--place` on `task run` (and on `task
retry`, to move a retry), `place = "..."` in a job's `[dispatch]`, `place` under
`[defaults]` in `pastor.toml`; `task describe` prints it. A head from before
`task retry --place` would retry the task where it was, so `task retry
--place` refuses one (`head_too_old`): restart `pastor serve` after an
upgrade.

- `repo` (the default) keeps an agent under the repo it works on. A
  `--worktree` task gets a new worktree, which herdr shows under the repo's
  workspace. A task whose `--repo` a workspace already shows, such as a fix
  round started in a pull request's worktree, gets a new pane in that
  workspace. herdr reports a workspace's directory only when it is a git
  checkout, and pastor compares it with the expanded `--repo`, trailing `/`
  ignored, without resolving symlinks. With no such workspace, or no repo, the
  task gets a workspace of its own named `t-N`, as before.
- `own`: always a workspace of its own named `t-N` (for a worktree, the one
  herdr opens on the new checkout), whatever already shows the repo.
- `pastor`: a pane in the machine's one workspace labelled `pastor`, made on
  first use in the home directory. The first workspace with that label is
  the one used, so a checkout of a repo named `pastor` that herdr labelled
  after its folder is taken too.
- `pane:<workspace>`: a pane in the workspace with that label. A machine with
  no such workspace fails the task before anything is made.

A `pastor` or `pane:<workspace>` workspace that closes while pastor places the
task is looked up once more; still gone, the task fails (`pastor` is made
again instead), and it never lands in a workspace of its own.

A pane in a workspace the task did not make is still the task's own: the
task records that pane and that workspace, and `task close` and auto-close
close only the pane, never the workspace or its other panes (herdr still
closes a workspace whose last pane closes, so one left with only the task's
pane goes with it). A worktree task placed in `pastor` or `pane:<workspace>`
still gets its worktree on disk and works in it; the workspace herdr opened on
the checkout is closed once the agent's pane is split off the shared one. A
retry whose `worktree.open` finds a workspace already showing the checkout
(the failed task's own, or one someone opened) never closes anything in it:
the retry's pane is split off it and the workspace keeps every pane it had.
Removing that worktree (`--remove-worktree`, or auto-close of a clean one)
closes the task's pane, has herdr open a workspace on the checkout
(`worktree.open`) and removes that; when herdr refuses (uncommitted changes),
the workspace it opened is closed again and the checkout stays. When a
workspace already shows the checkout, herdr answers that one, and removing
the checkout would close it: pastor did not open it, so the checkout stays
and the task closes with a note to remove it with `git worktree remove`
(its workspace is kept on the row, so `--remove-worktree` can try again
once that workspace is gone). Whatever the place, a task whose own dispatch found a
workspace already showing its checkout (a retry placed `repo` or `own` that
joined the failed task's workspace, or one someone opened) records that, and
its checkout is kept the same way on `--remove-worktree` and auto-close:
only the task's pane goes.

Since a fix round joins the workspace of the worktree it works in, removing
that worktree would end the fix round too. While another agent works in a
worktree task's checkout, `task close --remove-worktree` refuses and names
it before closing or opening anything, and auto-close closes the task's pane
but keeps the checkout, with a note on the task. pastor looks for such an
agent in the task's own workspace, in any workspace showing the checkout,
and among the other open tasks on the machine (and the failed ones that
name an agent, which may still be running) whose checkout or `--repo` is
that path, wherever their pane is: a fix round placed in `pastor` works in
the checkout from a pane of the shared workspace, which no workspace of the
checkout lists. An agent pastor did not start that works there from a
workspace not showing the checkout is not seen (herdr reports no directory
per agent).

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
# model = "sonnet"        # optional: a [models] name, see Models below
# priority = "high"       # optional: see Priority and queue order below
# agents = { opencode = "opencode" }  # optional: the agent per kind, see Models
# profile = "develop"     # optional: a permission profile, see Permission profiles below

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
load unchanged. A machine's `ssh` is one `[user@]host` word: one that is
empty, starts with `-`, or holds whitespace or a control character makes the
file fail to load the same way, since ssh would read it as an option. Put
ports, keys and jump hosts in `~/.ssh/config` under a host alias instead.

```
pastor flock list                       NAME, DEFAULT, MACHINES, AGENTS, QUEUED (--json)
pastor flock add <name> [--default]
pastor flock remove <name>              refused while it has machines or queued tasks, or is the default
pastor flock default show               print the default flock
pastor flock default set <name>         new tasks and jobs go to <name>
pastor machine add ... [--flock F]      default: the default flock
pastor machine move <name> <flock>
pastor machine list [--flock F]
pastor machine describe <name>          one machine in full (--json)
pastor flock describe <name>            one flock in full (--json)
pastor flock edit                       flock.toml in $VISUAL or $EDITOR
```

These commands edit `flock.toml` in place: comments, order and layout that
the edit does not touch stay as they were, and a running head picks the
change up at once, as it does for `machine add|remove`. The first `flock add`
on a file with no `[[flock]]` entry writes the implicit flock down as
`default` first, and the machines stay in it; with `--default` the new flock
takes the implicit one's place instead, and the machines that name no flock
move to it (`default` is still written down if a machine names it). While
tasks are queued in the implicit flock the machines stay there, so those
tasks keep somewhere to run, and the output names the tasks. `flock add` says
which flock those machines are in afterwards. `flock default set` writes the old
default flock onto every machine that named none, so changing where new work goes moves no machine.
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

### A flock's or a machine's agent

`agent` and `agent_args` under a `[[flock]]` entry are the agent its tasks and
jobs run when they name none. The same keys under a `[[machine]]` entry set it
for one machine, for flocks whose machines differ. Each task settles its agent
from the first of these that says:

1. `--agent` and `--agent-arg` on `pastor task run`, or `agent` and
   `agent_args` under a job's `[dispatch]`;
2. the machine the task runs on, in `flock.toml`;
3. the task's flock, in `flock.toml`;
4. `[defaults]` in `pastor.toml`;
5. the built-in: `claude` with no args.

The agent and its args are looked up on their own, with one rule: args follow
the agent they were written for. A layer's `agent_args` only apply when that
layer names no `agent`, or names the one the task runs. So with the `work`
flock above, `pastor task run --flock work --agent codex` runs codex without
`--model claude-sonnet-5`, and `[defaults] agent_args` (written for
`[defaults] agent`) do not reach a flock that runs another agent. An
`agent_args = []` is a choice, not a gap: it stops the lookup with no args.

Two machines of one flock can run different agents. Say both are personal
machines, but on the first the plain `claude` is logged in to a work account,
so it must run the personal one from an [agent definition](#agent-definitions),
while the second's plain `claude` already is the personal account:

```toml
# flock.toml
[[flock]]
name = "personal"
default = true

[[machine]]
name = "laptop"
ssh = "user@laptop"
flock = "personal"
agent = "claude-personal"   # `claude` here is the work account

[[machine]]
name = "desktop"
ssh = "user@desktop"
flock = "personal"          # no agent: the flock's, then [defaults]: claude
```

A task that is not pinned to a machine does not know its machine until it is
dispatched, so the head settles its agent again when it places it, from the
files as they stand then; a task pinned with `--machine` (or a job's
`machine`) has its machine's agent from the start. `pastor task describe` prints
the agent and args a task resolved to and where each came from, such as
`claude-personal (from machine laptop)` or `--model claude-sonnet-5 (from
flock work)`; before dispatch it shows what the task would run without a
machine of its own. Once dispatched, a task keeps what it ran, so a later
edit of `flock.toml` or `pastor.toml` changes only tasks not yet placed, and a
retry settles again on the machine it lands on. `pastor task run` makes the
head apply an edit of `pastor.toml` or `flock.toml` first; for a job, an edit
reaches the head on its next tick, or at once with `pastor job reload`.
Changing a machine's agent, like moving it, keeps its connection.

### Tool allow and deny lists

Unless a [permission profile](#permission-profiles) applies, pastor leaves
the agent's own permission mode alone: Claude still asks before it runs a
tool its settings do not already allow, and a task that waits on
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
`pastor task retry`; a job records the error for that item). `pastor task describe`
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
in `[defaults]`. While a permission profile applies to a Claude task, args
that pick a permission mode are refused instead (`profile_args_conflict`).

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
one, the name itself), and `task describe` and `task list` keep the name
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

The head checks a task's agent against pastor.toml when it queues it (for a
task that is not pinned, the agent each machine of its flock would run), and
`pastor task run` makes the head apply an edit of pastor.toml or flock.toml
first, so a definition you have just added is the one its machine starts.

`pastor machine move` changes the flock of tasks dispatched after it; tasks
already on the machine keep running there, and its connection stays up. A
queued task pinned to a machine that has moved to another flock stays queued
for its own flock, and the head logs a warning once.

### Models

`[models.<name>]` in pastor.toml names a model, so a task can pick it by name
instead of repeating the agent's own flags. Each has a `kind`, the herdr agent
kind whose agents can run it, and `args`, the argv that selects it (it may be
`[]`); both are required. Names follow the job names' rules
(`[a-z0-9][a-z0-9_.-]{0,63}`), and there are no built-in models. A bad entry
fails the file's load.

```toml
# pastor.toml
[models.sonnet]
kind = "claude"
args = ["--model", "claude-sonnet-5"]

[models.opus]
kind = "claude"
args = ["--model", "claude-opus-5-5"]

[defaults]
model = "opus"
```

```toml
# flock.toml: the personal flock runs sonnet unless a task says otherwise
[[flock]]
name = "personal"
agent = "claude-personal"
model = "sonnet"
```

A task settles its model like its agent, from the first of these that names
one: `--model` on `pastor task run` (or `model` under a job's `[dispatch]`),
`model` under the `[[machine]]` entry it runs on, `model` under its
`[[flock]]` entry, `[defaults] model`; with none, it runs no model and nothing
changes. `--model` takes a name only, never raw args.

A job's `model` is a template, rendered for each item:
`model = "{{ item.model }}"` runs the model the item names. It may use
`item.*` and `job.name`. When it renders empty, the job says nothing and the
lookup goes on to the machine, the flock and `[defaults]`.

The model's `args` go first, then the task's `agent_args` settled as above,
then the allow and deny flags, so permission flags in `agent_args` still
apply. A task in the personal flock above starts as `claude --model
claude-sonnet-5 ...` under the `claude-personal` definition.

A model runs only on agents of its kind (the agent's `kind`, or its name
without a definition). A task that asks for an agent of another kind with
`--agent`, or is pinned to a machine with no agent of the model's kind (see
[An agent per kind](#an-agent-per-kind)), is refused (`model_kind_mismatch`).
A task that is not pinned is offered only to the machines of its flock that
have an agent of the model's kind; the others are skipped as machines of
another flock are. If none has, it stays queued and
`pastor task describe` says why in its `error`.

A name `[models]` does not define is refused: `unknown_model` from `pastor
task run` and `pastor task retry`, and the job records the error for that
item. In flock.toml, a flock's or a machine's unknown `model` fails the load
(the head keeps the previous flock on a reload).

`pastor task describe` prints the model and where it came from, such as
`model: sonnet (from flock personal)`; `task list` has a MODEL column and
`--json` a `model` field; `flock describe` and `machine describe` show their
own `model`; task events carry `model`. The store keeps the name, and a retry
settles the model's args again from pastor.toml as it stands. Since a head
from before models would start the agent without its model, every command
that can make it queue a task refuses one (`head_too_old`).

### Permission profiles

A permission profile names a pair of tool pattern lists, `allow` and `deny`,
in the syntax of [Tool allow and deny lists](#tool-allow-and-deny-lists), so
a kind of work can be named once, and so that a Claude task under one never
stops at a permission prompt.

Three are built in, in Claude Code's patterns:

| Profile        | Allows                                                     | Denies                                                        |
| -------------- | ---------------------------------------------------------- | ------------------------------------------------------------- |
| `review`       | `Read`, `Glob`, `Grep`, `git status`, `diff`, `log`, `show`, `blame` | `Edit`, `Write`, `NotebookEdit`, `git push`, and the destructive ones |
| `develop`      | the read tools, `Edit`, `Write`, `NotebookEdit`, `Bash`    | the destructive ones: `rm -rf`, `sudo`, `git push --force`   |
| `unrestricted` | everything `develop` allows, `WebFetch`, `WebSearch`       | nothing                                                       |

`[profiles.<name>]` in pastor.toml adds one, or replaces the built-in one of
that name. Every key is optional: `description`, one line for `profile list`;
`extends`, another profile, built in or not; `allow` and `deny`. Names follow
the job names' rules (`[a-z0-9][a-z0-9_.-]{0,63}`).

```toml
# pastor.toml
[profiles.ci]
description = "develop, plus docker"
extends = "develop"
allow = ["Bash(docker:*)"]
deny = ["WebFetch"]
```

A profile's lists are its own added to those of the profile it extends,
farthest first, each pattern once. As everywhere in pastor a deny wins: a
pattern any link denies is dropped from `allow`, so a profile can narrow the
one it extends but never lift its deny. That is why `develop` does not extend
`review`. An `extends` that names no profile, or a chain that comes back to
itself, fails the file's load, as does a pattern that is empty or starts with
`-`.

```bash
pastor profile list               # NAME, SOURCE (built-in, pastor.toml), EXTENDS, DESCRIPTION; --json
pastor profile describe ci        # the chain (ci -> develop) and the allow and deny it adds up to; --json
```

Both read pastor.toml on this machine and never ask the head, so they work
with it down; with a remote head set they are refused
(`remote_head_unsupported`). A name that is not a profile is
`unknown_profile`.

### Priority and queue order

A task that no machine can take yet waits in the queue. Each task has a
level, `low`, `normal`, `high` or `critical`, and each dispatch pass takes
queued tasks by level, highest first, then by position, then oldest first.
A task that does not fit anywhere (its machines are full, or none has its
tags) is skipped and the pass goes on to the next, so a lower task can
still start where a higher one cannot.

A task settles its level when it is queued, from the first of these that
sets one: `--priority` on `pastor task run` (or `priority` under a job's
`[dispatch]`), `priority` under the `[[machine]]` entry it is pinned to
(`--machine` or a job's `machine`; an unpinned task never takes a
machine's, since no machine is picked yet), `priority` under its
`[[flock]]` entry, `[defaults] priority`; with none it is `normal`.

```toml
# pastor.toml
[defaults]
priority = "low"            # tasks that set none

# flock.toml
[[flock]]
name = "work"
priority = "high"           # this flock's tasks, before [defaults]

[[machine]]
name = "pi-3"
flock = "work"
priority = "critical"       # tasks pinned to pi-3, before the flock's
```

A job's `priority` is a template, rendered for each item, so `priority =
"{{ item.priority }}"` takes the level the item names; rendered empty, it
falls through to the machine, flock and `[defaults]`. A value that is not
one of the four levels is refused: `unknown_priority` from `task run
--priority` and `task priority`, an error for that item in a job (the
cursor holds, as for any item error), and a load failure in pastor.toml,
flock.toml or the job file.

`pastor task priority t-4 critical` puts a queued task at another level; it
keeps its position, so among the tasks of its new level it goes by when it
was queued. A task a machine has taken has left the queue, and is refused
with `not_queued`; an agent pastor started is refused, as for any change to
the fleet (`agent_refused`). `pastor task retry` keeps the level of the task
it copies, and the copy queues last in it.

`pastor task describe` prints the level and the layer that set it, such as
`priority: high (from flock work)` (`task priority` when set by hand);
`task list` has a PRIORITY column, and `--json` has `priority`,
`priority_from` and `queue_pos`, the task's position. A head from before
levels would queue the task at its own level without a word, so `task run
--priority` and `task priority` refuse one (`head_too_old`).

### Room on a machine

`max_agents` on a `[[machine]]` (default 2) is how many tasks it runs at
once: its shared slots. Two more keys make room past them:

```toml
# flock.toml
[[machine]]
name = "pi-3"
ssh = "user@pi-3"
max_agents = 2
job_slots = 1       # default 1: extra slots only tasks from jobs take
burst = 1           # default 1: how far past max_agents a critical task goes
```

A task is from a job unless `pastor task run` made it. Up to `job_slots` live
job tasks count as in job slots; every other live task, and every orphan,
counts against `max_agents`. A job task takes a free job slot, then a shared
one. A `critical` task that finds the shared slots full may start while the
live tasks outside job slots are fewer than `max_agents + burst`; a critical
job task tries a job slot, then a shared slot, then burst. `0` turns either
off, and a normal `task run` task is always held at `max_agents`. So two long
`task run` tasks on a two-slot machine leave a job's task room to start, and
a critical task still gets past a full machine.

Among the machines with room for the task, the one with the fewest live tasks
takes it; ties keep flock order. `machine add` takes `--job-slots` and
`--burst` as it takes `--max-agents`. `machine list` shows the room as
`2+1j+1b` (`2` alone when both are 0), `machine describe` as `1 of 2+1j+1b`,
and `--json` has `job_slots` and `burst` on each machine. Changing either
restarts the machine's actor on reload, as changing `max_agents` does.

#### An agent per kind

A machine whose agent is a claude can still run a model of another kind if
it says which agent runs that kind. `agents = { <kind> = "<agent>" }` goes on
a `[[machine]]` or a `[[flock]]` entry in flock.toml, and under `[defaults]`
in pastor.toml:

```toml
# pastor.toml
[models.gpt]
kind = "opencode"
args = ["--model", "openai/gpt-5.5"]
```

```toml
# flock.toml: desk runs gpt as its opencode; pi-3 has no opencode agent
[[flock]]
name = "personal"
default = true
agent = "claude-personal"
agent_args = ["--permission-mode", "auto"]

[[machine]]
name = "desk"
ssh = "user@desk"
agents = { opencode = "opencode" }

[[machine]]
name = "pi-3"
ssh = "user@pi-3"
```

The task's agent is settled as above first. When the model's kind differs
from that agent's, pastor looks the kind up through the machine, the flock
and `[defaults]`, in that order: at each, the layer's own `agent` if it is of
that kind, else its `agents` entry for the kind. The first hit runs the task.
So `pastor task run --model gpt` above lands on desk and starts `opencode
--model openai/gpt-5.5`, while its claude tasks still run as
`claude-personal`, and pi-3 never gets a gpt task. An agent the task or job
named itself (`--agent`, a job's `agent`) is kept, and its kind must match
the model's (`model_kind_mismatch`).

An agent found this way takes `agent_args` only from layers whose `agent` is
that same agent: a layer's `agent_args` with no `agent` are for its default
agent, so the flock's claude args above never reach opencode.

A value is any name `agent` accepts, a definition under `[agents]` or a
built-in kind, and its kind must be the key: `agents = { opencode =
"claude-personal" }` fails the load, as does an entry for the kind of the
layer's own `agent` (which already runs that kind). An unpinned task goes only
to the machines where the lookup finds an agent; with none in its flock it
stays queued, and `pastor task describe` says why, such as `no machine in
flock personal has an opencode agent`. A task pinned to a machine with none
is refused (`model_kind_mismatch`), and a job records the error for the
item. `task describe` shows where such an agent came from, such as `agent:
opencode (from machine desk agents.opencode)`; `machine describe` and
`flock describe` list the entry's own `agents` as `by kind`. A pastor from
before this refuses the key on a `[[flock]]`, so add it once every machine
runs a release that knows it.

#### A task's profile

A task settles its profile like its model, from the first of these that
names one: `--profile` on `pastor task run` (or `profile` under a job's
`[dispatch]`), `profile` under the `[[machine]]` entry it runs on, `profile`
under its `[[flock]]` entry, `[defaults] profile`; with none, it runs no
profile and nothing changes. `--profile` and a job's `profile` take a name,
not a template.

```toml
# pastor.toml
[defaults]
profile = "review"

# flock.toml
[[flock]]
name = "work"
profile = "develop"
```

The profile's `allow` and `deny` go before the task's own lists (from
`[defaults]`, the flock and the job, as in [Tool allow and deny
lists](#tool-allow-and-deny-lists)), each pattern once, and a deny from
either side drops the pattern from `allow`. A Claude agent (an agent of kind
`claude`) then starts with `--permission-mode dontAsk` after its args and
before the tool flags: it does not ask about a tool the lists and its own
settings do not allow, it is refused it. A task in the `work` flock above
starts as `claude ... --permission-mode dontAsk --allowedTools Read ...
--disallowedTools 'Bash(rm -rf:*)' ...`. Agent args that pick a permission
mode themselves (`--permission-mode`, `--dangerously-skip-permissions`, in
`agent_args` or a model's `args`) are refused while a profile applies
(`profile_args_conflict`), since Claude would take only one of the two.

An opencode agent (kind `opencode`) has no flags for tool lists, so it gets
the lists in its pane's env instead, as `OPENCODE_PERMISSION`, the JSON
opencode merges over its config's `permission`, and starts with no
permission args. pastor writes the patterns in opencode's terms: everything
is denied first (`"*": "deny"`), so a tool the lists do not allow is refused,
not asked about, except under `unrestricted`, which starts from
`"*": "allow"` so tools with no Claude name, such as MCP ones, stay open; then each allowed tool, then each denied one, since opencode
takes the last rule that matches. `Read` is opencode's `read` and `list`,
`Glob` `glob`, `Grep` `grep`, `Edit`, `Write` and `NotebookEdit` `edit`,
`Bash` `bash`, `WebFetch` `webfetch`, `WebSearch` `websearch` and `Task`
`task`; `Bash(git log:*)` is `bash` on `git log` and `git log *`, and any
other argument goes as written. A tool opencode has no permission for adds
nothing, and the agent's to-do list (`todoread`, `todowrite`) is always
allowed. The pane also gets `OPENCODE_CONFIG`, `OPENCODE_CONFIG_DIR` and
`OPENCODE_CONFIG_CONTENT` set empty, over an `[agents]` env that sets them,
so no other config file adds rules around the profile's.

Before it makes anything on the machine, pastor checks the machine's own
opencode config (`config.json`, `opencode.json` and `opencode.jsonc` in
`$XDG_CONFIG_HOME/opencode`, else `~/.config/opencode`): a `"permission"` key
anywhere in them, even under an agent, fails the task
(`opencode_permissions_conflict` in its `error`), since opencode would merge
those rules with the profile's. Move them out, or run the task without a
profile. A `command` machine cannot be checked, and goes ahead. The repo's
own opencode config (an `opencode.json` in the checkout) is still read, and
its rules would merge with the profile's too; keep permission rules out of
repos that profiled opencode tasks run in.

An agent of any other kind gets the lists through its `allow_flag` and
`deny_flag`, as any list, and keeps its own permission mode.

Patterns are passed as written. Claude reads a `~/` path in a pattern
(`Read(~/.ssh/**)`) as the home of the machine it runs on, so pastor does not
expand it, and a machine that cannot report its home (a `command` one) runs
such a profile too.

**The unrestricted rule.** A task may ask for `unrestricted` (with
`--profile` or a job's `profile`) only on a machine whose own profile, the
one its tasks get when they name none, is `unrestricted` as well: its
`[[machine]]` entry says so, or its flock's, or `[defaults]`. Denying nothing
is the choice of whoever owns the machine, made in flock.toml or
pastor.toml, not one a run flag or a job file can make for it. A task pinned
to another machine is refused (`profile_not_allowed`); one that is not
pinned is offered only to such machines of its flock, and waits with the
reason in its `error` while none has a free slot or none exists.

The head checks the profile when it queues a task, and again when it places
it on a machine. An unknown name is refused (`unknown_profile` from `pastor
task run` and `pastor task retry`; the job records the error for that item),
and a flock's or machine's unknown `profile` fails flock.toml's load (the
head keeps the previous flock on a reload). A profile removed from
pastor.toml while a task waits for a machine keeps the task queued, with
`waiting for a machine: profile ... is not built in ...` in its `error`,
rather than starting it without the profile; put it back, or close the task.
A retry settles the profile again from pastor.toml as it stands.

`pastor task describe` prints it and where it came from, such as `profile:
develop (from flock work)`, and the task's `--json` has a `profile` field;
`machine list` has a PROFILE column, `machine describe` a `profile` line with
the profile the machine's tasks get, and `flock describe` the flock's own.
Since a head from before profiles would start the agent without them, every
command that can make it queue a task (`pastor task run`, `pastor task
retry`, `pastor tick` without `--dry-run`, `pastor job run`) refuses one
(`head_too_old`): restart `pastor serve` after an upgrade.

## Events

`pastor serve` appends every task, job and machine event to
`~/.local/state/pastor/events.jsonl`, one JSON record per line. When the next
line would take the file past 10 MiB it is moved to `events.jsonl.1`
(replacing the previous one) and a new file started, so the log keeps at most
two generations. `pastor events` prints both, oldest first; `--task t-3` keeps
one task's records, `--json` prints the records as stored, and `--follow`
keeps printing as new ones are written. It reads the file, not the daemon, so
it works with `pastor serve` down.

With a remote head set (see [A head on another machine](#a-head-on-another-machine)
below), `pastor events` asks the head instead, through `events_since`: it pages
from the start of the head's log, 500 records at a time, and with `--follow`
asks again every second. `--task` and `--json` work as they do here, and the
output is the same, except that lines written before pastor numbered records
are not shown. When records the command had not read yet were rotated out of
the head's log (`gap: true`), it prints one line to stderr naming the missing
numbers and carries on from the oldest record left; a first read of a log
that has rotated says so too. It needs a head speaking protocol 4 or newer
(`head_too_old` otherwise).

A record, which is also what connector event hooks get on stdin:

```json
{
  "seq": 812,
  "at": "2026-09-24T10:15:02.123Z",
  "type": "task.done",
  "task": {"id": 3, "job": "triage", "item": {"key": "...", "title": "..."},
           "prompt": "...", "spec": {"agent": "claude", "...": "..."},
           "machine": "pi-3", "state": "done", "error": null, "...": "..."},
  "job": "triage",
  "machine": null
}
```

- `seq`: the record's number, from 1. It only grows, never repeats, and
  carries on across restarts of the head and rotations of the log (the last
  one given is kept in the head's database). Lines written before pastor
  numbered records read as 0, and so does a record built after that if the
  head's database cannot hand out a number for it; consumers must not assume
  every new record has a positive `seq`.
- `at`: when the daemon received the event, RFC 3339 UTC.
- `type`: `task.queued|running|blocked|done|stale|failed|closed`,
  `task.input` (`pastor task send`), `task.trusted` (the head answered a
  trust prompt), `job.failed`, `connector.finish_failed` (a connector's
  `[finish]` command failed), `machine.connected`, `machine.lost`.
- `task`: the full task row (the same object as `pastor task describe --json`) at
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
  `task.trusted`, `keys`; on a `task.blocked` for an agent that ended its
  turn on a question, `question`; on `connector.finish_failed`, `connector`
  (its id) and `reason` (why: the exit status and stderr tail, or a timeout).

Fields may be added; none will be renamed or removed. Unreadable lines (a
torn write, a hand edit) are skipped.

A client can read the log through the head by number: the IPC request
`events_since` (`{"op":"events_since","after":N,"limit":L,"task":ID}`, head
protocol 4 or newer) answers the records whose `seq` is past `N`, oldest
first, at most `L`, only those about task `ID` if given, with `oldest`, the
oldest number the two log files still hold, `newest`, the newest number they
hold (whatever `N`, `L` and `ID`, so a limit of 0 asks only where the log
ends), and `gap: true` when records after `N` were already rotated out of both.
It is a read, so an agent may send it.

## Watch

`pastor watch` is the events log cut down to what someone watching the fleet
acts on, one line per change, for an orchestrator to wait on:

```bash
pastor watch --name night            # every minute, what changed; never returns
pastor watch --now                   # what needs attention right now, then exit
pastor watch --interval 2m --all --json --connector prs
```

```
TASK t-12 failed pi-3 nightly: agent exited
TASK t-14 blocked pi-1 run
JOB nightly failing: failed (2x): exit 1: gh: rate limited
JOB nightly ok
HEAD down: pastor serve is not running (...); start it with `pastor serve`
HEAD up
HEAD gap: events 41..57 were rotated out of the log before this watcher read them
CONNECTOR prs failing: exit 1: gh: not logged in
CONNECTOR prs ok
PR 31 reviewed, 2 open threads
```

- `TASK <id> <state> <machine> <job>[: <error>]`, from the head's numbered
  events: a task that reached `blocked`, `done`, `failed` or `stale`
  (`--all`: every state change, `queued` and `running` too). `-` stands for
  a machine or job it has none of. The error is only there for `failed` and
  `stale`.
- `JOB <name> failing: <last result>` when an enabled job's last run failed,
  again when that result changes (`failed (1x)` becomes `failed (2x)`), and
  `JOB <name> ok` when it runs fine after that, from `job list`. A job that is
  disabled or removed is dropped without a line.
- `HEAD down: <why>` when the head stops answering, once, and `HEAD up` when
  it answers again. `HEAD gap` says the log rotated events out before the
  watcher read them; `pastor watch --now` shows where things stand.
- A connector's lines, as it printed them, and `CONNECTOR <id> failing: <why>`
  / `CONNECTOR <id> ok` around a spell of failed runs (see [The watch
  command](#the-watch-command)).

Each interval (`--interval`, `1m` by default) the watcher asks the head for
the events after the last one it read, then `job list`, then runs its
connectors. A watcher keeps a cursor in
`~/.local/state/pastor/watch/<name>.json` (`--name`, `default` by default):
the last event number it read, the failing jobs and connectors, whether the
head was down, and the connector lines it has printed (the last 1000 per
connector). Started again with the same name it carries on where it stopped
and repeats nothing, so an orchestrator can re-arm it as often as it likes. A
new name, or `--reset`, starts at the end of the log: the past is what `--now`
is for.

`--now` reads no cursor and writes none. It prints every task that is
`blocked`, `done`, `failed` or `stale` (`--all`: the live ones too), every
failing job, `HEAD down` if the head does not answer, and every line each
connector prints now, then exits 0.

The connectors are those `--connector` names (repeatable), or else
`[[watch.connector]]` in pastor.toml:

```toml
[[watch.connector]]
name = "prs"                         # a connector id with a [watch] command
```

`--json` prints one object per line: `kind` (`TASK`, `JOB`, `HEAD`,
`CONNECTOR` or `OUTPUT` for a connector's line), the fields of the line
(`task`, `state`, `machine`, `job`, `reason`, `connector`, `text`) and
`line`, the text form. With a head on another machine (`pastor head set`) the
events, tasks and jobs are that head's, and the connectors run here. The
watcher only reads, so an agent pastor started may run it; through a bridge
that limits an agent to its own tasks (`bridge --agent`) the head refuses the
events, and the watcher stops with the head's error. So does one whose head
predates `events_since`. A head that answers with any other error stops it
too; one that does not answer at all is `HEAD down`.

Runtime errors print JSON on stderr with a stable `code` and exit 1; a
malformed command line gets clap's plain usage text and exit 2.

Each machine needs herdr 0.9 or newer (protocol 22 or newer) with its server
running, and SSH access from the head without a passphrase prompt (a key in
ssh-agent won't be there for a service; use a dedicated key or Tailscale SSH).

## Try it

For a fleet you'll keep, see `docs/recommended-setup.md` first: which
credentials each machine gets, and the settings that keep unattended agents
moving.

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
pastor task run "Fix the typo in README" --model sonnet   # a [models] name from pastor.toml
pastor task run "Triage the inbox" --flock work   # only work machines take it
pastor task run --prompt-file ./prompt.md --repo '~/work/api'   # a long prompt, no shell quoting
pastor task run --prompt-file ./plan.md --role orchestrator   # may run, retry and send to tasks and disable jobs
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
pastor task priority t-5 high        # a queued task goes ahead of normal ones
pastor task close t-1 --remove-worktree   # close its pane and remove its worktree
pastor task done t-1                 # mark it done; its pane closes after close_done_after
pastor task prune --done --closed --older-than 7d
pastor task attach t-1               # lands in the agent's pane; ctrl+b q detaches
                                     # (a closed Claude task: its session, reopened)
pastor machine open pi-3             # the full herdr UI on that machine
pastor events --follow               # task, job and machine events as they happen
pastor watch --now                   # what needs attention: blocked, done, failed tasks, failing jobs
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
`--model` (a name from `[models]`, see [Models](#models)), `--priority` (see
[Priority and queue order](#priority-and-queue-order)), `--profile` (see
[Permission profiles](#permission-profiles)), `--worktree`, `--branch` (with `--worktree`), `--tag` (repeatable),
`--timeout`, `--place` (see [Where a task's pane goes](#where-a-tasks-pane-goes))
and `--json`. `--agent-arg` hands one argument to the agent,
through herdr's `agent.start`; repeat it for more, in order. It always takes the
next word as its value, even one that starts with a dash, so
`--agent-arg --model --agent-arg claude-opus-5-5` and
`--agent-arg=--model --agent-arg=claude-opus-5-5` mean the same thing; the
`=` form just reads more clearly. There is no single-string form: pastor would
have to split it on spaces, and that breaks any argument that contains one. A
job file's `agent_args` does the same for its tasks. When neither says
anything, the machine's `agent_args` apply, then the flock's, then
`[defaults] agent_args` in pastor.toml (see [A flock's or a machine's
agent](#a-flocks-or-a-machines-agent)); a job file that sets `agent_args = []`
opts out of all three. `pastor task describe t-1` prints the agent and
args a task was started with.

An agent of kind `claude` also gets `--session-id <uuid>`, after every other
argument, with a new id per task. The task records it (`session:` in `task
describe`, `spec.session_id` in its JSON) so that `pastor task attach` can
reopen the conversation after the pane is gone (see [Reopening a finished
task](#reopening-a-finished-task)). Args that already choose a session
(`--session-id`, `--resume`/`-r`, `--continue`/`-c` or `--fork-session`) are
left as they are, and nothing is recorded. A retry starts a new session.

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

### Reopening a finished task

`pastor task attach t-N` on a task whose pane is still there lands in it, as
always. Once the task is `closed` or `failed`, pastor looks on the task's
machine first: while herdr still lists the task's agent, attach goes to it.
Otherwise, for a task that recorded a Claude session, it opens a new
workspace on that machine, labelled `t-N-resume`, in the task's directory
(its worktree, else its repo), with the env of the task's agent definition
(so `CLAUDE_CONFIG_DIR` points at the same account's sessions), runs `claude
--resume <session>` there as the agent `t-N-resume`, and attaches. Attaching
again while that pane is open goes back to it.

The pane is yours, not the task's: the task's state does not change, and
closing the pane leaves no trace on it. Claude files its sessions by
directory, so a worktree removed at close is first put back at the same path
on the task's branch (`git worktree prune`, then `git worktree add <path>
<branch>`, in the task's repo); when the branch is gone too, attach fails with
`branch_gone` and opens nothing. A task of another kind (opencode, codex), or
a Claude task that recorded no session, has nothing to reopen: attach fails
with `no_agent`, as before, and says so. A `command` machine has no terminal,
so attach refuses it with `no_terminal` either way.

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
systemd the way they do in a terminal; re-run it after moving a binary.
Entries anyone could plant a binary in are left out and named on stderr: an
empty or relative entry (`.`, `node_modules/.bin`), a world-writable
directory and one that does not exist, since someone else could create it
later. `herdr` is looked up in the `PATH` the unit keeps, and setup fails
if it is only in an entry left out. A value with a line break is refused, since it would start a new
directive, and a `$` in the binary's path is written `$$`, since systemd
expands it in `ExecStart`.
`pastor.service` also gets the config, state and data dirs this run resolved,
as absolute `PASTOR_CONFIG_DIR`, `PASTOR_STATE_DIR` and `PASTOR_DATA_DIR`, so
the head uses the same dirs as the shell that set it up, whether they came from
an override, an XDG variable or the default. A unit
that differs from what setup would write is kept as `<unit>.service.bak`, and a
running service is not restarted, since restarting herdr stops its agents: run
`systemctl --user restart pastor` (or `herdr`) yourself.

`pastor.service` runs with `NoNewPrivileges=yes`, `UMask=0077`,
`LockPersonality=yes` and `RestrictRealtime=yes`: what works in a user unit
without user namespaces and leaves pastor's own writes alone.
`ProtectSystem=strict`, `PrivateTmp` and `ProtectHome` are left off: they would
stop ssh updating `known_hosts` and connectors writing their caches, hide a
state dir under `/tmp` or `~/.ssh`, and on a host without unprivileged user
namespaces keep the unit from starting. `herdr.service` has no hardening,
since the agents it runs need what the user's own terminal allows. ssh runs
with `BatchMode=yes`, so it cannot ask for a passphrase under systemd: add
`Environment=SSH_AUTH_SOCK=...` for an agent, or use a key without one.

A user service stops at logout unless lingering is on. Setup checks
`loginctl show-user` and prints `loginctl enable-linger` when it is off.
For `pastor.service` it also sets the config and state dirs to 0700 and the
socket and connector `.env` files to 0600, and says what it changed.

## Run under launchd (macOS)

`pastor setup launchd` is the macOS counterpart, with the same flags and the
same ask-first prompt. It writes `contrib/launchd/pastor.plist` to
`~/Library/LaunchAgents/pastor.serve.plist` (label `pastor.serve`; with
`--herdr`, `pastor.herdr` from `contrib/launchd/herdr.plist`), pointing
ProgramArguments at the binary it finds and copying your shell's `PATH` (and,
for pastor, the three absolute `PASTOR_*_DIR` values) into
EnvironmentVariables. Output goes to `~/Library/Logs/<label>.log`. The actions
run in your `gui/<uid>` domain: `--enable` is `launchctl enable`, and on an
agent that is not loaded yet also `launchctl bootstrap`, which starts it:
unlike `systemctl enable`, launchd has no way to register an agent without
loading it, and `RunAtLoad` starts it as it loads. `--start`
is `launchctl bootstrap` (or `kickstart` when the agent is already loaded),
`--stop` is `launchctl bootout`, and no flag means enable and start. Both
agents have `RunAtLoad` and `KeepAlive`, so they come back after a crash and
at the next login, and run only while you are logged in. A changed plist is
kept as `<label>.plist.bak`; a loaded agent keeps the old one until you
`launchctl bootout` and `bootstrap` it again, which setup prints.

On macOS pastor uses the same XDG layout as Linux (`~/.config/pastor`,
`~/.local/state/pastor`, `~/.local/share/pastor`), next to herdr's own
`~/.config/herdr`. A config left in `~/Library/Application Support/pastor` by
an older pastor is moved there on the first run, with a note on stderr,
unless `PASTOR_CONFIG_DIR` or `XDG_CONFIG_HOME` points the config elsewhere. That
pastor still said plugins: the checkouts it kept in `plugins/` go to
`~/.local/share/pastor/connectors` and their `.env` files to
`~/.config/pastor/connectors`. A linked connector's `.env` sat in its own
checkout (the link pointed there) and stays there; the note names it, to copy
by hand. Rename each `pastor-plugin.toml` to
`pastor-connector.toml` by hand, as for any upgrade from plugins.
A move that fails part way leaves `~/.config/pastor/.pastor-migrating`
behind, and the next run finishes it.

## Connectors

A connector is a directory with a `pastor-connector.toml` and the commands it
names. It can provide a connector command (where a job's items come from),
event hooks, or both. `tests/fixtures/connector/` has small working examples (`echo`
and `stream` connectors, a `notify` hook).

```
~/.local/share/pastor/connectors/<id>/   the connector: a checkout, or a symlink for `connector link`
~/.config/pastor/connectors/<id>/.env    secrets and settings, written by you
~/.local/state/pastor/connectors/<job>/  the job's scratch directory, owned by pastor
~/.local/state/pastor/runs/<job>/        run logs, one <ts>.log per run
```

`PASTOR_DATA_DIR` overrides the first. The directory name is the connector's id
and must equal `id` in the manifest.

### The manifest

```toml
id = "slack"                         # required: [a-z0-9][a-z0-9_.-]{0,63}
name = "Slack"                       # optional, defaults to the id
version = "0.1.0"                    # required: major.minor.patch, numbers only
min_pastor_version = "0.1.0"         # optional: refuse to load on an older pastor
description = "Watch a channel, report back in thread"
authors = ["Ana <ana@example.org>"]  # optional, like the next three
homepage = "https://example.org/slack-connector"
repository = "https://github.com/owner/repo"
license = "MIT"

[connector]
mode = "poll"                        # poll (the default) or stream
command = ["bash", "poll.sh"]        # argv, run in the connector's directory
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

[finish]                             # optional: runs when a task of its jobs ends
command = ["bash", "close-issue.sh"]
timeout = "60s"                      # the default

[watch]                              # optional: lines for `pastor watch`
command = ["bash", "prs.sh"]
timeout = "60s"                      # the default
```

The manifest is checked when pastor discovers the connector, and a key it does
not know is an error. A connector needs a `[connector]`, at least one `[[events]]`
hook or a `[watch]` command, and may have any mix of them. Each `command` is an argv array with a program in its first
place; a relative program with a slash (`./poll`) means the connector's own file.
An `on` entry is an event type like `task.done`, and timeouts may not be
zero. `[finish]` needs a `[connector]`: only a connector's own jobs have tasks
to finish. The id `clock` belongs to the built-in connector. A connector that fails
any of this, or asks for a newer pastor than the one running, is listed by
`connector list` as invalid with the reason, and a job that uses it is invalid
too.

`config` and `secrets` are declarations, with no schema language beyond
`required` and `description`: pastor checks that a job's `[connector]` table
has every `required` key and passes the rest through untouched, and
`connector list` reports the secrets the `.env` leaves unset or empty. Secret
names must look like environment variables (`[A-Z_][A-Z0-9_]*`).

`authors`, `homepage`, `repository` and `license` are for people reading
`connector describe`; pastor shows them and checks nothing about them. They
are new in the release after 0.5.0. pastor 0.5.0 reads a manifest strictly and
rejects these fields as unknown keys, naming the key; that can't be changed in
a release already out. From this release on, pastor checks
`min_pastor_version` before the strict read, so a connector that adds fields a
future pastor introduces, and raises its `min_pastor_version` to match, gets
"needs pastor X or later" on an older one instead of an unknown key.

### Connector protocol

To run a connector, pastor starts its `command` in the connector's directory
with the connector's `.env` in its environment plus `PASTOR_CONNECTOR_ID`,
`PASTOR_JOB`, `PASTOR_CONFIG_DIR`, `PASTOR_STATE_DIR` and
`PASTOR_CONNECTOR_STATE_DIR` (the job's scratch directory, created 0700). It
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
  logs it, and `connector try` prints it.
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
that log. So does a run that returns more than 10,000 items, or more than
64 MiB of them: pastor keeps no more items past that point and fails the run
once the connector exits, so a connector with more to hand over should page
with its cursor.

A stream connector gets the same handshake once, when it is started, and
answers in the same lines at any time; it owns its own sockets and pastor
proxies nothing. If it exits it is started again with backoff (1s doubling to
5 minutes; a run that stayed up a minute starts it over). Pastor hands the
restarted process, in its handshake, the newest cursor the connector emitted
before it exited, whether or not a job run has saved it yet, or the job's
saved cursor if it has emitted none since the daemon started. Between job
runs pastor holds up to 10,000 of a stream's items and 64 MiB of them, the
batch handed out but not yet saved included, and 1,000 of its log lines; past
that the oldest are dropped, with one warning in the next run's log. How a job run takes a stream's output is
under "Using a connector in a job".

### Secrets and the `.env` file

Secrets and settings go in `~/.config/pastor/connectors/<id>/.env`, which every
command of the connector (connector command and hooks) gets in its environment. The
format is the common dotenv subset: `KEY=value` lines, an optional `export `,
`#` comments, single quotes for a literal value and double quotes for one with
`\n`, `\"` and `\\` escapes, no interpolation; the last of a repeated key
wins. A line that does not parse makes every command of that connector fail
naming the line, and `connector list` shows the error. `pastor setup systemd`
sets these files to 0600.

Only what the manifest declares under `[secrets]` is treated as secret. What
a run writes to stderr lands in `~/.local/state/pastor/runs/<job>/<ts>.log`
(a hook's or finish command's stdout goes to its log too); in those logs, in the connector's `log`
records and in the reason a failed run reports, the value of every declared
secret is replaced by `[redacted:NAME]` (a value shorter than four characters
is left alone, since hiding it would mangle the log and protect nothing). Redaction works line by line, so a
declared secret may not contain a line break (a double-quoted `\n`): pastor
refuses such a `.env` and names the variable. Each log is cut at 256 KiB, and
each run directory keeps its newest 20: `runs/<job>/` for a job's connector
runs, and `runs/@<id>/` for all of a connector's hook and finish runs together. A stdout or stderr line longer than 256 KiB is
cut there and the rest of it dropped.

### Connector commands

```bash
pastor connector install owner/repo/connectors/slack     # owner/repo[/subdir], --ref, --yes
pastor connector link ~/src/my-connector                 # use a working copy in place
pastor connector list [--json]                           # version, mode, hooks, missing secrets
pastor connector describe slack [--json]                 # one connector in full, and the jobs that use it
pastor connector try slack --job support --since 1h      # run its command once, print its items
pastor connector try prs watch                           # run its [watch] command once, print its lines
pastor connector uninstall slack                         # or unlink, for a linked one
```

`install` clones the repository from GitHub with `git` (`PASTOR_CONNECTOR_GIT_BASE`
points it at a mirror), checks out `--ref` if given (a ref that starts with `-`
is refused), validates the manifest and shows what the connector will run (its
connector and hook commands, shell-quoted and with control characters escaped,
which hooks hear about every job's tasks, and its secrets) before asking to
continue. `--yes` skips the question, and it is required when stdin is not a
terminal. `link` puts a symlink to a directory of yours in the connectors
directory, for developing one; it shows the same description, and warns when
the directory or its manifest is group- or world-writable or owned by another
user, since whoever can change it changes what runs next. Both print the secrets
still unset in the `.env`. `uninstall` removes a checkout and `unlink` a
link (the directory itself stays); the `.env` and the state directory are
kept, and jobs that use the connector are invalid until it is back.

`list` shows one row per connector: id, version, connector mode, number of hooks,
whether it is installed or linked, and `ok`, the secrets still missing, or why
it is invalid.

`describe` shows one connector in full: its manifest (name, description,
version, `min_pastor_version`, authors, homepage, repository, license); how it
got here; its connector command's mode, argv and timeout and each hook's
events, `only_own` and argv; its `[finish]` and `[watch]` commands and
timeouts; its config keys, required ones marked; its
secrets, each `set` or `missing` in the `.env` (names only, never values);
the jobs whose `[connector] use` names it, with their last run and result as
`job list` has them (from the head when one runs); and its status, `ok`, the
missing secrets, or why it does not load. A connector that does not load
still shows its directory, origin and jobs. `install` writes where a
connector came from to `.<id>.install.json` beside the checkout: the source
(`owner/repo/subdir`), the clone URL, the `--ref` asked for (none means the
default branch), the commit checked out and when; `uninstall` removes it. A
connector installed before pastor wrote that file shows its origin as
unknown, except the commit and remote of a checkout that is a whole
repository, read from its `.git`. A linked connector shows the directory it
points to, marked `(missing)` when that is gone. `list` keeps to its columns
so it still fits 80 columns; the detail is here.

`try` runs the connector once for a job and dispatches nothing. It uses the
`[connector]` table of `~/.config/pastor/jobs/<job>.toml` if that file exists,
and an empty config only when there is no such file, so you can try a connector
before writing the job. A job file that exists is never ignored: one that
names a different connector, or that is invalid, is an error. The cursor is `null`, and `since` is `--since` before now, by
default the job's `backfill`, or zero. Items are printed to stdout as JSON
lines; logs and the summary go to stderr, and the run log is written as for any
run. Nothing is saved: no cursor, no tasks. A stream connector is collected for
its `timeout` and then stopped. `try <id> watch` runs the connector's `[watch]`
command instead, as `pastor watch` does, and prints its lines; a failed run is
`connector_failed`.

`install`, `link`, `uninstall` and `unlink` tell a running daemon to reload,
so a connector is usable without a restart (this also restarts stream
connectors). A daemon that is running but does not answer gets a warning to
run `pastor job reload` yourself.

### Using a connector in a job

A job uses a connector by its id (`[connector] use = "slack"`, with
the connector's own keys beside it);
`pastor serve` and `pastor tick` check the job's connector table against the
keys the manifest marks `required`, and `job list` shows a job whose connector is
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
`repo` or `branch` may not be empty or `.`, or contain `/`, `\`, `..`, a
leading `-` or control characters; such an item is skipped and reported.

### What a connector inherits

A connector is code you run as the head's user, not a sandboxed extension. Each
command inherits:

- the whole environment of the `pastor serve` or `pastor` process that runs
  it, not just its `.env`: `SSH_AUTH_SOCK`, API tokens and cloud credentials
  exported there reach every connector. Only the secrets a manifest declares are
  redacted from logs, so a connector that prints its environment writes the rest
  to its run log in clear text;
- the head user's files, including every other connector's `.env`, so keeping
  secrets in separate `.env` files organises them but does not isolate them;
- `PASTOR_STATE_DIR`, and with it `pastor.sock` and the ssh ControlMaster
  sockets, which is control of the fleet (see [Trust model](#trust-model)).

A hook without `only_own = true` also hears about every other job's tasks,
though without their item or prompt (see [Event hooks](#event-hooks)). Start `pastor serve` from an
environment that holds only what its connectors and ssh need, and install only
connectors you would run by hand.

### Event hooks

A connector's `[[events]]` hooks run on the head for every event whose type is
in `on` (`task.queued`, `task.done`, `task.blocked`, `task.failed`,
`job.failed`, `machine.lost`, ... as in `pastor events`). The hook gets the
event record, the same JSON `pastor events --json` prints, on stdin, and the
same environment as the connector (`PASTOR_JOB` is the task's job, and unset
for an event about no job, such as `machine.lost`). With `only_own = true` it
only hears about tasks and jobs that use this connector, found by
reading `connector.use` in the job's file; one-off `pastor task run` tasks
belong to no connector, and an event about no job passes.

A hook that hears about a task whose job another connector owns (or a one-off
task, which no connector owns) gets the task with `item` set to `null` and
`prompt` empty: those hold the text of the other connector's items, which a
notifier has no need to see. Its `PASTOR_CONNECTOR_STATE_DIR` is then its own
`@<id>` scratch dir, not the job's, which belongs to the job's connector;
`PASTOR_JOB` still names the job.

```toml
[[events]]
on = ["task.done", "task.blocked"]
only_own = true
command = ["sh", "report.sh"]
timeout = "60s"                     # the default
```

Hooks of different connectors run at the same time; one connector's hooks run one
after another, in event order, from a queue that holds 256 events; when a
connector's hooks fall that far behind, the oldest waiting events are dropped
and logged. A hook that fails or times out is logged and not retried. Its output goes to `~/.local/state/pastor/runs/@<id>/`, redacted
like connector logs.

### The finish command

A connector only brings work in; `[finish]` lets it act when the work is done,
to close the loop where the item came from (comment on the issue, say). When a
task of a job that uses the connector reaches `done` or `failed`, the head runs
`command` once, with the connector's environment (the job's scratch dir as
`PASTOR_CONNECTOR_STATE_DIR`, `PASTOR_JOB`, the `.env` and its secrets), as an
event hook runs. It is queued with the connector's hooks, so one connector
never races itself, and its output goes to the same `runs/@<id>/` log,
redacted.

Stdin is one JSON object on one line:

```json
{
  "task": { "id": 7, "job": "support", "item": {"key": "k1", "title": "..."}, "state": "done", "...": "the whole task row" },
  "state": "done",
  "job": "support",
  "branch": "fix/issue-12",
  "last_output": "...the last lines of the agent's pane..."
}
```

- `task` is the task row, as in `pastor task describe --json`, with its `item`
  and prompt.
- `state` is `done` or `failed`, the state the task ended in.
- `branch` is the branch of the task's worktree, or the job's `branch` when
  it names one; `null` when the task has neither.
- `last_output` is up to the last 40 lines pastor read from the agent's pane
  when it judged the task done (pastor reads the pane then to see whether the
  agent ended on a question). It is `""` when pastor read none, as for a task
  that failed, and after a restart of the head.

It runs once per task, the first time pastor sees it `done` or `failed`. A task
that goes on to `closed`, or that is reopened and ends again, does not run it
again. The list of tasks already run is in memory, so it is lost when the head
restarts. A failure never changes the task: an exit status other than 0, a
timeout, or a command that could not start is logged and sent as a
`connector.finish_failed` event (`detail.connector` and `detail.reason`), and
not retried.

### The watch command

`[watch]` feeds [`pastor watch`](#watch) what the head cannot know, like the
state of the pull requests the agents opened, without pastor itself calling
`gh`. The table is `[watch]` rather than `[events]` because `[[events]]` is
the hooks' array, and TOML cannot hold both under one name.

Each interval, every watcher that lists the connector runs `command` in the
connector's directory with its environment and no job: the `.env` and its
secrets, `PASTOR_CONNECTOR_ID`, and `~/.local/state/pastor/connectors/@<id>/`
as `PASTOR_CONNECTOR_STATE_DIR`, as its hooks get. Stdin is empty. Each line it prints on stdout is
one line for the watcher, trimmed, with empty ones dropped; print the state as
it is now (`PR 31 reviewed, 2 open threads`) every time, and the watcher prints
a line only the first time it sees it. A run that exits other than 0, times
out or cannot start prints nothing of its output: the watcher says `CONNECTOR
<id> failing: <why>` once, and `CONNECTOR <id> ok` when a run works again.
Stderr goes to the `runs/@<id>/` log, redacted.

## The bridge

`pastor bridge` lets a CLI on another machine reach this machine's head over
ssh (`ssh <head> pastor bridge`), so the head never opens a network port. It
reads request lines on stdin, passes each to `pastor.sock` unchanged, and
writes each reply line to stdout until stdin closes. It never starts a head
or reads `flock.toml`; with no head running it writes one `no_head` error
line and exits non-zero. It checks nothing itself: the head refuses what it
would refuse from a local CLI, so ssh access to the head's user is access to
the fleet.

## A head on another machine

The CLI can use a head that runs on another machine. It reaches it the same
way pastor reaches a machine: ssh, never a network port.

```bash
pastor head set user@pi-1                                # checks the head, then saves it
pastor head set user@pi-1 --pastor '~/.local/bin/pastor' # pastor is not on its PATH over ssh
pastor head set user@pi-1 --force                        # save it even if it does not answer or is too old
pastor head show [--json]                                # head: user@pi-1 (remote), or head: this machine
pastor head unset                                        # back to this machine's head
```

The setting lives in `~/.config/pastor/client.toml`:

```toml
[head]
ssh = "user@pi-1"               # an ssh destination, as ssh takes it
pastor = "~/.local/bin/pastor"  # optional: pastor's path on the head
```

`PASTOR_HEAD=<dest>` names a head for one shell over the file, and
`pastor --head <dest> ...` for one command over both. `head` never edits
`flock.toml`.

With a head set, each request goes through

```
ssh -o BatchMode=yes -o ControlMaster=auto -o ControlPersist=60s \
    -o ControlPath=<state dir>/ssh-%C <dest> <pastor> bridge
```

one request line in, one reply line back (see [The bridge](#the-bridge)).
It needs:

- key-based ssh from here to the head: `BatchMode` never asks for a password
  or a host key, so either one missing fails at once;
- pastor on the head, on the PATH a non-interactive shell there gets, or
  named with `--pastor` (the remote shell expands a `~` in it);
- `pastor serve` running there.

`head set` sends one ping first and saves nothing if it fails:
`head_unreachable` when ssh fails (the message carries ssh's stderr),
`no_head` when nothing runs there, `head_too_old` when that pastor has no
`bridge` or speaks an older protocol. Any command fails the same way later;
it never falls back to this machine's files.

These commands go to a remote head: `task run|list|describe|read|retry|priority|close|prune|send|done`,
`machine list`, `tick`, `job reload`, `events`, and job commands for
the head's jobs (see [Jobs on a shepherd](#jobs-on-a-shepherd)). `machine list`'s first line
names the head by its ssh destination and shows its herdr as `-`. `task run`
fills what its flags leave out from the built-in defaults, not from the
head's `[defaults]` (the head still resolves the agent with its own).

Local on purpose, as with no head set: `completions`, `setup`, `head`,
`bridge`, `connector` (connectors are this machine's) and `task attach`
(it goes to the machine directly, reading this machine's `flock.toml`).

Every other command would read or edit this machine's files instead of the
head's, so it fails with `remote_head_unsupported` until it is moved behind
the head; run it on the head. `machine open` is one of them.

### A headless serve

With a head set, `pastor serve` runs headless, as this machine's shepherd:
it runs the jobs in this machine's `jobs/` and this machine's connector
hooks, and nothing else. It has no queue, no machine actors and no task
store, and never reads `flock.toml`.

- The new items a job run finds (not seen here, within `max_tasks_per_run`)
  go to the head together, in one `job_submit` request with the job's
  `[dispatch]` table as written and its unrendered prompt. The head applies
  its own `[defaults]` to what the table leaves out, renders each item with
  the id it gives the task, checks its paths and the flock, and queues and
  dispatches the tasks under the job's name. The keys it queued, and those
  it had seen already (a run whose reply was lost), are marked seen here.
- A head that does not answer fails the run with `head_unreachable`, and a
  head with a job file of that name with `job_name_taken`: either counts as
  a failed run, backing the job off as a failing connector does, and no item
  is kept, so the next run asks the connector for them again. An item the
  head refuses for its paths is reported and skipped; one past its cap waits
  for the next run; any other refusal fails the run and holds the job's
  cursor, as a failed insert does.
- Every tick it reads the head's events past its cursor (`EventsSince`) and
  hands them to this machine's hooks in order, saving the cursor after each.
  The first time it reaches the head it skips the head's history and starts
  from there. A hook with `only_own` hears only tasks of a job in this
  machine's `jobs/` that uses its connector (and records about no job, as on
  the head); a hook without it hears every head event in its `on`, with the
  item and prompt of other jobs' tasks left out. A head job with the same
  name as a local job counts as local, since ownership is read from the job
  file here.
- Events the head rotated out of its log before this machine read them are
  lost to the hooks: it logs a `head_events_gap` warning and goes on from
  the oldest record the head still has.
- Its database is `shepherd.db` in the state dir: the jobs' state and seen
  keys, and the event cursor. `pastor.db` is left alone.
- On `pastor.sock` it answers `ping` (with `role: "shepherd"`), `tick` and
  `job list|run|reload` for its own jobs; anything else is
  `shepherd_unsupported`. The CLI's `job` commands ask it for this machine's
  jobs (next section); `tick` and `job reload` still go to the head.
- A head that does not answer is a warning in its log, `shepherd_needs_head`,
  not a reason to stop; it asks again each tick and says when the head
  answers again.
- It refuses to start while a head holds this machine's socket
  (`head_running`), and a head, or a second headless serve, refuses to start
  while it holds it (`shepherd_running`). A CLI with no head set that finds
  a shepherd on the socket, and `head set` pointed at a machine running one,
  fail with `shepherd_running` too; `--force` does not save a shepherd as
  the head.

`pastor setup systemd` (or `launchd`) installs it the same way: the unit
runs `pastor serve`, which reads the head from `client.toml`. The head
needs IPC protocol 7 or later for `job_submit`.

### Jobs on a shepherd

With a head set, `pastor job list` shows two tables: the head's jobs under
`head: <dest>`, then this machine's under `shepherd: <host> (this machine)`.
The columns are those of a head's `job list`, and a side with no jobs
prints its header and `no jobs`:

```text
head: user@head-1
NAME     SCHEDULE  ENABLED  FLOCK    CONNECTOR  LAST RUN  NEXT    RESULT
issues   every 1h  yes      default  github     3m ago    in 57m  ok: 1 items, 1 tasks

shepherd: laptop (this machine)
no jobs
```

`--json` stays one flat array, the head's jobs first, each with `where`:
`head` or `shepherd`.

This machine's jobs come from its headless serve. With none running,
`job list` reads the job files here and the last state in `shepherd.db`,
and says so on stderr. A serve that listens but does not answer stops the
command with `shepherd_unresponsive`.

`job run|enable|disable|describe|edit <name>` go to wherever the job lives.
A job whose file is in this machine's `jobs/` is this machine's; any other
name is the head's. For this machine's jobs:

- `run` asks this machine's serve, so it must be running.
- `enable`, `disable` and `edit` change the file here. A running serve
  re-reads it at once; otherwise it applies when the serve starts.
- `describe` reads the file and the state here, and asks the head for the
  job's recent tasks.

The head must answer for all of these, since `job list` shows its side too.

### Agents on other machines

An agent on another machine reaches the head the same way, but with a key
that can do no more than an agent should: ping, list the tasks of its
machine's flock, and `describe`, read or end (`task done`) the tasks placed on
its machine. `pastor bridge --agent --machine <name>` is that locked bridge.
It reads each request and passes on only those; anything else, a task on
another machine included, it answers `not_allowed_for_agent` without asking
the head. It learns a task's machine and a machine's flock from the head,
never from local files, cuts a task list down to the flock, and names the
task a request comes from itself, whatever the caller said.

To set it up for machine `pi-1`:

1. On `pi-1`, as the user its agents run as, make a key with no passphrase
   (`ssh-keygen -t ed25519 -f ~/.ssh/pastor_head -N ""`) and copy
   `~/.ssh/pastor_head.pub` to the head.
2. On the head, print the line for that key:
   `pastor machine authorized-key pi-1 --key pastor_head.pub` (or `--key -`
   to read it on stdin). It refuses a machine that is not in `flock.toml`,
   and edits no file.
3. Append the line to the head user's `~/.ssh/authorized_keys`. It runs
   `<this pastor> bridge --agent --machine pi-1` whatever command the client
   asks for, with no terminal and no forwarding:

   ```
   command="/path/to/pastor bridge --agent --machine pi-1",no-pty,no-user-rc,no-port-forwarding,no-agent-forwarding,no-X11-forwarding ssh-ed25519 AAAA... user@pi-1
   ```

Give each machine its own key: the machine named in the line is the only one
whose tasks that key reaches. A work flock's machine then cannot even list a
personal flock's tasks.

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
- Any process running as the head's user can drive the fleet through
  `pastor.sock`, an agent on the head included. pastor sets `PASTOR_TASK=t-N`
  in the pane of every agent it starts, and refuses a command from such a pane
  that changes the fleet: `task run`, `send`, `attach` (herdr's agent terminal
  types into any task's pane), `retry`, `priority`, `close` and `prune`, `tick`
  (`--dry-run` too), `job run` and `job reload`, `connector install`, `link`,
  `uninstall` and `unlink`, edits of machines, flocks, jobs and `pastor.toml`
  (`config edit`), `serve` and `setup` (a head started from the pane would
  dispatch with nothing to refuse), and `machine open` (herdr's full UI drives every
  pane) (`agent_refused`). A dry tick and a reload count because both apply
  `pastor.toml` and `flock.toml` first. `task done` is refused too, save for
  the pane's own task: an agent may end its own task, and nobody else's. Reads still work, `describe` included.
  `agents_change_fleet = true` in `pastor.toml` turns this off. It stops an
  agent acting on its own, not a determined one: it runs as the same user and
  can unset the variable.
- A task's role widens that guard for one task. `pastor task run --role
  orchestrator` starts an orchestrator: from its pane it may also run tasks
  (`task run`), retry and send to them (`task retry`, `task send`) and
  disable a job (`job disable`); everything else that changes the fleet is
  still `agent_refused`, `task close` and `job enable` included, and the
  message names the role. Only a person starts one: `--role orchestrator`
  from any task's pane is `role_refused`, an orchestrator's included and
  whatever `agents_change_fleet` says, and so is a retry of an orchestrator
  from a task's pane, since the copy keeps the role. The head keeps the role
  (`role` in `task describe` and `task list --json`, `agent` for every other
  task), so these go through `pastor serve`: with no head, `job disable` from
  a task's pane is refused. Like the rest of the guard, a role is a guard
  against an agent's mistakes, not a boundary: an orchestrator runs as the
  same user as pastor and can do anything that user can. `--role
  orchestrator` needs a head of IPC protocol 10 (`head_too_old`).

The head's user is the fleet's trust boundary. Anything that runs as that
user on the head controls every machine in the flock file, because it can:

- talk to `pastor.sock`, which takes any request the CLI can make: queue a
  task with any prompt, agent arguments, repo and machine, type into a live
  task with `task send`, run a job, or edit the flock. The socket is 0600, so
  it keeps other users out, not other processes of the same user;
- write `flock.toml`, where a `command = [...]` machine is any argv, and the
  job files and the plugins directory;
- use the ssh ControlMaster sockets under `~/.local/state/pastor/ssh/`, which
  reach every ssh machine without authenticating again.

That includes an agent on a `local = true` machine: it runs on the head as
the head's user, so an agent that a repo or an item's text talks into it can
dispatch unattended agents across the fleet. Do not give a `local = true`
machine untrusted work, such as a job fed by issues or chat messages from
people outside your team. To use the head for that work, run herdr there as
a separate user and add it as an ssh machine (`ssh = "agents@localhost"`),
so its agents reach the head only as that user. What the pastor skill asks
of a dispatched agent, to stay in its own pane and worktree, is advice, not
a control.

Connectors run as the head's user too, with the same reach and the head's
environment; [What a connector inherits](#what-a-connector-inherits) lists
what they get.

## Describe and edit

In the terminal pastor reads like kubectl: `list` shows many things, `describe`
one in full, `edit` its file.

Taking things away has three verbs, each with one meaning. `remove` takes one
thing you name (`machine remove`, `flock remove`, `trust remove`). `uninstall`
and `unlink` undo `connector install` and `connector link`. `prune` removes
many at once, chosen by state and age (`task prune`).

```text
pastor job describe <name>       schedule, connector and its config, dispatch, last runs and errors, next run, recent tasks and job events
pastor machine describe <name>   host, flock, session, model, profile, channel, herdr, protocol and pastor versions, agents, orphans, tags, its tasks, recent errors
pastor flock describe <name>     default or not, its agent, agent args, allow and deny, model, profile, machines, live agents, queued and running tasks
pastor connector describe <id>   manifest, origin, commands, config, secrets set or missing, jobs using it, status
pastor task describe <id>        state, machine, agent and where it came from, prompt, error
pastor job edit <name>           ~/.config/pastor/jobs/<name>.toml
pastor flock edit                ~/.config/pastor/flock.toml
pastor config edit               ~/.config/pastor/pastor.toml
```

Every `describe` takes `--json`. A description reads from the head when one
runs and from the files and the store when not; a machine is then probed
directly, as `machine list` does. `machine describe` and `flock describe` are
built by the head itself, from the flock it last applied rather than
`flock.toml` as it reads now, so a machine the head has not picked up yet is
`unknown_machine`. These and the `trust` commands need a head from this
release or later (`head_too_old` otherwise). The job's `connector` and `dispatch` are the
tables as written in its file. Recent events come from `events.jsonl`: a
job's `job.*` events, and for a machine its `machine.*` events that carried
an error and its `task.failed` ones, ten at most.

`edit` opens a copy of the file in `$VISUAL`, else `$EDITOR`, else `vi`, run
through `sh` so the variable can carry arguments (`code --wait`). A file that
does not exist yet (`flock.toml`, `pastor.toml`) starts empty; a job must
exist. When the editor exits the copy is checked the way the head loads the
file: a job against `pastor.toml`'s `[defaults]` and the connectors installed
here, `flock.toml` and `pastor.toml` by their own rules. A valid edit replaces
the file atomically (a new hidden temp file beside it, then a rename, keeping
its mode) and a running head reloads it at once. A symlink is edited at its
target and stays a symlink; the directory it points into keeps its mode.

An invalid edit prints the error and asks `reopen the editor to fix it?
[Y/n]`. Yes reopens the copy with the error on top as `# pastor:` comments,
which are taken off again. No, a closed stdin, or saving the reopened copy
unchanged gives up: the file is left as it was, and the error
(`invalid_edit`) names where the edit is kept. A copy saved as it was means
no changes and writes nothing; an editor that exits non-zero writes nothing
(`editor_failed`); a file that changed on disk while the editor was open is
not overwritten (`edit_conflict`). That check and the rename run together
under an advisory lock on `.<file>.lock` beside the file, left in place, so
two pastor edits of one file never interleave.

With a head running, `edit`, `job describe` and `job enable|disable` act on
the head's files, not the caller's copies. The editor still runs here: the CLI
fetches the head's file and a hash of it (`FileGet`), edits a temp copy, and
sends the result back with that hash (`FilePut`). The head checks it with the
same code as an edit with no head, against its own `[defaults]` and
connectors, and refuses a stale hash (`edit_conflict`); an invalid edit comes
back with the head's error and reopens the editor as above. An unchanged file
sends nothing. A head from before these requests is refused
(`head_too_old`): restart `pastor serve` after an upgrade.

## Files

```
~/.config/pastor/pastor.toml      tick, settle, reconcile_every, request_timeout, agent_ready_timeout, close_done_after, agents_change_fleet, head_address, defaults, agents, models, profiles, watch (all optional)
~/.config/pastor/flock.toml       flocks and machines
~/.config/pastor/jobs/<name>.toml one job per file
~/.config/pastor/client.toml      this CLI's `[head]`, from `pastor head set`
~/.local/state/pastor/pastor.db   tasks (schema 10, with retry_of, flock, trust_sent, activity_seen, ended, priority, priority_from, queue_pos and role), seen keys, job state, trusted repos, the last event seq
~/.local/state/pastor/shepherd.db  a headless serve's job state, seen keys and head event cursor
~/.local/state/pastor/pastor.sock daemon socket
~/.local/state/pastor/events.jsonl events log (and events.jsonl.1, the previous one)
~/.local/state/pastor/watch/<name>.json   a `pastor watch` cursor
~/.local/state/pastor/ssh/        one ssh ControlMaster socket per machine and host, and one (`head-<hash>`) for a remote head
~/.config/systemd/user/{pastor,herdr}.service   written by `pastor setup systemd`
~/Library/LaunchAgents/pastor.{serve,herdr}.plist   written by `pastor setup launchd` (macOS)
~/.config/pastor/connectors/<id>/.env   a connector's secrets and settings
~/.local/share/pastor/connectors/<id>/  installed connectors (a symlink for a linked one)
~/.local/share/pastor/connectors/.<id>.install.json   where `connector install` got it from
~/.local/state/pastor/connectors/<job>/ a job's connector scratch
~/.local/state/pastor/runs/<job>/       captured connector output, capped and pruned
~/.local/state/pastor/runs/@<id>/       captured hook output (and job-less `connector` runs)
```

`PASTOR_CONFIG_DIR`, `PASTOR_STATE_DIR` and `PASTOR_DATA_DIR` override the
locations. `PASTOR_CONNECTOR_GIT_BASE` (default `https://github.com`) is where
`connector install` clones `owner/repo` from.

```toml
# pastor.toml, every key optional; these are the defaults
tick = "10s"                 # scheduler pass
settle = "10s"               # a finished agent stays idle this long before its task is done
reconcile_every = "60s"
request_timeout = "60s"      # one herdr request, connect included
agent_ready_timeout = "30s"  # agent.start to an accepted prompt; below request_timeout
close_done_after = "15m"     # a done task's pane closes after this; "never" keeps it
agents_change_fleet = false  # true lets agents pastor started run tasks and edit the fleet
# head_address = "user@head.example"  # unset by default; see below
[defaults]                   # for run flags, job keys and flock keys that are left out
agent = "claude"
agent_args = []              # e.g. ["--model", "claude-opus-5-5"]
allow = []                   # tool patterns the agent may use unasked, e.g. ["Bash(git:*)"]
deny = []                    # tool patterns it must never use; wins over allow
max_tasks_per_run = 5
timeout = "2h"
place = "repo"               # where a task's pane goes: repo, own, pastor or pane:<workspace>
# model = "sonnet"           # a [models] name for tasks that name none; unset: no model
# priority = "normal"        # the level of tasks that set none: low, normal, high or critical
# agents = { opencode = "opencode" }  # the agent for a model of another kind than agent's
# profile = "develop"        # a permission profile for tasks that name none; unset: none
[agents.claude]              # one table per agent that needs one
kind = "claude"                  # the herdr agent it starts; default: the table's name
env = {}                         # env for its pane, e.g. { CLAUDE_CONFIG_DIR = "~/.claude-personal" }
trust_keys = ["Down", "Enter"]   # accept its folder-trust prompt; [] for none
trust_marker = "Yes, I trust this folder"  # saved trust presses them only while the pane shows this
allow_flag = "--allowedTools"    # the flag before each allow pattern
deny_flag = "--disallowedTools"  # the flag before each deny pattern
[models.sonnet]              # one table per model; none are built in
kind = "claude"                  # the herdr agent kind that runs it (required)
args = ["--model", "claude-sonnet-5"]  # put before agent_args (required, may be [])
[profiles.ci]                # one table per permission profile; review, develop, unrestricted are built in
description = "develop, plus docker"  # one line for `pastor profile list`
extends = "develop"              # its lists come first; default: none
allow = ["Bash(docker:*)"]       # added to what it extends
deny = []                        # added too; wins over any allow
[[watch.connector]]          # connectors `pastor watch` runs; none by default
name = "prs"
```

`head_address` is the ssh destination other machines reach the head by. When
it is set, every agent pastor starts on a machine that is not the head's own
(`local = true`) gets `PASTOR_HEAD=<head_address>` in its pane next to
`PASTOR_TASK`, so it knows where to send `pastor task done`. An agent on the
head's machine gets none: it uses the local socket. An `[agents]` env cannot
override it. The value must not be empty or hold whitespace.

## Shell completions

`pastor completions <shell>` prints a completion script generated from the
command definitions, so it always matches the installed binary. Ready-made
copies for bash and fish live in `contrib/completions/`.

In bash and fish the script also offers the names a command takes: job names
after `job describe`, `--job` and the like, flock and machine names after
`--flock`, `--machine` and the flock and machine commands, task ids after the
task commands, connector ids after `connector uninstall|unlink|try`, and
the `[models]` names of pastor.toml after `--model`, profile names
after `profile describe`, and the levels after `--priority` and
`task priority`. A
static script cannot know them, so at TAB it runs `pastor __complete <shell>
-- <words>`, which reads the job files, `flock.toml`, the connectors
directory and the task store directly, never the head, and prints nothing
when it cannot read them. Task ids come live ones first, newest first, and
fish shows each task's note beside it. Other shells get the static script
only. `--opt=<TAB>` works in fish; bash splits it at the `=`, which pastor
handles too.

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
A unit test checks that every command and flag named by a skill exists, so a
CLI change that breaks one fails `make check`.

`skills/spec/SKILL.md` plans work for the flock. It starts from the
superpowers brainstorm, then writes a plan in which every task has a flock or
machine, a repo, its own branch, a model with a one-line reason, a timeout
and a prompt file an agent can finish with nobody answering it. The plan, the
prompt files and a ledger live on a plan branch, `pastor/<name>`, in the repo
being worked on: each task resets to that branch, does its part, appends
`Task N: complete` to the ledger and pushes, so any machine can pick up the
next one. Tasks run one after another; a task starts only once the ledger
says the one before it is complete. Each task's Dispatch block holds the
command that starts it, `pastor task run --prompt-file ...`, so long prompts
need no shell quoting. `skills/spec/plan-format.md` is the layout and
`skills/spec/example/` a whole plan. The skill plans only; it never starts a
task.

The repository is also a Claude Code plugin named `pastor`
(`.claude-plugin/plugin.json`), which is how the skills other than the
built-in one travel: installed as a plugin, the skill is `/pastor:spec`;
linked like the one above, it is `/spec`.

## Development

The Makefile is the list of things you can run here; `make help` prints it.

```bash
make check            # fmt check, clippy with warnings as errors, full test suite
make test             # unit tests plus an end-to-end run against fake-herdr
make test-machine     # the machine actor tests five times, to catch timing flakes
make smoke SESSION=s  # opt-in test against a real herdr running session s on this host
make smoke-profiles REPO='~/src/app' CLAUDE=pi-1 OPENCODE=pi-2  # a live review task per agent through the head
make build            # debug build of both binaries; cargo run -- --help works from there
```

`make check` is what a pull request has to pass. Nothing in the suite talks to
a real herdr, so run `make smoke` on a fleet machine before trusting it there.

`make smoke-profiles` starts real agents: through the running head, a task
under the `review` profile for each agent named (`CLAUDE` and `OPENCODE` take
a machine from flock.toml, `CLAUDE_AGENT` and `OPENCODE_AGENT` another agent
of that kind), in the checkout `REPO` on that machine. Each is asked to read,
then to write a file it must be refused, and passes when it ends `done`
without ever going `blocked`. It prints the end of each pane, to paste in a
pull request, and closes the tasks. The head must run the pastor under test,
since the head is what hands the agent its profile. Run it before trusting
a change to profiles.
