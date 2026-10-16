### Added

- `version_every` in `pastor.toml` sets how often the head asks a connected
  or polling machine for its pastor version again, after a reconcile. The
  default is `2m`, down from a fixed ten minutes, so a fresh install shows in
  `pastor machine list` within about three minutes. `pastor machine
  describe` shows when the version was last read (`version read`), and the
  row carries it as `pastor_read_at` in `--json`.
