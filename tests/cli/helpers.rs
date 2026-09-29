//! Shared fixtures and helpers for the `cli` integration test binary,
//! used across its command-group modules.
pub(crate) use std::process::{Command, Stdio};
pub(crate) use std::time::{Duration, Instant};

pub(crate) use crate::common;
pub(crate) use crate::common::pastor;

/// How long a test waits for the daemon or the fake herdr to do something.
/// Generous on purpose: a CI runner under load has taken more than 10s to
/// bring a daemon up, and a wait that ends early only ever fails a good run.
pub(crate) const WAIT: Duration = Duration::from_secs(60);

pub(crate) struct Env {
    pub(crate) _tmp: tempfile::TempDir,
    pub(crate) config: std::path::PathBuf,
    pub(crate) state: std::path::PathBuf,
    pub(crate) serve: std::process::Child,
    pub(crate) herdr: std::process::Child,
    /// Every request the fake herdr has received, as a JSON array
    /// (`FAKE_HERDR_REQUEST_LOG`).
    pub(crate) herdr_log: std::path::PathBuf,
}

impl Drop for Env {
    fn drop(&mut self) {
        for child in [&mut self.serve, &mut self.herdr] {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub(crate) fn start() -> Env {
    start_with_jobs(&[])
}

/// Like `start()`, but writes each `(name, text)` to `config/jobs/<name>.toml`
/// before spawning `pastor serve`, so the scheduler picks the jobs up at start.
pub(crate) fn start_with_jobs(jobs: &[(&str, &str)]) -> Env {
    start_with(jobs, &[])
}

/// Like `start_with_jobs`, with extra env for the fake herdr
/// (`FAKE_HERDR_TRUST_PROMPT`, ...).
pub(crate) fn start_with(jobs: &[(&str, &str)], herdr_env: &[(&str, &str)]) -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(config.join("jobs")).unwrap();
    // Fast tick/settle/reconcile so the test doesn't wait out the 60s defaults.
    std::fs::write(
        config.join("pastor.toml"),
        "tick = \"1s\"\nsettle = \"1s\"\nreconcile_every = \"1s\"\n",
    )
    .unwrap();
    for (name, text) in jobs {
        std::fs::write(config.join("jobs").join(format!("{name}.toml")), text).unwrap();
    }
    // A herdr connection carries one request, so the `command` transport spawns a
    // bridge process per request. The state has to outlive those: `fake-herdr
    // --listen` is the server (herdr's role) and `--connect` is the bridge
    // (`remote-api-bridge`'s role), which is exactly the shape pastor talks to
    // over ssh.
    let socket = tmp.path().join("herdr.sock");
    let herdr_log = tmp.path().join("herdr-requests.json");
    let herdr = Command::new(env!("CARGO_BIN_EXE_fake-herdr"))
        .arg("--listen")
        .arg(&socket)
        .env("FAKE_HERDR_REQUEST_LOG", &herdr_log)
        .env("FAKE_HERDR_AUTO_DONE_MS", "300")
        // Agents spend a moment launching, as they do under a real herdr, so
        // this run exercises dispatch's readiness wait and not just the happy
        // path where the agent is up the instant it is started.
        .env("FAKE_HERDR_READY_MS", "200")
        .envs(herdr_env.iter().copied())
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
    let serve_log = std::fs::File::create(tmp.path().join("serve.log")).unwrap();
    let serve = pastor()
        .args(["serve", "--foreground"])
        .env("PASTOR_CONFIG_DIR", &config)
        .env("PASTOR_STATE_DIR", &state)
        .stdout(Stdio::null())
        .stderr(serve_log)
        .spawn()
        .unwrap();
    let mut env = Env {
        _tmp: tmp,
        config,
        state,
        serve,
        herdr,
        herdr_log,
    };
    let deadline = Instant::now() + WAIT;
    loop {
        let out = env.cmd(&["machine", "list", "--json"]);
        if out.status.success() && String::from_utf8_lossy(&out.stdout).contains("\"connected\"") {
            break;
        }
        // A daemon that exited is not a slow one: fail now, with what it said.
        let exited = env.serve.try_wait().unwrap().is_some();
        if exited || Instant::now() >= deadline {
            let log =
                std::fs::read_to_string(env._tmp.path().join("serve.log")).unwrap_or_default();
            let herdr = std::fs::read_to_string(&env.herdr_log).unwrap_or_default();
            panic!(
                "daemon {}: {}\n--- serve stderr ---\n{log}\n--- herdr log ---\n{herdr}",
                if exited {
                    "exited before the machine connected"
                } else {
                    "never reported the machine connected"
                },
                String::from_utf8_lossy(&out.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    env
}

impl Env {
    /// The `params` of the `agent.start` the fake herdr got for `agent`.
    pub(crate) fn agent_start_params(&self, agent: &str) -> serde_json::Value {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Ok(text) = std::fs::read_to_string(&self.herdr_log)
                && let Ok(reqs) = serde_json::from_str::<Vec<serde_json::Value>>(&text)
                && let Some(r) = reqs
                    .iter()
                    .find(|r| r["method"] == "agent.start" && r["params"]["name"] == agent)
            {
                return r["params"].clone();
            }
            assert!(
                Instant::now() < deadline,
                "no agent.start for {agent} in {}",
                self.herdr_log.display()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Polls `task describe` until `task` is done, so later snapshots of it are
    /// stable: the fake finishes agents on its own and the daemon reconciles.
    pub(crate) fn wait_done(&self, task: &str) {
        let deadline = Instant::now() + WAIT;
        loop {
            let out = self.cmd(&["task", "describe", task, "--json"]);
            let t: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
            if t["state"] == "done" {
                return;
            }
            assert!(Instant::now() < deadline, "task never became done: {t}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    pub(crate) fn cmd(&self, args: &[&str]) -> std::process::Output {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &self.config)
            .env("PASTOR_STATE_DIR", &self.state)
            .output()
            .unwrap()
    }

    /// Like `cmd`, with `input` on the command's stdin.
    pub(crate) fn cmd_stdin(&self, args: &[&str], input: &str) -> std::process::Output {
        use std::io::Write;
        let mut child = pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &self.config)
            .env("PASTOR_STATE_DIR", &self.state)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }
}

/// `--agent-arg` reaches herdr's `agent.start` as `args`, in order, and shows
/// in `task describe` and `task list --json`; without it, the flock's
/// `agent_args` do, then `[defaults] agent_args`.
/// A claude agent's args without the `--session-id <uuid>` dispatch puts
/// last.
pub(crate) fn without_session(args: &serde_json::Value) -> serde_json::Value {
    let mut args = args.as_array().unwrap().clone();
    let n = args.len();
    assert!(n >= 2 && args[n - 2] == "--session-id", "{args:?}");
    args.truncate(n - 2);
    serde_json::Value::Array(args)
}

/// systemd counts SIGTERM and SIGHUP as a clean exit and would not restart a
/// `Restart=on-failure` unit after either; `pastor serve` used to only
/// handle SIGINT (ctrl-c), so a SIGTERM or SIGHUP from a plain `kill`,
/// `systemctl stop` of a wrapper, or a stray signal killed the head
/// silently, with the socket file left behind and the unit never coming
/// back. Drives a real `pastor serve` child (`serve` refuses an empty
/// flock, so a fake-herdr-backed machine is needed too), sends it the given
/// signal and checks that it exits cleanly, removes its socket, and logs
/// which signal it got.
/// Kills `serve` and `herdr` on drop, mirroring `Env::drop` above, so a
/// failing assertion anywhere in the test never leaves either process
/// running.
pub(crate) struct ServeEnv {
    pub(crate) serve: std::process::Child,
    pub(crate) herdr: std::process::Child,
}

impl Drop for ServeEnv {
    fn drop(&mut self) {
        for child in [&mut self.serve, &mut self.herdr] {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Shared body for the SIGTERM and SIGHUP regression tests: `signal` is the
/// `kill` flag (`TERM`, `HUP`), which also names the log line to expect
/// (`SIGTERM`, `SIGHUP`) since `ExtraSignals::recv` labels each branch that
/// way.
pub(crate) fn assert_daemon_shuts_down_cleanly_on(signal: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();

    // `pastor serve` refuses an empty flock, so it needs one machine; a
    // `command` machine backed by fake-herdr is the cheapest way to get one
    // without a real fleet.
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

    let stderr_path = tmp.path().join("serve.stderr");
    let serve = pastor()
        .args(["serve", "--foreground"])
        .env("PASTOR_CONFIG_DIR", &config)
        .env("PASTOR_STATE_DIR", &state)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&stderr_path).unwrap())
        .spawn()
        .unwrap();
    let mut env = ServeEnv { serve, herdr };

    let cmd = |args: &[&str]| -> std::process::Output {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .output()
            .unwrap()
    };
    let deadline = Instant::now() + WAIT;
    loop {
        let out = cmd(&["machine", "list", "--json"]);
        if out.status.success() && String::from_utf8_lossy(&out.stdout).contains("\"connected\"") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "daemon never reported the machine connected: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    let pid = env.serve.id().to_string();
    let flag = format!("-{signal}");
    let killed = Command::new("kill").args([&flag, &pid]).status().unwrap();
    assert!(killed.success(), "kill {flag} {pid} failed to run");

    let deadline = Instant::now() + WAIT;
    let exit = loop {
        if let Some(status) = env.serve.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "pastor serve did not exit within {WAIT:?} of SIG{signal}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(exit.success(), "pastor serve exited {exit:?}, not cleanly");

    let socket_file = state.join("pastor.sock");
    assert!(
        !socket_file.exists(),
        "pastor.sock was left behind after SIG{signal}"
    );

    let log = std::fs::read_to_string(&stderr_path).unwrap();
    let label = format!("SIG{signal}");
    assert!(log.contains(&label), "log did not mention {label}: {log}");
}

/// A head at `socket` that records each request's op and answers every one
/// with `reply`.
pub(crate) fn fake_head(
    socket: &std::path::Path,
    reply: &'static [u8],
) -> std::sync::Arc<std::sync::Mutex<Vec<String>>> {
    let listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
    let ops = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = ops.clone();
    std::thread::spawn(move || {
        use std::io::{BufRead, Write};
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut line = String::new();
            let _ = std::io::BufReader::new(&stream).read_line(&mut line);
            let req: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
            seen.lock()
                .unwrap()
                .push(req["op"].as_str().unwrap_or_default().to_string());
            let _ = stream.write_all(reply);
        }
    });
    ops
}

/// What a 0.3.0 head answers to ping: a pong with no protocol.
pub(crate) const OLD_PONG: &[u8] = b"{\"kind\":\"pong\",\"data\":{\"version\":\"0.3.0\"}}\n";

/// A head that takes the connection but never answers ping with a pong, as
/// a busy or wedged one looks from outside.
pub(crate) const NO_PONG: &[u8] =
    b"{\"kind\":\"error\",\"data\":{\"code\":\"boom\",\"message\":\"nope\"}}\n";

pub(crate) const NAMED_FLOCKS: &str =
    "[[flock]]\nname = \"work\"\ndefault = true\n\n[[machine]]\nname = \"pi-1\"\nlocal = true\n";

/// A head at `socket` that speaks IPC protocol `protocol`: it answers ping
/// with a pong, and every other request with the text `said by the head`. It
/// records each request whole.
pub(crate) fn text_head(
    socket: &std::path::Path,
    protocol: u32,
) -> std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>> {
    let listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
    let reqs = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = reqs.clone();
    std::thread::spawn(move || {
        use std::io::{BufRead, Write};
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut line = String::new();
            let _ = std::io::BufReader::new(&stream).read_line(&mut line);
            let req: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
            let reply = if req["op"] == "ping" {
                serde_json::json!({"kind": "pong", "data": {"version": "test", "protocol": protocol}})
            } else {
                serde_json::json!({"kind": "text", "data": "said by the head"})
            };
            seen.lock().unwrap().push(req);
            let _ = stream.write_all(format!("{reply}\n").as_bytes());
        }
    });
    reqs
}

impl Env {
    pub(crate) fn json(&self, args: &[&str]) -> serde_json::Value {
        let out = self.cmd(args);
        assert!(
            out.status.success(),
            "pastor {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "pastor {args:?}: {e}: {}",
                String::from_utf8_lossy(&out.stdout)
            )
        })
    }

    /// A runtime error: exit 1 and the JSON error on stderr with this code.
    pub(crate) fn fails_with(&self, args: &[&str], code: &str) {
        let out = self.cmd(args);
        assert_eq!(out.status.code(), Some(1), "pastor {args:?}");
        let err: serde_json::Value = serde_json::from_slice(&out.stderr)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stderr)));
        assert_eq!(err["code"], code, "pastor {args:?}: {err}");
    }

    pub(crate) fn wait_for(&self, what: &str, args: &[&str], ok: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + WAIT;
        loop {
            let out = self.cmd(args);
            let text = String::from_utf8_lossy(&out.stdout).to_string();
            if ok(&text) {
                return text;
            }
            assert!(Instant::now() < deadline, "never saw {what}: {text}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Force a task's row into `state` behind the daemon's back, as a
    /// dispatch that failed after `agent.start` would leave it. Retried: the
    /// daemon may write the same row in between (optimistic update).
    pub(crate) fn set_state(&self, id: i64, state: pastor::task::TaskState) {
        let store = pastor::store::Store::open(&self.state.join("pastor.db")).unwrap();
        for _ in 0..50 {
            let mut t = store.get_task(id).unwrap().unwrap();
            t.state = state;
            if store.update_task(&mut t).is_ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("could not set t-{id} to {state}");
    }
}

pub(crate) fn wait_for_machine(env: &Env, name: &str, present: bool) {
    let deadline = Instant::now() + WAIT;
    loop {
        let out = env.cmd(&["machine", "list", "--json"]);
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
        let ms = v["machines"].as_array().cloned().unwrap_or_default();
        let ok = match ms.iter().find(|m| m["name"] == name) {
            Some(m) => present && m["channel"] == "connected",
            None => !present,
        };
        if ok {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{name}: present={present} never held: {ms:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The code of a runtime error (JSON on stderr, exit 1).
pub(crate) fn error_code(out: &std::process::Output) -> String {
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stderr)
        .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&out.stderr)));
    v["code"].as_str().unwrap().to_string()
}

pub(crate) fn ok(out: std::process::Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The params of every request for `method` the fake herdr has received.
pub(crate) fn herdr_calls(env: &Env, method: &str) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(&env.herdr_log).unwrap_or_default();
    serde_json::from_str::<Vec<serde_json::Value>>(&text)
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r["method"] == method)
        .map(|r| r["params"].clone())
        .collect()
}

/// Polls `task describe` until `task` is in `state`.
pub(crate) fn wait_state(env: &Env, task: &str, state: &str) -> serde_json::Value {
    let deadline = Instant::now() + WAIT;
    loop {
        let out = env.cmd(&["task", "describe", task, "--json"]);
        let t: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        if t["state"] == state {
            return t;
        }
        assert!(Instant::now() < deadline, "task never became {state}: {t}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// An orchestrator file `name` of `text` in the head's config, with a
/// `pre.sh` of `pre` beside it.
pub(crate) fn write_orchestrator(env: &Env, name: &str, text: &str, pre: &str) {
    use std::os::unix::fs::PermissionsExt;
    let dir = env.config.join("orchestrators");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{name}.toml")), text).unwrap();
    let script = dir.join("pre.sh");
    std::fs::write(&script, format!("#!/bin/sh\n{pre}\n")).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
}

pub(crate) const MERGE: &str =
    "kind = \"scheduled\"\ncron = \"0 3 * * *\"\npre = [\"./pre.sh\"]\nprompt = \"Decide.\"\n";

/// The first fenced block after the first `**Dispatch:**` line, joined across
/// trailing backslashes and split into words the way sh would for the plain
/// and single-quoted words a plan uses.
pub(crate) fn first_dispatch_command(plan: &str) -> Vec<String> {
    let after = plan
        .split_once("**Dispatch:**")
        .expect("a Dispatch block")
        .1;
    let block = after
        .split_once("```bash\n")
        .expect("a bash block")
        .1
        .split_once("```")
        .unwrap()
        .0;
    let line = block.replace("\\\n", " ");
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut any = false;
    for c in line.chars() {
        match c {
            '\'' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any {
                    words.push(std::mem::take(&mut word));
                    any = false;
                }
            }
            c => {
                word.push(c);
                any = true;
            }
        }
    }
    if any {
        words.push(word);
    }
    words
}

/// A config and state dir with no head, and a way to run pastor in them
/// with a given editor. `VISUAL` is cleared so the test's `EDITOR` is the
/// one that runs, whatever the developer's shell has.
pub(crate) struct Offline {
    pub(crate) tmp: tempfile::TempDir,
    pub(crate) config: std::path::PathBuf,
    pub(crate) state: std::path::PathBuf,
}

pub(crate) const NIGHTLY: &str = "# nightly sweep\nevery = \"1h\"\n\n[connector]\nuse = \"clock\"\n\n[dispatch]\nrepo = \"/tmp/r\"\nprompt = \"sweep {{ task.id }}\"\n";

pub(crate) fn offline() -> Offline {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(config.join("jobs")).unwrap();
    std::fs::write(config.join("jobs/nightly.toml"), NIGHTLY).unwrap();
    Offline { tmp, config, state }
}

impl Offline {
    pub(crate) fn edit(&self, editor: &str, args: &[&str], stdin: &str) -> std::process::Output {
        use std::io::Write;
        let mut child = pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &self.config)
            .env("PASTOR_STATE_DIR", &self.state)
            .env_remove("VISUAL")
            .env("EDITOR", editor)
            // Kept copies land in the test's dir, not the real temp dir.
            .env("TMPDIR", self.tmp.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    pub(crate) fn cmd(&self, args: &[&str]) -> std::process::Output {
        self.edit("false", args, "")
    }

    /// An editor script that writes `edits[n]` over the file on its n-th
    /// run (the last one for every later run), and logs what it was given.
    pub(crate) fn editor(&self, edits: &[&str]) -> String {
        use std::os::unix::fs::PermissionsExt;
        let dir = self.tmp.path().join("editor");
        std::fs::create_dir_all(&dir).unwrap();
        for (i, e) in edits.iter().enumerate() {
            std::fs::write(dir.join(format!("edit{i}")), e).unwrap();
        }
        let script = dir.join("ed.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nd={dir}\nn=$(cat $d/count 2>/dev/null || echo 0)\necho $((n+1)) > $d/count\ncp \"$1\" $d/seen$n\nf=$d/edit$n\n[ -f $f ] || f=$d/edit{last}\ncp $f \"$1\"\n",
                dir = dir.display(),
                last = edits.len() - 1
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script.display().to_string()
    }

    pub(crate) fn runs(&self) -> usize {
        std::fs::read_to_string(self.tmp.path().join("editor/count"))
            .map_or(0, |s| s.trim().parse().unwrap())
    }

    pub(crate) fn seen(&self, n: usize) -> String {
        std::fs::read_to_string(self.tmp.path().join(format!("editor/seen{n}"))).unwrap()
    }
}

/// The code and message of a failed edit. The error is the last line of
/// stderr: before it, the edit's problem and the reopen prompt.
pub(crate) fn last_error(out: &std::process::Output) -> (String, String) {
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    let line = stderr.trim_end().lines().last().unwrap_or_default();
    let line = &line[line.find('{').unwrap_or_else(|| panic!("{stderr}"))..];
    let v: serde_json::Value = serde_json::from_str(line).unwrap_or_else(|_| panic!("{stderr}"));
    (
        v["code"].as_str().unwrap().to_string(),
        v["message"].as_str().unwrap().to_string(),
    )
}

/// A usage limit on the account `me`, seen by t-7 on pi-1, that resets in
/// an hour, written straight into the store at `state`.
pub(crate) fn seed_limit(state: &std::path::Path) {
    let now = chrono::Utc::now();
    let store = pastor::store::Store::open(&state.join("pastor.db")).unwrap();
    store
        .record_limit(&pastor::limit::AccountLimit {
            account: "me".into(),
            model: None,
            hard: true,
            no_credit: false,
            until: Some(now + chrono::Duration::hours(1)),
            retry_at: now + chrono::Duration::hours(1),
            line: "5-hour limit reached".into(),
            task_id: Some(7),
            machine: Some("pi-1".into()),
            agent: Some("claude".into()),
            seen_at: now,
        })
        .unwrap();
}

/// `pastor __complete <shell> -- <words>` prints the names the word being
/// typed (the last one) can take, from files alone: no head is running here.
pub(crate) fn complete(
    config: &std::path::Path,
    state: &std::path::Path,
    words: &[&str],
) -> (bool, String) {
    let out = pastor()
        .args(["__complete", "fish", "--"])
        .args(words)
        .env("PASTOR_CONFIG_DIR", config)
        .env("PASTOR_STATE_DIR", state)
        .env("PASTOR_DATA_DIR", state.join("data"))
        .output()
        .unwrap();
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), String::from_utf8(out.stdout).unwrap())
}

/// A config with two jobs, two flocks, two machines and one connector.
pub(crate) fn completion_config() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(config.join("jobs")).unwrap();
    let job =
        "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"p {{ task.id }}\"\n";
    std::fs::write(config.join("jobs/nightly.toml"), job).unwrap();
    std::fs::write(config.join("jobs/triage.toml"), job).unwrap();
    std::fs::write(
        config.join("flock.toml"),
        "[[flock]]\nname = \"home\"\ndefault = true\n\n[[flock]]\nname = \"lab\"\n\n\
         [[machine]]\nname = \"pi-1\"\nssh = \"user@pi-1\"\nflock = \"home\"\n\n\
         [[machine]]\nname = \"pi-2\"\nssh = \"user@pi-2\"\nflock = \"lab\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(state.join("data/connectors/github-issues")).unwrap();
    (tmp, config, state)
}

/// Two models, and one machine whose name holds a comma.
pub(crate) fn fallback_completion_config()
-> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let (tmp, config, state) = completion_config();
    std::fs::write(
        config.join("pastor.toml"),
        "[models.sonnet]\nkind = \"claude\"\nargs = []\n[models.gpt]\nkind = \"opencode\"\nargs = []\n",
    )
    .unwrap();
    std::fs::write(
        config.join("flock.toml"),
        "[[flock]]\nname = \"home\"\ndefault = true\n\n\
         [[machine]]\nname = \"west,east\"\nssh = \"user@pi-1\"\nflock = \"home\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(&state).unwrap();
    (tmp, config, state)
}

/// PATH with the pastor under test first, for the shell scripts' hooks.
pub(crate) fn path_with_pastor() -> String {
    let bin_dir = std::path::Path::new(env!("CARGO_BIN_EXE_pastor"))
        .parent()
        .unwrap()
        .to_path_buf();
    format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

/// Runs the real generated bash script's `_pastor_names` for `words`, the
/// last one under the cursor, and returns COMPREPLY.
pub(crate) fn bash_complete(
    config: &std::path::Path,
    state: &std::path::Path,
    words: &str,
    cword: usize,
) -> Vec<String> {
    let script = pastor().args(["completions", "bash"]).output().unwrap();
    assert!(script.status.success());
    let script_path = state.join("completions.bash");
    std::fs::write(&script_path, &script.stdout).unwrap();
    let out = Command::new("bash")
        .arg("-c")
        .arg(format!(
            r#"
source "{script}"
COMP_WORDS=({words})
COMP_CWORD={cword}
_pastor_names
printf '%s\n' "${{COMPREPLY[@]}}"
"#,
            script = script_path.display()
        ))
        .env("PATH", path_with_pastor())
        .env("PASTOR_CONFIG_DIR", config)
        .env("PASTOR_STATE_DIR", state)
        .env("PASTOR_DATA_DIR", state.join("data"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

/// A second config dir that shares `env`'s state dir, so its socket reaches
/// `env`'s head: a CLI on another machine, with its own (stale) copy of
/// the nightly job. Anything it edits through the head lands in `env`'s
/// config, never in its own.
pub(crate) fn laptop(env: &Env) -> Offline {
    let mut o = offline();
    o.state = env.state.clone();
    o
}

/// A CLI with its own, empty config and state dirs, and a fake `ssh` first on
/// its PATH. The fake ignores ssh's options, runs the remote command here and
/// picks the head by destination: `head-up` is `head`'s state dir, `no-head`
/// an empty one, `unreachable` fails as ssh does when it cannot connect.
pub(crate) struct Client {
    pub(crate) tmp: tempfile::TempDir,
    pub(crate) config: std::path::PathBuf,
    pub(crate) state: std::path::PathBuf,
    /// Its own data dir, so a `connector link` never lands in the real one.
    pub(crate) data: std::path::PathBuf,
    pub(crate) path: std::ffi::OsString,
}

pub(crate) fn client(head: Option<&Env>) -> Client {
    client_to(head.map(|e| (e.config.as_path(), e.state.as_path())))
}

/// `client`, whose `head-up` is the machine with these config and state
/// dirs.
pub(crate) fn client_to(head: Option<(&std::path::Path, &std::path::Path)>) -> Client {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let empty = tmp.path().join("empty");
    let (up_config, up_state) = match head {
        Some((config, state)) => (config.to_path_buf(), state.to_path_buf()),
        None => (empty.clone(), empty.clone()),
    };
    let script = format!(
        r#"#!/bin/sh
while [ $# -gt 0 ]; do
  case "$1" in
    -o) shift 2 ;;
    -*) shift ;;
    *) break ;;
  esac
done
dest=$1; shift
case "$dest" in
  head-up) export PASTOR_CONFIG_DIR='{}' PASTOR_STATE_DIR='{}' ;;
  no-head) export PASTOR_CONFIG_DIR='{e}' PASTOR_STATE_DIR='{e}' ;;
  *) echo "ssh: connect to host $dest port 22: Connection refused" >&2; exit 255 ;;
esac
exec sh -c "$*"
"#,
        up_config.display(),
        up_state.display(),
        e = empty.display(),
    );
    let ssh = bin.join("ssh");
    std::fs::write(&ssh, script).unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut path = std::ffi::OsString::from(&bin);
    path.push(":");
    path.push(std::env::var_os("PATH").unwrap_or_default());
    Client {
        config: tmp.path().join("c"),
        state: tmp.path().join("s"),
        data: tmp.path().join("d"),
        tmp,
        path,
    }
}

impl Client {
    pub(crate) fn cmd(&self, args: &[&str]) -> std::process::Output {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &self.config)
            .env("PASTOR_STATE_DIR", &self.state)
            .env("PASTOR_DATA_DIR", &self.data)
            .env("PATH", &self.path)
            .env_remove("PASTOR_HEAD")
            .output()
            .unwrap()
    }

    /// `head set <dest>` with pastor's path the test binary.
    pub(crate) fn head_set(&self, dest: &str, extra: &[&str]) -> std::process::Output {
        let mut args = vec![
            "head",
            "set",
            dest,
            "--pastor",
            env!("CARGO_BIN_EXE_pastor"),
        ];
        args.extend_from_slice(extra);
        self.cmd(&args)
    }
}

impl Client {
    /// `pastor serve` in the background, its log in `serve.log`.
    pub(crate) fn serve(&self) -> Served {
        let log = self.tmp.path().join("serve.log");
        let child = pastor()
            .args(["serve", "--foreground"])
            .env("PASTOR_CONFIG_DIR", &self.config)
            .env("PASTOR_STATE_DIR", &self.state)
            .env("PASTOR_DATA_DIR", &self.data)
            .env("PATH", &self.path)
            .env_remove("PASTOR_HEAD")
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .spawn()
            .unwrap();
        Served { child, log }
    }

    /// One request line to this machine's own socket, through `pastor
    /// bridge`, and its reply.
    pub(crate) fn local(&self, line: &str) -> serde_json::Value {
        use std::io::Write;
        let mut child = pastor()
            .args(["bridge"])
            .env("PASTOR_CONFIG_DIR", &self.config)
            .env("PASTOR_STATE_DIR", &self.state)
            .env("PASTOR_DATA_DIR", &self.data)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        writeln!(child.stdin.take().unwrap(), "{line}").unwrap();
        let out = child.wait_with_output().unwrap();
        serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&out.stdout)))
    }
}

pub(crate) struct Served {
    pub(crate) child: std::process::Child,
    pub(crate) log: std::path::PathBuf,
}

impl Served {
    pub(crate) fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Wait until the log says `what`, failing if serve exits first.
    pub(crate) fn wait_log(&mut self, what: &str) {
        let deadline = Instant::now() + WAIT;
        while !self.log().contains(what) {
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "serve exited:\n{}",
                self.log()
            );
            assert!(
                Instant::now() < deadline,
                "never logged {what:?}:\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A fake herdr listening on `socket`, killed when dropped, and the
/// `command` that reaches it (`--connect`).
pub(crate) struct FakeHerdrServer(pub(crate) std::process::Child);

impl Drop for FakeHerdrServer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub(crate) fn fake_herdr_at(
    socket: &std::path::Path,
    env: &[(&str, &str)],
) -> (FakeHerdrServer, String) {
    let child = Command::new(env!("CARGO_BIN_EXE_fake-herdr"))
        .arg("--listen")
        .arg(socket)
        .env("FAKE_HERDR_READY_MS", "200")
        .envs(env.iter().copied())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let command = format!(
        "[\"{}\", \"--connect\", \"{}\"]",
        env!("CARGO_BIN_EXE_fake-herdr"),
        socket.display()
    );
    (FakeHerdrServer(child), command)
}

/// The head of `start()` with `laptop` as a pull machine too, and
/// `extra` in its pastor.toml; waits until the head has reloaded.
pub(crate) fn head_with_pull_machine(extra: &str) -> Env {
    let env = start();
    let flock = std::fs::read_to_string(env.config.join("flock.toml")).unwrap();
    std::fs::write(
        env.config.join("flock.toml"),
        format!("{flock}\n[[machine]]\nname = \"laptop\"\npull = true\n"),
    )
    .unwrap();
    let config = std::fs::read_to_string(env.config.join("pastor.toml")).unwrap();
    std::fs::write(env.config.join("pastor.toml"), format!("{config}{extra}")).unwrap();
    let deadline = Instant::now() + WAIT;
    while !ok(env.cmd(&["machine", "list", "--json"])).contains("\"laptop\"") {
        assert!(Instant::now() < deadline, "the head never took laptop");
        std::thread::sleep(Duration::from_millis(100));
    }
    env
}

/// A shepherd whose head is `env` and which is its pull machine `laptop`,
/// running tasks on the fake herdr `command` reaches.
pub(crate) fn pull_shepherd(env: &Env, command: &str) -> (Client, Served) {
    let c = client(Some(env));
    ok(c.head_set("head-up", &[]));
    std::fs::create_dir_all(&c.config).unwrap();
    std::fs::write(
        c.config.join("pastor.toml"),
        format!(
            "tick = \"1s\"\nsettle = \"1s\"\nreconcile_every = \"1s\"\n[shepherd]\nmachine = \"laptop\"\ncommand = {command}\n"
        ),
    )
    .unwrap();
    let mut serve = c.serve();
    serve.wait_log("headless");
    (c, serve)
}

pub(crate) fn task_state(env: &Env, id: &str) -> serde_json::Value {
    serde_json::from_str(&ok(env.cmd(&["task", "describe", id, "--json"]))).unwrap()
}

pub(crate) fn wait_pulled(env: &Env, id: &str, want: &str, serve: &Served) -> serde_json::Value {
    let deadline = Instant::now() + WAIT;
    loop {
        let t = task_state(env, id);
        if t["state"] == want {
            return t;
        }
        assert!(
            Instant::now() < deadline,
            "{id} never {want}: {t}\n{}",
            serve.log()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

impl Client {
    /// `cmd` with `editor` as $EDITOR.
    pub(crate) fn edit(&self, editor: &std::path::Path, args: &[&str]) -> std::process::Output {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &self.config)
            .env("PASTOR_STATE_DIR", &self.state)
            .env("PASTOR_DATA_DIR", &self.data)
            .env("PATH", &self.path)
            .env_remove("PASTOR_HEAD")
            .env_remove("VISUAL")
            .env("EDITOR", editor)
            .env("TMPDIR", self.tmp.path())
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
}

pub(crate) const SWEEP: &str = "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nrepo = \"/tmp\"\nprompt = \"sweep {{ item.key }}\"\n";

/// The two tables of `job list` on a shepherd: the head's, then this
/// machine's, as `(header, body)`.
pub(crate) fn job_sections(text: &str) -> Vec<(String, String)> {
    text.split("\n\n")
        .map(|s| {
            let (header, body) = s.split_once('\n').unwrap_or((s, ""));
            (header.to_string(), body.trim_end().to_string())
        })
        .collect()
}

/// A flock.toml with a `spare` flock nobody uses, and the dirs around it,
/// for the fleet lock tests.
pub(crate) fn spare_flock(tmp: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let config = tmp.join("c");
    let state = tmp.join("s");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        config.join("flock.toml"),
        "[[flock]]\nname = \"work\"\ndefault = true\n\n[[flock]]\nname = \"spare\"\n\n[[machine]]\nname = \"pi-1\"\nlocal = true\n",
    )
    .unwrap();
    (config, state)
}

/// Returns once process `pid` has `fleet.lock` in `state` open: an offline
/// edit opens it only after its ping found no head, just before it waits on
/// the lock. Linux only, from `/proc`, like CI.
///
/// Until it execs, the child is a fork of this test and still holds the
/// test's own lock descriptor, so the fd alone can show up before the ping;
/// under load that let the test listen first and the ping found a head that
/// never answers. Only the fds of the pastor binary count.
pub(crate) fn wait_for_fleet_lock_open(pid: u32, state: &std::path::Path) {
    let lock = state.canonicalize().unwrap().join("fleet.lock");
    let bin = std::path::Path::new(env!("CARGO_BIN_EXE_pastor"))
        .canonicalize()
        .unwrap();
    let deadline = Instant::now() + WAIT;
    loop {
        let exec = std::fs::read_link(format!("/proc/{pid}/exe")).is_ok_and(|p| p == bin);
        let open = exec
            && std::fs::read_dir(format!("/proc/{pid}/fd"))
                .into_iter()
                .flatten()
                .flatten()
                .any(|fd| std::fs::read_link(fd.path()).is_ok_and(|p| p == lock));
        if open {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the edit never opened {}",
            lock.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
