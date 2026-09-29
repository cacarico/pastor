# Changelog

All notable changes to this project are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## 0.8.0 - 2026-09-29

### Added

- `pastor flock join <flock> <machine> [--max N]` puts a machine in a flock
  with that number (by default the one it has there, else its
  `max_agents`), keeping its other flocks; joining again with `--max`
  changes the number. `pastor flock leave <flock> <machine>` takes it out;
  out of its last flock the machine is back in the default one, and the
  output names the queued tasks of that flock pinned to it, which wait with
  a note. `pastor flock add <name> [machines...]` joins the machines named.
  The first `join`, `leave` or `machine move` on a machine with the old
  `flock = "..."` key moves it into the flock's `machines` with the
  machine's `max_agents`, keeping comments. They go through the head like
  the other fleet edits (IPC protocol 22; the CLI refuses an older head
  with `head_too_old`), an agent pastor started may not run them, and tab
  completion offers flocks and machines. A machine that is not in the flock
  is `not_in_flock`.
- Every command works from a machine with a remote head set.
  - The `flock` commands, `machine add|remove|move|describe`, `trust`,
    `profile` and `config edit` go to the head and print what they would
    print there; `flock edit` and `config edit` edit the head's file, and
    `config edit --local` this machine's `pastor.toml`.
  - `task attach` and `machine open` still go to the machine directly, but
    ask the head for the task, its `flock.toml` and `pastor.toml`; the
    head's own machine is reached at the head's ssh destination.
  - `machine authorized-key` stays on the head and is refused here, naming
    it.
  - `flock list`, `flock default show`, `profile`, `task attach` and
    `machine open` need a remote head speaking IPC protocol 6.
- The manual has a "Moving the head" runbook.
- Orchestrators run from files: `~/.config/pastor/orchestrators/<name>.toml`,
  with a required `kind`.
  - A `scheduled` orchestrator runs its `pre` script on `every` or `cron`.
    Only when the script prints lines that need judgment does the head start
    one agent, with the `orchestrator` role, on its own machine, with the
    file's `prompt`, `skill` and `model`, the handover note and every line.
    Its `post` script gets the agent's end state (`done`, `failed` or
    `stale`), summary and lines.
  - A run is skipped while the last agent works or its post script waits. A
    pre script that fails or prints more than 64 KiB of lines backs off as a
    failing job does. `max_orchestrators` in `pastor.toml` (default 1; each
    also takes a slot under `max_agents`) holds agents back, and an agent
    that stopped on a quota error holds the next until the reset.
  - `session` files are checked (a key of the other kind makes a file
    invalid) but not run yet.
  - New commands `pastor orchestrator list | describe | run | enable |
    disable | note`, and events
    `orchestrator.started|skipped|held|quota|failed`.
  - The pre and post scripts run with `PASTOR_ORCHESTRATOR`, which the CLI
    sends with each request, and the head applies the orchestrator role's
    table to them; `PASTOR_TASK` wins when both are set. The agent may keep
    its note (`pastor orchestrator note`).
  - `pastor task list` shows orchestrator tasks first, in a table of their
    own.
  - The head's IPC protocol goes to 23; the store keeps its schema
    (orchestrator state lives under `state/orchestrators/<name>/`).
