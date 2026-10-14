### Added

- Permission profiles apply to agy agents. A task under a profile starts
  agy with `--dangerously-skip-permissions --mode accept-edits`, so it never
  stops at a prompt, and before launch pastor reads agy's settings on the
  machine and checks they deny what the profile denies, failing the task
  with the rules to add otherwise (`agy_permissions_missing`). A deny
  pattern agy has no rule for is refused when queued
  (`agy_deny_unsupported`). agy's rules are per machine, so a profiled agy
  task runs only on a machine whose own profile denies everything the
  task's does (`agy_profile_mismatch`); unpinned, it skips the others.
  `make smoke-profiles` takes `AGY=<machine>`.

### Changed

- `own_permissions = true` on an agy agent is no longer needed for a
  profile; it still means pastor adds and checks nothing.
