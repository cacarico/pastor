### Added

- `pastor login <machine> <agent>` signs an agent in on a machine, guided.
  The head opens a `login/<agent>` workspace there with the agent's env,
  types the interactive login of its harness manifest (`[login] start`:
  `claude auth login`, and agy, whose way in it picks), and answers with
  the URL and device code the pane shows. It watches the pane and runs the
  agent's login check; when that passes, it closes the pane and clears the
  signed-out mark with `agent.signed_in` (`by: login`). `login.pending`
  says when a URL or code shows, `login.closed` when a login ends
  unsigned (pane closed, 30 minutes, or `pastor login cancel`).
  `pastor login list --json` shows each pending login, and `pastor login
  code` types a code given back in the browser into the pane. pastor never
  reads or keeps the credential, nor the code.
- Harness manifests take `[login] start`, `pick` and `keys`, `code_after`
  and `paste`.

### Changed

- The login check `pastor agent signed-in` runs is the agent's own
  `login_check`, else its kind's manifest `[login] check`, else Claude's
  built-in: agy now has one.
- The IPC protocol is 44 (`LoginStart`, `LoginList`, `LoginCode`,
  `LoginCancel`).
