### Added

- `pastor machine check` ends with a `signing` row. It makes a throwaway
  repo in a run directory of its own on the machine, commits to it signed
  as git there is set up to sign, checks the commit with `git verify-commit`
  and removes the repo. The row is `ok` when a commit signs and verifies,
  and `fail` when git is missing, the commit does not sign or the signed
  commit does not verify, with the last lines git said. It never reads a
  key, runs no hooks, and cuts off a signer that waits for a passphrase
  after a minute (an `error` row).
