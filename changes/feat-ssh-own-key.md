### Added

- The ssh layer can use a key and a `known_hosts` file of pastor's own: a key
  offered alone or beside the user's, and a file that is the only source of
  host keys, with `accept-new` for a first connection. The state directory
  names the files (`head_key`, `machine_key`, `agent_key` and `known_hosts`).
  Nothing makes or sets them yet, so every ssh command line is as it was.
