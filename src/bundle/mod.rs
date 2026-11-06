//! Bundles: what pastor places in the harness homes of a flock's machines.
//! A bundle is a directory with a `pastor-bundle.toml`: skills, files, an
//! instruction block, settings fragments, MCP servers, harness plugins and
//! requirements. Anyone authors one; `pastor bundle add` takes it from a
//! directory or a git ref, and a flock names the bundles its machines get
//! in flock.toml (`bundles`, `bundle_vars`, `bundle_apply`).
//!
//! - `manifest`: the file's shape and its validation against the bundle's
//!   files.
//! - `secrets`: what makes `add` refuse a bundle.
//! - `store`: the bundles dir on the head, versions by content hash, `add`
//!   and `remove`.
//! - `plan`: what the head renders for one machine, by the harness paths
//!   in `harness_manifest`.
//! - `push`: sending a plan to a push machine and running it there.
//! - `apply`: placing a plan on the machine, with its ledger and backups,
//!   and taking a home over for `adopt`.
//! - `cli`: `pastor bundle add|list|show|remove|apply|adopt`.
//! - `workdir`: sets placed in an agent's working directory instead of a
//!   home, by the same apply engine; `workdir_cli` is `pastor workdir`.
//! - `workdir_git`: keeping those files out of git, in the repo's
//!   `info/exclude`.
//!
//! Plugins and requirements are carried but not placed; settings fragments and
//! MCP servers are placed for claude and agy homes only.

pub mod apply;
pub mod cli;
pub mod keep;
pub mod manifest;
pub mod plan;
pub mod push;
pub mod secrets;
pub mod slots;
pub mod status;
pub mod store;
pub mod workdir;
pub mod workdir_cli;
pub mod workdir_git;
pub mod workdir_keep;
pub mod workdir_push;
pub mod workdir_report;
pub mod workdir_stored;
pub mod workdir_task;

pub use manifest::{MANIFEST_FILE, Manifest};
pub use store::{Bundle, Catalog};

/// The error code when the manifest, a file or the bundle directory's
/// shape is wrong.
pub const BUNDLE_INVALID: &str = "bundle_invalid";
/// The error code when a file in the bundle looks like it holds a secret.
pub const BUNDLE_SECRET: &str = "bundle_secret";
/// The error code when `bundle add --git` could not fetch the ref.
pub const BUNDLE_FETCH_FAILED: &str = "bundle_fetch_failed";
/// The error code for `bundle remove` of a bundle a flock or machine
/// still names.
pub const BUNDLE_IN_USE: &str = "bundle_in_use";
/// The error code when no bundle has that id in the bundles dir.
pub const UNKNOWN_BUNDLE: &str = "unknown_bundle";

/// How a machine takes a new version of its bundles: `manual`, the
/// default, only marks it stale; `auto` applies it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Apply {
    Auto,
    #[default]
    Manual,
}
