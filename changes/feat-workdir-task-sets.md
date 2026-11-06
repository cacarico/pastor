### Added

- `task run --workdir-set` and a job's `[dispatch] workdir_set`: a workdir
  set placed in the task's folder through the head before each start of its
  agent (dispatch, resume, a move to another model of the same kind, `task
  attach` of a closed pane). A place that fails fails the start with `workdir_failed`.
  The set must be for the task's kind of agent and name its profile or
  none; a task with a set goes only to push machines. Removing a task's
  worktree takes its sets out with it.
- IPC protocol 50.
