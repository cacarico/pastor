# Connector protocol

Copied from [docs/manual.md](../../docs/manual.md#connectors) so
`pastor:connector` stands on its own; the code and the manual are
authoritative if the two ever drift.

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

## The manifest

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

## Connector protocol

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

## Secrets and the `.env` file

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

## Connector commands

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

## Using a connector in a job

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

## What a connector inherits

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
  sockets, which is control of the fleet (see [Trust model](../../docs/manual.md#trust-model)).

A hook without `only_own = true` also hears about every other job's tasks,
though without their item, prompt or summary text (see [Event hooks](#event-hooks)). Start `pastor serve` from an
environment that holds only what its connectors and ssh need, and install only
connectors you would run by hand.

## Event hooks

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

## The finish command

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
  with `task done --summary`, or `no summary` and the pane's last lines;
  `null` when pastor has none.

It runs once per task, the first time pastor sees it `done` or `failed`. A task
that goes on to `closed`, or that is reopened and ends again, does not run it
again. The list of tasks already run is in memory, so it is lost when the head
restarts. A failure never changes the task: an exit status other than 0, a
timeout, or a command that could not start is logged and sent as a
`connector.finish_failed` event (`detail.connector` and `detail.reason`), and
not retried.

## The watch command

`[watch]` feeds [`pastor watch`](../../docs/manual.md#watch) what the head cannot know, like the
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

