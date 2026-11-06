### Added

- `pastor machine check` ends with a `path` row: the directories the commands
  pastor runs on the machine see, and whether herdr and the CLI of each
  harness its agents run resolve on them. A CLI found nowhere is a `warn`.
- The commands pastor runs on an ssh machine (the herdr bridge and every
  probe, check, install and bundle command) now start by putting mise's shims
  directory (`$MISE_DATA_DIR/shims`, else `~/.local/share/mise/shims`) first on
  the PATH when it exists, so a herdr or harness mise provides is found by
  ssh's non-login shell. The machine's shell files are not edited.
