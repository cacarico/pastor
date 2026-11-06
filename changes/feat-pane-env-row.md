### Added

- `pastor machine check` has a `pane-env` row for each agent that can land
  on the machine, when its herdr answers. It opens a short-lived herdr
  workspace with the env a task of that agent gets, has the pane's own
  login shell write each of those variables and `PATH` to a file (only
  set or unset for a name that looks secret), and warns naming each one the
  shell's profile changed or dropped. The pane is closed on every path,
  after 20 seconds at most.
