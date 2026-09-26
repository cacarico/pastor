//! What `cargo install pastor-cli` gets: the crate is published as
//! `pastor-cli` (the `pastor` name on crates.io belongs to someone else),
//! installs one binary named `pastor`, and leaves the `fake-herdr` test
//! double out of the published package.
use std::path::{Component, Path};
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

/// The files `cargo package` would upload, relative to the crate root.
fn package_files() -> Vec<String> {
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
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(String::from)
        .collect()
}

/// Every `include_str!` target under `contrib/` in `dir`, relative to the
/// crate root.
fn contrib_includes(root: &Path, dir: &Path, found: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            contrib_includes(root, &path, found);
            continue;
        }
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        for rest in text.split("include_str!(\"").skip(1) {
            let target = rest.split('"').next().unwrap();
            let mut full = path.parent().unwrap().to_path_buf();
            for part in Path::new(target).components() {
                match part {
                    Component::ParentDir => {
                        full.pop();
                    }
                    Component::Normal(p) => full.push(p),
                    _ => {}
                }
            }
            let rel = full
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if rel.starts_with("contrib/") {
                found.push(rel);
            }
        }
    }
}

/// A file the binary embeds but the package leaves out breaks
/// `cargo install pastor-cli` with a missing-file error, as the launchd
/// plists once did.
#[test]
fn every_embedded_contrib_file_is_published() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut wanted = Vec::new();
    contrib_includes(root, &root.join("src"), &mut wanted);
    for want in [
        "contrib/systemd/pastor.service",
        "contrib/launchd/pastor.plist",
    ] {
        assert!(wanted.iter().any(|w| w == want), "{want} not in {wanted:?}");
    }
    let files = package_files();
    for want in &wanted {
        assert!(files.contains(want), "{want} missing from {files:?}");
    }
}

#[test]
fn the_published_package_leaves_fake_herdr_out() {
    let files = package_files();
    let files: Vec<&str> = files.iter().map(String::as_str).collect();
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
