### Added

- `pastor workdir place --local <dir> --set FILE|DIR|-` places a set of
  files in an agent's working directory before it starts: skills, hook
  scripts, agent files, with modes and vars. Like a bundle in a home, it
  keeps a ledger of what it wrote, outside the directory, leaves what
  someone changed or what was there before, never writes through a
  symlink, refuses secrets, and takes out what a later, smaller set no
  longer has. `pastor workdir remove --local <dir> [--id ID]` takes a set
  out. Claude sets only, for now.
