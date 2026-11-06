### Added

- A task can carry a load, the kind of work it is (`rust`, `light`), and
  only a machine that takes that load places it. `pastor task run --load`
  sets it. Without the flag, the task takes its agent's `load`
  (`[agents.<name>]` in pastor.toml), else its repo's (`[repos."<path>"]`).
  `loads` on a `[[flock]]` or a `[[machine]]` in flock.toml lists the loads
  a machine takes. A load no machine of the task's flock takes is refused
  when the task is queued (`load_not_taken`). A task with no load places as
  before. IPC protocol 46.
