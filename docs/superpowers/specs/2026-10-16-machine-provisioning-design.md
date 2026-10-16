# Machine provisioning: bare machine to running agents

Date: 2026-10-16. Branch `docs/machine-provisioning-design`. This is the
design for the whole feature and, in detail, for its first build: `pastor
join` and `pastor machine add --bootstrap`. Later builds get their own
specs and point back here.

## Why

Pastor runs coding agents on machines you own. It places tasks, watches
panes, answers trust prompts and startup dialogs, tracks usage limits and
falls back between models. But before pastor can use a machine, a person
does all of the following by hand on it:

- install herdr and start it as a service
- install pastor
- install each harness (claude, codex, opencode, agy, gemini)
- sign each harness in
- copy skills, AGENTS.md / CLAUDE.md, MCP servers and settings
- run `pastor machine add` and pick a flock

That costs machines × harnesses steps. It is tolerable for three machines
and does not work for three hundred.

Pastor is an open-source project, and this feature is what it is for: the
tool that provisions machines for AI agents. A bare OS plus one command
should give a machine that runs agents, with nothing done by hand and no
outside configuration tool. You declare what a machine should have, and
pastor makes it so and keeps it that way, the way Kubernetes treats a node.

What pastor has today, read from the code at `aa141fa`:

- It installs nothing on a machine, signs no harness in, and has no code
  that touches credentials.
- It does not notice a signed-out harness. A task starts and sits at the
  login screen until it goes stale.
- It writes no agent config except agy's permission lists
  (`src/provision/agy.rs`).
- `install.sh` already installs pastor itself without root: it puts a
  checksummed release binary in `~/.local/bin`, and `PASTOR_DOWNLOAD_URL`
  points it at a mirror.
- `pastor setup systemd|launchd [--herdr]` installs user services, on the
  machine where it runs.
- Pull machines (`pull = true`, `src/shepherd.rs`) exist. Their shepherd
  reaches the head over ssh through plain `pastor bridge`, with an unlocked
  key. Plain bridge passes any request as if from a person, and a
  `TaskClaim` names its own machine, so that key can act for any machine.
- `pastor machine authorized-key` prints a locked line, which is for
  agents. It forces `bridge --agent --machine X`, which allows only Ping,
  List, TaskShow, TaskRead and TaskDone (`bridge.rs:104`).
- Pastor's ssh passes no `-i` and no known-hosts file of its own. It runs
  with `BatchMode=yes` and uses the user's own keys and `~/.ssh/known_hosts`
  (`ssh.rs`).

## Goals

- **One command from a bare machine.** Either form leaves the machine with
  pastor and herdr running as services, in the right flock, reachable by
  the head, and showing up in `pastor machine list`:
  - pull: on the machine, `curl … | sh -s -- join <head> <token>`
  - push: on the head, `pastor machine add NAME --ssh DEST --bootstrap`
- **Behind NAT, from cloud-init, or from an image.** Pull needs no inbound
  connection to the machine.
- **No outside tool.** No Ansible, no config manager. Pastor does all of it.
- **Hundreds of machines.** After the one interactive sign-in per account,
  adding a machine needs no person.
- **Works without root, better with it.** With root, agents run as a
  dedicated user.

## Non-goals

- Managing the OS beyond what agents need: users, firewall, networking,
  updates, dotfiles.
- Creating machines (cloud VMs, flashing SD cards). Pastor starts once a box
  boots and has ssh or outbound network. Cloud-init and images carry the
  join line; pastor does not drive the cloud APIs.
- Windows.
- A web UI.

## Anti-goals

These would make it a failure even if it worked:

- Tying the design to one person's tools. Secret backends and installers
  sit behind interfaces; nothing in the core names a specific vault or
  config tool.
- A second transport next to ssh. Pastor is ssh-only today, and stays so.
- Agents running as root.
- Secrets written to a machine's disk when the harness can take them
  another way.
- A join token that works twice, or a machine key that cannot be revoked on
  its own.

## Decisions

The person made these on 2026-10-16. Each is binding on every build.

