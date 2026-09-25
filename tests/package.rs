//! What `cargo install pastor-cli` gets: the crate is published as
//! `pastor-cli` (the `pastor` name on crates.io belongs to someone else),
//! installs one binary named `pastor`, and leaves the `fake-herdr` test
//! double out of the published package.
use std::process::Command;

fn manifest() -> toml::Table {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap();
    text.parse().unwrap()
}

#[test]
fn the_package_is_pastor_cli_and_installs_pastor() {
    let m = manifest();
    assert_eq!(m["package"]["name"].as_str(), Some("pastor-cli"));
    // The library keeps its name, so `use pastor::...` still works.
    assert_eq!(m["lib"]["name"].as_str(), Some("pastor"));
    let bins = m["bin"].as_array().unwrap();
    assert!(
        bins.iter()
            .any(|b| b["name"].as_str() == Some("pastor")
                && b["path"].as_str() == Some("src/main.rs")),
        "{bins:?}"
    );
}

#[test]
fn the_published_package_leaves_fake_herdr_out() {
    let out = Command::new(env!("CARGO"))
        .args(["package", "--list", "--allow-dirty", "--offline"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let files = String::from_utf8(out.stdout).unwrap();
    let files: Vec<&str> = files.lines().collect();
    for want in [
        "src/main.rs",
        "src/lib.rs",
        "Cargo.lock",
        "LICENSE",
        "README.md",
        "skills/pastor/SKILL.md",
    ] {
        assert!(files.contains(&want), "{want} missing from {files:?}");
    }
    assert!(
        files
            .iter()
            .all(|f| !f.contains("fake-herdr") && !f.starts_with("tests/")),
        "{files:?}"
    );
}
