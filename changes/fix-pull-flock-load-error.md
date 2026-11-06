### Fixed

- A pull machine's `pastor serve` no longer starts without the env layers of
  its own flock.toml when that file does not load. It exits with the file and
  the reason (a syntax error, an `env` name that is not valid, a value that
  looks like a secret); it used to carry on as if the file were not there and
  run tasks without the env it sets.
