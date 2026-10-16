//! Provisioners: what checks and sets up a machine for agents. The one here
//! is built in, since it needs what only the head has (the machine's own
//! profile); one installed and run like a connector is later work.
//!
//! - `agy`: makes agy's global settings on a machine deny what the
//!   machine's own permission profile denies, just before a profiled agy
//!   launch.

pub mod agy;
