### Changed

- A profiled opencode task on a machine whose own global opencode config or
  `~/.opencode` has permission rules no longer fails the task
  (`opencode_permissions_conflict`): pastor reads and parses those files over
  ssh and denies every top-level permission key it finds there (in
  `permission`, one action for everything being the `"*"` key, or in the
  legacy `tools` block, whose `write` and `patch` are `edit`) in
  `OPENCODE_PERMISSION` too, under every profile including `unrestricted`,
  so a denied key can never be lifted. opencode keeps a key the machine set
  in the machine's place, ahead of pastor's, and takes the last matching
  rule, so under `unrestricted` on such a machine pastor sends no
  `"*": "allow"` of its own (it would land after the machine's keys and lift
  their denies): opencode's default allow stands for tools the lists do not
  name, and what that default asks about is allowed by name. The refusal
  remains for what that object cannot neutralise: rules under an agent or a
  mode of the machine's config, which opencode puts after pastor's; the
  machine's managed opencode config and, on macOS, its managed preferences
  (merged after `OPENCODE_CONFIG_CONTENT`); a file pastor found but could
  not parse; and more config than pastor reads (48 KiB in all), which used
  to read as the machine lost.
- `skills/pastor/SKILL.md` and the manual say an opencode task's permission
  rule for external directories should cover both a common parent of its
  checkouts (such as `~/ghq`) and any other reference directory it opens,
  such as a notes vault, so opencode's "Access external directory" dialog
  never blocks a task reading both.
