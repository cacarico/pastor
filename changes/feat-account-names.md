### Added

- A flock's `accounts` can be a map from the kind to the account `machine
  check` expects, such as `accounts = { github = "alice", claude =
  "alice@example.com" }`; the list form still loads and names none. With a
  name, the `gh-auth` row fails when another account is signed in to
  github.com, and each Claude agent gets a `claude-account` row that fails,
  naming both accounts, when it is signed in as someone else. A flock without
  a name checks nothing new.