- A `critical` task started with `--preempt` (or `preempt = true` under a
  job's `[dispatch]`) that finds its machines full, job slots and burst
  included, pauses the newest running `low` Claude task on one and starts
  in its slot in the same pass. The paused task's agent is interrupted and
  its pane closed; its worktree stays. It goes to a new `paused` state
  (event `task.paused`), first among `low` tasks and pinned to its machine,
  and resumes its own session there (`claude --resume`) when a slot frees.
  A normal, opencode, done or recently resumed task is never paused.
  `pastor task priority t-N critical --preempt` sets the flag on a queued
  task; `--preempt` below critical is `preempt_needs_critical`. On a paused
  task `task close` closes the row (`--remove-worktree` removes the kept
  checkout), `task send` answers `task_not_live` and `task attach` refuses
  with `task_paused`. `pastor queue` shows paused tasks and why they wait,
  and `task describe` when and for which task one was paused. The store
  goes to schema 12 (`preempt`, `paused_at`, `paused_for`, `resumed_at`) and
  the head's IPC protocol to 16; the CLI refuses `--preempt` to an older
  head (`head_too_old`).
- Task summaries: `pastor task done --summary <text>` (or `--summary-file
  <path|->`, through the bridge too) says how a task ended, its first line
  the outcome: `done`, `partial`, `blocked` or `nothing to do`, up to 2,000
  characters. Each round of a task keeps one, in a new `task_summaries`
  table (schema 13, created on first open); a round that ends without one
  keeps `no summary` and the pane's last lines. `task describe` shows it
  (`--all-summaries` every round's), `task list --wide` adds RESULT,
  `--json` has `summary`, the `task.done` and `task.failed` events carry
  it, `pastor watch` prints `outcome=` on their TASK lines, and a
  connector's `[finish]` stdin has `summary`. Needs a head speaking IPC
  protocol 17.
- pastor asks every task for a summary: each prompt it sends ends with a
  paragraph asking for `pastor task done --summary-file -` in that shape,
  added when it is sent and not stored in the task's prompt, and again on a
  `task send` that reopens a done task. `summary = "ask" | "require" |
  "off"` in `[defaults]`, on a flock, in a job's `[dispatch]` or as `task
  run --summary` (the most specific wins, stored on the task) turns it off,
  or makes a summary a condition of success: with `require`, the agent's own
  bare `task done` is refused (`summary_required`) and an idle finish
  without one ends the task `failed` ("stopped without a summary"); a
  person's `task done t-N` passes. `task describe` shows the setting.
  Config files without `summary` load unchanged and ask. `--summary` and a
  job's `summary` need a head speaking IPC protocol 20.
- A machine can be in many flocks. A `[[flock]]` entry takes `machines = {
  desk = 2 }`: the machines it may use and at most how many of its live tasks
  each one runs. Dispatch starts a task on a machine only when the machine has
  room and the task's flock is under its number there; job slots and burst
  never pass it. A task whose flock is full waits with `flock <name> is at N
  of N on <machine>` as its note, and the next task goes. `machine list`
  shows every flock (`--json`: `flocks`), and an agent's `task list` through
  the bridge covers all its machine's flocks. The machine's old `flock` key
  still loads, as membership with the machine's own limits. A critical task
  with `--preempt` pauses a task only where that makes room for it, its
  flock's number included. Needs a head speaking IPC protocol 18 once
  flock.toml gives a flock's machines a number.
- A `label` template for the herdr workspace pastor makes for a task (see
  Changed): `--label` on `task run`, `label` in a job's `[dispatch]`, on a
  `[[flock]]` and under `[defaults]`, the first of those that sets one
  winning. It takes `{{ task.id }}`, `{{ flock }}`,
  `{{ machine }}`, `{{ job }}` and `{{ item.key }}`. A label that renders
  empty or with a control character falls back to `t-N` with a warning.
  `task describe` shows the label and where it came from. `task run --label`
  needs a head of IPC protocol 19.
- `pastor serve` starts the head in the background and returns once it
  answers, logging to `~/.local/state/pastor/serve.log` (rotated at 10 MiB,
  three old files kept). `pastor serve --foreground` (`-f`) keeps it in the
  terminal, as `pastor serve` did before. `pastor serve status` (with
  `--json`) says whether a head or headless serve runs here, its pid,
  version, service manager and log; `pastor serve stop` sends it SIGTERM and
  waits for it to exit, and refuses one a service runs (`service_managed`).
  A `pastor serve` started by systemd, launchd or another pid 1 stays in the
  foreground, so units installed by an older pastor keep working. The
  shipped `pastor.service` and `pastor.serve.plist` now run `pastor serve
  --foreground`. **Upgrading:** on each machine with a unit, re-run `pastor
  setup systemd` (or `pastor setup launchd` on macOS) and restart the
  service, so its unit says `--foreground` and `pastor serve status` can
  report the service.
- Pull machines: a machine the head cannot reach over ssh runs tasks all
  the same. The head's flock.toml marks it `pull = true` in place of
  `local`, `ssh` or `command`; the head never connects to it and runs no
  actor for it, and `machine list` shows its HOST as `pull`. Its headless
  serve claims tasks each tick (`TaskClaim`, IPC protocol 21), runs them on
  its own herdr with the head's machine actor, and reports every change
  back (`TaskReport`). It takes the tasks pinned to it (`task run --machine
  <name>` or a job's `machine`), and also its flocks' other tasks when its
  pastor.toml has `[shepherd] takes_flock_work = true` or the serve runs
  with `PASTOR_SHEPHERD_FLOCK_WORK=1` (or `true`). `[shepherd] machine` is
  its name in the head's flock.toml (default: the hostname), and
  `[shepherd] command` is a developer option in place of its herdr. One
  that neither claims nor reports for `pull_lost_after` (head's
  pastor.toml, default `10m`) is lost, and its starting and running tasks
  go stale.
- `session` orchestrators run. One keeps one agent running through its
  `hours` (`{ start, stop }`, local time): the head starts it at
  `hours.start`, or at once inside the hours, with the prompt, the skill,
  the note and `pastor watch --now`; at `hours.stop` its agent gets a last
  message and is closed after `stop_grace`. An agent that dies, goes stale
  or ends early is restarted with the note, at most three times an hour; one
  that stopped on a quota error restarts at the reset. A session holds its
  `max_orchestrators` slot from start to stop, so a scheduled run meanwhile
  is held, and a session due while a scheduled agent works starts once it
  ends. New commands: `pastor orchestrator start | stop`; events
  `orchestrator.restarted|stopping|stopped`. The head's IPC protocol goes to
  25; `start` and `stop` refuse an older head (`head_too_old`).
- A flock's number on a machine can be a share and a max:
  `machines = { desk = { share = 2, max = 4 } }`. Under its share the flock
  takes a free slot as usual; between share and max it takes one only while
  no task of a flock under its share on that machine is waiting, so slots a
  quiet project leaves idle get used and it gets its share back as soon as it
  has work. The machine's own room still caps everything, and the plain
  `desk = 2` stays a hard ceiling. `flock list` shows `desk 1/2/4`,
  `machine list` `work:2/4`, and `--json` carries `share` and `max`. A head
  needs IPC protocol 26 to read the form, and the CLI refuses an older one
  while flock.toml uses it.
- CI: `make test-ssh` runs the CLI against a head over a real ssh, through a
  throwaway sshd on localhost, with a client state dir too deep for the
  full control socket name, so a bad ssh argv fails a pull request instead
  of a user's `head set`.
- `pastor task close` takes several tasks: `pastor task close t-1 t-2 t-3`
  closes each in turn, `--remove-worktree` applying to all, and prints one
  line per task (with `--json` an array of objects). A task that fails does
  not stop the rest; the command then exits 1 with `close_failed`, naming
  the ones not closed. One task prints as before, and tab completion offers
  more tasks after the first.
- `tests/panes.rs` runs the pane heuristics (the blocked question, the
  background shell footer, Claude's trust dialog) on a corpus of scrubbed
  screens under `tests/fixtures/panes/`, one test per screen and heuristic.
  The screens the parsers read wrong today are in it with their right answer
  and an ignored test that says why.
- `make smoke-rc TAG=vX.Y.Z-rc.N` (`scripts/smoke-rc.sh`): builds a release
  candidate in a scratch worktree, runs `make smoke` and optionally `make
  smoke-profiles` against it, and prints a Markdown report whose exit status
  is the result.
- The website serves `llms.txt` at its root: the docs pages in reading
  order, for agents.
- `make links` checks every internal link and anchor in the built website
  and in `README.md`, `docs/*.md` and the skills with `lychee --offline`;
  the website workflow runs it on pull requests that touch them.
- `make mutants` runs cargo-mutants over the transport, head, dispatch and
  config code (`.cargo/mutants.toml` picks the files), and `make
  mutants-diff` runs only the mutants in lines the branch changed against
  `origin/main`. Survivors are listed in `mutants.out/missed.txt`.

### Changed

- A flock carries every per-task setting `[defaults]` has: `[[flock]]` takes
  `timeout` and `place` too, before `[defaults]`, and `task describe` says
  when one came from the flock (`timeout: 1800s (from flock work)`); `flock
  describe` lists them. The order changed for `model`, `profile` and
  `priority`: the flock now comes before the machine (task > job > flock >
  machine > `[defaults]`), so a project's flock sets its model, permissions
  and level on a shared machine. The agent and its args stay machine first.
  A flock that names an agent sets its kind: a machine whose agent is of
  another kind runs its `agents` entry for that kind, and a machine with
  none is skipped for the flock's tasks, with a note while they wait, or
  refuses a task pinned to it (`agent_kind_missing`). A machine's own
  profile still decides whether `unrestricted` may run there; a flock's
  never lifts it. A head needs IPC protocol 24 to read a flock's `timeout`
  and `place`, and the CLI refuses an older one while flock.toml sets
  them.
- The agent skill (`pastor --skill`) has a checklist for bringing a new
  machine into a flock: herdr, pastor and the agent on the PATH of a
  non-interactive ssh command, every repo cloned at the same path, `pastor
  trust add` for each repo up front, the agent's first-run setup finished
  once, and a pinned test task. "When something goes wrong" gains the
  matching symptoms.
- `pastor machine move` takes the machine out of every flock and lists it
  in the one named, with its `max_agents`, instead of setting its `flock`
  key and leaving other flocks' `machines` alone.
- `pastor machine list` heads its flocks column FLOCKS. `pastor flock list`
  shows each machine with the flock's number and live tasks there
  (`desk 1/2`), and `--json` adds `members` (`name`, `max`, `live`) beside
  `machines`.
- An orchestrator task may also close tasks (`pastor task close`) and enable
  a job (`pastor job enable`), so it can clean up tasks it sent wrong and turn
  back on a job it disabled without waiting for a person. A plain agent is
  still refused both.
- The herdr workspace pastor makes for a task is labelled
  `<flock>/t-N`, such as `personal/t-285`, instead of `t-N`, so tasks of
  several flocks on one machine can be told apart in herdr's sidebar. The
  agent is still named `t-N`, and a task that joins a workspace leaves its
  label alone.
- `close_done_after` defaults to `5s` instead of `15m`, so a done task frees
  its machine slot almost at once. Set `close_done_after = "15m"` to keep
  time to attach and read its last screen. A grace shorter than
  `reconcile_every` is checked on its own tick, so a done task closes about
  5s after it finishes, not at the next reconcile.
- Every command now answers `daemon_not_running` when nothing listens on the
  head's socket (refused or missing), as `task retry`, `task close` and
  `queue` already did; the others said `runtime_error`. A head that is only
  busy is still `timeout`, and a connect denied for permissions is still
  `runtime_error`.
- A key pastor does not know in `pastor.toml` (top level, `[defaults]`,
  `[agents.<name>]`), in `flock.toml` (top level, `[[machine]]`) or under
  `[head]` in `client.toml` is now a load error naming the file and the key,
  as it already was in job files and `[[flock]]`. A typo used to load and
  leave the default it meant to change: `close_done_afer = "never"` still
  closed done panes, `max_agent = 1` left 2, and `[[machines]]` loaded as an
  empty fleet. On upgrade, a head whose files carry such a key stops at
  start, or keeps its previous settings on reload, with the file and key in
  the error; fix or remove the key. A machine's legacy `flock = "..."` still
  loads.
- The website's homepage says what pastor is in one line, plays a scripted
  session in a live terminal (by hand, then an agent driving pastor), and
  links to what you can do with it. A unit test checks the terminal's
  commands against the CLI.
- The website's docs are five sections: start, concepts, deployment modes
  (solo, fleet, remote, as a service), examples (real workflows to copy)
  and reference. Every page was rewritten from the code, and the manual is
  no longer part of the site. Old page addresses redirect to the new ones.
- Every request to a head is checked against its IPC protocol as it is
  sent, not per CLI command: a request carrying something the head predates
  is refused `head_too_old` before it goes out, with one ping per head. A
  headless serve's claims, reports, event reads and job submits now get the
  same check, so a pull machine or shepherd refuses a head too old for them
  instead of sending a request it would drop or refuse.
- The website's CLI reference lists every command, argument and flag with
  its `--help` text. `make cli-reference` generates it from the binary, and
  `make check` fails when it is stale.

### Fixed

- A Claude agent that ended its turn waiting on a background shell (its
  footer says `1 shell still running`) is no longer read as `done`: herdr
  shows it idle, so pastor closed its pane after `settle` and the work it
  would have finished when the shell ended was lost. Its task now stays
  `running` until the shell ends and the agent goes idle again.
- A task with no repo (no `--repo`, no `repo` in its job) opened its pane
  wherever herdr's focused pane was, so it could start in an unrelated
  checkout. It now starts in `~/pastor-tasks` on its machine, which pastor
  makes when it is missing; a machine that cannot report its home (a
  `command` one) still leaves it to herdr. Not the home itself: Claude Code
  never saves folder trust for the home directory, so a task there asked
  "Is this a project you trust?" at every start and sat `blocked`. In
  `~/pastor-tasks` Claude asks once per machine. If the folder cannot be
  made, the task starts in the home.
- A task blocked on Claude's folder-trust prompt that a person answered at
  the pane got its prompt at once, while Claude redraws after the dialog
  and drops what is typed, so it read `running` with the agent at an empty
  input. The prompt now waits `settle` after the block clears, however it
  was answered. A prompt the agent still does not take (idle a settle
  window at the sequence it went in at, never seen working) is sent again,
  up to twice, then the task is `blocked` with an error saying so.
- A head of another version that answers a request with a reply the CLI
  does not expect no longer makes it panic with exit 101: the command fails
  with the usual JSON error on stderr, code `internal`, and exit 1.
- A panic in one request no longer breaks every later one until restart:
  the head's store connection and its shared state recover a lock that the
  panicking request left poisoned instead of panicking again.
- `pastor watch` and a shepherd's hooks no longer go silent when their
  cursor is past the head's newest event, after the head moved or its state
  dir was wiped: the watcher prints one `HEAD reset` line and the shepherd
  logs `head_events_reset`, and both go on from the head's end without
  replaying the events already in its log.
- Running pastor's test suite in an agent's pane on a fleet machine no
  longer sends the tests' commands to the real head: the tests ignore an
  inherited `PASTOR_HEAD` and run against their own temporary state.
- An orchestrator whose `state.json` cannot be read or parsed no longer
  starts afresh: its runs are held, `orchestrator.failed` (stage `state`)
  says why once, `orchestrator list` shows it `held` with the error, and no
  second agent starts beside a live one or drops a pending post script. A
  missing file is still a fresh start; removing or fixing the file lets it
  run again.
- A dispatch pass decides under the dispatch lock and sends outside it, so
  one machine slow to start an agent, or wedged, no longer holds up
  `task run`, pull claims, a reload or dispatch to every other machine. A
  task placed and not yet claimed counts on its machine, so two passes
  still dispatch a task once and never past a machine's room.
- A reload stops the actors it removes or replaces together, waiting about
  2s for all of them instead of 2s each.
- A request to a machine's actor (dispatch, pause, resume, read, send,
  done, close) fails after three times `request_timeout` instead of
  waiting on a stuck actor forever.
- `pastor flock remove` with no head running can no longer strand a task
  queued by a head that starts during the edit. The offline `flock
  add|join|leave|remove` and `machine remove` hold a lock (`fleet.lock` in
  the state dir) from their store check to the save, and `pastor serve`
  holds it from before it reads flock.toml until it listens; an edit that
  finds a head listening once it has the lock stops with `head_started`.
  Either side gives up after 30 seconds with `fleet_locked`, a bare
  `pastor serve` included.
- `task priority` and `queue move` refuse a task a dispatch pass is sending
  to a machine with `not_queued`, as for a task a machine has taken. Before,
  they reported success while the pass sent the old decision, so clearing
  `--preempt` could still pause a victim.
- The orchestrator's close of an agent's pane is bounded like its other
  requests to a machine, so a stuck actor no longer holds a stop or restart
  pass forever.
- A task a dispatch pass placed keeps its `not_queued` refusal for as long
  as the actor may still claim it, not just until the head gives up
  waiting for an answer: a machine slow past `reply_wait` no longer lets
  `task priority` or `queue move` land on it while the placement is still
  in flight.
- `queue move --before` or `--after` naming a task a dispatch pass has
  placed is refused as `not_queued` too, instead of moving by a neighbour
  about to leave the queue; `--to` no longer counts such a task toward its
  position either.
- A done task given more work with `pastor task send` after its timeout no
  longer goes `stale` on the next reconcile: the timeout counts from the
  task's latest start, so a reopen restarts it as a resume already did.
  `started` in `task list` and `task describe` now shows that latest start.
- A `blocked` task no longer goes `stale` past its timeout; it waits for a
  person, however long that takes.
- A task unblocked past its old deadline no longer goes `stale` the moment
  it starts working again: the timeout restarts from when the block clears,
  not from the dispatch or resume before it.
- The test that starts a head while an offline `flock remove` waits for the
  fleet lock no longer fails under load with `head_unresponsive`: it waits
  for the pastor process itself to hold the lock, not for the test's own
  lock descriptor in the child before it has exec'd.
- `pastor task run --help` names the flock before the machine for
  `--model`, `--profile` and `--priority`, the order pastor reads them in.
- The manual, the website pages and the skill no longer contradict the
  code on the points the docs review of 0.8.0 found wrong or stale, among
  them: orchestrator agents take a `max_agents` slot, the remote-head ssh
  line and the commands sent to a head, what a headless serve runs, the
  protocol and schema numbers, and the permission mode under a profile.
- `pastor task attach` on a closed Claude task with no repo reopens its
  session in `~/pastor-tasks`, where it ran. It passed no directory, so
  herdr opened the pane wherever its focused pane was and `claude --resume`
  could not find the session.
- The second docs review's findings: a remote `task run` takes only
  `timeout` and `place` from the built-in defaults, `queue move` ignores
  paused tasks, a paused task resumes only while its flock has room on its
  machine, the full list of commands refused from a task's pane, a failed
  task holds no slot, hooks without `only_own` get other connectors' text
  blanked, and the spec skill names models with `--model` so a plan task
  keeps its flock's agent and account.
- `make demo` stops its demo head again: it runs `pastor serve
  --foreground`, since a plain `pastor serve` forks and returns.
- A Copilot pass over this docs review: the Tier 2 platform table no longer
  claims every Tier 2 target goes untested, when Linux aarch64 and macOS
  arm64 are smoke-run on release; the connector security model no longer
  calls every connector's files and sockets the head user's, when a
  headless serve's own connectors are the shepherd machine's; the manual's
  refreshed `machine list` transcript no longer prints a stale pastor
  version; the recommended setup no longer claims a permission profile
  stops a task from ever blocking on a question, when it only silences
  tool prompts; and the skill's remote-head section now names `task
  attach`, `machine open` and `serve status`/`serve stop` among the
  commands that stay local.
- A GPT (Codex) pass over the docs review: `SECURITY.md` now says a
  headless serve's shepherd does hold one ssh ControlMaster socket, to its
  own remote head, and that a connector on it can reuse that socket, not
  that it holds none. `pastor task attach` on a repo-less task now resumes
  in the directory dispatch actually started it in (`~/pastor-tasks`, or
  the home if that folder could not be made), recorded on the task instead
  of asked for again, which could answer differently once the folder is
  fixed and leave `claude --resume` looking in the wrong place.
- A second GPT (Codex) pass: resuming a paused repo-less task now goes back
  to the directory recorded at its first dispatch instead of asking
  `no_repo_dir` again, which could answer differently once `~/pastor-tasks`
  became makeable and leave the resumed agent unable to find its session.
  The manual's `task attach` section describes that recorded directory and
  its home fallback too.

### Security

- A profiled opencode task runs with the repo's own opencode config off
  (`OPENCODE_DISABLE_PROJECT_CONFIG=1`), so an `opencode.json` in a branch
  under review can no longer lift the profile's deny rules. The checkout's
  `AGENTS.md` and `CLAUDE.md` still reach the agent, by path in
  `OPENCODE_CONFIG_CONTENT`.
- The opencode permission check before a profiled task also reads
  `~/.opencode/`, the managed config directory (`/etc/opencode`, on macOS
  `/Library/Application Support/opencode`) and a legacy `"tools"` block.

## 0.7.1 - 2026-09-28

### Fixed

- `pastor head set` and every command sent to a remote head failed with
  `head_unreachable` ("path ... too long for Unix domain socket") when the
  state dir was deep enough, as under a long macOS home: ssh's ControlPath
  plus its 17-byte staging suffix went past `sun_path` (104 bytes on macOS).
  The head's ControlPath is now shortened as the machine transport's is, and
  ssh runs without multiplexing when even the shortest one does not fit.
  In that case both the head client and the machine transport now pass
  `ControlMaster=no` and `ControlPath=none`, so a `ControlMaster` in
  `~/.ssh/config` cannot bring the socket back.

## 0.7.0 - 2026-09-28

### Added

- A website for pastor, a small terminal-style site built with Hugo from
  `docs/manual.md` and short pages under `docs/website`. It deploys to
  GitHub Pages from the public repo; `make site` builds it and `make
  site-serve` serves it locally.
- `pastor watch`: one line per change an orchestrator acts on, so it stops
  building its own polling. `TASK t-N <state> <machine> <job>` for a task that
  is blocked, done, failed or stale (`--all`: every state change), from the
  head's numbered events; `JOB <name> failing: ...` and `JOB <name> ok` from
  `job list`; `HEAD down`, `HEAD up` and `HEAD gap`; and the lines of each
  connector with a `[watch]` command, each printed once, with `CONNECTOR <id>
  failing` and `ok` around failed runs. A watcher keeps a cursor under
  `--name` in the state dir, so one started again repeats nothing; `--reset`
  starts at the end of the log. `--now` prints what needs attention and
  exits. `--json`, `--interval` and `--connector` (else `[[watch.connector]]`
  in pastor.toml). It only reads, so an agent may run it. A connector's
  manifest takes `[watch]` (`command`, `timeout`), which `connector describe`
  shows and `connector try <id> watch` runs. The head's `events_since`
  answers `newest` too, so a new watcher starts at the end without reading
  the whole log.
- Profiles reach opencode tasks: an opencode agent under a profile gets the
  lists as `OPENCODE_PERMISSION` in its pane, in opencode's terms and denying
  what they do not allow (under `unrestricted`, allowing it), with `OPENCODE_CONFIG`, `OPENCODE_CONFIG_DIR` and
  `OPENCODE_CONFIG_CONTENT` set empty, so it never stops at a permission
  prompt (a profiled opencode task was `agent_tools_unsupported` before).
  A machine whose own opencode config has permission rules fails such a task
  before anything is made there (`opencode_permissions_conflict`).
  `make smoke-profiles` runs a live review task per agent through a head.
- Profiles reach Claude tasks: `profile` on a `[[flock]]`, a `[[machine]]`,
  a job's `[dispatch]`, `[defaults]` and `pastor task run --profile`, settled
  like the model (run or job, machine, flock, `[defaults]`). The profile's
  lists go before the task's own, a deny anywhere wins, and a Claude agent
  starts with `--permission-mode dontAsk` and the lists as `--allowedTools`
  and `--disallowedTools`, so it never stops at a permission prompt; args
  that pick a permission mode are then `profile_args_conflict`. A task may
  ask for `unrestricted` only on a machine whose own profile (machine, flock
  or `[defaults]`) is `unrestricted` (`profile_not_allowed`; an unpinned task
  waits for one). An unknown name is `unknown_profile`, and a profile dropped
  while a task waits keeps it queued and says why. `task describe` (with
  where it came from), the task's JSON, `machine list` (PROFILE),
  `machine describe` and `flock describe` show it. Every command that can
  make the head queue a task refuses an older head (`head_too_old`).
- Descriptions: an optional one-line `description` at the top of a job
  file and in each `[[flock]]` and `[[machine]]` entry of flock.toml
  (`pastor flock add --description`, `pastor machine add --description`).
  A task's is `pastor task run --description`, else its job's `[dispatch]
  description` template (`{{ item.title }}` by default), else the prompt's
  first line; `task retry` copies it. `-w, --wide` on `task`, `job`,
  `machine`, `flock` and `connector list` adds a DESCRIPTION column, cut to
  the terminal's width; every `describe` shows it, and every `list --json`
  and `describe --json` has a `description` key. fish completion shows
  descriptions beside job, flock and machine names. The tasks table gains a
  `description` column (schema 11), so the release that carries this needs
  an -rc first. `--description` is refused on an older head
  (`head_too_old`). The IPC protocol goes to 14.

- Permission profiles: `[profiles.<name>]` in pastor.toml, each an optional
  `description`, `extends`, `allow` and `deny`, beside the built-in `review`,
  `develop` and `unrestricted`. A profile's lists add up along its `extends`
  chain, and a deny anywhere wins. `pastor profile list` and `pastor profile
  describe <name>` (both with `--json`) show them; an unknown name is
  `unknown_profile`, and a bad profile fails pastor.toml's load.
- `pastor queue` lists the queued tasks in the order they will start: POS,
  TASK, LEVEL, WHERE (the pinned machine or the flock), FROM (`task run` or
  `job <name>`), WAITED and WHY NOT YET, from a dispatch pass played
  through on the machines as they are; `--flock`, `--machine` and `--json`.
  `pastor queue move <task>` with `--top`, `--before <task>`, `--after
  <task>` or `--to <n>` puts a queued task there; it takes the level of
  where it lands (lifted in front of a higher task, lowered behind a lower
  one, `--top` only lifts), and the answer says when the level changed.
  Refused with `not_queued` for a task, or an anchor, that is not queued,
  and from an agent pastor started. The IPC protocol goes to 14 (`Queue`,
  `QueueMove`); both commands refuse an older head (`head_too_old`).
  Completion offers the queued tasks after `queue move`.
- Task priority: a task has a level, `low`, `normal` (the default), `high`
  or `critical`, and each dispatch pass takes queued tasks by level, then
  position, then age, still skipping what does not fit. The level comes from
  `pastor task run --priority`, a job's `[dispatch] priority` (a template;
  rendered empty it falls through), the `priority` of the machine the task
  is pinned to, of its flock, or `[defaults] priority`, settled when the task
  is queued. `pastor task priority <task> <level>` changes a queued task's
  level, refused with `not_queued` for any other and from an agent pastor
  started; `task retry` keeps it. A word that is not a level is
  `unknown_priority`. `task describe` shows the level and the layer that set
  it, `task list` a PRIORITY column, and `--json` `priority`,
  `priority_from` and `queue_pos`. The store goes to schema 9 (columns
  `priority`, `priority_from` and `queue_pos`; existing rows become `normal`,
  placed by id), and the IPC protocol to 9: `task run --priority` and `task
  priority` refuse an older head (`head_too_old`).
- Job slots and burst: two keys on a flock.toml `[[machine]]`, next to
  `max_agents`. `job_slots` (default 1) are extra slots only tasks from jobs
  take, a free one before a shared one, so long `task run` tasks no longer
  keep a job's tasks from starting. `burst` (default 1) lets a `critical`
  task start on a machine whose shared slots are full, while the live tasks
  outside job slots are below `max_agents + burst`. `0` turns either off.
  The picker takes the machine with the fewest live tasks among those with
  room for the task. `machine add` takes `--job-slots` and `--burst`;
  `machine list` shows the room as `2+1j+1b` and `--json` has `job_slots`
  and `burst`.
- Task roles. `pastor task run --role orchestrator` starts a task whose
  agent may, from its own pane, run tasks, retry and send to them, and
  disable a job; every other fleet change stays `agent_refused`, with a
  message naming the role, unless `agents_change_fleet = true`. Only a
  person starts an orchestrator: `--role orchestrator`, or a retry of an
  orchestrator, from any task's pane is `role_refused`. A retry keeps its
  task's role. `task describe` and `task list --json` show `role` (`agent`
  for every other task). The store goes to schema 10 (a `role` column, `agent`
  for existing rows) and the IPC protocol to 12; `--role orchestrator`
  refuses an older head (`head_too_old`). A guard against mistakes, not a
  boundary: the agent runs as the same user as pastor.
- Named models: `[models.<name>]` in pastor.toml, each with a herdr agent
  `kind` and the `args` that select it. A task runs one with `pastor task run
  --model <name>`, a job's `[dispatch] model` (a template, so
  `"{{ item.model }}"` works), or the `model` of its machine, its flock or
  `[defaults]`, in that order. The model's args go before `agent_args`. A
  name `[models]` lacks is `unknown_model`; a model whose kind is not the
  agent's is `model_kind_mismatch`, and an unpinned task only goes to
  machines whose agent has the model's kind. `task describe`, `task list`
  (MODEL, and `model` in `--json`), `flock describe`, `machine describe` and
  task events show it. The IPC protocol goes to 8, and every command that can
  make the head queue a task refuses an older head (`head_too_old`).
- An agent per kind: `agents = { <kind> = "<agent>" }` on a `[[machine]]`
  or `[[flock]]` in flock.toml, or under `[defaults]` in pastor.toml, names
  the agent that runs a model whose kind differs from the default agent's.
  The kind is looked up through the machine, the flock and `[defaults]`: a
  layer's own `agent` of that kind, else its `agents` entry. An agent found
  this way gets `agent_args` only from layers whose `agent` is that agent. An
  unpinned task goes only to machines where the lookup finds one; with none
  in the flock it waits and `task describe` says why. An entry whose agent
  has another kind, or one for the kind of the layer's own agent, fails the
  load. `task describe` shows `(from machine <m> agents.<kind>)`, and
  `machine describe` and `flock describe` list the entry as `by kind`. An
  older pastor refuses the key on a `[[flock]]`.
- `docs/recommended-setup.md`: how to set up a fleet you'll keep. It covers
  flocks per account, least-privilege credentials for agent machines (a
  deploy key and a fine-grained token for one repository, so they can't
  merge), permissions for unattended agents, fresh code for every task, and
  `close_done_after` for short tasks. The docs test checks its commands too.
- `pastor task attach` reopens a finished Claude task. Dispatch starts a
  `claude` agent with `--session-id <uuid>` after its other args (unless they
  already choose a session) and records the id on the task (`session:` in
  `task describe`, `spec.session_id` in JSON). Attaching to a closed or
  failed task whose agent is gone opens a workspace `t-N-resume` on the
  task's machine, in its directory and with its agent definition's env, runs
  `claude --resume <uuid>` there and attaches; the task does not change. A
  worktree removed at close is re-created first on the task's branch
  (`branch_gone` when the branch is gone too). Tasks of other agents keep the
  old error, with a hint that only Claude tasks can be reopened. Closing done
  panes early (`close_done_after`) no longer loses the conversation.
- A connector can declare a `[finish]` command in its manifest (`command`,
  and a `timeout` that defaults like a hook's). The head runs it once, with the
  connector's env and secrets, when a task of one of its jobs reaches `done`
  or `failed`, so the connector can act where the work came from. Stdin is
  the task row with its item, the final state, the job, the branch and
  `last_output`, the last lines read from the agent's pane. A failure or
  timeout is logged, emitted as `connector.finish_failed` and never changes
  the task. `connector describe` shows the command.
- `pastor head set <dest> [--pastor PATH] [--force]`, `pastor head show
  [--json]` and `pastor head unset`: the CLI can use a head on another
  machine, over ssh and `pastor bridge`. The setting is `[head]` in
  `~/.config/pastor/client.toml`; `PASTOR_HEAD` and a global `--head <dest>`
  override it. `head set` pings the head first and refuses with
  `head_unreachable`, `no_head` or `head_too_old` unless `--force`. With a
  remote head, `task` commands (but `attach`), `machine list`, `tick` and
  `job list|run|reload` go to it; other commands that would act on local
  files fail with `remote_head_unsupported`.
- `pastor bridge --agent --machine <name>` is a bridge locked to one
  machine's agents: it passes on `ping`, a task list cut to the machine's
  flock, and `describe`, `read` and `done` for a task placed on that
  machine, sets the request's task itself, and answers anything else
  `not_allowed_for_agent` without reaching the head.
- `pastor machine authorized-key <name> --key <file|->` prints the
  `authorized_keys` line that locks a machine's key to that bridge. It edits
  no file. The manual's "Agents on other machines" has the steps.
- `pastor trust add <machine> <repo>` saves a folder trust without a blocked
  task.
- `head_address` in `pastor.toml`: the ssh destination other machines reach
  the head by. When set, agents on machines other than the head's own start
  with `PASTOR_HEAD=<head_address>` in their pane, which `pastor head`
  above already routes their CLI commands through. Agents on the head's
  machine get none.
- With a head set, `pastor serve` runs headless instead of refusing: it runs
  this machine's jobs and connector hooks and nothing else. The new items a
  job run finds go to the head in one `JobSubmit`, with the job's
  `[dispatch]` table as written, and the head renders, queues and dispatches
  them as that job's tasks; keys it queued or had seen are marked seen here,
  so a lost reply does not hold the job's cursor. A head that does not
  answer fails the run with `head_unreachable`, and a head with a job file
  of that name with `job_name_taken`: a failed run with backoff, and no item
  kept, so the next run asks for them again. The head's events
  come back each tick for the hooks here. It keeps job state, seen keys and
  its event cursor in `shepherd.db`, and answers `ping` (role `shepherd`),
  `tick` and `job list|run|reload` on the local socket, `shepherd_unsupported`
  to the rest. An unreachable head is a `shepherd_needs_head` warning in its
  log, asked again each tick. It refuses to start beside a head
  (`head_running`), and a head refuses to start beside it
  (`shepherd_running`), as does `head set` pointed at it, even with
  `--force`. `pastor setup systemd` installs it the same way.
- With a remote head, `pastor events` (with `--follow`, `--task` and
  `--json`) reads the head's log through `events_since`: a page at a time
  from the start, then a poll every second with `--follow`. Records rotated
  out before they were read get one warning line on stderr.
- A headless serve's hooks decide `only_own` from this machine's job files,
  so they hear the tasks of its own jobs, and a head event rotated out of
  the head's log before it was read is a `head_events_gap` warning, after
  which the hooks go on from the oldest record left.
- With a head set, `pastor job list` shows the head's jobs under `head:
  <dest>` and this machine's under `shepherd: <host> (this machine)`, a side
  with none saying `no jobs`; `--json` is one flat array whose jobs carry
  `where` (`head` or `shepherd`). This machine's jobs come from its headless
  serve, or from the job files and `shepherd.db` when it is down; a serve that
  does not answer is `shepherd_unresponsive`. `job
  run|enable|disable|describe|edit` go to this machine when the job's file is
  here, and to the head otherwise, so the last four no longer fail with
  `remote_head_unsupported`.

### Changed

- Publishing a release's draft on GitHub publishes the crate to crates.io
  (`pastor-cli`) from the tag; a prerelease is skipped. It was a manual
  `cargo publish` before.
- `pastor flock default` is split in two: `pastor flock default show` prints
  the default flock, and `pastor flock default set <name>` makes another flock
  the default. The old `pastor flock default <name>` is gone. `show` reads
  flock.toml only and never asks the head.
- The CLI uses one verb per action, and the old names are gone with no
  alias: `pastor task show` is `pastor task describe`, like the other
  nouns' `describe`; `pastor connector run` is `pastor connector try`, since
  it creates no tasks; `pastor open <machine>` is `pastor machine open
  <name>`. Every command and argument has a line of help, each `--json`
  says the shape it prints, `machine add` refuses more than one of an ssh
  target, `--local` and `--command`, and a bad task id says both forms,
  `t-12` and `12`.
- With a head running, `pastor trust list|add|remove`, `pastor flock
  describe` and `pastor machine describe` ask it (new requests, IPC protocol
  9) instead of reading the local store and `flock.toml`, so a CLI on
  another machine gets the head's answer. The two describes show the flock
  the head last applied. A head from before this is refused with
  `head_too_old`; with no head the commands work as before. `trust add` and
  `trust remove` now count as changing the fleet, so an agent pastor started
  is refused them unless `agents_change_fleet` is on.

## 0.6.0 - 2026-09-26

### Added

- Tab completion offers real names in fish and bash: `pastor job describe
  <TAB>` lists the job files, and flocks, machines, live tasks (with their
  note) and installed connectors complete the same way. The scripts ask
  `pastor __complete` at TAB time, which reads local files and the store and
  never waits on the head.
- `pastor connector describe <id> [--json]` shows one connector in full: its
  manifest, where it came from (source, ref, commit and install time for an
  installed one, the directory for a linked one and whether it still exists),
  its connector command and hooks, its config keys, which declared secrets
  its `.env` is missing (names only), the jobs that use it with their last
  run and result, and its status, or why it does not load. `connector
  install` now records its origin in `.<id>.install.json` beside the
  checkout; a connector installed earlier shows what its checkout still
  tells, and unknown for the rest. Manifests may carry `authors`,
  `homepage`, `repository` and `license`; pastor 0.5.0 rejects them, so a
  connector that adds them should raise its `min_pastor_version`.

- `pastor task done` lets an agent end its own task from its pane (the task
  defaults to `PASTOR_TASK`), the one change the fleet guard allows an agent;
  another task's is refused with `agent_refused`. The task is `done` at once
  and stays so while the agent finishes its turn, and auto-close takes its
  pane after `close_done_after`, freeing the machine's slot. A human may end
  any task with a pane. The store goes to schema 7 (`tasks.ended`).

- `place` decides where a task's agent gets its pane: `--place` on `task run`
  and `task retry`, `place` in a job's `[dispatch]` or under `[defaults]` in
  `pastor.toml`. The default, `repo`, keeps an agent under the repo it works
  on: a task whose `--repo` a herdr workspace already shows, such as a fix
  round in a pull request's worktree, now gets a new pane in that workspace
  instead of a new top-level workspace. `own` always makes a workspace `t-N`,
  as before; `pastor` puts every task in one `pastor` workspace per machine,
  made on first use; `pane:<workspace>` puts it in the workspace with that
  label and fails the task if the machine has none. Closing a task closes only
  its own pane, never a workspace it joined, and a worktree task placed in a
  shared workspace still gets, and on removal loses, its worktree on disk.
  `task show` prints the place. A worktree another agent is working in (a
  fix round that joined its workspace) is not removed: auto-close keeps it
  with a note and `task close --remove-worktree` refuses.

### Changed

- Dependabot watches the crates and the GitHub Actions weekly, and a security
  report goes through GitHub's private vulnerability reporting; the issue
  template no longer offers a public fallback.

### Security

- `create_private_dir`, which makes the config, state and data dirs, refuses a
  directory owned by another user, or a symlink owned by another user, instead
  of using it. With `PASTOR_STATE_DIR` under a shared path such as `/tmp`,
  someone else could otherwise plant the dir that receives the ssh and IPC
  sockets. A symlink of the user's own is still followed. Before making a
  missing dir it checks the ancestors that exist and refuses a symlink owned
  by neither the user nor root, or a dir anyone can write to without the
  sticky bit that neither owns.
- A hook hearing about a task of a job another connector owns, or of a
  one-off task, gets the task with `item` null and `prompt` empty, and its own
  `@<id>` scratch dir as `PASTOR_CONNECTOR_STATE_DIR` rather than the job's.
  A notifier no longer receives the text of every other connector's items,
  nor another connector's cursors.
- Saved trust reads the pane before pressing an agent's trust keys, and
  presses them only while the prompt at the bottom of the pane, not its
  scrollback, shows the trust prompt's `trust_marker`, a new
  `[agents.<name>]` setting; Claude's is built in as "Yes, I trust this
  folder". A task blocked on another dialog the same keys would accept, such
  as Claude's bypass-permissions warning, is left for a human.
- Item and pane text printed for a human has its control characters escaped:
  the NOTE column of `task list` (an item title, now cut to 60 characters),
  every field and the prompt of `task show`, and `task read`. `one_line`
  escapes every C0 and C1 control, not only CR and LF. `--json` is unchanged.
- The confirmation `connector install` shows escapes the manifest's strings,
  shell-quotes its commands and marks the hooks that hear about every job.
  `connector link` shows the same, and warns when the directory or its
  manifest is group- or world-writable or owned by another user.
  `connector install --ref` refuses a ref that starts with `-`.
- A poll connector run that returns more than 10,000 items or 64 MiB of them
  fails instead of holding it all; a stream's buffer is bounded at 64 MiB as
  well as 10,000 items, the unacked batch included, as each item arrives.
- Every command pastor runs over ssh goes as `sh -c '<command>'`, so a fish
  or csh login shell on a machine no longer breaks the repo check. A machine's
  `session` may not contain a backslash or a control character.
- `pastor.service` runs with `NoNewPrivileges`, `UMask=0077`,
  `LockPersonality` and `RestrictRealtime`. `pastor setup systemd` leaves
  empty, relative, missing and world-writable entries out of the unit's
  `PATH`, looks `herdr` up only in the entries it keeps, refuses a value with
  a line break and writes `$` in `ExecStart` as `$$`.

## 0.5.0 - 2026-09-26

### Added

- `pastor task send` types into a done task whose pane is still open, and
  the task goes back to running. An agent marked done with its work
  unfinished can be told to finish in the same pane, with its context, instead
  of a new task in its worktree. A closed task, or one with no pane, is still
  `task_not_live`.

- Agents pastor starts may no longer change the fleet. Every agent's pane gets
  `PASTOR_TASK=t-N`, and a command from it that runs, sends to, attaches to,
  retries, closes or prunes tasks, ticks (dry runs too), runs or reloads jobs,
  installs, links, uninstalls or unlinks connectors, edits machines, flocks or
  jobs, starts or sets up a head, or opens herdr's UI fails with `agent_refused`; reads still work. The head refuses such a
  request, and the CLI refuses the edits it makes on its own.
  `agents_change_fleet = true` in `pastor.toml` allows them again. A worktree
  task's agent now always runs in a pane split off the worktree's, since the
  mark is env and herdr's worktree calls take none.

- A second agent skill, `skills/spec` (`/pastor:spec` once the repo is
  installed as a Claude Code plugin; `.claude-plugin/plugin.json` makes it
  one). It starts from a superpowers brainstorm and writes a plan whose tasks
  each carry a flock, repo, branch, model, timeout and a prompt file that
  needs no answers, run with `pastor task run --prompt-file`. The plan, its
  prompts and a ledger live on a plan branch so any machine can resume;
  tasks run one after another.

- `describe` for one thing in full, kubectl style, each with `--json`:
  `pastor job describe <name>` (schedule, connector and its config, dispatch,
  last runs and errors, next run, recent tasks and job events),
  `pastor machine describe <name>` (host, flock, channel, versions, agents,
  tags, its tasks, recent errors), `pastor flock describe <name>` (default or
  not, its agent and args, machines, queued and running tasks), and
  `pastor task describe <id>`, an alias of `task show`.
- `edit` for the files behind them: `pastor job edit <name>`,
  `pastor flock edit` and `pastor config edit` open a copy in `$VISUAL`, else
  `$EDITOR`, else `vi`, check it the way the head loads it, and only then
  replace the file atomically and reload a running head. An invalid edit
  offers to reopen with the error on top; declining keeps the file as it was
  and names where the edit is kept (`invalid_edit`). A symlinked job file is
  edited at its target. `editor_failed` and `edit_conflict` cover an editor
  that fails and a file changed during the edit.

- `pastor setup launchd [--herdr]` installs pastor (or the herdr server) as a
  macOS LaunchAgent in `~/Library/LaunchAgents`, with the same action flags
  and confirmation prompt as `pastor setup systemd`.
- `pastor task run --prompt-file <path>` reads the prompt from a file on the
  machine running the CLI (`-` for stdin), so a long prompt with quotes needs
  no shell quoting. It conflicts with the positional prompt; exactly one is
  required. Trailing newlines are trimmed. An unreadable file fails with
  `prompt_file_unreadable`, an empty one with `prompt_file_empty`.

- A machine can name its own agent: `agent` and `agent_args` under a
  `[[machine]]` entry in `flock.toml`, before its flock's and `[defaults]`, so
  machines of one flock can run different agents. The head settles a task's
  agent again when it places it on a machine, and `pastor task show` prints
  where the agent and its args came from.

- A flock can name the agent its tasks run: `agent` and `agent_args` under a
  `[[flock]]` entry in `flock.toml`. A task or job that names none takes its
  flock's, then `[defaults]`, then the built-in `claude`; `--agent` and
  `--agent-arg` always win. The head settles the agent when it queues the
  task, and `pastor task show` prints what it resolved to.
- Tool allow and deny lists: `allow` and `deny` (tool patterns such as
  `"Bash(git:*)"`) under `[defaults]`, a `[[flock]]` entry and a job's
  `[dispatch]`. They add up across the three and deny wins over allow. pastor
  passes them as the agent's own flags, `--allowedTools` and
  `--disallowedTools` for Claude, and leaves its permission mode alone;
  `[agents.<name>] allow_flag` and `deny_flag` name them for another agent,
  and a task whose agent has none for a list it carries is refused
  (`agent_tools_unsupported`). `pastor task show` prints both lists.
- Agent definitions can set `kind` and `env`: `[agents.claude-personal]`
  with `kind = "claude"` and `env = { CLAUDE_CONFIG_DIR = "~/.claude-personal" }`
  starts a claude whose pane has that env, `~` expanded against the task
  machine's home. Built-in trust keys and tool flags follow the kind, and a
  task, job or flock names the definition like any agent. A worktree task
  gets the env through a pane split off the worktree's, since herdr's
  worktree calls take none.
- A Trust model section in the manual, and a warning there on agent args
  that turn permission checks off, such as `--dangerously-skip-permissions`.

- A release workflow (`.github/workflows/release.yml`). Pushing a `vX.Y.Z`
  tag builds static musl binaries for x86_64, aarch64, armv7 and riscv64
  Linux and native macOS arm64 and x86_64 binaries, packs each as
  `pastor-<version>-<target>.tar.gz` with `SHA256SUMS`, attests their build
  provenance, and drafts a GitHub release with that version's changelog
  section as notes.
- `install.sh`: `curl -fsSL https://raw.githubusercontent.com/cacarico/pastor/main/install.sh | sh`
  picks the tarball for the OS and architecture, checks it against
  `SHA256SUMS` (and the provenance attestation when a logged-in `gh` is
  present), and installs the binary to `~/.local/bin` without sudo.
  `PASTOR_VERSION`, `PASTOR_INSTALL_DIR` and `PASTOR_DOWNLOAD_URL` override
  the version, the directory and the download source.
- CI runs a `portability` job on every pull request: `cargo check` of the
  whole crate for x86_64 musl, armv7 musl and x86_64 FreeBSD through zig, so
  a change that only compiles against glibc fails on the pull request, not
  on the release tag.

### Changed

- Breaking: plugins are now called connectors, everywhere, and the old names
  are gone. `pastor plugin ...` is `pastor connector
  install|link|uninstall|unlink|list|run`; the manifest is
  `pastor-connector.toml`; commands get `PASTOR_CONNECTOR_ID` and
  `PASTOR_CONNECTOR_STATE_DIR`; `PASTOR_PLUGIN_GIT_BASE` is
  `PASTOR_CONNECTOR_GIT_BASE`; the `plugins/` directories under the config,
  data and state dirs are `connectors/`. To upgrade, rename each manifest and
  move the three `plugins/` directories to `connectors/`, or reinstall. A
  connector still holds a connector command, event hooks, or both. "Plugin"
  is kept for a later idea: code that changes how pastor itself behaves.

- The crate is `pastor-cli` on crates.io, because `pastor` there belongs to
  another project: `cargo install pastor-cli` and `cargo binstall pastor-cli`
  install the `pastor` binary. The command, the release tarballs and
  `install.sh` keep the name pastor, and `fake-herdr` is left out of the
  published package.

- On macOS the config, state and data dirs follow the XDG layout, as on
  Linux: `~/.config/pastor`, `~/.local/state/pastor`, `~/.local/share/pastor`
  instead of `~/Library/Application Support/pastor`. A config left in the old
  place is moved once, with a note; its `plugins/` goes to the new
  `connectors/` dirs, and a move that fails part way is finished by the next
  run.
- Agent args follow the agent they were written for: `[defaults] agent_args`
  no longer reach a task that runs another agent than `[defaults] agent`
  (`pastor task run --agent codex` used to get Claude's `--model`).
- The head protocol is 2. `pastor task run`, `pastor task retry`,
  `pastor tick` (not `--dry-run`) and `pastor job run` refuse a head below it
  (`head_too_old`), which would start the agent without its tool lists:
  restart `pastor serve` after upgrading.
- The head, not the CLI, settles a `pastor task run` task's agent, since only
  it knows the task's flock. `pastor task run` has the head apply any edit of
  `pastor.toml` or `flock.toml` first, so an edit still takes effect at once.

- `cargo install` and `make install` install only the `pastor` binary
  (`--bin pastor`); `fake-herdr` is a test double and no longer lands on
  the PATH.
- Release builds are stripped and link-time optimised, so a downloaded
  binary is about a third smaller.
- `Cargo.toml` declares the minimum Rust version (1.88) and the repository,
  and carries cargo-binstall metadata pointing at the GitHub release
  tarballs.

### Security

- Item values rendered into a job's prompt lose every control character but
  newline and tab (C0, DEL and C1), so an issue title or chat message cannot
  type key presses into the agent's terminal. The job's own prompt text and
  `pastor task send` are unchanged.
- Lines read from herdr are capped at 16 MiB and the output of the ssh probes
  (home, repo directory, pastor version) at 64 KiB per stream. Past the cap
  the connection is dropped with a protocol error, or the probe is killed,
  so one machine cannot exhaust the head's memory.
- An item value put into `repo` or `branch` may no longer be empty (which a
  missing field renders to) or `.`, so it cannot point the task at the
  template's parent directory.
- A job whose `branch` puts an item value in its first component, such as
  `{{ item.branch }}`, is invalid: the job must fix a prefix like
  `pastor/{{ item.key }}`, so an item cannot pick an existing branch such as
  `main`.
- An ssh target that is empty, starts with `-`, or holds whitespace or a
  control character makes `flock.toml` fail to load, and every ssh command
  pastor runs passes `--` before the target.
- `pastor serve` logs a failed `accept` on its socket and keeps serving
  instead of exiting. An IPC request line is capped at 1 MiB
  (`request_too_large`) and must arrive within 10 seconds.
- `SECURITY.md` and the manual's new Trust model section state that the
  head's user is the fleet's trust boundary, that `local = true` machines
  should not run untrusted work, and what plugins inherit.

### Fixed

- An agent that ends its turn on a question is marked `blocked` with the
  question as its error, not `done` with nothing pushed; `pastor task send`
  answers it. A pane read lost to a dropped connection keeps the task pending
  instead of settling it.
- Auto-close keeps a worktree whose commits are on no remote, closes the pane
  and notes the branch on the task, as it does for uncommitted changes.
- After a trust answer (`pastor task send --trust` or the head's own), the
  task's prompt is held for `settle` before it is sent. Claude redraws its
  prompt box then, and a prompt sent into that window was lost, leaving the
  agent at an empty input.
- `pastor machine list` refreshes a machine's pastor version while it stays
  connected, so an upgrade shows without restarting the head.
- The first `flock add --default` on a file without `[[flock]]` entries moves
  the machines that name no flock into the new flock, instead of writing them
  into an explicit `default` flock. Machines stay while tasks are queued in
  the implicit flock, and the output names those tasks.
- Connector commands (`install`, `link`, `uninstall`, `unlink`) reuse the
  CLI's head check instead of probing the head a second time.
- A prerelease build (`0.5.0-rc.1`) no longer panics on its own version: it
  compares as the release it leads to for `min_pastor_version`.
- A `local = true` machine on macOS finds herdr's socket under
  `~/.config/herdr`, where herdr puts it.
- The ssh ControlPath length check uses macOS's 104-byte socket path limit
  there, not Linux's 108, so a long state dir no longer breaks multiplexing.
- `pastor setup systemd` and `pastor setup launchd` write the effective
  config, state and data dirs into the pastor service as absolute
  `PASTOR_CONFIG_DIR`, `PASTOR_STATE_DIR` and `PASTOR_DATA_DIR`, so a head
  started at login uses the dirs the setup run used, not only when an override
  was set.

## 0.4.0 - 2026-09-25

### Added

- Named flocks. `[[flock]]` entries in `flock.toml` declare them, one with
  `default = true`, and `flock = "..."` puts a machine in one; a machine
  without it is in the default flock. A file with no `[[flock]]` entry is one
  flock named `default`, so existing files load unchanged.
- A task and a job target one flock and only its machines take their tasks:
  `--flock` on `pastor task run`, `flock` under a job's `[dispatch]`, else the
  pinned machine's flock, else the default. `--machine` outside `--flock` is
  refused (`flock_mismatch`), an unknown flock too (`unknown_flock`).
  `pastor task retry` keeps the flock, and is refused (`unknown_flock`) once
  it is removed. A head from before flocks is refused (`head_too_old`) by
  every command that talks to or reloads it while flocks are in play:
  `--flock`, an edit of flock.toml, or named flocks declared there. `Ping`
  answers the head's IPC protocol for this.
- `pastor flock list|add [--default]|remove|default`, `pastor machine move`,
  and `--flock` on `machine add`, `machine list` and `task list`. `task list`,
  `task show` and `job list` show the flock, and task events carry it in the
  task row.
- `pastor task send <task> [TEXT] [--key K]... [--no-enter]` types into a
  live task's agent through the head, to answer what it is waiting on without
  attaching. Only starting, running and blocked tasks take input
  (`task_not_live`). Each send emits `task.input`, which records the key
  names and the text length, never the text.
- Saved repo trust. `pastor task send <task> --trust` presses the agent's
  folder-trust keys to a task blocked on its startup prompt
  (`not_at_trust_prompt` otherwise) and saves the task's machine and repo;
  the head then answers the trust prompt of that repo's later tasks on that
  machine on its own, once per task, and emits `task.trusted`. `[agents.<name>] trust_keys`
  in `pastor.toml` sets the keys (Claude's, `Down` then `Enter`, are built
  in). `pastor trust list [--json]` and `pastor trust remove <machine>
  <repo>` show and revoke it.
- Events carry an optional `detail` object for what they add beyond their
  ids; records without one are unchanged.

### Changed

- Every command that talks to or reloads the head pings it once, first, and
  acts on that answer throughout. A head that holds the socket but does not
  answer is an error (`head_unresponsive`) on all of them, where `tick`,
  `job list`, `task list|show`, `flock list|remove` and `job enable|disable`
  used to take it for no head and work offline next to it, and `machine add`
  and `connector install|link|uninstall|unlink` made their change and only
  warned. `task prune` answered `daemon_unresponsive` for this; it now
  answers `head_unresponsive` like the rest.
- `pastor machine list` opens with a line about the head (its pastor and
  herdr versions, its host, the number of machines, and which machine it is
  when it is one) instead of a first table row named `pastor`, and has a
  FLOCK column after HOST. The head's own machine comes first. `--json`
  keeps its shape, with `flock` on each machine.
- `machine add|remove` and the new flock commands edit `flock.toml` in place,
  keeping comments and layout; they used to rewrite the whole file.
- The database is schema 6. Schema 4 stores each task's flock; rows from
  before flocks join the default flock. Schema 5 adds the `trusted_repos`
  table and a `trust_sent` flag on each task. Schema 6 stores whether the
  task's agent has been seen at work since its prompt (`activity_seen`).

### Fixed

- An agent that exits between turns (a human typing `/exit` once the work
  is done) leaves a running task `done`, not `failed` with "agent process
  exited". An exit while starting, blocked or working still fails the task.
- `pastor task retry` of a failed worktree task no longer fails with git's
  "fatal: '<path>' already exists": the retry reopens the old task's
  checkout with `worktree.open`, on its branch, when the old task made that
  checkout, it is still on disk at the same path, the old agent is gone and
  no agent is in its workspace (an earlier retry may be working there).
  Any other retry, a stale task's included, gets its own branch and
  worktree.
- A task whose agent finished its work and sat idle, waiting for input, no
  longer stays `running` until the session ends when the daemon restarted,
  or the flock or settings were reloaded, while the agent worked: whether
  pastor saw the agent at work is stored with the task instead of held in
  the machine actor's memory, so the new actor settles the idle agent as
  `done`.

## 0.3.0 - 2026-09-25

### Changed

- The README is a short introduction with a recorded demo; the full reference moved to `docs/manual.md`. `make demo` records the gifs with vhs.

### Added

- An agent skill, `skills/pastor/SKILL.md`, for agents that drive pastor or
  were dispatched by it. `pastor --skill` prints the copy built into the
  binary, and `pastor --help` points agents at it.
- Plugins: a directory with a `pastor-plugin.toml` that provides a connector,
  event hooks, or both. `pastor plugin install|link|uninstall|unlink|list|run`
  manages them; secrets live in `~/.config/pastor/plugins/<id>/.env` and are
  redacted from the capped run logs under `~/.local/state/pastor/runs/`.
- Process connectors, poll and stream: a job names a plugin's connector with
  `[connector] use = "<id>"`, and `pastor serve` and `pastor tick` run it.
  A stream keeps a batch until a run has persisted it; a standalone
  `pastor tick` refuses stream jobs, which need `pastor serve`.
- Event hooks: a plugin's `[[events]]` commands get each matching event on
  stdin, one plugin at a time in event order, from a bounded queue.

- `pastor machine list` adds a HOST column (ssh target, `local`, or a command
  machine's program) and a first row for the head itself, with its hostname
  and local herdr version. `--json` is now `{"head": {...}, "machines": [...]}`
  instead of a bare array.
- Without `pastor serve`, `pastor machine list` probes each machine directly
  instead of printing the flock file with no live data. CHANNEL reads
  `probed`, `server down`, `unreachable` or `error`. `pastor machine status`
  is folded into it.
- `pastor machine list` has a PASTOR column after HERDR: the pastor installed
  on each machine (`-` when there is none or it cannot be known), and the
  head's own version on the head row. The head asks each machine once per
  connect; `--json` carries it as `pastor_version`.
- `flock.toml` and `pastor.toml` are reloaded while `pastor serve` runs:
  machines are added, removed or replaced and new timings and defaults apply
  without a restart. `pastor tick` reloads them too.
- `close_done_after` in `pastor.toml` (default `15m`, `never` disables it):
  pastor closes a done task's pane after the grace period, removes the
  worktree it created when it is clean, and keeps a dirty one with a note on
  the task. Failed, blocked and stale tasks are never closed on their own.

### Changed

- `pastor tick` under `pastor serve` spawns its runs, so a slow connector no
  longer holds the scheduler.
- Item fields put into a job's `repo` or `branch` may not hold path
  separators, `..`, a leading `-` or control characters; such an item is
  skipped and reported.

### Removed

- The old spellings `pastor run`, `pastor list`, `pastor attach`,
  `pastor reload` and `pastor machine status` are gone. Use
  `pastor task run|list|attach`, `pastor job reload` and
  `pastor machine list`.

### Fixed

- A dispatch no longer fails when the new pane's shell is still starting.
  herdr answered `agent.start` with `agent_pane_busy` about a second after
  `workspace.create`, and pastor failed the task at once; it now retries up
  to 5 times, 500ms apart, and only then fails with herdr's message plus
  `after 5 attempts`.
- `pastor serve` now also shuts down cleanly on SIGTERM and SIGHUP, not just
  SIGINT (ctrl-c). systemd counts all three as a clean exit; the head used to
  handle only SIGINT, so a SIGTERM (a plain `kill`, `systemctl stop` of a
  wrapper, a stray signal) killed it silently, left the socket file behind,
  and the `Restart=on-failure` unit never came back.
- The `pastor.service` and `herdr.service` templates now use
  `Restart=always` instead of `Restart=on-failure`, so a head or herdr
  server killed by a signal systemd treats as clean restarts too.
  `systemctl --user stop` still stops it for good.
- `pastor task prune` no longer deletes a worktree task whose checkout may
  still be on disk. It keeps the row, names it, and points at
  `pastor task close t-N --remove-worktree`, which clears the recorded
  workspace so the next prune takes it. `--json` reports the kept ids as
  `kept_worktrees`.

## 0.2.0 - 2026-09-25

### Added

- Scheduled jobs: a job is a TOML file in `~/.config/pastor/jobs/`, run on an
  `every` interval or a cron schedule against a connector (`clock` for now).
  Each new item becomes one task; items already seen never fire twice.
- `pastor job list|enable|disable|run` to manage job files, and
  `pastor tick [--dry-run] [--job]` to run or preview a scheduler pass.
- An events log: `pastor serve` appends task, job and machine events to
  `~/.local/state/pastor/events.jsonl` (rotated by size), and
  `pastor events [--follow] [--task t-N] [--json]` reads it, even with the
  daemon down.
- `pastor setup systemd [--herdr]` installs the pastor or herdr user unit,
  checks that lingering is on, and tightens config/state file permissions.
- `pastor task show t-N` prints one task in full, and `pastor task run
  --agent-arg` plus a `[defaults] agent_args` key pass flags such as
  `--model` to the agent.
- CI: `make check` and `make test-machine` run on every pull request and on
  `main`.

### Changed

- `pastor flock add|remove|list|status` is now `pastor machine ...`;
  `flock.toml` keeps its name.
- Task and job commands move under `pastor task run|list|attach` and
  `pastor job reload`. The old top-level `run`, `list`, `attach` and `reload`
  stay as hidden, deprecated aliases for this release.
- `pastor task list` (formerly `pastor list`) shows only live tasks by
  default; pass `--all` to include closed ones too.
- `pastor setup systemd` gained `--enable`, `--start`, `--now` and `--stop`
  to choose the systemd action, plus `--yes` to skip the confirmation
  prompt for scripted and remote use.

### Fixed

- Tasks now reach `done` against herdr 0.9.1. herdr reports no
  `completion_seq`; pastor now uses `state_change_seq` past the prompt's
  reply, requires that it saw the agent working or blocked after the prompt,
  and confirms after the `settle` window. Before, finished agents stayed
  `running` for ever.
- A job run whose task insert fails is reported as a failed run, is not
  counted as a connector failure, and keeps its cursor; runs of one job are
  serialised and queued in command order; an elapsed backoff makes the job
  due at once.
- The store refuses a `meta` table that lost its schema version, creates any
  job tables missing from a current-version database, and reports a corrupt
  failure count against its own column instead of wrapping it.
- The CLI tells a busy head from a missing one: a request that connected but
  timed out reports `timeout` and says the head may still finish it; only a
  refused or missing socket says `pastor serve` is not running.
- The private ssh `ControlPath` directory is created before any ssh command
  that can start a master, including the daemonless CLI paths.
