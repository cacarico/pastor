### Changed

- A round that ends with no summary on an `ssh` machine of a head with no
  `head_address` has the outcome `no head`, not `no summary`: the agent's
  `pastor task done` could not reach the head, so the missing summary says
  nothing about the work. A task with `summary = "off"` keeps `no summary`.

### Added

- `pastor machine list` and `pastor machine describe` mark each `ssh`
  machine whose agents cannot reach the head because pastor.toml has no
  `head_address`: the head line names them, their ERROR says so, and
  `--json` carries `no_head_address`.
- `pastor setup systemd` and `pastor setup launchd` settle `head_address`
  before starting the head: at a terminal they ask, offering
  `<user>@<this host's name>`; with `--yes` or no terminal they take that
  default. Either way they say which. One already set is kept.
