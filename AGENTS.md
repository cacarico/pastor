# Working on pastor

Read this first, then `docs/manual.md` (how it works today; `README.md` is
the short version). The code is the source of truth for behaviour, and the
manual describes it; when they disagree, fix one and say which.

pastor queues tasks, schedules jobs and runs coding agents in herdr panes on
the machines of a flock. It also makes those machines ready and keeps them so
(harness installs at pins, logins, trust, bundles in agents' homes, permission
profiles rendered per harness, drift), and places workdir sets in an agent's
working folder. [The map of that code](#the-code-for-readiness-and-workdir-sets)
is below, after the conventions.

## herdr facts that shaped the code

Verified against herdr 0.9.1 (protocol 22) and its source. They are written
down here because getting them wrong cost a day.

- herdr's API server answers one request per socket connection and closes.
  Only `events.subscribe` stays open. pastor opens a connection per request
  over one multiplexed ssh master (`ControlMaster=auto`, ControlPath under
  the state dir, `ControlPersist=600`) plus one long-lived events connection.
- `agent.start` returns before the agent process is up. `agent.prompt`
  answers `agent_not_ready` while it launches, and also when the agent has
  exited. pastor polls `agent.list` until the agent is ready (30s bound)
  and fails fast if it vanishes or exits.
- An agent stuck on its own startup question (Claude's folder trust dialog)
  shows `agent_status: blocked` with `launch_pending: true`; herdr's
  `agent.prompt` checks `blocked` before `launch_pending`, so it answers
  `agent_blocked`. pastor marks the task `blocked` without prompting, and
  once the block clears the agent goes straight to active and the pending
  prompt is sent.
- herdr's Claude manifest (`live_blocked_form`) reads every form with
  `Enter to confirm` and `Esc to cancel` under the last rule as `blocked`,
  so each of Claude's startup dialogs is a startup block to pastor, one
  after the other. `Actor::auto_answer` reads the pane and answers the
  first dialog it knows (`Agents::dialogs`), once per task and dialog.
- There is no `completion_seq` in herdr 0.9.1. `agent.list` entries carry
  `agent_status` and `state_change_seq`, a server-wide counter stamped on an
  agent each time its detected state (idle, working, blocked, unknown)
  changes. `done` is idle after a working or blocked spell that nobody has
  looked at yet; once someone focuses the pane it reads `idle`, with no new
  sequence. Subscription events carry the status only, no sequence. So pastor
  takes the sequence from the `agent.prompt` reply as the task's baseline
  (column `last_completion_seq`, name kept for the schema), or, for a prompt
  sent through Claude's inbox socket, which has no reply from herdr, from the
  `agent.list` read just before the send, with no activity counted yet; and
  it calls a task
  done when pastor has seen the agent `working` or `blocked` since the prompt
  (event or `agent.list`) and, after `settle`, `agent.list` shows it idle or
  done at a sequence past that baseline and unchanged since it was first seen
  idle. The sequence alone is not proof of work: `unknown` bumps it too, so
  `idle -> unknown -> idle` would pass. herdr's own `agent.prompt --wait` makes
  the same two checks (`prompt_activity_statuses`, then
  `after_state_change_seq`). The activity flag is stored with the task
  (`tasks.activity_seen`, since schema 6), so a restart keeps it. Unreleased herdr adds `completion_seq` in the same sequence;
  pastor prefers it when present, with no activity needed.
- herdr has no state for an agent that ended its turn on a question: it is
  idle, exactly like one that finished. So before confirming `done`,
  `confirm_pending_done` reads the pane (`agent.read`, 100 lines) and
  `task::trailing_question` looks for a last `●` message ending in `?`; if so
  the task goes `blocked` with its baseline moved to that idle, and
  `next_state` keeps a blocked task at that same sequence where it is.
- herdr also shows Claude idle while it waits on a background shell it
  left running at the end of its turn (footer: `1 shell still running`,
  or a bare `· 1 shell ·` segment from Claude 2.1.3xx);
  Claude picks the turn up when the shell ends. The same pane read
  (`task::background_shell_running`) keeps such a task pending and running,
  and looks again after the next settle window.
- herdr reads agy idle in a long thinking pause and while it waits on a
  command it started, and Codex idle and ready at its folder-trust dialog.
  So a task whose agent is not Claude is done only on its `pastor task done`
  (`PaneEnd::NotEnded` blocks it otherwise, unless its `summary` is `off`),
  agy's `· N task` footer keeps it running (`task::tasks_running`), and a
  prompt to such an agent waits while its pane shows a startup question
  (`Agents::startup_markers`).
- A herdr error reply is an API error with a code, never a dead connection.
  Only EOF before a reply, spawn failure or a non-zero exit with no reply are
  transport failures, and only those make a machine `lost`.
- `pane.close {pane_id}` closes the pane and its agent, and closing a
  workspace's last pane closes the workspace. `worktree.remove
  {workspace_id, force}` deletes the checkout and closes its workspace, so
  `task close --remove-worktree` calls it instead of `pane.close`, never
  after. Codes: `pane_not_found`, `dirty_worktree_requires_force`,
  `not_linked_worktree` (a plain workspace), `workspace_not_found` (an
  unknown id). `make smoke` checks them.
- `pastor bridge` has three shapes, all reading request lines on stdin and
  answering on stdout, run over ssh by an `authorized_keys` line that fixes
  the command (`src/bridge.rs`, `src/authkeys.rs`): plain (a remote CLI's way
  to the head, every request passed on unread); `--agent --machine <name>`
  (the agents on that machine: `answer_agent` passes only a ping, the task
  list of the machine's flock and show, read and done for a task placed on
  it); and `--shepherd --machine <name> --gen <n>` (a pull machine's headless
  serve: `answer_shepherd` adds claims, task and bootstrap reports, saved
  trust, job submits pinned to the machine and its own events, and fetching
  an artifact from the head's cache, whose bytes follow the reply line). The
  shepherd bridge asks the head before every request whether enrollment `n`
  still speaks for the machine, and everything else it refuses; adding a
  request to either list is a security decision, so it needs a test that the
  rest stays refused.
- `machine_state` is a table of the store (`pastor.db`), one row per machine:
  how its bootstrap ended (`pending`, `ok` or `failed`), the isolation, the host
  key pinned, the versions seen and its enrollment generation. The head writes
  it (`Fleet::report_bootstrap`, from a report or a push) and reads it before
  placing a task (`Fleet::bootstrap_ready`): a machine with no row is ready,
  one that is `pending` or `failed` takes nothing. It is the head's own record
  and never goes to the machine; `machine remove` deletes it.
- `pastor join` (`src/bootstrap/pull.rs`) is the other end of a join token: it
  runs on the joining machine, before any runtime of its own, and handles a
  secret. The token's key must reach ssh only as a file in a `PrivateDir` (0700,
  removed on drop and by the signal handler), never in argv, a log line, an
  error message or a file that is kept; `Token`'s `Debug` redacts it and every
  test in `bootstrap/tests/joining.rs` that touches it checks that. Keep it so:
  a new message that formats the token, or a new file written before the
  `PrivateDir` goes, is a leak. A join leaves its outcome in
  `<state>/bootstrap.json`; the shepherd (`Puller::send_bootstrap_report`) sends
  it until the head answers.
- `remote-api-bridge` is a plain stdio forwarder to the local socket, so a
  bridge that exits with `command not found` means herdr is not on the PATH
  of a non-interactive shell on that machine.

## Conventions

- `make check` is the gate: fmt check, clippy with warnings as errors, the
  full suite. Run it before every commit. `make help` lists the rest.
- `make portability` is a `cargo check` for the musl and FreeBSD targets
  (the list is `PORTABILITY_TARGETS` in the Makefile), which finds code that
  only compiles against glibc on x86_64. It is not part of `make check`: run
  it on a change to platform code (cfg gates, libc, sockets) or a
  dependency. It needs `cargo-zigbuild`, `zig` and the Rust targets, names
  what is missing in one line and installs nothing.
- CI (`.github/workflows/ci.yml`) runs `make check`, `make test-machine`,
  `make portability` and `make test-ssh` (the CLI against a head over a real
  ssh, through a throwaway sshd on localhost; `tests/real_ssh.rs`) on every
  pull request that touches code (`paths:` skips docs-only diffs), on every
  push to `main` that touches code or the changelog, and by hand
  (`workflow_dispatch`).
- Pull requests merge with a merge commit (`gh pr merge --merge`) and no
  other way: not `--rebase`, not `--squash`, and not a push of the PR's head
  straight to `main`. PR CI runs on GitHub's merge of the PR into `main` as
  `main` was when the run started; `main` can move before the PR merges, so
  two PRs green on their own run can still break it together. That is why
  push to `main` runs the checks again, on the tree `main` really has.
- The repository is public. `.github/workflows/gitleaks.yml` scans the whole
  history of every ref on every pull request, on push to `main`, and weekly,
  and `make leaks` runs the same scan here. It does not run on push to other
  branches, to save runner time; GitHub's push protection is the front line
  for a secret landing on a branch with no open PR yet, until a PR opens or
  the weekly sweep runs. Nothing from the fleet goes into a commit: no
  addresses, hostnames, user names, tokens or home paths; examples use
  placeholders such as `user@pi-1`.
- Dependabot (`.github/dependabot.yml`) opens weekly version updates for
  cargo and GitHub Actions, minor and patch grouped into one pull request per
  ecosystem, plus security updates; malware alerts and private vulnerability
  reporting are repository settings. Its pull requests pass CI like any other.
  The file is the same in the public repo, which gets the same pull requests;
  there `.github/workflows/dependabot-close.yml` closes them with a note,
  since the update arrives with the sync.
- Nothing in the suite talks to a real herdr. `make smoke SESSION=s` runs the
  opt-in test against one on the same host; do it on a fleet machine before
  trusting a change to the transport or dispatch. `make smoke-profiles`
  runs a live review task per agent through a head running the build under
  test; do it before trusting a change to permission profiles.
- `make mutants` runs cargo-mutants over the transport, head, dispatch and
  config (`.cargo/mutants.toml`); a full run is hours, so it is not a PR
  job. `make mutants-diff` runs only the mutants in a branch's own diff. An
  equivalent mutant is skipped with `#[cfg_attr(test, mutants::skip)]` and a
  comment saying why.
- Work on a branch, open a pull request, never push `main`.
- Commit messages: conventional prefix, plain subject, a body that explains
  the why. No `Co-Authored-By` or other trailers.
- `skills/pastor/SKILL.md` is built into the binary. Change it with the CLI:
  a unit test fails when any Markdown file under `skills/`, `README.md` or
  `docs/manual.md` names a command or flag that does not exist (fences
  tagged `text`, `toml` or `json` are skipped), or a SKILL.md's frontmatter
  is off. Every `toml` fence in those docs must parse, and loads as the file
  its content shows (flock, job, orchestrator, provisioner manifest,
  connector manifest, client.toml, else pastor.toml) unless it starts with
  `# fragment`. Another fails when a command or argument has no help. The other
  skills (`skills/spec/`) are not built in; they install with the repo as the
  Claude Code plugin `pastor` (`.claude-plugin/plugin.json`, whose version
  follows `Cargo.toml`). `tests/cli.rs` runs the spec skill's example plan's
  first task against the fake herdr.
- Vocabulary is fixed: machine, flock, head, job, task, connector, agent,
  shepherd (a headless `pastor serve` on a machine whose head is elsewhere),
  orchestrator (an agent pastor runs to drive the others, from
  `orchestrators/<name>.toml`), pull machine (`pull = true`: its
  shepherd asks the head for work, the head never connects to it) and
  provisioner. What checks and sets up a machine for agents is a
  provisioner; it is installed and run like a connector, and it is never
  called a plugin. Agents are never renamed; hosts are not "sheep". "Plugin" is kept free
  for code that changes how pastor itself behaves; what installs a connector
  command or event hooks is a connector. The Claude Code plugin `pastor` that
  ships the skills is Claude Code's word, not pastor's.
- Runtime CLI errors are JSON on stderr with a stable code and exit 1; clap
  usage errors stay plain text with exit 2.
- Rust edition 2024, toolchain from mise. No new runtime dependencies without
  a reason in the commit body.
- A changelog entry is its own file, `changes/<branch>.md` (slashes as
  dashes, Keep a Changelog `###` subsections inside), never a line in
  `CHANGELOG.md`, so two pull requests never conflict over it. `make check`
  (`scripts/changelog.sh check`) fails on an Unreleased section or a
  malformed change file; `changes/README.md` has the format.
- Releases are tagged `vX.Y.Z` on `main` with a signed tag. `CHANGELOG.md`
  gets one section per release, written by `make changelog VERSION=X.Y.Z`
  on the release pull request from `changes/*.md` in merge order, which
  also deletes them; the tag push runs
  `.github/workflows/release.yml`, which builds the tarballs and drafts the
  GitHub release with that section as notes. `CONTRIBUTING.md` has the
  release policy.

## The code for readiness and workdir sets

Each file starts with a `//!` comment that says what it is for; this is where
to start reading.

Making a machine ready:

- `src/bootstrap/`: `pastor bootstrap` on the machine (`steps.rs`, in order,
  each checking before it acts; `facts.rs` probes through a `Runner` so tests
  script the answers; `outcome.rs` is the report the head keeps). `push.rs` is
  the head setting a machine up over ssh (`machine add --bootstrap`, `machine
  bootstrap`); `pull.rs` is `pastor join` on a new pull machine.
- `src/join.rs`: join tokens on the head, and `bridge --join` for the machine
  that redeems one.
- `src/provision/`: provisioners, the checks a machine must pass for its
  agents (`catalog.rs` has the built-in ones, `run.rs` runs them for `machine
  check`, `agy.rs` writes agy's rules before a profiled launch).
  `src/machine_check.rs` is the CLI side of `machine check`.
- `src/harness_manifest.rs` and `src/harness_install.rs`: what pastor knows of
  each harness CLI as data (binary, version, login check, install sources) and
  `machine install` at a flock's pins. `src/artifacts.rs` is the checked cache
  of pastor, herdr and harness files the head puts on machines;
  `src/fleet_update.rs` is `fleet update`.
- `src/login_cli.rs`, `src/trust_cli.rs`, `src/limit_cli.rs`: sign-in of an
  agent, saved folder trust, and the accounts out of usage.
- `src/profile_render.rs` and `src/account_rules.rs`: what a permission
  profile becomes for claude or agy, and the account rules that undo it.

Bundles in agents' homes (`src/bundle/`):

- `manifest.rs` (`pastor-bundle.toml`), `store.rs` (where bundles and their
  versions are kept, how one is added), `secrets.rs` (refuses a credential at
  `bundle add`), `cli.rs` (`pastor bundle`).
- `plan.rs` renders what one machine gets; `slots.rs` says where it goes in
  each harness's home; `apply.rs` applies a plan on the machine; `push.rs` is
  the head doing that over ssh; `keep.rs` and `status.rs` keep homes in line
  (`bundle_apply`) and remember what was applied last.

Workdir sets (`src/bundle/workdir*.rs`): files placed in an agent's working
folder, with a ledger of what was written on the machine itself.

- `workdir.rs`: the set, its checks, placing and removing on a machine, and
  the ledger (`~/.local/state/pastor/workdirs/`).
- `workdir_cli.rs`: `pastor workdir`; `--local` runs `workdir.rs` on the
  machine, otherwise the request goes to the head.
- `workdir_push.rs`: the head placing, removing and checking sets on ssh and
  local machines, and keeping its record of them (`workdir list`).
- `workdir_keep.rs`: how each set stands on its machine (the states `ok`,
  `drift`, `conflict`, ...), read by the head.
- `workdir_report.rs`: what a pull or command machine owes the head after a
  `--local` place or remove, queued and sent with its next request.
- `workdir_stored.rs`: bundles with `target = "workdir"`, turned into one set
  per kind of agent before each start. `workdir_task.rs`: a task's own set
  (`task run --workdir-set`, a job's `workdir_set`).
- `workdir_git.rs`: keeps placed files out of git through the repo's
  `info/exclude`.

## Known gaps

Still open as of the last review; none of them blocks normal use.

- A 30s unread events socket can overrun herdr's retained history; pastor
  reconnects and reconciles, at the cost of a `machine.lost` blip.
- `pastor machine list` without a head does unbounded connect/ping/list on the
  CLI path, one machine at a time.
- A stream connector starts on its job's first run, not at daemon start,
  and `pastor job reload` (which every `connector install|link|uninstall|unlink`
  sends) rebuilds the catalog, restarting every stream connector.
- `only_own` decides ownership by reading the task's job file for
  `connector.use`; a job file edited or removed after its tasks were made
  changes who owns them.
- A hand edit of `flock.toml` leaves herdr's saved-machine list stale; only
  `machine add|remove --herdr` touches it. Candidate: `machine sync --herdr`,
  or reconciling the two lists on head start. Going further, herdr could own
  machine identity, with flock entries referencing its saved-machine labels
  and carrying only pastor's extra fields.
- `tasks.id` has no `AUTOINCREMENT`, so an id can be reused after a rolled
  back insert of the newest task; ids appear in agent names, branch names
  (`pastor/t-<n>`) and `seen.task_id`. `task prune` never deletes the newest
  row for this reason. The real fix is a table rebuild in a later schema.
- `pastor machine open` should detect a nested herdr and say so instead of herdr
  refusing to start.
- The whole dispatch pass runs under the dispatch lock, so slow agent
  readiness delays `job list`, `tick`, `job reload` and `task run` too. Move
  readiness waits out of the lock.
- Orphans (agents named `t-N` that no open task owns) are found only by
  reconcile, so they appear up to `reconcile_every` late, and the rule
  assumes one pastor owns the `t-N` names on each herdr. `task close t-N`
  for an orphan with no row finds it through the machines' last reconcile.
- `task close --remove-worktree` never sends `force`; a dirty checkout is an
  error until someone commits or cleans it. A `--force` would be a separate
  decision.
- A `remove_worktree` task's worktree goes from `auto_close_stopped`, which
  only looks at failed and stale tasks still holding a pane: one that holds
  none (its pane closed before the setting, or by someone else) keeps its
  checkout until `task close --remove-worktree`.
- `tests/transport.rs` `command_transport_talks_to_fake_herdr` failed once
  under a loaded `make check` (the stdio fake-herdr closed before replying)
  and passed on every rerun.
- With the head set, `tick` and `job reload` go to the head only; the
  headless serve (`src/shepherd.rs`) is reached by the other `job` commands
  (`main.rs` `names_local_job`). Its own `job.failed` and `task.queued` events are only logged, never
  written to an events log or heard by its hooks. Its jobs share the head's
  seen table by name, so a head job of the same name with the same item key
  refuses the item (`job_task_refused`).
- Cron minutes that do not exist on a spring-forward day are skipped;
  Vixie cron runs them instead.
- `pastor login` keeps its pending logins in the head's memory
  (`login::Logins`): a head that restarts forgets them, and their
  `login/<agent>` panes stay open until someone closes them.
- A session orchestrator's restart closes the old agent's pane even when
  it only ended its turn, so whatever it left on screen is gone; the
  handover note is what carries over. Its quota wait reads Claude's
  messages only.
- The orchestrator runner ticks at pastor.toml's `tick` as the head started
  with; a later edit of `tick` reaches it only on a restart
  (`max_orchestrators` is read on each run).
- An orchestrator agent's branch is pastor's usual `pastor/t-<n>`, not the
  spec's `orchestrator/<name>-<n>`.
- With a remote head (`pastor head set`), `task run` fills what its flags
  leave out from the built-in defaults, not the head's `[defaults]` timeout
  and place: `settle_run` (`src/daemon.rs`) only overwrites a spec's timeout
  and place when an ask or the flock sets one, trusting the spec's own
  otherwise, which for a remote client is `PastorConfig::default()`
  (`src/main.rs::run`), not the head's file. Every other command that reads
  the fleet goes to the head (`remote_route` in `main.rs`); only `machine
  authorized-key` fails with `remote_head_unsupported`.
- A task pinned to a machine outside its flock (the machine was moved
  with `pastor machine move`, or removed) stays queued forever: the
  scheduler logs it once (`warned_queued` in `src/scheduler.rs`) and never
  fails or re-flocks it.
- A profiled opencode task runs with `OPENCODE_DISABLE_PROJECT_CONFIG=1`,
  so it ignores the repo's own `opencode.json` and `.opencode/` (agents,
  commands, MCP servers, not only permissions); only `AGENTS.md` or
  `CLAUDE.md` are passed back as instructions.
- agy's `artifactReviewPolicy` defaults to `asks-for-review`, which can
  stop an interactive session to review a plan artifact; pastor's dialog
  reading does not know that prompt. A machine that runs agy tasks should
  set it to `agent-decides` or `always-proceed`. And agy's rules are one
  file per machine, so an agy machine runs one profile; the agy
  provisioner writes that profile's rules there before each launch.

- The agy provisioner (`src/provision/agy.rs`) runs only just before a
  profiled agy launch, not at `machine add` or on reconcile, so a machine's
  agy settings match its own profile only from its first such task on. It
  never removes a rule, so a machine whose own profile narrows keeps the
  old denies until someone removes them by hand. agy can write back a copy
  of its settings between pastor's write and its start; nothing makes agy
  reload. A pull machine, and a headless serve's own machines, provision
  from that serve's `pastor.toml`, not the head's profiles.

## Where things live

```
~/.config/pastor/pastor.toml      tick, settle, reconcile_every, version_every, close_done_after, close_failed_after, defaults
~/.config/pastor/flock.toml       machines
~/.config/pastor/jobs/<name>.toml one job per file
~/.config/pastor/orchestrators/<name>.toml one orchestrator per file (kind scheduled or session)
~/.config/pastor/client.toml      [head]: a head on another machine (`pastor head`)
~/.local/state/pastor/pastor.db   tasks (schema 20), token usage, seen keys, usage limits, event seq, job state (SQLite)
~/.local/state/pastor/pastor.sock daemon socket
~/.local/state/pastor/fleet.lock  offline fleet edits and a starting `pastor serve` take turns on it
~/.local/state/pastor/shepherd.db a headless serve's job state, seen keys, head event cursor, its pull machine's trusted repos
~/.local/state/pastor/events.jsonl events log, rotated to events.jsonl.1
~/.local/state/pastor/watch/<name>.json `pastor watch` cursors
~/.local/state/pastor/serve.log   background `pastor serve` log, rotated at 10 MB, 3 kept
~/.local/state/pastor/serve.json  running serve's pid, service manager, log
~/.local/state/pastor/ssh/        one ssh ControlMaster socket per machine
~/.config/systemd/user/*.service  from `pastor setup systemd [--herdr]`
~/.config/pastor/connectors/<id>/.env   connector secrets and settings
~/.local/share/pastor/connectors/<id>/  connector checkouts or links (PASTOR_DATA_DIR)
~/.local/share/pastor/connectors/.<id>.install.json  where `connector install` got it
~/.local/share/pastor/provisioners/<id>/  provisioners copied by hand; one with a built-in's id replaces it
~/.config/pastor/bundles/<id>/    bundles, each with its pastor-bundle.toml (`src/bundle/`)
~/.local/state/pastor/bundles/<id>/  the versions of a bundle kept, by content hash
~/.local/share/pastor/bundles/    on a machine: the bundle versions its homes link into
~/.local/state/pastor/applied/    on a machine: the ledger of what bundles placed, and backups
~/.local/state/pastor/workdirs/<key>/  on a machine: the ledger of the workdir sets placed in one directory, and backups (`src/bundle/workdir.rs`)
~/.local/state/pastor/artifacts/<name>/<version>/<target>/  pastor, herdr and harness release files for machines, sha256-checked (`src/artifacts.rs`)
~/.local/share/pastor/harnesses/<id>/<version>/  on a machine: a harness `machine install` put there (`src/harness_install.rs`)
~/.local/share/pastor/claude-marketplace/<home>/  on a machine: the pastor-mcp plugin with a Claude home's MCP servers (`src/bundle/plan.rs`)
~/.local/state/pastor/connectors/<job>/ connector scratch per job (PASTOR_CONNECTOR_STATE_DIR)
~/.local/state/pastor/connectors/@<id>/ connector scratch for hooks and runs with no job
~/.local/state/pastor/runs/<job>/       connector run logs, 256 KiB each, newest 20 kept
~/.local/state/pastor/runs/@<id>/       hook logs (and `connector` runs with no job)
~/.local/state/pastor/orchestrators/<name>/ state.json, note, scripts' scratch/ and runs/
provisioners/<id>/pastor-provisioner.toml  the built-in provisioners (arch, debian, macos, raspberry-pi-os), embedded by `src/provision/catalog.rs`
skills/pastor/SKILL.md            agent skill, in the repo; `pastor --skill` prints it
skills/spec/                      plan-for-the-flock skill, its plan format and example
changes/<branch>.md               a pull request's changelog entry, gathered at release
scripts/changelog.sh              check, notes and gather for those files
.claude-plugin/plugin.json        makes the repo a Claude Code plugin named pastor
```

The schema number above is `SCHEMA_VERSION` in `src/store.rs`; a schema bump
updates it here too.

`PASTOR_CONFIG_DIR`, `PASTOR_STATE_DIR` and `PASTOR_DATA_DIR` override these;
tests always set them to temp dirs (`Paths::new` puts the data dir under the
state dir).

In the code: `task retry|close|prune|done` are `src/task_cli.rs` (CLI), the
`TaskRetry|TaskClose|TaskPrune|TaskDone` arms in `Daemon::handle`,
`Store::{insert_retry, close_task, prune}` and `MachineCommand::{Close, End}`
in the actor (`Task::ended` keeps an ended task done, and lets auto-close
skip its sequence checks; the fleet guard lets an agent through only for its
own task, `IpcRequest::ends_own_task`); auto-close of done tasks after `close_done_after` is
`Actor::auto_close_done`, run after each connected reconcile through the same
`run_close` (`CloseBy::AutoClose`); the panes of failed and stale tasks and
orphans whose agents stopped go after `close_failed_after` in
`Actor::auto_close_stopped`, which keeps the rows failed; both skip a task
whose spec keeps its pane (`DispatchSpec::keeps_pane`, settled at queue time
by `Fleet::settle_keep_pane`), and `machine::count_live` leaves such a task
out while it is done; orphan detection is
`machine::orphan_agents`, used by reconcile and by the head-less probe in
`machine list`.

Orchestrators are `src/orchestrator.rs` (files, state, `Runner`, which the
head spawns beside the scheduler) and `src/orchestrator_cli.rs`; the guard
for their scripts is `Daemon::handle_as` (`ipc::Caller`,
`FROM_ORCHESTRATOR_FIELD`) and the `caller().orchestrator` branch of the
CLI's own guard in `main`.
