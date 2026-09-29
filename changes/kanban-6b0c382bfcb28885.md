### Added

- pastor keeps what a Claude task used. When a round ends it reads the
  task's session (and its subagents') from the machine it ran on, under the
  agent's `CLAUDE_CONFIG_DIR` or `~/.claude`, and stores the model, API
  calls and input, cache write, cache read and output tokens.
  `pastor task describe` prints them as `used:` and `tokens:` lines, and
  `--json` of `task list` and `task describe` carries `usage`. Other agent
  kinds, `command` machines and pull machines show nothing.

### Changed

- The store is at schema 16: the `task_usage` table. An older store
  migrates on first open; a store at 16 is refused by an older pastor.
