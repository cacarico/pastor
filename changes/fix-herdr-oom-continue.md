### Fixed

- `herdr.service` sets `OOMPolicy=continue`, so the kernel killing an agent's
  own build for memory no longer stops herdr and fails every other agent's
  task on the machine along with it.
