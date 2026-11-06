//! Provisioners: what checks and sets up a machine for agents.
//!
//! - `agy`: built in, since it needs what only the head has (the machine's
//!   own profile). It makes agy's global settings on a machine deny what
//!   the machine's own permission profile denies, just before a profiled
//!   agy launch.
//! - `manifest` and `catalog`: the provisioners a `[[machine]]` names by
//!   `provisioner`, each a `pastor-provisioner.toml` of checks and fixes.
//!   Four ship in the binary (`provisioners/` in the repository: `arch`,
//!   `debian`, `macos`, `raspberry-pi-os`) and more can be copied into
//!   `<data>/provisioners/<id>/`.
//! - `run`: runs a provisioner's checks on one machine, in a run directory
//!   of its own that it removes again; `machine_check` picks the
//!   provisioner and the agents for `pastor machine check`.

pub mod agy;
pub mod catalog;
pub mod manifest;
pub mod run;

pub use catalog::{BUILTIN, Catalog, Invalid, Provisioner, load_manifest};
pub(crate) use manifest::check_check_id;
pub use manifest::{AGENT_KINDS, Account, Check, MANIFEST_FILE, Manifest, Os, Platform};
