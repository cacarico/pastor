### Added

- `pastor machine cordon NAME [--reason TEXT]` and `pastor machine uncordon
  NAME`: a cordoned machine takes no new task on any claim path (a pull
  machine's claim, a paused task's resume and `task run --now` included), the
  tasks it runs go on, and a queued task says `waiting for a machine: NAME is
  cordoned`. The cordon is kept by the head, shows in `machine list` and
  `machine describe` with its reason, and `--now` onto it is refused with
  `machine_cordoned`.
