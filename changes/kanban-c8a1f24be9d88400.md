### Fixed

- A task whose agent is not Claude is no longer `done`, and its pane no
  longer closed, while its agent still works: herdr reads agy idle through a
  long thinking pause and while it waits on a command it started. Idle after
  working without `pastor task done`, such a task goes `blocked` with `idle
  without task done` and the pane's last lines, runs again when the agent
  does, and is done on its `task done`. agy's `Requesting permission for:`
  and `Run this command?` read as a question, and its `· N task` footer
  keeps the task `running`. A task with `summary = "off"` and every Claude
  task end on the idle as before.
- A Codex task in a folder Codex has not trusted yet gets its prompt after
  the trust answer instead of into the dialog: herdr reads that dialog idle,
  so pastor reads the pane first and holds the prompt with the task
  `blocked`. An agent's own `trust_marker` holds it the same way.

### Changed

- `make smoke-profiles` runs its tasks with `--summary off`, so an opencode
  review ends on its idle as it did.
