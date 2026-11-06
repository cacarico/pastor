### Fixed

- `pastor task send --trust` on a task with no `--repo` now saves the folder
  it started in (`~/pastor-tasks`, as an absolute path) as trusted for the
  machine, as it does for a repo, so every later task with no repo there is
  answered at the folder-trust dialog instead of blocking each time. The
  answer no longer depends on Claude keeping its own record of it, which it
  does not for the home directory or when sessions sharing one config
  directory overwrite each other's `.claude.json`. A pull machine's serve
  now asks the head for its trust for a task with no repo too.
