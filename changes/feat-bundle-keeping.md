### Added

- Pastor keeps the bundles of its machines. `pastor machine check` has a
  `bundles` row for each harness home: `ok`, `stale` (a newer bundle version
  or changed vars), `drift` (something pastor wrote was changed by hand),
  `not adopted` (a symlink pastor does not write through) or `conflict`.
  `pastor machine describe --json` has a `config` section: the bundles and
  versions a machine is meant to have, its apply mode, the last apply and each
  home. The head sends `bundle.applied`, `bundle.failed`, `bundle.drift` and
  `bundle.conflict` events.
- `bundle_apply = "auto"` applies a machine's bundles on the head's next pass
  after a change, and before a task starts on a stale machine; `manual` (the
  default) holds and the row says `stale`. A failed apply stops the task in
  both modes (`bundle_stale`).
  Bundles taken off a machine whose removal failed stay in view: a failing
  `bundles` row, `bundle.failed`, and `auto` tries the removal again.
