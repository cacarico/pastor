//! The CLI against a head over a real ssh: an `sshd` of the test's own on
//! localhost, a throwaway host key and user keys, and a `pastor bridge` on the
//! far end. Every other head test runs a fake `ssh` that ignores its options,
//! so a bad argv (a ControlPath too long for a unix socket, as in 0.7.0 on
//! macOS) passed all of them. Opt-in: `make test-ssh` (which CI's check job
//! runs) needs `sshd` and `ssh-keygen`; the run fails, never skips, without
//! them.
//!
//! sshd runs as the user running the test, so that is the only user it can
//! log in, and `SetEnv` in its config points the far end at the head's dirs.
//! The client's `ssh` on PATH is the real one with `-F` in front, so neither
//! the user's `~/.ssh/config` nor their known hosts take part; pastor's own
//! arguments reach ssh as pastor built them.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(60);

/// `sun_path`, as `src/ssh.rs` has it.
const UNIX_PATH_MAX: usize = if cfg!(target_os = "linux") { 108 } else { 104 };

fn pastor() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_pastor"));
    c.env_remove("PASTOR_TASK");
    c.env_remove("PASTOR_ORCHESTRATOR");
    c.env_remove("PASTOR_HEAD");
    c
}

fn run(cmd: &mut Command) -> Output {
    cmd.output().unwrap()
}

fn ok(out: Output) -> String {
    assert!(
        out.status.success(),
        "exit {:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
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
        .unwrap_or_else(|| panic!("{program} not found: make test-ssh needs OpenSSH's server"))
}

fn keygen(path: &Path) -> String {
    let out = run(Command::new(find("ssh-keygen"))
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "pastor-test", "-f"])
        .arg(path));
    ok(out);
    std::fs::read_to_string(path.with_extension("pub")).unwrap()
}

