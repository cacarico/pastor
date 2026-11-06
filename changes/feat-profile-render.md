### Added

- `pastor profile render <profile> --kind <claude|agy>
  [--machine M] [--json]` prints what a profile becomes for claude or agy: the flags and env a task under it is launched with, the profile in
  the agent's settings shape (Claude's `permissions`, agy's permission
  object), and each pattern that does not reach the agent with why. A tool
  that drives pastor can name a profile for its own sessions and write the
  result itself. `--machine` settles the profile as for a task on that
  machine. `--kind opencode` and `codex` are refused for now.
