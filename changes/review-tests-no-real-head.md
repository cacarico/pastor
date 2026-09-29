### Fixed

- Running pastor's test suite in an agent's pane on a fleet machine no
  longer sends the tests' commands to the real head: the tests ignore an
  inherited `PASTOR_HEAD` and run against their own temporary state.