fn write_exec(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A head (`pastor serve` with one fake-herdr machine) and an sshd that
/// lands its logins on it.
struct Head {
    tmp: tempfile::TempDir,
    config: PathBuf,
    state: PathBuf,
    port: u16,
    /// `HostKeyAlias` line for a client's known_hosts.
    known_host: String,
    children: Vec<Child>,
}

impl Drop for Head {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Head {
    fn start() -> Head {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("c");
        let state = tmp.path().join("s");
        let data = tmp.path().join("d");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::write(
            config.join("pastor.toml"),
            "tick = \"1s\"\nsettle = \"1s\"\nreconcile_every = \"1s\"\n",
        )
        .unwrap();
        let socket = tmp.path().join("herdr.sock");
        let herdr = Command::new(env!("CARGO_BIN_EXE_fake-herdr"))
            .arg("--listen")
            .arg(&socket)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        std::fs::write(
            config.join("flock.toml"),
            format!(
                "[[machine]]\nname = \"fake\"\ncommand = [\"{}\", \"--connect\", \"{}\"]\nmax_agents = 2\n",
                env!("CARGO_BIN_EXE_fake-herdr"),
                socket.display()
            ),
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

        let host_key = tmp.path().join("host_ed25519");
        let host_pub = keygen(&host_key);
        let known_host = format!("pastor-test-head {}", host_pub.trim());
        // Taken and let go: sshd binds it next. Another process could take it
        // in between, which would only fail this run.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
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
        std::fs::write(tmp.path().join("authorized_keys"), "").unwrap();
        // sshd re-execs itself, so it wants an absolute path.
        let sshd = Command::new(find("sshd"))
            .args(["-D", "-e", "-f"])
            .arg(&sshd_config)
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(tmp.path().join("sshd.log")).unwrap())
            .spawn()
            .unwrap();

        let mut head = Head {
            tmp,
            config,
            state,
            port,
            known_host,
            children: vec![herdr, serve, sshd],
        };
        head.wait_ready();
        head
    }

    fn log(&self, name: &str) -> String {
        std::fs::read_to_string(self.tmp.path().join(name)).unwrap_or_default()
    }

    /// Until the machine is connected and sshd takes connections.
    fn wait_ready(&mut self) {
        let deadline = Instant::now() + WAIT;
        loop {
            let machine = ok_or_empty(self.cmd(&["machine", "list", "--json"]));
            let listening = std::net::TcpStream::connect(("127.0.0.1", self.port)).is_ok();
            if machine.contains("\"connected\"") && listening {
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

    /// A command on the head's own machine.
    fn cmd(&self, args: &[&str]) -> Output {
        run(pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &self.config)
            .env("PASTOR_STATE_DIR", &self.state))
    }

    /// Adds `line` to sshd's authorized_keys: a public key as is, or the
    /// restricted line `agent_key_line` gives.
    fn authorize(&self, line: &str) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(self.tmp.path().join("authorized_keys"))
            .unwrap();
        writeln!(f, "{}", line.trim()).unwrap();
    }

    /// `pastor machine authorized-key` for `machine`, as its docs say to run
    /// it on the head.
    fn agent_key_line(&self, machine: &str, public_key: &Path) -> String {
        ok(self.cmd(&[
            "machine",
            "authorized-key",
            machine,
            "--key",
            public_key.to_str().unwrap(),
        ]))
    }
}

fn ok_or_empty(out: Output) -> String {
    if out.status.success() {
        String::from_utf8_lossy(&out.stdout).into_owned()
    } else {
        String::new()
    }
}

/// A CLI on another machine, with `head` as its ssh destination. Its state
/// dir is `state_len` bytes long, which decides whether ssh's control socket
/// fits.
struct Client {
    tmp: tempfile::TempDir,
    config: PathBuf,
    state: PathBuf,
    data: PathBuf,
    path: std::ffi::OsString,
}

/// The masters `ControlPersist` left behind would outlive the test by a
/// minute: stop them, while the socket is still there to ask.
impl Drop for Client {
    fn drop(&mut self) {
        let Ok(dir) = std::fs::read_dir(self.state.join("ssh")) else {
            return;
        };
        for socket in dir.flatten() {
            let _ = Command::new(self.tmp.path().join("bin").join("ssh"))
                .arg("-o")
                .arg(format!("ControlPath={}", socket.path().display()))
                .args(["-O", "exit", "head"])
                .stderr(Stdio::null())
                .status();
        }
    }
}

impl Client {
    fn new(head: &Head, key: &Path, state_len: usize) -> Client {
        // Under /tmp, not $TMPDIR, whose length is the platform's: the state
        // dir's length is what this test is about.
        let tmp = tempfile::Builder::new()
            .prefix("p")
            .tempdir_in("/tmp")
            .unwrap();
        // A deep path, one directory per level, as a long home would give.
        let mut state = tmp.path().to_path_buf();
        assert!(
            state.as_os_str().len() + 2 < state_len,
            "{}",
            state.display()
        );
        while state.as_os_str().len() < state_len {
            // `/` and at least one byte per level; never leave one byte over,
            // which no level could take.
            let room = state_len - state.as_os_str().len() - 1;
            let mut n = room.min(20);
            if room - n == 1 {
                n -= 1;
            }
            state.push("s".repeat(n));
        }
        assert_eq!(state.as_os_str().len(), state_len, "{}", state.display());

        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let known = tmp.path().join("known_hosts");
        std::fs::write(&known, format!("{}\n", head.known_host)).unwrap();
        let user = ok(run(Command::new("id").arg("-un"))).trim().to_string();
        let ssh_config = tmp.path().join("ssh_config");
        std::fs::write(
            &ssh_config,
            format!(
                "Host head\n\
                 \tHostName 127.0.0.1\n\
                 \tPort {}\n\
                 \tUser {user}\n\
                 \tIdentityFile {}\n\
                 \tIdentitiesOnly yes\n\
                 \tIdentityAgent none\n\
                 \tHostKeyAlias pastor-test-head\n\
                 \tUserKnownHostsFile {}\n\
                 \tGlobalKnownHostsFile /dev/null\n\
                 \tStrictHostKeyChecking yes\n",
                head.port,
                key.display(),
                known.display(),
            ),
        )
        .unwrap();
        write_exec(
            &bin.join("ssh"),
            &format!(
                "#!/bin/sh\nexec '{}' -F '{}' \"$@\"\n",
                find("ssh").display(),
                ssh_config.display()
            ),
        );
        let mut path = std::ffi::OsString::from(&bin);
        path.push(":");
        path.push(std::env::var_os("PATH").unwrap_or_default());
        Client {
            config: tmp.path().join("c"),
            data: tmp.path().join("d"),
            state,
            tmp,
            path,
        }
    }

    fn cmd(&self, args: &[&str]) -> Output {
        run(pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &self.config)
            .env("PASTOR_STATE_DIR", &self.state)
            .env("PASTOR_DATA_DIR", &self.data)
            .env("PATH", &self.path))
    }

    fn head_set(&self) -> String {
        ok(self.cmd(&[
            "head",
            "set",
            "head",
            "--pastor",
            env!("CARGO_BIN_EXE_pastor"),
        ]))
    }

    fn task(&self, id: &str) -> serde_json::Value {
        let out = ok(self.cmd(&["task", "describe", id, "--json"]));
        serde_json::from_str(&out).unwrap()
    }
}

/// `<state>/ssh/head-%C` expands to the state dir plus 50 bytes, and ssh
/// stages the socket 17 bytes longer still. This length puts the full name
/// past `sun_path`, where 0.7.0's head client failed every request, but
/// leaves room for a shortened one.
fn state_len_past_the_full_name() -> usize {
    UNIX_PATH_MAX - 50 - 17 + 1
}

/// Too deep for even a bare `-%C`: no multiplexing at all.
fn state_len_past_any_name() -> usize {
    UNIX_PATH_MAX - 46 - 17 + 1
}

#[test]
#[ignore = "needs sshd; run with make test-ssh"]
fn a_head_over_real_ssh_runs_reads_and_ends_a_task() {
    let head = Head::start();
    let keys = tempfile::tempdir().unwrap();
    let user_key = keys.path().join("user");
    let user_pub = keygen(&user_key);
    head.authorize(&user_pub);
    let agent_key = keys.path().join("agent");
    keygen(&agent_key);
    head.authorize(&head.agent_key_line("fake", &agent_key.with_extension("pub")));

    // A person's CLI, deep enough that the full ControlPath does not fit.
    let user = Client::new(&head, &user_key, state_len_past_the_full_name());
    let out = user.head_set();
    assert!(out.starts_with("head: head (remote, pastor "), "{out}");
    let t: serde_json::Value = serde_json::from_str(&ok(
        user.cmd(&["task", "run", "hello", "--repo", "/tmp", "--json"])
    ))
    .unwrap();
    assert_eq!(t["machine"], "fake");
    assert_eq!(t["agent_name"], "t-1");
    let list = ok(user.cmd(&["task", "list", "--json"]));
    assert!(list.contains("\"t-1\""), "{list}");
    ok(user.cmd(&["task", "read", "t-1"]));
    // The shared master came up, under a name that fits.
    let sockets: Vec<String> = std::fs::read_dir(user.state.join("ssh"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(sockets.len(), 1, "one control socket: {sockets:?}");
    assert!(
        user.state.join("ssh").join(&sockets[0]).as_os_str().len() + 17 < UNIX_PATH_MAX,
        "{sockets:?}"
    );

    // An agent on `fake`, through the key locked to `bridge --agent`: it may
    // read and end its machine's task, and nothing else.
    let agent = Client::new(&head, &agent_key, state_len_past_the_full_name());
    agent.head_set();
    ok(agent.cmd(&["task", "read", "t-1"]));
    // The fake herdr never finishes an agent here, so only the agent's
    // `task done` below ends it.
    assert_eq!(user.task("t-1")["state"], "running");
    let refused = agent.cmd(&["task", "run", "more", "--repo", "/tmp"]);
    assert_eq!(error_code(&refused), "not_allowed_for_agent");
    ok(agent.cmd(&["task", "done", "t-1", "--summary", "done: said hello"]));
    assert_eq!(user.task("t-1")["state"], "done");

    // Deeper still: no socket fits, and ssh goes without multiplexing.
    let deep = Client::new(&head, &user_key, state_len_past_any_name());
    deep.head_set();
    let list = ok(deep.cmd(&["task", "list", "--all", "--json"]));
    assert!(list.contains("\"t-1\""), "{list}");
    assert!(!deep.state.join("ssh").exists(), "no control dir made");
}
