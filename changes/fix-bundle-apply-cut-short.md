### Fixed

- A bundle apply cut short after pastor on the machine started it (a
  timeout, or an answer that is no report) now counts as having placed the
  plan's bundles. Before, it counted as never reached. When those bundles
  are taken off, the machine stays stale and held, and the removal is tried,
  instead of leaving files on it with nothing saying so. Only an apply that
  never reached pastor there (not on its PATH, ssh failed) still counts as
  placing nothing.
