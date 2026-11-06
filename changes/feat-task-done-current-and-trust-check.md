### Added

- `pastor task done --current` does the whole end of a task in one
  command, for the task the pane runs: it pushes the worktree's branch
  first if it has one and it is not pushed yet, sends the summary, ends
  the task and prints `DONE` as its last line. A dirty worktree or a
  failed push leaves the task exactly as it was, with a clear error,
  instead of ending it.

### Fixed

- `task send --trust` now checks that Claude's folder-trust dialog has
  moved onto "Yes, I trust this folder" before pressing `Enter`, and
  refuses (`trust_not_selected`) without saving trust when it has not: a
  `Down` lost on the way to the pane once left `Enter` to choose "No,
  exit" while pastor still marked the repo trusted.
- A subscription event for an agent gone idle no longer throws away a
  settle window `reconcile`'s own `agent.list` poll already opened for
  the same task with the real sequence read off the agent: it only opens
  one if none is open yet, instead of always restarting it at `None`.
