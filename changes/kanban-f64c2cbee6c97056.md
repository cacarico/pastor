### Added

- An agy task that stops on its quota (`RESOURCE_EXHAUSTED (code 429):
  Individual quota reached. ... Resets in 4h21m30s.`) goes `waiting` until
  the reset instead of `done`, or moves to its next model, and starts again
  from its prompt with the handover, since agy keeps no session. Another
  `RESOURCE_EXHAUSTED` or 429 at the end of its turn is a short limit,
  retried in the pane. Only agy's last paragraph after its last prompt is
  read.
