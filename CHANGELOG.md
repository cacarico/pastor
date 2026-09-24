# Changelog

All notable changes to this project are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

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
