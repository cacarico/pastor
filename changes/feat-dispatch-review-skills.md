### Added

- `pastor:dispatch`, a skill that runs a `pastor:spec` plan's next task and
  watches it with nobody else looking at the branch: it reads the plan with
  `skills/dispatch/next-task.sh`, starts the next task once a machine in its
  flock is free, waits with no tight loop, and handles a folder-trust
  prompt, a `done` task's outcome, a `failed` task with a known cause and a
  `stale` task by fixed rules. Disabled for automatic invocation, since it
  starts agents on other machines.
- `pastor:review`, a skill that takes a finished plan branch, runs the
  repo's own check locally, reviews the diff fresh, sends back at most one
  fix task for Critical and Important findings with one scoped re-review,
  defers Minor findings to the ledger, and opens the pull request — as a
  draft listing the findings still open if that re-review still finds a
  Critical one. Merges only when asked.
