//! No test builds the pastor command except through `common::pastor()`.
/// Every test file builds pastor with `common::pastor()`, so none can forget
/// to scrub the environment.
#[test]
fn a_test_runs_pastor_only_through_the_helper() {
    // Split so this file does not match itself.
    let needle = concat!("Command::new(env!(\"CARGO_BIN_", "EXE_pastor\"))");
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut found = vec![];
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        // rustfmt may break the call over lines.
        let text: String = std::fs::read_to_string(&path)
            .unwrap()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        if text.contains(needle) {
            found.push(path.file_name().unwrap().to_string_lossy().into_owned());
        }
    }
    found.sort();
    assert!(
        found.is_empty(),
        "build the pastor command with common::pastor() instead: {found:?}"
    );
}
