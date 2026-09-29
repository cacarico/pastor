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
    // The library's half of the test double, 1800 lines nobody running
    // pastor needs.
    assert!(!files.contains(&"src/herdr/fake.rs"), "{files:?}");
}

/// The fake herdr builds only with the `fake-herdr` feature (or in the
/// library's own unit tests), so a default build of the library and
/// `cargo install` never compile it. The integration tests that run it say
/// so too, or a plain `cargo test` would fail on a missing binary.
#[test]
fn the_fake_herdr_needs_its_feature() {
    let m = manifest();
    assert!(m["features"].get("default").is_none_or(|d| {
        !d.as_array()
            .unwrap()
            .iter()
            .any(|f| f.as_str() == Some("fake-herdr"))
    }));
    assert!(m["features"].get("fake-herdr").is_some(), "{m:?}");
    let needs = |t: &toml::Value| {
        t.get("required-features")
            .and_then(|r| r.as_array())
            .is_some_and(|r| r.iter().any(|f| f.as_str() == Some("fake-herdr")))
    };
    let bins = m["bin"].as_array().unwrap();
    let fake = bins
        .iter()
        .find(|b| b["name"].as_str() == Some("fake-herdr"))
        .expect("a [[bin]] for fake-herdr");
    assert!(needs(fake), "{fake:?}");
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let tests = m["test"].as_array().unwrap();
    for entry in std::fs::read_dir(root.join("tests")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        if !text.contains(concat!("CARGO_BIN_EXE_", "fake-herdr"))
            && !text.contains(concat!("herdr::", "fake"))
        {
            continue;
        }
        let name = path.file_stem().unwrap().to_str().unwrap();
        let declared = tests
            .iter()
            .find(|t| t["name"].as_str() == Some(name))
            .unwrap_or_else(|| panic!("tests/{name}.rs uses the fake herdr: declare it"));
        assert!(needs(declared), "{declared:?}");
    }
    let src = std::fs::read_to_string(root.join("src/herdr/mod.rs")).unwrap();
    let src = src.replace("\r\n", "\n");
    assert!(
        src.contains("#[cfg(feature = \"fake-herdr\")]\npub mod fake;"),
        "src/herdr/mod.rs"
    );
}
