### Added

- `pastor machine check` has a `profile` row for each agent that runs under
  a permission profile on the machine: which profile, where it comes from,
  and whether it renders for that agent's harness, failing with the refusal
  a task there would get (`profile_args_conflict`, `profile_not_allowed`,
  `agy_profile_mismatch`).
- `pastor machine check` has an `opencode-config` row on a machine an
  opencode agent can land on. It reads opencode's config there as a task
  start does, and fails naming each file and key that would refuse a task
  under a profile (`opencode_permissions_conflict`).
