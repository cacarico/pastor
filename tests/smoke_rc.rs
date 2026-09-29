//! `scripts/smoke-rc.sh`: the release candidate smoke. Each test runs the
//! script from a clone of a throwaway repository whose Makefile stands in for
//! pastor's (`build`, `smoke`, `smoke-profiles`), with a fake `herdr` and
//! `pastor` on PATH, and checks what it prints, its exit status, and that it
//! leaves the caller's checkout and TMPDIR as it found them.
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const TAG: &str = "v1.2.0-rc.1";

// The stand-in Makefile: `smoke` fails for the session `bad`, printing the
// directory it ran in so the test can see paths are scrubbed.
const MAKEFILE: &str = "build:\n\t@echo built\n\
smoke:\n\t@echo smoke in session $(SESSION)\n\
\t@if [ \"$(SESSION)\" = bad ]; then echo \"assertion failed in $(CURDIR)\"; exit 1; fi\n\
smoke-profiles:\n\t@echo \"profiles $(REPO) $(CLAUDE) $(OPENCODE)\"\n\
\t@echo \"PASS claude on $(CLAUDE)\"\n";

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/smoke-rc.sh")
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "tag.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

struct Env {
    root: tempfile::TempDir,
}

impl Env {
    /// An origin with the stand-in Makefile, a clone of it as the caller's
    /// checkout, and the rc tag made in origin after the clone, so only a
    /// fetch finds it.
    fn new() -> Env {
        let root = tempfile::tempdir().unwrap();
        let origin = root.path().join("origin");
        std::fs::create_dir(&origin).unwrap();
        git(&origin, &["init", "-q"]);
        std::fs::write(origin.join("Makefile"), MAKEFILE).unwrap();
        git(&origin, &["add", "Makefile"]);
        git(&origin, &["commit", "-q", "-m", "start"]);
        git(
            root.path(),
            &["clone", "-q", origin.to_str().unwrap(), "work"],
        );
        git(&origin, &["tag", TAG]);

        let bin = root.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        for (name, body) in [
            ("herdr", "echo herdr 0.9.1"),
            ("pastor", "echo \"pastor $FAKE_PASTOR_VERSION\""),
        ] {
            let p = bin.join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::create_dir(root.path().join("tmp")).unwrap();
        Env { root }
    }

    fn work(&self) -> PathBuf {
        self.root.path().join("work")
    }

    fn tmp(&self) -> PathBuf {
        self.root.path().join("tmp")
    }

    fn run(&self, args: &[&str], vars: &[(&str, &str)]) -> Output {
        let path = format!(
            "{}:{}",
            self.root.path().join("bin").display(),
            std::env::var("PATH").unwrap()
        );
        let mut c = Command::new("sh");
        c.arg(script())
            .args(args)
            .current_dir(self.work())
            .env("PATH", path)
            .env("TMPDIR", self.tmp())
            .env("FAKE_PASTOR_VERSION", "0.0.1")
            .env_remove("PROFILES")
            .env_remove("REPO")
            .env_remove("CLAUDE")
            .env_remove("OPENCODE")
            .env_remove("MAKEFLAGS")
            .env_remove("MAKELEVEL");
        for (k, v) in vars {
            c.env(k, v);
        }
        c.output().unwrap()
    }

    /// The caller's checkout is still on main with one worktree, and the
    /// scratch worktree is gone from TMPDIR.
    fn assert_clean(&self) {
        let list = git(&self.work(), &["worktree", "list", "--porcelain"]);
        assert_eq!(list.matches("worktree ").count(), 1, "{list}");
        let head = git(&self.work(), &["rev-parse", "--abbrev-ref", "HEAD"]);
        assert_eq!(head.trim(), "main");
        let left: Vec<_> = std::fs::read_dir(self.tmp()).unwrap().collect();
        assert!(left.is_empty(), "{left:?}");
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn without_a_tag_or_with_an_unknown_one_it_refuses() {
    let env = Env::new();
    let out = env.run(&[], &[]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let out = env.run(&["v9.9.9-rc.9"], &[]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("v9.9.9-rc.9"), "{err}");
    env.assert_clean();
}

#[test]
fn a_passing_smoke_prints_one_block_and_exits_zero() {
    let env = Env::new();
    let out = env.run(&[TAG, "s", "fleet-pi"], &[]);
    let text = stdout(&out);
    assert!(out.status.success(), "{out:?}");
    assert!(
        text.contains(&format!("### Smoke of {TAG} on fleet-pi: pass")),
        "{text}"
    );
    assert!(text.contains("- herdr: herdr 0.9.1"), "{text}");
    assert!(text.contains("| build | pass |"), "{text}");
    assert!(text.contains("| smoke | pass |"), "{text}");
    assert!(text.contains("| smoke-profiles | skipped |"), "{text}");
    assert!(!text.contains("```"), "no failure, no tail: {text}");
    env.assert_clean();
}

#[test]
fn a_failing_smoke_prints_its_last_lines_without_paths() {
    let env = Env::new();
    let out = env.run(&[TAG, "bad", "fleet-pi"], &[]);
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(text.contains(": fail"), "{text}");
    assert!(text.contains("| smoke | fail |"), "{text}");
    assert!(text.contains("assertion failed in <worktree>"), "{text}");
    let tmp = env.tmp();
    assert!(!text.contains(tmp.to_str().unwrap()), "{text}");
    env.assert_clean();
}

#[test]
fn profiles_need_the_head_on_the_tag() {
    let env = Env::new();
    let vars = [("PROFILES", "1"), ("REPO", "~/src/app"), ("CLAUDE", "m1")];
    let out = env.run(&[TAG, "s", "fleet-pi"], &vars);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("0.0.1"), "{err}");
    assert!(!stdout(&out).contains("built"), "refused before building");
    env.assert_clean();

    let mut vars = vars.to_vec();
    vars.push(("FAKE_PASTOR_VERSION", "1.2.0-rc.1"));
    let out = env.run(&[TAG, "s", "fleet-pi"], &vars);
    let text = stdout(&out);
    assert!(out.status.success(), "{out:?}");
    assert!(text.contains("| smoke-profiles | pass |"), "{text}");
    env.assert_clean();
}
