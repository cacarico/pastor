### Fixed

- `pastor watch` and a shepherd's hooks no longer go silent when their
  cursor is past the head's newest event, after the head moved or its state
  dir was wiped: the watcher prints one `HEAD reset` line and the shepherd
  logs `head_events_reset`, and both go on from the head's end without
  replaying the events already in its log.
