### Added

- A task with a `fallback` list whose model hits a usage limit goes on under
  the next free model of its list, on the same machine and in the same
  worktree and branch, as a new round of the same task: in the same Claude
  session when the new model runs on the same login, else from its prompt
  with the end of the last agent's pane. It waits instead when the reset is
  within `[limits] wait_under`, or when no other model of its list is free
  (`all_exhausted`) or runs on its machine (`no_fallback`, on its own model).
  `task.agent_switched` says so, and `task describe` lists the rounds and why
  a task waited when it could have moved on.

### Changed

- `[limits] wait_under` defaults to `30m` and accepts `"0"`;
  `handover_lines` defaults to `60`.
- `task.waiting` carries `why: reset_soon` or `all_exhausted` as well as
  `no_fallback`.
