### Fixed

- One slow machine no longer delays `task run`, pull claims and dispatch to
  every other machine: a dispatch pass decides its placements under the
  dispatch lock and sends the dispatches, pauses and resumes after letting
  it go, one machine's in order and the machines at once. A task on its way
  counts on its machine and is not placed again by a pass that runs
  meanwhile.
- A flock reload that drops or replaces several machines stops their actors
  together, waiting about 2s at most instead of 2s per machine.
- A request to a machine's actor that does not answer fails after twice
  `request_timeout` instead of waiting for as long as the actor is stuck.
