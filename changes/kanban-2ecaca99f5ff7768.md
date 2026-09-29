### Fixed

- A done task given more work with `pastor task send` after its timeout no
  longer goes `stale` on the next reconcile: the timeout counts from the
  task's latest start, so a reopen restarts it as a resume already did.
  `started` in `task list` and `task describe` now shows that latest start.
- A `blocked` task no longer goes `stale` past its timeout; it waits for a
  person, however long that takes.
