//! Push bootstrap, and a pull machine joining by token, against real containers: Debian, Fedora and Arch, each
//! with systemd as PID 1, sshd and a login user `me` with lingering on and no
//! sudo rights (`tests/fixtures/bootstrap/*.Dockerfile`), so a step that reached for
//! root fails the test. Opt-in: `make test-bootstrap` runs every test here
//! with rootless podman (podman only: the containers run systemd as PID 1
//! with `--systemd=always`, which docker lacks) and `--test-threads 1`, since the containers publish one fixed port; then
//! `make test-bootstrap-clean` removes every image and container labeled
//! `pastor-bootstrap-test`. Every test is `#[ignore]`.
//!
//! The head is this test process's own `pastor serve`, with a temp config and
//! state dir. The push runs in that daemon, which calls `ssh` off its PATH,
//! so it starts with a wrapper `ssh` first on the PATH that goes through
//! this test's own `ssh_config` (the key, `127.0.0.1:2222` as `me`, no shared
//! known_hosts), the trick `real_ssh.rs` uses. Its authorized_keys is a temp
//! file too, never the user's.
//!
//! **pastor and herdr never come from the network.** The head's checked
//! artifact cache is seeded directly (as `artifacts::ensure` leaves it after
//! a download), so the push's copy step finds both cached. pastor is the
//! binary this test run built, linked against this host's glibc, so the
//! fixtures' libc must be at least as new (Debian 13, Fedora 42 and Arch
//! are for a host as old as Debian 13); on a newer host,
//! `PASTOR_TEST_PASTOR_BIN` names a pastor built for an older glibc or a
//! static (musl) one. herdr is a static linux-x86_64
//! binary: `PASTOR_TEST_HERDR_BIN`, else the `herdr` on the PATH when it is
//! static.
//!
//! A test whose tools are missing prints why and returns; with
//! `PASTOR_E2E_REQUIRE=1` it panics instead, for a runner that must not pass
//! by skipping. An image that fails to build always panics.

use std::collections::HashSet;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use common::pastor;

mod common;

const WAIT: Duration = Duration::from_secs(180);
/// Baked into every fixture's sshd_config and published on the host's
/// loopback.
const SSHD_PORT: u16 = 2222;
/// On every image and container this suite makes, which `make
/// test-bootstrap-clean` removes by.
const LABEL: &str = "pastor-bootstrap-test";
/// What `bootstrap::push` reads back from a linux/x86_64 container.
const PASTOR_TARGET: &str = "x86_64-unknown-linux-musl";
const HERDR_TARGET: &str = "linux-x86_64";

