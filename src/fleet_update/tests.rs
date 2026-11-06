use std::path::Path;
use std::sync::Mutex;

use sha2::{Digest, Sha256};

use super::*;
use crate::setup::CmdOutput;

const NEW: &str = "0.11.0";

/// One command the update ran: which machine (by its login, or `here`),
/// the remote command, and whether a file went to its stdin.
#[derive(Debug, Clone)]
struct Call {
    machine: String,
    remote: String,
    stdin: bool,
}

/// A runner that answers as healthy machines at pastor 0.10.0 do, unless
/// told to fail one step on one machine.
#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<Call>>,
    /// The machine whose copy arrives with another sum.
    bad_sum: Option<&'static str>,
    /// The machine whose install script fails.
    fail_install: Option<&'static str>,
    /// The machine that says it runs this already.
    runs: Option<(&'static str, &'static str)>,
}

impl Fake {
    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    /// The machines a step ran on, in order: `copy` (a file on stdin) or
    /// `install`.
    fn ran(&self, step: &str) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|c| match step {
                "copy" => c.stdin,
                _ => c.remote.contains("pastor.new"),
            })
            .map(|c| c.machine)
            .collect()
    }
}

/// The command ssh runs on the machine: `sh -c '<it>'`, unquoted.
fn unwrap(remote: &str) -> String {
    match remote
        .strip_prefix("sh -c '")
        .and_then(|r| r.strip_suffix('\''))
    {
        Some(inner) => inner.replace("'\\''", "'"),
        None => remote.to_string(),
    }
}

fn ok(stdout: &str) -> std::io::Result<CmdOutput> {
    Ok(CmdOutput {
        success: true,
        stdout: format!("{stdout}\n"),
        stderr: String::new(),
    })
}

impl PushRunner for Fake {
    fn run(
        &self,
        program: &str,
        args: &[String],
        stdin: Option<&Path>,
    ) -> std::io::Result<CmdOutput> {
        let remote = unwrap(args.last().unwrap());
        let machine = match program {
            "sh" => "here".to_string(),
            _ => args
                .iter()
                .find(|a| a.starts_with("u@"))
                .map(|a| a[2..].to_string())
                .expect("an ssh login"),
        };
        self.calls.lock().unwrap().push(Call {
            machine: machine.clone(),
            remote: remote.clone(),
            stdin: stdin.is_some(),
        });
        if remote == LOOK {
            let runs = match self.runs {
                Some((m, v)) if m == machine => v,
                _ => "0.10.0",
            };
            return ok(&format!("Linux x86_64\n64\npastor {runs}"));
        }
        if let Some(file) = stdin {
            let sum = if self.bad_sum == Some(machine.as_str()) {
                "0".repeat(64)
            } else {
                hex(&std::fs::read(file).unwrap())
            };
            return Ok(CmdOutput {
                success: self.bad_sum != Some(machine.as_str()),
                stdout: format!("{sum}\n"),
                stderr: String::new(),
            });
        }
        if remote.contains("pastor.new") {
            if self.fail_install == Some(machine.as_str()) {
                return Ok(CmdOutput {
                    success: false,
                    stdout: String::new(),
                    stderr: "setup failed".into(),
                });
            }
            return ok(&format!("pastor {NEW}"));
        }
        panic!("unexpected command on {machine}: {remote}")
    }
}

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A head whose cache holds pastor `NEW` for x86_64 Linux, and a flock of
/// the head's own host listed first, two ssh machines and a pull machine.
struct Head {
    _dir: tempfile::TempDir,
    paths: Paths,
    flock: Flock,
}

fn head() -> Head {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(dir.path().join("c"), dir.path().join("s"));
    let a = Artifact::new("pastor", NEW, "x86_64-unknown-linux-musl").unwrap();
    let file = crate::artifacts::cache_path(&paths, &a);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, b"new pastor").unwrap();
    std::fs::write(file.with_file_name("pastor.sha256"), hex(b"new pastor")).unwrap();
    let flock = Flock::parse(
        Path::new("flock.toml"),
        "[[machine]]\nname = \"h\"\nlocal = true\n\
         [[machine]]\nname = \"a\"\nssh = \"u@a\"\n\
         [[machine]]\nname = \"b\"\nssh = \"u@b\"\n\
         [[machine]]\nname = \"p\"\npull = true\n",
    )
    .unwrap();
    Head {
        _dir: dir,
        paths,
        flock,
    }
}

fn run(h: &Head, fake: &Fake, live: &[(&str, usize)], dry_run: bool) -> Report {
    let ctx = Ctx {
        paths: &h.paths,
        flock: &h.flock,
        live: live.iter().map(|(m, n)| (m.to_string(), *n)).collect(),
        runner: fake,
    };
    update(&ctx, NEW, None, dry_run).unwrap()
}

