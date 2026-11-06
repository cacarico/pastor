### Added

- `pastor-provisioner.toml` can mark a `[[check]]` with `agent = "<kind>"`
  (`claude`, `codex`, `opencode` or `agy`), for a check that runs once per
  agent of that kind that can land on the machine, with that agent's own
  env; an unknown kind is a parse error. `Manifest::checks_for_kind` lists
  them. `[platform]` can also carry `detect`, an optional argv that exits 0
  when the provisioner fits the machine, checked after `os` and `distro`
  match and validated like a check's `command`. Running either is later
  work; this only parses and validates them.
