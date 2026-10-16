//! Push and pull bootstrap against real containers: debian, fedora and
//! arch, each systemd as PID 1 with sshd and a `me` user with passwordless
//! sudo (`tests/fixtures/bootstrap/*.Dockerfile`). Opt-in: `make
//! test-bootstrap`, which builds the images, runs them
//! `--privileged --cgroupns=host -v /sys/fs/cgroup:/sys/fs/cgroup:rw
//! --network host` and drives the real CLI against them. Every test is
//! `#[ignore]`; `--test-threads 1` is required, since containers share
//! `--network host` and a fixed sshd port (2222).
//!
//! The head is this test process itself, with a temp config and state dir,
//! exactly like `tests/real_ssh.rs`'s `Head`; push reaches a container over
//! a real ssh wrapped to go through a per-container `ssh_config` (the same
//! trick `real_ssh.rs`'s `Client` uses), and pull's container reaches this
//! process's own throwaway sshd over `127.0.0.1` because both share the
//! host's network namespace.
//!
//! **herdr, and pastor itself, never come from the network here.** The
//! daemon's own fetch is the real `CurlFetch`, which always passes `curl
//! --proto =https`; that refuses a `file://` herdr index outright (checked
//! by hand: `curl --proto =https --url file://... ` -> `Protocol "file"
//! disabled`), and `herdr_source` additionally requires every asset URL in
//! the index to start with `https://`. So there is no way to make the real
//! binary read a local, offline herdr index at all. What this suite does
//! instead is seed the head's checked artifact cache directly
//! (`seed_artifact`, using `pastor::artifacts`'s own public `Artifact` and
//! `cache_path`) with a real herdr binary found on the host and with the
//! `pastor` binary this suite is built from. `artifacts::ensure` finds
//! both already cached and verified, and never calls the fetcher: push's
//! own copy step and a pull machine's `ArtifactLocate` (streamed over the
//! ssh bridge, not through curl at all) both read the same cache. Every
//! test that needs a container first calls `skip_reason`, which requires a
//! real herdr on `PATH` or `PASTOR_TEST_HERDR_BIN`; without one, it prints
//! why and returns rather than failing. The `pastor` binary seeded for the
//! containers is the one this suite links against, built for whatever libc
//! the test host runs, not musl; it still runs on the (glibc) containers as
//! long as the host's glibc is no newer than theirs, which holds for an
//! ordinary dev machine or CI image against debian:12/fedora:40/arch, but
//! is not the real musl release pastor ships.

use std::ffi::OsString;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use common::pastor;

mod common;

const WAIT: Duration = Duration::from_secs(180);
/// Baked into every fixture's sshd_config; see the Dockerfiles for why a
/// fixed port is safe here.
const SSHD_PORT: u16 = 2222;

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
fn find(program: &str) -> PathBuf {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(["/usr/sbin", "/usr/local/sbin", "/sbin"].map(PathBuf::from))
        .map(|d| d.join(program))
        .find(|p| p.is_file())
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

/// A real herdr binary to seed the head's cache with, named by
/// `PASTOR_TEST_HERDR_BIN` or found on `PATH`. See the module doc for why
/// this stands in for a network fetch.
fn host_herdr_bin() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PASTOR_TEST_HERDR_BIN") {
        let p = PathBuf::from(p);
        return p.is_file().then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join("herdr"))
        .find(|p| p.is_file())
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

