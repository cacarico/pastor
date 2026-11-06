### Fixed

- `pastor bootstrap`'s services step finds the herdr it has just put in
  `~/.local/bin` on a machine whose ssh commands have no such directory on
  their PATH, instead of failing with `herdr is not on PATH`.
