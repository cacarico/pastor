### Added

- A permission profile, `[defaults]`, a flock or a job can allow or deny the
  tools of an MCP server for a Claude task with `mcp__<server>`,
  `mcp__<server>__<tool>` or `mcp__<server>__*`. A malformed `mcp__` pattern
  (an empty name, a `(...)` argument, a character a server name cannot
  hold) now fails the load of the file that holds it, naming the list.
