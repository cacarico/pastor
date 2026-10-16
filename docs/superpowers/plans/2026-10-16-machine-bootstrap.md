# Machine bootstrap (`pastor join`, `machine add --bootstrap`) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A bare Linux (systemd) or macOS machine becomes a working pastor machine with one command, pushed from the head or pulled with a one-time token.

**Architecture:**
- One local command, `pastor bootstrap`, runs every step on the machine and prints a JSON result.
- Push runs it over ssh from `machine add --bootstrap`. Pull runs it from `pastor join`, after joining with a token that carries a per-token ssh key.
- The head gains:
  - a `machine_state` table;
  - a locked `bridge --shepherd` mode and a `bridge --join` mode;
  - an `authorized_keys` editor and its own `known_hosts`;
  - an artifact cache for pastor and herdr, streamed outside the JSON socket.

**Tech Stack:**
- Rust 2024, tokio, rusqlite and clap, all existing.
- New dependency: `sha2`, for artifact checksums. Its reason goes in the commit body.
- `ssh-keygen` and `curl` are called as programs, as `install.sh` already does.

**Spec:** `docs/superpowers/specs/2026-10-16-machine-provisioning-design.md`, the "First build" section and decisions 14–16.

## Global Constraints

- Vocabulary is fixed (AGENTS.md): machine, flock, head, shepherd, pull machine, provisioner. Never "plugin" or "node".
- Runtime CLI errors are `{"code","message"}` on stderr with exit 1. Every new code goes in the table in `docs/manual.md` (`tests/error_codes.rs` enforces this).
- New error codes, exactly:
  - `join_token_invalid`, `join_token_used`, `join_token_expired`
  - `machine_name_taken`, `machine_name_invalid`
  - `bootstrap_unsupported_platform`, `bootstrap_missing`, `bootstrap_needs_sudo`
  - `artifact_checksum_mismatch`
  - `not_allowed_for_shepherd`, `machine_revoked`
- New events, exactly:
  - `machine.joined`
  - `machine.bootstrapped` (detail: `mode`, `isolation`, `missing`)
  - `machine.bootstrap_failed` (detail: `step`, `why`)
  - `machine.join_token_expired`
- Users: the agent user is named `pastor-agent`. The service user is the login user, or `pastor` with `--service-user pastor`.
- Platforms: Linux with systemd (root and user mode), and macOS in user mode only. Everything else is `bootstrap_unsupported_platform`.
- herdr targets: `linux-x86_64`, `linux-aarch64`, `macos-x86_64`, `macos-aarch64`.
- herdr index: `https://herdr.dev/latest.json`, overridable as `[provision] herdr_index`.
- `DEFAULT_HERDR_VERSION = "0.9.5"`.
- Token prefix: `pjt_`.
- Marker comments in the head's `authorized_keys`: `pastor:machine:<name>`, `pastor:agent:<name>`, `pastor:join:<id>`. Pastor never edits a line without its marker.
- Binaries never pass through the daemon socket (`MAX_IPC_REQUEST` is 1 MiB).
- Cache path parts match `^[A-Za-z0-9._-]+$` and are never `.` or `..`.
- `flock.toml` keeps `deny_unknown_fields` and gains no keys. Bootstrap state lives in the store.
- `IPC_PROTOCOL` goes from 31 to 32. Every new request and field is gated through `min_protocol()` with a `why` const, and gets an arm in `changes_fleet()`.
- No new runtime dependency except `sha2`.
- A changelog entry goes in `changes/feat-machine-bootstrap.md`.
- Commits are signed. `make check` passes before each commit.

## Review Focus

- **A join token pasted twice, or after expiry:** the second try gets ssh `Permission denied` (the line is gone), never a second machine. The pin is in Task 6, `token_line_removed_after_use`.
- **Two machines joining in the same second:** both lines end up in `authorized_keys`, and the user's own lines are untouched. The pin is in Task 3, `concurrent_edits_keep_every_line`.
- **A bootstrap rerun after a failed step:** no duplicate user, unit or key, and no `machine_exists`. The pin is in Task 9, `rerun_after_failure_resumes`.
- **A hijacked agent on a root-mode machine:** `pastor-agent` cannot read the service user's key. The pin is in Task 8, `service_state_dir_is_0700_and_owned`, and in Task 12's container test.
- **A pull machine whose shepherd sends `TaskClaim` with another machine's name:** it claims only for itself. The pin is in Task 4, `shepherd_machine_field_is_overwritten`.
- **A joining machine naming itself `x; rm -rf /` or similar:** the join is refused before any state changes, never reaches `authorized_keys`. The pin is in Task 5, `name_rejects_shell_metacharacters`.
- **A replaced machine's old key, still live on an open mux socket:** its next request is refused once the generation no longer matches, even though the socket is still authenticated. The pin is in Task 4, `shepherd_stale_generation_refused`.

---

### Task 1: Store `machine_state`

**Files:**
- Modify: `src/store.rs` (SCHEMA_VERSION 18 → 19, `V19_TABLES`, the migration branch)
- Modify: `tests/migrations.rs` (schema 18 DDL)

