### Fixed

- A dispatch pass decides under the dispatch lock and sends outside it, so
  one machine slow to start an agent, or wedged, no longer holds up
  `task run`, pull claims, a reload or dispatch to every other machine. A
  task placed and not yet claimed counts on its machine, so two passes
  still dispatch a task once and never past a machine's room.
- A reload stops the actors it removes or replaces together, waiting about
  2s for all of them instead of 2s each.
- A request to a machine's actor (dispatch, pause, resume, read, send,
  done, close) fails after three times `request_timeout` instead of
  waiting on a stuck actor forever.
