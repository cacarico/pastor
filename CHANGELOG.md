# Changelog

All notable changes to this project are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## 0.5.0 - 2026-09-26

### Added

- `pastor task send` types into a done task whose pane is still open, and
  the task goes back to running. An agent marked done with its work
  unfinished can be told to finish in the same pane, with its context, instead
  of a new task in its worktree. A closed task, or one with no pane, is still
  `task_not_live`.

- Agents pastor starts may no longer change the fleet. Every agent's pane gets
  `PASTOR_TASK=t-N`, and a command from it that runs, sends to, attaches to,
  retries, closes or prunes tasks, ticks (dry runs too), runs or reloads jobs,
  installs, links, uninstalls or unlinks connectors, edits machines, flocks or
  jobs, starts or sets up a head, or opens herdr's UI fails with `agent_refused`; reads still work. The head refuses such a
  request, and the CLI refuses the edits it makes on its own.
  `agents_change_fleet = true` in `pastor.toml` allows them again. A worktree
  task's agent now always runs in a pane split off the worktree's, since the
  mark is env and herdr's worktree calls take none.

- A second agent skill, `skills/spec` (`/pastor:spec` once the repo is
  installed as a Claude Code plugin; `.claude-plugin/plugin.json` makes it
  one). It starts from a superpowers brainstorm and writes a plan whose tasks
  each carry a flock, repo, branch, model, timeout and a prompt file that
  needs no answers, run with `pastor task run --prompt-file`. The plan, its
  prompts and a ledger live on a plan branch so any machine can resume;
  tasks run one after another.

- `describe` for one thing in full, kubectl style, each with `--json`:
  `pastor job describe <name>` (schedule, connector and its config, dispatch,
  last runs and errors, next run, recent tasks and job events),
  `pastor machine describe <name>` (host, flock, channel, versions, agents,
  tags, its tasks, recent errors), `pastor flock describe <name>` (default or
  not, its agent and args, machines, queued and running tasks), and
  `pastor task describe <id>`, an alias of `task show`.
- `edit` for the files behind them: `pastor job edit <name>`,
  `pastor flock edit` and `pastor config edit` open a copy in `$VISUAL`, else
  `$EDITOR`, else `vi`, check it the way the head loads it, and only then
  replace the file atomically and reload a running head. An invalid edit
  offers to reopen with the error on top; declining keeps the file as it was
  and names where the edit is kept (`invalid_edit`). A symlinked job file is
  edited at its target. `editor_failed` and `edit_conflict` cover an editor
  that fails and a file changed during the edit.

- `pastor setup launchd [--herdr]` installs pastor (or the herdr server) as a
  macOS LaunchAgent in `~/Library/LaunchAgents`, with the same action flags
  and confirmation prompt as `pastor setup systemd`.
- `pastor task run --prompt-file <path>` reads the prompt from a file on the
  machine running the CLI (`-` for stdin), so a long prompt with quotes needs
  no shell quoting. It conflicts with the positional prompt; exactly one is
  required. Trailing newlines are trimmed. An unreadable file fails with
  `prompt_file_unreadable`, an empty one with `prompt_file_empty`.

- A machine can name its own agent: `agent` and `agent_args` under a
  `[[machine]]` entry in `flock.toml`, before its flock's and `[defaults]`, so
  machines of one flock can run different agents. The head settles a task's
  agent again when it places it on a machine, and `pastor task show` prints
  where the agent and its args came from.

- A flock can name the agent its tasks run: `agent` and `agent_args` under a
  `[[flock]]` entry in `flock.toml`. A task or job that names none takes its
  flock's, then `[defaults]`, then the built-in `claude`; `--agent` and
  `--agent-arg` always win. The head settles the agent when it queues the
  task, and `pastor task show` prints what it resolved to.
- Tool allow and deny lists: `allow` and `deny` (tool patterns such as
  `"Bash(git:*)"`) under `[defaults]`, a `[[flock]]` entry and a job's
  `[dispatch]`. They add up across the three and deny wins over allow. pastor
  passes them as the agent's own flags, `--allowedTools` and
  `--disallowedTools` for Claude, and leaves its permission mode alone;
  `[agents.<name>] allow_flag` and `deny_flag` name them for another agent,
  and a task whose agent has none for a list it carries is refused
  (`agent_tools_unsupported`). `pastor task show` prints both lists.
