//! The shared `pastor()` in tests/common keeps a test off the real head. Its
//! own file, since the one test here sets PASTOR_HEAD in the test process as
//! an agent's pane does, and no other test may see it.
mod common;

#[test]
fn an_inherited_pastor_head_does_not_leave_the_test() {
    // SAFETY: the only test in this binary, so no other thread reads the
    // environment while it is written.
    unsafe { std::env::set_var("PASTOR_HEAD", "user@nowhere.invalid") };
    let tmp = tempfile::tempdir().unwrap();
    let out = common::pastor()
        .env("PASTOR_CONFIG_DIR", tmp.path().join("c"))
        .env("PASTOR_STATE_DIR", tmp.path().join("s"))
        .env("PASTOR_DATA_DIR", tmp.path().join("d"))
        .args(["task", "list", "--json"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    // No daemon runs, so pastor shows the temp state dir's store instead of
    // failing with head_unreachable after ssh to nowhere.invalid.
    assert!(out.status.success(), "{stderr}");
    assert!(stderr.contains("not running"), "{stderr}");
}