fn row<'a>(r: &'a Report, machine: &str) -> &'a Row {
    r.rows
        .iter()
        .find(|x| x.machine == machine)
        .unwrap_or_else(|| panic!("no row for {machine}: {:?}", r.rows))
}

#[test]
fn dry_run_changes_nothing() {
    let h = head();
    let fake = Fake::default();
    let r = run(&h, &fake, &[], true);
    assert!(r.ok, "{r:?}");
    for m in ["a", "b", "h"] {
        assert_eq!(row(&r, m).status, Status::WouldUpdate, "{m}");
        assert_eq!(row(&r, m).from.as_deref(), Some("0.10.0"));
    }
    // Only looked: nothing copied, nothing installed.
    assert!(fake.ran("copy").is_empty(), "{:?}", fake.calls());
    assert!(fake.ran("install").is_empty(), "{:?}", fake.calls());
    assert!(fake.calls().iter().all(|c| c.remote == LOOK));
}

#[test]
fn sha_mismatch_keeps_the_old_binary() {
    let h = head();
    let fake = Fake {
        bad_sum: Some("a"),
        ..Fake::default()
    };
    let r = run(&h, &fake, &[], false);
    assert!(!r.ok);
    let a = row(&r, "a");
    assert_eq!(a.status, Status::Failed);
    assert!(a.why.as_deref().unwrap().contains("sha256"), "{a:?}");
    // No install ran anywhere: a's old binary is where it was.
    assert!(fake.ran("install").is_empty(), "{:?}", fake.ran("install"));

    // On a real shell: a copy whose sum is not the one asked for never
    // reaches the place the binary is installed from.
    let home = tempfile::tempdir().unwrap();
    let bin = home.path().join(".local/bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("pastor"), "old").unwrap();
    let out = sh(
        home.path(),
        &copy_command(STAGE, "pastor", &"0".repeat(64)),
        b"new",
    );
    assert!(!out.status.success());
    assert!(!home.path().join(".cache/pastor-update/pastor").exists());
    assert_eq!(std::fs::read_to_string(bin.join("pastor")).unwrap(), "old");
}

#[test]
fn a_failure_stops_the_rollout_before_the_head() {
    let h = head();
    let fake = Fake {
        fail_install: Some("a"),
        ..Fake::default()
    };
    let r = run(&h, &fake, &[], false);
    assert!(!r.ok);
    assert_eq!(row(&r, "a").status, Status::Failed);
    for m in ["b", "h"] {
        let x = row(&r, m);
        assert_eq!(x.status, Status::Skipped, "{m}");
        assert_eq!(x.why.as_deref(), Some("the rollout stopped at a"));
    }
    assert_eq!(fake.ran("copy"), ["a"]);
    assert_eq!(fake.ran("install"), ["a"]);
}

#[test]
fn the_head_is_updated_last() {
    let h = head();
    let fake = Fake::default();
    let r = run(&h, &fake, &[], false);
    assert!(r.ok, "{r:?}");
    assert_eq!(fake.ran("copy"), ["a", "b", "here"]);
    assert_eq!(fake.ran("install"), ["a", "b", "here"]);
    let last = r
        .rows
        .iter()
        .rfind(|x| x.status == Status::Updated)
        .unwrap();
    assert!(last.head && last.machine == "h", "{last:?}");
}

/// A machine with live tasks is updated: its tasks run in herdr, which the
/// update does not touch, and pastor's serve finds them again from the
/// store. A pull machine, whose own serve runs its tasks, is skipped and
/// listed.
#[test]
fn a_machine_with_live_tasks_is_updated_and_its_panes_left_alone() {
    let h = head();
    let fake = Fake::default();
    let r = run(&h, &fake, &[("a", 2), ("h", 1), ("p", 3)], false);
    assert!(r.ok, "{r:?}");
    let a = row(&r, "a");
    assert_eq!((a.status, a.live_tasks), (Status::Updated, 2));
    let head = row(&r, "h");
    assert_eq!((head.status, head.live_tasks), (Status::Updated, 1));
    let p = row(&r, "p");
    assert_eq!((p.status, p.live_tasks), (Status::Skipped, 3));
    assert!(p.why.as_deref().unwrap().contains("pull machine"), "{p:?}");
    // Nothing that stops or restarts herdr, or reaches into a pane: the
    // lines that restart or stop anything name pastor's serve only.
    for c in fake.calls() {
        for line in c.remote.lines() {
            assert!(!line.contains("pane") && !line.contains("kill"), "{line}");
            if ["restart", "kickstart", "stop"]
                .iter()
                .any(|w| line.contains(w))
            {
                assert!(!line.contains("herdr"), "{line}");
            }
        }
    }
    assert!(install_command(NEW).contains("systemctl --user restart pastor.service"));
}

#[test]
fn nothing_changes_when_the_cache_lacks_a_build() {
    let mut h = head();
    // An aarch64 machine, whose build the cache does not hold.
    h.flock = Flock::parse(
        Path::new("flock.toml"),
        "[[machine]]\nname = \"a\"\nssh = \"u@a\"\n[[machine]]\nname = \"b\"\nssh = \"u@b\"\n",
    )
    .unwrap();
    struct Arm(Fake);
    impl PushRunner for Arm {
        fn run(
            &self,
            program: &str,
            args: &[String],
            stdin: Option<&Path>,
        ) -> std::io::Result<CmdOutput> {
            if args.last().is_some_and(|r| unwrap(r) == LOOK) && args.iter().any(|a| a == "u@b") {
                return ok("Linux aarch64\n64\npastor 0.10.0");
            }
            self.0.run(program, args, stdin)
        }
    }
    let fake = Arm(Fake::default());
    let ctx = Ctx {
        paths: &h.paths,
        flock: &h.flock,
        live: HashMap::new(),
        runner: &fake,
    };
    let r = update(&ctx, NEW, None, false).unwrap();
    assert!(!r.ok);
    let b = row(&r, "b");
    assert_eq!(b.status, Status::Failed);
    assert!(
        b.why.as_deref().unwrap().contains("cache does not hold"),
        "{b:?}"
    );
    assert_eq!(row(&r, "a").status, Status::Skipped);
    assert!(fake.0.ran("copy").is_empty());
}

#[test]
fn a_machine_at_the_version_is_skipped_and_one_can_be_named() {
    let h = head();
    let fake = Fake {
        runs: Some(("b", NEW)),
        ..Fake::default()
    };
    let r = run(&h, &fake, &[], false);
    assert_eq!(row(&r, "b").status, Status::Skipped);
    assert_eq!(fake.ran("install"), ["a", "here"]);

    let fake = Fake::default();
    let ctx = Ctx {
        paths: &h.paths,
        flock: &h.flock,
        live: HashMap::new(),
        runner: &fake,
    };
    let r = update(&ctx, NEW, Some("b"), false).unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(fake.ran("install"), ["b"]);
    let err = update(&ctx, NEW, Some("ghost"), false).unwrap_err();
    assert_eq!(
        err.downcast_ref::<CliError>().unwrap().code,
        "unknown_machine"
    );
}

/// `script` run by `sh` with `HOME` set to `home` and `input` on stdin.
fn sh(home: &Path, script: &str, input: &[u8]) -> std::process::Output {
    use std::io::Write;
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .env("HOME", home)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

/// A stand-in pastor: `--version` says `version`, and `serve status`
/// succeeds when a serve runs, `serve stop` as `stop` says.
fn fake_pastor(file: &Path, version: &str, serving: bool, stop: bool) {
    use std::os::unix::fs::PermissionsExt;
    let status = if serving { 0 } else { 1 };
    let stop = if stop { 0 } else { 1 };
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(
        file,
        format!(
            "#!/bin/sh\ncase \"$1 $2\" in\n\"--version \") echo \"pastor {version}\";;\n\"serve status\") exit {status};;\n\"serve stop\") exit {stop};;\n*) exit 0;;\nesac\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// The install script on a real shell, on a machine with no units and no
/// serve: the new binary goes into ~/.local/bin and nothing is left over.
/// One that says another version, or a step that fails after the binary
/// moved, leaves the old one there.
#[test]
fn the_install_script_installs_or_puts_the_old_binary_back() {
    let home = tempfile::tempdir().unwrap();
    let bin = home.path().join(".local/bin/pastor");
    let staged = home.path().join(".cache/pastor-update/pastor");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    std::fs::write(&bin, "old").unwrap();

    fake_pastor(&staged, "1.0.0", false, true);
    let out = sh(home.path(), &install_command(NEW), b"");
    assert_eq!(out.status.code(), Some(4), "{out:?}");
    assert_eq!(std::fs::read_to_string(&bin).unwrap(), "old");

    // A serve runs and will not stop: the old binary goes back.
    fake_pastor(&staged, NEW, true, false);
    let out = sh(home.path(), &install_command(NEW), b"");
    assert!(!out.status.success(), "{out:?}");
    assert_eq!(std::fs::read_to_string(&bin).unwrap(), "old");

    fake_pastor(&staged, NEW, false, true);
    let out = sh(home.path(), &install_command(NEW), b"");
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim().lines().last(),
        Some(format!("pastor {NEW}").as_str())
    );
    assert!(std::fs::read_to_string(&bin).unwrap().contains(NEW));
    assert!(!staged.exists());
    assert!(!home.path().join(".local/bin/pastor.old").exists());
}
