### Fixed

- A Claude task's first prompt is no longer lost behind its folder-trust
  dialog when herdr reads the agent ready before it reads the dialog. The
  prompt waits while the dialog shows, as it does for agy and Codex. A
  prompt that went into the dialog anyway is pending again once herdr reads
  the agent `blocked` with the dialog on screen. An agent seen only
  `blocked` and then idle, with no sign of the prompt on its screen, gets
  the prompt again instead of its task ending with no summary.
