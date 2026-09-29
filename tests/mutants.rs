//! `.cargo/mutants.toml` names files by path, and cargo-mutants says
//! nothing when a glob matches none: a module renamed or moved would drop
//! out of `make mutants` silently and look like a clean run.
use std::path::Path;

fn config() -> toml::Table {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/.cargo/mutants.toml");
    std::fs::read_to_string(path).unwrap().parse().unwrap()
}

/// Whether `dir` or anything under it holds a `.rs` file.
fn has_rust(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries.flatten().any(|e| {
            let path = e.path();
            if path.is_dir() {
                has_rust(&path)
            } else {
                path.extension().is_some_and(|x| x == "rs")
            }
        })
    })
}

#[test]
fn every_examined_glob_matches_a_rust_file() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let globs = config()["examine_globs"].as_array().unwrap().clone();
    assert!(!globs.is_empty());
    for glob in globs {
        let glob = glob.as_str().unwrap();
        // Only two shapes are used: a plain file, or `dir/**/*.rs`.
        let found = match glob.strip_suffix("/**/*.rs") {
            Some(dir) => has_rust(&root.join(dir)),
            None => {
                assert!(!glob.contains('*'), "unsupported glob shape: {glob}");
                root.join(glob).is_file()
            }
        };
        assert!(found, "{glob} in .cargo/mutants.toml matches no file");
    }
}
