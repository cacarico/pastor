### Added

- `pastor inbox send` hands the text on stdin to a running Claude Code
  session through its inbox socket, listens for the session's receipts, and
  prints one JSON line saying whether the message was accepted, held,
  denied, expired, refused, dropped or had no session to go to.
