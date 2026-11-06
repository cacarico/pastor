//! Where a bundle's files go on a machine. What a harness's home holds
//! (skills, hooks, instructions, settings) comes from its manifest
//! (`harness_manifest::HarnessPaths`), and how it places MCP servers
//! (`harness_manifest::McpPlacement`); what is left here is the one place
//! shared by every home.

/// Where a `bin` file goes on a machine, for every home at once.
pub const BIN: &str = "~/.local/bin";
