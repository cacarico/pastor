//! Push and pull bootstrap against real containers: debian, fedora and
//! arch, each systemd as PID 1 with sshd and a `me` user with passwordless
//! sudo (`tests/fixtures/bootstrap/*.Dockerfile`). Opt-in: `make
//! test-bootstrap`, which runs every test here `--privileged
//! --cgroupns=host -v /sys/fs/cgroup:/sys/fs/cgroup:rw --network host` and
//! drives the real CLI against them, then `make test-bootstrap-clean`,
//! which removes every image and container labeled `pastor-bootstrap-test`.
//! Every test is `#[ignore]`; `--test-threads 1` is required, since
//! containers share `--network host` and a fixed sshd port (2222).
//!
//! The head is this test process itself, with a temp config and state dir,
//! exactly like `tests/real_ssh.rs`'s `Head`. Push runs in the head's
//! daemon (`SystemPushRunner` calls `ssh` and `ssh-keyscan` off its PATH),
//! so the daemon itself is started with a wrapper `ssh` first on its PATH
//! that always goes through this test's own `ssh_config` (the same trick
//! `real_ssh.rs`'s `Client` uses). Pull's container reaches this process's
//! own throwaway sshd at `127.0.0.1:2223`, since both share the host's
//! network namespace; the container's system-wide ssh config names that
//! `head`.
//!
//! **herdr, and pastor itself, never come from the network here.** The
//! daemon's own fetch is the real `CurlFetch`, which always passes `curl
//! --proto =https`; that refuses a `file://` herdr index outright, and
//! `herdr_source` additionally requires every asset URL in the index to
//! start with `https://`. So the suite seeds the head's checked artifact
//! cache directly (`seed_artifact`, using `pastor::artifacts`'s own public
//! `Artifact` and `cache_path`) and `artifacts::ensure` finds both already
//! cached and verified: push's copy step and a pull machine's
//! `ArtifactLocate` both read that cache. Both binaries are static, as the
//! releases are, so neither depends on a container's glibc: pastor is built
//! here for `x86_64-unknown-linux-musl` (`cargo zigbuild` when
//! cargo-zigbuild and zig are installed, as the release does, else `cargo
//! build` with a musl gcc), and herdr is the static linux-x86_64 binary
//! `PASTOR_TEST_HERDR_BIN` names.
//!
//! Every test first calls `prepare`, which checks the host (OpenSSH client
//! and server, ssh-keygen, tar), the herdr binary, the Rust musl target and
//! a C compiler for it, and docker, and returns `None` after printing why when one is missing. With
//! `PASTOR_E2E_REQUIRE=1` a skip panics instead, for a runner that must not
//! pass by skipping. A `docker build` or musl build that fails always
//! panics: a broken Dockerfile is a failure, not a missing tool.

use std::collections::HashSet;
use std::ffi::OsString;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use common::pastor;

mod common;

const WAIT: Duration = Duration::from_secs(180);
/// Baked into every fixture's sshd_config; see the Dockerfiles for why a
/// fixed port is safe here.
const SSHD_PORT: u16 = 2222;
/// The label on every image and container this suite makes, which `make
/// test-bootstrap-clean` removes by.
const LABEL: &str = "pastor-bootstrap-test";

fn run(cmd: &mut Command) -> Output {
    cmd.output().unwrap_or_else(|e| panic!("{:?}: {e}", cmd))
}

fn ok(out: Output) -> String {
    assert!(
        out.status.success(),
        "exit {:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn error_code(out: &Output) -> String {
    assert!(!out.status.success(), "expected a failure: {out:?}");
    let v: serde_json::Value = serde_json::from_slice(&out.stderr)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stderr)));
    v["code"].as_str().unwrap_or_default().to_string()
}

/// A program on PATH, or in the sbin dirs a non-root PATH often leaves out.
fn which(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(["/usr/sbin", "/usr/local/sbin", "/sbin"].map(PathBuf::from))
        .map(|d| d.join(program))
        .find(|p| p.is_file())
}

/// `which`, once `prepare` has checked the program is there.
fn find(program: &str) -> PathBuf {
    which(program)
        .unwrap_or_else(|| panic!("{program} not found: make test-bootstrap needs OpenSSH"))
}

fn keygen(path: &Path) -> String {
    let out = run(Command::new(find("ssh-keygen"))
        .args([
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            "pastor-bootstrap-test",
            "-f",
        ])
        .arg(path));
    ok(out);
    std::fs::read_to_string(path.with_extension("pub")).unwrap()
}

fn write_exec(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.trim().replace('\'', "'\\''"))
}

