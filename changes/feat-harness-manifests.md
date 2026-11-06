### Added

- Harness manifests: one TOML file per agent CLI, with its binary, how to read
  its version, its login check, its install sources in order (a release URL
  with a sum, npm, the vendor's installer) and where it keeps its skills,
  hooks, instructions, settings and MCP servers. `claude`, `agy`, `codex` and
  `opencode` ship in the binary; a file in `harnesses/` of the config dir
  replaces one or adds a harness without Rust. The version pins and the
  machine check ask for a version the way the manifest says, and `pastor
  bundle apply` places a bundle by the manifest's paths. Nothing installs from
  the sources yet.
