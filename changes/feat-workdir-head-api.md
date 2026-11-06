### Added

- `pastor workdir place <machine> <dir>`, `remove <machine> <dir>`,
  `check <machine> <dir>` and `list [<machine>]` go through the head, which
  places a workdir set on a push machine (ssh or local). The head checks the
  set, renders its profile for that machine and sends both the way it
  applies bundles, then answers the rows. It records each real place in
  `workdirs.json` and raises `workdir.placed`, `workdir.failed` and
  `workdir.removed`. `check` places the recorded set again as a dry run and
  is ok only when nothing would change. An agent pastor started may only
  check, list and dry-run. IPC protocol 47 (`WorkdirPlace`, `WorkdirRemove`,
  `WorkdirCheck`, `WorkdirList`).
