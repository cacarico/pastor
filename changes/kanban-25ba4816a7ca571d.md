### Added

- Claude's limit picker ("Stop and wait for limit to reset", "Upgrade your
  plan", "Use extra usage"), which herdr shows as `blocked`: pastor reads
  the pane once a blocked spell, picks "Stop and wait" by its text wherever
  it sits in the list, emits `task.input` with `limit_picker: true`, and the
  task waits for its reset. A picker without that option, or in words
  pastor does not know, gets no key; the task stays `blocked` with an error
  saying it looks like a limit picker. pastor never picks extra usage or an
  upgrade, and no setting makes it.
- A 429 or 529 that outlived Claude's own retries is retried in the task's
  pane, which keeps the conversation, its slot and its state: after each
  wait of `[limits] rate_backoff` (at least the wait the message names)
  pastor types "the API was busy; carry on where you left off" and Enter,
  with `task.rate_limited` (`line`, `attempt`, `retry_at`) before each. A
  turn that ends otherwise starts the count over; after `rate_retries`
  retries the next one is handled as a hard limit with no reset.

### Changed

- `[limits] rate_backoff` is a list of waits, `["1m", "5m", "15m"]` by
  default, the last one repeating; one wait alone is still read, as a list
  of one.
