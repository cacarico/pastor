### Added

- `pastor join` on a new machine joins a head with a one-time token as a pull
  machine: it pins the head's host key, makes its keys, joins with the token's
  key (a file that is deleted whatever happens), fetches herdr from the head's
  cache, bootstraps, writes `[shepherd] machine`, installs the serve service
  and reports back, or keeps `bootstrap.json` for the machine's serve to send
  until the head has it. The token is read from stdin (`--token-stdin`),
  `PASTOR_JOIN_TOKEN` or the terminal, never an argument.
- `install.sh join` installs pastor and runs `pastor join` as the login user,
  handing the token over on a pipe. It uses no sudo and no package manager.