/// Whether `path` is a statically linked x86_64 ELF: no `PT_INTERP`
/// program header, so no dynamic loader and no libc of the container's to
/// match. `Err` says why it is not an x86_64 ELF at all.
fn is_static_x86_64_elf(path: &Path) -> Result<bool, String> {
    let b = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let what = |why: &str| format!("{} is not {why}", path.display());
    if b.len() < 64 || &b[..4] != b"\x7fELF" {
        return Err(what("an ELF binary"));
    }
    if b[4] != 2 || b[5] != 1 {
        return Err(what("a 64-bit little-endian ELF"));
    }
    let u16_at = |o: usize| usize::from(u16::from_le_bytes([b[o], b[o + 1]]));
    if u16_at(0x12) != 62 {
        return Err(what("an x86_64 binary"));
    }
    let phoff = u64::from_le_bytes(b[0x20..0x28].try_into().unwrap()) as usize;
    let (phentsize, phnum) = (u16_at(0x36), u16_at(0x38));
    for i in 0..phnum {
        let o = phoff + i * phentsize;
        let Some(kind) = b.get(o..o + 4) else {
            return Err(what("a whole ELF (its program headers are cut off)"));
        };
        // PT_INTERP
        if u32::from_le_bytes(kind.try_into().unwrap()) == 3 {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The static herdr to seed the head's cache with, from
/// `PASTOR_TEST_HERDR_BIN`. See the module doc for why this stands in for
/// a network fetch, and why it must be static.
fn host_herdr_bin() -> Result<PathBuf, String> {
    let Some(p) = std::env::var_os("PASTOR_TEST_HERDR_BIN") else {
        return Err(
            "PASTOR_TEST_HERDR_BIN is unset: set it to a static linux-x86_64 herdr \
                    binary (a release download); the real curl used to fetch one only reads \
                    https://, so there is no offline way to get one otherwise"
                .into(),
        );
    };
    let p = PathBuf::from(p);
    match is_static_x86_64_elf(&p) {
        Ok(true) => Ok(p),
        Ok(false) => Err(format!(
            "PASTOR_TEST_HERDR_BIN={} is dynamically linked; the containers need a static \
             linux-x86_64 herdr (a release download), not one built against this host's libc",
            p.display()
        )),
        Err(why) => Err(format!("PASTOR_TEST_HERDR_BIN: {why}")),
    }
}

/// Whether the Rust standard library for `PASTOR_TARGET` is installed, so
/// `musl_pastor` can build.
fn musl_target_installed() -> bool {
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    let Ok(out) = Command::new(rustc)
        .args(["--print", "target-libdir", "--target", PASTOR_TARGET])
        .output()
    else {
        return false;
    };
    let dir = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    out.status.success()
        && std::fs::read_dir(dir).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("libstd-"))
        })
}

/// Whether `cargo zigbuild` can build for musl here, as the release does:
/// cargo-zigbuild and zig, found where `make portability` looks.
fn zigbuild() -> bool {
    which("cargo-zigbuild").is_some()
        && (std::env::var_os("CARGO_ZIGBUILD_ZIG_PATH").is_some() || which("zig").is_some())
}

/// Whether something can compile the C in pastor's dependencies (SQLite)
/// for musl: zig through cargo-zigbuild, or a musl gcc cc-rs finds.
fn musl_cc() -> bool {
    zigbuild()
        || std::env::var_os("CC_x86_64_unknown_linux_musl").is_some()
        || which("x86_64-linux-musl-gcc").is_some()
}

/// A static release build of this crate's pastor for `PASTOR_TARGET`, made
/// once per run. A failed build panics: `prepare` checked the target and a
/// C compiler for it, so the build itself is broken.
fn musl_pastor() -> PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let mut cmd = Command::new(cargo);
        cmd.arg(if zigbuild() { "zigbuild" } else { "build" })
            .args([
                "--release",
                "--locked",
                "--target",
                PASTOR_TARGET,
                "--bin",
                "pastor",
                "--message-format=json-render-diagnostics",
            ])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .stderr(Stdio::inherit());
        let out = run(&mut cmd);
        assert!(
            out.status.success(),
            "the {PASTOR_TARGET} build of pastor failed; its errors are above"
        );
        let bin = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["reason"] == "compiler-artifact" && v["target"]["name"] == "pastor")
            .find_map(|v| v["executable"].as_str().map(PathBuf::from))
            .expect("the musl build named no pastor executable");
        assert_eq!(
            is_static_x86_64_elf(&bin),
            Ok(true),
            "{} is not a static x86_64 binary",
            bin.display()
        );
        bin
    })
    .clone()
}

/// Puts `src`'s bytes straight into the head's checked artifact cache,
/// under the same path and `.sha256` sidecar `artifacts::ensure` would
/// leave behind after a real download, so push's copy step and a pull
/// machine's `ArtifactLocate` both find it cached and verified already.
fn seed_artifact(
    paths: &pastor::config::Paths,
    name: &str,
    version: &str,
    target: &str,
    src: &Path,
) {
    use sha2::{Digest, Sha256};
    let artifact =
        pastor::artifacts::Artifact::new(name, version, target).unwrap_or_else(|e| panic!("{e}"));
    let dest = pastor::artifacts::cache_path(paths, &artifact);
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    std::fs::copy(src, &dest).unwrap_or_else(|e| panic!("copy {}: {e}", src.display()));
    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).unwrap();
    let bytes = std::fs::read(&dest).unwrap();
    let sum: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let mut sha_name = dest.file_name().unwrap().to_os_string();
    sha_name.push(".sha256");
    std::fs::write(dest.with_file_name(sha_name), format!("{sum}\n")).unwrap();
}

/// Seeds the static pastor and herdr a run uses into `paths`' cache.
fn seed_artifacts(paths: &pastor::config::Paths, bins: &Bins) {
    seed_artifact(
        paths,
        "pastor",
        env!("CARGO_PKG_VERSION"),
        PASTOR_TARGET,
        &bins.pastor,
    );
    seed_artifact(
        paths,
        "herdr",
        pastor::artifacts::DEFAULT_HERDR_VERSION,
        HERDR_TARGET,
        &bins.herdr,
    );
}