/// `docker info` works, it may run `--privileged`, and a real herdr binary
/// is on this host. `Some(why)` is a message to print and skip on, never a
/// panic: every ignored test here checks this first.
fn skip_reason(d: &Distro) -> Option<String> {
    match Command::new("docker").arg("info").output() {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            return Some(format!(
                "docker info failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ));
        }
        Err(e) => return Some(format!("docker not found: {e}")),
    }
    let image = "pastor-bootstrap-privcheck".to_string();
    let build = Command::new("docker")
        .args(["build", "-q", "-f"])
        .arg(fixtures_dir().join(d.dockerfile))
        .args(["-t", &image])
        .arg(fixtures_dir())
        .output();
    match build {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            return Some(format!(
                "docker build {}: {}",
                d.key,
                String::from_utf8_lossy(&o.stderr).trim()
            ));
        }
        Err(e) => return Some(format!("docker build failed: {e}")),
    }
    let privileged = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--privileged",
            "--cgroupns=host",
            "--entrypoint",
            "true",
        ])
        .arg(&image)
        .output();
    match privileged {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            return Some(format!(
                "docker cannot run --privileged containers here: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ));
        }
        Err(e) => return Some(format!("docker run failed: {e}")),
    }
    if host_herdr_bin().is_none() {
        return Some(
            "no herdr binary found (set PASTOR_TEST_HERDR_BIN, or put herdr on PATH); the real \
             curl used to fetch it only ever reads https://, so there is no offline way to get \
             one otherwise"
                .to_string(),
        );
    }
    None
}

/// A running container built from one of the fixtures: systemd as PID 1,
/// sshd on `SSHD_PORT`, and (once `inject_key` runs) a `me` whose
/// authorized_keys holds this run's own ephemeral key.
struct Container {
    name: String,
    key: PathBuf,
    _tmp: tempfile::TempDir,
    ssh_path: OsString,
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", "-v", &self.name])
            .output();
    }
}

