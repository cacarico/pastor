### Changed

- A profiled opencode task refused by `opencode_permissions_conflict`
  because the machine's managed opencode config (or, on macOS, its managed
  preferences) sets permission rules now names each file found and whether
  it had a `permission` key, a `tools` key, or could not be read at all
  (never read as clean). `CONFIG_CHECK_COMMAND` prints one `<file>\t<key>`
  line per hit instead of a plain yes or no, and `parse_config_check` turns
  that into the list dispatch reports.
