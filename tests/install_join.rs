//! `install.sh join`: the install, then `pastor join` as the login user. The
//! join token is a secret, so it reaches pastor on a pipe: never as an
//! argument, never in its environment. A release of the test's own, a
//! directory the script fetches over `file://`, holds a `pastor` that writes
//! down how it was run.

use std::path::Path;
use std::process::{Command, Output};

const TOKEN: &str = "pjt_SECRETTOKENTEXT";

/// A release directory with one tarball, whose `pastor` logs its argv,
/// whether its environment holds the token and what it read on stdin.
fn release(dir: &Path) {
    let name = "pastor-1.0.0-x86_64-unknown-linux-musl";
    let pkg = dir.join("pkg").join(name);
    std::fs::create_dir_all(&pkg).unwrap();
    let script = format!(
        r#"#!/bin/sh
if [ "$1" = --version ]; then echo "pastor 1.0.0"; exit 0; fi
{{
  echo "argv: $*"
  env | grep -c '{TOKEN}' | sed 's/^/env hits: /'
  case " $* " in *--token-stdin*) read -r t; echo "stdin: $t" ;; esac
}} > "$LOG"
"#
    );
    let bin = pkg.join("pastor");
    std::fs::write(&bin, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let rel = dir.join("rel");
    std::fs::create_dir_all(&rel).unwrap();
    let tarball = rel.join(format!("{name}.tar.gz"));
    let ok = Command::new("tar")
        .arg("-czf")
        .arg(&tarball)
        .arg("-C")
        .arg(dir.join("pkg"))
        .arg(name)
        .status()
        .unwrap();
    assert!(ok.success());
    let sum = ["sha256sum", "shasum"]
        .iter()
        .find_map(|p| {
            let mut c = Command::new(p);
            if *p == "shasum" {
                c.args(["-a", "256"]);
            }
            c.arg(tarball.file_name().unwrap())
                .current_dir(&rel)
                .output()
                .ok()
                .filter(|o| o.status.success())
        })
        .expect("sha256sum or shasum");
    std::fs::write(rel.join("SHA256SUMS"), sum.stdout).unwrap();
}

fn install(dir: &Path, token_env: Option<&str>, args: &[&str]) -> Output {
    let mut c = Command::new("sh");
    c.arg(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh"))
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir.join("home"))
        .env("PASTOR_VERSION", "1.0.0")
        .env("LOG", dir.join("log"))
        .env(
            "PASTOR_DOWNLOAD_URL",
            format!("file://{}", dir.join("rel").display()),
        );
    if let Some(t) = token_env {
        c.env("PASTOR_JOIN_TOKEN", t);
    }
    c.output().unwrap()
}

fn log(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("log")).unwrap_or_default()
}

#[test]
fn join_hands_the_token_to_pastor_on_a_pipe() {
    // Whoever runs the suite as root would be refused, as the script does.
    if Command::new("id").arg("-u").output().unwrap().stdout == b"0\n" {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    release(dir.path());
    let out = install(dir.path(), Some(TOKEN), &["join", "--name", "pi-3"]);
    assert!(out.status.success(), "{out:?}");
    let text = log(dir.path());
    assert!(
        text.contains("argv: join --token-stdin --name pi-3\n"),
        "{text}"
    );
    assert!(text.contains(&format!("stdin: {TOKEN}\n")), "{text}");
    assert!(text.contains("env hits: 0"), "{text}");
    assert!(dir.path().join("home/.local/bin/pastor").exists());
    // Nothing the script printed has it either.
    let shown =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(!shown.contains(TOKEN), "{shown}");

    // No token given: pastor is left to ask on the terminal, and gets none
    // on stdin.
    std::fs::remove_file(dir.path().join("log")).unwrap();
    let out = install(dir.path(), None, &["join", "--json"]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(log(dir.path()), "argv: join --json\nenv hits: 0\n");

    // --token-stdin given with a token in the environment: once only.
    let out = install(dir.path(), Some(TOKEN), &["join", "--token-stdin"]);
    assert!(out.status.success(), "{out:?}");
    assert!(log(dir.path()).starts_with("argv: join --token-stdin\n"));
}

#[test]
fn a_token_as_an_argument_is_refused_before_anything_is_installed() {
    if Command::new("id").arg("-u").output().unwrap().stdout == b"0\n" {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    release(dir.path());
    let out = install(dir.path(), None, &["join", TOKEN]);
    assert!(!out.status.success());
    let shown = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(shown.contains("never an argument"), "{shown}");
    assert!(!shown.contains(TOKEN), "{shown}");
    assert!(!dir.path().join("home/.local/bin/pastor").exists());
}

/// Without `join` it is the install it always was: a token in the
/// environment is not read.
#[test]
fn the_plain_install_does_not_run_pastor_join() {
    let dir = tempfile::tempdir().unwrap();
    release(dir.path());
    let out = install(dir.path(), Some(TOKEN), &[]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(log(dir.path()), "");
}
