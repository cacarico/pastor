### Added

- The head can edit its `authorized_keys` on lines of its own and no others.
  Every line pastor writes ends in a marker (`pastor:machine:<name>`,
  `pastor:agent:<name>`, `pastor:join:<id>`), and a line without one is never
  read as pastor's or changed. Edits take a lock, write a private (0600) temp
  file, sync it and rename it over the original, so two edits at once keep
  every line. Nothing calls it yet.
