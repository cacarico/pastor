### Fixed

- The test that starts a head while an offline `flock remove` waits for the
  fleet lock no longer fails under load with `head_unresponsive`: it waits
  for the pastor process itself to hold the lock, not for the test's own
  lock descriptor in the child before it has exec'd.