/// The pastor target `push::platform` reads back from a container's own
/// `uname -sm; getconf LONG_BIT`, and herdr's matching key
/// (`bootstrap::facts::target_for`). Every fixture runs linux/x86_64.
const PASTOR_TARGET: &str = "x86_64-unknown-linux-musl";
const HERDR_TARGET: &str = "linux-x86_64";

struct Distro {
    key: &'static str,
    dockerfile: &'static str,
    /// argv that removes git as root, for push_rerun_<distro>.
    remove_git: &'static [&'static str],
}

const DISTROS: &[Distro] = &[
    Distro {
        key: "debian",
        dockerfile: "debian.Dockerfile",
        remove_git: &["apt-get", "remove", "-y", "git"],
    },
    Distro {
        key: "fedora",
        dockerfile: "fedora.Dockerfile",
        remove_git: &["dnf", "remove", "-y", "git"],
    },
    Distro {
        key: "arch",
        dockerfile: "arch.Dockerfile",
        remove_git: &["pacman", "-R", "--noconfirm", "git"],
    },
];

fn distro(key: &str) -> &'static Distro {
    DISTROS.iter().find(|d| d.key == key).unwrap()
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bootstrap")
}

fn image(d: &Distro) -> String {
    format!("pastor-bootstrap-test-{}", d.key)
}

/// `docker build`s `d`'s image, labeled `LABEL`, once per run. A failed
/// build panics.
fn build_image(d: &Distro) -> String {
    static BUILT: Mutex<Option<HashSet<&'static str>>> = Mutex::new(None);
    let image = image(d);
    let mut built = BUILT.lock().unwrap_or_else(|e| e.into_inner());
    if built.get_or_insert_with(HashSet::new).contains(d.key) {
        return image;
    }
    let out = run(Command::new("docker")
        .args(["build", "-q", "--label", LABEL, "-f"])
        .arg(fixtures_dir().join(d.dockerfile))
        .args(["-t", &image])
        .arg(fixtures_dir()));
    assert!(
        out.status.success(),
        "docker build of {}: {}",
        d.dockerfile,
        String::from_utf8_lossy(&out.stderr).trim()
    );
    built.as_mut().unwrap().insert(d.key);
    image
}

