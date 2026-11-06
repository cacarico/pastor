### Added

- `pastor bundle apply [<machine>] [--dry-run] [--json]` places the
  machine's bundles in the homes of its agents: skills and hook scripts as
  links into a store of bundle versions on the machine, `bin` files in
  `~/.local/bin`, and the instructions as a marked block in `CLAUDE.md` or
  `AGENTS.md` with the flock's vars filled in, the user's text outside it
  left alone. A ledger on the machine lists what pastor wrote, so it changes
  and takes out only that; a file is backed up before it changes and
  written by a rename only if it still holds what was read; nothing is
  written through a symlink. It runs over ssh or locally, with pastor on
  the machine; plugins are not placed yet.
