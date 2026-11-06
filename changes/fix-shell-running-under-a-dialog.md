### Fixed

- A Claude task waiting on a background shell is no longer closed as
  finished with no summary when Claude draws a dialog under its turn
  summary instead of the input box, or wraps "1 shell still running" across
  lines: the phrase counts anywhere after the agent's last message.
