### Added

- `pastor task sweep [--machine M] [--yes]` lists the worktrees pastor kept
  for closed or failed tasks on the head's machines, build directories
  included, and with `--yes` removes those with nothing to lose. One with
  uncommitted or untracked files, commits on no remote, a live task, an agent
  or a workspace in it, or a path that does not look like pastor's own is
  kept, with the reason on its line. Without `--yes` nothing is removed.
