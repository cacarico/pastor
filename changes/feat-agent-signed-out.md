### Added

- An agent at its login screen, or whose login is gone, is signed out:
  pastor reads it at a task's start, when the agent goes idle and when it
  goes `blocked` (Claude's `Select login method:` and `Not logged in ·
  Please run /login`, agy's `You are currently not signed in`, or an
  agent's own `login_marker`). The task is held `waiting`, or moves to its
  next fallback model, instead of failing or prompting into the login
  screen, and the agent is marked signed out on that machine with
  `agent.signed_out`. Dispatch skips it there until the mark clears.
- `pastor agent signed-in <agent> --machine <m>` runs the agent's login
  check on the machine (`login_check`; Claude's `claude auth status` is
  built in) and, when it passes, clears the mark with `agent.signed_in`.
  `pastor limit clear` also clears it. `limit list`, `pastor queue`, `task
  describe` and `machine describe` say `signed out`.

### Changed

- The database schema is 20: the limits table keeps a `signed_out` flag.
  The IPC protocol is 36 (`AgentSignedIn`).
