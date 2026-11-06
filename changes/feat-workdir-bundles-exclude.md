### Added

- `pastor workdir place` keeps the files it placed out of git: it lists them
  in the repo's `info/exclude`, in a block of its own that `remove` takes
  them out of again. A path git tracks is a conflict and is not written
  over, and `git_exclude: false` in a set turns the listing off.
