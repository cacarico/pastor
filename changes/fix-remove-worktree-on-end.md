### Added

- `pastor task run --remove-worktree`, and `remove_worktree` in a job's
  `[dispatch]`, on a `[[flock]]` and under `[defaults]`: the task's worktree
  goes whatever it ends in, failed and stale too, once its agent has stopped
  and its pane would close. Never forced: a checkout with uncommitted or
  untracked files, one another agent works in, or one a workspace pastor did
  not open shows is kept with a note. The branch stays, and `pastor task
  retry` puts the checkout back on it. `task describe` shows it with where it
  came from. IPC protocol 33.

### Changed

- When herdr has no workspace left on a task's checkout, `task close
  --remove-worktree` and auto-close remove it with `git worktree remove` on
  the machine instead of asking you to.
