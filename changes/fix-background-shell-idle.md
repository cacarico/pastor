### Fixed

- A Claude task waiting on a background shell is no longer read as
  finished: Claude 2.1.3xx shows the shell in its footer as a bare
  `· 1 shell ·` segment, without "still running", and pastor now reads that
  too, so the task stays running until the shell ends and the agent's next
  turn does.
- A task that must end with a summary and stops without one, its worktree
  holding uncommitted files, goes `blocked` instead of `failed`, keeping its
  pane and session, so an agent cut off before it committed can still
  finish with `pastor task done`.