| # | Decision | What it means |
|---|---|---|
| 1 | Pastor provisions everything | No outside config tool. A bare OS plus pastor gives a working machine. |
| 2 | Pull and push | `pastor join` with a one-time token, and `machine add --bootstrap` over ssh. |
| 3 | Harnesses are defined by manifests | claude, codex, opencode, agy and gemini ship as manifests in the binary; anyone adds more without Rust. |
| 4 | Install: the manifest lists sources, the head caches | Sources in order: a release URL with sha256, npm, the vendor's installer. The head downloads each file once, checks it, and serves it to its machines. Self-update is turned off where the harness allows it. |
| 5 | Root: two modes | With root or `--sudo`: OS packages, lingering and the users below. Without root: everything at user level, and pastor reports what is missing. |
| 6 | Account pools per harness | Accounts are their own objects. A task goes to the account with the most usage left. |
| 7 | Secrets in the head's own store | Envelope encryption (below). |
| 8 | Store protection: envelope encryption | A data key encrypts the store. A key provider wraps the data key: a 0600 key file by default; systemd-creds/TPM and macOS Keychain as options; Vault or KMS helpers later. |
| 9 | Machine identity: a key pair now, SSH certificates later | An ed25519 key per machine, made at join, registered on the head with a restricted command. Later, the head acts as an SSH CA. |
| 10 | Config bundles are files in pastor's config dir | Skills, AGENTS.md / CLAUDE.md, MCP servers and settings, declared once. |
| 11 | Config sync right after sign-in | Part of the design, not left for later. |
| 12 | agy signs in by credentials file import | Tested on 2026-10-16: a copy of `~/.gemini/antigravity-cli/antigravity-oauth-token` (mode 0600) signs agy in on another machine with no login step, and both machines keep working. The check is `agy -p "reply with OK"`. The file is not tied to the machine, and agy keeps no secret in the OS keyring. agy refreshes the file on its own, so pastor copies the newest version. |
| 13 | The first build is bootstrap | `pastor join` and `machine add --bootstrap`. |
| 14 | Agents run as their own user, from the first build | A `pastor-agent` user runs herdr and every agent. It cannot read the shepherd's machine key. |
| 15 | The shepherd runs as the login user, or as a `pastor` user on request | The default keeps the login user. An option creates or reuses a `pastor` user for the shepherd and its key. |
| 16 | First-build platforms: Linux with systemd, and macOS in user mode | Alpine/OpenRC, macOS root mode, armv7 and riscv64 are reported as unsupported. herdr ships no armv7 or riscv64 build. |

## The whole design

Each part below is a later build, except the first, which this document
specifies in detail.

**Machine spec.** A named desired state in `pastor.toml`: which harnesses
at which versions, the account pool, the config bundle and the tags. A
flock points at a spec.

```toml
[specs.builder]
harnesses = { claude = "2.1", codex = "latest", opencode = "1.4" }
pool = "team"
bundle = "default"
```

**Conditions.** Each harness on each machine moves through `Installed` →
`SignedIn` → `ConfigSynced` → `Ready`.
- A task goes only to an agent that is `Ready` on that machine.
- `machine describe` shows each condition, why it is not met, and how the
  machine differs from its spec.
- A change of condition is an event, so `watch` and orchestrators see it.
- The first build adds only the machine-level conditions (below).

**Harness provisioners.** In AGENTS.md's words, a provisioner is "what
checks and sets up a machine for agents; it is installed and run like a
connector". Each harness gets one, as a manifest. It declares:
- install sources, in order (decision 4), and how to read the version
- sign-in methods, best first:
  1. a token in the pane's env
  2. a secret on stdin
  3. a credentials file import
  4. a device code
  5. a person at a popup
- the command that checks the harness is signed in
- where its config lives
- its trust markers and startup dialogs
- the pane text that means a usage limit
- how a permission profile becomes its flags

What is Rust special-casing today moves into the built-in manifests:
`config/mod.rs`, `limit.rs`, `config/agy.rs`, `config/opencode.rs`,
`provision/agy.rs`, the launch code in `dispatch.rs`, `reopen.rs`, and the
pane rules in `task.rs` (trailing question, background shell, agy's task
footer). Writing codex's manifest also adds the usage-limit detection codex
lacks today.

