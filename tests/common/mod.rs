//! What every test that runs the pastor binary shares.
use std::process::Command;

/// The variables a test must never inherit. The suite may run in an agent's
/// pane, which carries PASTOR_TASK and PASTOR_HEAD; PASTOR_HEAD beats
/// client.toml, so an inherited one sends the test's commands to the real
/// head. An orchestrator's script sets PASTOR_ORCHESTRATOR, a shell may name
/// real config, state and data dirs, and an editor would open on the terminal.
/// A test that wants one sets it after.
const SCRUBBED: &[&str] = &[
    "PASTOR_HEAD",
    "PASTOR_TASK",
    "PASTOR_ORCHESTRATOR",
    "PASTOR_DATA_DIR",
    "PASTOR_CONFIG_DIR",
    "PASTOR_STATE_DIR",
    "VISUAL",
    "EDITOR",
];

/// Removes `SCRUBBED` from `c`, for a command that runs pastor indirectly.
pub fn scrub(c: &mut Command) -> &mut Command {
    for var in SCRUBBED {
        c.env_remove(var);
    }
    c
}

/// The pastor binary with `SCRUBBED` removed. The only place a test builds
/// one, which tests/helper_lint.rs checks.
pub fn pastor() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_pastor"));
    scrub(&mut c);
    c
}
