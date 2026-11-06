### Changed

- The `signing` row of `pastor machine check` is a `warn`, not a `fail`, on a
  machine where signing was never set up: git's config sets neither
  `user.signingkey` nor `commit.gpgsign` and a commit does not sign. A
  configured signing that is broken still fails.
