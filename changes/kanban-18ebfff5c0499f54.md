### Fixed

- `task priority` and `queue move` refuse a task a dispatch pass is sending
  to a machine with `not_queued`, as for a task a machine has taken. Before,
  they reported success while the pass sent the old decision, so clearing
  `--preempt` could still pause a victim.
- The orchestrator's close of an agent's pane is bounded like its other
  requests to a machine, so a stuck actor no longer holds a stop or restart
  pass forever.
