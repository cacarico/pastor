### Fixed

- Claude's autocompact offer ("We recommend compacting earlier") is a
  built-in dialog, answered with `Keep`. Under a finished turn an idle agent
  is no longer closed as done with no summary while the offer waits for an
  answer; a task whose offer stays up is blocked for a person instead.