impl Container {
    fn start(d: &Distro) -> Container {
        let image = format!("pastor-bootstrap-test-{}", d.key);
        let build = run(Command::new("docker")
            .args(["build", "-q", "-f"])
            .arg(fixtures_dir().join(d.dockerfile))
            .args(["-t", &image])
            .arg(fixtures_dir()));
        ok(build);

        let name = format!("pastor-bootstrap-test-{}-{}", d.key, std::process::id());
        let _ = Command::new("docker")
            .args(["rm", "-f", "-v", &name])
            .output();
        let run_out = run(Command::new("docker").args([
            "run",
            "-d",
            "--name",
            &name,
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

        let tmp = tempfile::tempdir().unwrap();
        let key = tmp.path().join("key");
        let pubkey = keygen(&key);

        let mut c = Container {
            name,
            key,
            _tmp: tmp,
            ssh_path: OsString::new(),
        };
        c.wait_system_running(d);
        c.inject_key(&pubkey);
        c.wait_sshd_ready();
        c.ssh_path = c.build_ssh_wrapper();
        c
    }

    fn login(&self) -> &str {
        "me@127.0.0.1"
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

    /// `PATH` with a wrapper `ssh` in front that always runs through this
    /// container's own `ssh_config`: no other key, no shared known_hosts,
    /// `127.0.0.1:SSHD_PORT` as `me`. Everything pastor's push code itself
    /// invokes directly (`ssh-keygen`, `ssh-keyscan`) stays the real one,
    /// found later on the same PATH.
    fn build_ssh_wrapper(&self) -> OsString {
        let dir = self._tmp.path();
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let ssh_config = dir.join("ssh_config");
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
                self.key.display(),
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
        path
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

    /// `is-active` for `unit`, as `user`, through `machinectl`-style `-M
    /// user@`: works whether or not the docker exec session itself carries
    /// a user bus.
    fn user_service_active(&self, user: &str, unit: &str) -> bool {
        let out = self.exec(&[
            "systemctl",
            "--user",
            "-M",
            &format!("{user}@"),
            "is-active",
            unit,
        ]);
        String::from_utf8_lossy(&out.stdout).trim() == "active"
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
    serve: Child,
}

impl Drop for Head {
    fn drop(&mut self) {
        let _ = self.serve.kill();
        let _ = self.serve.wait();
    }
}

impl Head {
    fn start() -> Head {
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
        let serve = pastor()
            .args(["serve", "--foreground"])
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .env("PASTOR_DATA_DIR", &data)
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(tmp.path().join("serve.log")).unwrap())
            .spawn()
            .unwrap();
        let mut head = Head {
            tmp,
            config,
            state,
            data,
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
                std::fs::read_to_string(self.tmp.path().join("serve.log")).unwrap_or_default()
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

    /// Pre-seeds pastor and herdr into this head's own artifact cache; see
    /// the module doc for why this, and not a `file://` index, is how the
    /// suite avoids a real network fetch.
    fn seed_artifacts(&self, herdr_bin: &Path) {
        seed_artifact(
            &self.paths(),
            "pastor",
            env!("CARGO_PKG_VERSION"),
            PASTOR_TARGET,
            Path::new(env!("CARGO_BIN_EXE_pastor")),
        );
        seed_artifact(
            &self.paths(),
            "herdr",
            pastor::artifacts::DEFAULT_HERDR_VERSION,
            HERDR_TARGET,
            herdr_bin,
        );
    }

    /// `machine add NAME --ssh <container> --bootstrap [--sudo]`, run with
    /// the container's own ssh wrapper on PATH.
    fn bootstrap(&self, container: &Container, name: &str, sudo: bool) -> Output {
        let mut args = vec![
            "machine",
            "add",
            name,
            "--ssh",
            container.login(),
            "--bootstrap",
        ];
        if sudo {
            args.push("--sudo");
        }
        run(self.cmd(&args).env("PATH", &container.ssh_path))
    }
}

/// `push_root_<distro>`: `machine add --bootstrap --sudo` against a bare
/// container ends ok, in root mode; herdr is active as `pastor-agent`
/// through its systemd user service, and `pastor-agent` cannot read `me`'s
/// machine key (`service_state_dir_is_0700_and_owned` at the container
/// level, per the plan's Review Focus).
fn run_push_root(key: &str) {
    let d = distro(key);
    if let Some(why) = skip_reason(d) {
        eprintln!("skip push_root_{key}: {why}");
        return;
    }
    let Some(herdr_bin) = host_herdr_bin() else {
        eprintln!("skip push_root_{key}: no herdr binary");
        return;
    };
    let head = Head::start();
    head.seed_artifacts(&herdr_bin);
    let container = Container::start(d);

    let out = head.bootstrap(&container, "root-1", true);
    let text = ok(out);
    assert!(text.contains("root mode"), "{text}");

    assert!(
        container.user_service_active("pastor-agent", "herdr"),
        "herdr is not active as pastor-agent on {key}"
    );

    let hijack = container.exec(&[
        "sudo",
        "-u",
        "pastor-agent",
        "--",
        "cat",
        "/home/me/.local/state/pastor/machine_key",
    ]);
    assert!(
        !hijack.status.success(),
        "pastor-agent could read me's machine_key on {key}: {hijack:?}"
    );
}

/// `push_user_<distro>`: `machine add --bootstrap` with no `--sudo` ends
/// ok, in user mode with isolation none: no users are created, and herdr
/// runs as `me` itself.
fn run_push_user(key: &str) {
    let d = distro(key);
    if let Some(why) = skip_reason(d) {
        eprintln!("skip push_user_{key}: {why}");
        return;
    }
    let Some(herdr_bin) = host_herdr_bin() else {
        eprintln!("skip push_user_{key}: no herdr binary");
        return;
    };
    let head = Head::start();
    head.seed_artifacts(&herdr_bin);
    let container = Container::start(d);

    let out = head.bootstrap(&container, "user-1", false);
    let text = ok(out);
    assert!(text.contains("user mode, isolation: none"), "{text}");

    assert!(
        container.user_service_active("me", "herdr"),
        "herdr is not active as me on {key}"
    );
}

/// `push_rerun_<distro>`: with git removed, a bootstrap with no `--sudo`
/// fails at the packages step (`bootstrap_missing`); a rerun of the same
/// machine with `--sudo` resumes instead of `machine_exists`, and ends ok
/// (the plan's `rerun_after_failure_resumes` pin, at the container level).
fn run_push_rerun(key: &str) {
    let d = distro(key);
    if let Some(why) = skip_reason(d) {
        eprintln!("skip push_rerun_{key}: {why}");
        return;
    }
    let Some(herdr_bin) = host_herdr_bin() else {
        eprintln!("skip push_rerun_{key}: no herdr binary");
        return;
    };
    let head = Head::start();
    head.seed_artifacts(&herdr_bin);
    let container = Container::start(d);
    container.remove_git(d);

    let first = head.bootstrap(&container, "rerun-1", false);
    assert_eq!(error_code(&first), "bootstrap_missing", "{first:?}");

    let second = head.bootstrap(&container, "rerun-1", true);
    let text = ok(second);
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
/// container reaches it over `127.0.0.1` because it runs `--network host`.
struct JoinHead {
    tmp: tempfile::TempDir,
    config: PathBuf,
    state: PathBuf,
    data: PathBuf,
    port: u16,
    children: Vec<Child>,
}

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

        // --network host means the joining container reaches this same
        // port on 127.0.0.1, so it is fixed too, like the containers' own.
        let port = SSHD_PORT + 1;
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
/// container, against this head over the docker network (`--network
/// host`); the full bootstrap that follows ends ok, and the same token
/// used again gets `join_token_invalid` with "Permission denied" from ssh
/// (`token_line_removed_after_use`, at the container level).
#[test]
#[ignore = "needs docker; run with make test-bootstrap"]
fn pull_join_debian() {
    let d = distro("debian");
    if let Some(why) = skip_reason(d) {
        eprintln!("skip pull_join_debian: {why}");
        return;
    }
    let Some(herdr_bin) = host_herdr_bin() else {
        eprintln!("skip pull_join_debian: no herdr binary");
        return;
    };

    let head = JoinHead::start();
    head.seed_artifacts_for_pull(&herdr_bin);
    let container = Container::start(d);

    let made: serde_json::Value =
        serde_json::from_str(&ok(run(&mut head.cmd(&["join", "create", "--json"])))).unwrap();
    let token = made["token"].as_str().unwrap().to_string();

    let mirror = Mirror::build(
        env!("CARGO_PKG_VERSION"),
        PASTOR_TARGET,
        Path::new(env!("CARGO_BIN_EXE_pastor")),
    );
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
    assert!(text.contains("root mode"), "{text}");

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
    loop {
        let describe = head.cmd(&["machine", "describe", "pull-debian-1"]).output();
        if let Ok(describe) = &describe
            && describe.status.success()
        {
            let text = String::from_utf8_lossy(&describe.stdout);
            if text.contains("bootstrap:") && text.contains("ok") {
                break;
            }
        }
        assert!(Instant::now() < deadline, "pull-debian-1 never reported ok");
        std::thread::sleep(Duration::from_millis(300));
    }

    // The pastor-agent key's own isolation, same as push_root's: the
    // service user is `pastor`, not `me`, by `--service-user` above.
    let hijack = container.exec(&[
        "sudo",
        "-u",
        "pastor-agent",
        "--",
        "cat",
        "/home/pastor/.local/state/pastor/machine_key",
    ]);
    assert!(
        !hijack.status.success(),
        "pastor-agent could read pastor's machine_key: {hijack:?}"
    );
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

impl JoinHead {
    fn seed_artifacts_for_pull(&self, herdr_bin: &Path) {
        seed_artifact(
            &self.paths(),
            "pastor",
            env!("CARGO_PKG_VERSION"),
            PASTOR_TARGET,
            Path::new(env!("CARGO_BIN_EXE_pastor")),
        );
        seed_artifact(
            &self.paths(),
            "herdr",
            pastor::artifacts::DEFAULT_HERDR_VERSION,
            HERDR_TARGET,
            herdr_bin,
        );
    }
}
