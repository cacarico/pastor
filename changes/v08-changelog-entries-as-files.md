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
- Every command works from a machine with a remote head set. The `flock`
  commands, `machine add|remove|move|describe`, `trust`, `profile` and
  `config edit` go to the head and print what they would print there;
  `flock edit` and `config edit` edit the head's file. `task attach` and
  `machine open` still go to the machine directly, but ask the head for the
  task, its flock.toml and pastor.toml; the head's own machine is reached at
  the head's ssh destination. `config edit --local` edits this machine's
  pastor.toml. `machine authorized-key` stays on the head and is refused
  here, naming it. `flock list`, `flock default show`, `profile`, `task
  attach` and `machine open` need a remote head speaking IPC protocol 6.
- The manual has a "Moving the head" runbook.
- Orchestrators run from files. `~/.config/pastor/orchestrators/<name>.toml`
  names its `kind` (required): a `scheduled` orchestrator runs its `pre`
  script on `every` or `cron`, and only when the script prints lines that need
  judgment does the head start one agent, with the `orchestrator` role, on its
  own machine, with the file's `prompt`, `skill` and `model`, the handover
  note and every line; its `post` script gets the agent's end state (`done`,
  `failed` or `stale`), summary and lines. A run is skipped while the last
  agent works or its post script waits, a pre script that fails or prints more
  than 64 KiB of lines backs off as a failing job does, `max_orchestrators` in
  pastor.toml (default 1, outside `max_agents`) holds agents back, and an
  agent that stopped on a quota error holds the next until the reset.
  `session` files are checked (a key of the other kind makes a file invalid)
  but not run yet. New commands: `pastor orchestrator list | describe | run |
  enable | disable | note`; events
  `orchestrator.started|skipped|held|quota|failed`. The pre and post scripts
  run with `PASTOR_ORCHESTRATOR`, which the CLI sends with each request, and
  the head applies the orchestrator role's table to them; `PASTOR_TASK` wins
  when both are set. An orchestrator's agent may keep its note (`pastor
  orchestrator note`). `pastor task list` shows orchestrator tasks in a table
  of their own first. The head's IPC protocol goes to 23; the store keeps its
  schema (orchestrator state lives under `state/orchestrators/<name>/`).
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
  answers, logging to `~/.local/state/pastor/serve.log` (rotated at 10 MB,
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

### Changed

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

### Fixed

- A Claude agent that ended its turn waiting on a background shell (its
  footer says `1 shell still running`) is no longer read as `done`: herdr
  shows it idle, so pastor closed its pane after `settle` and the work it
  would have finished when the shell ended was lost. Its task now stays
  `running` until the shell ends and the agent goes idle again.
- A task with no repo (no `--repo`, no `repo` in its job) opened its pane
  wherever herdr's focused pane was, so it could start in an unrelated
  checkout. It now starts in the machine's home directory; a machine that
  cannot report its home (a `command` one) still leaves it to herdr.
- A task blocked on Claude's folder-trust prompt that a person answered at
  the pane got its prompt at once, while Claude redraws after the dialog
  and drops what is typed, so it read `running` with the agent at an empty
  input. The prompt now waits `settle` after the block clears, however it
  was answered. A prompt the agent still does not take (idle a settle
  window at the sequence it went in at, never seen working) is sent again,
  up to twice, then the task is `blocked` with an error saying so.
