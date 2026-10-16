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
  machine whose bootstrap is pending or failed, and such a pull machine
  claims nothing; a report that makes it ready starts its queued tasks at
  once. Only `pastor bootstrap` reports: an agent's pane is refused
  (`agent_refused`). `machine describe` shows a `bootstrap:` line, naming
  the shepherd's user when the report gave it (and an `isolation:` line in
  user mode with none),
  and `machine list` falls back to the saved state for a pull machine's
  versions. Gated at IPC protocol 32.
- `pastor join` on a new machine, with the token on stdin
  (`--token-stdin`), in `PASTOR_JOIN_TOKEN` or as its argument, plus
  `--name`, `--sudo` and `--service-user`: it pins the head's host key,
  makes the users and keys, joins with the token's key (written 0600 and
  deleted whatever happens), names the shepherd, bootstraps with the herdr
  the head names fetched over the machine key, installs and starts the
  shepherd's unit, and leaves the outcome in `<state>/bootstrap.json`. The
  shepherd reports it after its first claim and claims again, so the
  machine's tasks start in that pass; a report the head refuses is kept
  as `bootstrap.json.refused` and not sent again. `install.sh join …`
  installs pastor, then runs `pastor join` with the rest of its arguments;
  given no token it asks for one on the terminal with echo off and passes
  it on a pipe, never in `ps`. As root it first installs what is missing
  of openssh-client, curl, tar and ca-certificates. `pastor join` takes
  `PASTOR_JOIN_TOKEN` out of its environment before it runs anything,
  pins only a plain host key line for the token's head, removes its key
  dir on SIGINT, SIGTERM and SIGHUP (and one a killed join left), and caps
  the herdr download at the size the head answered and 512 MiB.

### Changed

- A head's `Joined` reply names the herdr version for the machine to
  fetch. A failure on the head after flock.toml is saved says the machine
  is in and the token spent, and what to run; a join that replaces a
  machine reached over ssh forgets its host key and ends its ssh master.
- The ssh to a remote head uses pastor's `known_hosts` only beside a
  pastor machine or agent key, so a person's CLI keeps their own.
