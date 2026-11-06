### Added

- `pastor machine remove` also drops the machine's marked lines from the
  head's `authorized_keys`, its host from pastor's own `known_hosts` and its
  bootstrap state from the store; `--dry-run` lists what would go.
- `pastor leave` removes what pastor put on a host for itself: its marked
  `authorized_keys` lines and its own key and `known_hosts` files in the state
  directory, and nothing else. `--dry-run` lists them.
- The ssh options for pastor's own `known_hosts` turn hashing off, so its
  entries stay readable.
