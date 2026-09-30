### Added

- `skills/dispatch/next-task.sh <plan.md>` reads a `pastor:spec` plan and its
  ledger and prints exactly one of `RUN <n> <prompt-file>`, `WAIT <n> t-<id>`,
  `CHECK <n> t-<id> <state>`, `COMPLETE` or `BLOCKED <n> <why>`: the
  mechanical part shared by the interactive `pastor:dispatch` skill and a
  scheduled orchestrator's pre script, neither built yet.
