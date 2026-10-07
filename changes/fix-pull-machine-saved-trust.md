### Fixed

- Saved trust now applies to a pull machine's tasks. Its headless serve
  fetches the (machine, repo) pairs `pastor trust add` saved on the head for
  that machine, and only those, answers the folder-trust prompt with the
  same marker check and once-per-task rule as the head, and its report
  makes the head emit `task.trusted` with the keys. Before, such a task
  stayed `blocked` until someone pressed the keys on the machine. IPC
  protocol 31 (`pull_trust`, and `trusted` on a `task_report`); a pull
  machine whose head is older leaves the prompt for a person, as before.
  Store schema 17: a task keeps the trust keys sent to it (`trust_keys`),
  so a pull machine reports the keys it pressed even after a restart with
  other keys in its config.
