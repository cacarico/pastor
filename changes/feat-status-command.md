### Added

- `pastor status` shows the fleet on one screen: the head and its version,
  machines up and down, live tasks by state, failing jobs and exhausted
  accounts. `--json` prints it as an object. With no head it fails with
  `not_running` unless `--probe` is given, which probes the machines and
  reads the rest from the files here.