**Accounts and pools.**
- `pastor account add claude/team-3 --from-stdin` adds an account.
- Pools are named, and a spec names a pool.
- The scheduler places each task on the account with the most usage left
  (decision 6). Today's `limits` table only records accounts that hit a
  wall, with their retry time; nothing tracks how much an account has
  left. That build adds the measure. At first it is "not limited, and
  lowest recent spend", from what `usage.rs` already reads, and later a
  harness's own quota check where it has one.

**Secret store** (decisions 7 and 8).
- Secrets are encrypted at rest with a data key.
- A key provider wraps the data key, so changing the provider re-wraps one
  key instead of re-encrypting everything.
- Each machine gets only its flock's secrets, encrypted to that machine's
  own ed25519 key with age (age takes ssh-ed25519 recipients), so only that
  machine can open them.
- On the machine, a secret goes into the task's pane env where the harness
  takes it that way (for example Claude Code's `CLAUDE_CODE_OAUTH_TOKEN`).
  Otherwise it goes into a 0600 file in a folder pastor owns.
- Every handout is an event. Rotating a secret pushes it out again.
- SECURITY.md states the limit: this protects a leaked backup or disk, not
  root on the head.

**`pastor login`.** For harnesses with no headless sign-in:
- Pastor starts the sign-in in a pane on each machine that needs one.
- It reads the device code or URL from the pane, the way it already reads
  trust prompts, and lists every pending code on one screen.
- Failing that, herdr popups open one machine after another.
- Each minted credential goes into the store, so the next machine needs no
  person.

**Config bundles** (decisions 10 and 11).
- Bundles are files under `~/.config/pastor/bundles/<name>/`.
- Each harness's manifest maps a bundle's pieces to that harness's paths.
- Pastor merges into existing files instead of overwriting them, as the agy
  provisioner does today. A bundle has a content hash, so drift shows.
- Sync runs right after `SignedIn` and gates `ConfigSynced`.

**SSH certificates** (decision 9, later). The head acts as an SSH CA and
issues short-lived certificates that renew themselves. Join keys and
machine keys stay; certificates replace `authorized_keys` lines.

**Build order after this one:** harness provisioner manifests with
`SignedIn`/`Ready` → accounts and pools → secret store → `pastor login` →
config bundles → SSH certificates.

## First build: `pastor join` and `machine add --bootstrap`

### What a person sees

Push, from the head:

```sh
pastor machine add gpu-3 --ssh me@gpu-3 --flock builders --tag gpu --bootstrap --sudo
pastor machine add pi-7  --ssh pi@pi-7  --bootstrap --sudo --service-user pastor
pastor machine add lab-1 --ssh me@lab-1 --bootstrap          # no root: user mode
```

Pull, on the head first, then on the machine (or in its cloud-init):

```sh
pastor join create --flock builders --tag gpu --ttl 1h          # prints a token
curl -fsSL https://…/install.sh | sudo sh -s -- join --token-stdin <<<"pjt_…"
```

The token can also come from `PASTOR_JOIN_TOKEN`. The positional
`join <token>` form works too, but it shows the token in `ps`, so the docs
lead with stdin.

Within a minute the machine shows up on the head. Its versions are
reported by the machine itself, pull machines included:

```
$ pastor machine list
gpu-3  …  connected  herdr 0.9.5  pastor 0.31.0  …
$ pastor machine describe gpu-3
bootstrap: ok, root mode (agents as pastor-agent, shepherd as me)
```

When bootstrap cannot finish, `machine describe` names the step that
failed, why, and the fix. For example: "git is missing; rerun with --sudo
or install it". Or: "lingering is off, so services stop when you log out;
rerun with --sudo, or run `loginctl enable-linger me` as root".

### Users on the machine

Decisions 14 and 15 give a machine up to three users:

| Role | Who | Holds | Runs |
|---|---|---|---|
| Login user | the ssh user (`me`), or root for `sudo sh` | nothing pastor needs | only bootstrap |
| Service user | the login user by default; `pastor` with `--service-user pastor` (created if missing, reused if there) | the **machine key** (`bridge --shepherd`) | the shepherd (pull machines) |
| Agent user | `pastor-agent`, always created in root mode | the **agent key** (`bridge --agent`, as today) | herdr, and every agent pane |

herdr runs as the agent user, because a pane starts as whoever runs herdr.
The agent user's home holds the repos, worktrees and harness config. It
cannot read the service user's state dir (0700), so a hijacked agent never
holds the machine key. All it holds is today's agent key, which can only
read and end its own machine's tasks.

How the head reaches herdr: the flock entry records the agent user,
`ssh = "pastor-agent@gpu-3"`. Bootstrap puts the head's public key in that
user's `authorized_keys`. The head keeps the shell access it has today, as
that user. The login user is used only for bootstrap and for later reruns
of it.

**User mode** (no root, no sudo) cannot create users. Everything runs as
the login user and the machine key is readable by agents. Bootstrap still
works, and it says so plainly: `isolation: none` in `machine describe`, and
a `machine.bootstrapped` event carrying the warning. This is the documented
cost of no root; SECURITY.md says so. (My call: report it rather than
refuse, so a box without root can still be used.)

**Root needed once.** `--sudo` needs passwordless sudo over ssh
(`sudo -n true`, checked first, failing fast). Root is used only for:
- OS packages
- creating the users
- `loginctl enable-linger` for the users that run services
- placing their `authorized_keys`

Services stay `systemd --user` units of their users, with lingering on. So
no system units are needed, and `pastor setup systemd` is reused. (My
call: simpler than system units, and the same code runs in both modes.)

### One local command, used by both

`pastor bootstrap` runs the steps on the machine itself and prints one JSON
result. That result holds the facts, the mode, each step's outcome, both
public keys and the host key, or the step that failed and why.
- Push copies pastor to the machine and runs `pastor bootstrap` over ssh.
- Pull runs it from `pastor join`.

Each step is written and tested once, in Rust, not once as Rust and again
as shell.

### Steps

Every step checks first and does nothing when the right thing is already
there, so a rerun after a fix carries on where it stopped.

1. **Facts.** OS, arch and userland bits, using `install.sh`'s rules
   (including a 64-bit kernel over a 32-bit userland). Also the init system,
   package manager, root or `sudo -n`, homes, and the hostname via
   `gethostname`. Today's `config::hostname()` returns `-` on macOS, so it
   gains that fallback.
2. **Platform check** (decision 16). Linux with systemd: root mode and user
   mode. macOS: user mode, with launchd. Anything else, or an arch herdr does
   not ship, stops with `bootstrap_unsupported_platform` naming what is
   missing.
3. **Packages.** git, plus `openssh-client`/`ssh-keygen`, `curl`, `tar` and
   `ca-certificates` where missing. Root mode installs them with apt, dnf or
   pacman, and macOS checks only (brew refuses root). User mode checks only
   and reports what is missing.
4. **Users** (root mode): create the service user if asked, and
   `pastor-agent`, each with a home, and turn lingering on for both.
5. **pastor** into each user's `~/.local/bin`: the service user's, and the
   agent user's (agents call `pastor task done`).
6. **herdr** into the agent user's `~/.local/bin`:
   - The version is `[provision] herdr` in the head's `pastor.toml`.
   - Without it, the version most of the head's machines report.
   - Without any, a version compiled into pastor (`DEFAULT_HERDR_VERSION`).
7. **Services** (`pastor setup systemd|launchd`, run with `--yes` since
   nothing is a terminal):
   - herdr as the agent user;
   - on pull machines, the shepherd as the service user.

   `setup` learns to find herdr in `~/.local/bin` next to its own binary,
   not only on `PATH`. With lingering off and no way to turn it on, the step
   fails rather than reporting a machine that dies at logout. It also fails
   when there is no user bus (`XDG_RUNTIME_DIR` unset); the fix is to rerun
   with `--sudo`.
8. **Keys.** The service user makes the machine key, and the agent user
   makes the agent key. Each is ed25519, 0600, in its user's state dir.
   Each user gets a `client.toml` naming the head, so `PASTOR_HEAD` no
   longer depends on `head_address`. Pastor's ssh to the head then passes
   `-i <key> -o IdentitiesOnly=yes -o UserKnownHostsFile=<state>/known_hosts`
   whenever that key exists.
9. **Shepherd name** (pull): `pastor join` writes `[shepherd] machine =
   "<name>"` to the service user's `pastor.toml`, so the name survives
   restarts.

### Order and state, push and pull

**Push.**
1. `machine add --bootstrap` first writes the flock entry, with
   `bootstrap = "pending"` kept in the head's store, not in `flock.toml`.
   A pending machine takes no tasks.
2. It runs `pastor bootstrap` over ssh, as the login user.
3. It reads the result, installs the agent key's line and the machine's
   host key on the head, and rewrites the entry's `ssh` to the agent user.
4. It sets the state to `ok`, or to `failed` with the step and why.

`machine add` again on a pending or failed machine with the same target
reruns bootstrap instead of failing with `machine_exists`.
`pastor machine bootstrap NAME [--sudo]` does the same by name.

**Pull.**
1. `pastor join` joins **first**, then bootstraps. Join registers the
   machine as `pull = true` with `bootstrap = "pending"`, using the service
   user's new key.
2. The bootstrap that follows fetches herdr from the head's cache over its
   own `bridge --shepherd`.
3. It reports each step back. So a failed pull bootstrap shows on the head
   like a failed push one, and `machine.bootstrap_failed` fires either way.

**Where the state lives.** A new `machine_state` table in the head's store
(schema bump) holds:
- the bootstrap state, mode, isolation and the failed step;
- the host key;
- the reported herdr and pastor versions.

`flock.toml` keeps `deny_unknown_fields` and gains nothing.

**Pull versions.** `TaskClaim` (and the new bootstrap report) carry the
machine's herdr and pastor versions, and `Fleet::heard` stores them. Today
pull machines show `-` there. Both push and pull end `Connected`. A pull
machine is `Connected` once the head hears from it (`Fleet::heard`).
`Polling` stays what it is today: a push machine whose event stream will
not open.

### The join token

- `pastor join create` makes a **per-token ed25519 key** with `ssh-keygen`.
  It writes the public half to the head's `authorized_keys` with
  `restrict,command="pastor bridge --join <id>"`, and stores the expiry,
  flock, tags and `--replace` target. The head keeps no copy of the private
  half.
- The token is `pjt_` + base64url of (head ssh destination, head host key,
  the private key): one word, a few hundred characters. Using `ssh-keygen`
  instead of deriving the key from a seed keeps crypto crates out of pastor.
- The machine connects with that key. sshd itself checks the token.
- `bridge --join --id` accepts exactly one request:
  `Join { name, machine key, agent key, facts }`.
- On success, or at expiry (swept on each tick), the head deletes that
  token's line. A used or expired token then gets nothing, not even an ssh
  session.
- There is no shared join key.
- `--replace NAME`, the flock and the tags are fixed when the head makes the
  token. The joining machine cannot choose them, so a stolen token cannot
  take over another machine's name.
- A name already in `flock.toml` is refused, unless the token was made with
  `--replace` for that exact name.

### The head's `authorized_keys` and `known_hosts`

**`authorized_keys`.**
- Every pastor-written line carries a marker comment,
  `pastor:machine:<name>` or `pastor:join:<id>`.
- Edits take a lock, write a 0600 temp file, fsync it and rename it, so
  100 machines joining at once lose nothing.
- Pastor never touches a line without its marker.

**`known_hosts`.**
- Pastor keeps its own, at `<state>/known_hosts`, and passes
  `UserKnownHostsFile` on every ssh it runs. Recycled cloud IPs and the
  user's own file then never collide.
- Push bootstrap accepts a new host key once (`accept-new`), or checks it
  against `--host-key <fp>` when given.
- A pull machine's host key arrives with `Join`.
- `machine remove` and `--replace` delete the entry, and close the ssh
  ControlMaster for that machine (`ssh -O exit`), so a live mux socket does
  not skip the check.

### Bridge modes

| Mode | Who uses it | May send |
|---|---|---|
| `--join --id` | a joining machine, once | `Join` only |
| `--shepherd --machine X` (new) | the service user's machine key | Ping, the agent set, `TaskClaim`, `TaskReport`, `PullTrust`, `JobSubmit`, `EventsSince`, the bootstrap report, and artifact fetches |
| `--agent --machine X` (today) | the agent user's agent key | Ping, List, TaskShow, TaskRead, TaskDone |
| plain | a person's own key | everything, as today |

Rules for `--shepherd`:
- The bridge sets the `machine` field of every claim, report and trust
  request to `X` before forwarding. A machine can only act for itself.
- `EventsSince` is filtered to records about `X` and its tasks.
- `JobSubmit` is allowed, because pull machines run their own jobs today.

Existing pull machines keep working on plain bridge. `pastor machine
authorized-key --shepherd` prints the new locked line, for moving them over.

### The artifact cache

- It lives on the head under `<state>/artifacts/<name>/<version>/<target>`,
  with the sha256 beside each file.
- Names, versions and targets must match `[A-Za-z0-9._-]+`, with no `..`,
  so a request cannot leave the cache.
- **pastor**: the release tarball checked against its `SHA256SUMS`, as
  `install.sh` does. A head whose own build matches the target copies
  itself.
- **herdr**: `https://herdr.dev/latest.json` lists every release, with
  per-target assets and sha256s for `linux-x86_64`, `linux-aarch64`,
  `macos-x86_64` and `macos-aarch64`. It is a static binary, not a tarball.
  `[provision] herdr_index` overrides the index URL, for a mirror.
- **Moving files.** Binaries never go through the head's JSON socket, which
  caps a request at 1 MiB (`MAX_IPC_REQUEST`).
  - A pull machine's artifact request asks the daemon for the checked path
    and sha256 only. `pastor bridge` then streams the file itself over the
    ssh session.
  - A push bootstrap copies over its own ssh connection.
  - The machine checks the sha256 again before it installs anything.
- **Offline pull** in the first build: `install.sh` honours
  `PASTOR_DOWNLOAD_URL` (a mirror the person hosts, or `file://`). After the
  join, herdr comes from the head. The head serving `install.sh` itself is
  left for later.

### Removal and upgrades

**`pastor machine remove NAME`** also:
- deletes the machine's `authorized_keys` lines and its `known_hosts`
  entry;
- closes its ssh mux socket;
- drops its `machine_state` row.

A removed pull machine's shepherd then fails to log in. It backs off and
says why. `pastor leave` on the machine stops and removes its services, for
a clean teardown.

**Upgrades** in the first build: rerun `pastor machine bootstrap NAME`, and
herdr upgrades with `herdr update --handoff` semantics, so running panes
carry on. Upgrades driven by the machine spec and reconcile come with the
later builds.

### Errors and events

- New stable error codes:
  - `join_token_invalid`, `join_token_used`, `join_token_expired`
  - `machine_name_taken`
  - `bootstrap_unsupported_platform`
  - `bootstrap_missing`, which lists what is missing
  - `bootstrap_needs_sudo`
  - `artifact_checksum_mismatch`
  - `not_allowed_for_shepherd`
- New events:
  - `machine.joined`
  - `machine.bootstrapped`, with the mode, isolation and missing items
  - `machine.bootstrap_failed`, with the step and why
  - `machine.join_token_expired`

### Testing

- Unit tests for each step with a fake runner, as `setup.rs` tests systemd
  and loginctl.
- Bridge modes:
  - each request allowed or refused per mode;
  - a forged `machine` field gets overwritten.
- Tokens:
  - used twice, expired, wrong head host key;
  - a name clash, and `--replace`;
  - the line is removed after use and at expiry.
- `authorized_keys`:
  - concurrent joins lose no line;
  - pastor never edits a line without its marker.
- End-to-end in containers with sshd and systemd, next to `tests/real_ssh.rs`:
  - Debian, Fedora and Arch;
  - push and pull;
  - root mode and user mode;
  - a rerun after a failed step;
  - a hijacked-agent check: the agent user cannot read the machine key.
  - macOS user mode is tested by hand at first.

## Left to the builder

- Every TOML key, flag and request name not fixed above.
- Exactly how the token is encoded, as long as it is one copy-pasteable
  word.
- Package names per distro.
- Where the steps live in `src/`.
- Whether `install.sh` runs `pastor join` itself or prints the command.
