### Added

- For a Claude or agy task in a folder pastor trusts on the machine (its
  repo, the worktree of one, or the folder a task with no repo starts in),
  pastor writes the trust into the harness's own store just before the agent
  starts, so the folder-trust dialog does not show: `hasTrustDialogAccepted`
  for the folder in the agent home's `.claude.json`, or the folder in agy's
  `trustedWorkspaces`. The store goes to the machine on the command's standard input, not its
  command line, and is replaced under the lock Claude saves under. The write
  is a compare and swap that is checked again
  after half a second and made again, three times at most, if a running
  session rewrote the file; a store that is not there, a symlink and a file
  that is not JSON are left alone. The auto-answer of the dialog stays as
  the backstop.