**Interfaces:**
- Produces:
  - `pub enum BootstrapState { Pending, Ok, Failed }`, serde snake_case.
  - `pub struct MachineState { pub machine: String, pub bootstrap: BootstrapState, pub mode: Option<String>, pub isolation: Option<String>, pub failed_step: Option<String>, pub why: Option<String>, pub host_key: Option<String>, pub herdr_version: Option<String>, pub pastor_version: Option<String>, pub bootstrap_login: Option<String>, pub generation: u64, pub updated_at: String }`. `generation` starts at 1 on enrollment and is bumped on `--replace`; `bootstrap_login` is the ssh destination a push bootstrap last connected with, kept separately from the flock entry's `ssh` (which bootstrap rewrites to the agent user), so a later `--sudo` rerun still has a sudo-capable login to connect as.
  - `Store::machine_state(&self, machine: &str) -> anyhow::Result<Option<MachineState>>`
  - `Store::put_machine_state(&self, s: &MachineState) -> anyhow::Result<()>`, an upsert.
  - `Store::set_versions(&self, machine: &str, herdr: Option<&str>, pastor: Option<&str>) -> anyhow::Result<()>`
  - `Store::drop_machine_state(&self, machine: &str) -> anyhow::Result<()>`
  - Table `join_tokens(id TEXT PRIMARY KEY, flock TEXT, tags TEXT, replace TEXT, expires_at TEXT NOT NULL, used_at TEXT)`.
  - Its methods:
    - `Store::put_join_token(&self, t: &JoinToken)`
    - `Store::join_token(&self, id: &str) -> Option<JoinToken>`
    - `Store::use_join_token(&self, id: &str, now: &str) -> anyhow::Result<bool>`: false when already used or expired.
    - `Store::expired_join_tokens(&self, now: &str) -> Vec<String>`
  - `JoinToken { id, flock: Option<String>, tags: Vec<String>, replace: Option<String>, expires_at: String, used_at: Option<String> }`.

- [ ] **Step 1: Write failing tests** in `src/store.rs` tests:
  - `machine_state_round_trips`: a put, then a get, gives equal fields.
  - `use_join_token_is_single_use`: the first call is true, the second false.
  - `expired_tokens_listed`: one expired token and one live token give `vec!["old"]`.
  - In `tests/migrations.rs`, `migrates_from_18`: a v18 file opens and `machine_state` exists.
- [ ] **Step 2: Run** `cargo test --features fake-herdr machine_state use_join_token expired_tokens migrates_from_18`. Expected: FAIL, nothing defined.
- [ ] **Step 3: Implement** the tables, following the `V5_TABLES` pattern. Tags are stored as JSON text.
- [ ] **Step 4: Run** the same command. Expected: PASS.
- [ ] **Step 5: Commit** `feat(store): machine_state and join_tokens tables`.

### Task 2: ssh options: pastor's own key and known_hosts

**Files:**
- Modify: `src/ssh.rs` (`Ssh`)
- Modify: `src/config/mod.rs` (`Paths::known_hosts_file()`, `Paths::head_key()`, `Paths::machine_key()`, `Paths::agent_key()`)
- Modify: `src/herdr/transport.rs` (`ssh_argv_running`, `Endpoint::Ssh`)
- Modify: `src/head.rs` (`RemoteHead::ssh_args`)

**Interfaces:**
- Produces:
  - `Ssh` gains `pub identity: Option<&'a Path>`, `pub known_hosts: Option<&'a Path>` and `pub accept_new: bool`.
  - `args()` adds:
    - with `identity`: `-i <p> -o IdentitiesOnly=yes`;
    - with `known_hosts`: `-o UserKnownHostsFile=<p>`, plus `-o StrictHostKeyChecking=accept-new` when `accept_new`, else `yes`.
  - Paths, all under the state dir:
    - `Paths::known_hosts_file() -> PathBuf`: `<state>/known_hosts`
    - `Paths::head_key() -> PathBuf`: `<state>/head_key`
    - `Paths::machine_key() -> PathBuf`: `<state>/machine_key`
    - `Paths::agent_key() -> PathBuf`: `<state>/agent_key`
  - `RemoteHead::ssh_args` passes `machine_key()`, else `agent_key()`, whichever exists, and `known_hosts_file()` when it exists.
  - The head's ssh to a machine passes `-i head_key()` without `IdentitiesOnly`, so existing user keys still work, and `known_hosts_file()` once the machine has a `machine_state.host_key`.

- [ ] **Step 1: Write failing tests** in `src/ssh.rs`:
  - `identity_adds_identities_only` asserts the argv contains `["-i", "/k", "-o", "IdentitiesOnly=yes"]`.
  - `known_hosts_strict_by_default` asserts `StrictHostKeyChecking=yes`.
  - `accept_new_when_asked`.
  - `no_options_unchanged`: today's argv byte for byte, so existing machines see no change.
- [ ] **Step 2: Run** `cargo test --features fake-herdr --lib ssh::`. Expected: FAIL.
- [ ] **Step 3: Implement.** Thread the new fields to every `Ssh {..}` literal, with `None`/`false` where nothing changes.
- [ ] **Step 4: Run** `cargo test --features fake-herdr`. Expected: PASS, the whole suite.
- [ ] **Step 5: Commit** `feat(ssh): pastor's own identity and known_hosts`.

### Task 3: The head's `authorized_keys` editor

**Files:**
- Create: `src/authkeys.rs`
- Modify: `src/lib.rs` (`pub mod authkeys;`)
- Modify: `src/bridge.rs`: `authorized_key_line` stays as it is, and its option list moves into `authkeys::restrict_options()`.

