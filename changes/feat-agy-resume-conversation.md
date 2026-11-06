### Added

- An agy task whose machine has herdr's agy hook installed records its
  conversation id, read back from herdr's `agent.list`/`agent.prompt`
  (`agent_session`), the same way a Claude task records its own
  `--session-id`. A low agy task with one recorded can now be paused by a
  critical `--preempt` and resumes on `agy --conversation <id>`; a usage
  limit waits out and resumes the same way; `pastor task attach` reopens a
  closed agy task with one on `agy --conversation <id>`, as it already did
  for Claude's `--resume`. A machine with no such hook installed, or one
  that has not reported anything yet, falls back to today's behaviour: the
  task simply records no session, so a critical task's `--preempt` cannot
  pause it and `pastor task attach` refuses a closed one with `no_agent`;
  only a usage-limit wait still starts the task fresh on its prompt, as it
  always has.
