### Fixed

- `task priority` and `queue move` refuse a task a dispatch pass is sending
  to a machine with `not_queued`, as for a task a machine has taken. Before,
  they reported success while the pass sent the old decision, so clearing
  `--preempt` could still pause a victim.
- The orchestrator's close of an agent's pane is bounded like its other
  requests to a machine, so a stuck actor no longer holds a stop or restart
  pass forever.
- A task a dispatch pass placed keeps its `not_queued` refusal for as long
  as the actor may still claim it, not just until the head gives up
  waiting for an answer: a machine slow past `reply_wait` no longer lets
  `task priority` or `queue move` land on it while the placement is still
  in flight.
- `queue move --before` or `--after` naming a task a dispatch pass has
  placed is refused as `not_queued` too, instead of moving by a neighbour
  about to leave the queue; `--to` no longer counts such a task toward its
  position either.