**Interfaces:**
- Produces:
  - `pub enum Marker { Machine(String), Agent(String), Join(String) }`, rendered `pastor:machine:<n>`, `pastor:agent:<n>`, `pastor:join:<id>`.
  - `pub fn line(pastor: &Path, command_args: &str, key: &str, marker: &Marker) -> anyhow::Result<String>` gives `restrict,command="<pastor> <args>" <key-type> <key-b64> <marker>`. It uses `restrict` plus today's options, and rejects a key that is not one line, with `invalid_key`.
  - `pub fn valid_name(s: &str) -> bool` checks `^[A-Za-z0-9][A-Za-z0-9._-]*$`. Every caller that embeds a machine name in `command_args` calls this first: `line()`'s own rejection of quotes, backslashes and whitespace guards the key and the pastor path, not a name crafted with `;`, backticks or `$(...)`, which only this charset check rules out.
  - `pub fn upsert(file: &Path, marker: &Marker, line: &str) -> anyhow::Result<()>` replaces the line with that marker or appends one.
  - `pub fn remove(file: &Path, marker: &Marker) -> anyhow::Result<bool>`.
  - `pub fn remove_machine(file: &Path, name: &str) -> anyhow::Result<()>` removes the Machine and Agent lines of that name.
  - All writes take an flock on `<file>.lock`, write `<file>.tmp.<pid>` at 0600, fsync, then rename. A missing file is created at 0600, with `~/.ssh` at 0700.
  - `pub fn default_file() -> PathBuf` is `~/.ssh/authorized_keys`, overridable by `PASTOR_AUTHORIZED_KEYS` for tests.

- [ ] **Step 1: Write failing tests** in `src/authkeys.rs`:
  - `upsert_replaces_same_marker`.
  - `foreign_lines_untouched`: a user line `ssh-ed25519 AAAA me@laptop` survives upsert and remove.
  - `remove_machine_drops_both`.
  - `concurrent_edits_keep_every_line`: 50 threads each upsert a distinct marker, and the end file has 50 marked lines plus the foreign one.
  - `file_mode_is_0600`.
  - `valid_name_rejects_shell_metacharacters`: `;`, `` ` ``, `$(...)`, `|`, `&` and whitespace all fail; `pi-1`, `gpu_3.local` pass.
- [ ] **Step 2: Run** `cargo test --features fake-herdr --lib authkeys`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the same command. Expected: PASS.
- [ ] **Step 5: Commit** `feat(authkeys): marked, locked edits of the head's authorized_keys`.

### Task 4: `bridge --shepherd --machine X`

**Files:**
- Modify: `src/bridge.rs` (`run_shepherd`, `answer_shepherd`)
- Modify: `src/main.rs` (`BridgeArgs`: `shepherd: bool`; `machine` requires `agent` or `shepherd`; `MachineCmd::AuthorizedKey` gains `--shepherd`)
- Modify: `src/ipc.rs` (`TaskClaim` gains optional `herdr_version`/`pastor_version`; PROTOCOL 32)
- Modify: `src/daemon.rs` (`heard` stores the versions through `Store::set_versions`)
- Modify: `src/shepherd.rs` (sends its versions in `TaskClaim`)

**Interfaces:**
- Consumes: `authkeys::line` (Task 3) and `Store::set_versions` (Task 1).
- Produces:
  - `pub async fn answer_shepherd(socket: &Path, machine: &str, gen: u64, line: &[u8]) -> anyhow::Result<Vec<u8>>`.
  - It first checks `gen` against `Store::machine_state(machine).generation`: no row, or a mismatch, gets `machine_revoked` and nothing is forwarded. `answer_agent` is unchanged — it has no `--gen`, same as today — because an agent key's actions (Ping, List, TaskShow, TaskRead, TaskDone) are read-only or self-scoped already, and giving it `--gen` would also break every already-deployed agent line, which carries no such argument.
  - It allows: everything `answer_agent` allows, plus `TaskClaim`, `TaskReport`, `PullTrust`, `JobSubmit`, `EventsSince`, `BootstrapReport` (Task 6) and `ArtifactLocate` (Task 7).
  - It overwrites `machine` in `TaskClaim`, `TaskReport`, `PullTrust` and `BootstrapReport` with `X`.
  - It overwrites `JobSubmit`'s `dispatch.machine` with `X` and clears `dispatch.flock`, so a machine key can submit work for itself only.
  - `EventsSince` is answered by a store query filtered to `machine == X` before pagination, not by filtering the daemon's already-paginated reply: the query's own cursor always reflects how far it scanned, so a page that happens to hold none of `X`'s events still advances past it instead of being requested again forever.
  - Anything else gets `not_allowed_for_shepherd`.
  - `pastor machine authorized-key NAME --key F --shepherd` prints the shepherd line, marker `pastor:machine:NAME`, with `--gen` from the current `machine_state.generation` (1 if there is no row yet).

- [ ] **Step 1: Write failing tests** in `src/bridge.rs`, against a fake head socket like the existing `answer_agent` tests:
  - `shepherd_machine_field_is_overwritten`: `TaskClaim{machine:"other"}` arrives at the head as `machine:"pi"`.
  - `shepherd_refuses_task_run` gives code `not_allowed_for_shepherd`.
  - `shepherd_events_filtered`.
  - `shepherd_events_cursor_advances_past_a_page_of_other_machines_events`: several pages of unrelated events, then one for `X`; the cursor reaches it on the next poll instead of replaying the same empty-for-`X` page.
  - `shepherd_allows_job_submit`.
  - `shepherd_job_submit_is_scoped_to_self`: `JobSubmit{dispatch:{machine:"other"}}` from machine `pi`'s key arrives at the head targeting `pi`, not `other`.
  - `shepherd_stale_generation_refused`: after a `--replace` bumps the generation, a request carrying the old `--gen`, replayed on the same fake connection, gets `machine_revoked`.
  - `shepherd_unknown_machine_refused`: no `machine_state` row at all gets `machine_revoked`.

  In `src/daemon.rs` tests: `claim_versions_stored`.
