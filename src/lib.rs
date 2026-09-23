pub mod cli;
pub mod config;
pub mod daemon;
pub mod dispatch;
pub mod herdr;
pub mod ipc;
pub mod machine;
pub mod store;
pub mod task;

/// Lowest herdr socket protocol pastor speaks. herdr 0.9.0 and 0.9.1 ship 22.
pub const MIN_HERDR_PROTOCOL: u32 = 22;
