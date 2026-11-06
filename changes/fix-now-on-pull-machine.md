### Fixed

- `pastor task run --now --machine <pull machine>` was always refused with
  `machine_not_connected`. A pull machine the head has heard from now takes
  the task on its next claim, first and past its slots and its flock's
  number, and its serve starts it at once; the request answers once the
  claim has taken it, or closes the task with `now_not_started` if no claim
  comes within a minute. A pull machine never heard from, or counted lost,
  is still refused.
