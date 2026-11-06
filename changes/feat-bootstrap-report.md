### Added

- The head keeps a machine's bootstrap state. A `BootstrapReport` (what a
  bootstrap run found: its steps, keys and versions) is stored as the
  machine's state, and the head raises `machine.bootstrapped` or
  `machine.bootstrap_failed`. Dispatch places no task on a machine whose
  state is pending or failed (a pull machine claims none), and a machine with
  no state is not held. `pastor machine describe` shows a `bootstrap` line
  and `--json` the state. The command that sends the report comes later.