- Agent definitions can set `kind` and `env`: `[agents.claude-personal]`
  with `kind = "claude"` and `env = { CLAUDE_CONFIG_DIR = "~/.claude-personal" }`
  starts a claude whose pane has that env, `~` expanded against the task
  machine's home. Built-in trust keys and tool flags follow the kind, and a
  task, job or flock names the definition like any agent. A worktree task
  gets the env through a pane split off the worktree's, since herdr's
  worktree calls take none.
- A Trust model section in the manual, and a warning there on agent args
  that turn permission checks off, such as `--dangerously-skip-permissions`.

- A release workflow (`.github/workflows/release.yml`). Pushing a `vX.Y.Z`
  tag builds static musl binaries for x86_64, aarch64, armv7 and riscv64
  Linux and native macOS arm64 and x86_64 binaries, packs each as
  `pastor-<version>-<target>.tar.gz` with `SHA256SUMS`, attests their build
  provenance, and drafts a GitHub release with that version's changelog
  section as notes.
- `install.sh`: `curl -fsSL https://raw.githubusercontent.com/cacarico/pastor/main/install.sh | sh`
  picks the tarball for the OS and architecture, checks it against
  `SHA256SUMS` (and the provenance attestation when a logged-in `gh` is
  present), and installs the binary to `~/.local/bin` without sudo.
  `PASTOR_VERSION`, `PASTOR_INSTALL_DIR` and `PASTOR_DOWNLOAD_URL` override
  the version, the directory and the download source.
- CI runs a `portability` job on every pull request: `cargo check` of the
  whole crate for x86_64 musl, armv7 musl and x86_64 FreeBSD through zig, so
  a change that only compiles against glibc fails on the pull request, not
  on the release tag.

### Changed

- Breaking: plugins are now called connectors, everywhere, and the old names
  are gone. `pastor plugin ...` is `pastor connector
  install|link|uninstall|unlink|list|run`; the manifest is
  `pastor-connector.toml`; commands get `PASTOR_CONNECTOR_ID` and
  `PASTOR_CONNECTOR_STATE_DIR`; `PASTOR_PLUGIN_GIT_BASE` is
  `PASTOR_CONNECTOR_GIT_BASE`; the `plugins/` directories under the config,
  data and state dirs are `connectors/`. To upgrade, rename each manifest and
  move the three `plugins/` directories to `connectors/`, or reinstall. A
  connector still holds a connector command, event hooks, or both. "Plugin"
  is kept for a later idea: code that changes how pastor itself behaves.

- The crate is `pastor-cli` on crates.io, because `pastor` there belongs to
  another project: `cargo install pastor-cli` and `cargo binstall pastor-cli`
  install the `pastor` binary. The command, the release tarballs and
  `install.sh` keep the name pastor, and `fake-herdr` is left out of the
  published package.

- On macOS the config, state and data dirs follow the XDG layout, as on
  Linux: `~/.config/pastor`, `~/.local/state/pastor`, `~/.local/share/pastor`
  instead of `~/Library/Application Support/pastor`. A config left in the old
  place is moved once, with a note; its `plugins/` goes to the new
  `connectors/` dirs, and a move that fails part way is finished by the next
  run.
