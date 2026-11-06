### Changed

- A machine whose last `machine check` through the head found that its agents
  cannot reach the head (the `reach` row failed) takes no new task: dispatch
  passes it over as it does a machine under its room floor, and a queued task
  waits as `waiting for a machine: pi-3 cannot reach the head`. `task run
  --now` ignores it. A passing check frees the machine; one never checked is
  placed as before.
