### Changed

- Test fixtures that look like secrets (an AWS key id, a GitHub token, a long
  token under a secret's name, a private key) are put together when the test
  runs, so a secret scanner that reads the source finds none. The tests check
  the same things. A link in the manual to a heading that does not exist now
  points at "How it works".
- The tests that use the fake herdr are built only with the `fake-herdr`
  feature, so `cargo check --all-targets` compiles without it.
