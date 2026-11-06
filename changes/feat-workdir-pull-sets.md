### Added

- Pull machines take tasks with workdir sets. A pull machine's serve says in
  its claim that it places sets, and the head hands each task it claims with
  its own set settled, the set's profile rendered for that machine, and the
  set each stored workdir bundle gives the agent's kind. The serve places
  them before the agent starts and reports them, so `workdir list` shows
  them. A later start in the same claim places the copies that claim
  brought. A set the head cannot build at the claim fails the task with
  `workdir_failed`. A claim carries at most 8 MiB of sets. A task with no
  worktree gets none built, its stored bundles marked skipped (shared
  folder), and the bundles a pull machine's own start leaves out come back
  in its report, so `task describe` and `workdir.skipped` on the head match
  what was placed.
- IPC protocol 52.

### Changed

- A task with a workdir set pinned to a pull machine is queued instead of
  refused; it waits until that machine's pastor places sets, and `pastor
  queue` says so. Command machines still refuse it.
- `task attach` of a pull machine's closed task leaves its stored workdir
  bundles out, and says so, instead of failing `workdir_failed`.
