### Removed

- `agents_change_fleet` is gone. An agent pastor started can no longer be
  allowed to change the fleet, run tasks or edit machines, flocks, jobs and
  settings; the guard is always on, and no setting lifts it for an agent or
  an orchestrator's scripts. A pastor.toml that still says
  `agents_change_fleet = false` loads, with a warning that the key does
  nothing; one that says `true` does not load, and the error names the key,
  so nobody believes the guard is off.
