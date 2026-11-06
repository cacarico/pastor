### Added

- `pastor machine check` has an `inbox` row for each Claude agent: it starts a
  short-lived session on the machine, sends it one message through its inbox
  socket and closes it. It passes when the session takes the message and fails
  with the reason (refused, timed out) when it does not.