fn run(cmd: &mut Command) -> Output {
    cmd.output().unwrap_or_else(|e| panic!("{cmd:?}: {e}"))
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

fn write_exec(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn container(args: &[&str]) -> Command {
    let mut c = Command::new("podman");
    c.args(args);
    c
}

/// Whether `path` is a statically linked x86_64 ELF (no `PT_INTERP`).
fn is_static_x86_64_elf(path: &Path) -> bool {
    let Ok(b) = std::fs::read(path) else {
        return false;
    };
    if b.len() < 64 || &b[..4] != b"\x7fELF" || b[4] != 2 || b[5] != 1 {
        return false;
    }
    let u16_at = |o: usize| usize::from(u16::from_le_bytes([b[o], b[o + 1]]));
    if u16_at(0x12) != 62 {
        return false;
    }
    let phoff = u64::from_le_bytes(b[0x20..0x28].try_into().unwrap()) as usize;
    let (phentsize, phnum) = (u16_at(0x36), u16_at(0x38));
    (0..phnum).all(|i| {
        b.get(phoff + i * phentsize..phoff + i * phentsize + 4)
            .is_some_and(|k| u32::from_le_bytes(k.try_into().unwrap()) != 3)
    })
}

/// The static herdr to seed the cache with.
fn herdr_bin() -> Result<PathBuf, String> {
    let named = std::env::var_os("PASTOR_TEST_HERDR_BIN").map(PathBuf::from);
    let p = named
        .or_else(|| which("herdr"))
        .ok_or("no herdr: set PASTOR_TEST_HERDR_BIN to a static linux-x86_64 herdr")?;
    if is_static_x86_64_elf(&p) {
        Ok(p)
    } else {
        Err(format!(
            "{} is not a static linux-x86_64 binary; set PASTOR_TEST_HERDR_BIN to one",
            p.display()
        ))
    }
}

/// Puts `src` into the head's artifact cache where `artifacts::ensure`
/// would leave a verified download.
fn seed_artifact(
    paths: &pastor::config::Paths,
    name: &str,
    version: &str,
    target: &str,
    src: &Path,
) {
    use sha2::{Digest, Sha256};
    let artifact = pastor::artifacts::Artifact::new(name, version, target).unwrap();
    let dest = pastor::artifacts::cache_path(paths, &artifact);
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    std::fs::copy(src, &dest).unwrap_or_else(|e| panic!("copy {}: {e}", src.display()));
    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).unwrap();
    let sum: String = Sha256::digest(std::fs::read(&dest).unwrap())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let mut sha_name = dest.file_name().unwrap().to_os_string();
    sha_name.push(".sha256");
    std::fs::write(dest.with_file_name(sha_name), format!("{sum}\n")).unwrap();
}

struct Distro {
    key: &'static str,
    dockerfile: &'static str,
    /// argv, as root in the container, that removes and restores git.
    remove_git: &'static [&'static str],
    install_git: &'static [&'static str],
}

