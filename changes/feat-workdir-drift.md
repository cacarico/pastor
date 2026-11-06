### Added

- The head keeps the workdir sets it placed on push machines. Every five
  minutes it reads each machine in one run and records each set's state:
  `ok`, `stale`, `drift`, `not adopted`, `conflict`, `gone` or `missing`.
  `workdir check` answers the state beside the rows, and `workdir list`
  shows it.
- A set whose profile renders otherwise is `stale` at once. On a machine
  whose `bundle_apply` is `auto`, the head places it again
  (`workdir.placed` with `auto: true`), unless the machine holds another
  version of it.
- `workdir.drift` when a set that was ok drifts. A directory still gone
  on a read at least five minutes after the one that found it gone loses
  its ledger and its record (`workdir.removed`,
  `reason: gone`).
- `machine check` has a `workdirs` row, such as `12 dirs: 11 ok, 1 drift`,
  and `machine describe` a `workdirs` section.
- A machine keeps at most 2,000 workdir ledgers before it drops those of
  gone directories, the oldest first; a live one is never dropped.
- IPC protocol 48.