- Agent args follow the agent they were written for: `[defaults] agent_args`
  no longer reach a task that runs another agent than `[defaults] agent`
  (`pastor task run --agent codex` used to get Claude's `--model`).
- The head protocol is 2. `pastor task run`, `pastor task retry`,
  `pastor tick` (not `--dry-run`) and `pastor job run` refuse a head below it
  (`head_too_old`), which would start the agent without its tool lists:
  restart `pastor serve` after upgrading.
- The head, not the CLI, settles a `pastor task run` task's agent, since only
  it knows the task's flock. `pastor task run` has the head apply any edit of
  `pastor.toml` or `flock.toml` first, so an edit still takes effect at once.

- `cargo install` and `make install` install only the `pastor` binary
  (`--bin pastor`); `fake-herdr` is a test double and no longer lands on
  the PATH.
- Release builds are stripped and link-time optimised, so a downloaded
  binary is about a third smaller.
- `Cargo.toml` declares the minimum Rust version (1.88) and the repository,
  and carries cargo-binstall metadata pointing at the GitHub release
  tarballs.

### Security

- Item values rendered into a job's prompt lose every control character but
  newline and tab (C0, DEL and C1), so an issue title or chat message cannot
  type key presses into the agent's terminal. The job's own prompt text and
  `pastor task send` are unchanged.
- Lines read from herdr are capped at 16 MiB and the output of the ssh probes
  (home, repo directory, pastor version) at 64 KiB per stream. Past the cap
  the connection is dropped with a protocol error, or the probe is killed,
  so one machine cannot exhaust the head's memory.
- An item value put into `repo` or `branch` may no longer be empty (which a
  missing field renders to) or `.`, so it cannot point the task at the
  template's parent directory.
- A job whose `branch` puts an item value in its first component, such as
  `{{ item.branch }}`, is invalid: the job must fix a prefix like
  `pastor/{{ item.key }}`, so an item cannot pick an existing branch such as
  `main`.
- An ssh target that is empty, starts with `-`, or holds whitespace or a
  control character makes `flock.toml` fail to load, and every ssh command
  pastor runs passes `--` before the target.
- `pastor serve` logs a failed `accept` on its socket and keeps serving
  instead of exiting. An IPC request line is capped at 1 MiB
  (`request_too_large`) and must arrive within 10 seconds.
- `SECURITY.md` and the manual's new Trust model section state that the
  head's user is the fleet's trust boundary, that `local = true` machines
  should not run untrusted work, and what plugins inherit.

### Fixed

- An agent that ends its turn on a question is marked `blocked` with the
  question as its error, not `done` with nothing pushed; `pastor task send`
  answers it. A pane read lost to a dropped connection keeps the task pending
  instead of settling it.
- Auto-close keeps a worktree whose commits are on no remote, closes the pane
  and notes the branch on the task, as it does for uncommitted changes.
- After a trust answer (`pastor task send --trust` or the head's own), the
  task's prompt is held for `settle` before it is sent. Claude redraws its
  prompt box then, and a prompt sent into that window was lost, leaving the
  agent at an empty input.
- `pastor machine list` refreshes a machine's pastor version while it stays
  connected, so an upgrade shows without restarting the head.
- The first `flock add --default` on a file without `[[flock]]` entries moves
  the machines that name no flock into the new flock, instead of writing them
  into an explicit `default` flock. Machines stay while tasks are queued in
  the implicit flock, and the output names those tasks.
- Connector commands (`install`, `link`, `uninstall`, `unlink`) reuse the
  CLI's head check instead of probing the head a second time.
- A prerelease build (`0.5.0-rc.1`) no longer panics on its own version: it
  compares as the release it leads to for `min_pastor_version`.
- A `local = true` machine on macOS finds herdr's socket under
  `~/.config/herdr`, where herdr puts it.
- The ssh ControlPath length check uses macOS's 104-byte socket path limit
  there, not Linux's 108, so a long state dir no longer breaks multiplexing.
- `pastor setup systemd` and `pastor setup launchd` write the effective
  config, state and data dirs into the pastor service as absolute
  `PASTOR_CONFIG_DIR`, `PASTOR_STATE_DIR` and `PASTOR_DATA_DIR`, so a head
  started at login uses the dirs the setup run used, not only when an override
  was set.

## 0.4.0 - 2026-09-25

### Added

- Named flocks. `[[flock]]` entries in `flock.toml` declare them, one with
  `default = true`, and `flock = "..."` puts a machine in one; a machine
  without it is in the default flock. A file with no `[[flock]]` entry is one
  flock named `default`, so existing files load unchanged.
- A task and a job target one flock and only its machines take their tasks:
  `--flock` on `pastor task run`, `flock` under a job's `[dispatch]`, else the
  pinned machine's flock, else the default. `--machine` outside `--flock` is
  refused (`flock_mismatch`), an unknown flock too (`unknown_flock`).
  `pastor task retry` keeps the flock, and is refused (`unknown_flock`) once
  it is removed. A head from before flocks is refused (`head_too_old`) by
  every command that talks to or reloads it while flocks are in play:
  `--flock`, an edit of flock.toml, or named flocks declared there. `Ping`
  answers the head's IPC protocol for this.
- `pastor flock list|add [--default]|remove|default`, `pastor machine move`,
  and `--flock` on `machine add`, `machine list` and `task list`. `task list`,
  `task show` and `job list` show the flock, and task events carry it in the
  task row.
- `pastor task send <task> [TEXT] [--key K]... [--no-enter]` types into a
  live task's agent through the head, to answer what it is waiting on without
  attaching. Only starting, running and blocked tasks take input
  (`task_not_live`). Each send emits `task.input`, which records the key
  names and the text length, never the text.
- Saved repo trust. `pastor task send <task> --trust` presses the agent's
  folder-trust keys to a task blocked on its startup prompt
  (`not_at_trust_prompt` otherwise) and saves the task's machine and repo;
  the head then answers the trust prompt of that repo's later tasks on that
  machine on its own, once per task, and emits `task.trusted`. `[agents.<name>] trust_keys`
  in `pastor.toml` sets the keys (Claude's, `Down` then `Enter`, are built
  in). `pastor trust list [--json]` and `pastor trust remove <machine>
  <repo>` show and revoke it.
- Events carry an optional `detail` object for what they add beyond their
  ids; records without one are unchanged.

### Changed

- Every command that talks to or reloads the head pings it once, first, and
  acts on that answer throughout. A head that holds the socket but does not
  answer is an error (`head_unresponsive`) on all of them, where `tick`,
  `job list`, `task list|show`, `flock list|remove` and `job enable|disable`
  used to take it for no head and work offline next to it, and `machine add`
  and `connector install|link|uninstall|unlink` made their change and only
  warned. `task prune` answered `daemon_unresponsive` for this; it now
  answers `head_unresponsive` like the rest.
- `pastor machine list` opens with a line about the head (its pastor and
  herdr versions, its host, the number of machines, and which machine it is
  when it is one) instead of a first table row named `pastor`, and has a
  FLOCK column after HOST. The head's own machine comes first. `--json`
  keeps its shape, with `flock` on each machine.
- `machine add|remove` and the new flock commands edit `flock.toml` in place,
  keeping comments and layout; they used to rewrite the whole file.
- The database is schema 6. Schema 4 stores each task's flock; rows from
  before flocks join the default flock. Schema 5 adds the `trusted_repos`
  table and a `trust_sent` flag on each task. Schema 6 stores whether the
  task's agent has been seen at work since its prompt (`activity_seen`).

### Fixed

- An agent that exits between turns (a human typing `/exit` once the work
  is done) leaves a running task `done`, not `failed` with "agent process
  exited". An exit while starting, blocked or working still fails the task.
- `pastor task retry` of a failed worktree task no longer fails with git's
  "fatal: '<path>' already exists": the retry reopens the old task's
  checkout with `worktree.open`, on its branch, when the old task made that
  checkout, it is still on disk at the same path, the old agent is gone and
  no agent is in its workspace (an earlier retry may be working there).
  Any other retry, a stale task's included, gets its own branch and
  worktree.
- A task whose agent finished its work and sat idle, waiting for input, no
  longer stays `running` until the session ends when the daemon restarted,
  or the flock or settings were reloaded, while the agent worked: whether
  pastor saw the agent at work is stored with the task instead of held in
  the machine actor's memory, so the new actor settles the idle agent as
  `done`.

## 0.3.0 - 2026-09-25

### Changed

- The README is a short introduction with a recorded demo; the full reference moved to `docs/manual.md`. `make demo` records the gifs with vhs.

### Added

- An agent skill, `skills/pastor/SKILL.md`, for agents that drive pastor or
  were dispatched by it. `pastor --skill` prints the copy built into the
  binary, and `pastor --help` points agents at it.
- Plugins: a directory with a `pastor-plugin.toml` that provides a connector,
  event hooks, or both. `pastor plugin install|link|uninstall|unlink|list|run`
  manages them; secrets live in `~/.config/pastor/plugins/<id>/.env` and are
  redacted from the capped run logs under `~/.local/state/pastor/runs/`.
- Process connectors, poll and stream: a job names a plugin's connector with
  `[connector] use = "<id>"`, and `pastor serve` and `pastor tick` run it.
  A stream keeps a batch until a run has persisted it; a standalone
  `pastor tick` refuses stream jobs, which need `pastor serve`.
- Event hooks: a plugin's `[[events]]` commands get each matching event on
  stdin, one plugin at a time in event order, from a bounded queue.

- `pastor machine list` adds a HOST column (ssh target, `local`, or a command
  machine's program) and a first row for the head itself, with its hostname
  and local herdr version. `--json` is now `{"head": {...}, "machines": [...]}`
  instead of a bare array.
- Without `pastor serve`, `pastor machine list` probes each machine directly
  instead of printing the flock file with no live data. CHANNEL reads
  `probed`, `server down`, `unreachable` or `error`. `pastor machine status`
  is folded into it.
- `pastor machine list` has a PASTOR column after HERDR: the pastor installed
  on each machine (`-` when there is none or it cannot be known), and the
  head's own version on the head row. The head asks each machine once per
  connect; `--json` carries it as `pastor_version`.
- `flock.toml` and `pastor.toml` are reloaded while `pastor serve` runs:
  machines are added, removed or replaced and new timings and defaults apply
  without a restart. `pastor tick` reloads them too.
- `close_done_after` in `pastor.toml` (default `15m`, `never` disables it):
  pastor closes a done task's pane after the grace period, removes the
  worktree it created when it is clean, and keeps a dirty one with a note on
  the task. Failed, blocked and stale tasks are never closed on their own.

### Changed

- `pastor tick` under `pastor serve` spawns its runs, so a slow connector no
  longer holds the scheduler.
- Item fields put into a job's `repo` or `branch` may not hold path
  separators, `..`, a leading `-` or control characters; such an item is
  skipped and reported.

### Removed

- The old spellings `pastor run`, `pastor list`, `pastor attach`,
  `pastor reload` and `pastor machine status` are gone. Use
  `pastor task run|list|attach`, `pastor job reload` and
  `pastor machine list`.

### Fixed

- A dispatch no longer fails when the new pane's shell is still starting.
  herdr answered `agent.start` with `agent_pane_busy` about a second after
  `workspace.create`, and pastor failed the task at once; it now retries up
  to 5 times, 500ms apart, and only then fails with herdr's message plus
  `after 5 attempts`.
- `pastor serve` now also shuts down cleanly on SIGTERM and SIGHUP, not just
  SIGINT (ctrl-c). systemd counts all three as a clean exit; the head used to
  handle only SIGINT, so a SIGTERM (a plain `kill`, `systemctl stop` of a
  wrapper, a stray signal) killed it silently, left the socket file behind,
  and the `Restart=on-failure` unit never came back.
- The `pastor.service` and `herdr.service` templates now use
  `Restart=always` instead of `Restart=on-failure`, so a head or herdr
  server killed by a signal systemd treats as clean restarts too.
  `systemctl --user stop` still stops it for good.
- `pastor task prune` no longer deletes a worktree task whose checkout may
  still be on disk. It keeps the row, names it, and points at
  `pastor task close t-N --remove-worktree`, which clears the recorded
  workspace so the next prune takes it. `--json` reports the kept ids as
  `kept_worktrees`.

## 0.2.0 - 2026-09-25

### Added

- Scheduled jobs: a job is a TOML file in `~/.config/pastor/jobs/`, run on an
  `every` interval or a cron schedule against a connector (`clock` for now).
  Each new item becomes one task; items already seen never fire twice.
- `pastor job list|enable|disable|run` to manage job files, and
  `pastor tick [--dry-run] [--job]` to run or preview a scheduler pass.
- An events log: `pastor serve` appends task, job and machine events to
  `~/.local/state/pastor/events.jsonl` (rotated by size), and
  `pastor events [--follow] [--task t-N] [--json]` reads it, even with the
  daemon down.
- `pastor setup systemd [--herdr]` installs the pastor or herdr user unit,
  checks that lingering is on, and tightens config/state file permissions.
- `pastor task show t-N` prints one task in full, and `pastor task run
  --agent-arg` plus a `[defaults] agent_args` key pass flags such as
  `--model` to the agent.
- CI: `make check` and `make test-machine` run on every pull request and on
  `main`.

### Changed

- `pastor flock add|remove|list|status` is now `pastor machine ...`;
  `flock.toml` keeps its name.
- Task and job commands move under `pastor task run|list|attach` and
  `pastor job reload`. The old top-level `run`, `list`, `attach` and `reload`
  stay as hidden, deprecated aliases for this release.
- `pastor task list` (formerly `pastor list`) shows only live tasks by
  default; pass `--all` to include closed ones too.
- `pastor setup systemd` gained `--enable`, `--start`, `--now` and `--stop`
  to choose the systemd action, plus `--yes` to skip the confirmation
  prompt for scripted and remote use.

### Fixed

- Tasks now reach `done` against herdr 0.9.1. herdr reports no
  `completion_seq`; pastor now uses `state_change_seq` past the prompt's
  reply, requires that it saw the agent working or blocked after the prompt,
  and confirms after the `settle` window. Before, finished agents stayed
  `running` for ever.
- A job run whose task insert fails is reported as a failed run, is not
  counted as a connector failure, and keeps its cursor; runs of one job are
  serialised and queued in command order; an elapsed backoff makes the job
  due at once.
- The store refuses a `meta` table that lost its schema version, creates any
  job tables missing from a current-version database, and reports a corrupt
  failure count against its own column instead of wrapping it.
- The CLI tells a busy head from a missing one: a request that connected but
  timed out reports `timeout` and says the head may still finish it; only a
  refused or missing socket says `pastor serve` is not running.
- The private ssh `ControlPath` directory is created before any ssh command
  that can start a master, including the daemonless CLI paths.
