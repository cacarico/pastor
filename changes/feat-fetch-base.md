### Added

- Before it makes a task's worktree, pastor fetches the remote the repo's
  current branch tracks (`origin` when it tracks none), so the branch point is
  not stale. `pastor task run --no-fetch`, or `no_fetch = true` in a job's
  `[dispatch]`, skips it. A fetch that fails does not fail the task: the
  worktree is made from the repo as it is, `pastor task describe` shows the
  warning (`spec.fetch.warning` in `--json`), and the head emits
  `task.fetch_failed`.