- [ ] **Step 2: Run** `cargo test --features fake-herdr shepherd_ claim_versions_stored`. Expected: FAIL.
- [ ] **Step 3: Implement.** Gate the new fields with `PULL_VERSIONS_PROTOCOL = 32`.
- [ ] **Step 4: Run** `cargo test --features fake-herdr`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(bridge): a locked shepherd mode; pull machines report versions`.

### Task 5: Join tokens on the head

**Files:**
- Create: `src/join.rs` (token type, encode/decode, create, redeem)
- Modify: `src/ipc.rs` (`JoinCreate`, `JoinList`, `JoinRevoke`, `Join` requests and replies)
- Modify: `src/daemon.rs` (handlers; the `Join` handler adds the machine)
- Modify: `src/scheduler.rs` (`pass` sweeps expired tokens)
- Modify: `src/main.rs` (`pastor join create|list|revoke`, `BridgeArgs.join: Option<String>` as `--join <ID>`)

**Interfaces:**
- Consumes: Task 1 (`join_tokens`), Task 3 (`authkeys`) and `fleet_edit::add_machine`.
- Produces:
  - `pub struct Token { pub head: String, pub head_host_key: String, pub key: String }`. `key` is the OpenSSH private key text.
    - `Token::encode(&self) -> String` gives `pjt_` + base64url, no padding, of `head\nhost_key\nkey`. The base64url encoder is a private fn in `src/join.rs`, about 30 lines; no crate.
    - `Token::decode(s: &str) -> anyhow::Result<Token>`, failing with `join_token_invalid`.
  - `pub fn create(paths, store, flock: Option<String>, tags: Vec<String>, replace: Option<String>, ttl: Duration) -> anyhow::Result<Token>`:
    1. Run `ssh-keygen -t ed25519 -N "" -C pastor:join:<id> -f <tmp>`. `<id>` is 16 hex chars from `/dev/urandom`.
    2. Upsert `authkeys::line(pastor, "bridge --join <id>", pub, Join(id))`.
    3. Store the row.
    4. Read the private key into the token, then delete both tmp files.
    5. `head_host_key` comes from `ssh-keyscan`-format lines of `/etc/ssh/ssh_host_ed25519_key.pub`. `head` is `head_address`, else `user@hostname()`.
  - `IpcRequest::Join { id: String, name: String, machine_key: String, agent_key: String, host_key: String, facts: serde_json::Value }`. The bridge sets `id` from `--join <ID>`; a request's own `id` is ignored. The reply is `IpcResponse::Joined { name: String }`.
  - The `Join` handler:
    1. `authkeys::valid_name(name)`, before anything else below: an invalid
       name gives `machine_name_invalid` without consuming the token or
       touching any state.
    2. `use_join_token`. Already used gives `join_token_used`; expired gives `join_token_expired`.
    3. The name check gives `machine_name_taken`, unless `replace == Some(name)`.
    4. Add `MachineConfig { pull: true, flock, tags, .. }`.
    5. Upsert the Machine line (`bridge --shepherd --machine <name> --gen <n>`) and the Agent line (`bridge --agent --machine <name>`, unchanged), where `n` is 1 for a fresh name or the existing row's `generation + 1` for `--replace`.
    6. Remove the Join line.
    7. `put_machine_state(Pending, host_key, generation: n)`.
    8. Emit `machine.joined`.
  - The scheduler `pass` removes the lines of expired tokens and emits `machine.join_token_expired`.
  - `bridge --join <ID>` accepts exactly one line, which must parse as `Join`. Anything else gets `join_token_invalid`. Then it exits.

- [ ] **Step 1: Write failing tests** in `src/join.rs` and `tests/cli/join.rs`:
  - `token_round_trips`.
  - `decode_rejects_garbage` gives `join_token_invalid`.
  - `join_adds_pull_machine`: after a join, `flock.toml` has `pull = true` with the token's flock and tags.
  - `token_line_removed_after_use`: the `pastor:join:<id>` line is gone after the join.
  - `second_join_refused` gives `join_token_used`.
  - `expired_swept`.
  - `name_taken_refused`, and `replace_allows_same_name_only`.
  - `name_rejects_shell_metacharacters`: `name = "x; rm -rf /"` gives `machine_name_invalid`, and neither marks the token used nor adds a machine — a second, valid join on the same token still succeeds.
  - `replace_bumps_generation`: the new Machine line carries `--gen 2` after a first join at `--gen 1`.
  - `bridge_join_refuses_other_requests`.

  Use `PASTOR_AUTHORIZED_KEYS` pointed at a temp file.
- [ ] **Step 2: Run** `cargo test --features fake-herdr join`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --features fake-herdr`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(join): one-time join tokens with a per-token ssh key`.

### Task 6: Bootstrap reports and machine state on the head

**Files:**
- Modify: `src/ipc.rs` (`BootstrapReport { machine: String, result: crate::bootstrap::Outcome }`)
- Modify: `src/daemon.rs` (handler: `put_machine_state`, emit `machine.bootstrapped` or `machine.bootstrap_failed`; a `Pending`/`Failed` machine takes no tasks)
- Modify: `src/describe.rs` and `src/cli.rs` (`machine describe` shows a bootstrap line; `machine list` versions fall back to `machine_state` for pull machines)

**Interfaces:**
- Consumes: `bootstrap::Outcome` (Task 8; define the type here first if Task 8 has not landed, in `src/bootstrap/outcome.rs`).
- Produces:
  - `pub struct Outcome { pub ok: bool, pub mode: Mode, pub isolation: Isolation, pub steps: Vec<StepResult>, pub machine_key: Option<String>, pub agent_key: Option<String>, pub host_key: Option<String>, pub herdr_version: Option<String>, pub pastor_version: Option<String> }`
  - `pub enum Mode { Root, User }`
  - `pub enum Isolation { Users, None }`
  - `pub struct StepResult { pub step: String, pub status: StepStatus, pub why: Option<String>, pub fix: Option<String>, pub missing: Vec<String> }`
  - `pub enum StepStatus { Done, AlreadyThere, Failed, Skipped }`
  - Dispatch skips a machine whose `machine_state.bootstrap` is not `Ok`. A machine with no row is treated as `Ok`, so existing machines are unaffected.
  - `machine describe` prints:
    - `bootstrap: ok, root mode (agents as pastor-agent, shepherd as <user>)`
    - or `bootstrap: failed at <step>: <why>; fix: <fix>`
    - or, in user mode, `isolation: none (agents can read the machine key)`.

- [ ] **Step 1: Write failing tests:**
  - `pending_machine_gets_no_task`.
  - `machine_without_state_still_dispatches`.
  - `failed_report_emits_event` (kind `machine.bootstrap_failed`, detail `step`).
  - `describe_shows_bootstrap_line`.
- [ ] **Step 2: Run** `cargo test --features fake-herdr bootstrap_ pending_machine machine_without_state describe_shows_bootstrap`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --features fake-herdr`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(machine): bootstrap state on the head`.

### Task 7: The artifact cache

**Files:**
- Create: `src/artifacts.rs`
- Modify: `Cargo.toml` (add `sha2`; the reason goes in the commit body: checksums of downloaded binaries must not depend on which sha tool a box has)
- Modify: `src/config/mod.rs` (`[provision]` table: `herdr: Option<String>`, `herdr_index: Option<String>`)
- Modify: `src/ipc.rs` (`ArtifactLocate { name, version, target }` → `IpcResponse::Artifact { path: PathBuf, sha256: String, size: u64 }`)
- Modify: `src/bridge.rs` (shepherd mode handles `ArtifactLocate { name, version, target }` as a request line like any other: locate and check it through the daemon, write the JSON reply line, then stream the file's raw bytes straight to stdout. The request travels on stdin, never as extra argv — sshd replaces whatever command a client asks for with the forced one, so argv the client appends after `bridge` never reaches the process)

**Interfaces:**
- Produces:
  - `pub struct Artifact { pub name: String, pub version: String, pub target: String }`. `Artifact::new` validates every part against `^[A-Za-z0-9._-]+$`, not `.`/`..`, else `invalid_artifact`.
  - `pub fn cache_path(paths: &Paths, a: &Artifact) -> PathBuf` is `<state>/artifacts/<name>/<version>/<target>/<name>`.
  - `pub async fn ensure(paths: &Paths, cfg: &ProvisionConfig, a: &Artifact) -> anyhow::Result<(PathBuf, String)>`:
    1. If cached and the sha256 matches its `.sha256` file, return it.
    2. Else download with `curl -fsSL --proto =https` to a temp file, check it, and rename.
    3. A mismatch gives `artifact_checksum_mismatch`.
  - Sources:
    - `pastor` is `https://github.com/cacarico/pastor/releases/download/v<ver>/pastor-<ver>-<triple>.tar.gz` plus `SHA256SUMS`; the binary is extracted with `tar`. When `env!("CARGO_PKG_VERSION") == ver` and the head's target equals `<triple>`, it copies `current_exe()`.
    - `herdr` comes from `herdr_index` JSON: `releases[ver].assets[target]` and `releases[ver].sha256[target]`.
  - `pub fn herdr_version(cfg: &ProvisionConfig, fleet_versions: &[String]) -> String`: the config value, else the most common version reported by the fleet, else `DEFAULT_HERDR_VERSION`.
  - `pub fn target_for(facts: &Facts, name: &str) -> anyhow::Result<String>` maps the facts to pastor's triple or herdr's key. Unsupported gives `bootstrap_unsupported_platform`.

