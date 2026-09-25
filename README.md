# pastor

pastor runs coding agents on always-on machines you own, so they pick up
tasks while your laptop is closed. It sits on top of [herdr](https://herdr.dev):
herdr owns the terminals and the agents, pastor owns the fleet and the
bookkeeping. When you open the laptop you attach to the panes through herdr.

Status: version 0.2.0. `pastor serve`, the head, runs one-off tasks and
scheduled jobs end to end with the built-in `clock` connector. See
`CHANGELOG.md` for what changed in this release. Connector plugins and event
hooks are the next milestones; see the design spec on the `docs` branch,
`docs/superpowers/specs/2026-09-23-pastor-design.md`.

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
kept in memory, so after a daemon restart an agent found idle stays `running`
until its task goes `stale`. Task state lives in SQLite under
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
replaces a socket file whose connection is refused. `task list` and `task show`
read from `pastor serve` when it's running and fall back to the SQLite store
when it's not (`task list` says so on stderr); `task attach` always reads the store
directly, since it only needs the task's machine and agent name to hand off
to `ssh`/`herdr`.

`pastor machine list` shows the head first, then each machine in the flock:

```
NAME    HOST        CHANNEL    HERDR  PASTOR  AGENTS  ORPHANS  TAGS  ERROR
pastor  darkbeat    head       0.9.1  0.2.0   -       -        -
pi-3    fleet@pi-3  connected  0.9.1  0.2.0   1/2     -        fast
```

HOST is the ssh target, `local`, or the program a `command` machine runs.
The head row carries this machine's hostname, the version of the `herdr`
on its PATH (`-` when there is none) and its own pastor version; it is not a
machine and takes no tasks. PASTOR is the pastor installed on the machine:
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
on each machine (`null` when unknown) and `channel` one of the same four probe
values (or the head's live channel state when `pastor serve` is running).
`pastor machine status` is the old spelling, kept as a hidden alias.

Jobs are one TOML file each in `~/.config/pastor/jobs/`. On every `tick` the
daemon re-reads files that changed (a file that stops parsing keeps its last
good version and shows the error in `pastor job list`), asks each due job's
connector for items, drops keys it has seen before, renders `prompt`, `repo`
and `branch` with `{{ item.* }}`, `{{ job.name }}` and `{{ task.id }}`, and
queues one task per new item up to `max_tasks_per_run`. The rest stay unseen
for the next run. A job never overlaps itself; `pastor job run <name>` fires
one regardless, and it starts once a run already going has finished. `every = "5m"` or `cron = "*/5 9-18 * * 1-5"` (local time)
says when. The only connector today is `clock`, one item per run keyed by the
run time; jobs that name another connector are `invalid` until plugins ship.
A failed connector backs the job off, one minute doubling to an hour, and
keeps its cursor.

A machine whose requests answer but whose event subscription will not open is
`polling`: it still takes tasks and is reconciled every `tick`. Two dispatch
passes never run at once, and a task moves from `queued` to `starting` with a
conditional update, so a machine is never given more than `max_agents`.

`pastor task list` shows live tasks only: queued, starting, running and blocked.
Finished ones (done, failed, stale, closed) appear with `--all`, and an empty
default list says so on stderr. `--blocked` and `--done` narrow to just that
state, `--job` and `--machine` narrow whichever set is shown, and `--json`
prints the same selection. `pastor task read t-1` fetches recent output from the
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
(`pastor task attach`) or `pastor task close`. None of them is closed on its
own. The grace period keeps the pane there for `pastor task attach`; the
check runs with each reconcile, while the machine is connected. An agent
that herdr shows working or blocked again
at that moment is left alone, and its task goes back to running or blocked.

Everything else pastor closes only when asked; three commands do it, all
with `--json`. `pastor task retry t-4` queues a new task
copying a failed or stale one (job, item, prompt and dispatch settings, with
`retry_of` pointing back and a "retry of t-4" note in `task list`) and dispatches it
at once. It gets a new id because the old agent `t-4`, named after the task, may
still be running. A retried job task keeps the branch it was rendered with, so
if that branch has no `{{ task.id }}` in it and the old worktree is still
there, close the old task with `--remove-worktree` first. `pastor task close
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
the socket and does not answer (`daemon_unresponsive`): that head may still be
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

## Events

`pastor serve` appends every task, job and machine event to
`~/.local/state/pastor/events.jsonl`, one JSON record per line. When the next
line would take the file past 10 MiB it is moved to `events.jsonl.1`
(replacing the previous one) and a new file started, so the log keeps at most
two generations. `pastor events` prints both, oldest first; `--task t-3` keeps
one task's records, `--json` prints the records as stored, and `--follow`
keeps printing as new ones are written. It reads the file, not the daemon, so
it works with `pastor serve` down.

A record, which is also what plugin event hooks will get on stdin:

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
  `job.failed`, `machine.connected`, `machine.lost`.
- `task`: the full task row (the same object as `pastor task show --json`) at
  that moment, on `task.*` events; `null` otherwise or if the row is gone.
- `job`: the job name. For a task event it is the task's `job` (`run` for a
  one-off `pastor task run` task); for `job.failed`, the job that failed.
- `machine`: on `machine.*` events, the machine's status as an entry of
  `machines` in `pastor machine list --json` shows it (`name`, `host`,
  `endpoint`, `channel`, `herdr_version`, `pastor_version`, `protocol`,
  `error`, `live`, `max_agents`, `tags`); `null` on other events.
  A task's machine is `task.machine`.

Fields may be added; none will be renamed or removed. Unreadable lines (a
torn write, a hand edit) are skipped.

Runtime errors print JSON on stderr with a stable `code` and exit 1; a
malformed command line gets clap's plain usage text and exit 2.

Each machine needs herdr 0.9 or newer (protocol 22 or newer) with its server
running, and SSH access from the head without a passphrase prompt (a key in
ssh-agent won't be there for a service; use a dedicated key or Tailscale SSH).

## Try it

```bash
make install                         # pastor and fake-herdr into ~/.cargo/bin
pastor machine add pi-3 fleet@pi-3 --max-agents 2 --herdr   # --herdr also saves it in herdr's sidebar
pastor machine add here --local
pastor machine list                  # the head, then each machine: host, channel, herdr, pastor, agents
pastor setup systemd                 # confirm, then install and enable --now; or `pastor serve &`
pastor task run "Fix the flaky test in ci.yml" --repo '~/work/api' --machine pi-3
pastor task run "Review the open PR" --agent-arg=--model --agent-arg=claude-opus-5-5
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

`pastor task run` takes `--repo`, `--machine`, `--agent`, `--agent-arg`,
`--worktree`, `--branch` (with `--worktree`), `--tag` (repeatable),
`--timeout` and `--json`. `--agent-arg` hands one argument to the agent,
through herdr's `agent.start`; repeat it for more, in order. It always takes the
next word as its value, even one that starts with a dash, so
`--agent-arg --model --agent-arg claude-opus-5-5` and
`--agent-arg=--model --agent-arg=claude-opus-5-5` mean the same thing; the
`=` form just reads more clearly. There is no single-string form: pastor would
have to split it on spaces, and that breaks any argument that contains one. A
job file's `agent_args` does the same for its tasks. When neither says
anything, `[defaults] agent_args` in pastor.toml applies; a job file that sets
`agent_args = []` opts out of it. `pastor task show t-1` prints the args a task
was started with.

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
hooks, or both. The manifest format and the connector protocol are in the
spec's Plugins section; `tests/fixtures/plugin/` has small working
examples (`echo` and `stream` connectors, a `notify` hook).

```bash
pastor plugin install cacarico/pastor/plugins/slack   # owner/repo[/subdir], --ref, --yes
pastor plugin link ~/src/my-plugin                    # use a working copy in place
pastor plugin list                                    # version, connector, hooks, missing secrets
pastor plugin run slack --job support --since 1h      # run the connector once, print its items
pastor plugin uninstall slack                         # or unlink, for a linked one
```

Secrets and settings go in `~/.config/pastor/plugins/<id>/.env`
(`KEY=value` lines). Every plugin command runs in the plugin's directory with
that file in its environment plus `PASTOR_PLUGIN_ID`, `PASTOR_JOB`,
`PASTOR_CONFIG_DIR`, `PASTOR_STATE_DIR` and `PASTOR_PLUGIN_STATE_DIR` (the
job's scratch directory). What a run writes to stderr lands in
`~/.local/state/pastor/runs/<job>/<ts>.log`, with the values of the secrets
the manifest declares replaced by `[redacted:NAME]`. Redaction works line by
line, so a declared secret may not contain a line break (a double-quoted
`\n`): pastor refuses such a `.env` and names the variable. Each log is cut at
256 KiB and the newest 20 per job are kept. A stdout or stderr line longer
than 256 KiB is cut there and the rest of it dropped.

A job uses a plugin's connector by its id (`[connector] use = "slack"`);
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

`plugin install|link|uninstall|unlink` tell a running daemon to reload, so a
new plugin is usable without a restart (this also restarts stream
connectors). A daemon that is running but does not answer gets a warning to
run `pastor job reload` yourself.

### Event hooks

A plugin's `[[events]]` hooks run on the head for every event whose type is
in `on` (`task.queued`, `task.done`, `task.blocked`, `task.failed`,
`job.failed`, `machine.lost`, ... as in `pastor events`). The hook gets the
event record, the same JSON `pastor events --json` prints, on stdin, and the
same environment as the connector (`PASTOR_JOB` is the task's job). With
`only_own = true` it only hears about tasks and jobs whose connector is this
plugin; one-off `pastor task run` tasks belong to no plugin.

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

## Files

```
~/.config/pastor/pastor.toml      tick, settle, reconcile_every, request_timeout, agent_ready_timeout, close_done_after, defaults (all optional)
~/.config/pastor/flock.toml       machines
~/.config/pastor/jobs/<name>.toml one job per file
~/.local/state/pastor/pastor.db   tasks (schema 3, with retry_of), seen keys, job state
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
[defaults]                   # for run flags and job keys that are left out
agent = "claude"
agent_args = []              # e.g. ["--model", "claude-opus-5-5"]
max_tasks_per_run = 5
timeout = "2h"
```

## Shell completions

`pastor completions <shell>` prints a completion script generated from the
command definitions, so it always matches the installed binary. Ready-made
copies for bash and fish live in `contrib/completions/`.

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
