### Fixed

- A task whose agy agent ended its message with `DONE` now ends itself. agy
  draws its input as a bare `>` between two horizontal rules, and pastor took
  the rule above it for the last line of the message, so `DONE` was never
  the last line it saw.
