### Added

- `pastor bundle apply` merges a bundle's settings into Claude's and agy's
  `settings.json`, key by key beside the user's own, and places its MCP
  servers: for Claude in a local plugin pastor makes and enables, with env
  given by variable name, and for agy in its `mcp_config.json`. A key the
  user holds with another value is reported as a `conflict` and kept; what
  pastor set comes out again when the bundle leaves, and what the user had
  stays.
