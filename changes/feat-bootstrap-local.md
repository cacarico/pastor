### Added

- `pastor bootstrap` sets up the machine it runs on for a flock, as the login
  user and without root: it checks what the machine must already have
  (`git`, `ssh-keygen`, `curl`, `tar`, CA certificates, and on Linux
  lingering and a user bus) and names what its owner adds, copies pastor to
  `~/.local/bin`, installs herdr at its pin, makes the machine and agent keys
  once, adds the head's key (`--head-key`) and client.toml (`--head`), sets
  up herdr's user service, installs the pinned harnesses and runs `machine
  check`. `--json` prints the bootstrap report; a failed step exits with
  `bootstrap_missing`, `bootstrap_unsupported_platform` or `bootstrap_failed`.

### Fixed

- The machine's hostname falls back to `gethostname(3)` when neither
  hostname file nor `HOSTNAME` names it, as on macOS, instead of `-`.
