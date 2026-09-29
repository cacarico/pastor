### Changed

- A Codex task under a permission profile starts with `--ask-for-approval
  never --sandbox workspace-write` (`danger-full-access` under
  `unrestricted`), so it no longer stops at an approval prompt on its first
  command. Codex has no per-command allow or deny flag, so its lists are no
  longer refused with `agent_tools_unsupported`: the sandbox stands in, and
  `pastor task describe` shows them as not applied. Agent args that pick an
  approval policy or sandbox are refused with `profile_args_conflict`, as
  Claude's permission mode is. The sandbox has no network; a task that
  needs it adds `-c sandbox_workspace_write.network_access=true`.
