### Added

- A flock's number on a machine can be 0 (`machines = { desk = 0 }`,
  `{ share = 0, max = 0 }` or `pastor flock join <flock> <machine> --max 0`)
  to drain it: the flock's tasks running there finish and are closed as
  usual, no new one starts there (`--now` included), a task pinned there
  waits with `flock <name> has 0 slots on <machine>`, and `pastor flock
  list` shows `desk 1/0`. It used to fail the whole file. Negative and
  fractional numbers are still refused.
