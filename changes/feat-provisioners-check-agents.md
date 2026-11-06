### Added

- A built-in `debian` provisioner, for a Debian machine that is not a
  Raspberry Pi.
- The built-in provisioners check every agent kind's CLI: `claude`,
  `codex`, `opencode` and `agy` each get an install row and a login row in
  `pastor machine check`, once per agent of that kind that can land on the
  machine. A kind no agent there uses gets no row.
- The built-in provisioners run nothing as root. They have no sudo check,
  and their only fixes are for pastor and the agents' CLIs; a missing OS
  package such as git or gh is reported for whoever runs the machine to
  install.
