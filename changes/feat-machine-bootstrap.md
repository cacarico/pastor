### Added

- The head answers `BootstrapReport` from `pastor bootstrap`: it saves the
  machine's bootstrap state (ok, pending or failed, its mode, isolation,
  host key and the herdr and pastor versions it read) and emits
  `machine.bootstrapped` or `machine.bootstrap_failed`. Dispatch skips a
  machine whose bootstrap is pending or failed, and such a pull machine
  claims nothing; a report that makes it ready starts its queued tasks at
  once. Only `pastor bootstrap` reports: an agent's pane is refused
  (`agent_refused`). `machine describe` shows a `bootstrap:` line, naming
  the shepherd's user when the report gave it (and an `isolation:` line in
  user mode with none),
  and `machine list` falls back to the saved state for a pull machine's
  versions. Gated at IPC protocol 32.
- `pastor machine add NAME LOGIN --bootstrap [--sudo] [--service-user U]
  [--host-key K]` and `pastor machine bootstrap NAME`: the head pins the
  machine's host key, copies pastor and herdr from its cache over its own
  ssh, runs `pastor bootstrap` there and records the outcome, adding the
  agent's `authorized_keys` line and, in root mode, moving the entry to
  `pastor-agent@<host>`. A rerun after a failed step carries on instead of
  failing `machine_exists`; two at once for one machine get `machine_busy`.
  The head makes its own key (`head_key` in the state dir) on first use.
  Gated at IPC protocol 32 (`MachineBootstrap`).
