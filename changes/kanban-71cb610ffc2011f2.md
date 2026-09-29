### Security

- A profiled opencode task runs with the repo's own opencode config off
  (`OPENCODE_DISABLE_PROJECT_CONFIG=1`), so an `opencode.json` in a branch
  under review can no longer lift the profile's deny rules. The checkout's
  `AGENTS.md` and `CLAUDE.md` still reach the agent, by path in
  `OPENCODE_CONFIG_CONTENT`.
- The opencode permission check before a profiled task also reads
  `~/.opencode/`, the managed config directory (`/etc/opencode`, on macOS
  `/Library/Application Support/opencode`) and a legacy `"tools"` block.
