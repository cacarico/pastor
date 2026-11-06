### Changed

- How a bundle's MCP servers are placed is part of the harness manifest: an
  `[mcp]` table with `method = "plugin"` (a local plugin pastor writes into the
  home, as for Claude) or `method = "file"` and `file = "~/..."` (one JSON file
  shared by every home of the kind, which may be outside the home, as for agy).
  A manifest in the config dir can set it for a harness of its own. A harness
  with no `[mcp]` has its MCP servers skipped with a note, as before for codex
  and opencode.
