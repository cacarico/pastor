### Added

- A machine can tell a task's agent to commit everything as one wip commit and
  push it before the task moves, and answers once the agent has finished that
  turn. The save is refused with `task_not_live`, `agent_busy` or
  `save_blocked`, and fails with `save_blocked` or `save_timeout` when the
  agent stops to ask or does not finish in time; the task and its pane stay as
  they were. Nothing moves a task with it yet.
