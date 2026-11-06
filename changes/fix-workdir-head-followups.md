### Fixed

- A workdir set placed through the head is measured by what it places, as
  `place --local` measures it, so a set under 4 MiB whose JSON text is
  over it is no longer refused.
- Two first places of one set in two directories at once no longer fail
  one of them after its files are placed. `workdir.placed` and
  `workdir.failed` are raised once the record is written.
- A `workdir remove` through the head that takes out some sets of a
  directory and not others drops the records of those it took out, and
  raises `workdir.removed` for each, also for a set placed with `--local`.
- The head records a directory one way however it is written: `/w/`,
  `/w/.` and `/w` are one directory to `check`, `remove` and `list`.
