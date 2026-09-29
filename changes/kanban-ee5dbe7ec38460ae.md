### Fixed

- A panic in one request no longer breaks every later one until restart:
  the head's store connection and its shared state recover a lock that the
  panicking request left poisoned instead of panicking again.
