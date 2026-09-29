//! Drives the real binaries: pastor serve with a fake-herdr machine, then run/list/task.

mod agents_and_models;
mod bridge;
#[path = "../common/mod.rs"]
mod common;
mod completions_and_help;
mod daemon_signals;
mod describe;
mod edit;
mod flock;
mod helpers;
mod jobs_and_pull_machines;
mod machine;
mod orchestrators;
mod queue;
mod remote_head;
mod role_guards;
mod setup;
mod spec_skill;
mod task;
mod trust;
