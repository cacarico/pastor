### Added

- Stored workdir bundles: a bundle with `target = "workdir"` in its
  manifest, named by a flock's `workdir_bundles` in flock.toml or a job's
  `[dispatch] workdir_bundles`. Before each start of such a task, the head
  places the set the newest version gives the task's kind of agent in its
  worktree, before the task's own set, with the flock's `bundle_vars`. In a
  folder tasks share (no worktree), on a pull or command machine, and for
  codex and opencode agents, the task starts without them and
  `workdir.skipped` is raised.
- `pastor workdir place <machine> <dir> --bundle ID --kind KIND`.
- `bundle list` and `bundle show` say each bundle's target.
- IPC protocol 51.
