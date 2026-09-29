//! `make portability`: the cross-target `cargo check` that CI's `portability`
//! job runs, as a make target, so the PR gate and any other CI run the same
//! command. Each test runs the real Makefile with a PATH that holds only
//! stand-ins for `cargo-zigbuild`, `zig` and `rustc`, so nothing is compiled
//! and what is "installed" is what the test put there.
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const TARGETS: [&str; 3] = [
    "x86_64-unknown-linux-musl",
    "armv7-unknown-linux-musleabihf",
    "x86_64-unknown-freebsd",
];

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// The real `make`, by absolute path: the PATH the target runs with has no
/// system directories on it.
fn make() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("make"))
        .find(|p| p.is_file())
        .expect("make on the PATH")
}

struct Env {
    dir: tempfile::TempDir,
}

impl Env {
    /// Everything installed: the two tools and the three Rust targets.
    fn new() -> Self {
        let env = Env {
            dir: tempfile::tempdir().unwrap(),
        };
        std::fs::create_dir(env.bin()).unwrap();
        env.tool(
            "cargo-zigbuild",
            r#"echo "$@" >> "$PORTABILITY_LOG"; case "$*" in *"$PORTABILITY_FAIL"*) exit 1 ;; esac"#,
        );
        env.tool("zig", "echo 0.0.0");
        // `rustc --print target-libdir --target T` names a directory that is
        // there only when T is installed.
        env.tool("rustc", r#"echo "$PORTABILITY_SYSROOT/$4/lib""#);
        for target in TARGETS {
            std::fs::create_dir_all(env.sysroot().join(target).join("lib")).unwrap();
        }
        env
    }

    fn bin(&self) -> PathBuf {
        self.dir.path().join("bin")
    }

    fn sysroot(&self) -> PathBuf {
        self.dir.path().join("sysroot")
    }

    fn log(&self) -> PathBuf {
        self.dir.path().join("log")
    }

    fn tool(&self, name: &str, body: &str) {
        let path = self.bin().join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn remove_tool(&self, name: &str) {
        std::fs::remove_file(self.bin().join(name)).unwrap();
    }

    fn remove_target(&self, target: &str) {
        std::fs::remove_dir_all(self.sysroot().join(target)).unwrap();
    }

    fn run(&self, envs: &[(&str, &str)]) -> Output {
        Command::new(make())
            .arg("-f")
            .arg(root().join("Makefile"))
            .arg("portability")
            .current_dir(self.dir.path())
            .env_clear()
            .env("PATH", self.bin())
            .env("PORTABILITY_LOG", self.log())
            .env("PORTABILITY_SYSROOT", self.sysroot())
            .env("PORTABILITY_FAIL", "no target fails")
            .envs(envs.iter().copied())
            .output()
            .unwrap()
    }

    /// What cargo-zigbuild was run with, one call per line.
    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.log())
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

/// The one line a failed precondition prints, after asserting that it is
/// one line, that make failed, and that nothing was checked.
fn missing(env: &Env, out: &Output) -> String {
    assert!(!out.status.success(), "{out:?}");
    assert!(env.calls().is_empty(), "{:?}", env.calls());
    let stderr = String::from_utf8_lossy(&out.stderr);
    let lines: Vec<&str> = stderr
        .lines()
        .filter(|l| l.starts_with("make portability: "))
        .collect();
    assert_eq!(lines.len(), 1, "{stderr}");
    lines[0].to_owned()
}

#[test]
fn checks_each_target_as_ci_did() {
    let env = Env::new();
    let out = env.run(&[]);
    assert!(out.status.success(), "{out:?}");
    let want: Vec<String> = TARGETS
        .iter()
        .map(|t| format!("check --locked --all-targets --features fake-herdr --target {t}"))
        .collect();
    assert_eq!(env.calls(), want);
}

#[test]
fn a_target_that_fails_stops_the_run() {
    let env = Env::new();
    let out = env.run(&[("PORTABILITY_FAIL", TARGETS[1])]);
    assert!(!out.status.success(), "{out:?}");
    assert_eq!(env.calls().len(), 2, "{:?}", env.calls());
}

#[test]
fn without_cargo_zigbuild_it_says_so() {
    let env = Env::new();
    env.remove_tool("cargo-zigbuild");
    let line = missing(&env, &env.run(&[]));
    assert!(
        line.contains("cargo install --locked cargo-zigbuild"),
        "{line}"
    );
    assert!(!line.contains("zig ("), "{line}");
    assert!(!line.contains("rustup"), "{line}");
}

#[test]
fn without_zig_it_says_so() {
    let env = Env::new();
    env.remove_tool("zig");
    let line = missing(&env, &env.run(&[]));
    assert!(line.contains("missing zig ("), "{line}");
    assert!(!line.contains("cargo install"), "{line}");
}

// cargo-zigbuild takes zig from this variable before it looks at the PATH.
#[test]
fn a_zig_named_by_cargo_zigbuild_zig_path_counts() {
    let env = Env::new();
    env.remove_tool("zig");
    let out = env.run(&[("CARGO_ZIGBUILD_ZIG_PATH", "/opt/zig/zig")]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(env.calls().len(), 3);
}

#[test]
fn without_a_rust_target_it_names_the_target() {
    let env = Env::new();
    env.remove_target(TARGETS[2]);
    let line = missing(&env, &env.run(&[]));
    assert!(
        line.contains("rustup target add x86_64-unknown-freebsd"),
        "{line}"
    );
    assert!(!line.contains(TARGETS[0]), "{line}");
}

#[test]
fn everything_missing_is_still_one_line() {
    let env = Env::new();
    env.remove_tool("cargo-zigbuild");
    env.remove_tool("zig");
    env.remove_tool("rustc");
    let line = missing(&env, &env.run(&[]));
    assert!(line.contains("cargo-zigbuild"), "{line}");
    assert!(line.contains("zig ("), "{line}");
    for target in TARGETS {
        assert!(
            line.contains(&format!("rustup target add {target}")),
            "{line}"
        );
    }
}

#[test]
fn make_help_lists_it() {
    let out = Command::new("make")
        .arg("help")
        .current_dir(root())
        .env_remove("MAKEFLAGS")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout
            .lines()
            .any(|l| l.trim_start().starts_with("portability ")),
        "{stdout}"
    );
}

/// The `portability` job of ci.yml: from its name to the next job's, or to
/// the end of the file.
fn ci_job() -> String {
    let ci = std::fs::read_to_string(root().join(".github/workflows/ci.yml")).unwrap();
    let mut job = String::new();
    for line in ci.lines().skip_while(|l| *l != "  portability:") {
        let next_job = line.starts_with("  ") && !line.starts_with("   ") && line.ends_with(':');
        if !job.is_empty() && next_job {
            break;
        }
        job.push_str(line);
        job.push('\n');
    }
    assert!(!job.is_empty(), "no portability job in ci.yml");
    job
}

#[test]
fn ci_runs_the_make_target() {
    let job = ci_job();
    assert!(job.contains("- run: make portability\n"), "{job}");
    assert!(!job.contains("cargo-zigbuild check"), "{job}");
}

// The job's toolchain step installs the Rust targets by name, so the list is
// in ci.yml too; a target added to the Makefile alone would fail there.
#[test]
fn ci_installs_the_targets_the_makefile_checks() {
    let makefile = std::fs::read_to_string(root().join("Makefile")).unwrap();
    let list = makefile
        .lines()
        .find_map(|l| l.strip_prefix("PORTABILITY_TARGETS"))
        .expect("PORTABILITY_TARGETS in the Makefile");
    let in_makefile: Vec<&str> = list
        .trim_start_matches([' ', ':', '?', '='])
        .split_whitespace()
        .collect();
    assert_eq!(in_makefile, TARGETS);

    let job = ci_job();
    let installed = job
        .lines()
        .find_map(|l| l.trim().strip_prefix("targets: "))
        .expect("targets: in the portability job");
    let in_ci: Vec<&str> = installed.split(", ").collect();
    assert_eq!(in_ci, in_makefile);
}
