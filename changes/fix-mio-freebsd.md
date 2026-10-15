### Fixed

- The FreeBSD portability check builds again: mio 1.2.4 drops the
  `NOTE_PCTRLMASK` constant that libc 0.2.190 no longer has, which failed
  `make portability` (and CI's portability step) on every branch.
