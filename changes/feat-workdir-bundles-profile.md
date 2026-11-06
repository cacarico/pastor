### Added

- A workdir set may name a permission `profile`: `pastor workdir place`
  renders it from pastor.toml and writes its rules into
  `.claude/settings.local.json`, replacing any rule the agent added since
  (counted as `agent_rules_dropped`), and answers the flags and env the
  agent's start needs. `--machine` settles the profile as for a task on that
  machine.
