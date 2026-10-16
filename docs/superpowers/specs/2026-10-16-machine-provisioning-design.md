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
  reaches the head over ssh through `pastor bridge`, and
  `pastor machine authorized-key` prints the restricted `authorized_keys`
  line by hand.

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
| 5 | Root: two modes | With root: a `pastor` user, OS packages, system services, agents run as `pastor`. Without root: everything at user level, and pastor reports what is missing. |
| 6 | Account pools per harness | Accounts are their own objects. A task goes to the account with the most usage left. |
| 7 | Secrets in the head's own store | Envelope encryption (below). |
| 8 | Store protection: envelope encryption | A data key encrypts the store. A key provider wraps the data key: a 0600 key file by default; systemd-creds/TPM and macOS Keychain as options; Vault or KMS helpers later. |
| 9 | Machine identity: a key pair now, SSH certificates later | An ed25519 key per machine, made at join, registered on the head with a restricted command. Later, the head acts as an SSH CA. |
| 10 | Config bundles are files in pastor's config dir | Skills, AGENTS.md / CLAUDE.md, MCP servers and settings, declared once. |
| 11 | Config sync right after sign-in | Part of the design, not left for later. |
| 12 | agy: test whether its token file works when copied | The person runs that test; agy's sign-in method waits on it. |
| 13 | The first build is bootstrap | `pastor join` and `machine add --bootstrap`. |

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

What is Rust special-casing today (`config/mod.rs`, `limit.rs`,
`config/agy.rs`, `config/opencode.rs`) moves into the built-in manifests.
Writing codex's manifest also adds the usage-limit detection codex lacks
today.

