### Added

- `pins` in a `[[flock]]` entry names the version of each harness CLI
  (`claude`, `agy`) its machines should run, and a `[[machine]]` entry's
  `pins` replace a kind's: `pins = { claude = "2.1.310", agy = "1.4" }`. They
  are checked at load and only checked after: `pastor machine list` has a
  HARNESS column (`-`, `ok` or `drift claude`) and `--json` carries each
  pinned kind with its pin, version and drift, and `pastor machine check`
  adds a `harness-version` row per agent that warns, without failing the
  check, when the CLI is at another version. Nothing installs yet.

### Changed

- `pastor machine list` has one more column, HARNESS, after PASTOR.