const DISTROS: &[Distro] = &[
    Distro {
        key: "debian",
        dockerfile: "debian.Dockerfile",
        remove_git: &["apt-get", "remove", "-y", "git"],
        install_git: &["sh", "-c", "apt-get update -q && apt-get install -y git"],
    },
    Distro {
        key: "fedora",
        dockerfile: "fedora.Dockerfile",
        remove_git: &["dnf", "remove", "-y", "git"],
        install_git: &["dnf", "install", "-y", "git"],
    },
    Distro {
        key: "arch",
        dockerfile: "arch.Dockerfile",
        remove_git: &["pacman", "-R", "--noconfirm", "git"],
        install_git: &["pacman", "-S", "--noconfirm", "git"],
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

/// Builds `d`'s image once per run. A failed build panics.
fn build_image(d: &Distro) {
    static BUILT: Mutex<Option<HashSet<&'static str>>> = Mutex::new(None);
    let mut built = BUILT.lock().unwrap_or_else(|e| e.into_inner());
    if !built.get_or_insert_with(HashSet::new).insert(d.key) {
        return;
    }
    let out = run(container(&["build", "-q", "--label", LABEL, "-f"])
        .arg(fixtures_dir().join(d.dockerfile))
        .args(["-t", &image(d)])
        .arg(fixtures_dir()));
    assert!(
        out.status.success(),
        "build of {}: {}",
        d.dockerfile,
        String::from_utf8_lossy(&out.stderr).trim()
    );
}

/// Why a container test cannot run here, or the herdr to seed.
fn missing() -> Result<PathBuf, String> {
    for program in ["ssh", "ssh-keygen", "ssh-keyscan"] {
        if which(program).is_none() {
            return Err(format!("{program} is not installed on this host"));
        }
    }
    match container(&["info"]).output() {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            return Err(format!(
                "`podman info` failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ));
        }
        Err(e) => return Err(format!("podman not found: {e}")),
    }
    herdr_bin()
}

/// Everything a test needs, `None` after printing why it skips.
fn prepare(test: &str, d: &Distro) -> Option<PathBuf> {
    match missing() {
        Ok(herdr) => {
            build_image(d);
            Some(herdr)
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

/// The head's way into a container: a fresh key, and a wrapper `ssh` that
/// always runs through its own `ssh_config`.
struct SshClient {
    _tmp: tempfile::TempDir,
    pubkey: String,
    path: OsString,
}

impl SshClient {
    fn new() -> SshClient {
        let tmp = tempfile::tempdir().unwrap();
        let key = tmp.path().join("key");
        ok(run(Command::new(which("ssh-keygen").unwrap())
            .args(["-q", "-t", "ed25519", "-N", "", "-C", LABEL, "-f"])
            .arg(&key)));
        let pubkey = std::fs::read_to_string(key.with_extension("pub")).unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let ssh_config = tmp.path().join("ssh_config");
        std::fs::write(
            &ssh_config,
            format!(
                "Host 127.0.0.1\n\tPort {SSHD_PORT}\n\tUser me\n\tIdentityFile {}\n\
                 \tIdentitiesOnly yes\n\tIdentityAgent none\n\tGlobalKnownHostsFile /dev/null\n",
                key.display(),
            ),
        )
        .unwrap();
        write_exec(
            &bin.join("ssh"),
            &format!(
                "#!/bin/sh\nexec '{}' -F '{}' \"$@\"\n",
                which("ssh").unwrap().display(),
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
}

/// A running container: systemd as PID 1, sshd on `SSHD_PORT` published on
/// the host's loopback, and `me` letting this run's key in.
struct Container {
    name: String,
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = container(&["rm", "-f", "-v", &self.name]).output();
    }
}

impl Container {
    fn start(d: &Distro, pubkey: &str) -> Container {
        Container::start_with(d, pubkey, &[])
    }

    /// `start`, with `extra` arguments to `podman run`.
    fn start_with(d: &Distro, pubkey: &str, extra: &[&str]) -> Container {
        let name = format!("pastor-bootstrap-test-{}-{}", d.key, std::process::id());
        let _ = container(&["rm", "-f", "-v", &name]).output();
        ok(run(container(&[
            "run",
            "-d",
            "--name",
            &name,
            "--label",
            LABEL,
            "--systemd=always",
            "-p",
            &format!("127.0.0.1:{SSHD_PORT}:{SSHD_PORT}"),
        ])
        .args(extra)
        .arg(image(d))));
        let c = Container { name };
        c.wait_system_running(d);
        let script = format!(
            "printf '%s\\n' '{}' > /home/me/.ssh/authorized_keys && chown me:me \
             /home/me/.ssh/authorized_keys && chmod 600 /home/me/.ssh/authorized_keys",
            pubkey.trim().replace('\'', "'\\''")
        );
        ok(c.exec(&["sh", "-c", &script]));
        c.wait_sshd();
        c
    }

    fn wait_system_running(&self, d: &Distro) {
        let deadline = Instant::now() + WAIT;
        loop {
            let out = self.exec(&["systemctl", "is-system-running"]);
            let state = String::from_utf8_lossy(&out.stdout).trim().to_string();
            // Degraded: a unit a container cannot run failed; sshd is up.
            if state == "running" || state == "degraded" {
                return;
            }
            assert!(Instant::now() < deadline, "{} never booted: {state}", d.key);
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    fn wait_sshd(&self) {
        let deadline = Instant::now() + WAIT;
        loop {
            // An answer on the published port is not sshd's: read its banner.
            if let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", SSHD_PORT)) {
                use std::io::Read;
                let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                let mut banner = [0u8; 4];
                if s.read_exact(&mut banner).is_ok() && &banner == b"SSH-" {
                    return;
                }
            }
            assert!(
                Instant::now() < deadline,
                "sshd on {} never came up",
                self.name
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// A command in the container, as root.
    fn exec(&self, args: &[&str]) -> Output {
        run(container(&["exec", &self.name]).args(args))
    }

    /// Whether `unit` becomes active in `me`'s own systemd manager, reached
    /// as a login would: with its runtime dir.
    fn user_service_active(&self, unit: &str) -> bool {
        let uid = ok(self.exec(&["id", "-u", "me"])).trim().to_string();
        let runtime_dir = format!("XDG_RUNTIME_DIR=/run/user/{uid}");
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let out = self.exec(&[
                "runuser",
                "-u",
                "me",
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
                eprintln!("{unit}: {out:?}");
                return false;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Whether `me` can run a command as root without a password.
    fn me_can_sudo(&self) -> bool {
        self.exec(&["runuser", "-u", "me", "--", "sudo", "-n", "true"])
            .status
            .success()
    }

    fn has(&self, program: &str) -> bool {
        self.exec(&["sh", "-c", &format!("command -v {program}")])
            .status
            .success()
    }
}

/// The head: this process's own `pastor serve --foreground`, with a temp
/// config, state and authorized_keys.
struct Head {
    tmp: tempfile::TempDir,
    config: PathBuf,
    state: PathBuf,
    data: PathBuf,
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
        Head::start_with(ssh, "pastor-bootstrap-test@head.invalid", None)
    }

    /// A head whose `head_address` is `address`, and, to make join tokens,
    /// whose sshd host key is the public key in `host_key`.
    fn start_with(ssh: &SshClient, address: &str, host_key: Option<&Path>) -> Head {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("c");
        let state = tmp.path().join("s");
        let data = tmp.path().join("d");
        std::fs::create_dir_all(&config).unwrap();
        // The machine never dials the head in a push test; the address only
        // has to be a valid ssh destination.
        std::fs::write(
            config.join("pastor.toml"),
            format!("tick = \"1s\"\nhead_address = \"{address}\"\n"),
        )
        .unwrap();
        // A flock must name a machine to serve; a pull machine needs no herdr.
        std::fs::write(
            config.join("flock.toml"),
            "[[machine]]\nname = \"idle\"\npull = true\n",
        )
        .unwrap();
        let mut serve = pastor();
        if let Some(key) = host_key {
            serve.env("PASTOR_SSH_HOST_KEY", key);
        }
        let serve = serve
            .args(["serve", "--foreground"])
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .env("PASTOR_DATA_DIR", &data)
            .env("PASTOR_AUTHORIZED_KEYS", tmp.path().join("authorized_keys"))
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
            assert!(
                self.serve.try_wait().unwrap().is_none() && Instant::now() < deadline,
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
            .env(
                "PASTOR_AUTHORIZED_KEYS",
                self.tmp.path().join("authorized_keys"),
            )
            .env("PATH", &self.path);
        c
    }

    fn paths(&self) -> pastor::config::Paths {
        pastor::config::Paths::new(self.config.clone(), self.state.clone())
            .with_data_dir(self.data.clone())
    }

    /// A pastor command; on failure the daemon's log is shown.
    fn pastor(&self, args: &[&str]) -> Output {
        let out = run(&mut self.cmd(args));
        if !out.status.success() {
            eprintln!("--- serve ---\n{}", self.log());
        }
        out
    }
}

/// A head with its cache seeded and a container that lets its key in.
fn setup(test: &str, d: &Distro) -> Option<(SshClient, Head, Container)> {
    let herdr = prepare(test, d)?;
    let pastor_bin = std::env::var_os("PASTOR_TEST_PASTOR_BIN").map_or_else(
        || PathBuf::from(env!("CARGO_BIN_EXE_pastor")),
        PathBuf::from,
    );
    let ssh = SshClient::new();
    let head = Head::start(&ssh);
    let paths = head.paths();
    seed_artifact(
        &paths,
        "pastor",
        env!("CARGO_PKG_VERSION"),
        PASTOR_TARGET,
        &pastor_bin,
    );
    seed_artifact(
        &paths,
        "herdr",
        pastor::artifacts::DEFAULT_HERDR_VERSION,
        HERDR_TARGET,
        &herdr,
    );
    let c = Container::start(d, &ssh.pubkey);
    Some((ssh, head, c))
}

/// The machine's `bootstrap` line in `machine describe`.
fn bootstrap_line(head: &Head, name: &str) -> String {
    let text = ok(run(&mut head.cmd(&["machine", "describe", name])));
    text.lines()
        .find(|l| l.starts_with("bootstrap:"))
        .unwrap_or_default()
        .to_string()
}

/// `push_<distro>`: `machine add --bootstrap` against a bare container ends
/// ok, as `me`, with no sudo in the image: herdr's user service is active.
fn run_push(key: &str) {
    let d = distro(key);
    let Some((_ssh, head, c)) = setup(&format!("push_{key}"), d) else {
        return;
    };
    assert!(!c.me_can_sudo(), "`me` can sudo on {key}");
    let text = ok(head.pastor(&["machine", "add", "m1", "me@127.0.0.1", "--bootstrap"]));
    assert!(text.contains("m1"), "{text}");
    let line = bootstrap_line(&head, "m1");
    assert!(line.contains("ok"), "{line}");
    assert!(
        c.user_service_active("herdr.service"),
        "herdr is not active as me on {key}"
    );
}

/// `push_rerun_<distro>`: with git removed the push stops at the
/// prerequisites step and the machine reads failed; with git back, the same
/// `machine bootstrap` ends ok.
fn run_push_rerun(key: &str) {
    let d = distro(key);
    let Some((_ssh, head, c)) = setup(&format!("push_rerun_{key}"), d) else {
        return;
    };
    ok(c.exec(d.remove_git));
    assert!(!c.has("git"), "git is still on {key}");
    let first = head.pastor(&["machine", "add", "m1", "me@127.0.0.1", "--bootstrap"]);
    assert_eq!(error_code(&first), "bootstrap_missing", "{first:?}");
    let line = bootstrap_line(&head, "m1");
    assert!(
        line.contains("failed at prerequisites") && line.contains("git"),
        "{line}"
    );

    ok(c.exec(d.install_git));
    ok(head.pastor(&["machine", "bootstrap", "m1"]));
    let line = bootstrap_line(&head, "m1");
    assert!(line.contains("ok"), "{line}");
}

#[test]
#[ignore = "needs a container runtime; run with make test-bootstrap"]
fn push_debian() {
    run_push("debian");
}

#[test]
#[ignore = "needs a container runtime; run with make test-bootstrap"]
fn push_fedora() {
    run_push("fedora");
}

#[test]
#[ignore = "needs a container runtime; run with make test-bootstrap"]
fn push_arch() {
    run_push("arch");
}

#[test]
#[ignore = "needs a container runtime; run with make test-bootstrap"]
fn push_rerun_debian() {
    run_push_rerun("debian");
}

#[test]
#[ignore = "needs a container runtime; run with make test-bootstrap"]
fn push_rerun_fedora() {
    run_push_rerun("fedora");
}

#[test]
#[ignore = "needs a container runtime; run with make test-bootstrap"]
fn push_rerun_arch() {
    run_push_rerun("arch");
}

/// The host's own sshd, which a joining machine reaches the head through: a
/// throwaway one on a loopback port, as the user running the test, whose
/// authorized_keys is the head's (so `join create` edits it) and whose
/// logins land on the head's directories, as `real_ssh.rs` does.
struct HostSshd {
    child: Child,
}

impl Drop for HostSshd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl HostSshd {
    fn start(head: &Head, port: u16, host_key: &Path) -> HostSshd {
        let dir = head.tmp.path();
        let config = dir.join("sshd_config");
        std::fs::write(
            &config,
            format!(
                "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nPidFile {}\n\
                 AuthorizedKeysFile {}\nStrictModes no\nUsePAM no\n\
                 PubkeyAuthentication yes\nPasswordAuthentication no\n\
                 KbdInteractiveAuthentication no\n\
                 SetEnv PASTOR_CONFIG_DIR={} PASTOR_STATE_DIR={} PASTOR_DATA_DIR={}\n",
                host_key.display(),
                dir.join("sshd.pid").display(),
                dir.join("authorized_keys").display(),
                head.config.display(),
                head.state.display(),
                head.data.display(),
            ),
        )
        .unwrap();
        std::fs::write(dir.join("authorized_keys"), "").ok();
        let mut child = Command::new(which("sshd").expect("sshd"))
            .args(["-D", "-e", "-f"])
            .arg(&config)
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(dir.join("sshd.log")).unwrap())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            let log = std::fs::read_to_string(dir.join("sshd.log")).unwrap_or_default();
            assert!(
                child.try_wait().unwrap().is_none(),
                "the host sshd exited\n--- sshd ---\n{log}"
            );
            assert!(
                Instant::now() < deadline,
                "the host sshd never came up\n--- sshd ---\n{log}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        HostSshd { child }
    }
}

/// The name, and the address behind it, by which a rootless container
/// reaches the host's loopback (pasta maps the address to it).
const HOST_NAME: &str = "head.pastor-test";
const HOST_ADDR: &str = "169.254.1.2";

/// `pull_join_debian`: a head on the host makes a join token; a Debian
/// container runs `pastor join --token-stdin` as `me`, over the host's sshd,
/// and ends a pull machine of the head whose bootstrap reports ok. The token
/// is neither in the container's process list nor in a file there after.
#[test]
#[ignore = "needs a container runtime; run with make test-bootstrap"]
fn pull_join_debian() {
    let d = distro("debian");
    if which("sshd").is_none() {
        let why = "sshd is not installed on this host";
        if std::env::var("PASTOR_E2E_REQUIRE").is_ok_and(|v| v == "1") {
            panic!("pull_join_debian cannot run and PASTOR_E2E_REQUIRE=1: {why}");
        }
        eprintln!("skip pull_join_debian: {why}");
        return;
    }
    let Some(herdr) = prepare("pull_join_debian", d) else {
        return;
    };
    let pastor_bin = std::env::var_os("PASTOR_TEST_PASTOR_BIN").map_or_else(
        || PathBuf::from(env!("CARGO_BIN_EXE_pastor")),
        PathBuf::from,
    );
    let ssh = SshClient::new();
    // The sshd's port is the head's address, so it is chosen first.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let keys = tempfile::tempdir().unwrap();
    let host_key = keys.path().join("host_ed25519");
    ok(run(Command::new(which("ssh-keygen").unwrap())
        .args(["-q", "-t", "ed25519", "-N", "", "-C", LABEL, "-f"])
        .arg(&host_key)));
    let user = ok(run(Command::new("id").arg("-un"))).trim().to_string();
    let head = Head::start_with(
        &ssh,
        &format!("ssh://{user}@{HOST_NAME}:{port}"),
        Some(&host_key.with_extension("pub")),
    );
    let _sshd = HostSshd::start(&head, port, &host_key);
    seed_artifact(
        &head.paths(),
        "herdr",
        pastor::artifacts::DEFAULT_HERDR_VERSION,
        HERDR_TARGET,
        &herdr,
    );
    let c = Container::start_with(
        d,
        &ssh.pubkey,
        &[
            "--network",
            &format!("pasta:--map-host-loopback,{HOST_ADDR}"),
            "--add-host",
            &format!("{HOST_NAME}:{HOST_ADDR}"),
        ],
    );
    // The binary the machine joins with, where a login finds it.
    ok(c.exec(&["sh", "-c", "mkdir -p /home/me/.local/bin"]));
    ok(run(&mut container(&[
        "cp",
        pastor_bin.to_str().unwrap(),
        &format!("{}:/home/me/.local/bin/pastor", c.name),
    ])));
    ok(c.exec(&[
        "sh",
        "-c",
        "chown -R me:me /home/me/.local && chmod 755 /home/me/.local/bin/pastor",
    ]));

    let made: serde_json::Value =
        serde_json::from_str(&ok(head.pastor(&["join", "create", "--json"]))).unwrap();
    let token = made["token"].as_str().unwrap().to_string();
    let key = pastor::join::Token::decode(&token).unwrap().key;
    // A line of the key's body that is its own: the first lines of every
    // OpenSSH key of a type are alike.
    let key_line = key.lines().nth(2).unwrap().to_string();
    assert!(key_line.len() > 20, "{key_line}");

    // Over ssh, as a login: it has the user's session and runtime dir.
    let mut join = Command::new("ssh");
    join.env("PATH", &head.path)
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "me@127.0.0.1",
            "/home/me/.local/bin/pastor join --token-stdin --name pulled --json",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = join.spawn().unwrap();
    {
        use std::io::Write;
        writeln!(child.stdin.take().unwrap(), "{token}").unwrap();
    }
    let out = child.wait_with_output().unwrap();
    if !out.status.success() {
        eprintln!("--- serve ---\n{}", head.log());
    }
    let report = ok(out);
    assert!(
        !report.contains(&token) && !report.contains(&key_line),
        "{report}"
    );

    // A pull machine of the head, whose bootstrap says ok.
    let list = ok(run(&mut head.cmd(&["machine", "list", "--json"])));
    assert!(list.contains("\"pulled\""), "{list}");
    let flock = pastor::config::flock::Flock::load(&head.config.join("flock.toml")).unwrap();
    assert!(
        flock.get("pulled").is_some_and(|m| m.pull),
        "pulled is not a pull machine in the head's flock"
    );
    let line = bootstrap_line(&head, "pulled");
    assert!(line.contains("ok"), "{line}");

    // The token's line is gone from the host sshd's authorized_keys, so it
    // opens nothing a second time.
    let lines = std::fs::read_to_string(head.tmp.path().join("authorized_keys")).unwrap();
    assert!(!lines.contains("pastor:join:"), "{lines}");
    assert!(lines.contains("pastor:machine:pulled"), "{lines}");
    // Its way in is locked to the shepherd's bridge for that machine.
    let line = lines
        .lines()
        .find(|l| l.contains("pastor:machine:pulled"))
        .unwrap();
    assert!(
        line.contains("restrict") && line.contains("bridge --shepherd --machine pulled"),
        "{line}"
    );

    // Nothing of the token is left in the container: not in a process's
    // arguments or environment, not in any file (binary ones, the task
    // database and the journals, included). The image has no ps; /proc is
    // read directly, and each scan must find something it is known to hold
    // before its silence about the token means anything.
    let procs = ok(c.exec(&[
        "sh",
        "-c",
        "cat /proc/[0-9]*/cmdline /proc/[0-9]*/environ 2>/dev/null | tr '\\0' '\\n'",
    ]));
    assert!(
        procs.contains("pastor") && procs.contains("--foreground"),
        "the process scan sees nothing of pastor's own: {procs:.200}"
    );
    let files_with = |needle: &str| {
        let found = c.exec(&[
            "grep",
            "-ralFs",
            "-D",
            "skip",
            "--exclude-dir=proc",
            "--exclude-dir=sys",
            "--exclude-dir=dev",
            "-e",
            needle,
            "/",
        ]);
        String::from_utf8_lossy(&found.stdout).trim().to_string()
    };
    assert!(
        !files_with(HOST_NAME).is_empty(),
        "the file scan finds nothing it should"
    );
    for needle in [token.as_str(), key_line.as_str()] {
        assert!(
            !procs.contains(needle),
            "the token is in a process of the container"
        );
        let found = files_with(needle);
        assert!(
            found.is_empty(),
            "the token is in files of the container: {found}"
        );
    }
}
