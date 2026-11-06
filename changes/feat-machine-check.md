### Added

- `pastor machine check <machine>` runs a provisioner's checks on a machine
  and says what it is missing for the agents that can land on it, one row
  per check, and once per agent for a check of an agent kind. It changes
  nothing there, exits 1 when a check failed, and takes `--provisioner` and
  `--json`. A machine with no `provisioner` gets the first one whose
  platform and `detect` fit it; the built-in `raspberry-pi-os` fits only a
  machine whose device tree names a Raspberry Pi, not any Debian machine.
  With a head running, each row that failed or could not run is a
  `machine.check_failed` event.