- [ ] **Step 1: Write failing tests** in `src/artifacts.rs`, using a `file://` index and tarball under a tempdir:
  - `rejects_traversal` (`..`, `a/b`, empty).
  - `herdr_from_index`.
  - `checksum_mismatch_refused`.
  - `cached_not_downloaded_twice`, which counts calls through an injected fetch fn.
  - `herdr_version_falls_back` (config, then fleet mode, then the default).
  - `target_mapping` (linux x86_64 and aarch64, darwin arm64; armv7 → error).

  In `src/bridge.rs`: `shepherd_fetch_reads_request_from_stdin_not_argv`: a shepherd session sent only `ArtifactLocate{..}` on stdin, with no extra argv at all (modelling what sshd actually hands the process), gets the JSON reply followed by the file's bytes.
- [ ] **Step 2: Run** `cargo test --features fake-herdr --lib artifacts` and `cargo test --features fake-herdr --lib bridge::`. Expected: FAIL.
- [ ] **Step 3: Implement.** `ensure` takes the fetch as a `&dyn Fetch` trait (`fn get(&self, url: &str, to: &Path) -> anyhow::Result<()>`). The real one shells out to curl; tests use a copy from `file://`.
- [ ] **Step 4: Run** `cargo test --features fake-herdr`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(artifacts): a checked cache of pastor and herdr on the head`.

### Task 8: `pastor bootstrap`, the local steps

**Files:**
- Create: `src/bootstrap/mod.rs` (`run`, the step list), `src/bootstrap/facts.rs`, `src/bootstrap/steps.rs`, `src/bootstrap/outcome.rs` (from Task 6)
- Modify: `src/config/mod.rs` (`hostname()` falls back to `libc::gethostname` before `"-"`)
- Modify: `src/setup.rs` (`Unit::Herdr` also looks in the dir of `current_exe()` and in `~/.local/bin`; `Install` takes `require_linger: bool` and fails with a clear message when lingering is off)
- Modify: `src/main.rs` (`pastor bootstrap` subcommand, hidden from the short help; flags `--sudo`, `--service-user <NAME>`, `--head <DEST>`, `--head-key <PUBKEY>`, `--herdr-from <PATH>`, `--json`)

**Interfaces:**
- Consumes: `setup::Runner` and `setup::Install`, `Outcome` (Task 6), `artifacts::target_for` (Task 7).
- Produces:
  - `pub struct Facts { pub os: String, pub arch: String, pub userland_bits: u8, pub init: Init, pub pkg: Option<Pkg>, pub uid: u32, pub sudo: bool, pub home: PathBuf, pub hostname: String }`
  - `pub enum Init { Systemd, Launchd, Other(String) }`
  - `pub enum Pkg { Apt, Dnf, Pacman, Brew }`
  - `pub fn facts(r: &dyn Runner) -> Facts`
  - `pub struct Options { pub sudo: bool, pub service_user: Option<String>, pub head: String, pub head_key: Option<String>, pub herdr_from: Option<PathBuf>, pub pastor_from: PathBuf }`
  - `pub fn run(r: &dyn Runner, o: &Options) -> Outcome`. It never panics; it stops at the first failed step.
  - The steps, in order: `facts`, `platform`, `packages`, `users`, `pastor`, `herdr`, `keys`, `client`, `services`. Each is `fn(&dyn Runner, &Ctx) -> StepResult` and checks before it acts. `services` runs last, after `keys` and `client`, because starting the unit is what picks the process's role (head client or shepherd) from the config those two steps just wrote; starting it any earlier risks it coming up as the wrong role.
  - Root work goes through `sudo -n` when not uid 0. Without sudo or root, user mode. `--sudo` with `sudo -n true` failing gives `bootstrap_needs_sudo`.
  - Root mode:
    - `useradd --system --create-home --shell /bin/sh pastor-agent` when missing (and the service user if asked).
    - `loginctl enable-linger` for both.
    - The service user's state dir is 0700.
    - Every per-user action runs as `sudo -n -u <user> -H env XDG_RUNTIME_DIR=/run/user/<uid> …`.
    - A scoped `/etc/sudoers.d/pastor-<name>` (NOPASSWD, naming exactly one command: running herdr as `pastor-agent`), so the service user's shepherd can reach the agent user's herdr. Without it, `shepherd_connector`'s default local session resolves a socket under the service user's own home, which in root mode is the wrong home for herdr entirely.
  - The head key goes into `pastor-agent`'s `~/.ssh/authorized_keys`. In user mode it goes into the login user's file (it is usually there already).
  - The `keys` step runs `ssh-keygen -t ed25519 -N "" -f <state>/machine_key` as the service user and `.../agent_key` as the agent user, only when missing. It reads the public halves and `/etc/ssh/ssh_host_ed25519_key.pub` into the Outcome.
  - The `client` step writes `client.toml` `[head] ssh = <head>` for both users (`head::save`), and, in root mode only, `[shepherd] command = ["sudo", "-n", "-u", "pastor-agent", "-H", "herdr", "attach", "default"]` for the service user, matching the sudoers rule above, so the shepherd reaches the agent's herdr instead of its own missing local session.
  - When the `herdr` step finds a running herdr at another version, it does the following. With no `t-N` agent live (`herdr agent list`), it swaps the binary and restarts the unit. Otherwise it leaves both alone and reports `Skipped`, with why `herdr <old> runs agents; rerun when idle`.

- [ ] **Step 1: Write failing tests** in `src/bootstrap/`, with a FakeRunner that answers by command prefix:
  - `facts_parse_debian_arm64_32bit_userland` (`userland_bits == 32` gives `bootstrap_unsupported_platform` for herdr).
  - `user_mode_without_sudo`: mode `User`, isolation `None`, no `useradd` call.
  - `root_mode_creates_both_users`.
  - `existing_user_reused`: `id pastor-agent` succeeds, so no `useradd`.
  - `needs_sudo_when_asked`.
  - `linger_off_fails_services_step`: status `Failed`, and `fix` mentions `loginctl enable-linger`.
  - `keys_not_regenerated`.
  - `service_state_dir_is_0700_and_owned`: a `chmod 700` and `chown <service>` call is recorded.
  - `services_step_runs_after_keys_and_client`: the recorded command order has `client.toml` written before the service unit starts.
  - `root_mode_writes_shepherd_command_and_sudoers`: `[shepherd] command` in the service user's `pastor.toml`, and a sudoers drop-in naming only the one herdr command, as `pastor-agent`.
  - `user_mode_writes_no_shepherd_command`: `[shepherd] command` is absent; login and agent user are the same.
  - `macos_root_unsupported`.
  - `hostname_falls_back_to_gethostname`.
- [ ] **Step 2: Run** `cargo test --features fake-herdr --lib bootstrap`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --features fake-herdr`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(bootstrap): pastor bootstrap runs the setup steps on a machine`.

### Task 9: Push: `machine add --bootstrap` and `machine bootstrap`

**Files:**
- Create: `src/bootstrap/push.rs`
- Modify: `src/main.rs` (`MachineCmd::Add` gains `--bootstrap`, `--sudo`, `--service-user`, `--host-key`; new `MachineCmd::Bootstrap { name, --sudo, --service-user, --login }`)
- Modify: `src/daemon.rs` (a `MachineBootstrap` request runs the push off the dispatch lock)
- Modify: `src/ipc.rs` (`MachineBootstrap { name: String, sudo: bool, service_user: Option<String>, host_key: Option<String> }`)

**Interfaces:**
- Consumes: Tasks 1, 2, 3, 6, 7 and 8.
- Produces: `pub async fn push(paths, store, fleet, name: &str, login: &str, opts: PushOpts) -> anyhow::Result<Outcome>`. It runs these steps:
  1. **Record the attempt.** `put_machine_state(Pending, bootstrap_login: Some(login), generation: existing.map(|s| s.generation).unwrap_or(1))` and write the flock entry with `ssh = login`. When the entry exists with the same login and state Pending or Failed, go on instead of failing `machine_exists`. `bootstrap_login` is kept separate from the flock entry's `ssh`, which step 6 below rewrites to the agent user in root mode: a later `machine bootstrap NAME --sudo` rerun connects with this stored login (or `--login` when given), not with whatever `ssh` currently says, so it still has a sudo-capable user to connect as.
  2. **Pin the host key.** Only when `known_hosts_file()` has no entry for `<host>` yet: `ssh-keyscan -t ed25519 <host>`, checked against `--host-key <fp>` when given, then written with `accept_new`. A rerun against a host already pinned skips the scan and connects with `StrictHostKeyChecking=accept-new` against that existing entry, so a changed key stops the rerun instead of being re-trusted on the strength of an unauthenticated scan.
  3. **Read the platform.** `ssh <login> uname -sm; getconf LONG_BIT`, then `target_for`.
  4. **Copy the binaries.** `artifacts::ensure` pastor and herdr, then copy them to `~/.cache/pastor-bootstrap/` over ssh (`ssh … 'cat > f'` with stdin from the file), and check the sha256 there with `sha256sum` or `shasum`.
  5. **Run bootstrap.** `ssh <login> ~/.cache/pastor-bootstrap/pastor bootstrap --json --head <head> --head-key "<head_key.pub>" --herdr-from … [--sudo] [--service-user …]`, then parse the Outcome.
  6. **Finish on success.**
     - Upsert the Agent line for the machine (agents reach the head with `bridge --agent`).
     - Store the host key.
     - Rewrite the entry's `ssh` to `pastor-agent@<host>` in root mode (`fleet_edit` replace). `machine_state.bootstrap_login` is left as `login`, untouched by this rewrite.
     - Set the state to Ok, emit `machine.bootstrapped`, and close the old ControlMaster (`ssh -O exit`).
  7. **On failure.** State Failed with the step, why and fix, then emit `machine.bootstrap_failed`.
- The head makes `head_key()` with `ssh-keygen` on first use.

- [ ] **Step 1: Write failing tests** in `src/bootstrap/push.rs`, with an injected ssh runner:
  - `push_writes_pending_then_ok`.
  - `rerun_after_failure_resumes`: the first run fails at `packages`; the second, with sudo, gets no `machine_exists` and ends Ok.
  - `host_key_mismatch_stops` (`--host-key` differs).
  - `rerun_keeps_pinned_host_key_and_rejects_a_changed_one`: a second push, no `--host-key` given, against a host whose key changed since the first push, fails instead of re-trusting it.
  - `root_mode_rewrites_ssh_to_agent_user`.
  - `sudo_rerun_after_rewrite_uses_stored_login`: `machine bootstrap NAME --sudo` after root-mode push connects with `bootstrap_login`, not the rewritten `pastor-agent@host`.
  - `user_mode_keeps_login`.
  - `agent_line_installed`.
- [ ] **Step 2: Run** `cargo test --features fake-herdr --lib push`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --features fake-herdr`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(machine): machine add --bootstrap and machine bootstrap`.

### Task 10: Pull: `pastor join` on the machine, and `install.sh join`

**Files:**
- Create: `src/bootstrap/pull.rs`
- Modify: `src/main.rs` (`pastor join` with `<TOKEN>`, or `--token-stdin`, or `PASTOR_JOIN_TOKEN`; `--name`, `--sudo`, `--service-user`; it lives under the same `join` command as `create|list|revoke`, which take a subcommand word)
- Modify: `src/shepherd.rs` (sends a `BootstrapReport` right after the first claim when `<state>/bootstrap.json` exists; deletes it only once the head's reply acknowledges receipt, and retries on every claim attempt until then)
- Modify: `install.sh` (`sh -s -- join [args…]` runs the installed pastor's `join` with the rest of the args; with `id -u` 0 and ssh-keygen/curl/tar missing, install `openssh-client curl tar ca-certificates` via apt/dnf/pacman first)

**Interfaces:**
- Consumes: Tasks 4, 5, 7 and 8.
- Produces: `pub async fn join(paths, token: Token, o: JoinOpts) -> anyhow::Result<Outcome>`. It runs these steps:
  1. **Pin the head's host key.** Write `token.head_host_key` into `known_hosts_file()` for `token.head`.
  2. **Make the identities.** Make the service user's machine key and the agent user's key, using bootstrap's `users` and `keys` steps first, run alone.
  3. **Join.** Write `token.key` to a 0600 temp file and `ssh -i tmp -o IdentitiesOnly=yes … <head> bridge` (the forced command decides the mode). Send `Join { name, machine_key, agent_key, host_key, facts }`, read `Joined { name }`, then delete the temp file whatever happens.
  4. **Name the shepherd.** Write `[shepherd] machine = "<name>"` to the service user's `pastor.toml`.
  5. **Bootstrap.** Run `bootstrap::run` with herdr fetched through `ssh -i machine_key … <head> bridge` (the forced command is always `bridge --shepherd --machine X --gen N`; sshd replaces whatever command the client asked for, so there is no argv to carry a fetch request). The fetch travels as an `ArtifactLocate { name: "herdr", version, target }` line on that session's stdin, the same as every other shepherd request; the reply is the JSON line followed immediately by the file's raw bytes on the same stdout.
  6. **Report.** Whatever happens — success, a failure at any step, even one before `services` ever starts and so before any shepherd exists to report it later — send a `BootstrapReport` directly over one more `ssh -i machine_key … <head> bridge` session, using the machine key step 2 already made. On success, delete `<state>/bootstrap.json` if it exists. If the head cannot be reached, write/keep the Outcome at `<state>/bootstrap.json` instead and print it; the shepherd, once it does start, retries the report on every claim and deletes the file only once the head's reply acknowledges it, never merely after sending it.

- [ ] **Step 1: Write failing tests:**
  - In `tests/real_ssh.rs` (ignored, `make test-ssh`): `pull_join_end_to_end`:
    - `join create`, then `pastor join <token>` against the sshd head with `PASTOR_AUTHORIZED_KEYS` set to the sshd `AuthorizedKeysFile`;
    - afterwards `flock.toml` has the machine with `pull = true`;
    - the join line is gone;
    - `machine describe` shows bootstrap ok after the shepherd's first claim.

  In `src/bootstrap/pull.rs`:
  - `temp_join_key_deleted_on_error`.
  - `token_from_stdin`.
  - `shepherd_name_written`.
  - `failure_before_services_is_reported_directly`: `bootstrap::run` fails at `packages`, before any shepherd service exists; the head still gets `machine.bootstrap_failed`, sent over the machine key `join` just made.
  - `unreachable_head_keeps_report_file`: the direct report's `ssh` fails; `<state>/bootstrap.json` survives for the shepherd to retry later.

  In `src/shepherd.rs`: `bootstrap_report_file_kept_until_acknowledged`: the head's reply errors; the file is still there on the next claim, and the report is sent again.
- [ ] **Step 2: Run** `cargo test --features fake-herdr --lib pull` and `make test-ssh`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** both again. Expected: PASS.
- [ ] **Step 5: Commit** `feat(join): pastor join bootstraps a pull machine; install.sh join`.

### Task 11: Removal and `pastor leave`

**Files:**
- Modify: `src/daemon.rs` (the `MachineRemove` handler also calls `authkeys::remove_machine`, drops the `known_hosts` entry with `ssh-keygen -R <host> -f <known_hosts>`, runs `ssh -O exit` on its control path, and calls `drop_machine_state`)
- Modify: `src/main.rs` (`pastor leave [--yes]`: disables and removes this machine's pastor and herdr user units for the service and agent users, and deletes the machine and agent keys)

**Interfaces:**
- Consumes: Tasks 1, 2 and 3.

- [ ] **Step 1: Write failing tests:**
  - `remove_cleans_keys_and_state`, with a temp `PASTOR_AUTHORIZED_KEYS` and known_hosts.
  - `leave_removes_units_and_keys`, with a FakeRunner.
- [ ] **Step 2: Run** `cargo test --features fake-herdr remove_cleans leave_removes`. Expected: FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test --features fake-herdr`. Expected: PASS.
- [ ] **Step 5: Commit** `feat(machine): removing a machine revokes its keys; pastor leave`.

