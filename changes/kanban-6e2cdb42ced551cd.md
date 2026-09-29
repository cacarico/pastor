### Added

- A usage limit is known on every machine. `account = "..."` under an
  `[agents.<name>]` table names the login the agent uses: every machine
  whose agent names the same account shares its limits, and an agent with
  none keeps a limit to the machine it was seen on (`<machine>/<agent>`).
  The head keeps the exhausted accounts (or one model of one) in a new
  `limits` table until their reset; a new task does not start on one: it
  starts on the first free model of its `fallback` list (`task describe`:
  `model: gpt (fallback 2 of 2; me exhausted until 03:00)`) or stays queued
  with `waiting: me exhausted until 03:00 (5-hour limit, seen by t-412)`,
  which `pastor queue` shows too. An orchestrator stopped on a quota records
  its account, and one whose account is exhausted starts no agent before
  the reset.
- `pastor limit list [--json]` and `pastor limit clear <account> [--model
  m]`; an agent pastor started may list but not clear. Events
  `agent.exhausted` and `agent.reset` (`by: time` or `hand`).
- `[limits]` in pastor.toml: `wait_under`, `rate_retries`, `rate_backoff`,
  `unknown_reset_wait`, `retry_after_no_credit`, `handover_lines`, all
  checked on load; only the two waits are used so far.
- IPC protocol 30 (`LimitList`, `LimitClear`, and a limit on a pull
  machine's `TaskReport`); an older head refuses `limit list|clear` with
  `head_too_old`.

### Changed

- The store is at schema 15: the `limits` table and `tasks.waiting_until`.
  An older store migrates on first open; a store at 14 is refused by an
  older pastor, so the release needs a release candidate.
