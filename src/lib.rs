pub mod cli;
pub mod complete;
pub mod config;
pub mod connector;
pub mod daemon;
pub mod describe;
pub mod dispatch;
pub mod edit;
pub mod events;
pub mod herdr;
pub mod hooks;
pub mod ipc;
pub mod machine;
pub mod schedule;
pub mod scheduler;
pub mod setup;
pub mod store;
pub mod task;
pub mod task_cli;
pub mod template;
pub mod trust_cli;

/// Lowest herdr socket protocol pastor speaks. herdr 0.9.0 and 0.9.1 ship 22.
pub const MIN_HERDR_PROTOCOL: u32 = 22;
