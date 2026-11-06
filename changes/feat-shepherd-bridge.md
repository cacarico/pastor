### Added

- `pastor bridge --shepherd --machine <name> --gen <n>`, the locked way in
  for a pull machine's own key. Before each request it checks with the head
  that the machine is still at enrollment `n` (`machine_revoked` when not).
  It passes on only the shepherd's claims, task and bootstrap reports,
  trust fetches, job submits and events, for that machine alone, and
  refuses anything else with `not_allowed_for_shepherd`.
- `pastor machine authorized-key <name> --key <file> --shepherd` prints or
  writes (`--write`) that line for a pull machine.
- A pull machine's claim carries the herdr and pastor versions it runs, and
  the head keeps them.
