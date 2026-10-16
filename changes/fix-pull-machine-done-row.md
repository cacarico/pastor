### Fixed

- A task on a pull machine that its agent ended with `pastor task done`
  no longer holds the machine's slot and its flock's seat for good. The
  `task done` reaches the head alone, so the machine's own serve read an
  idle agent that is not Claude as idle without `task done`, reported it
  `blocked` every tick, and once the agent was gone reported it `failed`
  and forgot the row while the head kept it `done` and counted it live.
  The serve now ends its row as the head did, with the head's summary, so
  auto-close takes the pane once the agent is idle and reports it closed;
  a `failed` report on an ended task closes the row on the head, its
  finish kept and with no error; a task the head ended before the serve
  started it is closed there, no agent started; and a pull machine
  counted lost has its ended tasks closed on the head, as its running
  ones go stale, so a seat never waits on a machine that is gone.
- `pastor queue` explains a task pinned to a pull machine by the
  machine's slots and its flock's seat there (`is full`, `flock F is at N
  of N on M`, `next claim: M has room`), as its claims see them. It said
  `machine M is not connected` for every such task, since the dispatch
  pass's view of a pull machine, which it never pushes to, was reused for
  the queue. A loose task never reads a pull machine as where it goes
  next, since whether the machine takes flock work is its own serve's
  setting.
