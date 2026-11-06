### Fixed

- A pull machine's task the head marked `stale` only because the machine went
  silent for `pull_lost_after` goes back to `running` on the next report of
  its agent at work; before, it stayed stale for good.

### Changed

- A pull machine's headless serve logs a request the head leaves unanswered
  (a timeout, an unreachable head, no serve there) as one `pull_head_silent`
  warning per outage, and the head's next answer at `info`; before, a dropped
  report was logged at debug and a failed claim not at all. A tick stops at
  the first unanswered request instead of waiting out each one in turn, so a
  silent head costs one timeout per tick, not three to five.
