### Fixed

- `pastor.service` sets `OOMPolicy=continue`, so the kernel killing a child
  the head spawned (a merge orchestrator's `make check`, say) no longer stops
  the head and drops every request in flight.
