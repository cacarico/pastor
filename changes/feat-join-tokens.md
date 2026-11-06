### Added

- `pastor join create|list|revoke` on the head: a one-time token a pull
  machine joins the flock with, into a flock, with tags, or taking over one
  machine's name (`--replace`). The token holds its own ssh key, whose line
  in the head's `authorized_keys` runs only `pastor bridge --join <id>` and
  stops working at the token's expiry; the head keeps only its id. A join
  checks the name and the keys, spends the token, adds the pull machine,
  enrolls it at a new generation (which cuts off the shepherd of a machine
  it replaces), writes its shepherd and agent lines, and raises
  `machine.joined`. Expired tokens are swept each scheduler pass
  (`machine.join_token_expired`).