**Accounts and pools.**
- `pastor account add claude/team-3 --from-stdin` adds an account.
- Pools are named, and a spec names a pool.
- The scheduler places each task on the account with the most usage left
  (decision 6). Limits are already tracked per account (`limits` table),
  which makes this possible.

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
pastor machine add gpu-3 --ssh me@gpu-3 --flock builders --tag gpu --bootstrap
pastor machine add pi-7  --ssh pi@pi-7  --bootstrap --sudo
```

Pull, on the head first, then on the machine (or in its cloud-init):

```sh
pastor join create --flock builders --tag gpu --ttl 1h          # prints a token
curl -fsSL https://…/install.sh | sh -s -- join head.example pjt_…
```

Either way, the person then sees, within a minute:

```
$ pastor machine list
gpu-3   builders  connected  herdr 0.9.5  pastor 0.31.0  bootstrap: ok (user mode)
```

When bootstrap cannot finish, `machine describe` names the step that
failed, why, and the fix. Examples: "git is missing; rerun with --sudo or
install it", or "lingering is off; services stop at logout".

### Steps, as one idempotent sequence

Push and pull run the same steps on the machine; only the start differs.
Every step is safe to run again, so a rerun after a fix carries on.

1. **Facts.** OS, arch and userland bits (the same rules as `install.sh`,
   including a 64-bit kernel over a 32-bit userland), init system, package
   manager, whether root or sudo works, and the home dir.
2. **Mode.** Root mode when bootstrap runs as root, or with `--sudo` and
   working sudo. Otherwise user mode.
3. **Packages.** git, plus what herdr needs. Root mode installs them with
   the package manager found (apt, dnf, pacman, apk, brew). User mode only
   checks for them and reports what is missing.
4. **User.** Root mode creates a `pastor` system user with a home. Every
   later step runs as that user. User mode uses the login user.
5. **pastor.** Its binary goes in the user's `~/.local/bin`.
   - Push copies it from the head's artifact cache (below).
   - Pull already has it from `install.sh`.
6. **herdr.** At the version pinned in `pastor.toml` (`[provision] herdr`,
   defaulting to the head's own herdr version), taken from the artifact
   cache.
7. **Services.**
   - Root mode: system units for herdr and pastor, run as `pastor`.
   - User mode: `pastor setup systemd --herdr` and `pastor setup systemd`
     (or launchd), with the existing linger check.
   - Pull machines run a shepherd (headless serve). Push machines need only
     herdr running; the head connects to it.
8. **Identity and registration** (below).
9. **Report.** The machine-level conditions: `Bootstrapped` (with the mode
   and any missing items), then `Connected` for push or `Polling` for pull,
   once the existing channel states say so.

### The artifact cache

- It lives on the head under the state dir, one entry per file and target:
  `artifacts/<name>/<version>/<target>/`, with the sha256 kept beside it.
- The first build caches pastor and herdr. The harness provisioners later
  reuse the same cache.
- pastor's own artifacts come from the release tarball and `SHA256SUMS`, as
  `install.sh` does. A head whose own build matches the target may copy
  itself.
- A push machine gets files over the ssh connection bootstrap already uses.
- A pull machine gets them through `pastor bridge` with a new request that
  streams one artifact by name, version and target. It may only fetch what
  its spec allows.
- Pull's own pastor comes from `install.sh`. It downloads from GitHub, or
  from `PASTOR_DOWNLOAD_URL` for an offline fleet. The head can serve that
  directory too.

### Identity and the join token

This uses only ssh, which is decision 9 together with the no-second-transport
anti-goal.

- **Machine key.** Each machine makes an ed25519 key pair at bootstrap,
  kept in pastor's state dir as the pastor user. The head trusts that public
  key only, through an `authorized_keys` line with a forced command, built
  like today's `authorized_key_line`. Removing the machine
  (`pastor machine remove`) deletes its line. That revokes one machine alone.
- **Push.** The head already reaches the machine over ssh. It reads the new
  public key back during bootstrap and installs the line itself. The head
  also records the machine's ssh host key, so a later connection that sees a
  different host key fails.
- **Pull: the token.** `pastor join create` makes a token with:
  - the head's ssh destination,
  - the head's ssh host key, so the first connection is pinned rather than
    trusting whatever answers,
  - a single-use secret with an expiry,
  - the flock and tags.

  The head stores only a hash of the secret.
- **Pull: the join key.** The head keeps one join key pair. Its
  `authorized_keys` line forces `pastor bridge --join`, which accepts exactly
  one request: `Join { secret, machine name, public key, facts }`. The
  private join key travels inside the token.
  - It is harmless without a live secret, and it can do nothing but ask to
    join.
  - On a valid secret, the head marks the secret used, writes the machine's
    own `authorized_keys` line, adds the machine to `flock.toml` as
    `pull = true` with the token's flock and tags, and answers with the
    machine's name.
  - From then on the shepherd uses its own key.
- **Names.** Pull takes the machine's hostname unless `join` is given
  `--name`. A name already in `flock.toml` is refused, unless the token
  was made with `--replace NAME` (reinstalling a machine).

### Errors and events

- New stable error codes follow the existing JSON error style:
  - `join_token_invalid`, `join_token_used`, `join_token_expired`
  - `machine_name_taken`
  - `bootstrap_unsupported_platform`
  - `bootstrap_missing`, which lists what is missing
  - `artifact_checksum_mismatch`
- New events: `machine.joined`, `machine.bootstrapped` (with the mode and
  missing items) and `machine.bootstrap_failed` (with the step and why),
  for `watch`, hooks and orchestrators.

### Testing

- Unit tests for each step, with a fake runner, as `setup.rs` tests
  systemd and loginctl today.
- End-to-end tests in containers with sshd, beside the existing
  `tests/real_ssh.rs`:
  - Debian, Fedora, Arch and Alpine
  - push and pull
  - root mode and user mode
  - a rerun after a failed step
- The fake herdr covers what does not need a real herdr.
- Token tests: used twice, expired, a wrong head host key, a name clash.

## Left to the builder

- Every TOML key, flag, request name and the shape of the token.
- How the token is encoded, as long as it is one copy-pasteable word.
- Unit file contents for root mode.
- The order of package names per distro, and whether herdr needs any beyond
  git.
- Where the bootstrap steps live in `src/`, and whether `install.sh` gains a
  `join` subcommand or hands off to `pastor join`.
