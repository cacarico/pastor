### Added

- `pastor join create|list|revoke` on the head: one-time join tokens
  (`pjt_...`) a new pull machine adds itself to the fleet with. Each token
  is its own ed25519 key in the head's `authorized_keys`, locked to
  `pastor bridge --join <id>` and removed once used, revoked or expired.
  The flock, tags and `--replace` target are fixed at creation; a join
  emits `machine.joined`, an expired token `machine.join_token_expired`.
  A join drops the token's line before it writes anything else, and
  refuses a name starting with `-`, a key of an unknown type or one
  already in `authorized_keys`.
- The head answers `BootstrapReport` from `pastor bootstrap`: it saves the
  machine's bootstrap state (ok, pending or failed, its mode, isolation,
  host key and the herdr and pastor versions it read) and emits
  `machine.bootstrapped` or `machine.bootstrap_failed`. Dispatch skips a
  machine whose bootstrap is pending or failed; `machine describe` shows
  a `bootstrap:` line (and an `isolation:` line in user mode with none),
  and `machine list` falls back to the saved state for a pull machine's
  versions. Gated at IPC protocol 32.
