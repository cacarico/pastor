### Added

- `tests/panes.rs` runs the pane heuristics (the blocked question, the
  background shell footer, Claude's trust dialog) on a corpus of scrubbed
  screens under `tests/fixtures/panes/`, one test per screen and heuristic.
  The screens the parsers read wrong today are in it with their right answer
  and an ignored test that says why.
