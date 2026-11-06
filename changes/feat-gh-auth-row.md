### Added

- `pastor machine check` adds a `gh-auth` row for each host `gh` is logged
  in to, on a machine whose flock lists the `github` account. It is `ok` when
  the login works, says when the token expires when GitHub reports one, and
  warns when that is under 7 days away; a token `gh` says does not work is a
  `fail`. The token is never asked for or printed.
