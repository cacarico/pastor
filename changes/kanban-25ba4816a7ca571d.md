### Added

- Claude's limit picker ("Stop and wait for limit to reset", "Upgrade your
  plan", "Use extra usage"), which herdr shows as `blocked`: pastor reads
  the pane once a blocked spell, picks "Stop and wait" by its text wherever
  it sits in the list, emits `task.input` with `limit_picker: true`, and the
  task waits for its reset. A picker without that option, or in words
  pastor does not know, gets no key; the task stays `blocked` with an error
  saying it looks like a limit picker. pastor never picks extra usage or an
  upgrade, and no setting makes it.