/// docker may run a `--privileged --cgroupns=host` container here, checked
/// once per run with the debian image.
fn privileged_ok() -> Result<(), String> {
    static RESULT: OnceLock<Result<(), String>> = OnceLock::new();
    RESULT
        .get_or_init(|| {
            let image = build_image(distro("debian"));
            let out = Command::new("docker")
                .args([
                    "run",
                    "--rm",
                    "--label",
                    LABEL,
                    "--privileged",
                    "--cgroupns=host",
                    "--entrypoint",
                    "true",
                ])
                .arg(&image)
                .output();
            match out {
                Ok(o) if o.status.success() => Ok(()),
                Ok(o) => Err(format!(
                    "docker cannot run --privileged containers here: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                )),
                Err(e) => Err(format!("docker run failed: {e}")),
            }
        })
        .clone()
}

/// What a container test runs with: the static binaries to seed.
struct Bins {
    pastor: PathBuf,
    herdr: PathBuf,
}

/// Everything a container test needs, checked cheapest first, then `d`'s
/// image built and pastor's musl build made. `None` after printing why
/// `test` skips; with `PASTOR_E2E_REQUIRE=1`, a panic instead.
fn prepare(test: &str, d: &Distro) -> Option<Bins> {
    match missing() {
        Ok(herdr) => {
            build_image(d);
            Some(Bins {
                pastor: musl_pastor(),
                herdr,
            })
        }
        Err(why) => {
            if std::env::var("PASTOR_E2E_REQUIRE").is_ok_and(|v| v == "1") {
                panic!("{test} cannot run and PASTOR_E2E_REQUIRE=1: {why}");
            }
            eprintln!("skip {test}: {why}");
            None
        }
    }
}

/// Why a container test cannot run here, or the herdr to seed.
fn missing() -> Result<PathBuf, String> {
    for program in ["ssh", "sshd", "ssh-keygen", "ssh-keyscan", "tar"] {
        if which(program).is_none() {
            return Err(format!(
                "{program} is not installed on this host; make test-bootstrap needs OpenSSH's \
                 client and server, and tar"
            ));
        }
    }
    let herdr = host_herdr_bin()?;
    if !musl_target_installed() {
        return Err(format!(
            "the Rust target {PASTOR_TARGET} is not installed (`rustup target add \
             {PASTOR_TARGET}`); the containers get a static pastor, not this host's glibc build"
        ));
    }
    if !musl_cc() {
        return Err(format!(
            "no C compiler for {PASTOR_TARGET}: install cargo-zigbuild and zig (as `make \
             portability` needs), or a musl gcc as x86_64-linux-musl-gcc"
        ));
    }
    match Command::new("docker").arg("info").output() {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            return Err(format!(
                "docker info failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ));
        }
        Err(e) => return Err(format!("docker not found: {e}")),
    }
    privileged_ok()?;
    Ok(herdr)
}

/// The head's way into a push container: a fresh key, and a wrapper `ssh`
/// that always runs through its own `ssh_config` (no other key, no shared
/// known_hosts, `127.0.0.1:SSHD_PORT` as `me`). Made before the head, so
/// the daemon, which runs the push, starts with the wrapper on its PATH.
/// Everything pastor's push code invokes besides `ssh` (`ssh-keygen`,
/// `ssh-keyscan`) stays the real one, found later on the same PATH.
struct SshClient {
    _tmp: tempfile::TempDir,
    pubkey: String,
    path: OsString,
}

impl SshClient {
    fn new() -> SshClient {
        let tmp = tempfile::tempdir().unwrap();
        let key = tmp.path().join("key");
        let pubkey = keygen(&key);
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let ssh_config = tmp.path().join("ssh_config");
        std::fs::write(
            &ssh_config,
            format!(
                "Host 127.0.0.1\n\
                 \tPort {SSHD_PORT}\n\
                 \tUser me\n\
                 \tIdentityFile {}\n\
                 \tIdentitiesOnly yes\n\
                 \tIdentityAgent none\n\
                 \tGlobalKnownHostsFile /dev/null\n",
                key.display(),
            ),
        )
        .unwrap();
        write_exec(
            &bin.join("ssh"),
            &format!(
                "#!/bin/sh\nexec '{}' -F '{}' \"$@\"\n",
                find("ssh").display(),
                ssh_config.display(),
            ),
        );
        let mut path = OsString::from(&bin);
        path.push(":");
        path.push(std::env::var_os("PATH").unwrap_or_default());
        SshClient {
            _tmp: tmp,
            pubkey,
            path,
        }
    }

    fn login(&self) -> &str {
        "me@127.0.0.1"
    }
}

/// A running container built from one of the fixtures: systemd as PID 1,
/// sshd on `SSHD_PORT`, and (with `authorize`) a `me` whose
/// authorized_keys holds this run's own ephemeral key.
struct Container {
    name: String,
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", "-v", &self.name])
            .output();
    }
}

impl Container {
    /// `prepare` built the image already.
    fn start(d: &Distro, authorize: Option<&str>) -> Container {
        let name = format!("pastor-bootstrap-test-{}-{}", d.key, std::process::id());
        let _ = Command::new("docker")
            .args(["rm", "-f", "-v", &name])
            .output();
        let image = image(d);
        let run_out = run(Command::new("docker").args([
            "run",
            "-d",
            "--name",
            &name,
            "--label",
            LABEL,
            "--privileged",
            "--cgroupns=host",
            "-v",
            "/sys/fs/cgroup:/sys/fs/cgroup:rw",
            "--network",
            "host",
            "--tmpfs",
            "/tmp",
            "--tmpfs",
            "/run",
            "--tmpfs",
            "/run/lock",
            &image,
        ]));
        ok(run_out);

        let c = Container { name };
        c.wait_system_running(d);
        if let Some(pubkey) = authorize {
            c.inject_key(pubkey);
        }
        c.wait_sshd_ready();
        c
    }

    fn wait_system_running(&self, d: &Distro) {
        let deadline = Instant::now() + WAIT;
        loop {
            let out = Command::new("docker")
                .args(["exec", &self.name, "systemctl", "is-system-running"])
                .output();
            if let Ok(out) = &out {
                let state = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if state == "running" || state == "degraded" {
                    return;
                }
            }
            assert!(
                Instant::now() < deadline,
                "{} never reached multi-user.target: {out:?}",
                d.key
            );
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    fn inject_key(&self, pubkey: &str) {
        let script = format!(
            "printf '%s\\n' {} > /home/me/.ssh/authorized_keys && chown me:me \
             /home/me/.ssh/authorized_keys && chmod 600 /home/me/.ssh/authorized_keys",
            shell_single_quote(pubkey.trim())
        );
        let out = run(Command::new("docker").args(["exec", &self.name, "sh", "-c", &script]));
        ok(out);
    }

    fn wait_sshd_ready(&self) {
        let deadline = Instant::now() + WAIT;
        while std::net::TcpStream::connect(("127.0.0.1", SSHD_PORT)).is_err() {
            assert!(
                Instant::now() < deadline,
                "sshd on {} never came up",
                self.name
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// A command inside the container, as root (docker exec's default).
    fn exec(&self, args: &[&str]) -> Output {
        run(Command::new("docker").args(["exec", &self.name]).args(args))
    }

    /// A command inside the container, as `me`, through its own
    /// passwordless sudo (so it does not need a tty or a password, unlike
    /// `docker exec --user me ... sudo ...` which still needs `-n`).
    fn exec_sudo_as_me(&self, args: &[&str]) -> Output {
        let mut full = vec!["exec", "--user", "me", self.name.as_str(), "sudo", "-n"];
        full.extend_from_slice(args);
        run(Command::new("docker").args(full))
    }

    /// Removes git as root through `me`'s own sudo, for push_rerun.
    fn remove_git(&self, d: &Distro) {
        let out = self.exec_sudo_as_me(d.remove_git);
        assert!(
            out.status.success(),
            "remove git on {}: {}",
            d.key,
            String::from_utf8_lossy(&out.stderr)
        );
        let still_there = self.exec(&["sh", "-c", "command -v git"]);
        assert!(
            !still_there.status.success(),
            "git is still on {}'s PATH after removing it",
            d.key
        );
    }

    /// Waits for `unit` to be active in `user`'s own systemd manager,
    /// reached the way a login session would: as that user, with its
    /// runtime dir, so `systemctl --user` finds the manager's bus there.
    fn user_service_active(&self, user: &str, unit: &str) -> bool {
        let uid = ok(self.exec(&["id", "-u", user])).trim().to_string();
        let runtime_dir = format!("XDG_RUNTIME_DIR=/run/user/{uid}");
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let out = self.exec(&[
                "runuser",
                "-u",
                user,
                "--",
                "env",
                &runtime_dir,
                "systemctl",
                "--user",
                "is-active",
                unit,
            ]);
            if String::from_utf8_lossy(&out.stdout).trim() == "active" {
                return true;
            }
            if Instant::now() >= deadline {
                eprintln!("{unit} as {user}: {out:?}");
                return false;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Root can read `file`, so the path is right, and `user` cannot.
    fn assert_only_root_reads(&self, file: &str, user: &str) {
        let root = self.exec(&["cat", file]);
        assert!(
            root.status.success() && !root.stdout.is_empty(),
            "root cannot read {file} on {}, so the check below proves nothing: {root:?}",
            self.name
        );
        let other = self.exec(&["runuser", "-u", user, "--", "cat", file]);
        assert!(
            !other.status.success(),
            "{user} could read {file} on {}: {other:?}",
            self.name
        );
    }
}

/// The head: this test process's own `pastor serve --foreground`, with a
/// temp config and state dir and an empty flock. Enough for push, which
/// never needs an inbound ssh of its own.
struct Head {
    tmp: tempfile::TempDir,
    config: PathBuf,
    state: PathBuf,
    data: PathBuf,
    /// The PATH with `SshClient`'s wrapper first, for the daemon and the CLI.
    path: OsString,
    serve: Child,
}

impl Drop for Head {
    fn drop(&mut self) {
        let _ = self.serve.kill();
        let _ = self.serve.wait();
    }
}

impl Head {
    fn start(ssh: &SshClient) -> Head {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("c");
        let state = tmp.path().join("s");
        let data = tmp.path().join("d");
        std::fs::create_dir_all(&config).unwrap();
        // head_address is never actually dialed in the push tests (the
        // machine never needs to reach back during them); it only has to
        // be a syntactically valid ssh destination.
        std::fs::write(
            config.join("pastor.toml"),
            "tick = \"1s\"\nsettle = \"1s\"\nreconcile_every = \"1s\"\n\
             head_address = \"pastor-bootstrap-test@head.invalid\"\n",
        )
        .unwrap();
        // The push runs in this process (`SystemPushRunner`), so this is
        // the PATH that has to find the wrapper ssh.
        let serve = pastor()
            .args(["serve", "--foreground"])
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .env("PASTOR_DATA_DIR", &data)
            .env("PATH", &ssh.path)
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(tmp.path().join("serve.log")).unwrap())
            .spawn()
            .unwrap();
        let mut head = Head {
            tmp,
            config,
            state,
            data,
            path: ssh.path.clone(),
            serve,
        };
        head.wait_ready();
        head
    }

    fn wait_ready(&mut self) {
        let deadline = Instant::now() + WAIT;
        loop {
            if self
                .cmd(&["machine", "list", "--json"])
                .output()
                .is_ok_and(|o| o.status.success())
            {
                return;
            }
            let exited = self.serve.try_wait().unwrap().is_some();
            assert!(
                !exited && Instant::now() < deadline,
                "the head never came up\n--- serve ---\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.tmp.path().join("serve.log")).unwrap_or_default()
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = pastor();
        c.args(args)
            .env("PASTOR_CONFIG_DIR", &self.config)
            .env("PASTOR_STATE_DIR", &self.state)
            .env("PASTOR_DATA_DIR", &self.data)
            .env("PATH", &self.path);
        c
    }

    fn paths(&self) -> pastor::config::Paths {
        pastor::config::Paths::new(self.config.clone(), self.state.clone())
            .with_data_dir(self.data.clone())
    }

    /// `machine add NAME --ssh <login> --bootstrap [--sudo]`.
    fn bootstrap(&self, ssh: &SshClient, name: &str, sudo: bool) -> Output {
        let mut args = vec!["machine", "add", name, "--ssh", ssh.login(), "--bootstrap"];
        if sudo {
            args.push("--sudo");
        }
        let out = run(&mut self.cmd(&args));
        if !out.status.success() {
            eprintln!("--- serve ---\n{}", self.log());
        }
        out
    }
}

/// A head, a container that lets the head's key in as `me`, and the
/// head's cache seeded.
fn push_setup(test: &str, d: &Distro) -> Option<(SshClient, Head, Container)> {
    let bins = prepare(test, d)?;
    let ssh = SshClient::new();
    let head = Head::start(&ssh);
    seed_artifacts(&head.paths(), &bins);
    let container = Container::start(d, Some(&ssh.pubkey));
    Some((ssh, head, container))
}

/// `push_root_<distro>`: `machine add --bootstrap --sudo` against a bare
/// container ends ok, in root mode; herdr is active as `pastor-agent`
/// through its systemd user service, and `pastor-agent` cannot read `me`'s
/// machine key (`service_state_dir_is_0700_and_owned` at the container
/// level, per the plan's Review Focus).
fn run_push_root(key: &str) {
    let d = distro(key);
    let Some((ssh, head, container)) = push_setup(&format!("push_root_{key}"), d) else {
        return;
    };

    let text = ok(head.bootstrap(&ssh, "root-1", true));
    assert!(text.contains("root mode"), "{text}");

    assert!(
        container.user_service_active("pastor-agent", "herdr.service"),
        "herdr is not active as pastor-agent on {key}"
    );

    container.assert_only_root_reads("/home/me/.local/state/pastor/machine_key", "pastor-agent");
}

/// `push_user_<distro>`: `machine add --bootstrap` with no `--sudo` ends
/// ok, in user mode with isolation none: no users are created, and herdr
/// runs as `me` itself. User mode never enables lingering (that is root
/// work), so the test does, as an admin would beforehand.
fn run_push_user(key: &str) {
    let d = distro(key);
    let Some((ssh, head, container)) = push_setup(&format!("push_user_{key}"), d) else {
        return;
    };
    ok(container.exec(&["loginctl", "enable-linger", "me"]));

    let text = ok(head.bootstrap(&ssh, "user-1", false));
    assert!(text.contains("user mode, isolation: none"), "{text}");

    assert!(
        container.user_service_active("me", "herdr.service"),
        "herdr is not active as me on {key}"
    );
}

/// `push_rerun_<distro>`: with git removed, a bootstrap with no `--sudo`
/// fails at the packages step (`bootstrap_missing`); a rerun of the same
/// machine with `--sudo` resumes instead of `machine_exists`, and ends ok
/// (the plan's `rerun_after_failure_resumes` pin, at the container level).
fn run_push_rerun(key: &str) {
    let d = distro(key);
    let Some((ssh, head, container)) = push_setup(&format!("push_rerun_{key}"), d) else {
        return;
    };
    container.remove_git(d);

    let first = head.bootstrap(&ssh, "rerun-1", false);
    assert_eq!(error_code(&first), "bootstrap_missing", "{first:?}");

    let text = ok(head.bootstrap(&ssh, "rerun-1", true));
    assert!(text.contains("root mode"), "{text}");
}

#[test]
#[ignore = "needs docker; run with make test-bootstrap"]
fn push_root_debian() {
    run_push_root("debian");
}

#[test]
#[ignore = "needs docker; run with make test-bootstrap"]
fn push_root_fedora() {
    run_push_root("fedora");
}

#[test]
#[ignore = "needs docker; run with make test-bootstrap"]
fn push_root_arch() {
    run_push_root("arch");
}

#[test]
#[ignore = "needs docker; run with make test-bootstrap"]
fn push_user_debian() {
    run_push_user("debian");
}

#[test]
#[ignore = "needs docker; run with make test-bootstrap"]
fn push_user_fedora() {
    run_push_user("fedora");
}

#[test]
#[ignore = "needs docker; run with make test-bootstrap"]
fn push_user_arch() {
    run_push_user("arch");
}

#[test]
#[ignore = "needs docker; run with make test-bootstrap"]
fn push_rerun_debian() {
    run_push_rerun("debian");
}

#[test]
#[ignore = "needs docker; run with make test-bootstrap"]
fn push_rerun_fedora() {
    run_push_rerun("fedora");
}

#[test]
#[ignore = "needs docker; run with make test-bootstrap"]
fn push_rerun_arch() {
    run_push_rerun("arch");
}

/// A throwaway sshd on the test host, landing logins on this process's own
/// `pastor serve`, exactly like `tests/real_ssh.rs`'s `Head`. The pull
/// container reaches it at `127.0.0.1:JOIN_PORT` because it runs
/// `--network host`; its ssh config calls that `head`.
struct JoinHead {
    tmp: tempfile::TempDir,
    config: PathBuf,
    state: PathBuf,
    data: PathBuf,
    port: u16,
    children: Vec<Child>,
}

/// --network host means the joining container reaches the head's sshd on
/// this same port on 127.0.0.1, so it is fixed too, like the containers'.
const JOIN_PORT: u16 = SSHD_PORT + 1;

impl Drop for JoinHead {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl JoinHead {
    fn start() -> JoinHead {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("c");
        let state = tmp.path().join("s");
        let data = tmp.path().join("d");
        std::fs::create_dir_all(&config).unwrap();
        let user = ok(run(Command::new("id").arg("-un"))).trim().to_string();
        // `head` is no real name: the container's ssh config maps it to
        // 127.0.0.1:JOIN_PORT, and its host key is pinned under `head`
        // (the token's host key line is named after this address's host).
        std::fs::write(
            config.join("pastor.toml"),
            format!(
                "tick = \"1s\"\nsettle = \"1s\"\nreconcile_every = \"1s\"\n\
                 head_address = \"{user}@head\"\n"
            ),
        )
        .unwrap();
        let host_key = tmp.path().join("host_ed25519");
        keygen(&host_key);
        std::fs::write(tmp.path().join("authorized_keys"), "").unwrap();
        let serve = pastor()
            .args(["serve", "--foreground"])
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .env("PASTOR_DATA_DIR", &data)
            .env("PASTOR_AUTHORIZED_KEYS", tmp.path().join("authorized_keys"))
            .env("PASTOR_SSH_HOST_KEY", host_key.with_extension("pub"))
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(tmp.path().join("serve.log")).unwrap())
            .spawn()
            .unwrap();

        let port = JOIN_PORT;
        let sshd_config = tmp.path().join("sshd_config");
        std::fs::write(
            &sshd_config,
            format!(
                "Port {port}\n\
                 ListenAddress 127.0.0.1\n\
                 HostKey {}\n\
                 PidFile {}\n\
                 AuthorizedKeysFile {}\n\
                 StrictModes no\n\
                 UsePAM no\n\
                 PubkeyAuthentication yes\n\
                 PasswordAuthentication no\n\
                 KbdInteractiveAuthentication no\n\
                 SetEnv PASTOR_CONFIG_DIR={} PASTOR_STATE_DIR={} PASTOR_DATA_DIR={}\n",
                host_key.display(),
                tmp.path().join("sshd.pid").display(),
                tmp.path().join("authorized_keys").display(),
                config.display(),
                state.display(),
                data.display(),
            ),
        )
        .unwrap();
        let sshd = Command::new(find("sshd"))
            .args(["-D", "-e", "-f"])
            .arg(&sshd_config)
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(tmp.path().join("sshd.log")).unwrap())
            .spawn()
            .unwrap();

        let mut head = JoinHead {
            tmp,
            config,
            state,
            data,
            port,
            children: vec![serve, sshd],
        };
        head.wait_ready();
        head
    }

    fn log(&self, name: &str) -> String {
        std::fs::read_to_string(self.tmp.path().join(name)).unwrap_or_default()
    }

    fn wait_ready(&mut self) {
        let deadline = Instant::now() + WAIT;
        loop {
            let up = self
                .cmd(&["machine", "list", "--json"])
                .output()
                .is_ok_and(|o| o.status.success());
            let listening = std::net::TcpStream::connect(("127.0.0.1", self.port)).is_ok();
            if up && listening {
                return;
            }
            let exited = self
                .children
                .iter_mut()
                .any(|c| c.try_wait().unwrap().is_some());
            assert!(
                !exited && Instant::now() < deadline,
                "the head or sshd never came up\n--- serve ---\n{}\n--- sshd ---\n{}",
                self.log("serve.log"),
                self.log("sshd.log")
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = pastor();
        c.args(args)
            .env("PASTOR_CONFIG_DIR", &self.config)
            .env("PASTOR_STATE_DIR", &self.state)
            .env("PASTOR_DATA_DIR", &self.data);
        c
    }

    fn paths(&self) -> pastor::config::Paths {
        pastor::config::Paths::new(self.config.clone(), self.state.clone())
            .with_data_dir(self.data.clone())
    }
}

/// A local file mirror for `install.sh`'s `PASTOR_DOWNLOAD_URL`: a tarball
/// named exactly as `install.sh` expects, plus `SHA256SUMS`. `install.sh`'s
/// own downloads pass `--proto-redir =https`, which (unlike the daemon's
/// `--proto =https`) restricts only redirects, not the initial URL, so a
/// `file://` base genuinely works here, offline, with the real script.
struct Mirror {
    dir: tempfile::TempDir,
}

impl Mirror {
    /// `src` becomes `pastor-<version>-<target>.tar.gz` under `pastor/`
    /// inside the tarball, as a release's own tarball lays it out.
    fn build(version: &str, target: &str, src: &Path) -> Mirror {
        let dir = tempfile::tempdir().unwrap();
        let name = format!("pastor-{version}-{target}");
        let stage = dir.path().join(&name);
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::copy(src, stage.join("pastor")).unwrap();
        std::fs::set_permissions(stage.join("pastor"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let tarball = format!("{name}.tar.gz");
        let tar = run(Command::new("tar")
            .arg("-czf")
            .arg(dir.path().join(&tarball))
            .arg("-C")
            .arg(dir.path())
            .arg(&name));
        ok(tar);
        let bytes = std::fs::read(dir.path().join(&tarball)).unwrap();
        use sha2::{Digest, Sha256};
        let sum: String = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        std::fs::write(dir.path().join("SHA256SUMS"), format!("{sum}  {tarball}\n")).unwrap();
        Mirror { dir }
    }
}

/// `pull_join_debian`: `install.sh join --token-stdin` inside a bare
/// container, against this head over the host network (`--network host`,
/// with `head` resolved to `127.0.0.1:JOIN_PORT` by the container's ssh
/// config); the full bootstrap that follows ends ok, and the same token
/// used again gets `join_token_invalid` with "Permission denied" from ssh
/// (`token_line_removed_after_use`, at the container level).
#[test]
#[ignore = "needs docker; run with make test-bootstrap"]
fn pull_join_debian() {
    let d = distro("debian");
    let Some(bins) = prepare("pull_join_debian", d) else {
        return;
    };

    let head = JoinHead::start();
    seed_artifacts(&head.paths(), &bins);
    let container = Container::start(d, None);

    // System-wide, not /root/.ssh/config: root runs the join, but the
    // shepherd that dials the head afterwards runs as the service user
    // `pastor`. HostKeyAlias keeps the lookup on the `head` line the join
    // pins, not `[127.0.0.1]:2223`.
    let head_config =
        format!("Host head\n\tHostName 127.0.0.1\n\tPort {JOIN_PORT}\n\tHostKeyAlias head\n");
    ok(container.exec(&[
        "sh",
        "-c",
        "mkdir -p /etc/ssh/ssh_config.d && printf '%s' \"$1\" > /etc/ssh/ssh_config.d/pastor-head.conf",
        "sh",
        &head_config,
    ]));

    let made: serde_json::Value =
        serde_json::from_str(&ok(run(&mut head.cmd(&["join", "create", "--json"])))).unwrap();
    let token = made["token"].as_str().unwrap().to_string();

    let mirror = Mirror::build(env!("CARGO_PKG_VERSION"), PASTOR_TARGET, &bins.pastor);
    let mirror_dir = "/tmp/pastor-mirror";
    ok(container.exec(&["mkdir", "-p", mirror_dir]));
    for entry in std::fs::read_dir(mirror.dir.path()).unwrap() {
        let entry = entry.unwrap();
        let dest = format!("{mirror_dir}/{}", entry.file_name().to_string_lossy());
        let cp = run(Command::new("docker").args([
            "cp",
            entry.path().to_str().unwrap(),
            &format!("{}:{dest}", container.name),
        ]));
        ok(cp);
    }
    let install_sh = Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh");
    let cp = run(Command::new("docker").args([
        "cp",
        install_sh.to_str().unwrap(),
        &format!("{}:/tmp/install.sh", container.name),
    ]));
    ok(cp);

    let join = |token: &str| -> Output {
        // `-e` on `docker exec` itself: a `.env` on the host-side `docker`
        // command would only change the CLI's own environment, not the
        // container's, since docker does not forward it automatically.
        let mut cmd = Command::new("docker");
        cmd.args([
            "exec",
            "-i",
            "-e",
            &format!("PASTOR_DOWNLOAD_URL=file://{mirror_dir}"),
            // Without this, install.sh's "latest" default asks GitHub's
            // API over the real network for a version to install.
            "-e",
            &format!("PASTOR_VERSION={}", env!("CARGO_PKG_VERSION")),
            &container.name,
            "sh",
            "/tmp/install.sh",
            "join",
            "--token-stdin",
            "--name",
            "pull-debian-1",
            "--service-user",
            "pastor",
            "--json",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        writeln!(child.stdin.take().unwrap(), "{token}").unwrap();
        child.wait_with_output().unwrap()
    };

    let out = join(&token);
    let text = ok(out);
    let outcome: serde_json::Value = text
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str(l).ok())
        .unwrap_or_else(|| panic!("join printed no JSON outcome: {text}"));
    assert_eq!(outcome["ok"], true, "{outcome}");
    assert_eq!(outcome["mode"], "root", "{outcome}");
    assert_eq!(outcome["isolation"], "users", "{outcome}");

    let flock = std::fs::read_to_string(head.config.join("flock.toml")).unwrap();
    let entry = flock
        .split("[[machine]]")
        .find(|e| e.contains("\"pull-debian-1\""))
        .unwrap_or_else(|| panic!("{flock}"));
    assert!(entry.contains("pull = true"), "{flock}");

    let keys = std::fs::read_to_string(head.tmp.path().join("authorized_keys")).unwrap();
    assert!(
        !keys.contains("pastor:join:"),
        "the token's line is gone: {keys}"
    );

    // The same token again, this time with the pastor the first join just
    // installed: sshd no longer has the token's key at all
    // (`token_line_removed_after_use`).
    let again = join_again(&container.name, &token);
    assert_eq!(error_code(&again), "join_token_invalid", "{again:?}");
    let message = String::from_utf8_lossy(&again.stderr);
    assert!(message.contains("Permission denied"), "{message}");

    // Wait for the pull machine's own shepherd (running as `pastor`, its
    // service user) to claim and report the bootstrap outcome it left
    // behind, exactly as `real_ssh.rs`'s `pull_join_end_to_end` does.
    let deadline = Instant::now() + WAIT;
    let state = loop {
        let describe = run(&mut head.cmd(&["machine", "describe", "pull-debian-1", "--json"]));
        if describe.status.success() {
            let v: serde_json::Value = serde_json::from_slice(&describe.stdout).unwrap();
            let state = &v["bootstrap"];
            assert_ne!(state["bootstrap"], "failed", "{v}");
            if state["bootstrap"] == "ok" {
                break state.clone();
            }
        }
        assert!(
            Instant::now() < deadline,
            "pull-debian-1 never reported ok\n--- serve ---\n{}",
            head.log("serve.log")
        );
        std::thread::sleep(Duration::from_millis(300));
    };
    assert_eq!(state["mode"], "root", "{state}");
    assert_eq!(state["isolation"], "users", "{state}");
    assert_eq!(state["service_user"], "pastor", "{state}");

    // The pastor-agent key's own isolation, same as push_root's: the
    // service user is `pastor`, not `me`, by `--service-user` above.
    container.assert_only_root_reads(
        "/home/pastor/.local/state/pastor/machine_key",
        "pastor-agent",
    );

    // A task actually runs there: the shepherd claims it and reaches
    // pastor-agent's herdr through its `[shepherd] command`, `sudo -n -u
    // pastor-agent herdr ...`, which a unit with NoNewPrivileges would
    // stop (sudo's setuid bit ignored), leaving the task without a pane.
    let t: serde_json::Value = serde_json::from_str(&ok(run(&mut head.cmd(&[
        "task",
        "run",
        "say hello",
        "--machine",
        "pull-debian-1",
        "--repo",
        "/tmp",
        "--json",
    ]))))
    .unwrap();
    let id = format!("t-{}", t["id"]);
    let deadline = Instant::now() + WAIT;
    loop {
        let v: serde_json::Value = serde_json::from_str(&ok(run(
            &mut head.cmd(&["task", "describe", &id, "--json"])
        )))
        .unwrap();
        assert_eq!(v["machine"], "pull-debian-1", "{v}");
        if v["pane_id"].is_string() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{id} never started on pull-debian-1: {v}\n--- serve ---\n{}",
            head.log("serve.log")
        );
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// The same token a second time, with the pastor the first join already
/// installed at `/root/.local/bin/pastor`.
fn join_again(container: &str, token: &str) -> Output {
    let mut child = Command::new("docker")
        .args([
            "exec",
            "-i",
            container,
            "/root/.local/bin/pastor",
            "join",
            "--token-stdin",
            "--name",
            "pull-debian-2",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(child.stdin.take().unwrap(), "{token}").unwrap();
    child.wait_with_output().unwrap()
}
