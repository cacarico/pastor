### Added

- `pastor join create|list|revoke` on the head: one-time join tokens
  (`pjt_...`) a new pull machine adds itself to the fleet with. Each token
  is its own ed25519 key in the head's `authorized_keys`, locked to
  `pastor bridge --join <id>` and removed once used, revoked or expired.
  The flock, tags and `--replace` target are fixed at creation; a join
  emits `machine.joined`, an expired token `machine.join_token_expired`.
