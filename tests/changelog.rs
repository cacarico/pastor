//! Changelog entries as files: a pull request adds `changes/<branch>.md`
//! instead of a line under Unreleased, so two of them never touch the same
//! lines, and `scripts/changelog.sh` gathers the files into the release's
//! CHANGELOG section in merge order. Each test runs the script in a throwaway
//! git repository shaped like this one.
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const HEADER: &str = "# Changelog\n\nIntro.\n\n";
const OLD_RELEASE: &str = "## 0.1.0 - 2026-01-01\n\n### Added\n\n- The start.\n";

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/changelog.sh")
}

fn git(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "merge.ff=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

fn ok(out: Output) -> String {
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new("bash")
        .arg(script())
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    ok(git(dir.path(), &["init", "-q"]));
    std::fs::write(
        dir.path().join("CHANGELOG.md"),
        format!("{HEADER}{OLD_RELEASE}"),
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("changes")).unwrap();
    std::fs::write(dir.path().join("changes/README.md"), "How to.\n").unwrap();
    ok(git(dir.path(), &["add", "-A"]));
    ok(git(dir.path(), &["commit", "-qm", "start"]));
    dir
}

/// A branch off main that adds one change file and commits it.
fn branch_with_change(dir: &Path, branch: &str, file: &str, body: &str) {
    ok(git(dir, &["checkout", "-q", "-b", branch, "main"]));
    std::fs::write(dir.join("changes").join(file), body).unwrap();
    ok(git(dir, &["add", "-A"]));
    ok(git(dir, &["commit", "-qm", branch]));
    ok(git(dir, &["checkout", "-q", "main"]));
}

fn merge(dir: &Path, branch: &str) {
    ok(git(dir, &["merge", "-q", "-m", branch, branch]));
}

#[test]
fn two_prs_that_each_add_a_change_file_merge_without_conflict() {
    let dir = repo();
    let d = dir.path();
    branch_with_change(d, "a", "a.md", "### Added\n\n- A.\n");
    branch_with_change(d, "b", "b.md", "### Fixed\n\n- B.\n");
    merge(d, "a");
    // `b` was cut before `a` merged, so this is the second PR of the pair.
    merge(d, "b");
    assert!(d.join("changes/a.md").exists() && d.join("changes/b.md").exists());
    ok(run(d, &["check"]));
}

#[test]
fn gather_writes_the_section_in_merge_order_and_removes_the_files() {
    let dir = repo();
    let d = dir.path();
    // Names sort against merge order, so the order has to come from git.
    branch_with_change(
        d,
        "z",
        "z.md",
        "### Added\n\n- Z added,\n  on two lines.\n\n### Fixed\n\n- Z fixed.\n",
    );
    branch_with_change(d, "m", "m.md", "### Fixed\n\n- M fixed.\n");
    branch_with_change(d, "a", "a.md", "### Added\n\n- A added.\n");
    merge(d, "z");
    merge(d, "m");
    merge(d, "a");
    // One not committed yet comes last.
    std::fs::write(d.join("changes/b.md"), "### Changed\n\n- B changed.\n").unwrap();

    ok(run(d, &["gather", "0.2.0", "2026-02-02"]));

    let changelog = std::fs::read_to_string(d.join("CHANGELOG.md")).unwrap();
    let want = format!(
        "{HEADER}## 0.2.0 - 2026-02-02\n\n\
         ### Added\n\n- Z added,\n  on two lines.\n- A added.\n\n\
         ### Changed\n\n- B changed.\n\n\
         ### Fixed\n\n- Z fixed.\n- M fixed.\n\n\
         {OLD_RELEASE}"
    );
    assert_eq!(changelog, want);
    let left: Vec<_> = std::fs::read_dir(d.join("changes"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(left, ["README.md"]);
    ok(run(d, &["check"]));
}

#[test]
fn notes_prints_the_pending_section_and_changes_nothing() {
    let dir = repo();
    let d = dir.path();
    branch_with_change(d, "a", "a.md", "### Fixed\n\n- A.\n");
    merge(d, "a");
    let notes = ok(run(d, &["notes"]));
    assert_eq!(notes, "### Fixed\n\n- A.\n");
    assert!(d.join("changes/a.md").exists());
}

#[test]
fn gather_with_no_change_files_fails() {
    let dir = repo();
    let out = run(dir.path(), &["gather", "0.2.0"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no change files"));
}

#[test]
fn check_fails_on_an_unreleased_section() {
    let dir = repo();
    let d = dir.path();
    std::fs::write(
        d.join("CHANGELOG.md"),
        format!("{HEADER}## Unreleased\n\n### Added\n\n- X.\n\n{OLD_RELEASE}"),
    )
    .unwrap();
    let out = run(d, &["check"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("Unreleased"));
}

#[test]
fn check_fails_on_a_malformed_change_file() {
    for (name, body) in [
        ("bad-heading.md", "### Improved\n\n- X.\n"),
        ("no-heading.md", "- X.\n"),
        ("empty-heading.md", "### Added\n\n### Fixed\n\n- X.\n"),
        ("release-heading.md", "## 0.2.0\n\n### Added\n\n- X.\n"),
        ("not-markdown.txt", "### Added\n\n- X.\n"),
    ] {
        let dir = repo();
        let d = dir.path();
        std::fs::write(d.join("changes").join(name), body).unwrap();
        let out = run(d, &["check"]);
        assert!(!out.status.success(), "{name} passed");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(name),
            "{name}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn this_repository_passes_the_check() {
    ok(run(Path::new(env!("CARGO_MANIFEST_DIR")), &["check"]));
}

#[test]
fn check_fails_on_an_entry_already_released() {
    // A branch older than the release can bring back entries the release
    // already gathered; the next gather would publish them twice.
    let released =
        "## 0.1.0 - 2026-01-01\n\n### Added\n\n- The start.\n- A long one,\n  on two lines.\n";
    for body in [
        "### Added\n\n- The start.\n",
        "### Fixed\n\n- New.\n- A long one,\n  on two lines.\n",
    ] {
        let dir = repo();
        let d = dir.path();
        std::fs::write(d.join("CHANGELOG.md"), format!("{HEADER}{released}")).unwrap();
        std::fs::write(d.join("changes/stale.md"), body).unwrap();
        let out = run(d, &["check"]);
        assert!(!out.status.success(), "{body} passed");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("changes/stale.md") && err.contains("already released"),
            "{err}"
        );
    }
}

#[test]
fn check_passes_a_new_entry_that_shares_a_first_line_with_a_released_one() {
    let dir = repo();
    let d = dir.path();
    std::fs::write(
        d.join("CHANGELOG.md"),
        format!("{HEADER}## 0.1.0 - 2026-01-01\n\n### Added\n\n- The start,\n  first.\n"),
    )
    .unwrap();
    std::fs::write(
        d.join("changes/new.md"),
        "### Fixed\n\n- The start,\n  again.\n- Something new.\n",
    )
    .unwrap();
    ok(run(d, &["check"]));
}
