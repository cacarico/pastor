# Working on pastor

Read this first, then `docs/manual.md` (how it works today; `README.md` is
the short version). The code is the source of truth for behaviour, and the
manual describes it; when they disagree, fix one and say which.

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
- There is no `completion_seq` in herdr 0.9.1. `agent.list` entries carry
  `agent_status` and `state_change_seq`, a server-wide counter stamped on an
  agent each time its detected state (idle, working, blocked, unknown)
  changes. `done` is idle after a working or blocked spell that nobody has
  looked at yet; once someone focuses the pane it reads `idle`, with no new
  sequence. Subscription events carry the status only, no sequence. So pastor
  takes the sequence from the `agent.prompt` reply as the task's baseline
  (column `last_completion_seq`, name kept for the schema) and calls a task
  done when pastor has seen the agent `working` or `blocked` since the prompt
  (event or `agent.list`) and, after `settle`, `agent.list` shows it idle or
  done at a sequence past that baseline and unchanged since it was first seen
  idle. The sequence alone is not proof of work: `unknown` bumps it too, so
  `idle -> unknown -> idle` would pass. herdr's own `agent.prompt --wait` makes
  the same two checks (`prompt_activity_statuses`, then
  `after_state_change_seq`). The activity flag lives in the machine actor's
  memory, not the store (a column would need a schema bump); after a restart
  an agent found working or blocked counts again, one found idle stays running
  until stale. Unreleased herdr adds `completion_seq` in the same sequence;
  pastor prefers it when present, with no activity needed.
- herdr has no state for an agent that ended its turn on a question: it is
  idle, exactly like one that finished. So before confirming `done`,
  `confirm_pending_done` reads the pane (`agent.read`, 100 lines) and
  `task::trailing_question` looks for a last `●` message ending in `?`; if so
  the task goes `blocked` with its baseline moved to that idle, and
  `next_state` keeps a blocked task at that same sequence where it is.
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
- `remote-api-bridge` is a plain stdio forwarder to the local socket, so a
  bridge that exits with `command not found` means herdr is not on the PATH
  of a non-interactive shell on that machine.

## Conventions

- `make check` is the gate: fmt check, clippy with warnings as errors, the
  full suite. Run it before every commit. `make help` lists the rest.
- CI (`.github/workflows/ci.yml`) runs `make check` and `make test-machine` on
  every pull request that touches code (`paths:` skips docs-only diffs), or
  by hand (`workflow_dispatch`). Since this project merges by fast-forwarding
  a PR's exact head sha to main, that sha was already checked on its PR, so
  push-to-main does not run `check` again; a direct push that skips a PR goes
  unchecked unless someone dispatches it by hand.
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
- Nothing in the suite talks to a real herdr. `make smoke SESSION=s` runs the
  opt-in test against one on the same host; do it on a fleet machine before
  trusting a change to the transport or dispatch.
- Work on a branch, open a pull request, never push `main`.
- Commit messages: conventional prefix, plain subject, a body that explains
  the why. No `Co-Authored-By` or other trailers.
- `skills/pastor/SKILL.md` is built into the binary. Change it with the CLI:
  a unit test fails when any Markdown file under `skills/` names a command
  or flag that does not exist, or a SKILL.md's frontmatter is off. The other
  skills (`skills/spec/`) are not built in; they install with the repo as the
  Claude Code plugin `pastor` (`.claude-plugin/plugin.json`, whose version
  follows `Cargo.toml`). `tests/cli.rs` runs the spec skill's example plan's
  first task against the fake herdr.
- Vocabulary is fixed: machine, flock, head, job, task, connector, agent.
  Agents are never renamed; hosts are not "sheep". "Plugin" is kept free
  for code that changes how pastor itself behaves; what installs a connector
  command or event hooks is a connector. The Claude Code plugin `pastor` that
  ships the skills is Claude Code's word, not pastor's.
- Runtime CLI errors are JSON on stderr with a stable code and exit 1; clap
  usage errors stay plain text with exit 2.
- Rust edition 2024, toolchain from mise. No new runtime dependencies without
  a reason in the commit body.
- Releases are tagged `vX.Y.Z` on `main` with a signed tag. `CHANGELOG.md`
  gets one section per release; the tag push runs
  `.github/workflows/release.yml`, which builds the tarballs and drafts the
  GitHub release with that section as notes. `CONTRIBUTING.md` has the
  release policy.

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
- `pastor open` should detect a nested herdr and say so instead of herdr
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
- `tests/transport.rs` `command_transport_talks_to_fake_herdr` failed once
  under a loaded `make check` (the stdio fake-herdr closed before replying)
  and passed on every rerun.
- Cron minutes that do not exist on a spring-forward day are skipped;
  Vixie cron runs them instead.

## Where things live

```
~/.config/pastor/pastor.toml      tick, settle, reconcile_every, close_done_after, defaults
~/.config/pastor/flock.toml       machines
~/.config/pastor/jobs/<name>.toml one job per file
~/.local/state/pastor/pastor.db   tasks (schema 3: retry_of), seen keys, job state (SQLite)
~/.local/state/pastor/pastor.sock daemon socket
~/.local/state/pastor/events.jsonl events log, rotated to events.jsonl.1
~/.local/state/pastor/ssh/        one ssh ControlMaster socket per machine
~/.config/systemd/user/*.service  from `pastor setup systemd [--herdr]`
~/.config/pastor/connectors/<id>/.env   connector secrets and settings
~/.local/share/pastor/connectors/<id>/  connector checkouts or links (PASTOR_DATA_DIR)
~/.local/state/pastor/connectors/<job>/ connector scratch per job (PASTOR_CONNECTOR_STATE_DIR)
~/.local/state/pastor/connectors/@<id>/ connector scratch for hooks and runs with no job
~/.local/state/pastor/runs/<job>/       connector run logs, 256 KiB each, newest 20 kept
~/.local/state/pastor/runs/@<id>/       hook logs (and `connector` runs with no job)
skills/pastor/SKILL.md            agent skill, in the repo; `pastor --skill` prints it
skills/spec/                      plan-for-the-flock skill, its plan format and example
.claude-plugin/plugin.json        makes the repo a Claude Code plugin named pastor
```

`PASTOR_CONFIG_DIR`, `PASTOR_STATE_DIR` and `PASTOR_DATA_DIR` override these;
tests always set them to temp dirs (`Paths::new` puts the data dir under the
state dir).

In the code: `task retry|close|prune` are `src/task_cli.rs` (CLI), the
`TaskRetry|TaskClose|TaskPrune` arms in `Daemon::handle`,
`Store::{insert_retry, close_task, prune}` and `MachineCommand::Close` in
the actor; auto-close of done tasks after `close_done_after` is
`Actor::auto_close_done`, run after each connected reconcile through the same
`run_close` (`CloseBy::AutoClose`); orphan detection is
`machine::orphan_agents`, used by reconcile and by the head-less probe in
`machine list`.
