### Added

- A room floor in `flock.toml`: `min_disk` and `min_memory` on a
  `[[machine]]` entry, as sizes (`"4G"`, `"512M"`), are the disk (on each
  of `$HOME` and `/tmp`) and memory the machine must have free to take a
  task. The floor is off until a machine sets one of them; the other side
  then comes from `[defaults]` in `pastor.toml`, else the built-in 4G of
  disk and 1G of memory, and `"0"` turns a side off. `unknown_room` on a
  flock or a machine says whether one whose room cannot be read counts as
  room (`room`, the default) or not (`no_room`). A bad size fails the load.
  The keys load and are checked; nothing reads a machine's room yet.
