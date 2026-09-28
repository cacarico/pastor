### Added

- `pastor task close` takes several tasks: `pastor task close t-1 t-2 t-3`
  closes each in turn, `--remove-worktree` applying to all, and prints one
  line per task (with `--json` an array of objects). A task that fails does
  not stop the rest; the command then exits 1 with `close_failed`, naming
  the ones not closed. One task prints as before, and tab completion offers
  more tasks after the first.
