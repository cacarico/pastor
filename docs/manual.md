# pastor manual

How pastor works today, in full. The [README](../README.md) is the short
version.

## What pastor does

- **Runs agents.** It queues tasks, schedules jobs, and runs coding agents (`claude`,
  `agy`, `codex`, `opencode`) in terminals on the machines of a flock, then
  watches them: when one is done, blocked on a question, stale or out of usage.
  A task is one prompt and one agent; a job turns an outside source (a
  connector) into tasks.
- **Keeps machines ready.** It sets a machine up without root, installs the
  harness CLIs at the versions a flock pins, checks the machine's
  prerequisites, signs agents in, answers folder-trust prompts, renders
  permission profiles for each harness, keeps agents' homes in line with
  bundles, and shows when any of it drifts.
- **Places files in a working folder.** It puts the skills, hooks, agent files,
  settings and instructions an agent starts with in its folder, keeps a ledger
  of what it wrote, checks that it is still there and takes it out again.
- **Reaches machines four ways.** Over ssh, locally, through a command that
  speaks to a herdr, or, for a machine the head cannot reach, by that machine
  asking the head for work (a pull machine).

Where to read next:

| To... | Read |
| --- | --- |
| run and follow tasks | [How it works](#how-it-works), [Status](#status), [Watch](#watch), [Events](#events) |
| choose an agent and a model | [Agent kinds](#agent-kinds), [Flocks](#flocks) |
| group machines, set slots, floors, profiles | [Flocks](#flocks) |
| schedule work and find it | [Connectors](#connectors), [Orchestrators](#orchestrators) |
| start the head | [Run the head](#run-the-head), [Run under systemd](#run-under-systemd), [Run under launchd (macOS)](#run-under-launchd-macos) |
| set a machine up, pin and install harnesses | [Bootstrapping a machine](#bootstrapping-a-machine), [Provisioners](#provisioners) |
| put team files in agents' homes | [Bundles](#bundles) |
| put files in an agent's working folder | [Workdir bundles](#workdir-bundles) |
| add a laptop behind NAT, or move the head | [A head on another machine](#a-head-on-another-machine) |
| know what an agent may do | [Trust model](#trust-model) |
| look something up | [Files](#files), [Describe and edit](#describe-and-edit) |
| work on pastor | [Development](#development), [Internals](#internals) |

## How it works

`pastor serve` runs on one machine, the head: the one whose serve holds the
queue. Other machines may run a headless serve (see [A headless
serve](#a-headless-serve)). The head keeps the queue and runs the jobs. For
each task it picks a machine and, for an `ssh`, `local` or `command` one (see
[Flocks](#flocks)), reaches that machine's herdr itself, opens a workspace,
starts an agent named after the task, waits for the agent to come up, gives
it the prompt (through a Claude session's inbox socket, else typed; see [How a
prompt reaches the agent](#how-a-prompt-reaches-the-agent)) and watches the
agent's status; for a `pull` machine it hands the
task to that machine's own headless serve instead, which does the same
against its own herdr and reports back (see [Pull
machines](#pull-machines)). Task state lives in SQLite under
`~/.local/state/pastor/`. How the connections work is under
[Internals](#internals).

An agent that never becomes ready within 30s fails the task; one that exits
on start fails it straight away, usually because the agent is not installed
on that machine, unless the end of its pane shows a usage limit: then the
task waits for the reset (see [Usage limits](#usage-limits)). An agent that is blocked on its own startup question marks
the task `blocked`; herdr drops the prompt then, so pastor sends it once
someone answers and the agent leaves `blocked`.

A task is `done` when its agent goes idle after working on the prompt and
stays idle for `settle` (10s). What pastor has seen is stored with the task,
so a daemon restart or a flock or settings reload does not forget work it saw
before. How pastor tells finished work from a flicker is under
[Internals](#internals).

If the agent's last message ends in a question, the task goes `blocked`
instead, and you answer it with `pastor task send`. herdr shows an agent that
stopped to ask something as idle too, so before it calls a task `done` pastor
reads the last 100 lines of the pane: when the agent's last message (Claude's
`●` block) ends in `?`, the task goes `blocked` with `agent asked:
<question>` as its error, and stays there until the agent moves again.
Agents that draw their messages without that marker are never read as
asking.

A Claude agent that stopped on a usage limit (`You've hit your limit ·
resets 3am`) is idle as well. The same read, before the question, finds the
limit in the agent's last message after the last prompt, and the task goes
`waiting` instead of `done` (see [Usage limits](#usage-limits)). A task its
agent ended with `pastor task done` is done whatever its pane shows.

Claude can also end its turn with a command still running in the background
(`make check`), and takes the turn up again when it ends. While the pane's
footer, below the input prompt, says `1 shell still running` (or `N
shells`), or holds `1 shell` (or `N shells`) as a segment of its own between
`·`s, as Claude 2.1.3xx draws it, the task stays `running` and pastor looks
again after each settle window. The same holds for the phrase anywhere Claude draws after its last
message, such as the turn summary with a dialog under it in place of the
input box, and for the phrase wrapped across lines.

An agent that is not Claude (agy, Codex, opencode) is done only when it says
so with `pastor task done` (agy also with a last line of `DONE`, below). herdr reads agy idle through a long thinking
pause and while it waits on a command it started, exactly as at the end of
its turn, and its pane draws no `●` messages to read. So such a task that
goes idle after working without `task done` goes `blocked`, with `idle
without task done: <the pane's last lines>` as its error and `task.blocked`
carrying `{"why": "idle_without_task_done", "tail": ...}`; its pane is not
closed, it goes back to `running` when the agent works again, and it is
`done` once the agent runs `task done`. The same read takes agy's
permission prompts (`Requesting permission for:`, `Run this command?`) as a
question, `agent asked: ...`, and its footer's `· 1 task` (or `N tasks`), a
command of its own still going, keeps the task `running` as a background
shell does. A task whose `summary` is `off` was never asked for `task done`
and ends on the idle, as a Claude task does.

A Claude or agy agent can also say it is finished without running a command:
when it goes idle and the last line of its last message is `DONE` alone,
pastor ends the task as `pastor task done` would, at once and with the same
`task.done` event, and keeps the message as the round's summary (cut to
`SUMMARY_MAX`, the way a summary is). Claude's last message is the block after
the last `●` in its pane, as for a question, read on the same pane read; agy
draws no marker, so its message is the lines above its input box (or `>`
prompt), the last twenty before the word. Only a last line of exactly `DONE`
counts: `DONE.`, `All DONE`, a `DONE` followed by more, an earlier message's
`DONE`, and a message that stops on a question end nothing, and an agent that
stops to ask or to wait is not closed. A turn that drew no message of its
own (the agent was interrupted, or an API error ended it) has none either:
the `DONE` above the user's turn is an earlier round's. A tool call and what it
printed are not the agent's words, and neither is a `DONE` that is the last
line of a code block, which reads as a message's last line. A
`summary = "require"` task that ends this way has its summary, so it does not
fail as one stopped without; a `DONE` with nothing above it is no summary, and
that task fails as before. A task whose agent is another kind keeps the rules
above. `pastor task done` stays the explicit way, and a task can end only
itself (`ends_own_task`); from a task's pane it takes no `--head` flag (the
head `PASTOR_HEAD` names is the task's own) and no `--summary-file`, standard
input included: the summary is `--summary` text, so a task cannot reach
another head or read a file past the agent's deny rules.

Every Claude task is allowed `Bash(pastor task done:*)` whatever its profile,
added after the profile's own `allow` list (and not twice), so `review`, which
allows no general Bash, and a task with no profile, which would stop at a
permission prompt, can still end themselves. An agy task under a profile gets
the rule `command(pastor task done)` in the allow list pastor writes into its
settings. An agent under a profile with `own_permissions` keeps its own.

A done task's pane goes after `close_done_after`, 5s by default, at the
first reconcile past it: a task called done while its agent still waited
on a command was closed some 6s later. That gap is the grace
working as meant; the fault was the `done`, which the rule above no longer
gives such an agent.

Codex asks whether to trust a folder it has not seen, and herdr reads that
dialog idle and ready, not blocked, so a prompt sent then would go into it.
So does agy, with every dialog it starts on. Before it sends a prompt,
pastor reads the agent's pane for Codex's dialog (`Do you trust the contents
of this directory`), the agent's `trust_marker` (Claude's and agy's built
in: herdr reads Claude's dialog `blocked` only once it has noticed it, which
can be after the agent first reads ready) and the marker of each of its other
startup dialogs (Claude's built-in ones below, as its own rows leave them,
and its own `[[agents.<name>.dialogs]]` rows) not yet answered for the task;
while one shows, the task is `blocked` with its prompt held. pastor answers a
row's dialog as below, a person the rest, and the prompt goes in a settle window
after the answer. A row already answered does not hold the prompt again,
so a second dialog with the same text gets the prompt, as before.

The prompt held, or sent again when the agent did not take it, is the
one that start meant: a paused task resumed gets its resume prompt, one
back from a usage limit its limit resume, handover or model switch, never
its own prompt, which its session already ran.

An agent whose process exits while it sits idle between turns (someone typed
`/exit` after the work) leaves its task `done`; one that exits while
starting, blocked or working fails it with "agent process exited".

Before creating a workspace, pastor checks that the repo directory is
actually there: `test -d` over the same ssh master for an `ssh` machine,
directly on the filesystem for a `local` one, and skipped for a `command`
bridge, which says nothing about where it lands. herdr otherwise opens the
workspace in the shell's home with no error when the requested `cwd` is
missing, so a missing repo fails the task instead of silently working in
the wrong place.

The CLI talks to `pastor serve` over a unix socket, `pastor.sock`.
`pastor serve` refuses to start if a daemon already holds that socket, or if
something answers it but not a ping within 2 seconds. It only replaces a
socket file that refuses connections.

Every command that talks to or reloads the head pings it once, first, and
acts on that answer throughout. A head that holds the socket but does not
answer the ping within 2 seconds stops the command (`head_unresponsive`)
before it does anything: it may be busy mid-request, and working as if no
head ran would let `tick` start a second scheduler next to it, or an edit or
a prune go offline behind it.

`task list` and `task describe` read from `pastor serve` when it's running
and fall back to the SQLite store when it's not (`task list` says so on
stderr). `task attach` reads the local store directly, since it only needs
the task's machine and agent name to hand off to `ssh`/`herdr`; with a
remote head set it asks the head for the task instead.

`pastor machine list` opens with a line about the head, then lists the
machines:

```text
pastor 0.8.0 on desk (herdr 0.9.1), 2 machines, desk is the head of the flock

NAME  HOST       FLOCKS      PROFILE  CHANNEL    HERDR  PASTOR  HARNESS       AGENTS     ORPHANS  TAGS  ERROR
desk  local      personal:2  -        connected  0.9.1  0.8.0   -             0/2+1j+1b  -        -
pi-3  user@pi-3  work        develop  connected  0.9.1  0.8.0   drift claude  1/2+1j+1b  -        fast
```

The line names the head's pastor version, its hostname, the version of the
`herdr` on its PATH (`-` when there is none) and how many machines follow.
When the head is itself a machine (a `local` one) the line ends by naming it,
and its row comes first; a head that runs no agents gets the line without
that ending. With no head running, the line is replaced by the notice on
stderr that the machines were probed directly. `--flock F` lists only that
flock's machines, and the line counts those. `--wide` adds RESERVED, DISK
and MEM before DESCRIPTION (see [Room on a machine](#room-on-a-machine)).

When pastor.toml has no `head_address`, an agent on an `ssh` machine is
never told where the head is, so its `pastor task done` fails and every
task there ends without a summary. The line then ends with `no
head_address, so agents on pi-3 cannot run pastor task done`, and each such
machine's ERROR (and `machine describe`'s `error:` line) says `no
head_address: its agents cannot run pastor task done`; `no_head_address`
is `true` in `--json`. Set `head_address` (see [Run the
head](#run-the-head)) to clear it. A `local` machine uses the head's socket
and a pull machine its own serve, so neither is marked.

HOST is the ssh target, `local`, or the program a `command` machine runs.
FLOCKS are the flocks the machine is in, with its number in each (see
Flocks).

PROFILE is the machine's own permission profile, the one that decides
whether a task may ask for `unrestricted` there: the machine's `profile`,
else its primary flock's, else `[defaults]` (see [Permission
profiles](#permission-profiles)); `profile` in `--json`. On a machine in
several flocks with no `profile` of its own, the gate reads the task's flock
instead, so a task from another flock can meet a different profile than the
one shown. A task there runs under its own flock's profile before the
machine's, so it can run under another one.

PASTOR is the pastor installed on the machine: over the ssh master, pastor
runs `pastor --version` in a shell that has `~/.cargo/bin` and
`~/.local/bin` on its PATH, since ssh's non-login shell often lacks them. A
`local` machine is the head's own pastor; a `command` machine, one with no
pastor, or one that gives an odd answer shows `-`. The head asks when it
connects, then again after each successful reconcile once `version_every`
(`pastor.toml`, default `2m`) has passed since it last asked — a connected
machine's own `reconcile_every` (60s by default), a polling one's
`poll_every` (10s by default, normally every tick) — so an install shows
within about three minutes without a restart either way, and each machine
costs one `pastor --version` over ssh per interval. A probe that gets no
answer keeps the version last read. `pastor machine describe` says when the
version was last read (`version read`), so a row that still shows the old
version after an install can be told from a head that has not asked yet.
`machine list` itself never probes while the head is running.

A machine can also have a bootstrap state, which a bootstrap run reports to
the head (`BootstrapReport`) and the head keeps. While it is `pending` or
`failed`, dispatch places no task on the machine, whatever room it has: a
task waits as it does for any machine that cannot take it, and starts once a
report says `ok`, without waiting for the next tick; a pull machine claims
nothing in the meantime. A machine with no state, every machine from before
this, is not held. `pastor machine describe` shows it as a `bootstrap` line:
`ok (user mode; isolation: none)`, `pending`, or `failed at herdr: herdr is
not on the PATH; fix: pastor machine install pi-3 --kind herdr`, the step
that failed, why, and what to do. The head raises `machine.bootstrapped` (with
`isolation` and what was `missing`) or `machine.bootstrap_failed` (with `step`
and `why`). What a machine reports is cut to 400 characters and stripped of
control characters; a host key already on record is never replaced by a report,
and one that names another key is refused as `host_key_mismatch`. `pastor
bootstrap` makes the report on the machine itself (see [Bootstrapping a
machine](#bootstrapping-a-machine)); sending it to the head is not part of pastor
yet.

HARNESS is the drift of the machine's harness CLIs from the versions its flock
pins (see [Harness version pins](#harness-version-pins)): `-` when nothing is
pinned for it, or when it is a pull machine, which the head never connects to
and so cannot ask; `ok` when every pinned CLI is at its pin or not installed,
and `drift` with the kinds that are not. The head asks for each pinned kind's
version with the pastor version, on the same schedule, and `--json` carries
`harness` for the machine: each pinned kind with `pin`, `version` (`null`
when it is not installed or the machine has not answered) and `drift`. A
`command` machine cannot say, so it never drifts. With no head running
`machine list` probes without it and shows `-`; `pastor machine check` reads
the versions then.

The head also reads each machine's room: its free disk under `$HOME` and
`/tmp` (`df -Pk`) and its free memory (`MemAvailable` in `/proc/meminfo`;
unknown on macOS, which has none), one probe over the same ssh master when
it connects and again after a reconcile once a minute has passed. The
numbers sit on the machine's status with when they were read; a probe that
fails keeps the last numbers and notes the error, and never makes the
machine reconnect, since the ping just proved it reachable. Nothing acts on
them yet, but `machine list --wide` shows them: DISK is the smaller of home
and tmp free space, with which (`1.2G /tmp`), shown once with no path when
both are on one filesystem; MEM is the free memory. Either reads `?` for a
number the machine could not say, and both carry their age, such as `(5m
ago)`, once the reading is more than twice as old as the minute it is
refreshed on. `--json` has the whole reading as `room` on each machine.

With `pastor serve` running, CHANNEL is the head's live channel state and
AGENTS counts pastor's tasks and orphans (see below) against the machine's
room, `max_agents` followed by `+<n>j` for job slots and `+<n>b` for burst
when they are set (see Room on a machine).

Without it, the command probes each machine itself (a ping and an
`agent.list`, one at a time, with no time limit, plus the pastor version for
a machine that answered). A pull machine is not probed: nothing connects to
it, so its row reads HOST `pull` and CHANNEL `not probed`, and AGENTS counts
the tasks the store puts there. For any other machine CHANNEL reads one of
four values: `probed` (the ping answered; an old protocol or a failed
`agent.list` shows in ERROR), `server down` (a local endpoint's own socket
has nothing listening), `unreachable` (any other transport failure), or
`error` (the ping itself came back with a non-transport API error). AGENTS
counts every agent herdr reports, and stderr says so.

`--json` prints `{"head": {...}, "machines": [...]}`, with `pastor_version`
on the head and on each machine (`null` when unknown), `flock` on each
machine, and `channel` one of the same four probe values or `not probed` (or
the head's live channel state when `pastor serve` is running).

Jobs are one TOML file each in `~/.config/pastor/jobs/`. On every `tick` the
daemon re-reads files that changed (a file that stops parsing keeps its last
good version and shows the error in `pastor job list`), asks each due job's
connector for items, drops keys it has seen before, renders `prompt`, `repo`
and `branch` with `{{ item.* }}`, `{{ job.name }}` and `{{ task.id }}`, and
queues one task per new item up to `max_tasks_per_run`. The rest stay unseen
for the next run.

`every = "5m"` or `cron = "*/5 9-18 * * 1-5"` (local time) says when. A job
never overlaps itself; `pastor job run <name>` fires one regardless, and it
starts once a run already going has finished. The built-in connector is
`clock`, one item per run keyed by the run time; any other is an installed
connector (see Connectors), and a job that names one that is not installed is
`invalid`. A failed connector backs the job off, one minute doubling to an
hour, and keeps its cursor. `summary` under `[dispatch]` (`ask`, `require` or
`off`) says whether the job's tasks are asked for a summary, or need one (see
Asking for one).

Item text comes from outside, so an item's control characters, other than
newline and tab, are dropped before its values go into the prompt, which is
typed into the agent's terminal. An item value put into `repo` or `branch`
must be one plain path component: not empty (a missing field renders empty),
not `.` or `..`, no `/` or `\`, no leading `-` and no control characters. An
item that breaks this is skipped and shown in the job's last result; the
job's cursor still moves. An item value may not appear in the first
component of `branch`: the job fixes a prefix such as `pastor/{{ item.key
}}`, so an item cannot name an existing branch like `main` and have the
agent commit to it. A job file that breaks this is `invalid`.

A job can also run on another machine and hand its items to the head, which
keeps the `seen` keys so no item is queued twice. A headless serve (below)
does this for each run of its jobs; the request it sends is under
[Internals](#internals).

A machine whose requests answer but whose event subscription will not open is
`polling`: it still takes tasks and is reconciled every `tick`. Two dispatch
passes never run at once, and a task moves from `queued` to `starting` with a
conditional update, so a machine is never given more than its room (see Room
on a machine).

`pastor task list` shows live tasks only: queued, starting, running,
blocked, paused (see [Pausing a low task](#pausing-a-low-task)) and waiting
(see [Usage limits](#usage-limits)); a waiting task reads `waiting 03:00`,
its reset in local time, and the waiting ones come first, the soonest reset
first. Finished ones
(done, failed, stale, closed) appear with `--all`, and an empty default list
says so on stderr. `--blocked` and `--done` narrow to just that state,
`--job`, `--flock` and `--machine` narrow whichever set is shown, and
`--json` prints the same selection. `--wide` adds RESULT, how each task's
last round ended (see Task summaries), and each task's description (see
Descriptions). FLOCK is the flock the task targets. When an orchestrator is
in the list, it prints two titled tables, `orchestrators:` and then
`tasks:`; a script should read `--json`, which stays one array.

`pastor task read t-1` fetches recent output from the task's pane over the
machine channel. Text that came from an item or a pane is printed with its
control characters escaped (`\x1b`, `\r`, ...), so none of it can move the
cursor, retitle the terminal or set the clipboard: the NOTE column of `task
list` (an item's title, cut to 60 characters), every field and the prompt of
`task describe`, and `task read`. `--json` prints it raw. `--lines N` reads
the last N lines of the pane (40 by default).

`pastor machine open pi-3` execs the full herdr UI against a flock machine
(`herdr --remote` for an SSH one, `herdr` directly for a local one) instead
of showing pastor's own view; herdr refuses to start inside one of its own
panes, so run it from a plain terminal. `machine add --herdr` and `machine
remove --herdr` keep herdr's machine list in step, by running `herdr machine
add|remove` for you; without the flag `machine add` prints the command
instead. `remove` matches herdr's entry by label only; when none matches but
the same host is saved under another label, it prints that entry's remove
command rather than guessing.

On the head, `pastor machine add pi-3 user@pi-3 --agent-key ~/pi-3.pub` also
puts the machine's agent key in this host's `~/.ssh/authorized_keys`
(`PASTOR_AUTHORIZED_KEYS` points elsewhere), on a line of its own that ends in
the marker `pastor:agent:pi-3` and lets the key in only to run `pastor bridge
--agent --machine pi-3`, with no terminal and no forwarding. Lines without a
marker, the user's own keys among them, are never touched. The key is a `.pub`
file, or `-` for stdin; one that is not a single public key (it brings options
of its own, say) stops `machine add` before flock.toml changes. `--dry-run`
changes nothing and says what would be added, and the line. Run it on the
head: with `--head` pointing at another host the flag is refused
(`agent_key_needs_the_head`). For a machine that is already in flock.toml,
`pastor machine authorized-key pi-3 --key ~/pi-3.pub --write` writes the same
line, replacing the one it had, and says `already in` and leaves the file
alone when it holds that line already; without `--write` the command only
prints the line, as ever, and `--write --dry-run` shows the marked line and
the file.

A task that reaches `done` is closed by pastor after `close_done_after`
(`pastor.toml`, default `5s`; `never` disables it): the pane closes, a
worktree pastor created is removed if it is clean, and the task shows as
`closed`. Clean means no uncommitted changes and no commits that are on no
remote (`git rev-list HEAD --not --remotes` is empty; a repo with no remote
at all always has some). A worktree that is not clean is kept, with a note
on the task naming the branch, for you to save and remove with `git worktree
remove`. herdr never deletes the branch, even for a worktree it removes.

The grace period keeps the pane there for `pastor task attach` (a task
that recorded a session can be reopened after it anyway; see [Reopening a
finished task](#reopening-a-finished-task)). Auto-close checks run after each
reconcile while the machine is connected, and in between at the shorter of
`close_done_after` and `close_failed_after` (leaving out one set to `never`)
when that is shorter than `reconcile_every`. An agent that herdr shows
working or blocked again at that moment is left alone, and its task goes
back to running or blocked.

Failed and stale tasks are left for `pastor task retry`, but not their
panes: once the agent is stopped (herdr shows it idle, done or gone) and
`close_failed_after` has passed since that check first observed it stopped
(default `5s`; `never` keeps the panes), pastor closes the pane and frees
the slot. That clock starts at the first check that finds the pane's agent
stopped, not at the moment the task failed or timed out, and resets each
time the same pane is found at work again, so an agent that keeps going past
the grace is not closed the instant it finally stops. The clock is the
agent's, not the pane's: herdr hands pane ids out again, and another agent
found in the pane starts it again. Closing one pane can take a while, so
before each further close in the same check pastor lists the agents again,
and a pane whose agent went back to work or was replaced meanwhile is kept.
A failed task stays `failed` and records
no pane any more, and its worktree stays for the retry (unless the task asked
for it to go, see [Removing a task's
worktree](#removing-a-tasks-worktree)); a failed task whose pane someone else
closes lets go of it too.
A stale task whose agent stopped turns `failed` first (a `task.failed`
event); one whose agent is still at work past its timeout keeps its pane. A
pane herdr has handed to another agent since is not the task's any more: its
task is repaired at the next check (a stale one turns `failed`) and its pane
id cleared, and the other agent's pane is left alone. Blocked tasks need
their prompt answered (`pastor task send` or `pastor task attach`) or
`pastor task close`, and are never closed on their own.

### Keeping a task's pane

A task can keep its pane, so you can go on talking to its agent after it
finishes: `pastor task run --keep-pane`, `keep_pane = true` in a job's
`[dispatch]`, on a `[[flock]]` in `flock.toml`, or under `[defaults]` in
`pastor.toml`. The first of the task's own, its job's, its flock's and
`[defaults]` wins, so a job's `keep_pane = false` beats its flock's `true`;
none of them is no. It is settled when the task is queued, and a retry keeps
it. `pastor task describe` shows it with where it came from, and `--json`
carries it as `spec.keep_pane` and `spec.keep_pane_from`.

A kept task still goes `done` when its agent stops, but neither
`close_done_after` nor `close_failed_after` closes its pane: done, failed or
stale, it stays open, its row as it is, until `pastor task close`. Its
worktree stays too, since removing the checkout under an open pane would
break the follow-up. `task list` shows such a done task as `done (kept)`. It
holds no slot while it is done: its agent is idle, and the machine takes new
work. `pastor task send` makes it run again, and it holds a slot once more.
A kept failed or stale task still holds its slot, as its agent may be at
work.

```toml
[defaults]
keep_pane = true
```

### Removing a task's worktree

A failed task keeps its worktree for the retry, and on a machine whose tasks
mostly fail (a push machine, whose agents cannot run `pastor task done`)
those checkouts, with their builds, fill the disk. A task can ask for its
worktree to go whatever it ends in: `pastor task run --remove-worktree`,
`remove_worktree = true` in a job's `[dispatch]`, on a `[[flock]]` in
`flock.toml`, or under `[defaults]` in `pastor.toml`, the first of them
winning as for `keep_pane`. It is settled when the task is queued, and a
retry keeps it. `pastor task describe` shows a worktree task's `worktree:`
line, what becomes of its checkout and where that came from; `--json`
carries `spec.remove_worktree` and `spec.remove_worktree_from`. A task that
keeps its pane keeps its worktree too.

Such a task done loses its worktree with its pane after `close_done_after`,
as any done task does, except that commits on no remote do not keep it: they
stay on the branch. Failed or stale, it loses its worktree when its pane
would close, once its agent has stopped and `close_failed_after` has passed
(a stale task whose agent still works keeps both, and turns `failed` first,
as above). The worktree goes through herdr's `worktree.remove`, which takes
the pane with it, and the task stays `failed`, with no pane or workspace, for
`pastor task retry`. The guards are those of `task close --remove-worktree`:
never forced, so uncommitted or untracked files keep the checkout, and a
checkout another agent works in, or that a workspace pastor did not open
shows, stays too. A kept one gets a note after the task's own error, naming
the branch; its pane still goes. The branch always stays.

When herdr has no workspace left on the checkout (it closed with the task's
last pane, or went some other way), pastor removes the checkout with `git
worktree remove` on the machine, after the same checks. git keeps a checkout
with modified or untracked files, but not one with only ignored files, such
as a build's `target/`. This holds for `task close --remove-worktree` and for
every auto-close, whether the task asked or not.

### Sweeping kept worktrees

A worktree pastor keeps (uncommitted changes, a task that timed out, a push
that failed) stays on the machine with its build directories until someone
removes it, and `task prune` removes rows, not checkouts. `pastor task sweep
[--machine M]` lists, for every machine the head connects to (or `M`), the
worktrees pastor made for tasks that are `closed` or `failed` and that are
still on disk, one line each: the machine, the path, the tasks that
record it and its branch, and what became of it. Without `--yes` nothing is
removed (`--dry-run` says so too, and cannot be given with `--yes`); with it,
the worktrees that can go are removed, `git worktree remove` on the machine,
which takes ignored build directories like `target/` with them. The branch
stays, and so do the task rows: `pastor task retry` puts the checkout back.

A worktree is kept, with the reason on its line, when any of these holds, and
anything the machine cannot tell is treated as holding:

- a task that has not ended records it, or has it as its `--repo` (a retry
  working in it, a fix round, a paused, waiting or stale task, a done task whose
  pane is open): `t-9 is live (running)`;
- it has uncommitted or untracked files (ignored files do not count);
- it has commits on no remote;
- a workspace shows it, an agent works in it, or a workspace pastor did not
  open showed it when the task started;
- its recorded path does not look like one pastor made (not absolute, a `..`
  part, fewer than three parts), or is the repo a task works in.

A path that is already gone is not listed. A pull machine is swept on that
machine, not from the head: asking for it by name says so. The sweep needs a
running head, is refused to a task's pane, and happens only when asked: pastor
never sweeps on a timer. Worktrees pastor did not make are not in its task
rows and so are never looked at.

```toml
# fragment of flock.toml
[[flock]]
name = "push"
remove_worktree = true
```

An agent says it is finished with `pastor task done` from its own pane (the
task defaults to `PASTOR_TASK`; a human may name any task, `pastor task done
t-3`). The task goes `done` at once, however herdr reads the agent, and stays
done while the agent finishes the turn it said so in: a trailing question or
a turn still at work no longer keeps it `blocked` or `running`, and the
completion sequence check does not hold its close. Auto-close then closes its
pane once the agent is idle and `close_done_after` has passed, which frees the
machine's slot. `pastor task send` to such a task gives it more to do, and it
runs again as any done task does. `task describe --json` prints `"ended": true`.

### Errors

Runtime errors print JSON on stderr with a stable `code` and exit 1; a
malformed command line gets clap's plain usage text and exit 2.

Scripts, skills and orchestrators branch on the code, so every one pastor
uses is here (a test fails when one is missing). A code from herdr itself,
such as `pane_not_found`, is passed on as herdr gave it.

| Code | When |
| --- | --- |
| `account_exhausted` | every model the task may run is on an account that ran out, or an agent signed out, where it would run |
| `agent_key_needs_the_head` | `machine add --agent-key` run against a head on another host: it writes this host's authorized_keys, so it runs on the head |
| `agent_busy` | a task's work cannot be saved before it moves to another machine: its agent is working |
| `agent_kind_missing` | the task's flock names an agent of a kind the machine has none of |
| `agent_pane_busy` | herdr kept saying the agent's pane was busy after pastor's retries |
| `agent_refused` | a task or an orchestrator's script asks for a fleet change agents may not make |
| `agent_tools_unsupported` | the task has tool lists and its agent has no setting to pass them |
| `agy_deny_unsupported` | a profiled agy task denies a pattern agy has no rule for |
| `bundle_stale` | a task was not started because its machine's bundles are not in a state to start it: the last apply failed, or `auto` applied it just now and it failed; the message says why and how to fix it |
| `agy_permissions_missing` | a profiled agy task on a machine whose agy settings do not deny what the profile denies, or that pastor cannot read or write to make them (a `command` machine, a file that is not JSON, a symlink, a failed write); the message lists the rules to add |
| `agy_profile_mismatch` | a profiled agy task, or a workdir set of kind `agy` that names a profile, on a machine whose own profile does not deny everything the profile does |
| `already_seen` | a job's key was queued before and its task row is gone |
| `artifact_changed` | a shepherd's bridge found the cached file not the size the head said (it changed in between); ask again |
| `artifact_not_cached` | `ArtifactLocate` for an artifact the head's cache does not hold (or whose file no longer hashes to its recorded sum): it is never fetched for the asker |
| `artifact_outside_cache` | the file the head named for an artifact is outside its cache directory, or a link that leaves it: the shepherd's bridge reads nothing there |
| `artifact_checksum_mismatch` | a download for the artifact cache does not match its published SHA-256, or the published sums do not list it; nothing is cached |
| `authorized_keys_busy` | the head's authorized_keys kept changing under an edit of it (another program writing to it), eight times in a row; nothing was written |
| `bootstrap_failed` | a `pastor bootstrap` step failed for a reason other than the two below; the message names the step, why and the fix |
| `bootstrap_missing` | `pastor bootstrap` stopped at what the machine lacks (a command, lingering, a user bus, herdr); `--json` lists it in the step's `missing`, and the fix names what the machine's owner does |
| `bootstrap_unsupported_platform` | `pastor bootstrap` on an OS, init or architecture pastor and herdr do not ship for: a Linux without systemd, or a 32-bit userland |
| `join_failed` | a join spent its token and added the machine to flock.toml, then could not save its enrollment or write its key lines; the message says to make a `--replace` token and join again |
| `join_token_expired` | a join with a token past its `--ttl` |
| `join_token_invalid` | a join with a token id the head does not know (revoked, swept, or never made), `join revoke` of one, or anything but one join request through a token's key |
| `join_token_used` | a second join with a token that already let a machine join |
| `machine_name_taken` | a join under a name flock.toml has, with a token not made with `--replace` for it, or a `--replace` token used under another name |
| `authorized_keys_linked` | the head's authorized_keys file is a symlink, which pastor does not write through; edit its target, or point `PASTOR_AUTHORIZED_KEYS` at it |
| `branch_gone` | reopening a closed task: its worktree and its branch are both gone |
| `bundle_fetch_failed` | `bundle add --git` could not fetch the ref |
| `bundle_in_use` | `bundle remove` of a bundle a flock or machine in flock.toml, or a job, names |
| `bundle_invalid` | `bundle add` or `bundle show` of a bundle whose manifest, files or directory are wrong (an unknown key, a missing `src`, a symlink), `bundle add` of a version flock.toml does not give the vars of, or `bundle adopt` of a machine whose plan cannot be rendered |
| `bundle_secret` | `bundle add` of a bundle, or `workdir place` of a set, with a file or value that looks like a secret; the message names each file and line |
| `bundle_unreachable` | `bundle adopt` of a pull machine or a `command` machine, which bundles do not reach |
| `check_unknown` | `agent signed-in` or `login` could not run the agent's login check on the machine (a `command` machine, or no answer) |
| `close_failed` | `task close` could not close one or more of the tasks |
| `config_error` | a config file (pastor.toml, flock.toml, a job, the head file) does not read |
| `connector_failed` | a connector command exited non-zero or printed what pastor cannot read |
| `connector_not_found` | no installed or linked connector has that id |
| `daemon_not_running` | nothing listens on the daemon socket; start `pastor serve` |
| `empty_message` | `inbox send` was given no text on stdin |
| `edit_conflict` | the file changed while an `edit` had it open; it is left as it is now |
| `editor_failed` | `$EDITOR` would not run or exited non-zero; nothing changed |
| `events_read_failed` | the head could not read its events log |
| `fleet_locked` | another pastor held flock.toml too long (a head starting or another edit) |
| `flock_exists` | `flock add` of a flock that exists |
| `flock_has_tasks` | `flock remove` of a flock with open tasks |
| `flock_is_default` | `flock remove` of the default flock |
| `flock_mismatch` | the task names a machine outside its flock |
| `flock_not_empty` | `flock remove` of a flock that still has machines |
| `head_running` | `pastor serve` while a head already runs here |
| `head_started` | a head started while a head-less edit waited; run the command again |
| `head_too_old` | the head predates something the request needs; upgrade it |
| `head_unknown` | a command in a task pane found no `PASTOR_HEAD` and no serve on its machine; on a machine other than the head, set `head_address` in the head's pastor.toml |
| `head_unreachable` | ssh to the remote head failed |
| `host_key_mismatch` | a bootstrap report names a host key other than the one on record for the machine |
| `head_unresponsive` | something holds the socket but does not answer a ping |
| `herdr_error` | herdr could not be run, or answered a request with an error |
| `internal` | the daemon sent a reply of a kind the CLI did not expect |
| `invalid_args` | the arguments of an authorized_keys `command=` hold a character the user's shell would read as syntax |
| `invalid_artifact` | an artifact name, version or target that cannot be a path part, or a download URL from herdr's index that is not https |
| `invalid_dispatch` | a headless job submission has an invalid dispatch configuration |
| `invalid_edit` | the edited file does not check; `edit` asks to reopen it, and the edit is kept either way |
| `invalid_file` | a request for a file pastor does not edit (only flock, config or `job:<name>`) |
| `invalid_key` | `bridge`, or an authorized_keys line, was given something other than one public key line |
| `invalid_load` | a task's load is not a load name: a letter or digit, then letters, digits, `-` and `_` |
| `invalid_machine` | `machine add`, `bridge` or an authorized_keys marker: a machine name that does not start with a letter or digit and hold only letters, digits, `.`, `_` and `-` |
| `invalid_name` | a name (such as `watch --name`) with characters pastor refuses |
| `invalid_path` | `bridge`, or an authorized_keys line: a pastor path that cannot go in authorized_keys unquoted |
| `invalid_report` | a pull machine reported a state it may not set |
| `invalid_request` | a line that is not a request, on the daemon socket or the bridge; a headless job submission with an invalid job name or an item with no string key |
| `item_rejected` | a job item has no usable key or prompt |
| `job_name_taken` | a headless serve's job has the name of one of the head's |
| `job_not_found` | no job by that name |
| `job_task_refused` | the head would not queue a headless serve's job task |
| `load_not_taken` | no machine of the task's flock takes its load, or the machine it is pinned to does not (see [Loads](#loads)) |
| `login_not_pending` | `login code` or `login cancel` of an agent with no login pending on that machine |
| `machine_check_unsupported` | a machine check of a machine pastor runs no shell on: a `command` bridge or a pull machine |
| `machine_busy` | `machine add --bootstrap` or `machine bootstrap` of a machine a push is already setting up |
| `machine_exists` | `machine add` of a machine that exists |
| `machine_cordoned` | `task run --now` on a machine that is cordoned: `--now` runs past a room floor and its limits, not a cordon |
| `machine_not_connected` | `task run --now` on a machine that is not connected |
| `machine_revoked` | a shepherd's bridge whose `--gen` is not the machine's enrollment on the head, or whose machine has none or is not in flock.toml |
| `machine_shutting_down` | the machine's actor is stopping; try again |
| `machine_unreachable` | `agent signed-in` or `login` could not reach the machine over ssh |
| `model_kind_mismatch` | the task's model is for another kind of agent |
| `no_agent` | `task attach` to a task that has no agent yet |
| `no_head` | the bridge found no `pastor serve` on its machine; `login` with no head running; or `machine add --bootstrap` and `machine bootstrap`, which run on a head, found none |
| `no_head_address` | `machine add --bootstrap` or `machine bootstrap` while pastor.toml has no `head_address`: the machine's pastor could not reach the head |
| `no_login_check` | `agent signed-in` or `login` of an agent with no `login_check`, built in, in its manifest or set |
| `no_login_start` | `login` of an agent whose kind's harness manifest has no `[login] start` |
| `no_machine` | the task is not on any machine |
| `no_picker` | `task send --pick` to a task whose pane shows no picker at its bottom |
| `no_session` | reopening a closed task with no session to resume |
| `no_such_option` | `task send --pick` with text no option of the picker contains; the message lists them |
| `no_terminal` | `task attach` to a task on a command machine |
| `no_trust_keys` | `task send --trust` and the agent has no `trust_keys` |
| `no_worktree` | `task close --remove-worktree` of a task with no worktree |
| `not_allowed_for_agent` | an agent's bridge will not pass the request on |
| `not_allowed_for_shepherd` | a shepherd's bridge will not pass the request on |
| `not_an_orchestrator` | an orchestrator note from a task no orchestrator file started |
| `not_at_trust_prompt` | `task send --trust` to a task not blocked on its startup prompt |
| `not_exhausted` | `limit clear` of an account, or an account's model, with no limit kept |
| `not_in_flock` | the machine is not in that flock |
| `not_on_machine` | a pull machine reported a task that is not on it |
| `not_prunable` | `task prune` of a state other than done, failed or closed |
| `not_pull_machine` | a pull machine request from a machine that is not one |
| `not_queued` | `task priority` or `queue move` of a task that is not waiting in the queue |
| `not_retryable` | `task retry` of a task in a state that cannot be retried |
| `not_running` | `serve stop` or `serve status` with no serve running here; `status` with no head and no `--probe` |
| `not_trusted` | `trust remove` of a repo that is not trusted on that machine |
| `nothing_pinned` | `machine install` of a machine none of whose agents' harnesses is pinned and with no herdr pin, or whose `--agent` or `--kind` names none that is, or `--kind herdr` with no `[artifacts] herdr` |
| `nothing_to_send` | `task send` with no text, `--key`, `--trust` or `--pick` |
| `now_needs_machine` | `task run --now` with no `--machine` |
| `now_not_started` | `task run --now` could not start the task at once, or its pull machine did not claim it in time; it is closed, not left queued |
| `now_refused` | `task run --now` from an agent or an orchestrator's script; a guard against mistakes, not a boundary (see [Starting a task now](#starting-a-task-now)) |
| `opencode_permissions_conflict` | a profiled opencode task on a machine whose managed opencode config (or managed preferences) has permission rules or a file pastor could not read, naming each file and key, or whose global config or `~/.opencode` has rules under an agent or a mode, a file pastor could not parse, or more config than pastor reads |
| `orchestrator_held` | a session orchestrator's state could not be read or kept |
| `orchestrator_invalid` | the orchestrator's file has never been valid |
| `orchestrator_kind` | `run` of a session orchestrator, or `start`/`stop` of a scheduled one |
| `orchestrator_not_found` | no orchestrator by that name |
| `orchestrator_not_running` | `orchestrator stop` of one that is not running |
| `preempt_needs_critical` | `--preempt` on a task below critical |
| `profile_args_conflict` | the profile meets agent args that pick a permission mode of their own |
| `profile_cycle` | permission profiles that extend each other in a loop |
| `profile_not_allowed` | `unrestricted` asked for on a machine whose own profile is not |
| `no_profile` | `profile render` with no name, and no machine profile to take |
| `render_kind_unsupported` | `profile render --kind opencode` or `codex`: only claude and agy are rendered so far |
| `render_failed` | the agy settings of a profile could not be built |
| `prompt_file_empty` | `--prompt-file` names an empty file |
| `prompt_file_unreadable` | `--prompt-file` names a file pastor cannot read |
| `pull_machine` | `agent signed-in` or `login` on a pull machine, which the head never connects to |
| `pull_machine_task` | read, send or attach to a pull machine's task from the head |
| `push_failed` | `task done --current` could not push the task's branch |
| `read_failed` | `task read` could not read the agent's pane |
| `remote_head_unsupported` | a command that runs on the head only, with a remote head set |
| `reopen_failed` | reopening a closed task could not start its agent again |
| `request_too_large` | a request over the daemon's size limit |
| `role_refused` | a task or a script asks for what only a person may, such as `--role orchestrator` |
| `runtime_error` | any other failure; the message says what |
| `save_blocked` | a task's work cannot be saved before it moves to another machine: its agent was waiting on a question, or stopped to ask one instead of finishing |
| `save_timeout` | told to commit and push its work before a move, the agent did not finish in time; the task and its pane are left as they were |
| `scheduler_error` | the scheduler could not run, list or reload jobs |
| `serve_failed` | `pastor serve` in the background exited before answering |
| `serve_slow` | `pastor serve` started one that has not answered yet |
| `service_managed` | `serve stop` of a serve that systemd or launchd would restart |
| `shepherd_not_running` | a job command for this machine's headless serve, which is not running |
| `shepherd_running` | `pastor serve` as a head while a headless serve runs here |
| `shepherd_unexpected` | this machine's socket belongs to a head, not a headless serve |
| `shepherd_unresponsive` | this machine's headless serve does not answer |
| `shepherd_unsupported` | a request a headless serve does not answer; ask the head |
| `signed_out` | `agent signed-in`: the login check ran and failed; the agent is still signed out |
| `socket_denied` | the connect to the socket was denied for permissions, most often a sandbox around the command |
| `stop_failed` | `serve stop` could not tell which process holds the socket |
| `stop_timeout` | the serve still runs after SIGTERM and the wait |
| `store_error` | the task database failed or kept changing under the request |
| `summary_empty` | `task done --summary` or `--summary-file` is blank or whitespace-only |
| `summary_file_unreadable` | `task done --summary-file` could not read the file |
| `summary_required` | `task done` without a summary on a `summary = "require"` task |
| `summary_unknown_outcome` | `task done --current --summary` whose first line names no outcome |
| `task_not_found` | no task with that id |
| `task_not_live` | the task has no live agent to send to or act on |
| `task_paused` | `task attach` to a paused task |
| `task_waiting` | `task retry` or `task attach` of a task waiting for a usage limit to reset |
| `timeout` | the daemon did not answer in time; the request may still land |
| `trust_not_selected` | `task send --trust` read the dialog after its keys but one short of `Enter`, and it is not on the agent's `trust_marker` |
| `unknown_agent` | `bundle adopt --agent` of an agent no task on the machine gets |
| `unknown_bundle` | no bundle by that id in the bundles dir |
| `update_failed` | `pastor fleet update` stopped at a machine it could not look at or update; its row says why, and the machines after it were not changed |
| `unknown_flock` | no flock by that name |
| `unknown_machine` | no machine by that name |
| `unknown_model` | no model by that name in `[models]` |
| `unknown_priority` | a priority level that is not low, normal, high or critical |
| `unknown_profile` | no permission profile by that name |
| `unknown_provisioner` | `machine check --provisioner` names no provisioner the catalog has, or one whose copy does not load |
| `unsupported_platform` | pastor or herdr ships no build for the machine's OS and architecture |
| `usage_error` | arguments clap accepts but pastor does not, such as a bad task id |
| `workdir_failed` | `workdir place` or `remove` through the head that got no answer from the machine: it could not be reached, pastor is not on its PATH, or the run was cut short; or a task whose workdir set could not be placed before its agent started, which then does not start, or whose sets the head could not build when a pull machine claimed it |
| `workdir_conflict` | `workdir place` of a set that wants a file or a settings key another set in the directory holds; the message names both sets |
| `workdir_kind_mismatch` | a task whose workdir set is for another kind of agent than the task's |
| `workdir_invalid` | `workdir place` of a set that is wrong (its id, kind, a path outside the directory or in `.git`, a harness's own file, a mode, settings that are not an object, a block marker in its instructions, over 4 MiB, a `from` outside the set), or for a directory that is missing, a symlink or the home |
| `workdir_not_found` | `workdir check` of a directory, or a set, the head has placed nothing in, or whose kept set is gone |
| `workdir_mcp_unsupported` | `workdir place` of a set that names MCP servers: a working directory has no file of its own for them; a home bundle carries them |
| `workdir_profile_mismatch` | a task whose workdir set names another profile than the task runs: name the task's profile in the set, or none |
| `workdir_permissions_owned` | `workdir place` of a set whose settings name `permissions.allow`, `deny`, `ask` or `defaultMode`: what an agent may run comes from a profile |
| `workdir_unsupported` | `workdir place` of a set with a kind other than `claude` or `agy`, or `workdir place` or `remove` through the head onto a pull machine or one reached through a command bridge; or a task with a workdir set pinned to a `command` machine or in a flock with no push or pull machine, or a job submitted from another machine that names one |
| `worktree_dirty` | `task done --current` found uncommitted changes in the task's worktree |
| `worktree_needs_repo` | a worktree task with no repo to branch from |

### After an upgrade

After an upgrade, restart `pastor serve`. A CLI newer than its head refuses
what that head can't do, with `head_too_old`.

Every request to a head, from the CLI or from a headless serve, is checked
against the head's protocol before it goes out. A request that carries
something the head predates (a field it would drop without a word, or a
request it does not know) is refused `head_too_old`, naming what the head
lacks, and is not sent. So a head from before a feature never queues a task
without its flock, model, profile, allow and deny lists, level, `preempt`,
label, description or summary setting, and never gets a request it cannot
read, such as `queue`, `events` from another machine or an `orchestrator`
command. The check uses one ping per head, kept for a minute, so a command
pings once and a headless serve sees a restarted head within a minute. A
request every head takes sends no ping for it.

A headless serve's job run that needs what its head lacks fails the same way,
and backs off as a failing connector does. `pastor head set` refuses such a
head unless given `--force`. Which protocol each feature needs is under
[Internals](#internals).

### Task summaries

An agent says how its work ended as it finishes:

```bash
pastor task done --summary "done: pushed pastor/t-4, PR #31"
pastor task done --summary-file notes.md      # or - for stdin; not from a task's pane
```

The first line names the outcome: `done`, `partial`, `blocked` or `nothing
to do`, in any case, alone or followed by something that is not a letter, digit or underscore
(`Partial - tests left`). A first line that names none is stored as
`unknown`. A summary keeps its first 2,000 characters; a blank one is
refused (`summary_empty`), and an unreadable file too
(`summary_file_unreadable`). The summary goes through `pastor bridge` like
the rest of `task done`, so an agent on a remote machine can send one.

`--current` does the whole end of a task in that one command, for the task
the pane runs (from `PASTOR_TASK`; no task argument, and `usage_error`
outside one): if the task has a worktree with a branch, it pushes it (`git
push origin HEAD:<branch>`, a no-op once it is pushed) before anything
else, refusing without sending or ending anything when the worktree has
uncommitted changes (`worktree_dirty`) or the push fails (`push_failed`);
then it sends the summary, refusing one whose first line names no outcome
(`summary_unknown_outcome`, unlike a bare `--summary`, which only requires
one not blank); then it ends the task and prints `DONE` as its last line,
so the agent does not push, send a summary, print `DONE` and go idle as
four separate steps.

Each round of a task (from its prompt, or from the `task send` that
reopened it, to `done` or `failed`) keeps one summary, numbered from 1. A
round that ends with none, because pastor found the task done or failed on
its own or the agent ran `task done` bare, stores `no summary` with the
last lines pastor read from the pane (up to 2,000 characters, empty when it
read none), marked `source: pane`. On an `ssh` machine of a head with no
`head_address` such a round stores `no head` instead: its agent's `task
done` could not reach the head, so the missing summary says nothing about
the work (a task with `summary = "off"` keeps `no summary`). `task done
--summary` on a task pastor already found done replaces that round's
summary rather than starting another.

Where it shows:

- `pastor task describe t-4` prints a `summary:` line (outcome, round, who
  wrote it, when) and the text below it; `--all-summaries` prints every
  round's.
- `pastor task list --wide` has a RESULT column, the outcome, `-` for a task
  with none.
- `--json` of `task list` and `task describe` carries `summary`
  (`round`, `outcome`, `text`, `source`, `at`), absent when there is none;
  `task describe --all-summaries --json` adds `summaries`, every round's.
- The `task.done` and `task.failed` event records carry `summary` (see
  Events), `pastor watch` prints `outcome=` on their TASK lines, and a
  connector's `[finish]` command gets `summary` on stdin.

A task shows its last round's summary only while it is done, failed or
closed; a running one has none yet. Tasks from before 0.8.0 show no
summary.

#### Asking for one

pastor asks for the summary itself. Every prompt it sends ends, after a
blank line, with:

> When you finish, run `pastor task done --current --summary '<summary>'`,
> the summary a short text in single quotes (a single quote inside it is
> written `'\''`): first line `done`, `partial`, `blocked` or
> `nothing to do`; then up to five short lines: what changed, where
> (branch, PR, files or notes), what is left. It pushes your branch if you
> have one, saves the summary and prints DONE, so there is nothing left to
> do by hand.

The line is added as the prompt is sent (at dispatch, or once an agent
blocked at launch is past its question), not stored in the task's prompt:
`task list`'s NOTE, the description and `task describe`'s prompt stay the
task's own words. A `task send` whose text reopens a done task adds it
again, after a space, since that is new work and a new round; a reply to a
running or blocked task adds nothing, and neither does resuming a paused
session.

The `summary` setting decides what pastor does:

| value | the prompt | a round without a summary |
|---|---|---|
| `ask` (default) | the line above | ends as usual, keeping the pane's last lines |
| `require` | the line, then "pastor fails this task if you stop without one." | see below |
| `off` | nothing added | ends as usual; the agent may still send one |

It is set in `[defaults]` in pastor.toml, on a `[[flock]]` in flock.toml,
under a job's `[dispatch]`, or per task with `pastor task run --summary
ask|require|off`. The most specific wins: `task run` or the job, then the
flock, then `[defaults]`, else `ask`. It is settled when the task is queued
and stored with it (`summary` in its spec, left out of `--json` when it is
`ask`); a retry keeps it. `task describe` shows it, as `summary: ask (line
added to the prompt)`. A config file without `summary` loads as before and
asks.

With `require`, a summary is a condition of success; any outcome counts,
`blocked` included:

- The agent's own `pastor task done` (from its pane, where `PASTOR_TASK`
  names the task, or through the bridge) without `--summary` or
  `--summary-file` is refused with `summary_required`, and the task stays
  as it was, so the agent can run it again with one. A bare `task done`
  after one that carried a summary keeps it.
- An agent that pastor finds idle and finished without having sent one
  ends the task `failed` with error `stopped without a summary`, and the
  round keeps `no summary` and the pane's last lines; its pane closes after
  `close_failed_after`, as any failed task's does. When its worktree holds
  uncommitted or untracked files (or pastor cannot tell), the idle may have
  been read too early, the agent cut off before it committed: the task goes
  `blocked` instead, `stopped without a summary, with uncommitted files in
  its worktree`, with `task.blocked` carrying `{"why":
  "uncommitted_without_summary"}`. Its pane and session stay, it runs again
  when the agent does, and the agent's `task done` with a summary ends it.
- A person's `pastor task done t-N`, from outside the task's pane, is not
  refused: the round reads `no summary (ended by hand)`.

No reminder is sent first.

`pastor task send t-3 "yes, go on"` sends the text to a live task (starting,
running, blocked, or done with its pane still open): a Claude session takes it
as a message on its inbox socket, and any other agent gets it typed into its
pane, followed by Enter. See [How a prompt reaches the
agent](#how-a-prompt-reaches-the-agent).
`--no-enter` leaves Enter out, and each `--key K` presses one named
key after the text, in order (`--key esc`, `--key Down --key Enter`; herdr's
key names). `--pick OPTION` answers the picker on screen by the text of one
of its options, case ignored: the head reads the pane, presses the arrows
to the first option containing the text, then Enter, and records the keys
and the text on `task.input`. It answers `no_picker` when the bottom of the
pane shows none, and `no_such_option`, naming the options, when none
matches; like `--key`, it takes any task with a live pane and saves
nothing. It goes through the head to the task's machine; anything else
answers `task_not_live`. For a Claude task, text with its Enter goes to the
session's inbox socket instead of the pane (see [The inbox](#the-inbox)), and
is typed when the socket does not take it, when herdr shows the agent blocked
or still starting, or with `delivery = "type"`; `--no-enter`, `--key`,
`--pick` and `--trust` are always keystrokes. Each send is a `task.input` event
recording the key names and the length of the text, never the text, which may
be a secret, and `socket: true` when it went through the socket.
`--json` prints the head's answer as `{"message": "..."}`.

A done task that is sent input goes back to running (`task.running`), so an
agent marked done with its work unfinished can be told to finish in the same
pane, context and all. Its next turn marks it done again; the idle it was
done at does not.

An agent started in a folder it has not seen stops at its folder-trust
prompt, and a worktree is always a new folder. `pastor task send t-3
--trust` presses the agent's trust keys (`trust_keys` under
`[agents.<name>]` in `pastor.toml`; Claude's are built in as `Down`,
`Enter`, agy's as `Enter`) and saves the task's machine and repo, the `--repo` as given, so
every worktree of that repo counts. When the agent also has a `trust_marker`
(Claude's and agy's are built in), all but the last key go first, then the pane is read
and the last key goes only once the marker is the dialog's *selected*
option, not merely shown: a key lost on the way to the pane (`Down` not
reaching it) would otherwise leave `Enter` to choose whatever is selected
instead, such as "No, exit", while pastor still believed the folder
trusted. The selected option is the cursor's line, with the lines a
narrow pane wraps it onto in a numbered menu; in a menu without numbers
each line is an option, so a label wrapped there does not match. A
dialog not on the marker after the other keys answers
`trust_not_selected`, and nothing is sent or saved; an agent with a single
trust key, or no marker, is answered in one press as before, except agy's
built-in one: its dialog opens on "Yes, I trust this folder" and `Enter`
takes whatever is selected, so the pane is read before that key too. It answers
only a task blocked on its startup prompt; any other task answers
`not_at_trust_prompt`, and nothing is sent or saved. An agent without trust
keys answers `no_trust_keys`, and a task without `--repo` gets the keys but
nothing is saved.

From then on, a task blocked while it starts has its pane read when it
blocks, after each answer's settle window, and at each reconcile, and the
first of its agent's startup dialogs that shows in the prompt at the bottom
of the pane, and was not answered for that task, is answered: the head presses the
dialog's keys, once per task and dialog, and emits `task.answered` with the
dialog's name and the keys. Inside an answer's settle window the pane is
not read again, whoever answered: pastor, or a person with `pastor task
send` (`--pick`, `--key`, `--trust` or text) or at the pane itself. The trust dialog is the first row, known by its
`trust_marker` (Claude's and agy's are built in as their "Yes, I trust
this folder" option); it is pressed only for a saved repo, and emits `task.trusted` as
before. Claude's other dialogs are built in, in its 2.1 wording, each
answered with the option that keeps things as they are: the fullscreen
renderer offer (`Not now`), the auto-mode default nudge (`No, keep ...`),
the auto-mode setup offer (`Don't show again`), the default-effort switch
(`No, keep ...`) and the autocompact offer (`Keep ...`, "We recommend
compacting earlier"). Its bypass-permissions warning is not: accepting it is
yours to do, with `pastor task send t-N --pick "Yes, I accept"`. One dialog
is answered per settle window, so a chain of them on a new machine takes a
few windows. A task blocked on a dialog no row matches, or still blocked
after its rows, waits for a person. The marker must be in the prompt at the
bottom of the pane (after the last output, and after any menu above the
last one, and after the key hints of any dialog above the last thing on
screen), since the read includes scrollback where an earlier dialog can
linger above a later one. The trust row with no marker (`trust_marker =
""`, or an agent of a kind with none built in that sets none) is pressed without the
check. `[[agents.<name>.dialogs]]` in pastor.toml adds a dialog or changes
a built-in (see [Agent definitions](#agent-definitions)).

The autocompact offer also comes up where a blocked startup is not: under
a finished turn, in place of Claude's input box. herdr reads it idle (its
hints are `... esc not now`), so an idle agent would be closed as done
with no summary while Claude waits for an answer. The head answers it (the
same row, once per task) and
looks at the pane again after the settle window, closing the task only
when the offer is gone. A task whose agent stops under a dialog that is
still up and that pastor will not press (it was answered already, or the
option it picks is not there) is `blocked`, not closed, so a person can
answer it with `pastor task send t-N --pick`.

The fullscreen offer rarely shows at all: every Claude task's pane gets
`CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1`, which keeps Claude on its classic
renderer, the one with the scrollback pastor reads, and turns the offer
off. `env` in `[agents.<name>]` overrides it.

On a pull machine the head never reads the pane, so the machine's own serve
does all of this, with the same rules: while one of its tasks has a repo and
is queued, or blocked during startup with no keys sent yet, it asks the head
for the trust saved for that machine (only that machine's pairs, never
another's), keeps a copy in `shepherd.db`, and its actor answers the
dialogs of the pull machine's own `pastor.toml` as the head would, once per
task and dialog. Its next report carries the trust keys and the head emits
`task.trusted` with them; its `task.answered` events are only logged there. `task send --trust` cannot reach a pull machine's
task (`pull_machine_task`), so its repos are saved with `pastor trust add
<machine> <repo>` on the head; a pair added while a task is blocked is
picked up at the serve's next tick. A head older than IPC protocol 31 is not
asked, and the pull machine's tasks wait at the prompt for a person.

For a Claude or agy task in a folder pastor trusts on the machine, the dialog
does not have to show at all: just before the agent starts, pastor writes the
trust into the harness's own store there, and the agent finds its folder
trusted. For Claude that is `projects.<folder>.hasTrustDialogAccepted = true`
in the agent home's `.claude.json` (`$CLAUDE_CONFIG_DIR/.claude.json` for an
agent that sets it, else `~/.claude.json`); for agy, the folder in
`trustedWorkspaces` of `~/.gemini/antigravity-cli/settings.json`. The folder
is the one the agent starts in: the repo, a task's worktree of a trusted repo,
or the folder a task with no repo starts in; one nobody trusted is never
written. Running Claude sessions save `.claude.json` at any time, and a
session can write back a copy older than pastor's entry, so the write is a
compare and swap. The file's new content goes to the machine on the command's
standard input, never on its command line (where any local user could read a
store that holds keys and tokens, and which cannot hold a file this big); the
old content is compared as a sha256, and the new content's byte count and
sha256 go in the command so the machine can check what arrived: a transfer cut
short (the ssh client died) is removed and the file left as it was. The temp
file is written and given the file's mode first, then the file is compared and renamed over, so only the
rename is left between them. For Claude both run holding `.claude.json.lock`,
the lock directory Claude itself saves under; while a session holds it, pastor
waits up to two seconds and then reads again. The file is replaced only if it
still holds what pastor read (a session that saved meanwhile makes pastor read
again and add to what is there), it is read once more half a second later to
see the entry survived, and a write a session undid is made again, three times
at most. Every other key keeps its value and place. A store that is not there
(a home that has never run), a symlink, and a file that is not the JSON it
should be are left as they are. None of it can fail a task: what pastor could
not write is answered when the dialog shows, as above.

Either way the task's prompt goes in once, `settle` after the trust keys:
Claude redraws for a moment after the dialog and loses what is typed then,
though herdr takes it. A prompt answered by a person at the pane waits the
same: the head sees the agent leave `blocked` and sends the prompt `settle`
later.

`pastor trust list [--json]` shows the saved pairs, `pastor trust add
<machine> <repo>` saves one without a blocked task, and `pastor trust remove
<machine> <repo>` forgets one. The folder of a task with no repo is saved in
the same place, as the absolute path it started in (`/home/me/pastor-tasks`). With `pastor serve` running they go through
it, so the table is the head's; with it down they read and write the store
directly. `add` and `remove` change the fleet: an agent pastor started is
refused them.

A prompt the agent does not take anyway is sent again. An agent the head
gave its prompt (at start or resume, or after its startup prompt) that sits
idle a whole settle window at the sequence the prompt went in at, never seen
working or blocked, did not take it; the head sends it again, up to twice,
then marks the task `blocked` with the error "agent did not take its
prompt", for a person to look at the pane and `task send` it. This is kept
in the head's memory: after a restart such a task stays `running` until it
goes stale.

Codex takes a prompt typed into it as a bracketed paste and reads the Enter
that follows as a newline, so the prompt can sit in its input box, whole or as a
`[Pasted Content N chars]` placeholder, and the agent stays idle. For a Codex
agent in that state the head presses `Enter` instead of sending the prompt
again (and emits `task.input` with `"submit": true`); each press counts as one
of the two tries above.

`blocked` before any work is not proof the prompt was taken: herdr can read
an agent ready a moment before it reads the startup dialog on its screen,
and a prompt sent then goes into the dialog. So the first `blocked` after a
prompt reads the pane. A startup dialog there (one that holds a prompt, as
above) means the prompt went into it: the prompt is pending again and the
task `blocked`, the dialog is answered as any startup dialog is, and the
prompt goes in after it, counted as a resend. An agent that was only seen
blocked and then sits idle is read again: if its pane shows neither the
start of the prompt, a `[Pasted text` placeholder nor a message of its own,
the dialog took the prompt and its Enter answered it, so the prompt is sent
again as above instead of the task ending.

Everything else pastor closes only when asked. Three commands do it, retry,
close and prune, all with `--json`.

`pastor task retry t-4` queues a copy of a failed or stale task (job, item,
prompt and dispatch settings, with `retry_of` pointing back and a "retry of
t-4" note in `task list`) and dispatches it at once. It gets a new id because
the old agent `t-4`, named after the task, may still be running. A failed
worktree task goes back to its own checkout, on its branch, when that
checkout is still there and free: pastor knows the checkout is that task's
own (its dispatch made it and recorded where), it is still on disk at the
same path, the old agent is gone, no agent is in the checkout's workspace
(an earlier retry of the same task may have reopened it) and no other task's
agent works in the checkout from elsewhere (found by its path, as for
removing a worktree, below). A failed task whose worktree auto-close removed
because it asked (`remove_worktree`) has its checkout put back on its branch,
at the same path (`git worktree add`), when that branch is still there and
checked out nowhere else, so the retry goes on from its commits. Any other
retry, a stale task's included (its agent may still be working), gets a
branch of its own (`pastor/t-<new id>`, even when the task named one) and a
new worktree.

`pastor task close t-4` closes the task's pane (and the agent in it) and
marks it `closed`; a queued task only has its row closed.
`--remove-worktree` removes the task's worktree instead, which closes its
workspace, pane included. herdr refuses a checkout with uncommitted or
untracked files; the task is then left as it was, so commit or clean up and
run it again. When herdr has no workspace left on the checkout, `git worktree
remove` removes it on the machine instead, after the same checks; a checkout
git keeps is noted on the closed task. `pastor task close t-4 t-5 t-6` closes each in turn,
`--remove-worktree` applying to all: it prints one line per task (`t-4
closed`, or `t-5 no_worktree: ...` for one that failed; with `--json` an
array of objects), goes on past a failure, and then exits 1 with
`close_failed` naming the ones not closed. One task prints as it always has.

### What a closed task leaves running

Closing a pane closes its shell, not a build the agent started under `flock`,
`nohup` or `setsid`, or one its tool gave a process group of its own: those
outlive the pane and hold their disk. So what a task left running is dealt with
in two steps around the close:

1. Before the pane closes, whether by `task close`, auto-close, the pane of a
   failed task closing, an orphan's, or a pause, pastor asks herdr for the
   pane's shell (`pane.process_info`) and lists, on that machine, what runs
   under it: every descendant of the shell; every process in a process group
   one of them is in (this finds a member whose parent already exited),
   unless a process that must stay is in that group too; and, when the
   checkout goes with the task (`--remove-worktree`, or auto-close removing
   it), every process whose working directory is in it, on Linux, since a
   daemonised build has left the tree and the group but not the directory. A
   checkout that stays, or one another agent works in, is not searched.
   Nothing is signalled yet: the close can still be refused (a dirty
   checkout), and a refused close leaves the task, its agent and its builds as
   they were. In a shared workspace (`place = "pastor"` or `pane:<label>`) the
   pane closes before the checkout is removed, so a refused removal has already
   closed the agent: what it left under its pane is ended all the same, and
   what works in the checkout is not.
2. Once the close went through, whatever of that list still runs, as the same
   process (its start time is the same), gets `TERM`, five seconds to end,
   then `KILL`. The agent and its tools went with the pane and are not among
   them. The shell and its ancestors (herdr) are never listed.

Neither step can stop the close: a pane herdr cannot describe (an older herdr
has no `pane.process_info`), a machine that cannot be reached or one with no
shell command (a `command` machine) leaves the close as it was. A pane that is
already gone has no shell to start from, so only a checkout that goes with a
close that removes it is searched. What works in a checkout that is kept after
all (a dirty one with auto-close, one a workspace pastor did not open shows, or
a removal that was refused) is never ended; only a checkout that went has it
ended with it.

What was ended goes in `task.processes_killed` on the task: `pane`, `count`
(the ones ended) and `processes`, each with `pid`, `name`, `found` (`tree`,
`group` or `cwd`), `forced` (it did not end on `TERM`) and `ended`, false for
a process that cannot be signalled (another user's), which is still running.
None is sent when nothing was left running.

`pastor task prune --done --older-than 3d` deletes done tasks that finished
more than three days ago; `--failed` and `--closed` add those states. A
pruned task's item stays seen, so a job never queues it again, and the
newest task is always kept so its id is never handed out twice. Prune keeps
a row whose worktree may still be on disk, and says which: a plain close only
closes the pane, and the row is then the only record of that checkout. `task
close --remove-worktree` on it removes the worktree (through herdr, or with
`git worktree remove` on the machine when herdr has already lost the
workspace) and clears the recorded workspace, and the next prune takes the
row.

Prune works without `pastor serve`, but not behind one that holds the socket
and does not answer (`head_unresponsive`): that head may still be writing
tasks. Retry and close need it.

### Tokens a task used

When a round of a Claude task ends (done, failed, or `task done`), pastor
reads the task's session from the machine it ran on and keeps the totals:
the model of the session's last reply, the number of API calls, and the
input, cache write, cache read and output tokens. The files are
`<config>/projects/*/<session>.jsonl` and the subagents' files under
`<config>/projects/*/<session>/subagents/`, `<config>` being the agent's
`CLAUDE_CONFIG_DIR` from `[agents.<name>] env` (`~` is the machine's home),
or `~/.claude`. `awk` on the machine adds them up, one call per message id,
and only the totals come back, over ssh for an ssh machine. The session
holds every round, so each read replaces the one before.

- `pastor task describe t-4` prints `used: claude-opus-4-5, 42 API calls`
  and `tokens: input 1,234, cache write 56,000, cache read 3,400,500,
  output 12`. The model is the one the session ran on, which the `model`
  line leaves as `-` for a task that took its agent's default.
- `--json` of `task list` and `task describe` carries `usage` (`model`,
  `api_calls`, `input`, `cache_write`, `cache_read`, `output`, `read_at`),
  absent until pastor has read one.

A task shows nothing when it has no session id (another kind of agent, or
agent args that pick the session), on a `command` machine or a pull machine
(the head cannot read their files), and when the read failed; the head's log
says why. The read runs in the background, so `usage` can lag the `done` by
the time the machine takes to answer.

### Where a task's pane goes

`place` decides where on its machine's herdr a task's agent gets its pane.
It is set like every dispatch setting: `--place` on `task run` (and on `task
retry`, to move a retry), `place = "..."` in a job's `[dispatch]`, `place` under
`[defaults]` in `pastor.toml`, or under the task's `[[flock]]` entry, which
comes before `[defaults]`; `task describe` prints it and where it came from,
such as `place: own (from flock work)`.

- `repo` (the default) keeps an agent under the repo it works on. A
  `--worktree` task gets a new worktree, which herdr shows under the repo's
  workspace. A task whose `--repo` a workspace already shows, such as a fix
  round started in a pull request's worktree, gets a new pane in that
  workspace. herdr reports a workspace's directory only when it is a git
  checkout, and pastor compares it with the expanded `--repo`, trailing `/`
  ignored, without resolving symlinks. With no such workspace, or no repo, the
  task gets a workspace of its own (see [Workspace
  labels](#workspace-labels) for its name; with no repo, in
  `~/pastor-tasks` on the machine).
- `own`: always a workspace of its own (for a worktree, the one herdr opens
  on the new checkout), whatever already shows the repo.
- `pastor`: a pane in the machine's one workspace labelled `pastor`, made on
  first use in the home directory. The first workspace with that label is
  the one used, so a checkout of a repo named `pastor` that herdr labelled
  after its folder is taken too.
- `pane:<workspace>`: a pane in the workspace with that label. A machine with
  no such workspace fails the task before anything is made.

A `pastor` or `pane:<workspace>` workspace that closes while pastor places the
task is looked up once more; still gone, the task fails (`pastor` is made
again instead), and it never lands in a workspace of its own.

A task closes only its own pane, never a workspace it joined. A pane in a
workspace the task did not make is still the task's own: the task records
that pane and that workspace, and `task close` and auto-close close only the
pane, never the workspace or its other panes (herdr still closes a workspace
whose last pane closes, so one left with only the task's pane goes with it).

A worktree task placed in `pastor` or `pane:<workspace>` still gets its
worktree on disk and works in it; the workspace herdr opened on the checkout
is closed once the agent's pane is split off the shared one. A retry that
finds a workspace already showing its checkout (the failed task's own, or
one someone opened) never closes anything in it: the retry's pane is split
off it and the workspace keeps every pane it had.

pastor never removes a checkout that a workspace it didn't open still shows.
When one does, removing the checkout would close that workspace, so on
`--remove-worktree` or auto-close the checkout stays and the task closes with
a note to remove it with `git worktree remove`. Its workspace is kept on the
row, so `--remove-worktree` can try again once that workspace is gone.
Whatever the place, a task whose own dispatch found a workspace already
showing its checkout (a retry placed `repo` or `own` that joined the failed
task's workspace, or one someone opened) records that, and its checkout is
kept the same way: only the task's pane goes. When herdr refuses to remove a
checkout (uncommitted changes), the checkout stays too.

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

### Workspace labels

The workspace pastor makes for a task, its own or its worktree's, is
labelled from a template, so tasks of many flocks on one machine can be told
apart in herdr's sidebar. The default is `{{ flock }}/{{ task.id }}`, such as
`personal/t-85`. `label` is set like the other dispatch settings: `--label`
on `task run`, `label` in a job's `[dispatch]`, `label` on a `[[flock]]` in
`flock.toml`, and `label` under `[defaults]` in `pastor.toml`; the first of
the task's own, its job's, its flock's and `[defaults]` wins. It is settled
when the task is queued, and a retry keeps it.

```toml
[defaults]
label = "{{ machine }}/{{ task.id }}"
```

A template knows five placeholders, in the `{{ }}` syntax of job prompts:
`{{ task.id }}` (`t-85`), `{{ flock }}`, `{{ machine }}`, `{{ job }}` (empty
for a `task run` task) and `{{ item.key }}` (empty with no item). Any other,
an empty template or a control character is refused where it is written: the
job, `flock.toml` or `pastor.toml` does not load, and `--label` is a usage
error. Dispatch renders it on the machine it picked and drops leading and
trailing spaces and slashes, so `{{ job }}/{{ task.id }}` reads `t-85` for a
task with no job. herdr takes any label, but one that renders empty or with a
control character (an item's key can hold one) names the workspace `t-N`
instead, with a warning in the head's log and a note on the task.

Only the workspace is named so. The herdr agent stays `t-N`, since pastor
finds its agents by that name, and so do the default branch `pastor/t-N` and
`PASTOR_TASK`. `pane:<workspace>` and the shared `pastor` workspace are
untouched, and a task that joins a workspace (`place = "repo"` finding one,
`pastor`, `pane:<workspace>`) leaves its label as it is. `task describe`
prints the label with where it came from: the template until dispatch, then
the workspace's name, `(joined workspace)` for one the task joined, and the
reason when it fell back to `t-N`.

An agent named like a task (`t-N`) that no open task owns is an orphan: a
dispatch that failed after the agent started, a daemon killed mid-dispatch, a
failed task whose agent never exited, a pruned row. Orphans still hold a pane,
so they count toward `max_agents`. `pastor task list` prints a line for each under
its table, unless `--blocked`, `--done` or `--job` narrows it (an orphan has no
state or job; `--machine` still applies), `machine list` names them in an ORPHANS column,
and `pastor task close t-N` closes one, with or without a row. pastor finds
them when it reconciles (every `reconcile_every`), and closes an orphan's pane
itself once `close_failed_after` has passed since a check first observed its
agent stopped; one at work is left alone, and the clock resets each time the
same pane is found at work again or holds another orphan. It assumes it is the only pastor naming
agents `t-N` on each herdr.

## Agent kinds

pastor starts four kinds of agent through herdr: `claude` (Claude Code),
`agy` (the Antigravity CLI), `codex` (Codex) and `opencode`. An agent's
`kind` in `[agents.<name>]` (its own name, for the built-in kinds) picks
which one; the sections linked below have the detail, this table has all of
it in one place, so a task does not find out what its agent lacks by
failing.

|                      | claude                                                                                                                    | agy                                                                                                            | codex                                                                                                                                    | opencode                                                                                                                    |
| -------------------- | -------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------ |
| **Trust**            | herdr reads its folder-trust dialog `blocked`, sometimes only after it reads the agent ready, so pastor also holds the prompt while its built-in `trust_marker` shows; pastor sends the held prompt once it clears. Built-in `trust_marker` and `trust_keys`, so `pastor task send t-N --trust` answers it and saves the repo; its other startup dialogs are built in too (`task.answered`) | herdr reads its folder-trust dialog idle and ready, not blocked, and its other dialogs too; pastor holds the prompt while one shows, the trust dialog by its built-in `trust_marker` ("Yes, I trust this folder") and others by their `[[agents.<name>.dialogs]]` rows. Built-in `trust_keys` (`Enter`), so `--trust` answers it and saves the repo | herdr reads its dialog idle and ready, not blocked; pastor holds the prompt until someone answers by hand (`Do you trust the contents of this directory`). No built-in `trust_keys`, so `--trust` needs one set in `[agents.<name>]` (`no_trust_keys` otherwise) | no dialog pastor knows |
| **Profile**          | `--permission-mode dontAsk`, plus the tool flags                                                                          | `--dangerously-skip-permissions --mode accept-edits`, so it never asks; pastor first writes the machine's own profile into its settings there (the agy provisioner: allow, deny, mode and the rest, whole), then checks they deny what the task's profile denies (`agy_permissions_missing`), and an agy machine runs one profile, its own (`agy_profile_mismatch`); a shell command can still write, so `review` is not a read-only checkout. `own_permissions = true` adds and checks nothing | `--ask-for-approval never --sandbox workspace-write` (`danger-full-access` under `unrestricted`); no per-command flag, so the sandbox stands in for the lists | `OPENCODE_PERMISSION` in its pane's env instead of flags (deny-first, `unrestricted` allow-first); a machine or repo config with its own permission rules fails the task (`opencode_permissions_conflict`) |
| **Lists** (no profile) | built-in `--allowedTools`/`--disallowedTools`                                                                           | needs `allow_flag`/`deny_flag` in `[agents.<name>]`, else a list refuses the task (`agent_tools_unsupported`)     | same as agy                                                                                                                                 | same as agy                                                                                                                     |
| **Session**          | `--session-id` at launch, recorded; a pause, wait or model move resumes it with `claude --resume <session>`               | none chosen by pastor; the conversation id herdr's agy hook reports back (`agent_session`) is recorded instead, once it has one | none recorded                                                                                                                               | none recorded                                                                                                                   |
| **Pause** (`--preempt`) | eligible: `running`, `low`, a recorded session, not ended, not resumed in the last 10 minutes                           | same as claude, once a conversation id is recorded; never with none (a machine with no agy hook installed)       | never                                                                                                                                       | never                                                                                                                           |
| **Attach**           | a live pane, as any kind; once closed, reopens the recorded session with `claude --resume` in a new pane (`no_agent` with none recorded) | a live pane, as any kind; once closed, reopens a recorded conversation with `agy --conversation` in a new pane (`no_agent` with none recorded) | same as agy, with none ever recorded                                                                                                        | same as agy, with none ever recorded                                                                                            |
| **Dialogs**          | a trailing `●` message ending in `?` blocks the task; `N shell(s) still running` in its footer keeps it running; Claude's limit picker is answered by itself | `Requesting permission for:` / `Run this command?` blocks the task; `· N task(s)` in its footer keeps it running | none read; an idle agent with no `pastor task done` just goes `blocked` (`idle_without_task_done`)                                          | same as codex                                                                                                                   |
| **Limits**           | reads `You've hit your limit · resets ...` and the limit picker at the end of its last message; a short 429/529 is retried in the pane | reads its last paragraph (it draws no message marker) for `RESOURCE_EXHAUSTED`/429: hard when it names the quota, short otherwise | not read yet: a task that runs out just reads idle, like one that finished                                                                 | not read yet, same as codex                                                                                                    |
| **Prompt delivery**  | one message to the session's inbox socket (`pastor inbox send`), as are `task send` text, the rate retry and an orchestrator's stop message, typed with `agent.prompt` when the session does not take it or `delivery = "type"`; with `--session-id`, a paused, waiting or moved task resumes the same session, or hands over the pane tail across kinds | `agent.prompt`, no flag of its own to choose one; a limit, pause or model move resumes the recorded conversation on `--conversation` when there is one, else hands over the pane tail | `agent.prompt`, no session id; neither pause nor a limit applies today, so it only ever starts fresh                                       | same as codex                                                                                                                   |

See [How it works](#how-it-works) for trust and dialogs, [Permission
profiles](#permission-profiles) and [Tool allow and deny
lists](#tool-allow-and-deny-lists) for profile and lists, [Usage
limits](#usage-limits) for session, limits and prompt delivery,
[Pausing a low task](#pausing-a-low-task) for pause, and [Reopening a
finished task](#reopening-a-finished-task) for attach.

A `kind` that is none of these four gets the lists through its `allow_flag`
and `deny_flag` and no never-ask mode, and refuses a list without them
(`agent_tools_unsupported`).

## Flocks

A flock is a named group of machines. A machine can be in many flocks, and
every task and job targets one: only that flock's machines take its tasks.
Flocks keep kinds of work apart, such as work and personal machines that run
agents on different accounts, or share one machine between projects so one
project never takes every slot. A flock also carries every per-task setting
`[defaults]` has, so a project's flock sets its model, permissions, priority
and timeout on a shared machine, while the machine picks the agent (see
[What a flock sets](#what-a-flock-sets)).

```toml
# flock.toml
[[flock]]
name = "personal"
default = true            # tasks and jobs that name no flock go here
machines = { desk = 2 }   # desk runs at most 2 of personal's tasks

[[flock]]
name = "work"
machines = { desk = 1 }   # and at most 1 of work's
agent = "claude"          # optional: the agent for this flock's tasks
agent_args = ["--model", "claude-sonnet-5"]
# model = "sonnet"        # optional: a [models] name, see Models below
# fallback = ["opus"]     # optional: models its tasks may fall back to, see Fallback models
# priority = "high"       # optional: see Priority and queue order below
# agents = { opencode = "opencode" }  # optional: the agent per kind, see Models
# profile = "develop"     # optional: a permission profile, see Permission profiles below
# timeout = "30m"         # optional: how long its tasks may run, before [defaults]
# place = "pastor"        # optional: where its tasks' panes go, see Where a task's pane goes
# summary = "require"     # optional: ask, require or off, see Asking for one
# keep_pane = true        # optional: keep its tasks' panes, see Keeping a task's pane
# remove_worktree = true  # optional: remove its tasks' worktrees when they fail too, see Removing a task's worktree
# unknown_room = "no_room"  # optional: a machine whose room can't be read holds its tasks, see A room floor
# accounts = ["github", "claude"]  # optional: what its machines must be logged into, see Provisioners
# bundles = ["team-rules"]  # optional: what goes in its machines' agent homes, see Bundles
# bundle_vars = { ticket_url = "https://tickets.example.com" }  # optional: its bundles' vars
# bundle_apply = "auto"   # optional: auto or manual (the default), see Bundles
# workdir_bundles = ["repo-notes"]  # optional: what goes in its tasks' working directories, see Stored workdir bundles
# pins = { claude = "2.1.310", agy = "1.4" }  # optional: the harness versions it expects, see Harness version pins
# env = { CARGO_BUILD_JOBS = "2" }  # optional: env for its tasks' agents, over [defaults], under the machine's, see Env for every agent

[[machine]]
name = "desk"
local = true
max_agents = 3

[[machine]]
name = "pi-3"
ssh = "user@pi-3"
flock = "work"            # the old way: in work, with pi-3's own limits
min_disk = "4G"           # optional: free disk on $HOME and /tmp it must have to take a task, see A room floor
# min_memory = "512M"     # optional: free memory it must have; unset with a floor on: [defaults], else 1G
# unknown_room = "no_room"  # optional: as on a flock, after the flock's
# provisioner = "raspberry-pi-os"  # optional: what checks and sets it up, see Provisioners
# bundles = ["gpu-tools"]  # optional: bundles on top of its flocks', see Bundles
# bundle_apply = "manual"  # optional: over its flocks', see Bundles
# pins = { claude = "2.1.309" }  # optional: its own harness pins, over its flock's
# env = { CARGO_BUILD_JOBS = "1" }  # optional: env for every agent on it, over its flock's, see Env for every agent

[[machine]]
name = "laptop"
pull = true               # never connected to: its headless serve claims tasks
```

Each machine sets exactly one of `local`, `ssh`, `command` and `pull` (see
[Pull machines](#pull-machines)).

A machine's name starts with a letter or digit, then holds letters, digits,
`.`, `_` and `-`: the name goes into the `authorized_keys` command a bridge key
runs and into its marker, where anything else would be read by a shell or
would split the line. `pastor machine add` refuses a name that breaks the rule
(`invalid_machine`). A flock.toml written before the rule still loads, with a
warning for each such machine: once by each command, and by `pastor serve` once
for each new version of the file; rename it (`machine remove`, then
`machine add`), as `machine authorized-key` refuses it.

A flock's `machines` names the machines it may use, each with its number
there: at most how many of the flock's live tasks that machine runs. Dispatch
starts a task on a machine only when the machine has room for it
(`max_agents`, and job slots and burst for the tasks that may take them) and
the task's flock is under its number there; job slots and burst never take a
flock past its number. A task whose flock is full everywhere waits, with
`waiting for a machine: flock work is at 1 of 1 on desk` as its error in
`pastor task list` and `pastor queue`, and the next task in the queue goes.
Live tasks count by the flock each was created in.

A plain number is a hard ceiling. A flock can instead have a share and a max
on a machine, so a busy project uses slots the quiet ones leave idle:

```toml
# fragment of flock.toml
[[flock]]
name = "code"
machines = { desk = { share = 2, max = 4 } }
```

Under its share the flock takes a free slot as usual. From its share up to
its max it takes one only while no task of a flock still under its share on
that machine is waiting for it: queued behind it, not pinned elsewhere, with
its tags and an agent for its model there. So a quiet project gets its share
back as soon as it has work, and nothing is reserved while it has none. A
flock past its share that waits says `waiting for a machine: flock code is
past its share on desk, at 2 of 2/4, while flock life waits under its share`;
one at its max says `waiting for a machine: flock code is at 4 of 2/4 on
desk`. Nothing passes the machine's own room,
and job slots and burst never take a flock past its max. `max` below `share`,
`max` alone (the plain number is that), a share alone and a share of 0 with a
max above 0 fail the load, as does a negative or fractional number.

0 (`desk = 0`, or `{ share = 0, max = 0 }`) keeps the machine in the flock
with no slots, to drain it, say before a restart: the flock's tasks already
running there run to the end and are reconciled and closed as usual, and no
new one starts there. A plain task of that flock pinned to the machine
stays queued with `waiting for a machine: flock work has 0 slots on desk:
it starts no new tasks there (1 still running)`, and `pastor flock list`
shows the machine as `desk 1/0` while one runs. A `--now` task pinned to
it never queues there: `now_machine` turns down a drained machine like any
other it cannot use, so the task is closed with `now_not_started`, the
same as a `--now` task its machine would not take for any other reason (on
a pull machine, once the head's wait for its claim ends).
Taking the machine out of the map does not drain it: a machine no flock
names is in the default flock with its full `max_agents`.

The `flock` key on a machine still works: it puts the machine in that flock
with no number but the machine's own limits (`max_agents`, job slots and
burst), as before. A machine that neither a `machines` table nor its own
`flock` key places is in the default flock the same way. A machine is in
every flock that places it, in file order; where one flock has to stand for
it (a task pinned to it that names no flock, the machine's profile, `flock`
in `--json` for older scripts) that is the default flock if it is in it, else
its first. `pastor machine list` shows every flock under FLOCKS, with its
number where it has one (`personal:2,work:1`, `code:2/4` for a share and a
max), and `--json` carries them as `flocks` (`name`, `share`, `max`, `live`;
a plain number has `share` equal to `max`) next to `flock`. A flock's `machines` naming a
machine the file lacks, a number of 0, or a machine placed in the same flock
by both its `flock` key and the flock's `machines` fails the load. `pastor
machine remove` takes the machine out of every flock's `machines` too.

`machine remove` also takes what the head kept for the machine: its marked
lines in the head's `authorized_keys` (`pastor:machine:NAME` and
`pastor:agent:NAME`), its host in pastor's own `known_hosts`
(`known_hosts` in the state directory: the host of its `ssh` target, with or
without a port, and only that name from a line that lists several), and its
bootstrap state in the store. The user's own lines, `~/.ssh/known_hosts`,
another machine's lines and the task rows stay. `machine remove NAME
--dry-run` changes nothing and lists, in one line, what would go, flock.toml
entry first; a name that is not in the flock is refused as `unknown_machine`.
A line of `known_hosts` whose host is hashed (`|1|...`) hides which host it is
for, so it stays, and the line says so with the `ssh-keygen -R` that drops it;
the ssh options pastor uses with its own file turn hashing off, so its own
entries are plain. A head that predates the clean-up removes the machine as
before, and refuses `--dry-run`.

`[[flock]]` entries declare the flocks, so a flock can have no machines yet.
Names are unique and exactly one has `default = true`. A machine naming an
undeclared flock, a membership error above, two defaults, or none makes the
file fail to load, like any other bad flock file: `pastor serve` refuses to start on it, `pastor tick`
and `pastor job list` without a head refuse it too, and a running head keeps
the previous version. A file with no `[[flock]]` entry at all is
one flock named `default` holding every machine, so files from before flocks
load unchanged. A machine's `ssh` is one `[user@]host` word: one that is
empty, starts with `-`, or holds whitespace or a control character makes the
file fail to load the same way, since ssh would read it as an option. Put
ports, keys and jump hosts in `~/.ssh/config` under a host alias instead.

```
pastor flock list                       NAME, DEFAULT, MACHINES, AGENTS, QUEUED (--json; --wide adds DESCRIPTION)
pastor flock add <name> [machines...] [--default] [--description TEXT]
pastor flock join <flock> <machine> [--max N]   put a machine in a flock, or change its number there
pastor flock leave <flock> <machine>    take a machine out of a flock
pastor flock remove <name>              refused while it has machines or queued tasks, or is the default
pastor flock default show               print the default flock
pastor flock default set <name>         new tasks and jobs go to <name>
pastor machine add ... [--flock F]      default: the default flock
pastor machine move <name> <flock>     leave every flock, join this one
pastor machine list [--flock F]
pastor machine describe <name>          one machine in full (--json)
pastor machine cordon <name> [--reason TEXT]    take no new task there; running ones go on
pastor machine uncordon <name>          take tasks again
pastor machine check <name>             what it is missing, from its provisioner (--json)
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

`flock join <flock> <machine>` lists the machine in the flock's `machines`
with `--max N`, by default the number it has there already (a share and a
max stay as written), else its `max_agents`; it stays in its other flocks.
`--max N` writes a plain number; a share and a max are set by editing the
file (`flock edit`). Joining again with `--max`
changes the number; `--max 0` keeps it in the flock with no slots, to drain
it (leave the flock to take it out). A
machine nothing placed was in the default flock only for that, so once a flock
lists it, it is in that flock alone; the output ends with the machine's
flocks afterwards (`its flocks: home:2,work:4`). `flock add <name>
[machines...]` joins the named machines to the new flock the same way.
`flock leave <flock> <machine>` takes it out; out of its last flock it is back
in the default flock, as a machine no flock lists, and a machine in the
default only for that has nothing to leave (`not_in_flock`, as for a flock it
is not in). Tasks already running keep running; a queued task of that flock
pinned to the machine stays queued with a note, and the output names it.
`machine move <name> <flock>` leaves every flock and joins that one with the
machine's `max_agents`, for setups with one flock per machine, so it stays
there whichever flock is the default later. A machine whose only membership
is already that flock's `machines` table is left as it is.

`pastor machine move`, `pastor flock join` and `pastor flock leave` change
the flocks of tasks dispatched after them; tasks already on the machine keep
running there, and its connection stays up. A queued task pinned to a machine
that has moved to another flock stays queued for its own flock, and the head
logs a warning once.

With no head running, `flock add|join|leave|remove` and `machine remove`
hold a lock (`fleet.lock` in the state dir) from their read of the queued
tasks to the save, and `pastor serve` holds the same lock from before it
reads flock.toml until it listens. So a head starting meanwhile waits for the
edit and reads the edited file, never queueing a task in a flock the edit
just removed. An edit that finds a head listening once the lock is free stops
with `head_started`; run it again and it goes through the head. Either side
gives up after 30 seconds with `fleet_locked`. A bare `pastor serve` waits
for the lock before it starts the head and begins its own 30 seconds, so it
reports `fleet_locked` too rather than `serve_slow`.

pastor never rewrites a machine's old `flock` key on its own. The first of
`flock join`, `flock leave` or `machine move` on that machine (a move even to
the flock the key names) moves the key into that flock's `machines`, with the
machine's `max_agents` as the number, and keeps the file's comments. That number caps the machine's job
slots and burst for the flock, as any flock number does; join with a higher
`--max` to let them through.

`flock list` shows each flock's machines with the flock's number and live
tasks there: `desk 1/2, pi-3 0/1`, and with a share and a max live, share
and max: `desk 1/2/4`. A machine with no number written (the old
key, or the default flock of a machine no flock lists) shows its
`max_agents`. The live count needs a running head and is `-` without one
(`desk -/2`), and so is AGENTS, the flock's live tasks over all its
machines. `--json` keeps `machines` as the names and adds `members`
(`name`, `share`, `max`, `live`). The flock commands go through a running
head like the other edits, and an agent pastor started may not run them.

A task's flock is fixed when it is created: `--flock` on `pastor task run`, or
`flock` under a job's `[dispatch]`; else the flock that stands for the machine it is pinned
to (`--machine`, a job's `machine`); else the default flock. `--machine` with
a `--flock` the machine is not in is refused (`flock_mismatch`), as is a flock
that does not exist (`unknown_flock`). A job whose flock does not fit, or
whose pinned machine is not in the flock, fails its run before the connector
is asked for anything, with the reason in `pastor job list`. A flock or a
pinned machine removed while the connector runs gets none of that run's tasks:
each item fails, and the cursor holds for the next run. `pastor task retry`
keeps the flock of the task it copies, and is refused (`unknown_flock`) once
that flock has been removed.

### What a flock sets

A `[[flock]]` entry takes every per-task setting `[defaults]` has: `agent`,
`agent_args`, `agents`, `model`, `fallback`, `profile`, `priority`,
`age_after`, `allow`, `deny`, `timeout`, `place`, `label` and `summary`
(`max_tasks_per_run` stays a job setting), and `unknown_room`, which only a
flock or a machine sets (see [A room floor](#a-room-floor)). A task gets the settings of its own flock, the one it is queued
in; a machine in many flocks does not mix them.

For everything but the agent, the flock comes before the machine: a task
takes each setting from the first of its run flags (or job's `[dispatch]`),
its flock, the machine it runs on, and `[defaults]` that sets it. Only
`model`, `fallback`, `profile` and `priority` have a machine layer; a
machine has no `timeout`, `place`, `label`, `summary` or `age_after`. `allow`
and `deny` add up instead (see [Tool allow and deny lists](#tool-allow-and-deny-lists)).

```toml
# flock.toml: desk is shared; each project's flock sets its own
[[flock]]
name = "pastor"
machines = { desk = 2 }
model = "sonnet"
profile = "develop"
priority = "high"
timeout = "1h"

[[flock]]
name = "life"
default = true
machines = { desk = 1 }
model = "haiku"
place = "pastor"

[[machine]]
name = "desk"
local = true
max_agents = 3
agent = "claude-alt"   # the account logged in here: every flock runs it
model = "opus"              # only for a flock that names no model
```

The agent, its args and `fallback` are the exception: the machine comes
before the flock, since it knows what is installed and logged in there (see
[A flock's or a machine's agent](#a-flocks-or-a-machines-agent) and
[Fallback models](#fallback-models)). A machine's
`profile` also keeps its other job, deciding whether a task may ask for
`unrestricted` there (see [the unrestricted
rule](#permission-profiles)); a flock's profile never lifts it.

A timeout or place from the flock shows in `task describe` as `timeout:
3600s (from flock pastor)`, and `flock describe` lists the flock's own.

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

A flock that names an agent also names its kind. On a machine that names
an agent of another kind, the task runs that machine's agent of the flock's
kind instead: its `agents` entry for the kind (see [An agent per
kind](#an-agent-per-kind)). A machine that has none is skipped for the task,
as a machine whose agent cannot run a task's model is: an unpinned task goes
to the flock's other machines, or waits with `waiting for a machine: flock
work runs opencode agents, and machine pi-3 has none ...` in its `error`,
and a task pinned there is refused (`agent_kind_missing`). A machine that
names no agent runs the flock's. A task with a model goes by the model's
kind instead, as below, and an agent the task or job named is kept.

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
agent = "claude-alt"   # `claude` here is the work account

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
`claude-alt (from machine laptop)` or `--model claude-sonnet-5 (from
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
# fragment of flock.toml
[[flock]]
name = "work"
allow = ["Bash(make:*)"]
deny = ["Bash(git push:*)"]
```

```toml
# fragment of a job file
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

A tool of an MCP server is named `mcp__<server>__<tool>`; `mcp__<server>`
and `mcp__<server>__*` mean every tool of that server. A name is letters,
digits, `_` and `-`, and the pattern takes no `(...)` argument. Any other
`mcp__` pattern fails the file's load, naming the list it is in. Claude gets
the pattern as written; an agy agent maps it to its `mcp(<server>/<tool>)`
rule; a Codex or OpenCode agent has no MCP rule to give it.

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
prints a task's `allow` and `deny`.

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
[agents.claude-alt]
kind = "claude"
env = { CLAUDE_CONFIG_DIR = "~/.claude-alt" }
```

```toml
# fragment of flock.toml: every task of the personal flock runs it
[[flock]]
name = "personal"
agent = "claude-alt"
```

`pastor task run --agent claude-alt` or `agent = "claude-alt"` in
a job does the same for one task. herdr starts a `claude` (the `kind`; without
one, the name itself), and `task describe` and `task list` keep the name
`claude-alt`. Built-in settings follow the kind, not the name: the
definition gets Claude's trust keys and marker and its `--allowedTools` and
`--disallowedTools` flags unless it sets `trust_keys`, `trust_marker`, `allow_flag` or
`deny_flag` of its own. It does not inherit what `[agents.claude]` sets.
`own_permissions = true` says the agent's permissions are in its own
settings on the machine instead (see [Permission profiles](#permission-profiles)).
`login_marker` and `login_check` say how its login screen looks and how to
check that it is signed in (see [Signed-out agents](#signed-out-agents));
the kind's built-ins hold unless it sets them. `load` is the load of a task
that runs the agent and names none (see [Loads](#loads)).

`env` values are passed as written, except that a value of `~` or one that
starts with `~/` is expanded against the home of the machine the task runs
on, as `--repo` is; a `command` machine cannot report a home, so give those
absolute paths. Keys must be variable names (letters, digits and `_`, not
starting with a digit). pastor sets the env on the task's pane, worktree
tasks included (see [Internals](#internals)). A `claude` kind gets
`CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN=1` under its own `env` (see [How it
works](#how-it-works)).

#### Env for every agent

The same `env` key sits in three more places, for what every agent needs and
should not be repeated in each `[agents.<name>]`, such as a `NOTES_URL` or a
`CARGO_BUILD_JOBS`:

```toml
# pastor.toml
[defaults]
env = { NOTES_URL = "http://notes.example:8790" }
```

```toml
# fragment of flock.toml
[[flock]]
name = "work"
env = { CARGO_BUILD_JOBS = "2" }

[[machine]]
name = "pi-3"
env = { CARGO_BUILD_JOBS = "1" }
```

A task's agent gets all of them in its pane, each key over the same key in
the layer before: `[defaults]`, then the `[[flock]]` of the task, then the
`[[machine]]` it runs on, then the agent's own `env`. So the example gives
the pane `CARGO_BUILD_JOBS=1` on pi-3 and `2` on another machine of the flock,
and an agent that sets `CARGO_BUILD_JOBS` itself keeps its own. A layer sets
only the keys it names. The task's flock is the one it was queued for, not
the machine's, so a machine in two flocks gives each its own layer. These
layers are over what pastor sets for the kind (`CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN`
for Claude) and under what it sets last (`PASTOR_TASK`, `PASTOR_HEAD`, and the
harness pin's keys). `~/` values are expanded as in an agent's `env`.

Names are checked like an agent's. Unlike an agent's, a value in these three
places is refused at load when it looks like a secret, by
the rules `pastor bundle add` uses (a token shaped like a GitHub, Slack,
AWS or API key, a private key, or a long value under a name that says token,
secret, key or password): a flock or defaults file is read by more people than
one agent's. Put a secret in the agent's own `env`, or in the machine's
environment. The message names the layer and the key, never the value. A
change to any layer replaces the actors of the machines it reaches, as an
`[agents]` edit does. The pane `pastor task attach` opens again for a closed
task's session gets the same layers, over by the agent's `env`.

A pull machine's own `pastor serve` applies the layers of its own config, read
when it starts: its `[defaults] env`, and its own flock.toml's, either its entry
for the machine (its flocks and its own `env`) or, with none, every `[[flock]]`
that sets `env`, for a task to pick by the name of its flock. The head's layers
are not sent to it.
A flock.toml that is there but does not load (a syntax error, an `env` name that
is not valid, a value that looks like a secret) stops the serve from starting:
it exits 1 with the file and the reason, rather than run every task without the
env those layers set. No flock.toml at all is fine.

`pastor task describe` lists the env the task's agent gets from the head's
config, as the config reads now (through a remote head, the head's), in an
`env` row, each key with the layer that gave it (`NOTES_URL=http://notes.example:8790
(defaults)`, `(flock work)`, `(machine pi-3)`, `(agent claude)`). In `--json`,
`env` is an object of keys, each `{"value": ..., "from": ...}`. The value of a
key that looks like a secret, or that holds what looks like one, is
`(hidden)`. Nothing is listed when the config sets none; what pastor sets
itself is not listed. For a task on a pull machine the row says its own
config decides, and lists none of the head's layers (`env` is left out of
`--json`).

`[[agents.<name>.dialogs]]` lists startup dialogs beyond the trust one, one
table per dialog: `name`, `marker` (text only that dialog shows) and `pick`
(the text of the option to choose) or `keys` (named keys, pressed as they
are). A row named as one of the kind's built-ins replaces it; a row with
`pick = ""` and no `keys` removes it, and needs no `marker`. Any other row
needs its `marker` and one of `pick` and `keys` (`needs pick or keys`). A
prompt waits while a row's marker shows, until the row is answered for the
task. The trust dialog is set with `trust_keys` and `trust_marker`, never here. Claude's built-ins are
`fullscreen`, `auto-mode-default`, `auto-mode-setup`, `effort-default` and
`autocompact`
(see [How it works](#how-it-works)).

```toml
# fragment of pastor.toml: accept Claude's bypass warning on this head's tasks
[[agents.claude.dialogs]]
name = "bypass"
marker = "Bypass Permissions mode"
pick = "Yes, I accept"
```

The head checks a task's agent against pastor.toml when it queues it (for a
task that is not pinned, the agent each machine of its flock would run), and
`pastor task run` makes the head apply an edit of pastor.toml or flock.toml
first, so a definition you have just added is the one its machine starts.

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
# fragment of flock.toml: the personal flock runs sonnet unless a task says otherwise
[[flock]]
name = "personal"
agent = "claude-alt"
model = "sonnet"
```

A task settles its model like its agent, from the first of these that names
one: `--model` on `pastor task run` (or `model` under a job's `[dispatch]`),
`model` under its `[[flock]]` entry, `model` under the `[[machine]]` entry it
runs on, `[defaults] model`; with none, it runs no model and nothing
changes. The flock comes before the machine, unlike for the agent (see [What
a flock sets](#what-a-flock-sets)). `--model` takes a name only, never raw args.

A job's `model` is a template, rendered for each item:
`model = "{{ item.model }}"` runs the model the item names. It may use
`item.*` and `job.name`. When it renders empty, the job says nothing and the
lookup goes on to the flock, the machine and `[defaults]`.

The model's `args` go first, then the task's `agent_args` settled as above,
then the allow and deny flags, so permission flags in `agent_args` still
apply. A task in the personal flock above starts as `claude --model
claude-sonnet-5 ...` under the `claude-alt` definition.

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
settles the model's args again from pastor.toml as it stands.

#### Fallback models

`fallback` names, in order, the `[models]` a task may go on under when its
own model runs out: a new task starts on the first that is free, and a
running one moves down the list when its model hits a usage limit (see
[Usage limits](#usage-limits)).

```toml
# fragment of flock.toml
[[flock]]
name = "personal"
model = "opus"
fallback = ["sonnet", "gpt"]   # then sonnet, then gpt

[[machine]]
name = "desk"
local = true
fallback = []                  # tasks on desk fall back to nothing
```

Each entry is a model, not an agent: `gpt` finds its agent on the task's
machine as any model of another kind does (see [An agent per
kind](#an-agent-per-kind)), so a task never falls back to another login of
its own kind.

A task takes its list from the first of these that sets one:
`--fallback sonnet,gpt` or `--no-fallback` on `pastor task run` (or
`fallback` under a job's `[dispatch]`), `fallback` under the `[[machine]]`
entry it runs on, `fallback` under its `[[flock]]` entry, `[defaults]
fallback`. Unlike `model`, the machine comes before the flock. The first
layer that sets a list wins whole: lists never merge, and `fallback = []`
means none and ends the lookup, so desk above gives its tasks no list
although the personal flock has one. With none set anywhere, the task has
none. `--no-fallback` is `[]` for one task, and `--fallback` with it is
refused.

A job's entries are templates, rendered for each item like its `model`;
an entry that renders empty is dropped, so `fallback = ["{{ item.fallback
}}"]` gives an item with no `fallback` a task with no list (the job still
set it, so the machine's and flock's are not used).

Each entry must be in `[models]`: `unknown_model` from `pastor task run` and
`pastor task retry`, a load error in flock.toml and pastor.toml, and the
job records the error for that item. `--fallback` takes names only, never
raw args. The task's own model, and a repeat, are dropped from its list
without an error.

`pastor task describe` prints the list and where it came from, such as
`fallback: sonnet, gpt (from flock personal)`, or `fallback: - (from
machine desk)` for a `[]`; `--json` has a `fallback` array.
`flock describe` and `machine describe` show their own `fallback`. A head
from before fallback models refuses `--fallback`, `--no-fallback` and a
job's `fallback` (`head_too_old`).

A new task goes on under its fallback list when its own model is on an
exhausted account where it would run, and a running one moves to its next
model when it stops on a limit whose reset is not near; see [A limited task
moves to its next model](#a-limited-task-moves-to-its-next-model).

### Usage limits

A usage limit belongs to an account: when one Claude login runs out on one
machine, it has run out on every machine that uses it. The head keeps a
record of exhausted accounts, and a new task does not start on one before it
resets.

Which account an agent is logged in to is a label you give it, `account`
under its `[agents]` table. pastor never reads it as a credential; it only
compares names.

```toml
# pastor.toml
[agents.claude-alt]
kind = "claude"
env = { CLAUDE_CONFIG_DIR = "~/.claude-alt" }
account = "me-personal"    # every machine's claude-alt is this login
```

An agent that names an account shares its limit with every machine whose
agent names the same one. An agent that names none keeps its limit to the
machine it was seen on, under `<machine>/<agent>` (`pi-1/claude`): the same
agent name can be another login on another machine, so nothing is shared
unless you say so. An account may not be empty or hold a `/`.

A limit is kept for the whole account, or for one model when the message
names one (`Opus weekly limit reached` stops `opus` only). It holds until
`retry_at`: the reset the message named, else `unknown_reset_wait` from when
it was seen, else `retry_after_no_credit` for a message that says the credit
ran out (`[limits]` below). A later limit on the same account and model
replaces the row. Each dispatch pass first deletes the rows whose
`retry_at` has passed, each with `agent.reset` (`by: time`).

Until then, dispatch skips a machine where every model the task may run is
on an exhausted account: its own model first, then each of its
[fallback models](#fallback-models) in order, each settled on that machine as
any model is. On the first one that is free, the task starts, and `task
describe` says which and why:

```text
model:      gpt (fallback 2 of 2; me-personal exhausted until 03:00)
```

With none free anywhere, the task stays `queued`, and `task describe` and
`pastor queue` say why:

```text
waiting: me-personal exhausted until 03:00 (5-hour limit, seen by t-12)
```

The rows come from a task's agent that stopped on a limit (below), from an
orchestrator's agent that stopped on a quota (see
[Orchestrators](#orchestrators)), and from a pull machine's report. An
orchestrator whose agent's account is exhausted does not start one before
the account resets, and shows `waiting for quota`.

`pastor limit list` shows every row: the account, the model (`-` for the
whole account), when it is tried again, what ran out and the task that saw
it; `--json` has every field. `pastor limit clear <account>` forgets an
account's limits, `--model <m>` only that model's, with `agent.reset`
(`by: hand`), and the tasks they held, queued or waiting, start on the next
pass; clearing an account with no limit fails with `not_exhausted`. There
is no `limit set`. An agent pastor started may list the limits, and may
not clear one.

`[limits]` in pastor.toml:

| key | default | does |
|---|---|---|
| `wait_under` | `"30m"` | a limited task waits for a reset closer than this, and moves to its next model past it; `"0"` never waits when it can move |
| `rate_retries` | `3` | a task stopped on a 429 or 529 is retried in its pane this many times before it counts as limited |
| `rate_backoff` | `["1m", "5m", "15m"]` | the wait before each of those retries; the last one repeats |
| `unknown_reset_wait` | `"1h"` | how long a limit whose message names no reset holds |
| `retry_after_no_credit` | `"6h"` | how long a limit for no credit holds |
| `handover_lines` | `60` | the pane lines a task moving to another agent hands to it |

A pull machine's serve reports a limit with its task's state, and the head
keeps it under the account the task's agent names on that machine. The
table lives on the head only; a head from before usage limits refuses
`limit list` and `limit clear` (`head_too_old`).

#### A limited task waits

A Claude agent that runs out stops with a message such as `You've hit your
limit · resets 3am` and sits idle, exactly like one that finished. Before
it calls a task `done`, pastor reads the end of the pane (as for a
question, see [How it works](#how-it-works)); an agent that dies at its
start leaves the same message at the end of its pane, read before the task
would fail. Only the agent's last message after the last prompt counts: a
limit further up, a quoted one, or tool output that shows one (a grep of
pastor's own source) is no limit. A task its agent ended with `pastor task
done` is never limited.

pastor reads limits for three kinds of agent, Claude, agy (the
Antigravity CLI) and opencode; another kind that runs out reads as done.
agy draws no marker before its messages, so for agy the end of its turn is its last
paragraph after the last prompt, above its input box. It counts when it
starts with agy's API error, `RESOURCE_EXHAUSTED (code 429): Individual
quota reached. ... Resets in 4h21m30s.`: a message that says the quota is
reached is a usage limit, with its reset read from `Resets in`, and any
other `RESOURCE_EXHAUSTED` or 429 is a short one, retried in the pane as
Claude's 429 is. The quota counts for the whole account, not one model.

opencode shows its provider's error once its own retries are over, as a
block right above the `▣  Build · <model>` line that closes the turn, and
pastor reads that block only, from its first line, and only when nothing
but the input box is under the turn. The block's first line decides:

| opencode shows | limit |
|---|---|
| `You exceeded your current quota` (OpenAI's `insufficient_quota`), `Quota exceeded` | no credit: waits `retry_after_no_credit` |
| `The usage limit has been reached` (a ChatGPT sign-in), `You've hit your usage limit` | usage limit, with the reset it names, if any |
| `Rate limit reached for ... Please try again in 20s` (OpenAI) | short, reset in 20 seconds |
| `This request would exceed the rate limit ...`, `Overloaded` (Anthropic's 429 and 529), or their `rate_limit_error` or `overloaded_error` JSON | short |

A tool call's output is a block too, but it starts with the call (`$ grep
...`), so it is no limit, and neither is an error above a later prompt.

The limit goes in the table as above, with `agent.exhausted` and
`task.limited`, and the task goes `waiting`:

- pastor presses `esc` in its pane, so Claude ends its turn and keeps the
  conversation, then closes the pane, as a pause does. A worktree stays on
  disk.
- The task is pinned to its machine, holds no slot there, and waits until
  the row's `retry_at`, which is `waiting_until` on the task: the reset the
  message named, else `unknown_reset_wait` (`retry_after_no_credit` for no
  credit).
- Its error says which account and until when: `me-personal exhausted
  until 03:00 (5-hour limit, seen by t-12)`. `task list` shows `waiting
  03:00`; `task describe` has `waiting: until 03:00` and the error as
  `limit:`.
- `task.waiting` goes out with `why` (`no_fallback` for a task with no
  [fallback models](#fallback-models), `reset_soon` for one whose reset is
  within `wait_under`), the account, the model, `until`, and `shown`, the
  `waiting 03:00` a board or a notification can print as it is.

A task with a fallback list waits only when the reset is known and within
`wait_under` (`30m` by default): a short wait keeps the model and the
conversation. `task describe` then says why it did not move on:

```text
waited:     reset in 12m, under wait_under 30m
```

Any other limit moves it to its next model (below). With no list the task
waits, whatever the reset: pastor builds no list of its own.

Each dispatch pass looks at the waiting tasks, the soonest first and before
the queue. One goes on once its `waiting_until` has passed and no live limit
holds its agent and model on its machine: when the pass drops the row at its
`retry_at`, or on the pass after `pastor limit clear`, which brings
`waiting_until` forward on the tasks the cleared rows held. A task whose
limit could not be kept in the table still waits for its time. A later limit
on the same account keeps it waiting. It resumes as a paused task does, on
its machine and once there is room, and like one, past its flock's share
there, it leaves the slot to a flock under its share that has a task queued
for it: back in its own checkout, on its recorded session (`claude --resume
<session>`, or, for an agy task that has one, `agy --conversation <id>`;
see [Agent kinds](#agent-kinds)) and a line telling the agent it stopped on
a usage limit that has reset. A task with no session to resume (an agy
task on a machine with no agy hook installed, or one whose agent died at
the start of a new one, so its prompt never reached it) starts again in
the same checkout with its prompt and a paragraph saying that an earlier
start stopped on a limit and may have left work there. An opencode task
keeps no session, so it always starts again this way. If the limit still
holds, the agent stops on it again and the task waits again.

#### A limited task moves to its next model

A task with [fallback models](#fallback-models) whose reset is unknown or
further off than `wait_under` goes on under the next model of its list,
on the same machine, in the same worktree and branch. It is a new round of
the same task, not a retry: the same id, one row. `wait_under = "0"` never
waits when a move is possible.

Its pane is closed as for a wait, and the next dispatch pass, which is due
at once, picks the model: the first after the one it stopped on that its
machine can run (an agent of the model's kind, as for any model) and whose
account is not exhausted. The task restarts there once there is room:

- On an agent of the same kind and account (Opus to Sonnet on one Claude
  login), in the same session: `claude --resume <session>` with the new
  model's args, and a line telling the agent it hit the limit on the old
  model and runs on the new one now.
- Otherwise (Claude to opencode's GPT), a new session with the task's
  prompt, then a paragraph saying another agent started the task in this
  worktree and stopped at its limit, to read `git status` and `git log`
  first, and the last `handover_lines` of that agent's pane.

`task.agent_switched` goes out with `from` and `to` (each an `agent` and a
`model`), `why` (`limit`, or `rate_limit` for short limits that counted as
hard), `until` (when the old model is tried again) and `handover`
(`session` or `pane_tail`). The round that ended gets `limit: <line>` for
its summary, and `task describe` lists the rounds:

```text
rounds:     1 claude-alt (opus): limit: Opus weekly limit reached; 2 claude-alt (sonnet): running
```

A task that moved stays on its new model when the old one resets; only new
tasks start on the first choice again. A limit on the new model is handled
the same way, with the rest of the list; past its end the task goes back to
the first model before it that is free. When no other model of its list is
free, it waits: `task.waiting` with `why: all_exhausted`, until the earliest
reset among all the models of its list. At that time it starts on the first
model of its list that is free: its own session when that is the model it
stopped on, the handover above otherwise. When no other model of its list
runs on its machine, it waits on its own model as a task with no list does:
`why: no_fallback` with the account and model, until that model's reset,
and it resumes on it.

`waiting` is not `paused`: they share the closing and the resuming, not the
meaning. A paused task goes first among the `low` tasks when a slot frees;
a waiting one goes on at its time. `pastor task close` closes a waiting
task's row (`--remove-worktree` removes its kept worktree, as for a paused
one). `task retry` refuses it with `task_waiting`, since it goes on by
itself and a retry would put a second agent on the same work, and so does
`task attach`. `task send` answers `task_not_live`.

#### Short limits are retried in the pane

Claude retries a 429 (rate limit) or 529 (overloaded) about ten times by
itself; pastor sees only one that outlives that, when the turn ends on
`API Error: 529 ...`. Such a task keeps its pane, its slot and its state,
so the conversation goes on where it stopped:

- After the next wait of `rate_backoff` (`1m`, `5m`, `15m` by default, the
  last one repeating), pastor sends `pastor: the API was busy; carry on
  where you left off.` as `task send` would send it (through a Claude
  session's inbox socket, else typed into its pane with Enter; each retry past
  the first says its attempt number), with a `task.input` that has
  `rate_retry: true`. A message that
  names a wait (`try again in 20s`) is waited at least that long.
- Before each retry, `task.rate_limited` goes out with `line` (the error),
  `attempt` (from 1) and `retry_at`.
- A turn that ends on anything else starts the count over.
- After `rate_retries` retries (3 by default) whose turns still end on a
  short limit, the next one is handled as a hard limit with no reset: the
  task moves to its next model with `why: rate_limit`, or waits
  `unknown_reset_wait`, as above. `rate_retries = 0` does that at once.

The count is kept in memory: after a restart of the head it starts over.

#### Claude's limit picker

Some Claude builds show a picker at the limit instead of ending the turn:
stop and wait for the limit to reset, upgrade the plan, or use extra usage.
herdr reports the agent `blocked`. Each time a task goes `blocked`, pastor
reads the end of its pane once, as strictly as it reads a limit message:
the picker has to be the last thing there, under at most its key hints, a
list numbered from 1 with the cursor (`❯`) on one option, and either an
option of Claude's limit picker or a limit message just above it. A
numbered list in a message, or another dialog such as a permission prompt,
is no picker.

pastor picks "Stop and wait for limit to reset" by its text, wherever it
sits in the list (the arrow keys to it, then Enter), emits `task.input`
with the keys and `limit_picker: true`, and the task waits for its reset as
above. A picker without that option, or in words pastor does not know,
gets no key: the task stays `blocked` for a person, its error saying
`looks like a usage limit picker with no "Stop and wait for limit to reset"
(1. Upgrade your plan, 2. Use extra usage); nothing was pressed`.

pastor never picks extra usage or an upgrade, and no setting makes it:
spending money stays a person's call.

A headless serve keeps no table, so its own tasks settle as before, and a
pull machine does not read limits from its panes yet.

#### Signed-out agents

An agent whose login is gone cannot work until someone signs it in, so
pastor holds its tasks as it holds them on a usage limit, with no reset to
wait for. It reads an agent as signed out when:

- its pane shows its login screen, at the bottom of the pane as a trust
  marker is read: Claude's `Select login method:`, agy's `You are currently
  not signed in`, or the agent's own `login_marker`;
- a Claude turn ends on its message that the login is gone (`Not logged in
  · Please run /login`, `Login expired`, `OAuth token revoked`, `Session
  expired`, `Invalid API key`, an `API Error: 401`), read where a limit
  message is read, so the same words in a tool call's output or further up
  do not count.

pastor looks when a task starts (once the agent is up, blocked or ready,
before it sends the prompt, so the prompt never goes into the login
screen), when a running agent goes idle, and once each time it goes
`blocked`. The task is held, not failed: its pane is closed, its worktree
kept, and it goes `waiting`, or moves to its next
[fallback model](#fallback-models) on the next pass when it has one, as a
task on a usage limit does.

The agent is marked signed out in the limits table, under
`<machine>/<agent>` whatever its `account`: a login lives on one machine,
so the same account on another machine keeps working. The mark comes with
`agent.signed_out` and `task.limited`. While the mark holds, dispatch skips
that agent there, and `task describe`, `pastor queue` and `pastor machine
describe` say why:

```text
waiting: pi-1/claude signed out (seen by t-12)
```

No time clears the mark. `pastor limit list` shows it with `sign-in` for
when and `signed out` for what ran out. Once you have signed the agent in
on the machine, `pastor agent signed-in <agent> --machine <machine>` runs
its login check there and, when it passes, clears the mark with
`agent.signed_in` (`by: check`); the tasks it held start on the next pass.
A check that fails says `signed_out`, an agent with no check
`no_login_check`. `pastor limit clear <account>` also clears it, with
`agent.signed_in` (`by: hand`). Clearing a mark leaves the account's usage
limits as they are.

Once you have signed the agent in on the machine, or to have pastor walk
you through it, see [Signing an agent in](#signing-an-agent-in).

The login check is a shell command run on the machine with the agent's
`env`, and passes when it exits 0. It is the agent's own `login_check`, else
the `[login] check` of its kind's [harness manifest](#harness-manifests)
(Claude's `claude auth status --text`, agy's listing of its models), else,
for a Claude agent, `claude auth status` read for `loggedIn`. Set
`login_check` for any other agent:

```toml
# pastor.toml
[agents.codex]
login_marker = "Sign in with ChatGPT"   # text only its login screen shows
login_check = "codex login status"      # exits 0 when signed in
```

An empty `login_marker` or `login_check` turns the built-in off, and an
empty `login_check` the manifest's too.

#### Signing an agent in

`pastor login <machine> <agent>` signs an agent in on a machine with you at
the browser. The head opens a workspace there labelled `login/<agent>`, with
the agent's `env` (so `claude-alt` signs in to its own
`CLAUDE_CONFIG_DIR`), as the machine's login user, and types the
interactive login of the agent's kind into it: the `[login] start` of its
[harness manifest](#harness-manifests). Claude's is `claude auth login`;
agy's is agy itself, which opens on its list of ways to sign in, where
pastor presses Enter for Google OAuth. It then reads the pane and answers
with what you need:

```text
signing claude in on pi-1 in pane w4:p1
open https://claude.ai/oauth/authorize?...
then give the code the browser shows: pastor login code pi-1 claude <code>
the head clears the signed-out mark once the agent's login check passes
```

Open the URL, sign in, and enter the device code where the harness shows one
(`code` in the answer). Claude gives a code back in the browser for the
pane: `pastor login code <machine> <agent> <code>` types it there, then
Enter; leave the code out to give it on stdin, so it stays out of your
shell's history. pastor keeps the code nowhere, events and logs included.

The head watches the login after the command returns. It reads the pane
every 2 seconds, and says when the URL or code shows or changes with
`login.pending`. Every fifth read it runs the agent's login check, the same
one `pastor agent signed-in` runs. When the check passes, it closes the
pane and clears the agent's signed-out mark on that machine, with
`agent.signed_in` (`by: login`), even when there was no mark; the tasks the
mark held start on the next pass. A pane closed before then, a login still
waiting after 30 minutes, or `pastor login cancel <machine> <agent>` ends it
with `login.closed` and leaves the mark.

`pastor login list` shows each pending login (`--json` for an array of
`{machine, agent, pane, url, code, paste, started}`, the same as
`login.pending` carries). `paste` says the pane waits for a code given
back. A second `pastor login` of the same agent on the same machine answers
the pending one rather than opening another pane, and one whose check
passes already opens nothing and only clears the mark. A login whose pane
shows no URL within 20 seconds is answered without one; `login list`
shows it once it does.

pastor reads the pane and runs the check, nothing more: it never reads,
copies or stores the credential the harness writes. `pastor login` needs a
running head, which keeps the pending logins in memory: a head that
restarts forgets them, and their panes stay open until closed by hand. A
pull machine cannot be signed in this way (`pull_machine`), nor an agent
whose kind's manifest has no `[login] start` (`no_login_start`), nor one
whose check cannot run there (`check_unknown`). An agent pastor started is
refused all but `login list`.

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
`extends`, another profile, built in or not; `allow` and `deny`; and an `agy`
table, the settings an agy machine gets beside the lists (see
[a task's profile](#a-tasks-profile)). Names follow
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
itself, fails the file's load, as does a pattern that is empty, starts with
`-` or is a malformed `mcp__` one. A profile can allow or deny a server's
tools, so a task can post to a board and nothing else of its server's:

```toml
# pastor.toml
[profiles.poster]
description = "review, plus posting to a board"
extends = "review"
allow = ["mcp__board__post"]
deny = ["mcp__board__delete"]
```

```bash
pastor profile list               # NAME, SOURCE (built-in, pastor.toml, or pastor.toml (overrides built-in)), EXTENDS, DESCRIPTION; --json (source: built_in, config, overrides)
pastor profile describe ci        # the chain (ci -> develop) and the allow and deny it adds up to; --json
pastor profile render ci --kind agy [--machine pi-3] [--json]   # what ci becomes for claude or agy
```

All three read pastor.toml on this machine and never ask the head, so they
work with it down; with a remote head set they read the head's pastor.toml
through it. A name that is not a profile is `unknown_profile`.

`profile render` prints what a profile becomes for one kind of agent, so a
tool that drives pastor can name a pastor profile for its own sessions and
write the result itself, and a permission rule lives in one place. It renders
`--kind claude` and `--kind agy`; `opencode` and `codex` are refused for now
with `render_kind_unsupported`. It reuses what a task under the profile is
launched with, so the two cannot disagree: `flags` are the args the agent is
started with after its own, `env` is what the launch adds to the agent's own
(nothing for these two kinds), and `settings` is the profile in the agent's
settings file shape: Claude's `permissions` with `defaultMode` `dontAsk`, and
for agy the permission object pastor writes into its settings (see [a task's
profile](#a-tasks-profile)). `files` is empty for now. Every allow that does
not reach the agent is listed in `unsupported` with why, not dropped: an allow
agy has no rule for, one it would state more widely than the profile does, and
all of them for an agent with `own_permissions`. A deny agy has no rule for is
not listed but refused, as a task under the profile is
(`agy_deny_unsupported`): rendering it without the deny would let agy do what
the profile forbids.

```text
$ pastor profile render ci --kind agy --json
{
  "profile": "ci",
  "kind": "agy",
  "flags": ["--dangerously-skip-permissions", "--mode", "accept-edits"],
  "env": {},
  "settings": {"permissions": {"allow": ["read_file(*)", "mcp(board/post)"], "deny": ["write_file(*)"]},
               "toolPermission": "always-proceed", "agentMode": "accept-edits"},
  "files": [],
  "unsupported": [{"pattern": "Grep", "why": "agy has no rule for it"}]
}
```

With `--machine pi-3` the profile is settled as for a task there: it is the
machine's own profile (its `[[machine]]`, else its flock's, else
`[defaults]`) when none is named, `unrestricted` renders only where that is
`unrestricted` (`profile_not_allowed`), for agy the machine's own profile must
deny everything the profile does, as for a task, since agy's rules are per
machine (`agy_profile_mismatch`), and a machine with no profile and no name is
`no_profile`. It reads flock.toml the way it reads pastor.toml. An
unknown machine is `unknown_machine`. `--json` prints the object above; the
default is the same as lines.

### Priority and queue order

A task that no machine can take yet waits in the queue. Each task has a
level, `low`, `normal`, `high` or `critical`, and each dispatch pass takes
queued tasks by level, highest first, then by position, then oldest first.
A task that does not fit anywhere (its machines are full, or none has its
tags) is skipped and the pass goes on to the next, so a lower task can
still start where a higher one cannot.

A task settles its level when it is queued, from the first of these that
sets one: `--priority` on `pastor task run` (or `priority` under a job's
`[dispatch]`), `priority` under its `[[flock]]` entry, `priority` under the
`[[machine]]` entry it is pinned to (`--machine` or a job's `machine`; an
unpinned task never takes a machine's, since no machine is picked yet),
`[defaults] priority`; with none it is `normal`.

```toml
# pastor.toml
[defaults]
priority = "low"            # tasks that set none
```

```toml
# fragment of flock.toml
[[flock]]
name = "work"
machines = { pi-3 = 2 }
priority = "high"           # this flock's tasks, before the machine's

[[flock]]
name = "play"               # sets no priority
machines = { pi-3 = 2 }

[[machine]]
name = "pi-3"
priority = "critical"       # tasks pinned to pi-3 whose flock sets none
```

A job's `priority` is a template, rendered for each item, so `priority =
"{{ item.priority }}"` takes the level the item names; rendered empty, it
falls through to the flock, the machine and `[defaults]`. A value that is not
one of the four levels is refused: `unknown_priority` from `task run
--priority` and `task priority`, an error for that item in a job (the
cursor holds, as for any item error), and a load failure in pastor.toml,
flock.toml or the job file.

`pastor task priority t-4 critical` puts a queued task at another level; it
keeps its position, so among the tasks of its new level it goes by when it
was queued. A task a machine has taken, or is being sent to, has left the
queue, and is refused with `not_queued`; an agent pastor started is refused, as for any change to
the fleet (`agent_refused`). `pastor task retry` keeps the level the task
had before it aged (its current level, if it has not), and the copy queues
last in it.

#### Ageing

A queued task that has waited `age_after` goes up one level, and again
after each further `age_after`, so a `low` task behind a steady stream of
`normal` and `high` tasks still runs in the end. Ageing stops at `high`: it
never makes a task `critical`, so it never lets one burst or pause another,
and it promises nothing against `critical` work: a steady stream of
`critical` tasks still goes first, for as long as it keeps coming. `high` and
`critical` tasks do not age. A task `--preempt` paused ages like a queued
one, its wait counted from when it was paused, so a `low` task queued after
it never ages past it. The wait is the flock's
`age_after`, else `[defaults] age_after`, else 30 minutes; `never` turns
ageing off for that flock, or for every flock that sets none:

```toml
# pastor.toml
[defaults]
age_after = "1h"
```

```toml
# fragment of flock.toml
[[flock]]
name = "batch"
age_after = "never"         # its low tasks stay low
```

Each dispatch pass ages the queue first, so a task steps up at most once a
pass. The first step counts from when the task was queued, each later one
from the last step. A level set by hand (`task priority`, or a `queue move`
that lifts or lowers the task) is the task's own from then on: it forgets
the level it aged from, and ages on from there like any other; a task
placed by `queue move` ages the same as any other. `pastor queue` shows the
level an aged task had before (`high (was low)`, `aged_from` in `--json`),
`task describe` too (`priority: high, aged from low (from task run)`), and
the task's JSON has `aged_from` and `aged_at`. A retry of an aged task
queues at the level it had before it aged, and waits afresh. The level the
task had when a dispatch pass placed it is the one it keeps: it does not age
while its machine is still starting it.

`pastor task describe` prints the level and the layer that set it, such as
`priority: high (from flock work)` (`task priority` when set by hand);
`task list` has a PRIORITY column, and `--json` has `priority`,
`priority_from` and `queue_pos`, the task's position.

### Loads

A load says what kind of work a task is, such as `rust` for one that
compiles Rust or `light` for one that only reads. A machine takes the loads
flock.toml lists for it, and a task with a load goes only to a machine that
takes it. A task with no load goes to any machine, as before.

```toml
# fragment of flock.toml: only desk builds Rust; pi-1 takes light work
[[flock]]
name = "build"
machines = { desk = 2 }
loads = ["rust"]

[[machine]]
name = "pi-1"
ssh = "fleet@pi-1"
loads = ["light"]
```

A machine takes its own `loads` and the `loads` of every flock it is in.
A flock's loads count for any task placed on its machines, not only for
the flock's own tasks. So a task in the default flock with load `rust`
goes to desk when desk is in the default flock too, and the task does not
have to name `--flock build`. A load narrows where a task goes inside its
own flock. It never moves the task to another flock.

A task's load is the first of these that names one:

1. `task run --load rust`;
2. the `load` of the agent it runs, `[agents.<name>] load` in pastor.toml;
3. the `load` of its repo, by the path it names with `--repo`:

```toml
# pastor.toml
[agents.builder]
kind = "claude"
load = "rust"

[repos."~/src/site"]
load = "light"
```

The repo's path is matched as the task writes it, except for a trailing `/`,
since it is a path on the machine that runs the task. A job's tasks take
their agent's and their repo's load the same way. A job's `[dispatch]` has
no `load` of its own.

The head settles the load when it queues the task, and checks it there.
If no machine of the task's flock takes the load, the task is refused with
`load_not_taken`. The same happens when the task is pinned to a machine
that does not take the load. A load that is not a load name is refused with
`invalid_load`. `task describe` shows the load and where it came from
(`load: rust (from agent builder)`), and `--json` has `spec.load` and
`spec.load_from`. If a flock.toml edit later leaves no machine for a queued
task's load, the task waits, and `pastor queue` says why: `no connected
machine in flock default takes load rust`.

A head older than protocol 46 would drop the load and place the task on any
machine. So the CLI refuses to send it `--load`, and refuses to reload it
with a flock.toml that lists `loads`.

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
reserved = 0        # default 0: max_agents slots kept free of pastor's own tasks
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
takes it; ties keep flock order. A critical task can also take the slot of
a low one: see [Pausing a low task](#pausing-a-low-task). `machine add`
takes `--job-slots` and `--burst` as it takes `--max-agents`. `machine list` shows the room as
`2+1j+1b` (`2` alone when both are 0), `machine describe` as `1 of 2+1j+1b`,
and `--json` has `job_slots` and `burst` on each machine. Changing either
restarts the machine's actor on reload, as changing `max_agents` does.
This is slots, not disk or memory; for those see [A room
floor](#a-room-floor) and `machine list --wide`'s DISK and MEM columns.

People and other tools open agent panes on a machine too, and pastor counts
only its own tasks against `max_agents`. `reserved = N` on a `[[machine]]` entry
keeps `N` of its slots for those panes: dispatch places at most `max_agents -
reserved` of its own tasks there, job slots and burst on top as before, so a
machine with `max_agents = 3` and `reserved = 1` takes two shared tasks and
the third waits (`machine pi-3 is full (2/2)` in `pastor queue`). The default
is 0; more than `max_agents` fails the load (`machine pi-3: reserved 4 is
more than max_agents 3`). `task run --now` looks past it, as it looks past
`max_agents`. pastor does not look for the other panes: it keeps the room,
whether they are there or not. Changing it takes effect on the next reload and
does not restart the machine's actor. `machine list --wide` has a RESERVED
column (`-` for none), `machine describe` says `1 of 3, 1 reserved for other
panes` on its `agents` line, and `--json` has `reserved` on each machine. It is
a key of `flock.toml`; `machine add` has no flag for it.

### A room floor

A machine can also have a floor: the disk and memory it must have free to
take a task, so a full disk or a swapping box does not get the next one.
`min_disk` and `min_memory` on a `[[machine]]` entry are sizes, digits and
one of `K`, `M`, `G` or `T` (powers of 1024), and a bare `"0"` turns that
side off. The disk floor applies to each of `$HOME` and `/tmp`, where tasks
and their worktrees go.

```toml
# fragment of flock.toml
[[machine]]
name = "pi-3"
ssh = "user@pi-3"
min_disk = "4G"        # free on $HOME and on /tmp
min_memory = "0"       # no memory floor
```

The floor is off until a machine's own entry sets `min_disk` or
`min_memory`. Once one is set the floor is on for that machine, and the side
it leaves out comes from `[defaults]` in `pastor.toml` (`min_disk`,
`min_memory` there, the same sizes), else the built-in `4G` of disk and `1G`
of memory. `[defaults]` only fills in sizes; on its own it turns no floor on.
A size that does not parse, in either file, fails the load like any other
key.

`unknown_room` says how a machine whose room cannot be read counts: `room`,
the default, lets it take tasks as if it had room; `no_room` holds them. It
is read from the task's flock first, then the machine, as every setting but
the agent is.

A machine under its floor takes no new task, whether the task wants a
shared slot, a job slot or burst: dispatch reads the room the machine last
reported (`$HOME`, then `/tmp`, then memory) and passes the machine over
while one side is below its floor. A task with nowhere else to go waits in
the queue with the first such side as its reason, `waiting for a machine:
pi-3 has 1.2 GB free under /tmp` in `pastor queue` and `task describe`, and
goes once the next room reading (every minute) is back over the floor. A
machine whose room cannot be read (no reading yet, a probe that failed
before any answered, memory on macOS while the memory floor is on) counts
as having room unless `unknown_room = "no_room"` applies, and then waits as
`pi-3's room cannot be read`. A probe that fails never marks the machine
lost. A critical `--preempt` task does not pause a task on a machine under
its floor, since a pause frees a slot, not disk. `task run --now` looks at
none of this, as it looks past `max_agents`. Tasks already running on the
machine go on. The numbers show in `machine list --wide` as DISK and MEM.
A machine whose agents cannot reach the head is passed over the same way (see
[Checking a machine](#checking-a-machine)).

### Cordoning a machine

`pastor machine cordon pi-3 --reason "kernel update"` stops new work landing on
a machine, without editing `max_agents` to 0: a cordoned machine takes no new
task on any path that places one. A loose task goes to another machine, one
pinned to it waits, a pull machine's claim finds nothing, and a paused task
does not resume there until the cordon is lifted. The tasks it runs are not
touched, nor stopped or moved. `pastor machine uncordon pi-3` lifts it and
places what waited at once.

```text
$ pastor machine cordon pi-3 --reason "kernel update"
pi-3 is cordoned: kernel update; it takes no new task
$ pastor machine list
NAME   ...  ERROR
pi-3   ...  cordoned: kernel update
```

The cordon is the head's, kept in its database (not in `flock.toml`), so it
survives a restart and a reload, and `machine remove` drops it with the
machine. Cordoning again changes the reason; the time it began stays. `machine
list` shows it in the ERROR column and `machine describe` on a `cordoned`
line with the time and the reason; `--json` has a `cordon` object (`machine`,
`reason`, `since`) on the machine. The head raises `machine.cordoned` (with the
`reason`) and `machine.uncordoned`. A task waiting for a cordoned machine says
`waiting for a machine: pi-3 is cordoned` in `pastor queue` and `task
describe`.

Unlike a [room floor](#a-room-floor), a cordon is not one `task run --now`
runs past: `--now` onto a cordoned machine is refused with `machine_cordoned`,
and a `--now` task already queued waits like the others. Both commands need a
running head and are refused to an agent pastor started, like any change to
the fleet.

### Pausing a low task

A `critical` task does not have to wait behind `low` work, but only when
you ask for it: `pastor task run --priority critical --preempt` (or
`preempt = true` under a job's `[dispatch]`) marks the task, and the flag
stays on it. When a dispatch pass finds no machine with room for such a
task, job slots and burst included, it looks at the machines the task may
run on (its flock, connected, its tags, its pin) for one that would have
room once its newest pausable task is gone, takes the one with the fewest
live tasks, pauses that task and starts the critical one there, in the same
pass.

A task can be paused when it is `running`, `low`, of an agent of a kind
that resumes a session (`claude` or `agy`; see [Agent
kinds](#agent-kinds)), has one recorded (see [Reopening a finished
task](#reopening-a-finished-task)), has not said it is done (`task done`),
and did not resume from a pause in the last 10 minutes, so two critical
tasks cannot trade one low task back and forth. A `normal` task, an
opencode or codex one, a blocked or done one is never paused; nor is an agy
task on a machine with no agy hook installed, since it never recorded a
conversation to come back to (see [Agent kinds](#agent-kinds)).

Pausing goes in this order: pastor presses `esc` in the task's pane, which
interrupts its turn, then closes the pane, which ends the agent. A
worktree stays on disk. The task goes
`paused` (event `task.paused`), with no pane or workspace, and records when
and for which task (`paused: ... for t-9` in `task describe`, `paused_at`
and `paused_for` in its JSON).

A paused task waits in the queue first among the `low` tasks (first in its
level, once it has aged; see [Ageing](#ageing)) and pinned to the machine it
was paused on, whatever its pin: `pastor queue` shows it
there with `paused for t-9; machine pi-3 is full (2/2)`. When that machine is
still in the task's flock and has room for it, and the flock is under its
number there, a dispatch pass resumes it: the same steps as a
dispatch, but a worktree task goes back to its own checkout on its branch,
the agent starts on its recorded session (`claude --resume <session>` in
place of a new `--session-id`, or `agy --conversation <id>`; see [Agent
kinds](#agent-kinds)), and its prompt is a line telling it that it was
paused and to carry on; its own prompt is already in the conversation. The
task goes `running` again, with `resumed:` in `task describe`
(`resumed_at`). A resume that fails (the agent does not come up, the
checkout is gone) fails the task, as any dispatch does, and `pastor task
retry` starts it over.

On a paused task, `pastor task close` closes the row: there is no pane to
close. `--remove-worktree` opens its kept checkout in a workspace and
removes it through that, as for a closed task placed in a shared workspace.
`pastor task send` answers `task_not_live`, and `pastor task attach` refuses
it with `task_paused`: its session resumes on its own, and a second resume
in attach's pane would put two agents on one conversation.

`--preempt` below critical is refused with `preempt_needs_critical`: from
`task run`, checked against the level the task settles at (a machine or
flock may make it critical); from `task priority`, which sets the flag with
the level (`pastor task priority t-4 critical --preempt`) and drops it when
given without; and in a job file whose `priority` is written below
critical. A job's tasks keep `preempt` only when they settle at critical,
however they get there. `task retry` keeps it.

### Starting a task now

Sometimes a task cannot wait, and you know the machine it should go to is
full. `pastor task run --now --machine pi-3 "..."` starts it on `pi-3` in
the same request, past `max_agents`, job slots, burst, its flock's
number and its [room floor](#a-room-floor) there, whatever else waits. It never waits in the queue: the head
refuses it up front without `--machine` (`now_needs_machine`) or when that
machine is not connected (`machine_not_connected`), and if the machine still
does not take it (it went away meanwhile, or its agent cannot run the task's
model), the task is closed and the request answers `now_not_started`.
A [pull machine](#pull-machines) is connected once the head has heard from
it (a claim or a report) and until it is counted lost. The head cannot
start a task there itself, so the request waits for the machine's next
claim, which hands the task out first, past the machine's slots and the
number of tasks the claim asked for; the machine's serve then starts it at
once. The request answers when the claim has taken it (`starting` on that
machine). A claim comes each tick of the machine's serve (its `tick`,
default `10s`). The head waits for it 60 s at most, or the wait for a
push machine's start (three times `request_timeout`) when that is
shorter, so its answer reaches `pastor task run`, which gives up on a
request after 120 s. With no claim by then the task is closed and the
request answers `now_not_started`, saying the machine did not claim it;
a claim that fails the task instead (its workdir sets cannot be built)
answers `now_not_started` at once. It does not wait for the machine to finish
starting another task either: the two start side by side. It needs the machine's tags like any pinned task,
and it does not go with `--preempt` (clap refuses the two together): it
pauses nothing.

The task is marked: `now:` begins its NOTE in `task list`, `task describe`
adds `now: started at once, past its machine's limits` to its priority, its
JSON spec has `"now": true`, and `machine list` shows it after the count
(`4/3 now:t-9`) while it runs. It counts on the machine like any live task,
so the queue does not start more there until the count is back under the
limits.

`--now` is meant for a person at the CLI. A job cannot set it (`now` is not
a `[dispatch]` key), a `task retry` of such a task goes through the queue,
and the head refuses it from any agent's pane, an orchestrator's included,
and from an orchestrator's scripts (`now_refused`): an orchestrator that skipped the limits by habit would make them mean
nothing. That check stops an agent from using `--now` by mistake, and no
more. It reads a marker the caller sets itself (`PASTOR_TASK`, or the field
a request to `pastor.sock` carries), so an agent that wants around it can
unset the variable or leave the field out; like the rest of the
[guard](#trust-model), it is not a boundary. A head from before `--now` would
queue the task behind the limits, so the CLI refuses to send it there
(`head_too_old`).

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
agent = "claude-alt"
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
`claude-alt`, and pi-3 never gets a gpt task. An agent the task or job
named itself (`--agent`, a job's `agent`) is kept, and its kind must match
the model's (`model_kind_mismatch`).

An agent found this way takes `agent_args` only from layers whose `agent` is
that same agent: a layer's `agent_args` with no `agent` are for its default
agent, so the flock's claude args above never reach opencode.

A value is any name `agent` accepts, a definition under `[agents]` or a
built-in kind, and its kind must be the key: `agents = { opencode =
"claude-alt" }` fails the load, as does an entry for the kind of the
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
`[dispatch]`), `profile` under its `[[flock]]` entry, `profile` under the
`[[machine]]` entry it runs on, `[defaults] profile`; with none, it runs no
profile and nothing changes. `--profile` and a job's `profile` take a name,
not a template.

```toml
# pastor.toml
[defaults]
profile = "review"
```

```toml
# fragment of flock.toml
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
allowed. The pane also gets `OPENCODE_CONFIG` and `OPENCODE_CONFIG_DIR` set
empty, and `OPENCODE_CONFIG_CONTENT` set to the repo's instructions and
nothing else (below), over an `[agents]` env that sets any of them, so no
other config adds rules around the profile's.

Before it makes anything on the machine, pastor reads the machine's own
global opencode config and `~/.opencode` (`config.json`, `opencode.json`
and `opencode.jsonc` in `$XDG_CONFIG_HOME/opencode`, else
`~/.config/opencode`, and the same three in `~/.opencode`) and parses each
one as JSONC. Every top-level permission key they set — in `permission` (one
action for everything is the `"*"` key), or in the legacy `tools` block,
whose `write` and `patch` are opencode's `edit` — is denied in
`OPENCODE_PERMISSION` too, in the same object, so it stays denied under
every profile including `unrestricted`: a denied key can never be lifted,
neither by the profile's own rules nor by a narrower rule in the machine's
own config for the same key. The patterns a key had on the machine are not
kept, only the key itself; the task may end up stricter than its profile on
a tool the machine also restricted, which is safe, since it is told no
rather than run under rules nobody chose. opencode merges the object over
its config so that a key the machine already set keeps the machine's place,
ahead of everything pastor adds, and takes the last matching rule: so under
`unrestricted` on such a machine pastor sends no `"*": "allow"` of its own,
which would land after the machine's keys and lift their denies; opencode's
own default allow stands for tools the lists do not name, and what that
default asks about (a repeated tool call, a path outside the project) is
allowed by name instead. The other profiles' `"*": "deny"` can only deny
more, wherever it lands. What that object cannot reach fails the task
instead (`opencode_permissions_conflict` in its `error`): rules under an
agent or a mode of the machine's config (`agent.<name>.permission`,
`mode.<name>.permission`, or their `tools`), which opencode appends to that
agent's rules after the top-level ones, so an allow there would win over
pastor's deny; a file pastor found but could not parse (its JSONC does not
read as JSON once comments and trailing commas are stripped), since pastor
then cannot say what it would let through; and more config than pastor
reads, 48 KiB over all six files, a bound under the 64 KiB a probe's answer
may be, so a large config is a refusal and never a lost machine. The
managed config (`/etc/opencode`, on macOS `/Library/Application
Support/opencode`) and, on macOS, the MDM preferences
`ai.opencode.managed.plist` in `/Library/Managed Preferences/<user>` and
`/Library/Managed Preferences` (read through `plutil`, one it cannot
convert counting as unreadable) are merged after `OPENCODE_CONFIG_CONTENT`,
so pastor cannot neutralise them the same way: a `"permission"` key
anywhere in them, a legacy `"tools"` one, or a file pastor could not read
still fails the task (`opencode_permissions_conflict`), naming every file
and key it found, or `unreadable` for a file it could not read. Move them
out, or run the task without a profile. A `command` machine cannot be
checked, and goes ahead. pastor turns the repo's own opencode config (an `opencode.json` or
`.opencode/` in the checkout) off for a profiled task
(`OPENCODE_DISABLE_PROJECT_CONFIG=1`), so a branch cannot add rules to the
profile's, and passes the checkout's instructions back by path in
`OPENCODE_CONFIG_CONTENT`: its `AGENTS.md`, or when there is none its
`CLAUDE.md`, the one file opencode would have read itself. On a `command`
machine, which cannot be asked, both go.

Separately from a profile, an opencode agent reading a path outside its
`--repo` raises opencode's "Access external directory" dialog and stops
there; a permission rule for external directories (opencode's
`permission.external_directory`, set in the machine's own opencode config,
outside pastor) avoids it. Such a rule should cover every directory a task
may read, not only its checkouts: a common parent of them (such as `~/src`)
and any other reference directory, such as a notes vault, an agent also
opens, so the dialog never blocks a task that reads both.

A Codex agent (kind `codex`) starts with `--ask-for-approval never
--sandbox workspace-write` after its args: it never stops to ask, and its
commands run in a sandbox that may write only the workspace, with no
network. Under `unrestricted` the sandbox is `danger-full-access`. Codex has
no per-command allow or deny flag, so the sandbox stands in for the lists: a
list it has no `allow_flag` or `deny_flag` for is not passed, and not
refused either (`agent_tools_unsupported` is only for a task with no
profile). `pastor task describe` shows such a list with `(not applied: ...)`
after it. Agent args that pick an approval policy or sandbox
(`--ask-for-approval`, `-a`, `--sandbox`, `-s`, `--full-auto`,
`--dangerously-bypass-approvals-and-sandbox`, `--yolo`, or a `-c` of
`approval_policy` or `sandbox_mode`) are refused while a profile applies
(`profile_args_conflict`). A task that needs the network, such as a review
that calls `gh`, adds `-c sandbox_workspace_write.network_access=true` to
its agent args; the profile does not turn it on.

An agent of any other kind gets the lists through its `allow_flag` and
`deny_flag`, as any list, and keeps its own permission mode.

An agy agent (kind `agy`, the Antigravity CLI) takes no policy at launch:
its rules live in its settings on the machine, `permissions.allow`,
`permissions.deny` and `permissions.ask` in `~/.gemini/antigravity-cli/settings.json`
and in the config it shares with the desktop app,
`~/.gemini/config/config.json`, and nothing on its command line or in its
environment carries rules. So a task under a profile starts with
`--dangerously-skip-permissions --mode accept-edits` after its args, which
approves every `ask` and skips the diff review before a file write; a
`deny` still holds under them. Before it makes anything on the machine,
pastor reads those two files there and checks that each pattern the task
denies is covered by a deny: `Edit`, `Write` and `NotebookEdit` need
`write_file(*)`, `Bash(git push:*)` needs `command(git push)` (a prefix by
words, so `command(git)` covers it too), `Read(~/.ssh/**)` needs
`read_file(~/.ssh)`, `WebFetch` needs `read_url(*)`, and `mcp__a__b` needs
`mcp(a/b)`; the action's `*` covers all of it, and a `read_file` deny of a
path covers a `write_file` of it. A `regex:` rule, an `ask` and an `allow`
cover nothing. What is missing fails the task before it starts
(`agy_permissions_missing` in its `error`), with the rules to add, one per
line. So does a machine whose files pastor cannot read, or that takes no
shell command (a `command` machine): unlike opencode's check, this one never
goes ahead, since going ahead would be the skip flag with nothing verified
behind it. A pattern agy has no rule for (`Glob`, `Grep`, `WebSearch`,
`Task`, an unknown tool) is refused when the task is queued
(`agy_deny_unsupported`). agy's project files under
`~/.gemini/config/projects/` are not read: agy applies only the one for
its active workspace, which pastor cannot tell, so a deny there does not
count.

What the profile allows is not passed: agy has no allow list to take, and
under the flag everything its settings do not deny runs, a shell command
that writes a file included. A `review` task on agy therefore never asks,
never uses an edit tool, never runs `git push`, `rm -rf` or `sudo`; it is
not a read-only checkout, as a Codex one under `review` is not either.
`pastor task describe` shows the lists with `(agy takes no lists: the deny
was checked against its settings on the machine, the allow is not passed)`
after them. Agent args that set `--mode` or `--dangerously-skip-permissions`
are refused while a profile applies (`profile_args_conflict`).

Those rules are one file per machine, so an agy machine runs one profile at
a time: `review` needs `write_file(*)` denied, which stops every edit there,
the owner's own agy sessions included. Give each agy machine its profile in
its `[[machine]]` entry (or its flock's, or `[defaults]`), write that
profile's rules into its agy settings, and run its tasks under that
profile. A task that asks for a profile the machine's own does not cover
(the task denies a pattern the machine's profile does not) is refused when
pinned there (`agy_profile_mismatch`), and skips that machine when not; a
machine with no own profile is tried, and the check above decides. For the
built-in `review` the deny list on the machine needs at least:

```json
{
  "permissions": {
    "deny": ["write_file(*)", "command(git push)", "command(rm -rf)", "command(sudo)"]
  }
}
```

and `develop` needs `command(rm -rf)`, `command(sudo)` and
`command(git push --force)`, not `command(git push)`, which would stop the
pushes `develop` allows.

Pastor writes them itself, as the machine's agy provisioner, and not only
the denies: the whole permission object. Before each launch of an agy agent
under a profile (one without `own_permissions`) it makes agy's global
settings, `~/.gemini/antigravity-cli/settings.json`, say what the machine's
own profile says: its `[[machine]]` `profile`, else its flock's, else
`[defaults] profile`, not the task's. It sets

| Key | From |
| --- | --- |
| `permissions.deny` | the profile's `deny`, each pattern as the agy rule it needs (`Edit` is `write_file(*)`, `Bash(sudo:*)` is `command(sudo)`, `Read(~/.ssh/**)` is `read_file(~/.ssh)`); a pattern agy has no rule for, such as `Glob`, adds nothing |
| `permissions.allow` | the profile's `allow`, the same way; a pattern agy would state more widely than Claude does is left out: `Bash(git log)` is exact in Claude and a prefix in agy, so only `Bash(git log:*)` becomes `command(git log)` |
| `permissions.ask` | emptied, if the file has one |
| `toolPermission` | `always-proceed`, or the profile's `tool_permission` |
| `agentMode` | `accept-edits`, or the profile's `agent_mode` |
| `allowNonWorkspaceAccess` | the profile's `allow_non_workspace_access`, if set |
| `queuedMessages` | the profile's `queued_messages`, if set |

The first three lists are replaced whole: a rule the profile no longer has
goes on the next launch, and so does one agy saved from an "always allow"
answer. Every other key (`trustedWorkspaces`, `model`, the colours) keeps its
value and its place, and so does a key of `permissions` not listed above. A
profile sets the last four in its own `agy` table:

```toml
# pastor.toml
[profiles.worker]
extends = "develop"

[profiles.worker.agy]
tool_permission = "always-proceed"      # or "request-review"
agent_mode = "accept-edits"             # or "request-review", "auto"
allow_non_workspace_access = true
queued_messages = "send-immediately"    # or "queue", agy's default
```

A word agy does not know fails the load of pastor.toml, as does an unknown
key in the table. A nearer profile's key wins over the one it `extends`.
`pastor profile describe` shows the table.

A file that already says all of it is not written. Otherwise it is put back
in one rename, after a check that it is still what pastor read, and a missing
file is created. agy can save a copy of its settings from before pastor's
edit, so pastor trusts no past write and does this before every such launch.
When agy saved the file between pastor's read and write, pastor reads it
again and merges once more. A machine with no profile is not read. A task
under a narrower profile than its machine's needs rules pastor does not write
there: writing a `review` task's `write_file(*)` would stop every edit on a
`develop` machine, the owner's own agy sessions too. Give each agy machine
the profile its tasks run, in its `[[machine]]` entry.

Since the lists are the profile's, hand-written agy rules do not survive a
launch on a machine with a profile: put them in the profile instead.

When the machine's own profile denies something and pastor cannot read or
write the file (a `command` machine, a file that is not JSON or whose
`permissions` is not an object, a symlink, which pastor never replaces, or a
write that fails), the task fails before anything is made on the machine with
`agy_permissions_missing`, the reason, and the deny rules to add by hand, one
per line:

```text
agy_permissions_missing: ~/.gemini/antigravity-cli/settings.json is a symlink, which pastor does not replace; machine pi-1's own profile needs these deny rules in ~/.gemini/antigravity-cli/settings.json there:
  command(rm -rf)
  command(sudo)
```

A profile that denies nothing (`unrestricted`) never fails a task over this:
no rule is missing that could matter, since the launch approves every ask, and
pastor only logs that it could not write.

An agent whose permissions pastor should leave alone says so with
`own_permissions`:

```toml
# pastor.toml
[agents.agy-builder]
kind = "agy"
own_permissions = true
```

A task of that agent under a profile starts with its args alone: pastor
adds no permission flag and no mode, runs no check, and the profile's lists
are not passed. The agent's own settings on the machine decide what it may
run, so they must hold at least what the profile would, and agy with it
asks before whatever they do not allow. `pastor task describe` shows the
lists with `(not applied: agent agy-builder's permissions are in its own
settings on the machine)` after them. The field cannot stand beside an
`allow_flag` or `deny_flag`, and without a profile the lists still need
flags. The unrestricted rule below holds for it as for any agent. To give
it to some machines only, define it under its own name and name it on those
machines (`agent = "agy-builder"`, or `agents = { agy = "agy-builder" }`
for a model of kind agy).

Patterns are passed as written. Claude reads a `~/` path in a pattern
(`Read(~/.ssh/**)`) as the home of the machine it runs on, so pastor does not
expand it, and a machine that cannot report its home (a `command` one) runs
such a profile too.

**The unrestricted rule.** A task may ask for `unrestricted` (with
`--profile` or a job's `profile`) only on a machine whose own profile is
`unrestricted` as well: its `[[machine]]` entry says so, or, when it names
none, its flock's, or `[defaults]`. Here the machine comes first, though a
task's own profile takes the flock's before the machine's: a flock's
`unrestricted` never lifts a machine's narrower one. Denying nothing
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
the machine's own profile (its `profile`, else its flock's, else
`[defaults]`, as the unrestricted rule reads it), and `flock describe` the
flock's own.

### The queue

`pastor queue` lists the queued tasks in the order dispatch takes them:

```
POS  TASK  LEVEL   WHERE          FROM         WAITED  WHY NOT YET
1    t-9   high    flock work     job triage   4m      flock work is full
2    t-7   normal  machine pi-3   task run     1h      machine pi-3 is full (2/2)
3    t-8   low     flock default  task run     2d      next pass: pi-1 has room
```

POS numbers the whole queue, WHERE is the machine the task is pinned to or
else its flock, FROM is `task run` or `job <name>`, and WAITED is how long
since it was queued. A task that has aged shows the level it had before
beside its own, as `high (was low)` (see Ageing above). WHY NOT YET
plays a dispatch pass through on the machines as the head sees them, each
task that fits taking its slot, so a task behind the last free slot reads its
flock as full: `flock <f> has no machines`, `no machine in flock <f> is
connected`, `no machine in flock <f> has tags ...`, `flock <f> is full`,
`machine <m> is full (n/max)`, `is not connected`, `is not in the flock`, `is
in flock <g>, not <f>` or `lacks tags ...`, the model note `task describe`
shows (`waiting for a machine: ...`), or `next pass: <m> has room` for one
the next pass will start. A flock's number adds two: `flock <f> is at <n> of
<number> on <m>` when it is at its max there, and `flock <f> is past its
share on <m>, ... while flock <g> waits under its share` when another flock
under its share goes first. A pull machine (see Pull machines, under A
headless serve) is never pushed to: a task pinned to one waits on its slots and
its flock's seat there, as the machine's own claims take it, so it reads
`machine <m> is full (n/max)`, `flock <f> is at <n> of <number> on <m>`, `is
not connected` only while the machine has never claimed or is counted lost,
or `next claim: <m> has room` for one its next claim takes. A loose task never
reads a pull machine as where it goes next, since whether the machine takes
flock work is its own serve's setting, and a flock with only pull machines
reads `flock <f> has only pull machines; a loose task waits for one that
takes flock work`. A paused task reads `paused for t-N;` and then
why its machine can't take it yet, or `next pass: resumes on <m>`. `--flock <f>` keeps the tasks waiting in a flock,
`--machine <m>` the ones pinned to a machine, both keeping their POS in the
whole queue; `--json` gives each as `pos`, `id`, `priority`, `aged_from`, `where`,
`flock`, `machine`, `from`, `waited_secs`, `why` and the whole `task`. With
no head running it shows the store's queue, and each WHY NOT YET says so.

`pastor queue move <task>` puts a queued task elsewhere, with one of
`--top`, `--before <task>`, `--after <task>` or `--to <n>` (a POS; past the
end is last). `queue move` ignores paused tasks: while one is paused, `--to`
and the answer count the queue without it. The task takes the level of where it lands: moved in front of
a higher task it is lifted to that level, moved behind a lower one it is
lowered to it, and between two of its own level it keeps its own. `--top`
has no task in front, so it only lifts. The level's queued tasks then share
out the positions they held between them in their new order, so the task
sits between its new neighbours and a task queued later still goes last. The
queue is one order across flocks: a task can go before a task of another
flock, and it only matters against the tasks of its own. The answer says
where the task is now, and when the level changed, from what to what:

```
$ pastor queue move t-8 --top
t-8 is 1 of 3 in the queue; lifted from low to high
```

`task describe` then names `queue move` as what set the level. `--json`
prints `pos`, `of`, `priority_was` and the `task`. A task that is not queued
or is being sent to a machine, or a `--before` or `--after` task that is not, is refused with `not_queued`,
an unknown one with `task_not_found`, and an agent pastor started, as for
`task priority`, with `agent_refused`; reading the queue is fine from
anywhere.

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
below), `pastor events` asks the head instead: it pages
from the start of the head's log, 500 records at a time, and with `--follow`
asks again every second. `--task` and `--json` work as they do here, and the
output is the same, except that lines written before pastor numbered records
are not shown. When records the command had not read yet were rotated out of
the head's log (`gap: true`), it prints one line to stderr naming the missing
numbers and carries on from the oldest record left; a first read of a log
that has rotated says so too.

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
  "machine": null,
  "summary": {"round": 1, "outcome": "done", "text": "done: PR #31",
              "source": "agent", "at": "2026-09-24T10:15:01.900Z"}
}
```

- `seq`: the record's number, from 1. It only grows, never repeats, and
  carries on across restarts of the head and rotations of the log (the last
  one given is kept in the head's database). Lines written before pastor
  numbered records read as 0, and so does a record built after that if the
  head's database cannot hand out a number for it; consumers must not assume
  every new record has a positive `seq`.
- `at`: when the daemon received the event, RFC 3339 UTC.
- `type`: `task.queued|running|blocked|done|stale|failed|closed|paused`,
  `task.limited` (its agent stopped on a usage limit, or signed out; the
  detail is `agent.exhausted`'s or `agent.signed_out`'s), `task.waiting` (it waits for the reset),
  `task.agent_switched` (it moved to its next model) and
  `task.rate_limited` (a 429 or 529 it retries in its pane; see Usage
  limits),
  `task.input` (`pastor task send`, or pastor at Claude's limit picker, or at a Codex prompt left in its input),
  `task.trusted` (pastor answered a trust prompt: the head, or a pull
  machine's serve), `task.answered` (the head answered another of the
  agent's startup dialogs), `job.failed`, `connector.finish_failed` (a connector's
  `[finish]` command failed), `machine.connected`, `machine.lost`,
  `machine.check_failed` (a row of `pastor machine check` failed),
  `machine.bootstrapped` and `machine.bootstrap_failed` (a bootstrap report),
  `machine.joined` and `machine.join_token_expired` (see Joining with a token),
  `bundle.applied|failed|drift|conflict` (see Keeping bundles),
  `workdir.placed|failed|removed|drift` (see Through the head and Kept and checked),
  `workdir.skipped` (a task started without its stored workdir bundles; see
  Stored workdir bundles),
  `agent.exhausted` and `agent.reset` (see Usage limits),
  `agent.signed_out` and `agent.signed_in` (see Signed-out agents),
  `login.pending` and `login.closed` (see Signing an agent in), and
  `orchestrator.started|skipped|held|quota|failed|restarted|stopping|stopped`
  (see Orchestrators).
- `task`: the full task row at that moment, as stored: `pastor task describe
  --json` without the `model`, `profile` and `description_from` it adds, and
  with `description` null when none was given (the record's own `model`
  carries the model), on `task.*` events; `null` otherwise or if the row is gone.
  `task.flock` is the task's flock, so a hook can route work and personal
  notifications apart.
- `job`: the job name. For a task event it is the task's `job` (`run` for a
  one-off `pastor task run` task); for `job.failed`, the job that failed.
- `machine`: on `machine.*` events, the machine's status as an entry of
  `machines` in `pastor machine list --json` shows it (`name`, `host`,
  `endpoint`, `channel`, `herdr_version`, `pastor_version`, `protocol`,
  `error`, `live`, `max_agents`, `tags`, `flock`, and the rest of what
  `machine list --json` has: `flocks`, `job_slots`, `burst`, `live_jobs`,
  `orphans`, `profile`, `description`, `shutting_down`); `null` on other
  events.
  A task's machine is `task.machine`.
- `detail`: only on events that carry more, and absent otherwise. On
  `task.input`, `keys` (the key names pressed, Enter included), `text_len`
  (the length of any text), `trust` (sent by `--trust`), `pick` (the
  option text given to `--pick`),
  `limit_picker` (pastor answered Claude's limit picker), `rate_retry`
  (pastor retried a short limit), `submit` (pastor pressed Enter on a Codex
  prompt left in its input) and `socket` (the text went to a Claude
  session's inbox socket instead of being typed; `keys` still lists the Enter of a `task send` or a stop message, and is empty for a rate retry); on `task.rate_limited`, `line`, `attempt`
  and `retry_at`; on
  `task.trusted`, `keys`; on `task.answered`, `dialog` and `keys`; on a
  `task.blocked` for an agent that ended its
  turn on a question, `question`; on `connector.finish_failed`, `connector`
  (its id) and `reason` (why: the exit status and stderr tail, or a timeout);
  on `machine.bootstrapped`, `isolation` and `missing`; on
  `machine.bootstrap_failed`, `step` and `why`;
  on `machine.joined`, `id` (the token's), `flock`, `tags`, `replaced` and
  `generation`; on `machine.join_token_expired`, `id`;
  on `machine.check_failed`, `check` (the row's id), `agent` (the agent
  it ran for, or null), `status` (`fail` or `error`), `first_line`,
  `reason` and `provisioner`;
  on `orchestrator.*`, `orchestrator` (its name) and: `lines` (a count: how
  many lines its pre script printed) on a scheduled
  run's `started` and `by` (`hours` or `hand`) and `until` on a session's
  (whose task is the record's `task`), `reason` on `skipped` (`busy` or
  `post_pending`) and `held` (`max_orchestrators`, with `max`; `quota`, with
  `until`; or a session's `restarts`, with `max` and `until`), `until` on
  `quota`, `after` (the agent it replaces) and `restarts` (in the last hour)
  on `restarted`, `reason` (`hours` or `hand`) on `stopping` and `stopped`
  and `grace` on `stopping`, and `stage` (`pre`, `agent`, `post` or
  `state`) and `error` on `failed`, with `failures` (how many in a row) on a
  `pre` failure; on `agent.exhausted` and `agent.reset`, `account`, `model`
  (null for the whole account), `agent` and `retry_at`, with `until`,
  `hard`, `no_credit`, `what` (`5-hour limit`, `no credit`) and `line` on
  `agent.exhausted` and `by` (`time` or `hand`) on `agent.reset`; on
  `agent.signed_out`, the same as on `agent.exhausted` with `signed_out:
  true` and `machine`, the machine it was seen on, and on `agent.signed_in`
  the same as on `agent.reset` with `by` `check`, `hand` or `login` (with
  no `retry_at` when a login found no mark to clear); on `login.pending`,
  the login as `pastor login list --json` shows it; on `login.closed`,
  `agent`, `pane` and `why` (`the pane closed`, `timed out` or
  `cancelled`); the record's `task` is the task that saw the limit, when
  one did.
- `summary`: on `task.done` and `task.failed`, how the round that just ended
  ended (see Task summaries): `round`, `outcome` (`done`, `partial`,
  `blocked`, `nothing to do`, `unknown`, `no summary`, or `no head`), `text`, `source`
  (`agent` or `pane`) and `at`. Absent on other events. A hook of a connector
  that does not own the task's job gets it with an empty `text`, as it gets
  no item or prompt.

Fields may be added; none will be renamed or removed. Unreadable lines (a
torn write, a hand edit) are skipped.

A client can also read the log through the head by number; the request is
under [Internals](#internals).

## Status

`pastor status` puts the fleet on one screen: which head answered and its
version, how many machines can take work and which cannot (with their
channel), the live tasks by state, the enabled jobs and those whose last run
failed or whose file does not load, and the accounts out of usage.

```text
head      user@pi-1 (0.9.0)
machines  3 up, 1 down (pi-3: reconnecting)
tasks     1 blocked, 2 running, 3 queued
jobs      6 enabled of 7, 1 failing (nightly)
limits    claude until 14:00
```

It asks the head with the requests `machine list`, `task list`, `job list`
and `limit list` already send, so any head answers it; one that predates
usage limits shows `-` on that line (`null` in `--json`). With no head it
fails with `not_running`, since a summary of stale files would read as the
fleet's state. `--probe` takes that on: it probes each machine in flock.toml
over ssh, as `machine list` does without a head, and reads tasks, jobs and
limits from the files and store here. `--json` prints the same parts as one
object: `head`, `machines` (`total`, `up`, `down`), `tasks` (a count for
every live state), `jobs` (`total`, `enabled`, `failing`) and `limits`.

## Watch

`pastor watch` is the events log cut down to what someone watching the fleet
acts on, one line per change, for an orchestrator to wait on:

```bash
pastor watch --name night            # every minute, what changed; never returns
pastor watch --now                   # what needs attention right now, then exit
pastor watch --interval 2m --all --json --connector prs
```

```text
TASK t-12 failed pi-3 nightly outcome="no summary": agent exited
TASK t-13 done pi-2 nightly outcome=partial
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

- `TASK <id> <state> <machine> <job>[ outcome=<outcome>][: <error>]`, from
  the head's numbered events: a task that reached `blocked`, `done`, `failed`
  or `stale` (`--all`: every state change, `queued` and `running` too). `-`
  stands for a machine or job it has none of. `outcome=` is on `done` and
  `failed`, how the round ended (see Task summaries), quoted when it is more
  than a word (`outcome="nothing to do"`). The error is only there for
  `failed` and `stale`.
- `JOB <name> failing: <last result>` when an enabled job's last run failed,
  again when that result changes (`failed (1x)` becomes `failed (2x)`), and
  `JOB <name> ok` when it runs fine after that, from `job list`. A job that is
  disabled or removed is dropped without a line.
- `HEAD down: <why>` when the head stops answering, once, and `HEAD up` when
  it answers again. `HEAD gap` says the log rotated events out before the
  watcher read them; `pastor watch --now` shows where things stand. `HEAD
  reset` says the head's log ends before the watcher's cursor (the head
  moved, or its state dir was wiped): the watcher goes on from the log's end
  and never replays what is already there.
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
(`task`, `state`, `machine`, `job`, `outcome`, `reason`, `connector`, `text`) and
`line`, the text form. With a head on another machine (`pastor head set`) the
events, tasks and jobs are that head's, and the connectors run here. The
watcher only reads, so an agent pastor started may run it; through a bridge
that limits an agent to its own tasks (`bridge --agent`) the head refuses the
events, and the watcher stops with the head's error. So does one whose head
is too old to hand out events by number. A head that answers with any other error stops it
too; one that does not answer at all is `HEAD down`.

## Orchestrators

An orchestrator is an agent that drives the others: it reads what changed,
merges what is ready, sends fixes and answers what is blocked. pastor runs
orchestrators itself, from files in `~/.config/pastor/orchestrators/`, one
per orchestrator, next to `jobs/`. The head re-reads them on each tick; a
file that stops parsing keeps its last good version and shows the error in
`pastor orchestrator list`.

Each file names its `kind`, required and with no default:

- `scheduled`: a pre script runs on a schedule and does everything
  mechanical itself; the head starts an agent only when the script prints
  lines that need judgment, and gives it every line. Most runs start no
  agent, and each agent is short.
- `session`: one agent kept running through set hours, restarted when it
  dies, stopped when the hours end (see [Session
  orchestrators](#session-orchestrators)).

Keys: both kinds take `kind`, `model` (a `[models]` name), `skill` (a skill
the agent is told to use), `prompt` (required), and optionally `enabled`
(default true), `description` and `repo` (the repo its agents work in, each
in a worktree of its own; without it the agent starts in `~/pastor-tasks`,
as any task with no repo does). `scheduled` adds `every` or `cron` (exactly one), `pre`
(required), `post` and `timeout`; `session` adds `hours` (required, `{
start = "22:00", stop = "08:00" }`, local time, the two different) and
`stop_grace` (default `5m`). A file without
`kind`, with a key of the other kind (`pre` in a session, `hours` in a
scheduled one), or with an unknown key is invalid, and the error names the
key. There is no `machine` key: everything an orchestrator runs, its scripts,
its agent and the agent's worktree, runs on the head's own machine (the
`local = true` one in flock.toml).

```toml
# ~/.config/pastor/orchestrators/merge.toml
kind = "scheduled"
cron = "*/5 22-23,0-7 * * *"     # or every = "5m", as a job
pre = ["./merge-pre.sh"]         # relative to this file's directory
post = ["./merge-post.sh"]       # optional
timeout = "5m"                   # for pre and post each; the default
model = "sonnet"
skill = "orchestrating-pastor"
prompt = "Decide what to do with each line below."
description = "merges the night's green PRs"
```

A pre script, doing the mechanical part and printing the rest:

```sh
#!/bin/sh
# ~/.config/pastor/orchestrators/merge-pre.sh
# Merge what is green, send one rebase per PR, print what needs judgment.
set -eu
repo=owner/repo
gh pr list --repo "$repo" --json number,mergeStateStatus,reviewDecision \
  --jq '.[] | "\(.number) \(.mergeStateStatus) \(.reviewDecision)"' |
while read -r pr state review; do
  if [ "$state" = CLEAN ] && [ "$review" = APPROVED ]; then
    echo "merging #$pr" >&2
    gh pr merge "$pr" --repo "$repo" --merge >&2 || echo "PR #$pr merge refused"
  elif [ "$state" = BEHIND ] && [ ! -e "$PASTOR_ORCHESTRATOR_STATE_DIR/rebase-$pr" ]; then
    pastor task run "Rebase PR #$pr onto main and push." --repo '~/src/repo' --worktree >&2
    touch "$PASTOR_ORCHESTRATOR_STATE_DIR/rebase-$pr"
  else
    echo "PR #$pr merge=$state review=$review"
  fi
done
# A blocked task needs an answer.
pastor task list --json | jq -r '.[] | select(.state == "blocked") | "TASK t-\(.id) blocked"'
```

A scheduled run:

1. **Skip if busy.** While the last run's agent still works (queued,
   starting, running, blocked or paused), or its post script has not run
   yet, the run is skipped, pre script included, with
   `orchestrator.skipped`.
2. **Pre.** The head runs `pre` in the file's directory with
   `PASTOR_ORCHESTRATOR=<name>`, `PASTOR_ORCHESTRATOR_STATE_DIR` (the
   orchestrator's scratch dir, kept between runs), `PASTOR_CONFIG_DIR`,
   `PASTOR_STATE_DIR`, the `pastor` that runs the head first on `PATH`, and
   the variables of an `.env` in the orchestrators directory if there is one.
   Each line it prints on stdout is one thing that needs judgment; blank lines
   are dropped, and so are control characters other than tab. Its stderr is
   its log, kept with its stdout under
   `~/.local/state/pastor/orchestrators/<name>/runs/`, with every `.env`
   value redacted. Exit 0 with no lines ends the run: no agent. An exit other
   than 0, a run past `timeout`, or more than 64 KiB of lines fails the run (`orchestrator.failed`,
   `stage: "pre"`): its lines are dropped and the orchestrator backs off as a
   failing job does, a minute doubling to an hour.
3. **Agent.** With lines, one task with `role = "orchestrator"`, pinned to
   the head's machine, the file's `model`, and a prompt made of the file's
   `prompt`, the skill, the handover note and every line; as for every task,
   the `summary` setting adds the ask for a summary when it is sent (see
   [Task summaries](#task-summaries)), and the post script gets it. Its
   description is `orchestrator <name>: <n> lines`. When `max_orchestrators`
   orchestrator agents already work, or the orchestrator waits for a quota
   reset, no agent starts (`orchestrator.held`); the next run tries again.
4. **Post.** When that task ends `done`, `failed` or `stale`, the head runs
   `post` once, with the pre script's environment and on stdin the finish
   command's object (see [The finish command](#the-finish-command)) plus
   `orchestrator` and `lines`:

   ```json
   {
     "task": {"id": 17, "role": "orchestrator", "...": "the whole task row"},
     "state": "stale",
     "job": "run",
     "branch": null,
     "summary": {"round": 1, "outcome": "done", "text": "...", "...": "..."},
     "last_output": "...",
     "orchestrator": "merge",
     "lines": ["PR #31 merge=BLOCKED review=REVIEW_REQUIRED", "TASK t-40 blocked"]
   }
   ```

   `summary` is the task's latest summary, `null` when it has none. A post
   script that fails is logged and emitted as `orchestrator.failed` with
   `stage: "post"`, and changes nothing. A task closed before pastor saw it
   end runs no post script.

**The role.** Every agent an orchestrator starts runs with `role =
"orchestrator"` (see [Trust model](#trust-model)), and its pre and post
scripts run under the same table: the CLI sends `PASTOR_ORCHESTRATOR` with
each request, as it sends `PASTOR_TASK`, so a script may `task run`, `retry`,
`send` and `close`, `job enable` and `job disable`, keep its own note and
read everything, and anything else (`machine add`, file edits, `orchestrator
run`, `enable` and `disable`, `--role orchestrator`) is `agent_refused` or
`role_refused`. A name the head has no file for is refused every change.
When both variables are set, `PASTOR_TASK` wins, so an agent cannot widen
its rights by setting the other, and the head clears `PASTOR_TASK` for the
scripts it runs. Like the rest of the guard, it stops mistakes, not a
determined script.

**The limit.** `max_orchestrators` in pastor.toml (default 1) is how many
orchestrator agents the head runs at once, of both kinds and hand-started
ones (`task run --role orchestrator`) too, a limit on top of `max_agents`:
each orchestrator agent also takes a slot on its machine like any other task,
but never a job slot. A run that starts no agent counts nothing; a session counts from its
start to its stop, between agents too.

**The handover note.** `pastor orchestrator note <text>` keeps one short
note per orchestrator (4 KiB at most, the last one wins, empty removes it)
in `~/.local/state/pastor/orchestrators/<name>/note`. Every agent the
orchestrator starts gets it in its prompt. Its own agent and its scripts
leave the name out, and may keep only their own orchestrator's note; a
person names it with `--name`. `-` reads the note from stdin.

**Quota.** When the agent ends its turn on a usage limit, the head reads the
reset time from the message, or waits an hour when it names none, emits
`orchestrator.quota` with `until`, and starts no agent for that orchestrator
before then. The limit also goes in the head's table of exhausted accounts
(see [Usage limits](#usage-limits)), so no task starts on that account
either, and an orchestrator whose agent's account is in that table waits for
it the same way. The pre script keeps running, so the mechanical work goes on; a
session waits, holding its slot, and restarts at the reset.

The head looks at the end of the turn only: the agent's error and what
follows the last prompt in the last lines of its pane, at most the last 15
lines that are not empty. A line counts when it starts with the message
(`You've hit your session limit`, `Weekly limit reached`, `Opus weekly limit
reached`, `Claude AI usage limit reached`, `Credit balance is too low`) and is
the agent's last `●` message, a `⎿` notice that is not the output of a tool
call, a line in the first column or the banner under the input box. The same
words further up, in the middle of a line or in what a tool printed (a grep of
pastor's own source) are not a limit. An agy orchestrator is read for agy's
quota message (`RESOURCE_EXHAUSTED (code 429): Individual quota reached`) as
its last paragraph, as in [A limited task waits](#a-limited-task-waits). An
agent of another kind is restarted like one that ended. So is one that ended
on `API Error: 429` or `529`, or agy's 429 without the quota: the API was
busy, the account is not out.

The reset is a unix time (`|1759201200`), a time of day (`resets 3am`,
`resets at 15:30`), a weekday (`resets Mon 9am`), a date (`resets Oct 6,
9am`) or a wait (`try again in 2 hours 13 minutes`). A time of day is the
next such time in the zone the message names in brackets (`resets 3am
(Europe/Lisbon)`), else in the head's local time.

### Session orchestrators

For nights you want an agent watching all the time, not only when a script
finds something:

```toml
# ~/.config/pastor/orchestrators/night.toml
kind = "session"
hours = { start = "22:00", stop = "08:00" }   # local time; may cross midnight
stop_grace = "5m"                             # the default
model = "opus"
skill = "orchestrating-pastor"
prompt = "You are the night orchestrator: merge what is ready, unblock what waits."
```

- **Start.** The head starts the session at `hours.start`, or at once when it
  starts (or the file appears) inside the hours, if the file is enabled: one
  task with `role = "orchestrator"` on the head's machine, with the file's
  `prompt`, the skill, the handover note, and the ask to begin with `pastor
  watch --now` and keep watching with `pastor watch`. Its description is
  `orchestrator <name>: session until <stop>`, and its timeout reaches to the
  stop plus `stop_grace`. `orchestrator.started` carries `by: "hours"` and
  `until`.
- **Stop.** At `hours.stop` the agent gets a last message, sent as `task send`
  sends text (through a Claude session's inbox socket, else typed into its
  pane; keep a handover note, end the turn), `orchestrator.stopping`; `stop_grace`
  later, or as soon as it stops working, the head closes it,
  `orchestrator.stopped`.
- **Restart.** When the agent ends before the hours do (its pane died, it
  went stale or failed, it ended its turn, or someone closed it), the head
  closes what is left of its pane and starts another with the same prompt,
  the note as it is now and the name of the agent it replaces,
  `orchestrator.restarted`. At most three restarts in any hour: past that it
  emits `orchestrator.held` (`reason: "restarts"`) once and waits until the
  oldest is an hour old.
- **Quota.** An agent that ended on a quota error is not restarted until the
  reset read from its message (see Quota above), with `orchestrator.quota`.
- **The limit.** A session holds its `max_orchestrators` slot from start to
  stop, restarts and quota waits included. A scheduled run while it runs does
  its pre script and is held (`orchestrator.held`); a session due while a
  scheduled agent works is held (`orchestrator.held` once, `list` shows
  `held`) and starts on the first tick with a free slot.
- **By hand.** `pastor orchestrator start <name>` starts the session now,
  inside its hours or not and whatever `enabled` says, but not past the limit
  (`orchestrator_held`); it runs to the next `hours.stop`. `pastor
  orchestrator stop <name>` sends the last message and closes the agent after
  the grace, and the session does not start on its hours again before they
  next end; inside the hours with no session (held, say) it only keeps it from
  starting. `run` on a session, or `start` or `stop` on a scheduled one, is
  `orchestrator_kind`; `stop` with nothing to stop is
  `orchestrator_not_running`.
- Disabling a running session's file stops no agent; it runs to its stop.

```bash
pastor orchestrator list                 # kind, state, schedule, last and next run, agent
pastor orchestrator describe merge       # settings, note, last runs with their lines, events
pastor orchestrator run merge            # one run now, whatever the schedule and enabled
pastor orchestrator start night          # a session now, to its next hours.stop
pastor orchestrator stop night           # last message, grace, close; not again tonight
pastor orchestrator disable merge        # its agent keeps running; no more runs
pastor orchestrator note --name merge "merged #31; #32 waits on review"
```

`list` shows each file's state: `idle`, `running` (its agent works, or its
session runs), `stopping` (a session in its grace), `held` (a session due but
held by the limit, or a bad `state.json`, below), `waiting for quota`, `off`
(disabled) or `invalid` (the file never parsed); a session's next run is its
next start.
`run` ignores the schedule and `enabled`, waits for a run or post script of
the same orchestrator already going, and is still skipped while the last
agent works; it returns at once, and `describe` shows how it went. Orchestrator
tasks appear in `pastor task list` in a table of their own above the others;
`--json` keeps one array with `role`. With no head running, `list` and
`describe` read the files and the state the head left, `enable`, `disable`
and `note --name` edit them, and `run`, `start` and `stop` are refused
(`no_head`). With a head on another machine, every command goes to it.

What the head keeps per orchestrator lives in
`~/.local/state/pastor/orchestrators/<name>/`: `state.json` (last run,
failures and backoff, its last agent, the lines it was started with, a quota
wait, a running session with its restarts, the last ten runs), `note`,
`scratch/` and `runs/`.

A missing `state.json` is a fresh start. One that cannot be read or parsed
holds the orchestrator instead, since starting afresh would forget a live
agent (and start a second one beside it) and a pending post script: a
scheduled run comes back `held` with the error as its detail, no post script
or session step runs, and `start` and `stop` by hand answer
`orchestrator_held`. `orchestrator.failed` (`stage: "state"`) says why once
per distinct error, and `list` shows it `held` with the error in its `error`
field. The head never saves over the bad file; fix or remove it and the
orchestrator runs again on the next tick.

## Try it

For a fleet you'll keep, see [the recommended setup](recommended-setup.md)
first: which credentials each machine gets, and the settings that keep
unattended agents moving. The README's Install section has the other ways to
install pastor (`install.sh`, `cargo binstall`).

Each machine needs herdr 0.9 or newer with its server running. An `ssh`
machine also needs SSH access from the head without a passphrase prompt (a
key in ssh-agent won't be there for a service; use a dedicated key or
Tailscale SSH); a `local` machine needs none, since the head is that
machine, and a `command` machine needs whatever its own bridge program
requires. From a checkout:

```bash
make install                         # pastor into ~/.cargo/bin
pastor machine add pi-3 user@pi-3 --max-agents 2 --herdr   # --herdr also saves it in herdr's sidebar
pastor machine add here --local
pastor flock add work                # a second flock; the machines above stay in `default`
pastor flock join work pi-3 --max 1  # pi-3 also runs at most 1 of work's tasks
pastor machine list                  # a line about the head, then each machine: host, flock, channel, herdr, pastor, agents
pastor setup systemd                 # confirm, then install and enable --now; or just `pastor serve`
pastor task run "Fix the flaky test in ci.yml" --repo '~/work/api' --machine pi-3
pastor task run "Review the open PR" --agent-arg=--model --agent-arg=claude-opus-5-5
pastor task run "Fix the typo in README" --model sonnet   # a [models] name from pastor.toml
pastor task run "Triage the inbox" --flock work   # only work machines take it
pastor task run --prompt-file ./prompt.md --repo '~/work/api'   # a long prompt, no shell quoting
pastor task run --prompt-file ./plan.md --role orchestrator   # may run, retry, send to and close tasks and enable and disable jobs
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
pastor tick --dry-run --job hourly   # what a run would create, without creating it; --json: one entry per job run
pastor job run hourly                # fire it now
pastor task list --job hourly        # live tasks only
pastor job disable hourly
pastor task list --all               # finished tasks too
pastor task read t-1                 # recent pane output, without attaching
pastor task retry t-4                # a failed or stale task again, as a new task
pastor task priority t-5 high        # a queued task goes ahead of normal ones
pastor task run --priority critical --preempt "prod is down"  # pauses a low task if it must
pastor task run --now --machine pi-3 "prod is down"  # starts there at once, past its limits
pastor queue                         # the queue in the order it runs, and why each waits
pastor queue move t-6 --before t-5   # take t-5's place, and its level
pastor task close t-1 --remove-worktree   # close its pane and remove its worktree
pastor task close t-2 t-3 t-4           # several at once, one line each
pastor task done t-1                 # mark it done; its pane closes after close_done_after
pastor task done --summary 'done: PR #31'   # from its own pane: how it ended
pastor task done --current --summary 'done: pushed pastor/t-1, PR #31'   # from its own pane: push, summary, done, DONE, in one
pastor task run --summary require "Fix issue 12"   # fails if its agent stops without one
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

A task with no `--repo` (and no `repo` in its job) starts in `~/pastor-tasks`
on that machine, in a workspace of its own or a pane split into a shared one;
pastor makes the folder when it is missing. herdr puts a pane with no
directory wherever its focused pane is, which can be someone's unrelated
checkout, and the home itself will not do: Claude Code never saves folder
trust for the home directory, so it would ask at every start. In
`~/pastor-tasks` Claude asks for trust once per machine; answer it at the
pane, or with `pastor task send t-N --trust`, which saves the folder (the
absolute path of `~/pastor-tasks` on that machine) as trusted, as it saves a
`--repo`, so the head answers the dialog for every later task with no repo on
that machine. Claude can ask again even after it was answered: it never saves
trust for the home directory, and several Claude sessions that share one
config directory rewrite its `.claude.json` from memory, so an answer one of
them saved can be lost to another. Pastor's saved trust does not depend on
that file: it answers the dialog each time it shows. A folder that cannot be
made (a file in the way) leaves the task in the home, which is saved and
answered the same way. On a machine that cannot report its home, herdr still
decides, no folder is on record, and `--trust` answers the dialog and says it
saved nothing.

`pastor task run` takes the prompt as its argument or, instead,
`--prompt-file PATH`, and these flags:

| flag | what it does |
|---|---|
| `--repo` | where the agent works, a path on its machine (above) |
| `--flock`, `--machine` | the flock the task goes to, or the machine it is pinned to (see [Flocks](#flocks)) |
| `--agent`, `--agent-arg` | the agent and one argument for it (below) |
| `--model` | a name from `[models]` (see [Models](#models)) |
| `--fallback`, `--no-fallback` | the models it may fall back to, comma separated, or none (see [Fallback models](#fallback-models)) |
| `--priority` | its level (see [Priority and queue order](#priority-and-queue-order)) |
| `--preempt` | may pause a low task (see [Pausing a low task](#pausing-a-low-task)) |
| `--now` | starts at once on `--machine`, past its limits (see [Starting a task now](#starting-a-task-now)) |
| `--summary` | ask for a summary, require one, or not (see [Asking for one](#asking-for-one)) |
| `--profile` | a permission profile (see [Permission profiles](#permission-profiles)) |
| `--worktree`, `--branch` | a worktree of its own, on that branch; `--worktree` needs `--repo`, `--branch` needs `--worktree` |
| `--no-fetch` | do not fetch the repo's remote before the worktree is made (see [Worktrees](#worktrees)) |
| `--tag` | only a machine with this tag takes it; repeat for more, and it needs them all |
| `--load` | the kind of work it is; only a machine that takes it places it (see [Loads](#loads)) |
| `--timeout` | marks the task stale once it has run this long |
| `--place` | where its pane goes (see [Where a task's pane goes](#where-a-tasks-pane-goes)) |
| `--label` | the label of the workspace pastor makes for it (see [Workspace labels](#workspace-labels)) |
| `--role` | what its agent may change through the head, `agent` or `orchestrator` (see [Trust model](#trust-model)) |
| `--description` | one line on what it is about (see [Descriptions](#descriptions)) |
| `--json` | JSON output |

`--agent-arg` hands one argument to the agent; repeat it for more, in order.
It always takes the next word as its value, even one that starts with a
dash, so `--agent-arg --model --agent-arg claude-opus-5-5` and
`--agent-arg=--model --agent-arg=claude-opus-5-5` mean the same thing. The
`=` form reads better. There is no single-string form, since splitting on
spaces breaks arguments that hold one. A job file's `agent_args` does the
same for its tasks. When neither says anything, the machine's `agent_args`
apply, then the flock's, then `[defaults] agent_args` in pastor.toml (see [A
flock's or a machine's agent](#a-flocks-or-a-machines-agent)); a job file
that sets `agent_args = []` opts out of all three. `pastor task describe
t-1` prints the agent and args a task was started with.

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

### Reopening a finished task

`pastor task attach t-N` on a task whose pane is still there lands in it, as
always. Once the task is `closed` or `failed`, pastor looks on the task's
machine first: while herdr still lists the task's agent, attach goes to it.
Otherwise, for a task that recorded a session (Claude's `--session-id`, or
an agy conversation id read back from herdr; see [Agent
kinds](#agent-kinds)), it opens a new workspace on that machine, labelled
`t-N-resume`, in the task's directory (its worktree, else its repo, else
the directory dispatch recorded for it: `~/pastor-tasks`, or the machine's
home if that folder could not be made), with the env of the task's agent
definition (so `CLAUDE_CONFIG_DIR` points at the same account's sessions),
runs `claude --resume <session>` or `agy --conversation <id>` there
(`task::resume_flag`) as the agent `t-N-resume`, and attaches. Attaching
again while that pane is open goes back to it.

The pane is yours, not the task's: the task's state does not change, and
closing the pane leaves no trace on it. Each files its session by
directory, so a worktree removed at close is first put back at the same path
on the task's branch (`git worktree prune`, then `git worktree add <path>
<branch>`, in the task's repo); when the branch is gone too, attach fails with
`branch_gone` and opens nothing. When the worktree can't be put back, or the
directory is gone, attach fails with `reopen_failed`. A task of another kind (opencode, codex), a
Claude task that recorded no session, or an agy task with none recorded
(a machine with no agy hook installed, or one that has not reported
anything yet) has nothing to reopen: attach fails with `no_agent`, as
before, and says so. A `command` machine has no terminal, so attach
refuses it with `no_terminal` either way.

## Run the head

`pastor serve` starts the head in the background and returns once it answers
a ping, printing its pid and its log. The head runs in a session of its own,
so closing the terminal or pressing ctrl-c there does not stop it, and it
logs to `~/.local/state/pastor/serve.log`, rotated at 10 MiB to `serve.log.1`,
`serve.log.2` and `serve.log.3` (the oldest is dropped). With a head set on
another machine it starts a headless serve the same way (see [A headless
serve](#a-headless-serve)), and passes `--head` on when given one. If a head
already answers on the socket it says so (`head_running`,
`shepherd_running`) and starts nothing; if the new head exits before it
answers, `pastor serve` fails with `serve_failed` and the head's own error,
and after 30 seconds without an answer with `serve_slow`.

`pastor serve --foreground` (`-f`) is the head in this terminal, logging to
stderr until ctrl-c, SIGTERM or SIGHUP: what a service runs, and what to use
when you want to watch it. A `pastor serve` that a service manager started
stays in the foreground without the flag, so a unit written before
`--foreground` existed keeps working: pastor looks at the process that
started it, and a `systemd` manager (the user one or pid 1), launchd on
macOS, or any other pid 1 counts as a service.

`pastor serve status` pings the socket and says whether a head (or a
headless serve) runs here: its pid, its version, and whether it runs under a
service, in the background with its log, or in a terminal; `--json` prints
`running`, `role` (`head` or `headless`), `pid`, `version`, `protocol`,
`service` (`systemd`, `launchd`, `init` or null), `log` and `socket`. Nothing
running is `not_running`, and something on the socket that does not answer
is `head_unresponsive`. The head writes what it knows about itself to
`serve.json` in the state dir as it starts, and status believes that file only
when its pid is the one holding the socket; a head started by an older
pastor has none, and its service is `unknown`.

`pastor serve stop` sends SIGTERM to the process holding the socket and waits
up to 30 seconds for it to exit; agents keep running, as with any stop. With
nothing running it says so and succeeds. A head that runs under a service is
refused (`service_managed`), since `Restart=always` and `KeepAlive` would
start it again: stop it with `pastor setup systemd --stop` or `pastor setup
launchd --stop`.

A head whose flock has `ssh` machines needs `head_address` in pastor.toml:
the ssh destination those machines reach the head by. Their agents get it
as `PASTOR_HEAD` and send `pastor task done` there. Without it they have no
way back: `task done` in such a pane fails with `head_unknown`, the head
logs a warning at start and on each reload, `machine list` and `machine
describe` mark those machines, and a round that ends there with no summary
reads `no head` (see [Task summaries](#task-summaries)). `pastor setup
systemd` (or `launchd`) sets it for you, below; a head started only with
`pastor serve` does not, so set it with `pastor config edit`.

## Run under systemd

`pastor setup systemd` installs `contrib/systemd/pastor.service` to
`~/.config/systemd/user/`, shows what it will do, and only continues after
you type `yes`. `--yes` (`-y`) skips the prompt; it is required when stdin is
not a terminal (a script, a task, `ssh host pastor setup systemd --yes`),
where setup fails at once rather than wait for an answer. With no action
flag it runs `systemctl --user enable --now` on the unit. `--enable`,
`--start`, `--enable --now`, `--enable --start` and `--stop` map to the same
`systemctl --user` actions after the unit is written. `pastor setup systemd
--herdr` does the same with `herdr.service` (the herdr server) and belongs on
every machine in the flock.

`pastor.service` runs `pastor serve --foreground`; a unit from before that
flag, with a bare `pastor serve`, keeps its head in the foreground too (see
[Run the head](#run-the-head)), but re-run setup to bring it up to date.
Both units always restart (`Restart=always`, so a head killed by a stray
signal comes back) and log to the journal (`journalctl --user -u pastor`);
`systemctl --user stop` still stops one for good, since systemd does not
restart after an explicit stop.

Setup points `ExecStart` at the binary it finds (the running pastor, or
`herdr` on PATH) and copies your shell's `PATH` into the unit, so `ssh`,
`herdr` and the agents resolve under systemd the way they do in a terminal;
re-run it after moving a binary. Entries anyone could plant a binary in are
left out and named on stderr: an empty or relative entry (`.`,
`node_modules/.bin`), a world-writable directory and one that does not exist,
since someone else could create it later. `herdr` is looked up in the `PATH`
the unit keeps, and setup fails if it is only in an entry left out. A value
with a line break is refused, since it would start a new directive, and a `$`
in the binary's path is written `$$`, since systemd expands it in
`ExecStart`.

`pastor.service` also gets the config, state and data dirs this run
resolved, as absolute `PASTOR_CONFIG_DIR`, `PASTOR_STATE_DIR` and
`PASTOR_DATA_DIR`, so the head uses the same dirs as the shell that set it
up, whether they came from an override, an XDG variable or the default.

A unit that differs from what setup would write is kept as
`<unit>.service.bak`, and a running service is not restarted, since
restarting herdr stops its agents: run `systemctl --user restart pastor` (or
`herdr`) yourself.

`pastor.service` runs with `NoNewPrivileges=yes`, `UMask=0077`,
`LockPersonality=yes` and `RestrictRealtime=yes`: what works in a user unit
without user namespaces and leaves pastor's own writes alone.
`ProtectSystem=strict`, `PrivateTmp` and `ProtectHome` are left off: they would
stop ssh updating `known_hosts` and connectors writing their caches, hide a
state dir under `/tmp` or `~/.ssh`, and on a host without unprivileged user
namespaces keep the unit from starting. `pastor.service` also sets
`OOMPolicy=continue`, so the kernel killing a child the head spawned (a merge
orchestrator's `make check`, say) does not stop the head itself along with it.
`herdr.service` has no hardening, since the agents it runs need what the
user's own terminal allows, but it sets `OOMPolicy=continue` too: an agent's
own build, killed for memory, must not take herdr down and fail every other
agent's task with it. ssh runs
with `BatchMode=yes`, so it cannot ask for a passphrase under systemd: add
`Environment=SSH_AUTH_SOCK=...` for an agent, or use a key without one.

Before it starts the head's unit, `pastor setup systemd` settles
`head_address` (see [Run the head](#run-the-head)). One already in
pastor.toml is kept and named. Otherwise, at a terminal, setup asks for it,
offering `<user>@<this host's name>` (`$USER` and the kernel's hostname) as
the default that an empty answer takes; with `--yes`, or with no terminal,
it takes that default without asking. Either way it writes the value into
pastor.toml, leaving the rest of the file as it was, and prints the value
and whether it came from you or from the host's name. An answer that is no
ssh destination is refused before anything is written. When the host's name
or the user is unknown and nobody is asked, nothing is written and setup
says the address is unset. A machine whose head is elsewhere (`pastor head
set`) runs a headless serve and is not asked, and neither is `--herdr`.

A user service stops at logout unless lingering is on. Setup checks
`loginctl show-user` and prints `loginctl enable-linger` when it is off.
For `pastor.service` it also sets the config and state dirs to 0700 and the
socket and connector `.env` files to 0600, and says what it changed.

## Run under launchd (macOS)

`pastor setup launchd` is the macOS counterpart, with the same flags, the
same ask-first prompt and the same `head_address` step. It writes `contrib/launchd/pastor.plist` to
`~/Library/LaunchAgents/pastor.serve.plist` (label `pastor.serve`; with
`--herdr`, `pastor.herdr` from `contrib/launchd/herdr.plist`), pointing
ProgramArguments at the binary it finds and copying your shell's `PATH` (and,
for pastor, the three absolute `PASTOR_*_DIR` values) into
EnvironmentVariables. pastor's agent runs `pastor serve --foreground`. Output goes to `~/Library/Logs/<label>.log`. The actions
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

## Bootstrapping a machine

`pastor bootstrap` sets up the machine it runs on for a flock, as the user
pastor logs in as there. It needs no root and calls no `sudo`: it installs no
OS package, makes no user and edits no system file. What only the machine's
owner can add, it reports, and stops.

There are two ways to run it. The head can do it over ssh with `pastor machine
add pi-3 me@pi-3 --bootstrap` (or `pastor machine bootstrap pi-3` for a machine
already in the flock; see [Bootstrapping from the head](#bootstrapping-from-the-head)),
or the machine's user runs `pastor bootstrap` there by hand, or `pastor join` with a
token from the head (see [Joining from the new machine](#joining-from-the-new-machine)),
which is how a [pull machine](#pull-machines) is set up, since the head never
connects to it.
Both end in the same report, which the head keeps as the machine's state. This
section is the whole path, from what the machine's owner provides to taking the
machine out again; each part links the section that describes it in full.

### What the machine's owner provides first

pastor never installs an OS package, turns on lingering, makes a user or edits a
system file, and the commands above never call `sudo`: what only root can do,
the machine's owner does before the first run, and a step that finds it missing
stops with the exact fix.

- A login user the head can reach with ssh by key, without a password, for the
  push. That user owns everything pastor puts on the machine, under `~/.local`
  and `~/.cache`.
- Linux with systemd, or macOS, on x86_64 or aarch64 with a 64-bit userland.
- The commands `git`, `ssh-keygen`, `curl` and `tar`, and on Linux the CA
  certificates (`/etc/ssl/certs`).
- On Linux, lingering for that user (`loginctl enable-linger <user>`, as root),
  so herdr's user service keeps running with nobody logged in, and a session
  that has a user bus (a login, or ssh once lingering is on).
- Once bootstrap is done, what is the owner's by nature: a login for each agent
  CLI the flock's agents run (`claude auth login`, and the like; see
  `pastor machine check`), and each repo the flock's tasks use cloned at the same
  path as on the head, with its trust saved (`pastor trust add`).

`pastor machine check pi-3` lists what is still missing, row by row, without
changing anything.

### Isolation

The report says `isolation: none`, the only value there is: agents on the
machine run as the login user that bootstrapped it. The machine key and the
agent key bootstrap makes (`machine_key` and `agent_key` in the user's state
dir) are that user's files, so an agent can read them. With them it can ask the
head what those keys are locked to and nothing more: what a machine's agent
bridge and shepherd bridge pass on (see [Pull machines](#pull-machines)), for
that machine alone. That is its own tasks, its flock's task list, claims and
reports for its own machine, and the events about it; it cannot name another
machine, and it cannot change the flock. It can also read everything else the
login user can. To keep agents from the machine key, run them as another user:
add that user as an ssh machine (see [Trust model](#trust-model)), whose own
bootstrap makes its own keys.

### Upgrading

Every step checks before it acts, so running it again is the upgrade:

- `pastor machine bootstrap pi-3` from the head copies the head's pastor and
  herdr, runs the machine's steps again and leaves the machine `ok` or says
  which step stopped; or the user runs `pastor bootstrap` on the machine with
  the new pastor, which copies that binary to `~/.local/bin/pastor` when it
  differs. Neither replaces a pinned host key.
- herdr and the harnesses are kept at the versions pinned in `pastor.toml` and
  the flock by `pastor machine install pi-3` (see [Harness version
  pins](#harness-version-pins)); `pastor machine list` shows each machine's
  versions and `pastor machine check pi-3` the drift.
- A new pastor release goes to every machine at once with `pastor fleet
  update` (see [Updating pastor on the fleet](#updating-pastor-on-the-fleet)).

### Removing a machine, and leaving

`pastor machine remove pi-3` takes the machine out of `flock.toml` and drops
what the head kept for it: its marked lines in `authorized_keys`, its host in
pastor's `known_hosts`, and its bootstrap state (`--dry-run` lists them first).
On the machine, `pastor leave` removes what pastor put there for itself: its
marked `authorized_keys` lines and its own key and `known_hosts` files in the
state dir (see [Leaving a fleet](#leaving-a-fleet)). Neither uninstalls pastor,
herdr or a harness, or stops a service; those are the machine's owner's to
remove, and `pastor leave` only removes what pastor itself made.

```bash
pastor bootstrap --head user@head --head-key "$(cat head_key.pub)"
pastor bootstrap --name pi-3 --json   # the name the head's flock.toml uses, when it is not the hostname
```

It runs these steps in order, each checking before it acts, so a second run
does nothing new and a run after a failure carries on where it stopped:

| Step | What it does |
| --- | --- |
| `facts` | reads the OS, architecture, userland width (`getconf LONG_BIT`), init, user and home from the user database; refuses root |
| `platform` | refuses a Linux without systemd and a machine pastor or herdr ships no build for, such as a 64-bit kernel over a 32-bit userland |
| `prerequisites` | checks `git`, `ssh-keygen`, `curl`, `tar`, and on Linux the CA certificates, lingering (`loginctl show-user`) and a user bus (`XDG_RUNTIME_DIR`) |
| `pastor` | copies the running pastor to `~/.local/bin/pastor`, unless it is the same file |
| `herdr` | with `herdr` under `[artifacts]` in this machine's pastor.toml, installs that version as `pastor machine install --kind herdr` does, and leaves a herdr with task agents at work alone (skipped); without a pin, a herdr already there is enough |
| `keys` | makes the machine key and the agent key in the state dir (`ssh-keygen -t ed25519`) when each is missing, never again; with `--head-key`, puts the head's key in `~/.ssh/authorized_keys` on one line ending in `pastor:head`, which a rerun replaces and the user's own lines keep their place around |
| `client` | with `--head`, writes `[head] ssh` in client.toml (see [A head on another machine](#a-head-on-another-machine)) |
| `services` | runs `pastor setup systemd --herdr --yes`, or `launchd` on macOS, for herdr's user service; after the keys and client.toml, which a service reads as it starts |
| `harnesses` | when this machine's own flock.toml names it, installs the harnesses its agents need at their pins, as `pastor machine install` does; otherwise they are the head's to install (skipped) |
| `check` | runs `pastor machine check` here and keeps its rows; failed rows do not fail the bootstrap, and a check that cannot run is skipped; a harness that is installed but signed out puts `pastor login <machine> <agent>` in the step's `fix`, for the head to run (bootstrap never signs an agent in) |

A machine that [joined a head with a token](#joining-from-the-new-machine) runs one
more step, `shepherd`, after `services`: it writes `[shepherd] machine` in this
machine's pastor.toml and installs the user service of `pastor serve`.

A step that fails ends the run and the command exits 1 with
`bootstrap_missing` (what the machine lacks), `bootstrap_unsupported_platform`
or `bootstrap_failed`. Each failed step says why and the fix: for lingering,
`loginctl enable-linger <user>`, which the machine's owner runs as root.

Without `--json` it prints one line per step. With it, it prints the
bootstrap report: `ok`, `isolation`, each step's `step`, `status` (`done`,
`already_there`, `skipped` or `failed`), `why`, `fix` and `missing`, the
public machine, agent and host keys, the herdr and pastor versions, and the
check. `isolation` is `none`: agents run as the login user, who can read the
machine key.

### Bootstrapping from the head

The head can run it for an `ssh` machine, over the login `ssh` target the
entry already names, as that user: `pastor machine add pi-3 me@pi-3
--bootstrap` adds the entry and sets the machine up, and `pastor machine
bootstrap pi-3` sets up (or finishes, or upgrades) one that is in the flock.
It needs a running head and `head_address` in pastor.toml (the machine's pastor
reaches the head by it), nothing runs as root, and nothing calls `sudo`. In
order:

1. The machine is on record as `pending`, so dispatch passes it over from the
   moment its entry exists (see [How it works](#how-it-works)). A machine that was `ok` stays `ok`
   until its own steps start: a failure before that changed nothing there.
2. Its ed25519 host key is pinned, once, in pastor's own `known_hosts` (the one
   in the state dir) under the name ssh looks it up by, after `ssh -G` has
   resolved the target. With `--host-key` the key it offers must be the one
   given, as a `SHA256:` fingerprint or an `ssh-ed25519 AAAA...` key; without
   it the key is trusted the first time it is seen, so give it when the
   network is not yours. A key already pinned is not scanned again.
3. The platform is read (`uname -sm`, `getconf LONG_BIT`), and pastor and herdr
   are copied from the head's artifact cache (fetched first when missing, and
   checked against their published sums) to `~/.cache/pastor-bootstrap/pastor`
   and `~/.local/bin/herdr`, each hashed on the machine before it is moved
   into place.
4. `pastor bootstrap --json` runs there (see above) and its report is read.
5. On success the machine's agent key goes on its marked line of the head's
   `authorized_keys`, locked to the agent bridge, as `machine add --agent-key`
   writes it, and the host key pastor pinned is stored with the state, which is
   set `ok`. On a failed step the state is `failed` with the step, why and
   fix, and the machine stays in the flock. Either way the head emits
   `machine.bootstrapped` or `machine.bootstrap_failed` and ends any ssh
   master it held, so the next connection is checked against the pin.

Every ssh to the machine is checked strictly against the pin, with no shared
master, so a key that changed stops the push at its first connection (`Host key
verification failed`), and so does a scan that offers a key other than the one
on record or a pin that disagrees with the record: the step is `host_key` or
`connect`, the machine is not touched, and nothing replaces the pin. A machine
that was reinstalled is removed (`pastor machine remove pi-3`, which drops the
pin) and added again. The same `machine add --bootstrap` on a machine that is
`pending` or `failed` with the same target goes on from where it stopped, and
one that is `ok` is `machine_exists`: `machine bootstrap` is the rerun. A second
push of a machine while one runs is `machine_busy`. The entry's `ssh` stays the
login user's. The head's own key (`head_key` in the state dir), when it has
one, is handed to `--head-key`; the push does not make one.

### Updating pastor on the fleet

`pastor fleet update <version>` puts that pastor release on every machine
the head reaches, and on the head last, so no machine ever runs a pastor
older than its head's for longer than the rollout. It runs on the head.

```bash
pastor fleet update 0.11.0 --dry-run    # look at every machine; change nothing
pastor fleet update 0.11.0              # the ssh machines in flock.toml order, then the head
pastor fleet update 0.11.0 --machine pi-3 [--json]
```

First it looks at every machine over the pinned ssh a push uses: its
platform and the pastor in `~/.local/bin`. The head's artifact cache must
hold the version's build for each one, already checked against its
sha256 (the update downloads nothing). A machine that cannot be reached, or
whose build the cache lacks, fails here, and then nothing changes anywhere.
`--dry-run` stops after this and says what would be updated.

Then, one machine at a time:

1. the build is copied to `~/.cache/pastor-update` and its sha256 is
   checked on the machine before anything uses it;
2. the copy must say it is that version. It goes to `~/.local/bin/pastor`
   (where `install.sh` and bootstrap put it), and the old binary is kept
   until the rest is done;
3. `pastor setup systemd --enable` (or `launchd`) runs again for each unit
   the machine has, so a unit that changed ships with the release. The
   herdr unit is rewritten for herdr's next start; herdr is not restarted;
4. pastor's serve is restarted when one runs (the unit, or a `pastor serve`
   in the background), and the update waits for it to answer as the new
   version.

If any of steps 2 to 4 fails, the old binary goes back, and the serve, if it
was restarted, is restarted on it. The first machine that fails stops the
rollout: the machines after it, the head among them, are not changed and say
so. Each machine's row says `updated`, `skipped` (with why), or `failed`
(with why), and which version it ran. A machine already at the version is
skipped.

Tasks keep running. Agents run in herdr's panes, which the update does not
touch, and their rows live in the head's store, so a head that restarts
picks them up where they were; a push machine's tasks are the head's, not
its serve's. Each row counts the machine's live tasks. A pull machine runs
its tasks through its own serve and the head does not reach it, so it is
skipped, as is a `command` machine: update those on the machine with
`install.sh`.

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
~/.local/state/pastor/runs/@<id>/        hook and finish runs, named the same way
```

`PASTOR_DATA_DIR` overrides the first. A second run in the same millisecond
logs to `<ts>-1.log`. The directory name is the connector's id
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
need pastor 0.6.0 or later; 0.5.0 rejects them as unknown keys. From 0.6.0
on, pastor checks `min_pastor_version` before the strict read, so a connector
that adds fields a future pastor introduces, and raises its
`min_pastor_version` to match, gets "needs pastor X or later" on an older one
instead of an unknown key.

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
though without their item, prompt or summary text (see [Event hooks](#event-hooks)). Start `pastor serve` from an
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
task, which no connector owns) gets the task with `item` set to `null`,
`prompt` empty and the summary's `text` empty: those hold the text of the other connector's items, which a
notifier has no need to see. Its `PASTOR_CONNECTOR_STATE_DIR` is then its own
`@<id>` scratch dir, not the job's, which belongs to the job's connector;
`PASTOR_JOB` still names the job.

```toml
# fragment of pastor-connector.toml
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
  "last_output": "...the last lines of the agent's pane...",
  "summary": {"round": 1, "outcome": "done", "text": "done: pushed fix/issue-12", "source": "agent", "at": "..."}
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
- `summary` is how the round ended (see Task summaries): what the agent said
  with `task done --summary`, or `no summary` (`no head` when the agent
  could not reach the head) and the pane's last lines;
  `null` when pastor has none.

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
`gh`.

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

## Provisioners

A provisioner is what checks and sets up a machine for agents: a
`pastor-provisioner.toml` listing, for one platform, the checks a machine must
pass (herdr, pastor and git on the PATH of a non-interactive shell, the
agents' CLIs installed and logged in) and the command that would fix each.
Four are built in, `arch`, `debian`, `macos` and `raspberry-pi-os`, from
`provisioners/<id>/` in the repository. Pastor runs nothing on a machine as
root, so the built-ins fix only pastor and the agents' CLIs; a missing OS
package such as git or gh is reported for whoever runs the machine to
install. A machine names its provisioner in
`flock.toml`, and a flock says which accounts its machines must be logged
into:

```toml
# flock.toml
[[flock]]
name = "work"
default = true
machines = { pi-3 = 2 }
accounts = ["github", "claude"]   # only these two; pastor knows how each is checked

[[machine]]
name = "pi-3"
ssh = "user@pi-3"
provisioner = "raspberry-pi-os"   # a built-in id, or one copied under the data dir
```

A `provisioner` that names no provisioner, or one whose copy does not load,
fails the flock's load with the reason, as a `profile` that `[profiles]`
lacks does. `accounts` takes `github` and `claude`, each once; the
built-ins mark only their gh checks with an account and check every agent's
CLI whatever it lists.

`accounts` can also be a map from the kind to the account `machine check`
expects the machine to be signed in as: `accounts = { github = "alice",
claude = "alice@example.com" }`. The list form names none, so it checks only
that some login exists, as before. A name is the GitHub login for `github`
and the email `claude auth status` prints for `claude`, compared without
case, and may not be empty. A machine in several flocks is right when it
matches any name they give. agy has no account here: no command of its prints
which account it is signed in to, so `machine check` cannot compare one. A machine with no `provisioner` gets the first one that fits it (see [Checking a
machine](#checking-a-machine)).

To write one, or change a built-in, put a directory with the manifest under
`~/.local/share/pastor/provisioners/<id>/` (`PASTOR_DATA_DIR` overrides it,
as for connectors). The directory name must equal the manifest's `id`. A
copy with a built-in's id takes the built-in's place, broken or not: a copy
that does not load makes the id unusable, with the reason, rather than
quietly falling back to the built-in.

```toml
id = "nixos"                          # required: [a-z0-9][a-z0-9_.-]{0,63}
name = "NixOS"                        # optional, defaults to the id
version = "0.1.0"                     # required: major.minor.patch, numbers only
min_pastor_version = "0.9.0"          # optional: refuse to load on an older pastor
description = "A NixOS machine with nix, herdr and the agents' CLIs"
authors = ["Ana <ana@example.org>"]   # optional, like the next three
homepage = "https://example.org/nixos-provisioner"
repository = "https://github.com/owner/repo"
license = "MIT"

[platform]                            # required: the machines it is for
os = "linux"                          # darwin, linux or freebsd
distro = ["nixos"]                    # optional, linux only: os-release ID values it accepts
detect = ["sh", "-c", "command -v nixos-version"]   # optional: an argv; exit 0 fits the machine

[[check]]
id = "nix"                            # required: unique here, [a-z0-9][a-z0-9_-]{0,63}
description = "nix is on the PATH of a non-interactive shell"
command = ["sh", "-c", "command -v nix"]   # required: an argv; exit 0 passes
timeout = "60s"                       # the default; bounds the command, and the fix on its own

[[check]]
id = "herdr"
description = "herdr is installed"
command = ["sh", "-c", "command -v herdr"]
fix = ["nix", "profile", "install", "nixpkgs#herdr"]   # optional: what would make it pass
needs = ["nix"]                       # optional: checks before it in the file that must pass first

[[check]]
id = "gh-login"
description = "gh is logged in to github.com"
command = ["gh", "auth", "status", "--hostname", "github.com"]
account = "github"                    # optional: runs only for a flock whose accounts list it

[[check]]
id = "claude-login"
description = "claude is logged in"
command = ["claude", "auth", "status"]
agent = "claude"                      # optional: claude, codex, opencode or agy
```

The manifest is checked when the catalog is read, and a key it does not know
is an error. It needs a `[platform]` and at least one `[[check]]`. Each
`command` and `fix` is an argv array with a program in its first place, as
is `platform.detect` when given. A check may `need` only checks before it
in the file, so the file's order is also a run order and a cycle cannot be
written. A check with an `account` runs only for a machine whose flock lists
that account; the others always. A check with an `agent` names a herdr agent
kind and runs once per agent of that kind that can land on the machine,
with that agent's own env, and its row names the agent; an unknown kind is
an error. Such a check's need on a check of its own kind is met per agent;
any other need on a kind's check, from a check of another kind or of none,
is met only when that check ran and passed for every agent. `platform.detect` runs after `os` and
`distro` match, for a provisioner that still needs to tell machines
`os`/`distro` cannot, such as a Raspberry Pi from any other Debian box.
`min_pastor_version` is checked before the strict read, as for a connector,
so a manifest for a newer pastor says so instead of naming a key this one
lacks.

### Harness version pins

A flock can name the version of each harness CLI it expects on its machines,
and `pastor` shows where a machine differs. `pastor machine install` brings a
machine to its pins (see [Installing harnesses](#installing-harnesses)); nothing
installs on its own, and a task still goes to a machine whose CLI drifts. A
Claude agent of a kind its machine pins to a version starts with
`DISABLE_AUTOUPDATER=1`, so it does not update itself away from the pin.

```toml
# flock.toml
[[flock]]
name = "builders"
default = true
machines = { pi-3 = 1, desk = 2 }
pins = { claude = "2.1.310", agy = "1.4" }

[[machine]]
name = "pi-3"
ssh = "user@pi-3"

[[machine]]
name = "desk"
local = true
pins = { claude = "2.1.309" }     # desk's own, over the flock's, per kind
```

A pin is `claude` or `agy` (the other kinds come later; any other key fails
the load of flock.toml, naming the flock or machine) and a version of digits and
dots, or `"latest"`, which pins nothing. Each dot-separated part must match:
`"1.4"` takes `1.4.7` and not `1.40`, `"2.1.310"` takes only itself. A machine
takes its primary flock's pins with its own `pins` replacing a kind's. A
change reaches a running head as any flock.toml edit does.

`pastor machine list` shows the drift in its HARNESS column. `pastor machine
check` adds a `harness-version` row for each agent of a pinned kind: `ok` with
the version and the pin when it holds, `warn` naming both and the fix
(`pastor machine install <name> --kind <kind>`) when it does not, which does
not fail the check or raise an event. A CLI that is not installed has no such row (the
kind's install row says so), nor does a `latest` pin. The version is what
`claude --version` or `agy --version` prints, read in a shell that has
`~/.local/bin` and `~/.cargo/bin` on its PATH.

### Installing harnesses

`pastor machine install <name>` installs or updates each harness the
machine's flock pins, among the kinds of the agents that can land on it, to
the pinned version. `--kind claude` or `--agent <agent>` takes one;
`--dry-run` says which source and steps it would take and changes nothing,
on the head or the machine.

```text
$ pastor machine install pi-3
pi-3
  installed      claude 2.1.309 -> 2.1.310 from npm
```

- A harness already at its pin is `kept`; `"latest"` installs only a missing
  one. A pin of `"2.1"` goes to npm and the vendor's installer as written,
  and what they bring must satisfy it.
- The sources are the manifest's `[[install]]` entries, in order (see
  [Harness manifests](#harness-manifests)). A release file is downloaded by
  the head into the artifact cache, checked against its published SHA-256,
  sent to the machine and checked there again. `npm` needs npm on the
  machine; `script` downloads the vendor's installer with curl and runs it
  with its `args`. A source the machine lacks a tool for is passed over; when
  none is left, the row is `failed` and names what each one lacked.
- Everything runs as the machine's login user: no sudo and no OS packages.
  A missing tool is reported, never installed.
- A release or npm install goes into a dir of its own,
  `~/.local/share/pastor/harnesses/<id>/<version>/`. The head reads the
  version of what it installed there, and only when it is the pin is
  `~/.local/bin/<binary>` pointed at it. A file there that is not a link is
  not replaced by any source, the vendor's installer included: the row says
  to move it away. A vendor's installer puts the harness where it likes.
  Pastor saves what `~/.local/bin/<binary>` was first, or that nothing was
  there, and puts that back when the installer fails or the version after is
  not the pin. Either way, a failed install leaves the old version in place.
- A release whose file does not match its published sum is `refused`, and
  no other source is tried for that harness.
- The manifest's version command, with `bin_dirs` on the PATH, has the last
  word. A harness that another copy earlier on the machine's PATH shadows is
  `failed`, saying so, and `~/.local/bin/<binary>` is put back as it was.
- A release pinned to `"latest"` is downloaded again each time, since the
  file under that name changes; a release at a version is cached.
- `[update] env` in a manifest is set for every agent of a kind its machine
  pins to a version, over the agent's own env, so the harness does not
  update itself; the built-in `claude` sets `DISABLE_AUTOUPDATER=1`. agy has
  no such switch. A session the user starts by hand on the machine is not
  covered.

The built-in `claude` installs from npm, else from its installer at
`https://claude.ai/install.sh` with the pin as its argument. agy names no
install source, so its row says `agy has no install source`. `--json` prints
`{machine, dry_run, rows}`, each row `{kind, pin, before, after, source,
action, steps, detail}`, `action` one of `kept`, `installed`, `refused`,
`failed` and `skipped`. The command exits 1 when a row is `refused`, `failed`
or `skipped`; with no pinned harness among the machine's agents and no herdr
pin (or none that `--kind` or `--agent` names) it fails with
`nothing_pinned`. It runs on the head's
machine, through the head over ssh when one is set, and needs a machine
reached by ssh or local.

#### herdr

With `herdr` set under `[artifacts]` in pastor.toml (see [The artifact
cache](#the-artifact-cache)), `pastor machine install` also takes herdr to
that version, in a row of its own with the kind `herdr`; `--kind herdr` takes
it alone, and `--agent` does not take it. It goes the way a harness's release
file does:

- herdr at the pin is `kept`. Otherwise the head fetches the release for the
  machine's platform from herdr's release index into the artifact cache,
  checks its SHA-256 against the index, sends it to the machine and checks it
  there again; a mismatch is `refused` and nothing else is tried.
- It is put in `~/.local/share/pastor/harnesses/herdr/<version>/` and
  `~/.local/bin/herdr` is pointed at it only when it says the pinned
  version; the old version stays on any failure, and is not deleted. Nothing
  runs as root and nothing is installed with a package manager.
- A `~/.local/bin/herdr` that is a file, not a link, is not replaced, and the
  row says to move it away. That is the usual first time on a machine where
  herdr's own installer put it there. Moving it is safe while herdr runs: the
  running server keeps the file it started from. Once it is moved, `machine
  install` links the pinned version in its place.
- A herdr that is there is not updated while herdr lists an agent named
  `t-<id>` on the machine (`agent list`), whatever the task's state: a stale,
  failed or done task's agent is still in its pane, and so is one with no
  task at all. The row is `skipped` and names them, and nothing is fetched or
  changed. The same when herdr cannot be asked, a herdr with no server running
  among them: start it, or move the old file away and install. Run it again
  once they are closed. A machine with no herdr is installed whatever is running.
- pastor does not restart herdr. The herdr server that is running goes on
  being the version it started as until it is restarted, while new herdr
  commands run the installed one. `machine check` has a `herdr-running` row,
  `warn`, when the two differ (with or without a pin), saying so; restart
  herdr when no agent is at work.
- `machine check` has a `herdr-version` row with `[artifacts] herdr` set:
  `ok` at the pin, `warn` naming both and the fix (`pastor machine install
  <name> --kind herdr`) at another version or with no herdr on the PATH.
  A drift does not fail the check.

### Harness manifests

What pastor knows about an agent's CLI is a manifest, one TOML file per
harness. The ones that ship are in `harnesses/` in the repository and built
into the binary: `claude` and `agy` in full, `codex` and `opencode` with the
paths a bundle needs. A file `<id>.toml` in `harnesses/` of the config dir
(`~/.config/pastor/harnesses/`) replaces the built-in with that id or adds a
harness, with no Rust; the file name is the id, and the id is the agent
`kind` it describes. `pastor serve` reads the directory at start and at each
reload; a file that does not load fails `pastor bundle apply` and `pastor
machine check` naming the file and the reason, and a reload keeps the previous
manifests.

```toml
id = "claude"
name = "Claude Code"
binary = "claude"                       # on the machine's PATH
bin_dirs = ["~/.local/bin", "~/.cargo/bin"]   # added to the PATH to look

[version]
args = ["--version"]                    # prints the version as its last line
# parse = "version_word"                # the first word that looks like one

[login]
check = ["claude", "auth", "status", "--text"]   # exits 0 when signed in
start = ["claude", "auth", "login"]     # what `pastor login` types into a pane
paste = "Paste code here"               # shown while it waits for a code given back
# pick = "Select login method:"         # a list it opens on, and the keys
# keys = ["Enter"]                      #   that pick a way in, pressed once
# code_after = "Enter this one-time code"   # the device code follows this text

[[install]]                             # sources, in the order to try
source = "release"                      # a file at an https URL with its sum
url = "https://example.org/{version}/tool-{os}-{arch}"
sha256_url = "https://example.org/{version}/tool-{os}-{arch}.sha256"
[[install]]
source = "npm"
package = "@anthropic-ai/claude-code"
[[install]]
source = "script"                       # the vendor's installer, over https
url = "https://claude.ai/install.sh"
shell = "bash"                          # sh (default) or bash
args = ["{version}"]                    # its arguments; {version} is the pin

[update]
env = { DISABLE_AUTOUPDATER = "1" }     # keeps a pinned harness from updating itself

[mcp]                                   # how a bundle's MCP servers are placed
method = "plugin"                       # plugin, or file (with file = "~/...")

[paths]                                 # relative to the home
home_env = "CLAUDE_CONFIG_DIR"          # moves the home, if the harness has one
home = "~/.claude"
skills = "skills"
hooks = "hooks"
instructions = "CLAUDE.md"
settings = "settings.json"
mcp = ".claude.json"
mcp_default = "~/.claude.json"          # where it is when the home is the default
secrets = [".credentials.json"]         # its login: bundles never read or write it
```

Only `id` and `[paths]` with `home` are required; `version`, `login`,
`install` and `update` need `binary`. A release URL may hold `{version}`, `{os}` and
`{arch}` (`uname`'s, lowercase); every URL is https, and a release names the
URL of its sha256 sum too, which the head checks through the artifact cache.
A script's `args` are plain words and `{version}`. `paths.secrets` lists
the files that hold the harness's login, each in the home or with a leading
`~`, `*` in a name standing for any characters; `bundle apply` and `bundle
adopt` refuse them. `pastor machine install`
installs from the sources. The login check is what `pastor agent
signed-in` and `pastor login` run when the agent sets no `login_check` of
its own; it reads no secret and takes none. `[login] start` is the
interactive login `pastor login` types into a pane (see
[Signing an agent in](#signing-an-agent-in)): `pick` and `keys` go
together, `code_after` is the text the device code follows (the rest of its
line when that is one word, else the first word of the next line with
text), and `paste` the text the pane shows while it waits for a code given
back. A URL is read off the pane as it is, and a URL a harness wraps in its
own box is joined back. The built-in `claude` has no release source
(no published sum pastor can check) and `agy` has no install source, and says
so in its file.

Two things read manifests today. The version pins above ask for a version the
way `[version]` says, with `bin_dirs` on the PATH, and `pastor machine check`
and `machine list` do the same for each pinned kind; a pin can name a
kind whose built-in manifest has a `[version]` (`claude` and `agy`), so a
harness that only a file in the config dir describes can be described but not
pinned from flock.toml yet, and shows no version row or drift. A pin on a kind
whose manifest (a replacement in the config dir) has no `[version]` is a `warn`
row in `machine check` saying so, and shows nothing in `machine list`. `pastor bundle apply` places skills, hooks,
instructions and settings where `[paths]` says (a harness with no `skills`,
`hooks`, `instructions` or `settings` path has no such slot), so a manifest in
the config dir moves them, or gives a harness of its own a home. MCP servers
follow `[mcp]`, not `paths.mcp` (the file a running Claude rewrites, which
pastor stays out of): `method = "plugin"` is pastor's own local plugin in each
home, in Claude's plugin format: its `.mcp.json` names a server's env as
`${VAR}` for the harness to expand, and two keys in the manifest's
`paths.settings` enable it, so this method needs `paths.settings`;
`method = "file"` with `file = "~/..."` is one JSON file of `mcpServers`
shared by every home of the kind, outside the home if it is (agy's
`~/.gemini/config/mcp_config.json`), which takes env values, not names, so a
server that needs env by name is left out and said so. One file cannot be
shared by the several homes `paths.home_env` gives a kind, so the file method
is refused with it. A harness with no
`[mcp]` gets its MCP servers skipped with a note (codex and opencode, for
now). See Applying bundles. A bundle's `kinds` still names the four built-in
kinds.

### Checking a machine

`pastor machine check <name>` runs a provisioner's checks on a machine and
says what the machine is missing for the agents that can land on it. It
changes nothing there and never runs a `fix`.

```text
$ pastor machine check pi-3
pi-3: provisioner raspberry-pi-os 0.2.0
ok     git  /usr/bin/git
fail   gh-login  You are not logged into any GitHub hosts. To log in, run: gh auth login
ok     claude (claude)  /home/me/.local/bin/claude
ok     claude-login (claude)  Login method: Claude Max account
ok     claude (claude-main)  /home/me/.local/bin/claude
fail   claude-login (claude-main)  Not logged in. Run claude auth login to authenticate.: sign it in from the head with `pastor login pi-3 claude-main`
fail   codex (codex)  codex is not on the PATH
skip   codex-login (codex)  needs codex, which did not pass
ok     profile (claude-main)  develop, from flock work, renders for claude
ok     path  herdr, claude found; PATH /home/me/.local/share/mise/shims:/usr/local/bin:/usr/bin:/bin
ok     reach  pi-3 reaches the head at user@head.example
ok     gh-auth  github.com as alice: logged in, the token expires 2027-01-15 (in 76 days)
ok     signing  a commit here signs and verifies
ok     inbox (claude)  a test message reached the session
skip   inbox (claude-main)  claude is not signed in; see its login row
```

| Option | Meaning |
| --- | --- |
| `--provisioner <id>` | use this provisioner instead of the machine's `provisioner` or the first that fits it |
| `--json` | print `{machine, provisioner, version, agents, rows}`; a row is `{id, agent, status, first_line, reason}`, `status` one of `pass`, `fail`, `error`, `skipped`, `warn` |

The provisioner is the one `--provisioner` names, else the machine's
`provisioner`, else the first in the catalog, by id, whose `os` and
`distro` match what the machine says (`uname -s` and the `ID` in
`/etc/os-release`) and whose `detect` passes. Of the built-ins, `arch`
fits an Arch machine, `macos` a Mac, `raspberry-pi-os` a Debian or
Raspbian machine whose device tree names a Raspberry Pi, and `debian` any
other Debian or Ubuntu machine.

A failed `<kind>-login` row of an agent whose harness has an interactive login
(claude and agy; `[login] start` in its manifest, see [Harness
manifests](#harness-manifests)) ends with the command that signs it in:
`pastor login <machine> <agent>`, run on the head (see `pastor login`). A kind
with no such login, such as codex, gets none, and neither does a check that
could not run.

For each Claude agent a task can get on the machine there is also an `inbox`
row, which says whether a session there takes a message through its inbox
socket (see [The inbox](#the-inbox)), the way pastor hands it a task's prompt.
The head starts a short-lived Claude session in a pane on the machine as a task
starts one, with a session id of its own and the agent's env, sends it one
message with the machine's `pastor inbox send` (`prompting` mode), and closes the
pane on every path. A session needs a moment to open its socket, so a send that
finds none is tried again for up to 45 seconds. The row passes when the session
takes the message, and fails with what it said when it is `refused`, `held`,
`dropped`, `denied` or `expired`, or with `timed out` when it opened no socket
(a Claude without one fails the same way). It is skipped, with the reason, for
an agent that has `delivery = "type"`, one whose `claude-login` row failed, and
a machine that cannot send through the socket (a `command` machine, or no pastor
there with `inbox send`), and one where no pane can be opened (herdr is not
reachable, which other rows report); a pull machine, which the head never connects to, has
none. The message asks nothing of the session, but one that takes it may start a
turn before the pane closes, and it counts as a use of the agent's account.

The last row is `path`, whatever the provisioner: the directories the commands
pastor runs on the machine see, and whether herdr and the CLI of each harness
the machine's agents run resolve on them. ssh runs a command in a shell that is
neither a login nor an interactive one, so it reads none of the files that put
mise's shims (or `~/.local/bin`) on the PATH. For a machine reached by ssh,
pastor starts each of its own commands there (the herdr bridge and every
probe, check, install and bundle command) by putting mise's shims directory
first on the PATH when it exists, `$MISE_DATA_DIR/shims`, else
`~/.local/share/mise/shims`; a `local` machine runs them as they are. Shell
files on the machine are never edited. A CLI that resolves only in
`~/.local/bin` or a manifest's `bin_dirs`, which pastor adds for its own
version checks, still passes and the row says where it is; one found nowhere is
a `warn` naming it (the check still passes). The agents themselves run in
herdr's panes, with the PATH of the shell there.

A machine whose `ID` no provisioner names is tried again by the words of its
`ID_LIKE`, which lists the distributions it derives from, nearest first: Linux
Mint says `ubuntu debian`, elementary OS `ubuntu`, EndeavourOS and Manjaro
`arch`. The first word that a provisioner's `distro` lists, with `detect`
passing, gives its provisioner, so Mint gets `debian` (and a Mint or Ubuntu on
a Raspberry Pi gets `raspberry-pi-os`), and the report starts with a
`provisioner` row that says so: `debian, because os-release ID linuxmint has
ID_LIKE ubuntu`. An os-release with no `ID` is read the same way (`because
os-release has no ID and has ID_LIKE debian`). A provisioner that names the
machine's own `ID` always comes first, with no such row: Ubuntu is named by
`debian` itself. When none fits, the only row is a failing
`provisioner_none_fits` that says what was tried and why each did not fit,
with the machine's `ID_LIKE`; name a provisioner for such a machine in
`flock.toml`, or write one. A `--provisioner` no provisioner has is refused
with `unknown_provisioner`.

The checks run as `provision::run` runs them: in file order, each in a login
shell in a run directory of its own under `~/.cache/pastor/setup` that holds
the provisioner, removed when the run ends. A check exits 0 to pass and 1
to say the machine needs its fix; anything else, no answer, more than 64 KiB
of output or a run past its `timeout` is an `error`. A check whose `needs`
did not pass is `skipped`. The row's text is the first line the check
printed, after whatever the machine's rc files print.

A check with `agent = "<kind>"` runs once for each agent of that kind a task
can get on the machine, with that agent's `env` from `[agents]`: what
dispatch picks for each of the machine's flocks, for a task that asks for
nothing and for one that asks for each model in `[models]`. Two Claude
logins on a machine are two rows. A kind no agent there is gets no row. A
check with an `account` runs only when one of the machine's flocks lists
that account.

Every built-in has an install row and a login row per agent kind, each run
per agent as above, the login row only when the install row passed:

| Kind | Install row | Login row runs | It fails when |
| --- | --- | --- | --- |
| `claude` | `claude` on the PATH | `claude auth status --text`, with the agent's `CLAUDE_CONFIG_DIR` | that config dir is not logged in |
| `codex` | `codex` on the PATH | `codex login status` | codex is not logged in |
| `opencode` | `opencode` on the PATH | `opencode auth list` | it lists no stored credential and no provider key in the env |
| `agy` | `agy` on the PATH | `agy models` | agy asks to sign in; any other failure, such as no network, is an `error` |

An opencode that uses only a provider needing no key fails its login row;
give such a machine a copy of its provisioner without it.

When the machine has a permission profile of its own (its `[[machine]]`
`profile`, else its flock's, else `[defaults]`), pastor adds one row of its own
for each Claude agent, `account-rules`, about that agent's account: it reads
`settings.json` under the agent's `CLAUDE_CONFIG_DIR` (default `~/.claude`)
and looks for `ask` and `deny` rules that overlap one of the profile's allows.
Claude applies an account's `ask` over a command-line allow, and a task under
the profile never prompts, so the tool is refused, with no prompt and nothing
in the task's record of why (an `ask` on `Bash(git push:*)` beside an allowed
`Bash`); a `deny` narrows the allow the same way. A rule the profile denies
too is not reported, `ask` or `deny`: it costs the task nothing new. The row is `warn`, names each rule and the allow it
shadows, and the file it is in, and changes nothing: the rule may be wanted.
`ok` means no rule overlaps; a missing `settings.json` is `ok`, one that is not
JSON is a `warn`. Patterns are compared by tool and by the start of their
argument (`Edit` and `Write` count as one tool, an MCP server's name covers
its tools); a pair it cannot tell is not reported.

Each agy agent gets an `account-rules` row too, about the config agy shares
with its desktop app, `~/.gemini/config/config.json`. A task under a profile
starts agy in `always-proceed`, where every `ask` is approved, and pastor
writes the machine's own profile into agy's global settings before each
launch, so neither of those can shadow an allow. A `deny` in the shared
config, which pastor never writes, still refuses the tool: the row warns for
each one that overlaps one of the profile's allows as agy reads it, or
`pastor task done`, which every task under a profile is allowed, and that the
profile does not deny too. It names the rule, the allow and the file, and
reads only the file's `permissions.deny`. Codex and opencode agents get no
such row, and a machine with no profile none at all.

Each agent a task can get on the machine (as above) that runs under a
permission profile has a `profile` row: which profile, and where it comes
from (`machine pi-3`, `flock work` or `defaults`, the first that names one,
as for a task that asks for none). pastor settles the profile for that agent
as the head settles a task it queues, with the machine's own profile, then
starts it as dispatch would. `ok` means that works; `fail` gives the code and
message a task there would be refused with, such as `profile_args_conflict`
(the agent's args pick a permission mode of their own),
`profile_not_allowed` (`unrestricted` where the machine's own profile is
not) or `agy_profile_mismatch`, and names the model when only a task for one
model of `[models]` is refused. An agent no profile applies to has no row.
The row reads `pastor.toml` and `flock.toml` only; what the agent's harness
holds on the machine is for `account-rules` and `opencode-config`.

When an opencode agent can land on the machine, an `opencode-config` row says
what opencode's own config there holds, read as dispatch reads it before it
starts an opencode task under a profile. That is the managed config
(`/etc/opencode`, on a Mac `/Library/Application Support/opencode` and the
managed preferences), which opencode merges after what pastor sends, and the
global config (`~/.config/opencode`, or under `$XDG_CONFIG_HOME`) and
`~/.opencode`. A top-level permission key in the global config is `ok`: a
task under a profile denies it, and the row names it. A `permission` or
`tools` key in the managed config, rules under an agent or a mode, a file
pastor cannot parse or more config than it reads are what a task under a
profile is refused for (`opencode_permissions_conflict`). The row then fails,
naming each file and key, or warns when no opencode agent there runs under a
profile, since only a task that asks for one would be refused. A machine that
gives no answer is a `warn`; dispatch goes ahead then too. A machine with no
opencode agent has no row and is not asked.

When a machine's flock lists the `github` account, pastor adds `gh-auth` rows
about `gh` there, one for each host `gh auth status` names (the active account
of each): `ok` when the login works, `fail` when `gh` says the host's token
does not work, naming `gh auth login --hostname <host>`. When GitHub sends the
token's expiry (a fine-grained token, or one made with an expiry, answers with
a `Github-Authentication-Token-Expiration` header, which pastor reads with
`gh api -i user`), the row says the date and the days left, and is a `warn`
under 7 days, naming `gh auth login --hostname <host> --with-token`; a token
with no expiry says nothing of one. With no login at all there is one `warn`
row (the provisioner's `gh-login` check fails for github.com). A machine with
no `gh` gets no row: the `gh` check says so.
With `accounts = { github = "alice" }` the account signed in to github.com
must be that name (without case), or the row is a `fail` naming the account it
found and the one the flock expects; hosts other than github.com are not
compared. pastor never asks `gh` for the
token (no `gh auth token`, no `--show-token`) and prints none of what it
reads but the host, the account name and the date.

With `accounts = { claude = "alice@example.com" }` each Claude agent that can
land on the machine gets a `claude-account` row: pastor runs `claude auth
status --text` under that agent's `env` (its own `CLAUDE_CONFIG_DIR`) and
compares the email it prints with the name. `ok` is `signed in as <email>`;
a different account is a `fail` that names both (`signed in as bob@example.com,
the flock expects alice@example.com`); a claude that prints no email is a
`warn`. An agent whose `claude-login` row already failed gets no account row.
A flock that gives no name, or the list form, gets no `claude-account` row.

With a `head_address` in `pastor.toml`, an ssh machine gets a `reach` row: from
the machine, a ping through the door its agents use, `ssh -o BatchMode=yes -o
ConnectTimeout=8 -- <head_address>` with the machine's own ssh keys and
known_hosts, which the head's authorized_keys answers with `pastor bridge
--agent`. `ok` is `pi-3 reaches the head at user@head`; anything else is a
`fail` named `ssh to user@head from pi-3 failed`, with the last lines ssh said
(`Permission denied (publickey)`, `Host key verification failed`, no route) and
the command that fixes the first, `pastor machine authorized-key pi-3 --key
FILE --write` on the head. No `head_address`, or a `local` machine, which uses
the head's socket, gets no row.

A head that runs `machine check` for a machine keeps its `reach` result, and
dispatch places no task on a machine whose last result is a `fail`: a task it
could not even tell the head is done would be lost to it. The queued task
waits as it does for a machine under its room floor, `waiting for a machine:
pi-3 cannot reach the head`, in `pastor queue` and `task describe`; `task run
--now` looks past it. The result stays until the next `machine check` of that
machine runs through the head: once the key is fixed, run it again, and a pass
frees the machine and starts what waited. A machine the head never checked,
one checked without a `head_address`, and a check whose row could not run
(`error`) leave things as they were. A check run without a head keeps nothing.

Every report ends with a `signing` row: pastor makes a throwaway repo in a
run directory of its own on the machine (under `~/.cache/pastor/setup`, never
in one of your repos and never in `/tmp`), commits to it with `git commit -S`
as git there is set up to sign, asks `git verify-commit` about that commit,
and removes the repo. `ok` means both worked. A machine where git's config
sets neither `user.signingkey` nor `commit.gpgsign` and where a commit does not
sign has signing that was never set up, which is a `warn` (`signing is not
set up here`), not a failure. `fail` names what is broken, with
the last lines git said: `git is not on the machine's PATH`, `a commit here
does not sign` (a key that is not there, a signer that is not installed, no author identity) or
`a signed commit here does not verify` (an ssh key without an
`allowedSignersFile`, a gpg key whose public half is missing). Agents commit
on that machine, so a commit that cannot be signed stalls a task at its
first one. pastor never opens a key: git and its signer do. The commit runs no
hooks and asks nothing: a signer that waits for a passphrase it cannot be
given is cut off after a minute, as an `error` row. The repo is outside any
user repo, so a `git config` that only applies there (an `includeIf` on a
`gitdir`) does not reach it.

After it, when the machine's herdr answers, comes a `pane-env` row for each
agent that can land there (one without an agent when none can). The other
rows run over ssh; an agent runs in a herdr pane, whose login shell reads
your profile first, and a profile that sets `PATH`, `CLAUDE_CONFIG_DIR` or
a flock's `env` again undoes what pastor set for the task without a word.
For the row pastor opens a short-lived workspace, `check/pane-env`, the way
a task's starts: with the env a task of that agent gets (`[defaults]`, flock
and machine `env`, the agent's own `env` with `~` expanded, then
`PASTOR_TASK` and `PASTOR_HEAD`). It types a probe into the pane's shell
that writes each of those names and `PATH` to a file under
`~/.cache/pastor-check`: its value, or for a name that looks secret (one
with token, secret, key or password in it) only whether it is set. The check
reads that file over its own shell, removes it, and closes the pane, which
it does on every path: when the probe never ran, when typing failed, and
when the check itself is cut short. `ok` means the shell kept every one;
`warn` names each variable the shell changed and each it dropped (a secret
one only when dropped); `error` is a pane that could not be opened or a
shell that did not run the probe within 20 seconds, as a profile that waits
for input does. It never changes your profile.

`machine check` exits 1 when a row is `fail` or `error`; a `warn` or `skip`
does not. With a head
running, the head runs the checks, from the flock and `pastor.toml` it last
applied, and raises a `machine.check_failed` event for each such row (see
[Events](#events)); without one, it runs here and raises none. It reads the
fleet and changes nothing, so an agent pastor started may run it. A pull
machine, which the head never connects to, and a `command` machine, which
runs no shell, are refused with `machine_check_unsupported`.

### The artifact cache

The head keeps the binaries it will put on machines, pastor, herdr and a
harness's release file (as `harness-<id>`, its target `<os>-<arch>`), in a
cache: `~/.local/state/pastor/artifacts/<name>/<version>/<target>/<name>`,
with the file's SHA-256 beside it in `<name>.sha256`. `pastor machine
install` takes harness releases and herdr from it; installing pastor on a
machine comes later, and takes its file from here too.

- An artifact is downloaded once. A second request for one being fetched
  waits for the first and finds it cached. A cached file is hashed again on
  each use, and one that no longer matches its `.sha256` is fetched again.
- Downloads go through `curl -fsSL --proto =https`, at most 512 MiB and ten
  minutes each, to a temp dir beside the cache entry. A file is renamed
  into the cache only after its sum matches the published one; a mismatch
  is `artifact_checksum_mismatch` and caches nothing.
- Name, version and target each become a path part, so each must match
  `[A-Za-z0-9._-]+` and not be `.` or `..` (`invalid_artifact`).

Where each comes from:

| Name | Source | Sum |
| --- | --- | --- |
| `pastor` | the release tarball `pastor-<version>-<target>.tar.gz` of the pastor repository, or the head's own binary when it is that version, built for that target (a head built from source counts as that version; its sum is taken from the copy) | the release's `SHA256SUMS` |
| `herdr` | the asset herdr's release index names for that version and target; the index URL is `[artifacts] herdr_index`, else `https://herdr.dev/latest.json` | the index's `sha256` for the asset |

An asset URL the index gives must be https too (`invalid_artifact`). The
target is the machine's, from what it says of itself (`uname -s`,
`uname -m` and `getconf LONG_BIT`, asked with the os-release facts that
pick its provisioner): pastor's release triple
(`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`,
`armv7-unknown-linux-musleabihf` for a 32-bit userland,
`riscv64gc-unknown-linux-musl`, `aarch64-apple-darwin`,
`x86_64-apple-darwin`), or herdr's key (`linux-x86_64`, `linux-aarch64`,
`macos-x86_64`, `macos-aarch64`). A machine that neither ships a build for
is `unsupported_platform`.

The herdr version is `[artifacts] herdr`, else the one most of the fleet's
machines already run (a tie goes to the higher), else a built-in default:

```toml
# fragment of pastor.toml
[artifacts]
herdr = "0.9.5"                               # optional: the herdr version machines get
herdr_index = "https://mirror.example/herdr.json"   # optional: a mirror of herdr's release index; https only
```

## Bundles

A bundle is a directory of what a team wants in its agents' homes on every
machine: skills, hook scripts and other files, a block of instructions for
`CLAUDE.md` or `AGENTS.md`, settings fragments in each harness's own shape,
MCP servers, harness plugins, and requirements a machine must meet. Anyone
writes one; a flock names the bundles its machines get in `flock.toml`.

A bundle whose manifest says `target = "workdir"` goes into the working
directory of a flock's tasks instead of its machines' homes: see [Stored
workdir bundles](#stored-workdir-bundles). The rest of this section is about
home bundles.

Pastor keeps bundles on the head, checks `flock.toml` against them, and
`pastor bundle apply` places their skills, hook scripts, bin files and
instruction blocks in the homes of a machine's agents, merges their settings
fragments into the settings file of claude and agy homes, and places MCP
servers for those two as well. Not placed: a bundle's plugins and requirements,
which are carried in it but left alone, and settings fragments and MCP servers
for codex and opencode homes, which get a skipped row each.

### The manifest

A bundle is a directory with `pastor-bundle.toml` at its root:

```toml
id = "team-rules"                     # required: [a-z0-9][a-z0-9_.-]{0,63}
version = "2026.10.27.1"              # required: the author's, one word; pastor tells versions apart by content
description = "the team's hooks, skills and agent instructions"
author = "platform team"              # optional, like description
min_pastor_version = "0.10.0"         # optional: refuse to load on an older pastor

[vars]                                # values each flock gives in bundle_vars
ticket_url = { description = "the team's ticket tracker URL" }

[[skill]]                             # a skill folder with a SKILL.md, placed under its own name
src = "skills/reviewing-a-pr"
kinds = ["claude", "codex"]           # optional: unset is every kind

[[file]]                              # any other file, into a slot
src = "hooks/notify.sh"
slot = "hooks"                        # hooks, bin, mods or instructions-include
mode = "0755"                         # optional, octal

[instructions]                        # a marked block in each harness's instruction file
src = "instructions/team.md"
per_machine = "instructions/machines/{machine}.md"   # optional: appended when the bundle has one for the machine
kinds = ["claude", "codex", "opencode"]

[settings.claude]                     # merged into that harness's settings, in its own shape
src = "settings/claude.json"          # JSON for claude, agy and opencode; TOML for codex

[[mcp]]                               # one MCP server: a command with args, or a url
name = "tickets"
command = "npx"
args = ["-y", "@example/tickets-mcp", "--url", "{ticket_url}"]
env_from = ["TICKETS_TOKEN"]          # names only; the machine's env gives the values
kinds = ["claude", "agy", "codex", "opencode"]

[[plugin]]                            # a Claude plugin, installed by Claude's CLI
kind = "claude"
name = "code-review"
marketplace = "example-marketplace"
source = "github:example/claude-plugins"
ref = "v1.4.0"                        # optional pin

[[require]]                           # a check a machine must pass
id = "gh"
command = ["sh", "-c", "command -v gh"]
fix_hint = "install the GitHub CLI on this machine"
```

A key the manifest does not know is an error, and so is a bundle that
carries nothing. Every `src` is a path inside the bundle with no `..`, and
must be there: a skill a directory with a `SKILL.md`, everything else a
file. Two skills, or two files in one slot, may not share a name. A
settings fragment must parse, and may not set what pastor's permission
profiles own: `permissions` for Claude, `permission` for opencode, and
`permissions`, `toolPermission`, `agentMode` and `allowNonWorkspaceAccess`
for agy; put those rules in a profile (see [Permission
profiles](#permission-profiles)). `kinds` takes `claude`, `codex`,
`opencode` and `agy`. Only Claude plugins are installed by pastor.

`{name}` in an MCP server's command, args or url is a var: one the bundle
declares under `[vars]`, or one of pastor's own, which it may not declare:
`{home}`, `{config_dir}` (the harness home), `{bundle_dir}`, `{bin}`,
`{machine}`, `{flock}`, `{agent}`, `{kind}` and `{account}`. An unknown one
is an error. `per_machine` may use only `{machine}`.

### Adding a bundle

```
pastor bundle add <dir> [--json]                        copy a directory
pastor bundle add --git URL --ref REF [--path SUB] [--json]   fetch a pinned ref
pastor bundle list [--json]                             ID, VERSION, TARGET, HASH, KEPT, NAMED BY, DESCRIPTION
pastor bundle show <id> [--json]                        what it carries, who names it, the versions kept
pastor bundle remove <id> [--json]                      refused while flock.toml or a job names it
pastor bundle apply [<machine>] [--dry-run] [--json]    place bundles in a machine's agent homes
pastor bundle adopt <machine> [--agent A] [--dry-run] [--json]   take a machine's homes over in one step
```

`add` validates the bundle, refuses one that looks like it holds a secret,
and copies it to `~/.config/pastor/bundles/<id>/`, the bundle as it is now.
Each content it is added with is kept as a version under
`~/.local/state/pastor/bundles/<id>/<hash>/`, newest first in
`versions.json` with where it came from; the last 5 are kept. The hash is
SHA-256 over each file's path, executable bit and bytes, so the author's
`version` does not count: adding the same content again changes nothing and
says so. A bundle written by hand in the bundles dir is added in place.

`--git` fetches `--ref` (a branch, tag or commit) from the repository and
records the commit it named, so a version never follows a moving branch;
`--path` is the bundle's directory inside it. A `.git` at the bundle's root
is left out. A bundle holds plain files and directories only: a symlink is
refused, and so is a bundle over 64 MiB.

`add --json` prints `{id, version, hash, previous_hash, flocks,
machines_affected, warnings}`: the flocks that name it and the machines that
get it. `list --json` gives `{id, version, description, target, hash,
versions, flocks, machines, error}` per bundle, `target` being `home` or
`workdir`, `hash` being the bundle as it is now;
`show --json` gives `{id, dir, hash, changed_since_add, flocks, machines,
machines_affected, versions, manifest}`. A bundle dir that does not load is
listed with its `error`. `remove` takes the bundle and every version kept,
and is refused with `bundle_in_use` while a flock or machine names it, or
a job's `[dispatch] workdir_bundles` does; the refusal says each one and
under which key.

The bundles are the head's: with a remote head set, `bundle` commands are
refused (`remote_head_unsupported`). `add`, `remove`, `apply` and `adopt`
change what a flock's machines get, so an agent pastor started may only
`list`, `show`, `apply --dry-run` and `adopt --dry-run`.

### Applying bundles

`pastor bundle apply <machine>` places the newest version of each bundle the
machine gets in the homes of its agents; with no machine it does so on every
machine reached by ssh or locally, leaving out pull and `command` machines.
It runs from the head's files, with or without `pastor serve`, and needs
pastor on the machine (on its PATH, or in `~/.local/bin`): the head renders
what goes where, sends it to a run directory under `~/.cache/pastor/setup`,
and runs `pastor bundle apply` there, which does the writing.

The homes are those of the agents a task can get on the machine, the same
set `machine check` checks (one per distinct home; two agents with one
`CLAUDE_CONFIG_DIR` share it). In each:

| Kind | Home | Skills | Hooks | Instructions | Settings | MCP servers |
| --- | --- | --- | --- | --- | --- | --- |
| `claude` | `$CLAUDE_CONFIG_DIR`, else `~/.claude` | `skills/` | `hooks/` | `CLAUDE.md` | `settings.json` | pastor's own plugin |
| `codex` | `$CODEX_HOME`, else `~/.codex` | `skills/` | | `AGENTS.md` | | |
| `opencode` | `$OPENCODE_CONFIG_DIR`, else `~/.config/opencode` | | | `AGENTS.md` | | |
| `agy` | `~/.gemini/antigravity-cli` | | | | `settings.json` | `~/.gemini/config/mcp_config.json` |

- A version goes into the machine's bundle store,
  `~/.local/share/pastor/bundles/<id>/<hash>/`, with the modes its `[[file]]`
  entries give, and skills and hook scripts are symlinks into it under their
  own names. A new version is a new directory, and each link is swapped to
  it by a rename. A `bin` file is linked in `~/.local/bin` once for the
  machine; `mods` and `instructions-include` files stay in the store, for
  `{bundle_dir}` to name.
- The instructions go in the home's instruction file as a block between
  `<!-- pastor:bundle:<id> -->` and `<!-- /pastor:bundle:<id> -->`, after the
  user's text. Text outside pastor's blocks is never changed. The block's
  vars are filled in: the flock's `bundle_vars`, `{machine}`, `{flock}` (the
  machine's first flock that names the bundle), `{agent}` and `{account}`
  (the home's first agent by name), `{kind}`, and on the machine `{home}`,
  `{config_dir}` (the home), `{bundle_dir}` and `{bin}`. A brace that names
  no var stays as written. Skill, hook and other files are placed as they
  are, braces and all.
- Each bundle with instructions gets its own block in the file, in the
  order the machine's flocks name the bundles.
- A skill or instructions for a kind with no slot for them is a `skipped`
  row when the bundle names that kind in `kinds`; with `kinds` unset it goes
  only where there is a slot.
- Two bundles that place the same file or skill refuse the machine's
  apply.

Settings and MCP servers go in key by key, beside the user's own:

- A bundle's `[settings.<kind>]` fragment merges into the home's settings
  file, in the harness's own shape. For Claude, each `hooks` group is added
  to its event's list after the user's groups; `env`, `enabledPlugins` and
  `extraKnownMarketplaces` go key by key; any other key goes whole. For agy,
  each key goes whole (its permission keys are the profile's, and `add`
  refuses a fragment that sets them). The vars are filled in every string,
  as in an instruction block.
- A key that is not there is written. A key the user holds with another
  value is a `conflict` row naming the bundle's value, never the user's; the
  user's value stays, and a plain apply never overwrites it. A key or hook
  group the user already had with the bundle's value is theirs: pastor
  leaves it out of the ledger, so it stays when the bundle leaves. A key
  pastor set that was changed since is `changed_by_hand` and is left as it
  is.
- A settings file that is not JSON (a comment in it, say) is a `failed` row
  and is not touched. Pastor writes the file back with two-space indents
  and its keys in order.
- Claude's MCP servers go in a small plugin pastor makes for each home,
  `pastor-mcp`, in a marketplace of its own under
  `~/.local/share/pastor/claude-marketplace/`. Two settings keys enable it,
  `extraKnownMarketplaces.pastor` and `enabledPlugins["pastor-mcp@pastor"]`;
  Claude reads the plugin from that directory with no install step and no
  network, and a session started after an apply has the servers. This keeps
  pastor out of `.claude.json`, which a running Claude rewrites. A server's
  `env_from` names become `${VAR}` in the plugin, which Claude fills in from
  the agent's environment, so no value is written. No bundle may be called
  `pastor-mcp`.
- agy's servers go in `~/.gemini/config/mcp_config.json`, key by key under
  `mcpServers`. agy takes env values only, not names, so a server with
  `env_from` is a `skipped` row there.
- Settings for codex and opencode, and MCP servers for them, are `skipped`
  for now.
- Two bundles that set one key, or name one MCP server, in a home refuse the
  machine's apply, naming both and the key. Two bundles may add the same
  hook group: it is theirs together, and it stays until the last of them
  leaves.

The machine keeps a ledger of every link, block, settings key and file
pastor wrote, with what it wrote, in `~/.local/state/pastor/applied/ledger.json`. Pastor changes or
takes out only what the ledger lists, and only while it still holds what
pastor wrote; when a bundle or an item leaves the machine's bundles, what
pastor placed for it comes out, and the user's file is left as it was before
the block went in, byte for byte. An instruction file pastor made goes when
its last block does; one the user made stays, even empty. Each write is in
the ledger before it is made, so an apply cut short (a timeout, a dropped
connection) leaves nothing the next one does not know as pastor's; one apply
runs at a time on a machine.

Before pastor first changes a file in a run, it copies it to
`~/.local/state/pastor/applied/backup/`. The first copy of each file, as it
was before pastor touched it, is kept for good (its name ends in `.first`),
and the last 5 after it. The file is written to a temp file beside it with the old
file's mode and renamed over it, only if it still holds what was read (a
harness that saves it meanwhile makes pastor read it again, up to 3 times).
Pastor never writes through a symlink: a dir from `$HOME` down to the item
(the home, a dir above it or in it) or an instruction file that is a symlink
is a `linked` row naming where it points, and nothing behind it changes.
`pastor bundle adopt` takes such a home over (see Adopting a home).

Each row is `{bundle, home, path, key, action, detail}`; `key` is the dotted
key in a settings or MCP file, and absent for any other item. A `replaced`
row adds `old` and `backup`. `action` is one
of:

| Action | Meaning |
| --- | --- |
| `added`, `updated`, `removed` | written, or with `--dry-run` what would be |
| `kept` | already in place |
| `conflict` | something not pastor's is where the item goes: a file, a dir, or a link pastor did not make |
| `linked` | the item's file, or a dir on its way, is a symlink |
| `changed_by_hand` | what pastor wrote was changed since; it is left as it is |
| `busy` | the file kept changing while pastor wrote it |
| `failed` | the write failed, or the file is a secret one; `detail` says why |
| `skipped` | no home here has a place for it |
| `replaced` | only from `bundle adopt`: the bundle's went in place of the user's (see Adopting a home) |

`--dry-run` says what would change and writes nothing, not even the store
or the ledger; the run directory it was sent in is removed after, with
`~/.cache/pastor/setup` and `~/.cache/pastor` when they are left empty. Applying twice changes nothing the second time. `--json`
prints one object per machine, `{machine, ok, error, skipped, rows}`. The
command exits 1 when a machine could not be applied to or has a
`conflict`, `linked`, `changed_by_hand`, `busy` or `failed` row.

A harness's secret files, which its manifest lists under `paths.secrets`
(Claude's `.credentials.json`, codex's `auth.json`, agy's
`antigravity-oauth-token`, opencode's `~/.local/share/opencode/auth.json`),
are never read, copied or written: an item at one is a `failed` row saying
so, in an apply and an adopt alike.

### Adopting a home

```
pastor bundle adopt <machine> [--agent A] [--dry-run] [--json]
```

A plain apply never overwrites what is not pastor's: a key the user holds
with another value is a `conflict`, a symlinked file or dir a `linked` row,
a skill dir of the user's where the bundle's goes a `conflict`. `pastor
bundle adopt` takes the homes of a machine's agents over in one step, or
the home of one agent with `--agent` (an agent a task on the machine can
get; another is `unknown_agent`). In each home it takes over:

1. **Backup.** Every file it will touch is copied, before anything is
   written, to `~/.local/state/pastor/applied/backup/adopt-<time>/` on the
   machine, by its path below `$HOME` (a path outside it under `_/`): a
   symlink's content, not the link, and a dir's whole tree. If a copy
   fails, the adopt fails and writes nothing. These backups are kept; pastor
   never restores one by itself.
2. **Links.** A symlinked settings or instruction file, and a symlinked dir
   on an item's way (a `skills` dir linked into a dotfiles repo, say),
   becomes a plain copy of what it points to: a file's content and mode, a
   dir's tree with the links inside it kept as links. What the link pointed
   to is never written. A link that points at, or a dir that holds, one of
   the harness's secret files is not copied: its items are `linked` rows
   saying so, to be moved aside by hand.
3. **Merge.** Every item is applied, and where the user holds something
   else, the bundle's wins: a settings key with another value, a value on a
   key's way that is no object, a hook event that is no list, a block the
   user edited, a file or dir of the user's where a skill or hook goes, a
   link pastor did not make. Each is a `replaced` row with `old` (what was
   there, shortened) and `backup` (where its copy is). Keys no bundle sets,
   the user's own hook groups and the text outside pastor's blocks stay as
   they are. A settings file that is not JSON is still a `failed` row.
4. **Ledger.** Everything pastor wrote is in the ledger, and so is a value
   the user already had as the bundle has it: the home is pastor's now, so a
   later version changes it and a bundle that leaves takes it out. A later
   hand edit is `changed_by_hand`, as after any apply, and is not
   overwritten.

`--dry-run` lists the same rows, `old` included, and changes nothing: no
backup, copy, store or ledger. `--json` prints `{machine, ok, error, backup,
rows}`, `backup` being the dir on the machine. An adopt of every home
applies the whole plan, so it is recorded as an apply (see Keeping bundles);
one with `--agent` leaves the other homes, and the files every home shares
(`bin`), as they are, and is not. After an adopt the home is kept by plain
applies, `auto` or `manual`, like any other. The command exits 1 when the
machine could not be reached, or a row is a problem as for `apply`.

### Secrets

A bundle is copied to every machine that gets it, so `add` refuses one that
looks like it holds a secret (`bundle_secret`), naming each file and line and
what it looks like, never the value:

- files named like a credential store or a key: `.credentials.json`,
  `auth.json`, a name with both `oauth` and `token`, `.env` and `.env.*`,
  `*.pem`, `*.p12`, `*.pfx`, `id_*` but not `*.pub`, `.netrc`;
- GitHub, Slack and AWS tokens, `sk-` API keys and private key blocks;
- a value of 32 or more token characters, with letters and digits, set on
  a key whose name says token, secret, key or password; every such name on
  a line counts, so a minified JSON line or an inline TOML table is checked
  whole.

A value with eight or more of one character in a row (`sk-ant-xxxxxxxx`, a
placeholder in docs) and a UUID are not taken for secrets.

An MCP server or a hook gets its secrets by variable name (`env_from`), from
the machine's env.

### Naming bundles in flock.toml

```toml
[[flock]]
name = "ops"
default = true
machines = { build-01 = 2 }
bundles = ["base", "team-rules"]      # by id, each once
bundle_vars = { ticket_url = "https://tickets.example.com" }
bundle_apply = "auto"                 # auto or manual; unset is manual

[[machine]]
name = "build-01"
ssh = "user@build-01"
bundles = ["gpu-tools"]               # on top of its flocks'
bundle_apply = "manual"               # over its flocks'
```

A machine gets its flocks' bundles in file order, then its own, each once.
A head does not load a `flock.toml` that names a bundle the bundles dir
lacks or whose manifest does not load. Vars must fit: a flock gives every
var of the bundles it names, a machine's own bundles take theirs from its
flocks' `bundle_vars`, and two of its flocks may not give one var different
values. A var no bundle declares is let be, so a new var can be set before
the version that needs it is added. `bundle add` refuses a version that
would leave `flock.toml` short (`bundle_invalid`, naming each flock or
machine and the var it lacks), and copies nothing. A gap `flock.toml` had
before is not the new version's doing: the version goes in, and `add` warns
of the gap, which keeps a head from loading `flock.toml` until it is set.

A flock's `workdir_bundles` names workdir bundles (see [Stored workdir
bundles](#stored-workdir-bundles)); `bundles` names only home bundles, and
`workdir_bundles` only workdir ones. A head does not load a `flock.toml`
that mixes them up, and `bundle add` refuses a version whose `target` is not
the one `flock.toml` names it as. The flock gives the vars of both lists. A
machine has no `workdir_bundles`: a task lands per flock.

`bundle_apply` says how a machine takes a change to what it gets: `auto`
applies it, `manual` only marks the machine stale until a person applies it.
A machine's own `bundle_apply` wins; otherwise its flocks' when they agree,
else `manual`, so automatic is never forced on a machine. See Keeping
bundles.

### Keeping bundles

A running head keeps the bundles of its ssh and local machines (not pull or
`command` machines). Every apply, whether a person's `pastor bundle apply` or
the head's own, is recorded in `~/.local/state/pastor/bundles/applied.json`
on the head: when, whether it went through, and a fingerprint of what was
applied. A machine is stale when the plan the head renders now (the bundle
versions, the vars filled in, the agents' homes and the items) is not the one
last applied, or the last apply failed. A new bundle version, a `bundles`,
`bundle_vars` or `bundle_apply` edit, a machine joining a flock and an
agent's home moving all change the plan. A profile change changes it once a
plan carries profile rules; today the profile rules a harness takes in its
home are written before each launch, not by an apply.

| | `auto` | `manual` |
| --- | --- | --- |
| A change | the head applies the machine on its next pass (every 15 seconds) | nothing is written; the `bundles` row says `stale` |
| A task is about to start on a stale machine | the machine is applied first, then the task starts | the task starts with what is there |
| The last apply failed | the task is not started | the task is not started |

A task that is not started fails with `bundle_stale: <why>`: the last
apply's error, and the command to run once it is fixed (`pastor bundle apply
<name>`; `pastor task retry` runs the task again). In `auto` a plan
whose apply failed is not tried again on its own for ten minutes, and a launch
within a minute of the failure takes it as the answer instead of applying
again; a new plan, or a person's apply, tries at once. A machine no bundle is
meant for, and that has no bundle placed on it to take off, is not asked about,
whatever its last apply said: its tasks start, `machine check` has no
`bundles` row for it, and `pastor bundle apply` with no name leaves it out
(`skipped`) and records nothing for it.

The head reads every machine's homes against the plan every five minutes
(`apply --dry-run` there, which writes nothing) and in `pastor machine check`,
which adds a `bundles` row for each harness home. A home is:

| State | Meaning |
| --- | --- |
| `ok` | everything pastor wrote is there as it wrote it |
| `stale` | the plan is ahead of the home: a newer version, changed vars, an item to add or take out |
| `drift` | something pastor wrote was changed by hand (`changed_by_hand` rows) |
| `not adopted` | an item or a dir on its way is a symlink pastor does not write through |
| `conflict` | something that is not pastor's is where an item goes |

The worst one shows. A home that is not `ok` is a `warn` row: it does not
fail the check, and `pastor bundle apply` does not clear `drift`, `not
adopted` or `conflict` (it leaves what is not its own); they are fixed by
hand, or for `not adopted` and `conflict` by `pastor bundle adopt`, which
the row names. A machine whose homes cannot be read (pastor is not on its
PATH) is a `warn` row saying so. When the last apply to the machine did not go through,
the first row is a `fail` one with its error: tasks do not start there until
it is fixed. That includes a removal: bundles taken off a machine whose removal
failed are still on it, so the machine stays stale, `auto` tries the removal
again once the ten minutes are up, and the row and `bundle.failed` say so until
it goes through. An apply that never reached pastor on the machine (it is not
on the machine's PATH, or ssh failed first) placed nothing; one cut short after
pastor started it (a timeout, an answer that is no report) may have, so its
bundles count as on the machine in the same way. `pastor machine describe
<name> --json` has a `config` object, null when nothing is meant for the
machine and nothing was applied:

```json
{"apply_mode": "auto", "stale": false,
 "bundles": [{"id": "team-rules", "version": "2026.10.27.1", "hash": "…", "applied_hash": null}],
 "last_apply": {"at": "…", "ok": true, "error": null},
 "homes": [{"path": "~/.claude-main", "kind": "claude", "agents": ["claude-main"],
            "state": "ok", "adopted": true}],
 "checked_at": "…"}
```

`applied_hash` is the hash last applied when it is not the wanted one. A
home's `state` and `adopted` are what the head last read, null before it has
(and always null when no head runs); `adopted` is false for `not adopted` and
`conflict`. The text form lists the same under `bundles`.

The head sends these events (see Events), whoever ran the apply:
`bundle.applied` (detail: the bundles and hashes, rows added, updated and
removed), `bundle.failed` (detail: `error`), `bundle.drift` (a home that was
`ok` or `stale` has something changed by hand; detail: `home`, `agents`,
`items`), and `bundle.conflict` (a home came to a conflict, or an apply left
`conflict` or `linked` rows; detail: `items`). An apply a person ran before the
head started is not told again.

## Workdir bundles

A harness reads its config from two places: its home, shared by every
session of an account, and the directory the agent works in. Bundles keep
the homes. A workdir set is what goes in one working directory: the skills,
hook scripts, agent files, settings and instructions one agent starts with.
Whoever starts the agent hands pastor the set before the start, and pastor
places it, keeps a ledger of what it wrote, and takes out what a later,
smaller set no longer has. The website has a worked example, a reviewer set
placed and then started with the flags `place` answers.

```text
pastor workdir place --local <dir> --set FILE|DIR|- [--machine NAME] [--dry-run] [--json]
pastor workdir remove --local <dir> [--id ID] [--machine NAME] [--dry-run] [--json]
pastor workdir place <machine> <dir> --set FILE|DIR|- [--dry-run] [--json]
pastor workdir place <machine> <dir> --bundle ID --kind KIND [--dry-run] [--json]
pastor workdir remove <machine> <dir> [--id ID] [--dry-run] [--json]
pastor workdir check <machine> <dir> [--id ID] [--json]
pastor workdir list [<machine>] [--json]
```

With `--local`, `place` and `remove` run on the machine that has the
directory, and stay on that machine with a remote head set. Without it, they
name a machine of the flock and go through the head, which places the set
there (see [Through the head](#through-the-head)). An agent pastor started
may only run them with `--dry-run`, and may run `check` and `list`
(`agent_refused`).

### The set

One document, JSON or TOML, from a file or stdin (`-`):

```json
{
  "id": "reviewer",
  "kind": "claude",
  "skills": [{"name": "reviewing-a-pr", "files": {"SKILL.md": "---\nname: reviewing-a-pr\n---\n..."}}],
  "files": [
    {"path": ".claude/hooks/start.sh", "content": "#!/bin/sh\n...", "mode": "0755", "render": true},
    {"path": ".claude/agents/checker.md", "content": "..."}
  ],
  "settings": {
    "hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "{workdir}/.claude/hooks/start.sh"}]}]},
    "env": {"TICKET_URL": "{ticket_url}"},
    "permissions": {"additionalDirectories": ["{home}/notes"]}
  },
  "instructions": "You review pull requests for this repository.",
  "vars": {"ticket_url": "https://tickets.example.com"}
}
```

- `id` names the set in the directory: lower-case letters, digits, `.`,
  `_` and `-`, at most 64. Two sets with different ids share a directory;
  each is placed and removed on its own.
- `kind` is the harness of the agent that starts there. Pastor places sets
  for `claude` and `agy` (see [agy sets](#agy-sets) for where theirs go);
  `codex` and `opencode` are refused with `workdir_unsupported`.
- `skills` go to `.claude/skills/<name>/`, one file per entry of `files`;
  each needs a `SKILL.md`.
- `files` go to `path`, relative to the directory: no `..`, `.` or leading
  `/`, never in `.git`, and never `.claude/settings.json`,
  `.claude/settings.local.json`, `.mcp.json` or `CLAUDE.local.md`, the
  harness's own files. `mode` is octal, at most `0777` and readable by its
  owner; unset, a new file gets `0644` and a file pastor wrote keeps its
  own.
- `render: true` fills `{home}`, `{workdir}` (the directory's real path),
  `{kind}` and the set's own `vars` in a file. Any other `{name}` stays as
  written, and a file without `render` is placed as written. A set's vars
  may not take pastor's names: `home`, `workdir`, `machine`, `agent`, `kind`
  and `account`.
- `settings` merge into `.claude/settings.local.json` by the rules a home
  bundle's settings follow (see [Bundles](#bundles)): each hook group is an
  entry in its event's list, `env`, `enabledPlugins`,
  `extraKnownMarketplaces` and `permissions` go key by key, each of
  `permissions.additionalDirectories` is an entry of that list, and any
  other key goes whole. What else the file holds, such as a rule the agent
  was allowed for good, is not pastor's and stays. `permissions.allow`,
  `deny`, `ask` and `defaultMode` are refused with
  `workdir_permissions_owned`: what an agent may run comes from a profile,
  never a set.
- Claude reads `.claude/settings.local.json` and `CLAUDE.local.md` only
  with its `local` setting source on: an agent started with
  `--setting-sources` that leaves out `local` gets neither. pastor cannot
  see how the agent is started, so it does not check.
- `instructions` become a block in `CLAUDE.local.md`, between
  `<!-- pastor:workdir:<id> -->` and `<!-- /pastor:workdir:<id> -->`,
  after any text already there, which pastor never touches. When the
  repository tracks `CLAUDE.local.md`, the block goes in
  `.claude/CLAUDE.md` instead; when it tracks both, the block is
  `skipped`. Instructions may not hold a line that starts like one of
  pastor's markers, with their vars filled, and empty ones are no block.
  Claude also reads `CLAUDE.local.md` in each directory above the one it
  starts in, so a block placed in a clone reaches an agent started in a
  worktree below it too, beside that worktree's own.
- `{home}`, `{workdir}`, `{kind}` and the set's own `vars` are filled in
  every string of `settings` and `instructions`.
- `profile` names a permission profile (see [Permission profiles](#permission-profiles)),
  rendered when the set is placed (see
  [A profile in a set](#a-profile-in-a-set)); an agy set's must fit the
  machine's own. `mcp`, or MCP servers in
  `settings`, are refused with `workdir_mcp_unsupported`: a working
  directory has no file of its own for MCP servers, so a home bundle
  carries them.
- `git_exclude`, true unless the set says `false`, keeps the placed files
  out of git (see [Keeping the files out of git](#keeping-the-files-out-of-git)).

A set given as a directory has its manifest in `pastor-workdir.toml`, and
`from` in place of `content` or `files`: a file, or a skill's directory,
relative to the set's directory. The manifest and every file it points at
are plain files: a symlink anywhere on their way, or a FIFO or a device, is
refused, and so is a `from` that leads out of the set.

A set is at most 4 MiB, files and vars together; a directory set stops
reading at the file that would pass it. One that looks like it holds a
secret is refused (`bundle_secret`), by the rules under
[Secrets](#secrets): its files are written in plain text into a directory an
agent works in, often a repository. A var is checked as an env variable of
its name would be, a file with `render: true` as it will be written, its
vars filled in, and so is each string of `settings` and `instructions`; an
`env` entry of `settings` is checked as an env variable. A hook reads a
secret from the agent's env.

### agy sets

A set of kind `agy` goes where agy 1.4 reads a workspace's own files:

```json
{
  "id": "reviewer",
  "kind": "agy",
  "skills": [{"name": "reviewing-a-pr", "files": {"SKILL.md": "---\nname: reviewing-a-pr\ndescription: Use to review a pull request.\n---\n..."}}],
  "files": [
    {"path": ".agents/rules/style.md", "content": "---\ntrigger: always_on\n---\nKeep replies short.\n"},
    {"path": ".agents/agents/checker/agent.md", "content": "---\nname: checker\ndescription: Checks a change for tests.\n---\n# Checker\n..."},
    {"path": ".agents/hooks/start.sh", "content": "#!/bin/sh\necho '{}'\n", "mode": "0755"}
  ],
  "settings": {
    "reviewer-start": {"SessionStart": [{"type": "command", "command": "{workdir}/.agents/hooks/start.sh"}]}
  },
  "instructions": "You review pull requests for this repository."
}
```

- `skills` go to `.agents/skills/<name>/`. Rules (`.agents/rules/*.md`) and
  custom agents (`.agents/agents/<name>/agent.md`) are plain `files`. agy
  reads a rule file only with its frontmatter (`trigger: always_on` and
  the like); one without it is skipped.
- `settings` are named hooks, in the shape of agy's own `hooks.json`, and
  merge into `.agents/hooks.json`: each name is the set's whole, and what
  else the file holds stays. A name already there that is not pastor's is
  a `conflict`; one another set holds, `workdir_conflict`. Each hook is an
  object of its events. agy runs a hook's command in `.agents/`, so a
  path in it starts from `{workdir}`. agy has no local hooks file, so in a
  repository that tracks `.agents/hooks.json` a set's hooks are a
  `conflict`, `tracked by git`; put them in the committed file, or leave
  them out of the set.
- agy loads a folder's hooks in an interactive session only once its
  `trustedWorkspaces` lists that folder itself; a parent trusted does not
  count. A set with hooks for a folder the home's agy settings do not list
  gets a `skipped` row, path `trust`, `hooks need a trusted folder`; the
  hooks are placed all the same. A task pastor starts there has its folder
  trusted before the start. A run with `-p` loads them either way.
- `instructions` become a block in `AGENTS.md`, after any text already
  there. When the repository tracks `AGENTS.md`, they are a rule file of
  the set's own instead, `.agents/rules/pastor-<id>.md`, which starts with
  `trigger: always_on` so agy reads it.
- `files` are never `.agents/hooks.json` or `AGENTS.md` (pastor merges
  into them), `.agents/skills.json` or `.agents/mcp_config.json` (they
  point agy at skills and MCP servers elsewhere), nor anything under
  `.agents/teamwork/`, agy's own state, or `.agents/workflows/`, which agy
  no longer reads.
- agy's permissions are per home, so an agy set writes nothing
  permission-shaped. It may name a `profile` all the same: it must be one
  the machine's own profile covers (`agy_profile_mismatch`, as for an agy
  task), and the answer carries the flags the launch adds (see
  [A profile in a set](#a-profile-in-a-set)).

### Placing and removing

`place` answers one row per file, settings key and block, in the shape of
`bundle apply`'s, with the path relative to the directory and, for a
settings key, the key:

```json
{"target": "workdir", "dir": "/home/user/src/app", "id": "reviewer", "dry_run": false, "ok": true,
 "rows": [{"bundle": "reviewer", "path": ".claude/hooks/start.sh", "action": "added"}]}
```

- A file, key or block is `added`, `kept` or `updated`, and one the set no
  longer has is `removed`; a settings or instruction file pastor made goes
  once nothing else is in it, and the directories pastor made for a file
  go once they are empty. Placing the same set again is all `kept` and
  writes nothing.
- A settings key that holds another value already, not pastor's, is a
  `conflict` and stays. A block someone edited is `changed_by_hand`.
- A file pastor wrote and someone changed since is `changed_by_hand` and
  left as it is; it stays in the ledger, and comes out once it holds what
  pastor wrote again.
- Something else where a file goes is a `conflict`, left as it is. The same
  file there already, as the set has it, is `kept` but not pastor's: it
  stays when the set drops it. If its mode is not the one the set asks
  for, it is a `conflict` instead: pastor does not change it, and as it is
  a hook may not run.
- Pastor never writes through a symlink: the file itself, or a directory
  between it and the working directory, is a `linked` row. The working
  directory may not be a symlink, nor the home; a directory above it may
  be, and pastor uses its real path.
- A file pastor changes is written beside itself and renamed over, only if
  it still holds what pastor read (else `busy` after three tries), and
  copied to the ledger's `backup/` first.
- A row `conflict`, `linked`, `changed_by_hand`, `busy` or `failed` makes
  `ok` false and the command exit 1.

Two sets share a directory, and its settings and instruction files: each
has its own block, and keys of its own. A hook group or a directory two
sets both add stays until the last of them goes. A set that wants a file or
a settings key another set there holds, or a key on the way to one, is
refused with `workdir_conflict`, naming both sets, and writes nothing; so is
one whose file sits on the way to, or under, a file of another set. Within
one set, a file in the way of the settings or instruction file it merges
into is `workdir_invalid`.

`remove` takes out what pastor placed for the set `--id`, or for every set
in the directory without it, as long as each file still holds what pastor
wrote. It works for a directory that is gone, to clear its ledger: the path
is taken as `place` took it, `.`, `..` and a trailing `/` aside and a link
above it followed. Where pastor placed nothing, it writes nothing.

The ledger is never inside the directory, where the agent works:
`~/.local/state/pastor/workdirs/<key>/ledger.json`, `<key>` the first 16 hex
digits of the SHA-256 of the directory's real path. It names the directory,
each set's hash, the profile it names with a hash of its rendering, and
when that version was first placed, and every file with the hash of what
pastor wrote. A set is in it while it holds something of the set, or once
the set was placed with no problem row. Its `backup/` and the lock that
lets one place run at a time in a directory sit beside it.

### Through the head

`pastor workdir place <machine> <dir>` hands the set to the head, which
places it on that machine and answers the rows, so a launcher on any
machine places a set and then starts its agent. The head reaches push
machines, those with `ssh` or `local`. A pull machine, or one reached
through a command bridge, is refused with `workdir_unsupported`: run
`place --local` on it.

- **What the head does.** It reads and checks the set, and renders the
  profile the set names as for a task on that machine (with the machine's
  own profile, as `--machine` does for `--local`), from pastor.toml as it is
  on disk. It then sends both the way it applies bundles: packed into a run
  directory under `~/.cache/pastor/setup` on the machine, where `pastor
  workdir apply --local` places them with the same code as `place --local`
  and answers with the rows. So `{home}` and `{workdir}` are the machine's.
  The directory is an absolute path on the machine (`workdir_invalid`
  otherwise). The head takes it as written, a trailing `/`, `.` and
  repeated `/` aside: `/w/` and `/w` are one directory. A `..` or a
  symlink is resolved on the machine only, so the head's record has a line
  for each way it was written; give the directory the same way each time.
- **The answer** is `place --local`'s, with `machine` added. A machine that
  gives no answer (it cannot be reached, pastor is not on its PATH, the run
  is cut short) is `workdir_failed`. The machine's own refusal comes back
  with its code. One place or remove runs at a time in a directory, and
  the head waits for the machine before it answers.
- **The record.** A real place, one that is not `--dry-run`, is recorded in
  `~/.local/state/pastor/workdirs.json` on the head: machine, directory,
  set id, kind, profile, the set's hash and its rendering's, when, and
  whether it went through (`ok`, or `failed` with the rows or the error that
  stopped it). The head keeps each set it placed, by hash, under
  `workdir-sets/` beside it. A `remove` takes out the record of each set it
  took out. A set with a problem row, such as a file changed by hand, stays
  on the machine and in the record, while the others in the directory go.
  A set the record names that the machine no longer has goes too. A dry run
  records nothing.
- **The size of the answer.** The machine answers in at most 64 KiB, enough
  for a few hundred rows. A set with more files than that is placed on the
  machine, but the head gets no answer it can read: it answers
  `workdir_failed` and records the place as failed. Place such a set with
  `place --local` on the machine.
- **`check`** reads each set the head recorded for the directory (only
  `--id`, when given) on the machine, all in one run: each is placed again
  as a dry run, with the profile as it renders now. Each answer has the
  rows and the set's `state` and `items` (see
  [Kept and checked](#kept-and-checked)). It is `ok` only when the state
  is `ok` and every row is `kept` (or `skipped`), and exits 1 otherwise;
  `--json` answers one object per set. A check writes nothing. A directory
  the head placed nothing in is `workdir_not_found`.
- **`list`** prints the head's record, for one machine or all: machine,
  directory, id, kind, profile, result, when it was placed, and its state
  and when that was read (`-` before either). It reads the record only,
  not the machines.
- **Events.** A real place raises `workdir.placed`, or `workdir.failed` (with
  `error`) when it did not go through, once its record is written. A remove
  raises `workdir.removed` for each set it took out, also for one placed
  with `--local`, whose `kind` and `profile` the head does not know and
  gives as null. The detail is `{dir, id, kind, profile}`, with the machine
  on the event. The head's own place raises `workdir.placed` with `auto:
  true`, and its reading raises `workdir.drift` and `workdir.removed`
  (see [Kept and checked](#kept-and-checked)).
- A request goes over `pastor bridge` like any other, so a tool on another
  machine needs no library: a set travels inline in the request, as the
  document `place` reads. The 4 MiB is measured as `place --local` measures
  it, by the files, vars, settings and instructions, not by the JSON text,
  which escaping makes longer.

### Kept and checked

The head keeps what it placed on push machines. Every 5 minutes it reads
each machine it placed sets on, in one run per machine (a run asks about
at most 100 sets, so the answer stays small), and writes what it found in
its record. A read places each set again as a dry run, as `check` does,
and folds the rows into one state, as a home bundle's `bundles` row does:

| State | What it means |
|---|---|
| `ok` | what pastor placed is there as it placed it |
| `stale` | placing the set now would change something: a file was taken out, the profile renders otherwise, or `place --local` placed another version there since |
| `drift` | something pastor placed was changed by hand |
| `not adopted` | a symlink is on the way to something pastor placed |
| `conflict` | something that is not pastor's is where the set puts a file, or the directory cannot be read |
| `gone` | the directory is not there |
| `missing` | the directory's ledger no longer names the set: it was removed on the machine |

Each state but `ok` comes with a few items, such as `changed by hand
.claude/agents/checker.md`.

- **A changed profile.** The head renders each recorded set's profile
  every 15 seconds. When it renders otherwise than when the set was
  placed, the set is `stale` at once. On a machine whose `bundle_apply` is
  `auto` (see [Bundles](#bundles)), the head then places the set again, as
  `place` does, with `workdir.placed` and `auto: true`. If that fails it
  tries again after 10 minutes, or at once when the profile changes again.
  In `manual`, the default, it only says `stale`; `place` again settles it.
  A set the machine holds in another version is never placed again on its
  own: the head would undo that place. Only a version change places a set
  again; a file changed or taken out by hand is reported, not put back. A
  profile that no longer renders (`unknown_profile`, `profile_not_allowed`)
  makes the set `stale` with the error, and it is neither placed again nor
  removed; its directory is still read, so it can be `gone` or `missing`.
  A set that reads `drift`, `not adopted` or `conflict` keeps that state
  when its profile changes too, with the change as one more item. A
  session already running keeps the rules it started with.
- **`workdir.drift`** is raised when a set that was `ok` or `stale` reads as
  `drift`, `conflict` or `not adopted`, with `state` and `items` in its
  detail. A set with no state yet (never read nor placed with one) raises
  none on its first read.
- **A directory that is gone** is `gone` on the first read that finds it so,
  and its ledger and record stay: a directory on a volume that is only
  unmounted for a while comes back as it was. When a read at least five
  minutes after that one finds it still gone, the machine drops its
  ledger, and the lines the repo's
  exclude file had for it, and the head drops its record with
  `workdir.removed`, `reason: gone`. A set removed on the machine leaves the
  record at the first read, with `reason: removed on the machine`. The sets
  the head kept that no record names any more go too.
- **`machine check`** has a `workdirs` row for a push machine the head placed
  sets on, from a read that writes nothing. It is one line, each directory
  counted at the worst of its sets, such as `12 dirs: 11 ok, 1 drift`. It
  is a warning when a directory is not ok, naming the worst three and the
  fix, or when the machine holds more than 2,000 ledgers. It is never a
  failure: no task depends on a workdir set.
- **`machine describe`** has a `workdirs` section, and `--json` a `workdirs`
  key (`null` when the head placed nothing there):
  `{count, states, read_at, ledgers, worst}`. These are the directories,
  how many stand at each state (`unread` before the first read; `reported`
  on a pull or command machine, which the head never reads), when the
  machine was last read, the ledgers it holds then, and up to five
  directories that are not ok as `{dir, id, state}`, worst first.
- **At most 2,000 ledgers.** A machine keeps a ledger for each directory it
  placed a set in, also with `place --local`, where no head reads it.
  Past 2,000 on a machine, each place there drops the ledgers of
  directories that are gone, the least recently written first, with their
  exclude lines. A directory still there keeps its ledger however many
  there are.
- A pull machine, or one reached through a command bridge, is not read: the
  head cannot reach it. Its record is what the machine reported
  (see [Pull and command machines](#pull-and-command-machines)).

### Pull and command machines

The head places sets on the machines it reaches, with `ssh` or `local`. A
pull machine, which the head cannot reach, and a command machine, which
runs no shell, place their own: whoever starts the agent there runs `pastor
workdir place --local` (and `remove --local`) on the machine itself, with
the head set as for any command there. Nothing about the set changes; the
head's record is what this adds.

A pastor task on a pull machine is the exception: the head hands its sets
in the claim, and the machine's serve places and reports them itself (see
[Pastor tasks](#pastor-tasks)).

- **The report.** After a real `place` or `remove` that wrote something,
  with a remote head set, the command tells the head what it did, in one
  `workdir_report` request: the set's id, kind, profile, hash and its
  rendering's hash, when, and whether it went through (`ok`, or `failed`
  with the problem rows). The head records it as it records its own
  places, so `workdir list` shows the machine, and raises `workdir.placed`,
  `workdir.failed` or `workdir.removed` with the machine on the event. A
  dry run reports nothing. The head keeps no copy of the set, so it cannot
  `check` a directory it did not place: that is `workdir_unsupported` on a
  pull or command machine, and `list` is all it shows. The head refuses a
  report from a machine it reaches itself (`workdir_unsupported`: it
  places there, and its record is the truth) and from one it does not have
  (`unknown_machine`); a report older than the record's `placed_at` is left
  out, so a late delivery does not undo what happened since. A removal
  for a set the head has no record of (sent again, or answered late) raises
  no second `workdir.removed`.
- **The name.** The machine is `--machine NAME` (now also on `remove`),
  else `machine` under `[shepherd]` in pastor.toml, else the hostname. The
  head takes the name the request gives, except in what a shepherd's tick
  delivers: that bridge is for one machine, and the head takes its name
  whatever the request says. A `--local` run goes over whatever bridge its
  own ssh key reaches, so its name is the one it gave.
- **Whose shell.** Reports come from a person's or a launcher's shell. A
  task's pane sends none: the bridge it has allows no report, and a task is
  refused a real `place` or `remove` before that.
- **A silent head.** A set with no profile is placed all the same, and its
  report waits in `workdir-reports/` under the state directory, one file
  each, with a line on stderr. The next report from this machine, or the
  next tick of `pastor serve` on a pull machine, sends what waits in
  order; a command machine, which runs no serve, sends it with its next
  `--local` run. A head that refuses a report loses it. A set that names a
  profile needs the head's pastor.toml, so with the head silent it fails
  `head_unreachable` before anything is written: never a copy of the rules
  that may be stale. The run waits up to 5 seconds for the head, so a head
  that drops packets does not hold up a launcher that places, then starts its
  agent. One delivery at a time takes the queue (a lock in `workdir-reports/`);
  another finds it taken and sends nothing.

### A profile in a set

A set that names a `profile` gets the profile's rules in
`.claude/settings.local.json`, so a launcher does not hand-write the lists
for each agent: they come from `pastor.toml` and are rendered as
`pastor profile render <profile> --kind claude --json` does, with the same
refusals (`unknown_profile`, `profile_not_allowed`).

- **Where the rules come from.** `place` reads pastor.toml as the head
  reads it: the file on this machine, or the remote head's when one is set.
  The built-in profiles need no file. `--machine NAME` settles the profile as
  for a task on that machine of the flock: `unrestricted` goes only where
  the machine's own profile is, and `unknown_machine` answers a name the
  flock does not have. Left out, the rules that look at the machine are not
  checked.
- **What is written.** `permissions.allow`, `permissions.deny`,
  `permissions.ask` (empty, so no rule asks where the profile allows) and
  `permissions.defaultMode` (`dontAsk`), each a key of its own, beside the
  set's `additionalDirectories` and the other keys of its `settings`. That
  is a Claude set: agy's rules are per machine, in its own settings, so an
  agy set that names a profile gets no file for it. It must be one the
  machine's own profile covers, as for an agy task (`agy_profile_mismatch`,
  checked with `--machine`, or by the head, which knows the machine); the
  answer still carries its `flags` and `env`.
- **Pastor's whole.** Those keys are pastor's whole while it owns them: each
  `place` replaces them, and a rule the agent added since (an "always
  allow" answered in the session) goes. The row of a key that lost rules says
  how many, as `agent_rules_dropped: N` in its detail. Rules in a file pastor
  did not write are another matter: they are a `conflict` and stay. A set
  that drops its profile, and `remove`, take the keys out again, with
  whatever the agent added to them.
- **The launch.** The answer carries the rendering's `flags` and `env` as
  `profile: {name, flags, env, unsupported}` (and a line of text without
  `--json`): the args and env the launcher adds to the agent's start, so
  one answer gives the files and the command line. Pastor does not start the
  agent.
- **The ledger.** A set's record holds the profile it names and a hash of
  its rendering beside the set's own hash, so a changed profile in
  pastor.toml is a new version of the set at the next `place`.

### Keeping the files out of git

In a repo checkout a placed file would show in `git status`, and an agent
would commit it. So `place` lists each placed path in the repo's
`info/exclude`, in a block of its own:

```text
# pastor:workdir
/src/app/.claude/agents/checker.md
# /pastor:workdir
```

- The file is the one `git rev-parse --git-path info/exclude` names. Git
  reads one for a repo and all its linked worktrees, so a line covers a
  task's worktree and the clone alike. A directory below the top of the repo
  gets lines anchored at the top. The row is `updated` for lines added and
  `removed` for lines taken out, with the exclude file as its path.
- A path the repo already ignores (a `.gitignore`, a global excludes file)
  gets no line.
- `AGENTS.md`, `.agents/hooks.json` and `.claude/CLAUDE.md` (where a Claude
  set's block goes when the repo tracks `CLAUDE.local.md`) are files people
  commit with the project, and a line would hide the user's own from `git
  status` in every worktree. So a set that merges into one gets a line for
  it only when pastor made the file there. One that was there before,
  tracked or not, stays in `git status` with the set's part in it.
  `CLAUDE.local.md` and `.claude/settings.local.json` are local to the
  harness and always get their line.
- Git reads one exclude file for every worktree, so a line for a shared file
  pastor made in one worktree would also hide a user's own untracked copy of
  it in the clone or another worktree. Before it adds such a line, pastor
  looks at the repo's other worktrees (`git worktree list`): if one has the
  file, untracked and not ignored some other way, there is no line, and a
  `skipped` row names the worktree. Each `place` looks again, so the line
  comes once the copy is gone and goes when a copy appears. What is left is a
  copy made after the line was added: it stays hidden until the set is
  placed again or removed. A copy that the other worktree tracks hides
  nothing there, and is no reason to skip the line.
- A path git tracks is not written over: its row is a `conflict`, `tracked
  by git`, and the file stays as it is. A file pastor placed earlier and
  someone committed since is still pastor's and is kept as before. The
  check applies to `"git_exclude": false` as well; that setting only leaves
  the files out of the exclude file, for a set that wants its files tracked.
- What each directory asked for is in `exclude.json` beside its ledger. The
  block holds the lines of every ledger that names the same exclude file,
  so a line goes only when no set in any directory of the repo has its path
  any more: `remove` takes a set's lines out, and the block with the last of
  them. Text outside the block is never touched.
- With no repo there is nothing to do. On a machine without `git`, a
  directory that holds a `.git` is placed in all the same, with a `skipped`
  row `not excluded: git not found`.
- The exclude file is `info/exclude` in the repo's common git dir, by its
  real path: two paths to one repo are one file. If `.git`, `info` or
  `exclude` is a symlink, or the exclude file cannot be written, the files
  stay placed and the row is `failed`. A name with a control character
  cannot be listed (the set is refused when it names one).
- A path git tracks is not recorded as placed: a set whose every file is
  refused that way is not in the ledger.
- `--dry-run` names the lines it would add, as a real run would (not a path
  the repo ignores, nor one refused), and writes nothing.

### Pastor tasks

A task pastor starts can carry a set too, placed in the folder its agent
starts in before every start.

```text
pastor task run "review the open pull request" --repo ~/src/app --worktree \
  --profile review --workdir-set reviewer.json
```

```toml
# fragment of a job file: each task places the set beside it
[dispatch]
prompt = "review {{ item.title }}"
workdir_set = "sets/reviewer.json"
```

- **Naming it.** `task run --workdir-set` takes what `workdir place --set`
  takes: a JSON or TOML file, a directory with a `pastor-workdir.toml`, or
  `-` for stdin (not beside `--prompt-file -`). A job's `[dispatch]
  workdir_set` is a path relative to the jobs directory, read when the job
  loads: a set that does not read makes the job invalid. Keep sets in a
  subdirectory such as `sets/`, since a `.toml` file at the top of the jobs
  directory is read as a job. An edit to a set file is picked up as an edit
  to a job file is. A job run on
  another machine and submitted to the head may not name one
  (`workdir_unsupported`), since its path is on that machine. The set
  travels with the task, so a retry carries it too.
- **Checked when it is queued.** The set is read and checked as `place`
  checks it (`workdir_invalid`). It must be for the task's kind of agent
  (`workdir_kind_mismatch`). A set that names a profile must name the
  task's own (`workdir_profile_mismatch`); one that names none takes the
  task's when it is placed, so the files and the launch flags come from one
  profile. Claude adds the allow lists of `.claude/settings.local.json` to
  those of the launch flags, and a deny in either wins, so the two together
  are the profile as before.
- **Where it runs.** On a push machine the head reaches with a shell
  (`local` or `ssh`), the head places the set. On a pull machine its own
  serve places it (see below), so a pull machine takes a task with a set
  once its claim says its pastor places sets. A task pinned to a `command`
  machine, or in a flock with no machine of the other two kinds, is refused
  when it is queued (`workdir_unsupported`). One pinned to a pull machine
  whose pastor does not place sets waits, and `pastor queue` says to
  upgrade pastor there.
- **On a pull machine.** The head does not reach a pull machine, and its
  serve cannot read the head's pastor.toml or bundle store. So the head
  hands each task it claims with what placing takes: the task's set
  settled, the profile it names rendered for that machine, and the set each
  stored bundle gives the agent's kind, built from the newest version kept.
  The serve places them in the task's folder before its agent starts, as
  `workdir place --local` does, and reports each place and removal to the
  head (`workdir_report`), so `workdir list` shows them. A start later in
  the same claim, such as a resume, places the copies that claim brought:
  the set and the profile's rules as they were when the machine claimed
  the task, not the newest version, since the serve has no way to ask the
  head for them. A set the head cannot build or render at the claim (a
  bundle no longer kept, a profile that does not render) fails the task on
  the head, `workdir_failed`, rather than starting it there without it. A
  claim carries at most 8 MiB of sets; a task past that waits for the next
  claim. The head does not read a pull machine's files, so its sets are not
  checked for drift. `task attach` of a pull machine's closed task with a
  set is refused, since the head cannot place it there; its stored bundles
  are left out of the reopen instead, and `task attach` says so (`not
  placed: ...`).
- **When it is placed.** In the folder the agent starts in (its worktree,
  else its repo, else the folder a task with no repo starts in), after the
  folder's trust is written and before the agent starts: on its dispatch,
  on a resume after a pause or a usage limit, and on a move to another
  model of the same kind. A fallback model whose agent is of another kind
  could not take the set, so the task skips it as a model the machine
  cannot run, and waits rather than move there. `task attach` of a task
  whose pane is gone places it before it opens the session again. The
  place goes through the head as `workdir place` does: it is in `workdir
  list`, raises `workdir.placed`, and is kept and checked like any other.
  A place that does not go through, or does not answer within half of
  `request_timeout`, fails the start, and the agent does not start:
  `workdir_failed: <why>`, with the rows that stopped it and `pastor
  workdir check <machine> <dir>`. A place that is slow is not cut off: it
  goes on and is recorded, so the cleanup still finds what it wrote.
- **Cleanup.** When a task's worktree is removed (`task close
  --remove-worktree`, of a paused task too, `remove_worktree`, `task
  sweep`), the sets the head
  placed in it are taken out with it: their lines in the repo's exclude
  file and the head's record go, with `workdir.removed`. A task that works
  in a folder it does not own (no worktree) leaves its set there for the
  next start.

### Stored workdir bundles

What every task of a flock should find in its folder, such as a repo's
skills or a block of instructions for it, is a bundle like a home bundle,
added with `pastor bundle add`, kept by version and checked for secrets the
same way. Its manifest says `target = "workdir"`, and its keys are a set's:

```toml
id = "repo-notes"
version = "2026.11.02.1"
target = "workdir"

[vars]
ticket_url = { description = "the team's ticket tracker URL" }

[[skill]]
src = "skills/reviewing-a-pr"

[[file]]
src = "agents/checker.md"
path = ".claude/agents/checker.md"    # where it goes in the folder, by a set's path rules
kinds = ["claude"]                    # optional: unset is every kind
render = true                         # optional: fill the vars in it

[instructions]
src = "instructions/repo.md"
per_machine = "instructions/machines/{machine}.md"

[settings.claude]                     # claude and agy only
src = "settings/claude.json"
```

```toml
# fragment of flock.toml
[[flock]]
name = "ops"
workdir_bundles = ["repo-notes"]
bundle_vars = { ticket_url = "https://tickets.example.com" }
```

```toml
# fragment of a job file: after its flock's
[dispatch]
prompt = "review {{ item.title }}"
workdir_bundles = ["style"]
```

- **The manifest.** A `[[file]]` takes `path` in place of `slot`, with
  `kinds` and `render`. `[settings.<kind>]` is for `claude` and `agy`, in
  the shape a set's `settings` take, so Claude's may set
  `permissions.additionalDirectories` but not the keys a profile owns
  (`workdir_permissions_owned`). MCP servers, plugins and requirements
  belong to a home and are refused. `bundle add` builds the set the bundle
  gives each kind and checks it as `place` checks a set, so a path outside
  the folder, a harness file, a file that is not text or a bundle with
  nothing for `claude` or `agy` is refused when it is added.
  `bundle list` and `show` say each bundle's `target`.
- **Which tasks.** A flock's `workdir_bundles`, then a job's `[dispatch]
  workdir_bundles`, each once. The list is settled when the task is queued,
  with who named each; `task describe` shows it. A bundle a job names must
  be a workdir bundle whose vars the task's flock gives (`unknown_bundle`,
  `bundle_invalid`), and a task's own set may not take a listed bundle's id
  (`workdir_conflict`). A job submitted from another machine may name them,
  since they are the head's.
- **What is placed.** Before each start, the head turns the newest version
  kept of each bundle into one set for the task's kind of agent: its `id`
  is the bundle's, it holds the skills, files, instructions and settings
  whose `kinds` take that kind, its vars are the task's flock's
  `bundle_vars` and `{flock}`, and the `per_machine` instructions are the
  machine's (a machine with no such file gets the shared text; one that is
  there and unreadable fails the start). `bundle add` checks every
  `per_machine` file the bundle holds as the machine it is for would get it,
  so a bad one is `bundle_invalid` at add. A new version reaches a task's
  folder at its next start. A
  stored set never names a profile: only the task's own set carries the
  task's rules. A bundle with nothing for the kind places nothing.
- **When and in what order.** Where and when a task's own set is placed
  (see [Pastor tasks](#pastor-tasks)), but only in the task's own worktree:
  the stored ones first, in list order, then the task's own, each as its
  own set. Two that place one file
  or set one key clash, and the start fails `workdir_failed`, naming both.
  `task attach` of a closed task places them again first.
- **Where they are not placed.** A stored bundle never decides where a task
  runs. On a pull machine whose pastor does not place sets, on one reached
  through a command bridge, and for `codex` and `opencode` agents, the task
  starts without them: `task describe` says why (`not placed: ...`), and the
  head raises `workdir.skipped` with the ids and the reason. A task whose
  last start left them out is reopened without them too, and so is a pull
  machine's task. A pull machine whose pastor places sets gets them built
  in the claim (see [Pastor tasks](#pastor-tasks)); one it leaves out at
  the start, for a reason only it can see (a checkout that was already
  open, an agent of another kind than the claim's), goes back in the
  task's report, so `task describe` and `workdir.skipped` on the head say
  the same as for any other machine. The first reason the head records for
  a bundle stands.
- **Only in a task's own worktree.** Every task of a flock places a bundle
  under the bundle's id, so in a folder several tasks share each start
  would replace the last one's version: an agy task starting beside a
  Claude task would take out the Claude files it still reads. So a task
  with no worktree (it starts in its repo, or in `~/pastor-tasks` with no
  repo), or one that joined a checkout pastor did not open, starts without
  its stored bundles, and `task describe` says `not placed: shared folder`.
  Its own set is still placed. A pull machine's claim of a task with no
  worktree marks them skipped on the head and builds none.
- **Kept and cleaned up** like the task's own set: each place is recorded,
  read and checked, and a removed worktree takes every set in it out.

`pastor workdir place <machine> <dir> --bundle ID --kind KIND` places a
stored bundle the same way for any launcher, through the head, with the
vars of the machine's first flock that names it, else of its first flock.
It answers nothing, and says so, when the bundle has nothing for that kind.

## The bridge

`pastor bridge` lets a CLI on another machine reach this machine's head over
ssh (`ssh <head> pastor bridge`), so the head never opens a network port. It
reads request lines on stdin, passes each to `pastor.sock` unchanged, and
writes each reply line to stdout until stdin closes. It never starts a head
or reads `flock.toml`; with no head running it writes one `no_head` error
line and exits non-zero. It checks nothing itself: the head refuses what it
would refuse from a local CLI, so ssh access to the head's user is access to
the fleet.

## The inbox

`pastor inbox send` hands a message to a running Claude Code session through
its inbox socket, the way another Claude session would, instead of typing it
into the session's terminal. It acts on the machine it runs on and never talks
to the head or reads `flock.toml`.

```text
echo "run the tests again" | pastor inbox send --session <uuid> --mode prompting
```

| Option | Meaning |
| --- | --- |
| `--session` | the session's id, as `sessionId` in its record under the config directory's `sessions/` |
| `--mode` | `prompting` or `bypass`: the permission class the session was launched with, written into the message as `from-mode` |
| `--config-dir` | Claude's config directory; default `$CLAUDE_CONFIG_DIR`, else `~/.claude` |
| `--wait` | seconds to listen for a receipt, default 3 |

The text on stdin goes to the session as one message of priority `next`; it
is never sent as `now`, which would close an open permission prompt as if
the user had declined it. The session sees it as a request from another
Claude session named `pastor`, not as something its user typed, and it cannot
answer a pending permission prompt with it. A closing
`</cross-session-message` in the text is broken so the text cannot end the
envelope early. Pastor adds no text of its own to a message.

The command finds the session's record in `<config dir>/sessions/`, checks
the socket in it connects, then listens on a socket of its own beside that one
(`<pid>-<8 hex digits>.sock`, new for each send, so sends from one process
at once never share one, and removed when the send ends) and names it as the
sender. Claude sends no
receipt for a message it took, so silence for the `--wait` window means it
was accepted; a message it did not take is answered on the sender's socket.
That is why every send waits the whole window.

It prints one JSON line, `{"result": ..., "detail": ..., "msg_id": ...}`, and
exits 0 only for `accepted`:

| `result` | Meaning |
| --- | --- |
| `accepted` | no receipt came, or a held message was released while pastor listened |
| `held` | the session holds the message for its user to approve (a bypassing session holds one that does not say `bypass`); it may still be approved later, or dropped after five minutes |
| `denied` | the user declined a held message |
| `expired` | a held message was dropped unanswered |
| `refused` | the session's settings refuse messages from other sessions, or the text is over 1,048,576 characters (Claude drops a longer line; nothing is sent) |
| `dropped` | the session's guard dropped it; `detail` says why: `duplicate` (the same text from the same sender within 30 seconds), `rate-limited` or `queue-full` |
| `no-session` | no record names that session, its socket does not answer, or pastor could not listen for receipts; `msg_id` is `null` |

Pastor itself sends a Claude task's prompt this way, unless the agent's
`[agents.<name>]` says `delivery = "type"`: the first prompt at dispatch, a
prompt held while the agent was blocked or sent again because the agent
did not take it, `task send` text with its Enter, the retry line after a short
usage limit, and an orchestrator session's last message. It sends only once `agent.list` shows the agent up and not
blocked, as `--mode bypass` when the task's agent args start Claude bypassing
its permission prompts and `prompting` otherwise. A local machine sends from
pastor's own process; an ssh machine runs its own `pastor inbox send`, with
the prompt on its stdin. Anything but `accepted` (a machine that cannot send, a
pastor there without `inbox send`, any other result) types the prompt as
before, and the head's log names the path each prompt took. A
prompt sent again, and each retry after a short limit past the first, carries
its attempt number, since the session drops the same text from the same sender
within 30 seconds.

The socket protocol is Claude Code's own and unofficial; it can change in any
release.

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
    -o ControlPath=<state dir>/ssh/head-%C -- <dest> <pastor> bridge
```

one request line in, one reply line back (see [The bridge](#the-bridge)).
It needs:

- key-based ssh from here to the head: `BatchMode` never asks for a password
  or a host key, so either one missing fails at once;
- pastor on the head, on the PATH a non-interactive shell there gets, or
  named with `--pastor` (the remote shell expands a `~` in it);
- `pastor serve` running there.

ssh stops on a system configuration file that another user owns
(`Bad owner or permissions on /etc/ssh/ssh_config.d/...`). That is how
every root-owned file looks inside a sandbox that maps file owners, such
as Codex's, so a Codex task's `pastor task done` could not reach its head.
A request that fails this way is tried once more with `-F ~/.ssh/config`
(`-F /dev/null` when there is no such file), which leaves the system's
configuration out and keeps the user's own host aliases. That try, and any
request whose state dir cannot be made, runs without the shared ssh master,
since such a sandbox keeps the state dir read-only. If `~/.ssh/config` is
refused too (a symlink into a root-owned store), a last try reads
`/dev/null`, which still reaches a `user@host`. A sandbox that
also hides the keys, `~/.ssh` or the network (Codex's default
`workspace-write` has no network) still cannot reach a remote head.

`head set` sends one ping first and saves nothing if it fails:
`head_unreachable` when ssh fails (the message carries ssh's stderr),
`no_head` when nothing runs there, `head_too_old` when that pastor has no
`bridge` or is older than this one (see [After an upgrade](#after-an-upgrade)).
Any command fails the same way later; it never falls back to this machine's
files.

These commands go to a remote head: `task run|list|describe|read|retry|priority|close|prune|send|done`,
`queue` and `queue move`, `machine list|add|remove|move|describe|check`, `flock list|add|join|leave|remove|describe|default|edit`,
`trust list|add|remove`, `profile list|describe`, `config edit`, `tick`, `job reload`, `events`, `watch`, `orchestrator`, and job commands for
the head's jobs (see [Jobs on a shepherd](#jobs-on-a-shepherd)). `machine list`'s first line
names the head by its ssh destination and shows its herdr as `-`. `task run`
leaves the rest to the head, which fills it from the task's flock and its
own `[defaults]`. Only `timeout` and `place` fall back to the built-in
defaults, not the head's `[defaults]`, when the flock sets neither.

They print and exit as they would on the head's own machine. The edits
are the head's own requests, so the head checks and reloads them;
`flock edit` and `config edit` fetch the head's file, open it here and send
it back. `flock list`, `flock default show` and `profile` read the head's
`flock.toml` and `pastor.toml` with the same request. `machine add|remove --herdr` still changes this machine's herdr
sidebar, which is where you look at the machines from.

Local on purpose, as with no head set: `completions`, `setup`, `head`,
`bridge`, `inbox` (it reaches this machine's Claude sessions), `connector`
(connectors are this machine's), `workdir` (it writes in a directory on
this machine), `config edit --local`
(this machine's pastor.toml, which its headless serve reads), and
`task attach` and `machine open`: they go to the machine directly, but ask
the head for the task, its flock.toml and its pastor.toml instead of
reading this machine's, so they need the head up. The head's own machine, the local one in its
flock.toml, is reached at the head's ssh destination.

These commands are refused with a remote head: `machine authorized-key` and
`bundle add|list|show|remove|apply`. The first prints a line naming the head's
pastor for the head's own `authorized_keys`; bundles are kept in the head's
config dir. With a remote head each fails with `remote_head_unsupported`,
naming the head to run it on.

### A headless serve

With a head set, `pastor serve` runs headless, as this machine's shepherd:
it runs the jobs in this machine's `jobs/`, this machine's connector hooks
and, when the head's flock.toml has this machine as `pull = true`, the tasks
the head hands it, on this machine's herdr. It has no queue and never reads
`flock.toml`.

- The new items a job run finds (not seen here, within `max_tasks_per_run`)
  go to the head together, in one request with the job's `[dispatch]` table
  as written and its unrendered prompt. The head applies
  its own `[defaults]` to what the table leaves out, renders each item with
  the id it gives the task, checks its paths and the flock, and queues and
  dispatches the tasks under the job's name. The keys it queued, and those
  it had seen already (a run whose reply was lost), are marked seen here.
- A head that does not answer fails the run with `head_unreachable`, a head
  too old for what the job's `[dispatch]` names (a model, a priority, a
  profile, a label, `preempt`, `summary`, a description) as in [After an
  upgrade](#after-an-upgrade), and a head with a job file of that name with
  `job_name_taken`: each counts as
  a failed run, backing the job off as a failing connector does, and no item
  is kept, so the next run asks the connector for them again. An item the
  head refuses for its paths is reported and skipped; one past its cap waits
  for the next run; any other refusal fails the run and holds the job's
  cursor, as a failed insert does.
- Every tick it reads the head's events past its cursor and
  hands them to this machine's hooks in order, saving the cursor after each.
  The first time it reaches the head it skips the head's history and starts
  from there. A hook with `only_own` hears only tasks of a job in this
  machine's `jobs/` that uses its connector (and records about no job, as on
  the head); a hook without it hears every head event in its `on`, with the
  item, prompt and summary text of other jobs' tasks left out. A head job with the same
  name as a local job counts as local, since ownership is read from the job
  file here.
- Events the head rotated out of its log before this machine read them are
  lost to the hooks: it logs a `head_events_gap` warning and goes on from
  the oldest record the head still has.
- A cursor past the head's newest event (the head moved, or its state dir
  was wiped) logs a `head_events_reset` warning and moves to the head's end;
  no hook fires for the records already there.
- Its database is `shepherd.db` in the state dir: the jobs' state and seen
  keys, the event cursor, the rows of the tasks it runs, and on a pull
  machine a copy of the trust the head saved for it. `pastor.db` is left
  alone.
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

`[shepherd]` in this machine's `pastor.toml` (`pastor config edit --local`)
tunes it: `machine` is this machine's name in the head's flock.toml (default:
the hostname), and `takes_flock_work = true` (or
`PASTOR_SHEPHERD_FLOCK_WORK=1`) takes any task the head would place here, not
only those pinned to it with `--machine`. On the head, `pull_lost_after`
(default `10m`) is how long a pull machine may stay silent before the head
counts it lost and its starting and running tasks go stale.

`pastor setup systemd` (or `launchd`) installs it the same way: the unit
runs `pastor serve --foreground`, which reads the head from `client.toml`.
Without a service, `pastor serve` starts it in the background, and `pastor
serve status` and `stop` work on it as on a head.

### Leaving a fleet

`pastor leave` is for a host that stops being part of a fleet: it removes what
pastor put there for itself, and says each thing it removed, or `nothing of
pastor's to remove`. That is every line of `~/.ssh/authorized_keys`
(`PASTOR_AUTHORIZED_KEYS` points elsewhere) that ends in one of pastor's
markers (`pastor:machine:`, `pastor:agent:`, `pastor:join:`), and these files
of the state directory when they are plain files: `head_key`, `machine_key`
and `agent_key` with their `.pub`, `known_hosts` and a `bootstrap.json` a join left unsent. A link or a directory
at one of those names is left, and said. It leaves everything else as it is:
lines without a marker, `~/.ssh/known_hosts`, the config, `flock.toml`, the
task database, the harnesses and herdr. `--dry-run` lists what would go. It
acts on the host it runs on, whatever head is set, and a task may not run it
(only the dry run).

### Pull machines

A machine the head cannot reach over ssh, a laptop behind NAT say, can still
run tasks: the head's flock.toml has it as `pull = true`, and its own
headless serve asks the head for work. The head never connects to it and
runs no actor for it; `machine list` shows its HOST as `pull`.

- Each tick the headless serve asks the head for more work, a few tasks at
  most. The head decides how many from its own view of the
  machine (`max_agents`, job slots, burst, flock room) and hands out the
  queued tasks pinned to it first, then, when it takes flock work, any task
  its flocks would place there now; each is `starting` on the machine before
  the reply. Ahead of them, and on top of that number, come the
  [`--now`](#starting-a-task-now) tasks pinned to it, which the serve starts
  before the rest. The serve runs them on this machine's
  herdr with the same machine actor the head runs, and reports every change
  back.
- `task run --machine <name>` (or a job's `machine`) sends a task to a pull
  machine as to any other. There is no `task run` flag of its own for this.
- A pull machine takes only the tasks pinned to it unless `[shepherd]
  takes_flock_work = true` in its pastor.toml, or `PASTOR_SHEPHERD_FLOCK_WORK`
  is `1` or `true` in the serve's environment.
- `[shepherd] machine` is its name in the head's flock.toml, the hostname
  when unset.
- Saved trust (`pastor trust add <machine> <repo>` on the head) applies to
  a pull machine's tasks as to any other's: its serve fetches the pairs saved
  for it, answers the trust prompt and reports it, and the head emits
  `task.trusted`, by the rules given with `task send --trust` under How it
  works. It never gets another machine's pairs.
- An agent's `pastor task done` on a pull machine reaches the head, which
  ends the row there (`done`, with the summary). The machine's serve ends
  its own row the same way at its next report, with the head's summary, so
  auto-close takes the pane once the agent is idle and reports it closed;
  one the head ended before the serve started it is closed there without
  an agent ever starting. The row stays `done` on the head and counts as
  live, holding its slot and its flock's seat, until the machine reports
  the pane closed, or reports the agent `failed` (gone, with nothing left
  to close), which closes the row with its finish and summary kept.
- A pull machine that sends neither a claim nor a report for
  `pull_lost_after` (head's pastor.toml, default `10m`) is lost, and its
  `starting` and `running` tasks go stale, as for any lost machine; a task
  its agent ended with `task done` is closed, its finish kept, since the
  machine may never report its pane closed. The clock starts when the head
  does. A task gone stale only for that silence goes back to `running` (or
  `blocked`, or `starting`) on the next report of its agent at work, since
  the head only stopped hearing of it; one the machine itself reported
  stale stays stale.
- When the head does not answer (it cannot be reached, has no serve
  running, or takes longer than the 60 s each request may wait), the pull
  machine's serve log shows one warning with `code=pull_head_silent`
  naming what went unanswered (a report, the bootstrap report, the claim,
  the trust fetch or, before the serve has read any of the head's events, that
  read), then the hooks' own `shepherd_needs_head` warning;
  neither repeats while the silence lasts. When the head answers again it
  logs `the head answers this machine again` and `the head answers again`.
  A refusal (an error reply) is an answer and is logged as before. A tick
  stops at the first request the head leaves unanswered and tries again at
  the next tick, so a silent head costs a tick one wait rather than one per
  request, and changes the agents make meanwhile wait for that tick.
  Silence that lasts `pull_lost_after` is the head's `machine.lost` above.
- Each claim carries the herdr and pastor versions the machine runs, and
  the head keeps them on the machine's enrollment record.

A pull machine's own key reaches the head through `pastor bridge`, and a
plain bridge passes any request as if from a person. Lock the key to the
machine instead: on the head, `pastor machine authorized-key <name> --key
<file> --shepherd --write` writes its marked line (`pastor:machine:<name>`)
to `~/.ssh/authorized_keys`, beside any line the agent key has:

```text
restrict,command="/path/to/pastor bridge --shepherd --machine laptop --gen 0",no-pty,no-user-rc,no-port-forwarding,no-agent-forwarding,no-X11-forwarding ssh-ed25519 AAAA... pastor:machine:laptop
```

Without `--write` it prints the line, and `--dry-run` says what it would
write. `--gen` is the machine's enrollment: the one its record on the head
has, or 0 in a new record when it has none yet, whose bootstrap reads `ok`
as no record did. Only a pull machine has a shepherd; any other is refused
with `not_pull_machine`.

`pastor bridge --shepherd --machine <name> --gen <n>` then serves that
machine's shepherd, and nothing else:

- Before each request it asks the head whether the machine is still in
  flock.toml with enrollment `n`. When not (its record moved past `n`, or
  it has none), the request is refused with `machine_revoked`, and the
  bridge stops. sshd fixes a connection's forced command when it logs in,
  so this check is what cuts off a connection already open.
- It passes on what an agent's bridge does (below), plus claims, task and
  bootstrap reports, saved-trust fetches, job submits and events. A claim,
  a report and a trust fetch are for the bridge's machine, whatever machine
  they name.
- A job's `[dispatch]` is pinned to the machine: `machine` set to it, and
  `flock` dropped, so its tasks go under the machine's own flock.
- Events are those about the machine: its own `machine.*` events, and
  those of tasks placed on it. The head cuts them before a page's limit,
  so the cursor moves on past other machines' records.
- It may fetch an artifact the head's cache already holds: the request,
  `{"op":"artifact_locate","name":..,"version":..,"target":..}`, is a line
  on stdin like any other, never an argument, so what it names does not
  show in `ps`. The reply is one line, `artifact` with the file's `sha256`
  and `size` (the head's own path left out), and then exactly `size` raw
  bytes of the file, so the caller reads the line, then the bytes, and checks
  their SHA-256 against the line. Only what is cached is sent, and never
  fetched on the shepherd's say: a name the cache does not hold is
  `artifact_not_cached`, a name that is not one plain path part each (`..`, a
  slash) is `invalid_artifact`, a cached file that no longer hashes to its
  recorded sum counts as not cached, and a file the bridge finds outside the
  cache directory (a path outside it, or a link in it that leaves) is not
  read: `artifact_outside_cache`. A file whose size changed since the head
  looked is `artifact_changed`; ask again. An agent's bridge refuses it.
- Anything else is refused with `not_allowed_for_shepherd`, and never
  reaches the head.

### Joining with a token

A pull machine can join the flock with a one-time token instead of having
its lines written by hand. On the head:

```bash
pastor join create --flock personal --tag gpu --ttl 30m   # prints the token, one word
pastor join create --replace pi-3                         # a token that takes over pi-3's name
pastor join list [--json]
pastor join revoke 3f9c0a7e5b1d2c48
```

`join create` makes an ed25519 key pair for the token. Its public half goes
on a line of the head's `authorized_keys` that runs only `pastor bridge
--join <id>`, with sshd's own `expiry-time` set to the token's expiry, so
sshd refuses the key once it runs out, with or without the line. Its private
half, the head's ssh destination (`head_address`, else `user@hostname`) and
the head's host key go into the token, `pjt_` and base64url, and nowhere
else: the head keeps the token's id and what it grants (the flock, the tags,
the name it may replace, the expiry), never the token. `--flock` must name a
flock, `--replace` a machine in flock.toml. The token is printed once; `join
list` shows the ids.

`bridge --join <id>` takes one request line, which must be a join, and reads
nothing after it: anything else is `join_token_invalid`, and the id in the
request is the one the line names. The head then:

1. checks the name and the keys, and refuses a name that is not letters,
   digits, `.`, `_` and `-`, starting with a letter or digit (the token
   stays good);
2. marks the token used, in one store statement, so of two machines racing
   one token one joins (`join_token_used`, `join_token_expired` or
   `join_token_invalid` otherwise). From here its line goes whatever
   happens;
3. refuses a name flock.toml has (`machine_name_taken`) unless the token was
   made with `--replace` for it; a `--replace` token joins as that name
   only;
4. adds the machine to flock.toml as a pull machine in the token's flock,
   with its tags, in place of the one it replaces;
5. enrolls it one generation past any record of the name, with the host
   key it sent and a bootstrap state of `pending`, which holds it back
   from dispatch until its bootstrap reports `ok`. A shepherd
   bridge of the machine it replaced is refused from its next request;
6. writes its shepherd line (`bridge --shepherd --machine <name> --gen
   <n>`) and its agent line (`bridge --agent --machine <name>`), removes the
   token's line, and raises `machine.joined`.

The reply names the machine and the herdr version it should run: `[artifacts]
herdr`, else the one most machines run, else pastor's default. Each pass of
the scheduler sweeps the tokens past their expiry, their lines and rows
both, raising `machine.join_token_expired`, and takes away the line of a
used token a failed join left behind; `join list` shows a used token for a
week. `join revoke` removes a token's line and row at once.

#### Joining from the new machine

On the machine that joins, as the login user that will own pastor (it refuses
root, and never calls `sudo` or a package manager), with the token the head
printed:

```bash
curl -fsSL https://cacari.co/pastor/install.sh | sh -s -- join       # asks for the token
pastor join --name pi-3 --json                                       # pastor installed already
pastor join --token-stdin < token-file                               # from a file or a pipe
PASTOR_JOIN_TOKEN=pjt_... pastor join                                # from the environment
```

`--name` is the machine's name in the head's flock.toml (the hostname when
unset); a `--replace` token joins as the name it was made for only. The token
is a secret, so it is never an argument, which `ps` shows. `pastor join` reads
it from stdin with `--token-stdin`, else from `PASTOR_JOIN_TOKEN`, else asks
for it at the terminal with echo off. It removes the variable from its
environment before it starts anything, so no command it runs inherits it.
`install.sh join` takes the variable out of its own environment the same way,
hands the token to `pastor join --token-stdin` on a pipe, and refuses a token
given as an argument. A token that does not decode is refused without
quoting what was given.

`pastor join` then, in order:

1. pins the head's host key from the token in pastor's own `known_hosts` (the
   state dir's), once, and only a line for that head: a line with a marker,
   a pattern or a list is refused (`join_token_invalid`). Every ssh after it
   checks the head against that pin strictly, so a different key stops it;
2. runs the `facts`, `platform`, `prerequisites` and `keys` steps of
   [bootstrap](#bootstrapping-a-machine), so a machine that lacks what it needs
   stops here, with the token unspent, and the machine key, agent key and ssh
   host key exist to send;
3. joins: it writes the token's private key to a 0600 file in a 0700 dir of
   the state dir, runs ssh with that file as its only key, sends the join
   request (the name and the three public keys) and deletes the dir, whatever
   happened: an error, a panic, or SIGINT, SIGTERM or SIGHUP, on which it also
   ends its ssh. A dir an earlier join left because it was killed past
   catching (SIGKILL) is removed by the next one. The key is on no command
   line, in no log and in no file pastor keeps. A head that refuses the key
   (a token used, revoked or expired) is `join_token_invalid`; one that cannot be
   reached, `head_unreachable`;
4. fetches herdr from the head's cache over the machine key the head now
   knows, at the version the head named in its answer (pastor's default when
   it named none): the head sends the size and SHA-256, then the bytes, and
   the file is installed at `~/.local/bin/herdr` only when both match
   (`artifact_checksum_mismatch` otherwise). The head downloads nothing for
   it: a herdr the head's cache does not hold is `artifact_not_cached`, so
   `pastor machine install` on the head, or any pull of that version there,
   fills it first;
5. runs the bootstrap steps with that herdr, then one of its own: `shepherd`
   writes `[shepherd] machine = "<name>"` in this machine's pastor.toml,
   keeping the rest, and installs the user service of `pastor serve`, which
   reads the head from client.toml (the `client` step writes the token's
   head there) and, from then on, reaches it with the machine key and the
   pinned host key (`ssh` uses both only on a machine that has both files);
6. reports the outcome to the head at once, over the machine key
   (`BootstrapReport`), after leaving it in `<state>/bootstrap.json`, 0600 and
   whole. The file goes once the head answered. If the head cannot be reached
   it stays, and the machine's `pastor serve` sends it before its next claim,
   each pass, until the head answers (a refusal other than "try again" is
   kept as `bootstrap.json.refused` for a person, a file pastor cannot
   read is dropped). A report that reached neither the head nor a file is the
   error `runtime_error`, and the head shows the machine `pending`: join again
   with a token from `pastor join create --replace <name>`.

`pastor join --skip-bootstrap` is for a machine set up beforehand: it runs
only the `facts` and `keys` steps, joins, then writes the `client` settings
and names the serve (`shepherd_name`). No herdr is fetched, no service is
installed, and the head is told at once.

The head holds a joined machine back from dispatch until its report says `ok`.
A step that fails (herdr not in the cache, a missing login, the user
service) is in the report as for any bootstrap, with its `why` and `fix`,
and `pastor join` exits 1 with the same codes (`bootstrap_missing`,
`bootstrap_failed`). The report is the one `pastor bootstrap --json` prints.
`pastor leave` removes `bootstrap.json` with the keys.

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
`head` or `shepherd`. `--wide` is ignored here: the two tables have no
DESCRIPTION column, and `--json` has each job's `description`.

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
machine's flocks, and `describe`, read or end (`task done`) the tasks placed on
its machine. `pastor bridge --agent --machine <name>` is that locked bridge.
It reads each request and passes on only those; anything else, a task on
another machine included, it answers `not_allowed_for_agent` without asking
the head. It learns a task's machine and a machine's flocks from the head,
never from local files, cuts a task list down to those flocks, and names the
task a request comes from itself, whatever the caller said.

To set it up for machine `pi-1`:

1. On `pi-1`, as the user its agents run as, make a key with no passphrase
   (`ssh-keygen -t ed25519 -f ~/.ssh/pastor_head -N ""`) and copy
   `~/.ssh/pastor_head.pub` to the head.
2. On the head, print the line for that key:
   `pastor machine authorized-key pi-1 --key pastor_head.pub` (or `--key -`
   to read it on stdin). It refuses a machine that is not in `flock.toml`,
   and edits no file. sshd runs the command through the user's shell, so it
   also refuses what that shell would read as syntax: a machine name that
   does not start with a letter or digit and hold only letters, digits, `.`,
   `_` and `-` (`invalid_machine`), a pastor path that is not absolute or holds
   anything but those and `/` (`invalid_path`), and a key that is not a key
   kind, its base64 and an optional comment, so one that comes with options of
   its own (`command=`, `restrict`) is refused (`invalid_key`).
3. Append the line to the head user's `~/.ssh/authorized_keys`. It runs
   `<this pastor> bridge --agent --machine pi-1` whatever command the client
   asks for, with no terminal and no forwarding:

   ```
   command="/path/to/pastor bridge --agent --machine pi-1",no-pty,no-user-rc,no-port-forwarding,no-agent-forwarding,no-X11-forwarding ssh-ed25519 AAAA... user@pi-1
   ```

Give each machine its own key: the machine named in the line is the only one
whose tasks that key reaches. A work flock's machine then cannot even list a
personal flock's tasks.

### Moving the head

To move the head from `laptop` to `pi-1`, so that `laptop` keeps the CLI and
runs its tasks as a pull machine. Every step names the machine it runs on.

1. On `laptop`, stop the head, so nothing changes while it is copied:
   `pastor serve stop` (or stop its systemd or launchd unit).
2. Copy the head's files to the same places on `pi-1`: `flock.toml`,
   `pastor.toml`, `jobs/` and `orchestrators/` (with their `.env` files)
   from `~/.config/pastor/`, and `pastor.db` and `orchestrators/` from
   `~/.local/state/pastor/`. The tasks, the trust table, the seen keys and
   the jobs' state travel in `pastor.db`. Install the connectors the jobs
   use on `pi-1` too (`pastor connector install`); they are per machine.
   Delete the copied job files from `laptop`'s `jobs/`: a job file left there
   is `laptop`'s own and would run there as well.
3. On `pi-1`, edit the copied `flock.toml`. `pi-1`'s own entry becomes
   `local = true` in place of its `ssh`; `laptop`'s becomes `pull = true`
   in place of its `local = true`. Each machine sets exactly one of `local`,
   `ssh`, `command` and `pull`. In `pastor.toml` set
   `head_address = "user@head.example"`, the destination the other machines
   reach `pi-1` by, so their agents know where to send `task done`.
4. On `pi-1`, run `pastor setup systemd` (or `launchd`). It starts the head.
   `pi-1` needs key-based ssh to every `ssh` machine in the file, as `laptop`
   had.
5. On `laptop`, run `pastor head set user@head.example`, then
   `pastor setup systemd` again: with a head set, its unit runs a headless
   serve, which claims `laptop`'s tasks from the head. If `laptop`'s name in
   the flock is not its hostname, set it first with `pastor config edit
   --local`: `[shepherd]` with `machine = "laptop"`.
6. For every machine other than `pi-1` whose agents report back, `laptop`
   included, give it a locked key: make it there, then on `pi-1` run
   `pastor machine authorized-key <name> --key <file>` and append the line
   to `~/.ssh/authorized_keys` (see [Agents on other
   machines](#agents-on-other-machines)). `laptop`'s CLI keeps its own,
   unlocked key.
7. From `laptop`, `pastor machine list` names `user@head.example` as the
   head and lists `laptop` as a pull machine; `pastor task list --all` shows
   the tasks copied in `pastor.db`.

`laptop`'s own `pastor.db` is left alone by a headless serve; delete it once
the move works.

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
  that changes the fleet (`agent_refused`):
  - `task run`, `send`, `attach` (herdr's agent terminal types into any
    task's pane), `retry`, `priority`, `close`, `prune` and `done`, and
    `queue move`;
  - `tick` (`--dry-run` too);
  - `job run`, `reload`, `enable`, `disable` and `edit`;
  - every `orchestrator` command but `list` and `describe`;
  - every `machine` command but `list` and `describe`, `machine open`
    included (herdr's full UI drives every pane);
  - every `flock` command but `list`, `describe` and `default show`;
  - `trust add` and `trust remove`;
  - `connector install`, `link`, `uninstall` and `unlink`;
  - `config edit`;
  - `serve` (but `serve status`) and `setup`: a head started from the pane
    would dispatch with nothing to refuse.

  A dry tick and a reload count because both apply `pastor.toml` and
  `flock.toml` first. `task done` is allowed only for the pane's own task:
  an agent may end its own task, and nobody else's. Reads still work,
  `describe` included. Nothing turns the guard off (a setting that did,
  `agents_change_fleet`, was removed; a pastor.toml that still sets it to
  `true` does not load, and `false` is ignored with a warning). It stops an
  agent acting on its own, not a determined one: it runs as the same user and
  can unset the variable.
- A task's role widens that guard for one task. `pastor task run --role
  orchestrator` starts an orchestrator: from its pane it may also run tasks
  (`task run`), retry, send to and close them (`task retry`, `task send`,
  `task close`), enable or disable a job (`job enable`, `job disable`) and
  keep its orchestrator's handover note (`orchestrator note`);
  everything else that changes the fleet is still `agent_refused`, `task
  prune` and file and machine edits included, and the message names the
  role. Only a person starts one: `--role orchestrator`
  from any task's pane is `role_refused`, an orchestrator's included, and so is a retry of an orchestrator
  from a task's pane, since the copy keeps the role. The head keeps the role
  (`role` in `task describe` and `task list --json`, `agent` for every other
  task), so these go through `pastor serve`: with no head, `job enable` and
  `job disable` from a task's pane are refused. Like the rest of the guard, a role is a guard
  against an agent's mistakes, not a boundary: an orchestrator runs as the
  same user as pastor and can do anything that user can.

The head's user is the fleet's trust boundary. Anything that runs as that
user on the head controls every machine in the flock file, because it can:

- talk to `pastor.sock`, which takes any request the CLI can make: queue a
  task with any prompt, agent arguments, repo and machine, type into a live
  task with `task send`, run a job, or edit the flock. The socket is 0600, so
  it keeps other users out, not other processes of the same user;
- write `flock.toml`, where a `command = [...]` machine is any argv, and the
  job files and the connectors (`~/.config/pastor/connectors/`,
  `~/.local/share/pastor/connectors/`);
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

## Descriptions

A name like `answered-pastor` or `life` does not say what a job does. A job,
a flock, a machine and a task can each carry one short line that does. It is
optional everywhere: none shows as `-`, and files without one load as they
always have.

```toml
# ~/.config/pastor/jobs/answered-pastor.toml
description = "Carry out the answers on the Pastor board's Answered list"
every = "1m"

[connector]
use = "clock"

[dispatch]
description = "{{ item.title }}"   # each task's; the default, so it can be left out
prompt = "..."
```

```toml
# flock.toml
[[flock]]
name = "life"
default = true
description = "Personal errands and side projects"

[[machine]]
name = "pi-3"
ssh = "user@pi-3"
description = "The one under the desk"
```

A job's own `description` goes at the top of its file, beside `every`. A
flock's and a machine's go in their entries in `flock.toml`;
`pastor flock add <name> --description <text>` and `pastor machine add
<name> <ssh> --description <text>` write them, and `pastor flock edit`
changes them. A connector's is the `description` in its manifest. Leading
and trailing whitespace is trimmed, and there is no length limit.

A task's is fixed when it is queued, from the first of these that says
something:

1. `pastor task run "<prompt>" --description "<text>"`;
2. its job's `[dispatch] description`, rendered with the item like the
   prompt (`{{ item.* }}` and `{{ job.name }}`; not `{{ task.id }}`, which
   is not known yet). With no such key it is `{{ item.title }}`, so a
   board card's task reads as the card. A path the item lacks renders
   empty, which counts as nothing;
3. the prompt's first line.

`task retry` copies it. Tasks from before descriptions read as their
prompt's first line. A headless serve's jobs send `description` inside their
`[dispatch]` like any other key, and the head renders it.

The lists stay as narrow as they were unless asked. `-w, --wide` on `pastor
task list`, `pastor job list`, `pastor machine list`, `pastor flock list`,
`pastor connector list` and `pastor orchestrator list` adds a DESCRIPTION column, last:

```text
NAME             SCHEDULE   ENABLED  FLOCK  CONNECTOR  LAST RUN  NEXT    RESULT  DESCRIPTION
answered-pastor  every 1m   yes      life   obsidian   20s ago   in 40s  ok      Carry out the answers on the Pastor…
```

When stdout is a terminal, the column is cut to what the other columns
leave of its width, ending in `…`, but it keeps at least 20 characters, so
a very wide table can still wrap. The width comes from the terminal itself,
else `$COLUMNS`. Piped or saved to a file, nothing is cut. A newline in a
description shows escaped, as `\n`. `task list` keeps its NOTE column as it
was (the error, else the item's title, else the prompt's first line), since
NOTE also carries errors and "retry of"; RESULT (see Task summaries) and
then DESCRIPTION come after it.

Every `describe` prints a `description:` line near the top, `-` when there
is none, whole and with its newlines. A task's also says where it came from:
`--description`, `job <name>`, or `the prompt`.

`--json` always has them: every object of a `list --json` and every
`describe --json` has a `description` key, `null` when there is none. A
task's is always a string, the one it resolved to, with `description_from`
beside it. `--wide` with `--json` changes nothing.

## Describe and edit

In the terminal pastor reads like kubectl: `list` shows many things, `describe`
one in full, `edit` its file.

Taking things away has three verbs, each with one meaning. `remove` takes one
thing you name (`machine remove`, `flock remove`, `trust remove`). `uninstall`
and `unlink` undo `connector install` and `connector link`. `prune` removes
many at once, chosen by state and age (`task prune`).

```text
pastor job describe <name>       schedule, connector and its config, dispatch, last runs and errors, next run, recent tasks and job events
pastor machine describe <name>   host, flock, session, model, profile, channel, herdr, protocol and pastor versions and when the pastor one was read, agents, orphans, tags, its tasks, recent errors
pastor flock describe <name>     default or not, its agent, agent args, allow and deny, model, profile, machines, live agents, queued and running tasks
pastor connector describe <id>   manifest, origin, commands, config, secrets set or missing, jobs using it, status
pastor task describe <id>        state, machine, agent and where it came from, prompt, error, summary (--all-summaries: every round's)
pastor job edit <name>           ~/.config/pastor/jobs/<name>.toml
pastor flock edit                ~/.config/pastor/flock.toml
pastor config edit               ~/.config/pastor/pastor.toml
```

Every `describe` takes `--json`. A description reads from the head when one
runs and from the files and the store when not; a machine is then probed
directly, as `machine list` does. `machine describe` and `flock describe` are
built by the head itself, from the flock it last applied rather than
`flock.toml` as it reads now, so a machine the head has not picked up yet is
`unknown_machine`. The job's `connector` and `dispatch` are the
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
fetches the head's file and a hash of it, edits a temp copy, and sends the
result back with that hash. The head checks it with the
same code as an edit with no head, against its own `[defaults]` and
connectors, and refuses a stale hash (`edit_conflict`); an invalid edit comes
back with the head's error and reopens the editor as above. An unchanged file
sends nothing.

## Files

```
~/.config/pastor/pastor.toml      tick, settle, reconcile_every, version_every, request_timeout, agent_ready_timeout, close_done_after, close_failed_after, pull_lost_after, max_orchestrators, head_address, defaults, agents, models, profiles, watch, shepherd (all optional)
~/.config/pastor/flock.toml       flocks and machines
~/.config/pastor/jobs/<name>.toml one job per file
~/.config/pastor/orchestrators/<name>.toml one orchestrator per file (and an optional .env for its scripts)
~/.config/pastor/client.toml      this CLI's `[head]`, from `pastor head set`
~/.local/state/pastor/pastor.db   tasks, summaries, seen keys, job state, trusted repos, the last event seq
~/.local/state/pastor/shepherd.db  a headless serve's job state, seen keys, head event cursor, its pull machine's trusted repos
~/.local/state/pastor/pastor.sock daemon socket
~/.local/state/pastor/fleet.lock  held by a fleet edit with no head, and by `pastor serve` while it starts
~/.local/state/pastor/events.jsonl events log (and events.jsonl.1, the previous one)
~/.local/state/pastor/watch/<name>.json   a `pastor watch` cursor
~/.local/state/pastor/orchestrators/<name>/  an orchestrator's state.json, note, scripts' scratch/ and runs/
~/.local/state/pastor/serve.log   a background `pastor serve`'s log, rotated at 10 MiB to serve.log.1 .. .3
~/.local/state/pastor/serve.json  the running serve's pid, service and log, for `serve status|stop`
~/.local/state/pastor/ssh/        one ssh ControlMaster socket per machine and host, and one (`head-<hash>`) for a remote head
~/.local/state/pastor/artifacts/<name>/<version>/<target>/  the artifact cache: each binary with its .sha256
~/.config/systemd/user/{pastor,herdr}.service   written by `pastor setup systemd`
~/Library/LaunchAgents/pastor.{serve,herdr}.plist   written by `pastor setup launchd` (macOS)
~/.config/pastor/connectors/<id>/.env   a connector's secrets and settings
~/.local/share/pastor/connectors/<id>/  installed connectors (a symlink for a linked one)
~/.local/share/pastor/connectors/.<id>.install.json   where `connector install` got it from
~/.local/share/pastor/provisioners/<id>/  provisioners copied by hand, beside the built-in ones
~/.config/pastor/bundles/<id>/    a bundle as it is now, with its pastor-bundle.toml
~/.local/state/pastor/bundles/<id>/  the last 5 versions of a bundle, by content hash, and versions.json
~/.local/share/pastor/bundles/<id>/<hash>/  on a machine: the bundle versions its agents' homes link into
~/.local/state/pastor/applied/    on a machine: ledger.json of what bundles placed, and backup/
~/.local/share/pastor/claude-marketplace/<home>/  on a machine: the plugin carrying a Claude home's MCP servers
~/.local/share/pastor/harnesses/<id>/<version>/  on a machine: a harness `machine install` put there, linked from ~/.local/bin
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
version_every = "2m"         # a machine is asked for its pastor version again after this
request_timeout = "60s"      # one herdr request, connect included
agent_ready_timeout = "30s"  # agent.start to an accepted prompt; below request_timeout
close_done_after = "5s"      # a done task's pane closes after this; "never" keeps it
close_failed_after = "5s"    # a failed or stale task's, or an orphan's, stopped agent's pane closes after this; "never" keeps it
pull_lost_after = "10m"      # a pull machine silent this long is lost, its tasks stale
max_orchestrators = 1        # orchestrator agents at once; each also takes a max_agents slot
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
# fallback = ["haiku"]       # [models] names tasks may fall back to; [] or unset: none
# priority = "normal"        # the level of tasks that set none: low, normal, high or critical
# age_after = "30m"          # a queued task waits this long per level it ages, up to high; "never" turns it off
# agents = { opencode = "opencode" }  # the agent for a model of another kind than agent's
# profile = "develop"        # a permission profile for tasks that name none; unset: none
# summary = "ask"            # ask for a summary in each prompt; require: also fail without one; off: neither
# keep_pane = false          # true: tasks keep their pane once they end, until `pastor task close`
# remove_worktree = false    # true: a task's worktree goes when it fails or goes stale too, not only done
# min_disk = "4G"            # free disk a machine with a floor needs on $HOME and /tmp, when its entry sets none; "0": no disk floor
# min_memory = "1G"          # free memory it needs, when its entry sets none; "0": no memory floor
# env = { NOTES_URL = "http://notes.example:8790" }  # env for every agent's pane, under a flock's, a machine's and the agent's own; no secrets
[agents.claude]              # one table per agent that needs one
kind = "claude"                  # the herdr agent it starts; default: the table's name
env = {}                         # env for its pane, e.g. { CLAUDE_CONFIG_DIR = "~/.claude-alt" }
trust_keys = ["Down", "Enter"]   # accept its folder-trust prompt; [] for none
trust_marker = "Yes, I trust this folder"  # saved trust presses them only while the pane shows this
allow_flag = "--allowedTools"    # the flag before each allow pattern
deny_flag = "--disallowedTools"  # the flag before each deny pattern
# own_permissions = false       # true: its own settings hold its permissions; a profiled task gets no flags and no check
# account = "me-personal"       # a label: machines naming it share its usage limits; unset: none
# delivery = "socket"           # text pastor sends (prompts, task send, the rate retry, an orchestrator's stop message) through Claude's inbox socket, typed when it does not take it; "type": always typed. Default: socket for kind claude, type for the rest
[[agents.claude.dialogs]]        # a startup dialog beyond trust; a built-in's name replaces it
name = "bypass"                  # its name, in task.answered
marker = "Bypass Permissions mode"  # text only that dialog shows
pick = "Yes, I accept"           # the option to choose by its text; or keys = ["Down", "Enter"]
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
[shepherd]                   # read by this machine's headless serve; see Pull machines
# machine = "laptop"         # its name in the head's flock.toml; unset: the hostname
takes_flock_work = false     # true: also its flocks' unpinned tasks, not only those pinned here
# command = ["fake-herdr"]   # developer option: argv speaking herdr on stdio; unset: this herdr
[limits]                     # usage limits; see Usage limits
wait_under = "30m"           # a limited task waits for a reset closer than this, else moves on; "0": never waits
rate_retries = 3             # a 429 or 529 is retried in its pane this many times before it counts as a limit
rate_backoff = ["1m", "5m", "15m"]  # the wait before each of those retries; the last one repeats
unknown_reset_wait = "1h"    # how long a limit whose message names no reset holds
retry_after_no_credit = "6h" # how long a no-credit limit holds
handover_lines = 60          # pane lines a task moving to another agent hands over
[artifacts]                  # the binaries the head puts on machines; see The artifact cache
herdr = "0.9.5"              # the herdr version; unset: the fleet's most common, else the built-in one
herdr_index = "https://herdr.dev/latest.json"  # herdr's release index, or a mirror of it; https only
```

`head_address` is the ssh destination other machines reach the head by
(`pastor setup` writes one; see [Run under systemd](#run-under-systemd)). When
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
task commands, connector ids after `connector describe|uninstall|unlink|try`
and `watch --connector`, orchestrator names after the orchestrator commands, and
the `[models]` names of pastor.toml after `--model`, profile names
after `--profile` and `profile describe`, the levels after `--priority` and
`task priority`, and the queued tasks, in queue order with their level,
after `queue move` and its `--before` and `--after`. A
static script cannot know them, so at TAB it runs `pastor __complete <shell>
-- <words>`, which reads the job files, `orchestrators/`, `flock.toml`,
`pastor.toml`, the connectors
directory and the task store directly, never the head, and prints nothing
when it cannot read them. Task ids come live ones first, newest first, and
fish shows each task's note beside it; beside a job, flock or machine it
shows its description (see Descriptions), and without one a flock's
`default` mark or a machine's flock, as before. Other shells get the static script
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
ln -s ~/src/pastor/skills/pastor ~/.claude/skills/pastor   # for you; ~/src/pastor is your checkout
mkdir -p .claude/skills
ln -s ~/src/pastor/skills/pastor .claude/skills/pastor     # for one project
```

An agent that pastor dispatched runs on a flock machine, where this checkout
may not exist; if pastor is installed there, it can run `pastor --skill`.
A unit test checks that every command and flag named by a skill exists, so a
CLI change that breaks one fails `make check`.

`skills/spec/SKILL.md` plans work for the flock. It starts from the
brainstorm, then writes a plan in which every task has a flock or
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

`skills/connector/SKILL.md` scaffolds a connector: it asks at most whether
the source is polled or streamed and which secrets it needs, then writes
`pastor-connector.toml` and a `poll.sh` or `stream.sh` from
`skills/connector/protocol.md` (a copy of the connector section above, so
the skill stands on its own), and tests the result with `pastor connector
try` and `pastor tick --dry-run`. It never runs `pastor connector install`:
that clones a pushed repository, which stays the user's own call.
`skills/connector/example/local-files/` is a small worked connector.

`skills/dispatch/SKILL.md` runs a `pastor:spec` plan's next task and
watches it, with nobody else looking at the branch: it reads the plan's
Pastor header and ledger with `skills/dispatch/next-task.sh`, starts the
next task once a machine in its flock is free, waits for it with no tight
loop, and handles a `blocked` folder-trust prompt, a `done` task's outcome,
a `failed` task with a known cause and a `stale` task by fixed rules.
Anything else goes to the user. It is disabled for automatic invocation,
since it starts agents on other machines; a person runs `/pastor:dispatch`
when they mean to.

`skills/review/SKILL.md` takes a plan branch dispatch finished: it runs the
plan's own check locally, reviews the diff fresh, sends back at most one
fix task for Critical and Important findings (appended to the plan as one
more numbered task, then started and watched the same way `pastor:dispatch`
starts any task) with one scoped re-review, defers
Minor findings to the ledger, and opens the pull request — as a draft
listing the findings still open if that re-review still finds a Critical
one. It merges only when asked, and is disabled for automatic invocation
for the same reason dispatch is.

The repository is also a Claude Code plugin named `pastor`
(`.claude-plugin/plugin.json`), which is how the skills other than the
built-in one travel: installed as a plugin, they are `/pastor:spec`,
`/pastor:connector`, `/pastor:dispatch` and `/pastor:review`; linked like
the one above, they are `/spec`, `/connector`, `/dispatch` and `/review`.

## Development

The Makefile is the list of things you can run here; `make help` prints it.

```bash
make check            # changelog entry check, fmt check, clippy with warnings as errors, full test suite
make test             # unit tests plus an end-to-end run against fake-herdr
make test-machine     # the machine actor tests on their own
make portability      # cargo check for the musl and FreeBSD targets; needs cargo-zigbuild, zig and the Rust targets
make smoke SESSION=s  # opt-in test against a real herdr running session s on this host
make smoke-profiles REPO='~/src/app' CLAUDE=pi-1 OPENCODE=pi-2 AGY=pi-3  # a live review task per agent through the head
make smoke-rc TAG=v1.2.0-rc.1 LABEL=arm64  # make smoke against a tag, in a scratch worktree, as a Markdown report
make build            # debug build of both binaries; cargo run --bin pastor -- --help works from there
```

`make check` is what a pull request has to pass. Nothing in the suite talks to
a real herdr, so run `make smoke` on a fleet machine before trusting it there.
`make portability` is not part of it; CI runs it on every pull request that
touches code, and it tells you what to install when something is missing.

To try pastor without herdr, run the fake one. It comes in the same two
pieces the real thing does, a server and a bridge per request, because state
has to outlive a single request. It builds only with the `fake-herdr` cargo
feature, which the Makefile turns on (a bare `cargo test` skips the
integration tests that need it). `make build` leaves it at
`target/debug/fake-herdr`; `make install` does not install it, only `pastor`:

```bash
FAKE_HERDR_AUTO_DONE_MS=500 target/debug/fake-herdr --listen /tmp/fake-herdr.sock &
pastor machine add fake --command "target/debug/fake-herdr --connect /tmp/fake-herdr.sock"
pastor serve --foreground            # its log in this terminal; ctrl-c stops it
```

`make smoke-profiles` starts real agents: through the running head, a task
under the `review` profile for each agent named (`CLAUDE`, `OPENCODE` and
`AGY` take a machine from flock.toml, `CLAUDE_AGENT`, `OPENCODE_AGENT` and
`AGY_AGENT` another agent of that kind; the agy machine's own profile must
be `review`, with its rules in agy's settings there), in the checkout
`REPO` on that machine. Each is asked to read,
then to write a file it must be refused, and passes when it ends `done`
without ever going `blocked`. It prints the end of each pane, to paste in a
pull request, and closes the tasks. The head must run the pastor under test,
since the head is what hands the agent its profile. Run it before trusting
a change to profiles.

`make smoke-rc TAG=<tag>` is what a release candidate gets before its final
tag: it checks the tag out in a scratch worktree under `$TMPDIR`, builds it
and runs `make smoke` there (`SESSION` picks the herdr session), then prints
one Markdown block with the tag, the machine's `LABEL`, the herdr version,
pass or fail per suite and the last lines of a failure, with paths and the
hostname scrubbed. With `PROFILES=1` and `REPO`, `CLAUDE` and/or `OPENCODE`
it runs `make smoke-profiles` too, and refuses unless the running head
answers with the tag's version (`serve status --json`, over ssh when `head
show` names a remote head), since the head must run the candidate; `PASTOR`
picks the CLI for both. The exit status is the result; the worktree is
removed either way.

## Internals

What follows is how pastor does things, for someone working on it or
debugging it. Nothing here is needed to use it.

### Connections to herdr

herdr answers one request per connection and then closes it, so pastor opens
a connection per request, and how it opens one follows the machine's kind: a
`local` machine dials herdr's unix socket directly; a `command` machine
spawns its configured bridge program; an `ssh` machine runs `herdr --session
<s> remote-api-bridge` over ssh, which pipes herdr's socket protocol over
stdio. `<s>` is the herdr session the machine's agents run in: `pastor
machine add --session <s>`, or `session` in flock.toml, `default` when left
out. A pull machine's own headless serve reaches its herdr the same way,
as whichever of those three kinds it is in its own flock.toml; the head
never opens a connection to it at all (see [Pull
machines](#pull-machines)). Every command pastor runs over ssh (this one,
the probes, `task attach`) goes as `sh -c '<command>'`, so a remote login
shell that is not POSIX, such as fish, only parses one quoted word; sh
parses the rest. For the same reason a machine's `session` may not contain a
backslash, and a repo path with one needs a POSIX login shell.

For an `ssh` machine those connections are cheap because all of them share
one multiplexed ssh master (`ControlMaster=auto`,
`ControlPath=~/.local/state/pastor/ssh/<machine>-%C`, `ControlPersist=600`),
so only the first one authenticates. A long machine name is shortened inside
that socket name so it stays under the unix socket path limit; `%C` is what
tells machines apart. A master lingers for up to 10 minutes after `pastor
serve` exits; end one by hand with `ssh -O exit -o
ControlPath=~/.local/state/pastor/ssh/<machine>-%C <target>`. A `local` or
`command` machine pays no such per-connection cost: the socket dial or the
spawned bridge is already as direct as it gets. Alongside the per-request
connections, whatever their kind, each machine keeps one long-lived
connection open for `events.subscribe`, the one thing herdr holds open.

Each machine needs herdr protocol 22 or newer (herdr 0.9).

### Starting an agent

herdr's `agent.start` returns before the agent is up, so pastor waits for it
to come up before it sends the prompt (see [How a prompt reaches the
agent](#how-a-prompt-reaches-the-agent)). herdr answers `agent.start` with
`agent_pane_busy` while a new pane's shell is still starting, so pastor
retries the start up to 5 times, 500ms apart, before it fails the task.
`--agent-arg` reaches the agent through `agent.start`.

pastor sets an agent definition's env when it creates the task's pane:
`workspace.create` takes it directly. herdr's `worktree.create` and
`worktree.open` take none, so for a `--worktree` task pastor splits a pane
off the worktree's, with the env and the checkout as its directory, closes
the pane without it, and starts the agent in the new one. A task without env
keeps herdr's own pane either way.

### How a prompt reaches the agent

Everything pastor sends an agent as text goes by one of two routes, chosen per
agent by its `delivery` (default `socket` for kind `claude`, `type` for the
rest; see [The inbox](#the-inbox)).

- **The inbox socket.** For a Claude agent, the first prompt at dispatch, a
  prompt held while the agent was blocked, `task send` text with its Enter, the
  retry line after a short usage limit and an orchestrator session's last
  message go to the session's inbox socket as one message, not as keystrokes
  (the head's log names the path each took). Pastor sends only once `agent.list`
  shows the agent up and not blocked: a blocked or still starting agent may be
  waiting on a dialog, which only keystrokes answer. A pending prompt waits
  for that; a `task send` does not wait, it is typed at once.
- **Typing.** Any other text is typed into the agent's pane with `agent.prompt`
  (a prompt) or `pane.send_text` and Enter (a send). It is also the fallback
  for the socket: a session that does not take the message (`held`, `refused`,
  `dropped`, no socket), a machine that cannot send (a `command` machine, no
  pastor there with `inbox send`) and `delivery = "type"`. A `task send` is
  also typed when herdr lists the agent blocked or still starting. Keys of
  their own, text given with `--key`, a line without Enter (`--no-enter`),
  `--pick` and `--trust` are always keystrokes.

The text is the same either way, except that a message sent again, and each
rate retry past the first, ends with its attempt number (`(Sent again, attempt
2.)`, `(Attempt 2.)`) when it goes by socket: Claude drops the same text from
the same sender within 30 seconds. A send's `task.input` event has `socket:
true` when it went through the socket, with the `keys` (Enter) and `text_len`
it would have had typed; a prompt emits no `task.input`.

### When a task is done

A task is done when its agent has gone idle (herdr's `idle` or `done`) after
working on the prompt and stays idle for `settle`. pastor must have seen the
agent `working` or `blocked` since the prompt went in, from an event or from
`agent.list`; `unknown` does not count. herdr also counts each agent state
change, and the count must have moved past its value when the prompt went in
and not moved again during the window.

An agent that went idle without pastor ever seeing it `working` or `blocked`
(its whole working spell fell between two reconciles while the event was
lost) stays `running` until its task goes `stale`: herdr's counter moves on
`idle -> unknown -> idle` as well, so without the activity pastor cannot
tell finished work from a flicker.

### Worktrees

Before pastor makes a new worktree it fetches the remote the repo's current
branch tracks (`origin` when the branch tracks none), so the branch point is
not days old: `git fetch <remote>` on the machine, with git told never to ask
a person for a password. Only the remote-tracking refs change: nothing is
merged and nothing in the repo is checked out. `pastor task run --no-fetch`,
or `no_fetch = true` in a job's `[dispatch]`, skips it. A repo with no remote
has nothing to fetch. A task that does not make a worktree, one that reopens a
checkout (a retry, a paused or waiting task) and a machine reached through a
`command` transport fetch nothing.

A fetch that fails (no route, no credentials, the remote gone) does not fail
the task: the worktree is made from the repo as it is. `pastor task describe`
shows `fetch: warning: could not fetch origin: ...` for it, and `--no-fetch`
as `fetch: skipped (--no-fetch)`; `--json` carries `spec.fetch.warning` and
`spec.fetch.skip`. The head also emits `task.fetch_failed` with the repo and
the reason, once for the dispatch.

The retry of a failed worktree task reopens its checkout with herdr's
`worktree.open` instead of `worktree.create`, which git would refuse with
"already exists". When `worktree.open` finds a workspace already showing the
checkout, herdr answers that one, and the retry's pane is split off it. A
paused worktree task resumes the same way, with `worktree.open` on its
branch; pausing never deletes a worktree, since only `worktree.remove` does.

Removing a worktree (`--remove-worktree`, or auto-close of a clean one)
closes the task's pane, has herdr open a workspace on the checkout
(`worktree.open`) and removes that with `worktree.remove`; when herdr refuses
(uncommitted changes), the workspace it opened is closed again and the
checkout stays. When a workspace already shows the checkout, herdr answers
that one, and removing the checkout would close it, which is why pastor
keeps such a checkout.

### The socket and its requests

The CLI and the head speak newline-delimited JSON over `pastor.sock`; each
response is `{"kind": ..., "data": ...}`.

`job_submit` is how a job on another machine hands its items to the head. It
carries the job's name, its `[dispatch]` table, its prompt and the items
(each with its `key`). The head checks the table as it would a job file's
(`invalid_dispatch` with the same error), and queues each item with the same
rendering, path checks and `max_tasks_per_run` cap as its own jobs. It
answers the tasks queued, the keys skipped as seen, and each refused item
with its reason (`max_tasks_per_run` for those past the cap; they stay
unseen). A name the head has a job file for is `job_name_taken`; submitters
of one name share its seen keys.

`events_since` reads the log through the head by number:
`{"op":"events_since","after":N,"limit":L,"task":ID}` answers the records
whose `seq` is past `N`, oldest first, at most `L`, only those about task
`ID` if given, with `oldest`, the oldest number the two log files still
hold, `newest`, the newest number they hold (whatever `N`, `L` and `ID`, so a
limit of 0 asks only where the log ends), and `gap: true` when records after
`N` were already rotated out of both. It is a read, so an agent may send it.
`pastor events` and `pastor watch` with a remote head, and a headless serve's
hooks, read the head's events with it.

A pull machine's headless serve asks for work with `task_claim` and sends
every change back as `task_report`. It asks for the folder trust saved for
it with `{"op":"pull_trust","machine":M}`, which answers only `M`'s pairs
and refuses a machine that is not a pull machine as `task_claim` does, and
a `task_report` carries `trusted`, the keys its actor pressed at the task's
trust prompt, until one reaches the head. `edit` and `job enable|disable`
with a head running fetch the head's file with `FileGet` and send it back
with `FilePut`.

### Protocol numbers

A CLI refuses a head below the IPC protocol a request needs
(`head_too_old`). The numbers:

| request | head protocol |
|---|---|
| `events` from another machine, `events_since` | 4 |
| reading the head's `flock.toml` and `pastor.toml` (`flock list`, `flock default show`, `profile`, and `task attach` and `machine open` with a remote head) | 6 |
| `job_submit` | 7 |
| `task run --role orchestrator` | 12 |
| `task done --summary`, `task describe --all-summaries` | 17 |
| `task run --summary`, and a headless serve forwarding a job with `summary` | 20 |
| `task_claim` and `task_report` | 21 |
| `flock join`, `flock leave` and `flock add` with machines | 22 |
| `orchestrator` commands | 23 |
| `orchestrator start` and `stop` | 25 |
| a flock's share and max in flock.toml | 26 |
| `task run --fallback` or `--no-fallback`, and a job with `fallback` | 27 |
| `pull_trust`, and a `task_report` with `trusted` | 31 |
| `shepherd_check`, an `events_since` cut to one machine, and a `task_claim` with versions | 39 |
| `task run --load`, and `loads` in flock.toml | 46 |
| `workdir place`, `remove`, `check` and `list` through the head | 47 |
| `workdir_report`, a pull or command machine's report of its workdir sets | 49 |
| `task run --workdir-set`, and a job with `workdir_set` | 50 |
| a task or job with `workdir_bundles`, and `workdir place --bundle` | 51 |
| a `task_claim` from a pull machine whose pastor places workdir sets, and a `task_report` with the workdir bundles it left out | 52 |

`task run --description`, `flock add --description` and `machine add
--description` need a head of 0.7.0 or later.

A head started from a pastor before flocks would ignore `--flock` and read
`flock.toml` as one flock, so while flocks are in play every command that
talks to or reloads the head asks its protocol first and refuses an old one.
Flocks are in play when the command takes `--flock`, edits `flock.toml`
(`flock add|join|leave|remove|edit|default set`, `machine add|remove|move`), or
`flock.toml` declares named flocks, since then no `--flock` means the
default flock rather than every machine. A head that is listening but does
not answer is refused whether flocks are in play or not
(`head_unresponsive`); only a head that is not running at all is passed by.

### The store

`pastor.db` is at schema 19. Its tasks table has, among others, `retry_of`,
`flock`, `trust_sent`, `trust_keys` (the keys sent with it, since schema
17), `activity_seen`, `ended`, `priority`,
`priority_from`, `queue_pos`, `aged_from`, `aged_at` (since schema 14),
`role`, `description` (since schema 11), `preempt`, `paused_at`,
`paused_for`, `resumed_at` and `waiting_until` (schema 15). Summaries are in
the `task_summaries` table (schema 13), usage limits in the `limits` table
(schema 15), what Claude tasks used in the `task_usage` table (schema 16),
the startup dialogs answered for each task in the `answered_dialogs` table
(schema 18), how each machine's bootstrap ended in the `machine_state`
table and the one-use join tokens in the `join_tokens` table (schema 19).
A join token's row holds its id, flock, tags, the machine it may replace,
its expiry and when it was used, never the token itself.
A new pastor adds what it needs in place
when it first opens an older store.
