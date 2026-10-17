### Added

- `pastor join create|list|revoke` on the head: one-time join tokens
  (`pjt_...`) a new pull machine adds itself to the fleet with. Each token
  is its own ed25519 key in the head's `authorized_keys`, locked to
  `pastor bridge --join <id>` and removed once used, revoked or expired.
  The flock, tags and `--replace` target are fixed at creation; a join
  emits `machine.joined`, an expired token `machine.join_token_expired`.
  A join drops the token's line before it writes anything else, and
  refuses, before the token is used, a name that is not letters, digits,
  `.`, `_` and `-` starting with a letter or digit
  (`machine_name_invalid`), a key of an unknown type or one already in
  `authorized_keys`.
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
  key is trusted again; the new key goes to `known_hosts` and the
  bootstrap record together, so a step failing after the pin leaves them
  agreeing. For a pull machine it drops the key on record, and the next
  join with a `--replace` token sets the one it brings: only a join sets a
  pull machine's host key, never its bootstrap report. Gated at IPC
  protocol 32.

### Changed

- A head's `Joined` reply names the herdr version for the machine to
  fetch. A failure on the head after flock.toml is saved says the machine
  is in and the token spent, and what to run; a join that replaces a
  machine reached over ssh forgets its host key and ends its ssh master.
- The ssh to a remote head uses pastor's `known_hosts` only beside a
  pastor machine or agent key, so a person's CLI keeps their own.
- Each join of a name is a new enrollment: its shepherd line carries
  `--gen <n>`, and `bridge --shepherd` asks the head before every request
  whether that is still the machine's (`ShepherdCheck`, IPC protocol 32).
  A shepherd connected before a `--replace` or a `machine remove` is
  refused `machine_revoked` at its next request and hung up on, though
  OpenSSH never rereads `authorized_keys` for a live connection.
- `EventsSince` takes a `machine`, cut by the head before its page limit,
  and `bridge --shepherd` sets it to its own machine, so a page of other
  machines' events no longer leaves a shepherd's cursor stuck.
- Bootstrap writes the keys and each user's `client.toml` before it starts
  the services, so a unit never starts in the wrong role.
- A root-mode `pastor join` gives the shepherd a way to the agent user's
  herdr: a `/etc/sudoers.d/pastor-<service user>` rule, checked with
  `visudo`, for that one command, and `[shepherd] command` running it.
- A push keeps the login it got through as (`bootstrap_login`, in the
  head's bootstrap record), and `pastor machine bootstrap NAME` connects as
  it, so rerunning or upgrading a root-mode machine needs no `--ssh`
  (alias `--login`).
- A push rerun against a host already pinned no longer runs
  `ssh-keyscan` unless given `--repin`: it checks the machine against
  the pinned key, so a changed key stops the rerun, naming `--repin`,
  instead of being trusted.
- A pull machine's herdr fetch goes to the head as a request line
  (`artifact_fetch`) instead of `bridge --fetch` arguments, which sshd
  never passes to a forced command; `SSH_ORIGINAL_COMMAND` may only be the
  line bridge.
- `pastor join` sends the bootstrap report to the head itself first, over
  the machine key, and keeps `bootstrap.json` for the shepherd to send only
  when the head did not take it. A join records the service user its
  request names. `--service-user` from a login that is neither root nor
  that user is refused (`usage_error`) before the token is used, since its
  machine key is unreadable there.
- Bootstrap run as root (`curl … | sudo sh -s -- join`) with no
  `--service-user` makes, or reuses, a `pastor` service user: the
  shepherd never runs as root, and `--service-user root` is a
  `usage_error`.
- A head whose sshd is not on port 22 sets `head_address` as
  `ssh://user@host:port`; a join token then pins its key as `[host]:port`.
- `pastor setup systemd` writes `pastor.service` without
  `NoNewPrivileges` when pastor.toml's `[shepherd] command` starts with
  `sudo`, as a root-mode pull machine's does: sudo's setuid bit is ignored
  under it, so that shepherd could never reach herdr. Every other unit
  keeps it.
- A join restarts the shepherd's unit, and a shepherd answered
  `machine_revoked` ends its ssh master to the head and backs off before
  reconnecting, exiting after five in a row for systemd to restart it, so
  a rejoined machine's shepherd takes its new line instead of staying
  revoked on the old master's session.
- `pastor machine bootstrap NAME` keeps the mode on record: a root-mode
  machine reruns in root mode without `--sudo`, and `--sudo` on a
  user-mode machine is a `usage_error` (remove and add it to change mode).
- `bridge --shepherd` refuses every line, `--gen` or not, for a machine
  no longer in flock.toml, and a gen-less line passes only for a pull
  machine no join has enrolled.
- A refused join token's message asks whether the head's sshd is older
  than OpenSSH 8.2, the head's minimum for joins (`expiry-time`).

### Fixed

- A push pins the machine's host key under the name `ssh -G` resolves the
  login to (`[host]:port` off port 22, or its `HostKeyAlias`), so a machine
  reached through an ssh config alias is checked against it.
- Two pushes, or a push and a `machine remove`, no longer race on pastor's
  `known_hosts`: they take turns on a lock.
- A failed rerun keeps the host key on record until a new pin succeeds.
- A root-mode `pastor join` checks its sudoers rule as `pastor-agent`
  (`sudo -n -l -U <svc> -u pastor-agent …`); checked as root it always
  failed, so every root-mode pull join stopped at `shepherd_herdr`.
- `pastor leave` from a non-root login that bootstrapped with `--sudo`
  removes the service user's `/etc/sudoers.d/pastor-<user>` through
  `sudo -n`, and reports `leave_incomplete` naming the file when sudo
  refuses, where before it left the file and reported success.
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
- A push machine's bootstrap report naming another host key than the one
  on record is refused (`host_key_mismatch`) and changes nothing.
- A push refuses an agent key already in the head's `authorized_keys`
  under another line, as a join does.
- A join token's `authorized_keys` line carries `expiry-time`, so sshd
  refuses the key after `--ttl` even before the sweep removes it.
- Bootstrap reports lose zero-width and bidi control characters before the
  head keeps or prints them.
- A machine whose host key is pinned is not reached while the head cannot
  read that record, rather than reached without the pin.
