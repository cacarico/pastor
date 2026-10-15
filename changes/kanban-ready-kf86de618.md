### Added

- The head reads each machine's room, its free disk under `$HOME` and `/tmp`
  and its free memory, when it connects and every minute after, and keeps
  the numbers on the machine's status with when they were read. A probe that
  fails keeps the last numbers and notes the error; it never makes the
  machine reconnect. Memory stays unknown on macOS, which has no
  `/proc/meminfo`. Nothing acts on the numbers yet: a floor on them comes
  separately.
