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
- `pastor machine add NAME LOGIN --bootstrap [--sudo] [--service-user U]
  [--host-key K]` and `pastor machine bootstrap NAME`: the head pins the
  machine's host key, copies pastor and herdr from its cache over its own
  ssh, runs `pastor bootstrap` there and records the outcome, adding the
  agent's `authorized_keys` line and, in root mode, moving the entry to
  `pastor-agent@<host>`. A rerun after a failed step carries on instead of
  failing `machine_exists`; two at once for one machine get `machine_busy`.
  The head makes its own key (`head_key` in the state dir) on first use,
  and from then on offers it on its ssh to every machine (beside the
  user's keys), checking a pushed machine's host against the pinned key.
  The push's own ssh asks for the pinned ed25519 key only and times out on
  a machine that stops answering; a `machine remove` during a push gets
  `machine_busy`; a rerun of an ok machine keeps it ok until `pastor
  bootstrap` starts there. Gated at IPC protocol 32 (`MachineBootstrap`).
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
- `pastor machine bootstrap NAME --repin`: drops the host key pinned for
  the machine and pins the one it offers now, the only way a changed host
  key is trusted again (for a pull machine it drops the key on record, and
  its next report pins the one it sends). Gated at IPC protocol 32.

### Changed

- A head's `Joined` reply names the herdr version for the machine to
  fetch. A failure on the head after flock.toml is saved says the machine
  is in and the token spent, and what to run; a join that replaces a
  machine reached over ssh forgets its host key and ends its ssh master.
- The ssh to a remote head uses pastor's `known_hosts` only beside a
  pastor machine or agent key, so a person's CLI keeps their own.
- Bootstrap run as root (`curl … | sudo sh -s -- join`) with no
  `--service-user` makes, or reuses, a `pastor` service user: the
  shepherd never runs as root, and `--service-user root` is a
  `usage_error`.
- A head whose sshd is not on port 22 sets `head_address` as
  `ssh://user@host:port`; a join token then pins its key as `[host]:port`.

### Fixed

- A push pins the machine's host key under the name `ssh -G` resolves the
  login to (`[host]:port` off port 22, or its `HostKeyAlias`), so a machine
  reached through an ssh config alias is checked against it.
- Two pushes, or a push and a `machine remove`, no longer race on pastor's
  `known_hosts`: they take turns on a lock.
- A failed rerun keeps the host key on record until a new pin succeeds.
- A root-mode push moves the entry to `pastor-agent@<host>` last, and puts
  it back, with the agent line taken away, if the report after it fails.
- `machine add --bootstrap` of a machine that exists names the rerun to
  use, with `--ssh <login>` once the entry names `pastor-agent`.
- A shepherd keeps its bootstrap report for later when the head is too old
  for it (`head_too_old`), instead of putting it aside as refused.
- `pastor join` stopped by SIGINT, SIGTERM or SIGHUP kills and reaps its
  ssh instead of leaving it running.
- `machine remove` with no head running drops the machine's
  `authorized_keys` lines and bootstrap record too, so a removed pull
  machine's shepherd key stops logging in.
- Two artifacts of one version fetched at once no longer trip over each
  other's cleanup of the cache dir, and a cache dir made part way is
  removed.
- `pastor leave` also tears down the shepherd's service user when that is
  not the login, as after a root join, where it is `pastor`: its units,
  its machine and agent keys and root mode's `/etc/sudoers.d/pastor-<user>`
  drop-in go, before pastor-agent's. It finds the user by that drop-in,
  else a `pastor` user whose home has pastor or a pastor unit; one it
  cannot reach is a failed step (`leave_incomplete`), where before the
  shepherd kept running and leave reported success.

### Security

- Every ssh pastor runs sets `ForwardAgent=no`, `ForwardX11=no` and
  `ClearAllForwardings=yes`, so a `ForwardAgent` in the head user's ssh
  config never hands its agent to a pastor-agent pane.
- `pastor machine authorized-key` prints a `restrict` line marked
  `pastor:agent:<name>`, which `machine remove` takes away again.
- A bootstrap report naming another host key than the one on record is
  refused (`host_key_mismatch`) and changes nothing.
- A push refuses an agent key already in the head's `authorized_keys`
  under another line, as a join does.
- A join token's `authorized_keys` line carries `expiry-time`, so sshd
  refuses the key after `--ttl` even before the sweep removes it.
- Bootstrap reports lose zero-width and bidi control characters before the
  head keeps or prints them.
- A machine whose host key is pinned is not reached while the head cannot
  read that record, rather than reached without the pin.
