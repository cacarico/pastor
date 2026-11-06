### Fixed

- Editing a machine's `loads` in flock.toml no longer restarts its actor, and
  so no longer fails a dispatch in flight with `ActorStopped`. Dispatch reads
  loads from the flock last applied, as it does for a flock's, so the new
  loads count from the next task.