### Task 12: Container end-to-end tests

**Files:**
- Create: `tests/bootstrap_e2e.rs` (ignored; `make test-bootstrap`)
- Create: `tests/fixtures/bootstrap/{debian,fedora,arch}.Dockerfile` (systemd as PID 1, sshd, a `me` user with passwordless sudo and the test's key)
- Modify: `Makefile` (`test-bootstrap: cargo build --release && cargo test --features fake-herdr --test bootstrap_e2e -- --ignored --test-threads 1`)

**Interfaces:**
- Consumes: everything above.
- The tests start each container with `docker run --privileged --cgroupns=host -v /sys/fs/cgroup:/sys/fs/cgroup:rw`. The head is the test process, with a temp state dir. herdr comes from a `file://` index of the real herdr binary on the host when present, else the test is skipped with a message.

- [ ] **Step 1: Write the tests** (each fails until it runs against a container):
  - `push_root_<distro>`: `machine add --bootstrap --sudo` ends Ok. `systemctl --user -M pastor-agent@ is-active herdr` is active. `sudo -u pastor-agent cat <service state>/machine_key` fails.
  - `push_user_<distro>`: user mode, `isolation: none`.
  - `push_rerun_<distro>`: remove git first, run without `--sudo` (expect failed at packages), then rerun with `--sudo` (ends Ok).
  - `pull_join_debian`: `install.sh join --token-stdin` inside the container, against the head over the docker network.
- [ ] **Step 2: Run** `make test-bootstrap`. Expected: PASS on debian, fedora and arch.
- [ ] **Step 3: Commit** `test(bootstrap): end-to-end in debian, fedora and arch containers`.

### Task 13: Docs, error table, events, changelog

**Files:**
- Modify: `docs/manual.md`:
  - a "Bootstrapping a machine" section covering push, pull, users, isolation, upgrades and removal;
  - the error code table, with every code from Global Constraints;
  - the events list.
- Modify: `AGENTS.md` (the bridge modes; `pastor-agent`; that a bootstrapped machine's state lives in `machine_state`, not `flock.toml`)
- Modify: `SECURITY.md`: a threat model section covering:
  - the head holds every machine's trust;
  - user mode has no isolation;
  - what a leaked token can do before expiry, and after it.
- Modify: `skills/pastor/SKILL.md` (new commands; the "new machine" checklist now starts with `--bootstrap`)
- Create: `changes/feat-machine-bootstrap.md` (`### Added`)

- [ ] **Step 1: Run** `cargo test --features fake-herdr --test error_codes` and `make check`. Expected: PASS.
- [ ] **Step 2: Commit** `docs: machine bootstrap and join`.
