### Added

- The head answers `BootstrapReport` from `pastor bootstrap`: it saves the
  machine's bootstrap state (ok, pending or failed, its mode, isolation,
  host key and the herdr and pastor versions it read) and emits
  `machine.bootstrapped` or `machine.bootstrap_failed`. Dispatch skips a
  machine whose bootstrap is pending or failed; `machine describe` shows
  a `bootstrap:` line (and an `isolation:` line in user mode with none),
  and `machine list` falls back to the saved state for a pull machine's
  versions. Gated at IPC protocol 32.
