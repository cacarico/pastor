### Added

- `session` orchestrators run. One keeps one agent running through its
  `hours` (`{ start, stop }`, local time): the head starts it at
  `hours.start`, or at once inside the hours, with the prompt, the skill,
  the note and `pastor watch --now`; at `hours.stop` its agent gets a last
  message and is closed after `stop_grace`. An agent that dies, goes stale
  or ends early is restarted with the note, at most three times an hour; one
  that stopped on a quota error restarts at the reset. A session holds its
  `max_orchestrators` slot from start to stop, so a scheduled run meanwhile
  is held, and a session due while a scheduled agent works starts once it
  ends. New commands: `pastor orchestrator start | stop`; events
  `orchestrator.restarted|stopping|stopped`. The head's IPC protocol goes to
  25; `start` and `stop` refuse an older head (`head_too_old`).
