### Changed

- A profiled opencode task on a machine whose own global opencode config or
  `~/.opencode` has permission rules no longer fails the task
  (`opencode_permissions_conflict`): pastor reads and parses those files over
  ssh and denies every permission key it finds there in `OPENCODE_PERMISSION`
  too, under every profile including `unrestricted`, so a denied key can
  never be lifted. The refusal remains for the machine's managed opencode
  config and, on macOS, its managed preferences (merged after
  `OPENCODE_CONFIG_CONTENT`, so pastor cannot neutralise them), and for a
  file pastor found but could not parse.
- `skills/pastor/SKILL.md` and the manual say an opencode task's permission
  rule for external directories should cover both a common parent of its
  checkouts (such as `~/ghq`) and any other reference directory it opens,
  such as a notes vault, so opencode's "Access external directory" dialog
  never blocks a task reading both.
