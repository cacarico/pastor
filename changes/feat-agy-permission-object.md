### Changed

- Before a profiled agy launch, pastor now writes the machine's whole
  permission object into agy's settings, not only the deny rules:
  `permissions.allow`, `.deny` and `.ask`, `toolPermission` and `agentMode`
  come from the machine's own profile and replace what the file had, so a
  rule the profile dropped, or agy saved from an "always allow" answer, goes
  on the next launch. Every other key, `trustedWorkspaces` among them, keeps
  its value and its place. A machine whose profile denies nothing no longer
  fails a task when its settings cannot be written.

### Added

- `[profiles.<name>.agy]` sets `tool_permission`, `agent_mode`,
  `allow_non_workspace_access` and `queued_messages` for agy's settings; a
  word agy does not know fails the load.
