### Added

- `reserved = N` on a `[[machine]]` keeps `N` of its `max_agents` slots free
  for panes pastor did not start: dispatch places at most `max_agents -
  reserved` of its own tasks there. `task run --now` ignores it, more than
  `max_agents` fails the load, and `machine list --wide` and `machine
  describe` show it.
