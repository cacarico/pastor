### Fixed

- A command in a task pane that was not told where the head is (no
  `PASTOR_HEAD`) and finds no serve on its machine fails with
  `head_unknown`, naming `head_address`, instead of `daemon_not_running`
  telling the agent to start `pastor serve`. The head also logs a warning at
  start and after a reload when it has `ssh` machines and no `head_address`,
  since `pastor task done` cannot reach it from their panes.
