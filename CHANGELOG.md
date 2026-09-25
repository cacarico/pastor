# Changelog

All notable changes to this project are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

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
