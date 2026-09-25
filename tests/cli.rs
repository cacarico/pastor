//! Drives the real binaries: pastor serve with a fake-herdr machine, then run/list/task.
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long a test waits for the daemon or the fake herdr to do something.
/// Generous on purpose: a CI runner under load has taken more than 10s to
/// bring a daemon up, and a wait that ends early only ever fails a good run.
const WAIT: Duration = Duration::from_secs(60);

fn pastor() -> Command {
    Command::new(env!("CARGO_BIN_EXE_pastor"))
}

struct Env {
    _tmp: tempfile::TempDir,
    config: std::path::PathBuf,
    state: std::path::PathBuf,
    serve: std::process::Child,
    herdr: std::process::Child,
    /// Every request the fake herdr has received, as a JSON array
    /// (`FAKE_HERDR_REQUEST_LOG`).
    herdr_log: std::path::PathBuf,
}

impl Drop for Env {
    fn drop(&mut self) {
        for child in [&mut self.serve, &mut self.herdr] {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn start() -> Env {
    start_with_jobs(&[])
}

/// Like `start()`, but writes each `(name, text)` to `config/jobs/<name>.toml`
/// before spawning `pastor serve`, so the scheduler picks the jobs up at start.
fn start_with_jobs(jobs: &[(&str, &str)]) -> Env {
    start_with(jobs, &[])
}

/// Like `start_with_jobs`, with extra env for the fake herdr
/// (`FAKE_HERDR_TRUST_PROMPT`, ...).
fn start_with(jobs: &[(&str, &str)], herdr_env: &[(&str, &str)]) -> Env {
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
        .args(["serve"])
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
    fn agent_start_params(&self, agent: &str) -> serde_json::Value {
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

    /// Polls `task show` until `task` is done, so later snapshots of it are
    /// stable: the fake finishes agents on its own and the daemon reconciles.
    fn wait_done(&self, task: &str) {
        let deadline = Instant::now() + WAIT;
        loop {
            let out = self.cmd(&["task", "show", task, "--json"]);
            let t: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
            if t["state"] == "done" {
                return;
            }
            assert!(Instant::now() < deadline, "task never became done: {t}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn cmd(&self, args: &[&str]) -> std::process::Output {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &self.config)
            .env("PASTOR_STATE_DIR", &self.state)
            .output()
            .unwrap()
    }

    /// Like `cmd`, with `input` on the command's stdin.
    fn cmd_stdin(&self, args: &[&str], input: &str) -> std::process::Output {
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

#[test]
fn run_list_show_read_end_to_end() {
    let env = start();
    let out = env.cmd(&[
        "task",
        "run",
        "say hello\nthen stop",
        "--repo",
        "/tmp",
        "--json",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let task: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(task["state"], "running");
    assert_eq!(task["machine"], "fake");
    assert_eq!(task["agent_name"], "t-1");

    let out = env.cmd(&["task", "read", "t-1"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("fake output"));

    env.wait_done("t-1");
    // A done task is finished: the default list hides it and says where it
    // went, `--all` shows it, and `--json` follows the same selection.
    let out = env.cmd(&["task", "list"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("t-1"), "{text}");
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("no live tasks; pastor task list --all shows finished ones"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = env.cmd(&["task", "list", "--all"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("t-1") && text.contains("done"), "{text}");
    let out = env.cmd(&["task", "list", "--json"]);
    let tasks: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert!(tasks.is_empty(), "{tasks:?}");
    let out = env.cmd(&["task", "list", "--all", "--json"]);
    let tasks: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(tasks.len(), 1, "{tasks:?}");
    assert_eq!(tasks[0]["agent_name"], "t-1");
    assert_eq!(tasks[0]["state"], "done");

    let out = env.cmd(&["task", "show", "t-9"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("task_not_found"));
}

/// `pastor completions` must offer the real, nested command tree, and never
/// the old top-level spellings (`run`, `list`, `attach`, `reload`) or
/// `machine status`: pastor is fresh software, and those names are gone, not
/// merely hidden. This guards against a regression re-adding them.
#[test]
fn completions_offer_only_the_nested_spellings() {
    let gen_ = |shell: &str| {
        let out = pastor().args(["completions", shell]).output().unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap()
    };
    let fish = gen_("fish");
    let bash = gen_("bash");
    let top = bash
        .lines()
        .find(|l| {
            l.trim_start()
                .starts_with("opts=\"-h -V --skill --help --version")
        })
        .unwrap_or_else(|| panic!("no top-level opts line:\n{bash}"));
    assert!(fish.contains("-l skill"), "fish does not offer --skill");
    for old in ["run", "list", "attach", "reload"] {
        assert!(
            !fish.contains(&format!("__fish_pastor_needs_command\" -f -a \"{old}\"")),
            "fish offers {old}"
        );
        assert!(
            !top.split_whitespace().any(|w| w.trim_matches('"') == old),
            "bash offers {old}: {top}"
        );
        assert!(
            !bash.contains(&format!("pastor,{old})")),
            "bash knows {old}"
        );
    }
    assert!(fish.contains("-f -a \"run\" -d 'Create a one-off task and dispatch it'"));
    assert!(bash.contains("pastor__subcmd__task,run)"));
    assert!(bash.contains("pastor__subcmd__job,reload)"));
    // `machine status` never appears one level down either.
    assert!(
        !bash.contains("pastor__subcmd__machine,status)"),
        "bash knows machine status"
    );
    assert!(
        !fish.contains("-f -a \"status\""),
        "fish offers machine status"
    );
    assert!(bash.contains("pastor__subcmd__machine,list)"));
}

/// `pastor --skill` is how an agent on any machine gets the guide, so it must
/// print the embedded file and nothing else: no log line, no trailing text.
#[test]
fn skill_flag_prints_the_embedded_skill_and_nothing_else() {
    let out = pastor().arg("--skill").output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.starts_with("---\nname: pastor\n"), "{stdout}");
    assert_eq!(stdout, include_str!("../skills/pastor/SKILL.md"));
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // With a command it would be ambiguous which one ran; clap refuses it.
    let out = pastor().args(["--skill", "task", "list"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2), "{:?}", out.status);
}

/// An agent reads `--help` first; the footer is what sends it to the skill.
#[test]
fn help_footer_points_agents_at_the_skill() {
    let out = pastor().arg("--help").output().unwrap();
    assert!(out.status.success());
    let help = String::from_utf8(out.stdout).unwrap();
    let footer = help.trim_end().lines().rev().take(2).collect::<Vec<_>>();
    assert!(
        footer.iter().any(|l| l.contains("pastor --skill")),
        "{help}"
    );
    assert!(
        footer.iter().any(|l| l.contains("already in your context")),
        "{help}"
    );
}

/// `--agent-arg` reaches herdr's `agent.start` as `args`, in order, and shows
/// in `task show` and `task list --json`; without it, the flock's
/// `agent_args` do, then `[defaults] agent_args`.
#[test]
fn agent_args_reach_herdr_from_the_flags_or_the_defaults() {
    let env = start();
    let out = env.cmd(&[
        "task",
        "run",
        "hi",
        "--agent-arg=--model",
        "--agent-arg",
        "claude-opus-5-5",
        "--json",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let want = serde_json::json!(["--model", "claude-opus-5-5"]);
    let task: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(task["spec"]["agent_args"], want);
    let start = env.agent_start_params("t-1");
    assert_eq!(start["args"], want, "{start}");
    assert_eq!(start["kind"], "claude");

    let out = env.cmd(&["task", "show", "t-1"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("agent args: --model claude-opus-5-5"),
        "{text}"
    );
    let out = env.cmd(&["task", "list", "--all", "--json"]);
    let tasks: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(tasks[0]["spec"]["agent_args"], want, "{tasks}");

    // `pastor task run` reads pastor.toml itself, so the daemon need not restart.
    std::fs::write(
        env.config.join("pastor.toml"),
        "tick = \"1s\"\nsettle = \"1s\"\nreconcile_every = \"1s\"\n\
         [defaults]\nagent_args = [\"--model\", \"claude-sonnet-5\"]\n",
    )
    .unwrap();
    let out = env.cmd(&["task", "run", "hi again", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let start = env.agent_start_params("t-2");
    assert_eq!(
        start["args"],
        serde_json::json!(["--model", "claude-sonnet-5"]),
        "{start}"
    );

    // A flock's own agent_args come before `[defaults]`, its deny list
    // reaches claude as --disallowedTools, and `task show` prints what the
    // task resolved to.
    let flock = env.config.join("flock.toml");
    // A third slot, so t-3 need not wait for the first two to settle.
    let machines = std::fs::read_to_string(&flock)
        .unwrap()
        .replace("max_agents = 2", "max_agents = 3");
    std::fs::write(
        &flock,
        format!(
            "[[flock]]\nname = \"default\"\ndefault = true\nagent_args = [\"--model\", \"claude-haiku-4-5\"]\ndeny = [\"WebFetch\"]\n\n{machines}"
        ),
    )
    .unwrap();
    assert!(env.cmd(&["job", "reload"]).status.success());
    let out = env.cmd(&["task", "run", "third", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let start = env.agent_start_params("t-3");
    assert_eq!(
        start["args"],
        serde_json::json!([
            "--model",
            "claude-haiku-4-5",
            "--disallowedTools",
            "WebFetch"
        ]),
        "{start}"
    );
    let out = env.cmd(&["task", "show", "t-3"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("agent:      claude\n"), "{text}");
    assert!(
        text.contains("agent args: --model claude-haiku-4-5"),
        "{text}"
    );
    assert!(text.contains("deny:       WebFetch\n"), "{text}");
}

#[test]
fn machine_add_and_remove_edit_the_file() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .output()
            .unwrap()
    };
    let out = run(&[
        "machine",
        "add",
        "pi-3",
        "fleet@pi-3",
        "--max-agents",
        "3",
        "--tag",
        "fast",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = std::fs::read_to_string(config.join("flock.toml")).unwrap();
    assert!(
        text.contains("name = \"pi-3\"")
            && text.contains("ssh = \"fleet@pi-3\"")
            && text.contains("max_agents = 3"),
        "{text}"
    );
    let out = run(&["machine", "add", "pi-3", "fleet@pi-3"]);
    assert!(!out.status.success());
    // `--branch` only means something for a worktree; clap rejects it alone
    // before any daemon is asked.
    let out = run(&["task", "run", "hi", "--branch", "b"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(run(&["machine", "remove", "pi-3"]).status.success());
    assert!(!run(&["machine", "remove", "pi-3"]).status.success());
}

/// Copilot 4102376166: a socket that accepts but never answers `Ping` is
/// `Unresponsive` (a busy head), not the same as no daemon at all, so
/// `machine add` must not send the "start pastor serve" advice. Copilot
/// 4106353416, 4106669969: nor may it write the edit next to a head it
/// cannot check; it is refused (`head_unresponsive`) with flock.toml
/// untouched.
#[test]
fn machine_add_refuses_a_wedged_head_rather_than_taking_it_for_no_head() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&state).unwrap();
    let socket = state.join("pastor.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    // Accept the connection and never reply, so the probe times out into
    // `Unresponsive` rather than reading as an absent daemon.
    std::thread::spawn(move || {
        let _ = listener.accept();
        std::thread::sleep(Duration::from_secs(10));
    });
    let out = pastor()
        .args(["machine", "add", "pi-9", "fleet@pi-9"])
        .env("PASTOR_CONFIG_DIR", &config)
        .env("PASTOR_STATE_DIR", &state)
        .output()
        .unwrap();
    assert_eq!(error_code(&out), "head_unresponsive");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        !stderr.contains("start pastor serve") && stderr.contains("not answering"),
        "a busy head must not get the start advice: {stderr}"
    );
    assert!(!config.join("flock.toml").exists());
}

#[test]
fn list_without_daemon_reads_the_database() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    let out = pastor()
        .args(["task", "list"])
        .env("PASTOR_CONFIG_DIR", &config)
        .env("PASTOR_STATE_DIR", &state)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not running"), "{stderr}");
    assert!(stderr.contains("no live tasks"), "{stderr}");
    assert!(
        out.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let out = pastor()
        .args(["task", "list", "--all"])
        .env("PASTOR_CONFIG_DIR", &config)
        .env("PASTOR_STATE_DIR", &state)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("no tasks"));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("no live tasks"));
}

/// `--herdr` keeps herdr's saved-machine list in step with the flock: `add`
/// runs `herdr machine add`, `remove` resolves the label to a profile id via
/// `herdr machine list` and runs `herdr machine remove`. A fake `herdr` script
/// first on PATH records what pastor asked of it.
#[test]
fn herdr_flag_adds_and_removes_the_saved_machine_too() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = tmp.path().join("herdr.log");
    let script = format!(
        "#!/bin/sh\necho \"$@\" >> {log}\ncase \"$*\" in *boom*) echo 'herdr says no' >&2; exit 1;; esac\n\
         if [ \"$1 $2\" = \"machine list\" ]; then printf 'id-1\\tother\\tx@y\\tdefault\\tenabled\\nid-2\\tpi-3\\tfleet@pi-3\\tdefault\\tenabled\\n'; fi\n",
        log = log.display()
    );
    std::fs::write(bin.join("herdr"), script).unwrap();
    std::fs::set_permissions(bin.join("herdr"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PATH", &path)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .output()
            .unwrap()
    };
    let calls = || std::fs::read_to_string(&log).unwrap_or_default();

    let out = run(&[
        "machine",
        "add",
        "pi-3",
        "fleet@pi-3",
        "--session",
        "work",
        "--herdr",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        calls().trim(),
        "machine add fleet@pi-3 --label pi-3 --remote-session work"
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        !text.contains("to see it in your laptop"),
        "the hint is replaced by the action: {text}"
    );
    assert!(text.contains("saved in herdr"), "{text}");

    let out = run(&["machine", "remove", "pi-3", "--herdr"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = calls();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[1], "machine list");
    assert_eq!(
        lines[2], "machine remove id-2",
        "resolved by label, not by name"
    );
    assert!(
        !std::fs::read_to_string(config.join("flock.toml"))
            .unwrap()
            .contains("pi-3")
    );

    // Not saved in herdr under that label: the flock edit still happens, a
    // note says so, exit 0. herdr does know the same host under another label
    // (`other`, id-1), so the note names it with the command that removes it,
    // and pastor does not remove it on its own.
    assert!(run(&["machine", "add", "pi-9", "x@y"]).status.success());
    let out = run(&["machine", "remove", "pi-9", "--herdr"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no saved machine"), "{err}");
    assert!(
        err.contains("other") && err.contains("herdr machine remove id-1"),
        "the hint names the saved machine at the same target: {err}"
    );
    assert!(
        !calls()
            .lines()
            .any(|l| l.starts_with("machine remove id-") && l != "machine remove id-2")
    );

    // herdr failing: the flock edit is kept, the failure is reported with herdr's stderr, exit 1.
    let out = run(&["machine", "add", "boom", "fleet@boom", "--herdr"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("herdr_error") && err.contains("herdr says no"),
        "{err}"
    );
    assert!(
        std::fs::read_to_string(config.join("flock.toml"))
            .unwrap()
            .contains("name = \"boom\"")
    );
}

/// The default action is part of the command's own help, not only the README.
#[test]
fn setup_systemd_help_names_the_default_action() {
    let out = pastor()
        .args(["setup", "systemd", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(
        help.contains("With no action flag") && help.contains("enable --now"),
        "{help}"
    );
}

/// `--yes`/`-y` installs with no prompt, so setup runs from a script, a task
/// or `ssh host pastor setup systemd --yes`. Stdin here is not a terminal.
#[test]
fn setup_systemd_yes_installs_without_a_prompt() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    let xdg = tmp.path().join("xdg");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let calls = tmp.path().join("systemctl.log");
    std::fs::write(
        bin.join("systemctl"),
        format!("#!/bin/sh\necho \"$@\" >> {}\n", calls.display()),
    )
    .unwrap();
    std::fs::write(
        bin.join("loginctl"),
        "#!/bin/sh\nif [ \"$1\" = show-user ]; then echo Linger=yes; fi\n",
    )
    .unwrap();
    for f in ["systemctl", "loginctl"] {
        std::fs::set_permissions(bin.join(f), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    for flag in ["--yes", "-y"] {
        let _ = std::fs::remove_file(&calls);
        let out = pastor()
            .args(["setup", "systemd", flag])
            .env("PATH", &path)
            .env("XDG_CONFIG_HOME", &xdg)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "{flag}\nstdout:\n{}\nstderr:\n{stderr}",
            String::from_utf8_lossy(&out.stdout),
        );
        assert!(!stderr.contains("Continue?"), "{flag} prompted: {stderr}");
        assert!(
            xdg.join("systemd/user/pastor.service").exists(),
            "{flag} should install the unit"
        );
        let calls = std::fs::read_to_string(&calls).unwrap();
        assert!(calls.contains("--user daemon-reload"), "{calls}");
        assert!(
            calls.contains("--user enable --now pastor.service"),
            "{calls}"
        );
    }
}

/// Without `--yes` and without a terminal, setup fails at once, telling the
/// caller to pass --yes, instead of waiting on a stdin nobody will answer;
/// and it fails before touching anything.
#[test]
fn setup_systemd_without_a_terminal_fails_fast_and_names_yes() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    let xdg = tmp.path().join("xdg");
    let mut child = pastor()
        .args(["setup", "systemd"])
        .env("XDG_CONFIG_HOME", &xdg)
        .env("PASTOR_CONFIG_DIR", &config)
        .env("PASTOR_STATE_DIR", &state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Hold stdin open: a read_line would block here until the deadline.
    // This guard starts no daemon, so it keeps a short deadline of its own:
    // a regression that blocks on stdin should fail fast, not after WAIT.
    let stdin = child.stdin.take().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("setup systemd waited on a stdin that is not a terminal");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--yes"), "{stderr}");
    assert!(
        !xdg.join("systemd/user/pastor.service").exists(),
        "a refused setup must not write the unit"
    );
    assert!(
        !config.exists() && !state.exists(),
        "a refused setup must stop before permission hardening mutates paths"
    );
}

#[test]
fn clock_job_creates_tasks_end_to_end() {
    let env = start_with_jobs(&[(
        "tick",
        "every = \"1s\"\n[connector]\nuse = \"clock\"\n[dispatch]\nrepo = \"/tmp\"\nprompt = \"clock {{ item.key }} for {{ job.name }} as {{ task.id }}\"\n",
    )]);
    let deadline = Instant::now() + WAIT;
    let tasks: Vec<serde_json::Value> = loop {
        let out = env.cmd(&["task", "list", "--all", "--job", "tick", "--json"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let tasks: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
        if !tasks.is_empty() {
            break tasks;
        }
        assert!(
            Instant::now() < deadline,
            "the clock job never queued a task"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let t = &tasks[tasks.len() - 1];
    assert_eq!(t["job"], "tick");
    let prompt = t["prompt"].as_str().unwrap();
    assert!(prompt.starts_with("clock 20"), "{prompt}");
    assert!(prompt.contains(" for tick as t-"), "{prompt}");
    assert_eq!(t["item"]["key"], prompt.split(' ').nth(1).unwrap());

    let out = env.cmd(&["job", "list"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("tick") && text.contains("every 1s") && text.contains("ok:"),
        "{text}"
    );

    // The clock connector keys an item by the current wall-clock second, and
    // the background scheduler (also "every 1s") is racing this forced dry
    // run for that same second: whichever gets there first marks it seen. A
    // 0-created dry run right after a real run is correct product behaviour
    // (the item shows up as `skipped_seen` instead), so assert the outcome
    // either way rather than retry for the lucky one.
    let out = env.cmd(&["tick", "--dry-run", "--job", "tick", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let runs: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(runs[0]["outcome"], "dry_run");
    assert_eq!(runs[0]["items"], 1);
    let created = runs[0]["created"].as_array().unwrap().len();
    let skipped = runs[0]["skipped_seen"].as_u64().unwrap();
    assert_eq!(
        created as u64 + skipped,
        1,
        "the one item is either newly created or already claimed by the background tick: {runs:?}"
    );

    let out = env.cmd(&["job", "disable", "tick"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let file = std::fs::read_to_string(env.config.join("jobs/tick.toml")).unwrap();
    assert!(file.contains("enabled = false\n"), "{file}");
    let out = env.cmd(&["job", "list", "--json"]);
    let jobs: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        jobs[0]["enabled"], false,
        "disable reloads the daemon at once"
    );

    let out = env.cmd(&["job", "run", "tick"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("started"));

    let out = env.cmd(&["job", "run", "ghost"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("job_not_found"));

    // `job_path` joins the raw name under the jobs dir; an unvalidated name
    // like "../pastor" would resolve to config/pastor.toml, which exists, so
    // this must be rejected by name before any existence check runs.
    let pastor_toml_before = std::fs::read_to_string(env.config.join("pastor.toml")).unwrap();
    let out = env.cmd(&["job", "disable", "../pastor"]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("job_not_found"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(env.config.join("pastor.toml")).unwrap(),
        pastor_toml_before,
        "a path-traversal job name must not touch pastor.toml"
    );
}

#[test]
fn tick_without_daemon_queues_tasks_for_later() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(config.join("jobs")).unwrap();
    std::fs::write(
        config.join("jobs/tick.toml"),
        "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"p {{ task.id }}\"\n",
    )
    .unwrap();
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .output()
            .unwrap()
    };
    // No daemon and no background scheduler process exists here, so unlike
    // the e2e test above a dry run is not racing anything: it must create
    // exactly the one item and write nothing to the store.
    let out = run(&["tick", "--dry-run", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let runs: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(runs[0]["outcome"], "dry_run");
    assert_eq!(runs[0]["created"].as_array().unwrap().len(), 1);
    let out = run(&["task", "list", "--json"]);
    let tasks: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert!(tasks.is_empty(), "--dry-run must write nothing: {tasks:?}");

    let out = run(&["tick", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("not running"));
    let runs: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(runs[0]["outcome"], "ran");
    let out = run(&["task", "list", "--json"]);
    let tasks: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["state"], "queued");
    assert_eq!(tasks[0]["prompt"], "p t-1");
    let out = run(&["job", "list"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("ok: 1 items, 1 tasks"));
}

/// Offline `tick` takes each task's flock from flock.toml. One that does not
/// load (here, two defaults) is refused, as the head refuses to start on it,
/// rather than read as the implicit `default` flock, which would queue the
/// job's task for machines the file never put it on.
#[test]
fn tick_without_daemon_refuses_a_flock_file_that_does_not_load() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(config.join("jobs")).unwrap();
    std::fs::write(
        config.join("jobs/tick.toml"),
        "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nprompt = \"p\"\n",
    )
    .unwrap();
    std::fs::write(
        config.join("flock.toml"),
        "[[flock]]\nname = \"work\"\ndefault = true\n[[flock]]\nname = \"home\"\ndefault = true\n",
    )
    .unwrap();
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .output()
            .unwrap()
    };
    assert_eq!(error_code(&run(&["tick", "--json"])), "runtime_error");
    std::fs::remove_file(config.join("flock.toml")).unwrap();
    let out = run(&["task", "list", "--all", "--json"]);
    let tasks: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert!(tasks.is_empty(), "nothing queued: {tasks:?}");
}

/// `machine list` without a head connects directly, so nothing else has made the
/// state dir yet. The ssh ControlMaster socket directory must exist, private,
/// before ssh starts. A fake `ssh` first on PATH records that it did and fails
/// the way an unreachable host does, so no real ssh runs.
#[test]
fn machine_list_without_daemon_creates_the_ssh_dir_private_before_ssh_runs() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let seen = tmp.path().join("seen");
    let script = format!(
        "#!/bin/sh\n[ -d {ssh} ] && echo present > {seen}\necho 'no route to host' >&2\nexit 255\n",
        ssh = state.join("ssh").display(),
        seen = seen.display()
    );
    std::fs::write(bin.join("ssh"), script).unwrap();
    std::fs::set_permissions(bin.join("ssh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PATH", &path)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .output()
            .unwrap()
    };
    assert!(
        run(&["machine", "add", "pi-3", "fleet@pi-3"])
            .status
            .success()
    );
    let _ = std::fs::remove_dir_all(&state);

    let out = run(&["machine", "list", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&seen).unwrap_or_default().trim(),
        "present",
        "ssh ran before its ControlPath directory existed"
    );
    for dir in [state.clone(), state.join("ssh")] {
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{} is {mode:o}", dir.display());
    }
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
struct ServeEnv {
    serve: std::process::Child,
    herdr: std::process::Child,
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
fn assert_daemon_shuts_down_cleanly_on(signal: &str) {
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
        .args(["serve"])
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

#[test]
fn sigterm_shuts_the_daemon_down_cleanly() {
    assert_daemon_shuts_down_cleanly_on("TERM");
}

#[test]
fn sighup_shuts_the_daemon_down_cleanly() {
    assert_daemon_shuts_down_cleanly_on("HUP");
}

/// With a head running, `machine list` starts with the head's own row and
/// gives each machine its host; `--json` keeps the head out of `machines`.
#[test]
fn machine_list_opens_with_a_line_about_the_head() {
    let env = start();
    let out = env.cmd(&["machine", "list"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    // The head runs no agents here (its one machine is a command bridge), so
    // the line does not end by naming it. The hostname and herdr version
    // are this host's.
    let first = stdout.lines().next().unwrap();
    assert!(
        first.starts_with(&format!("pastor {} on ", env!("CARGO_PKG_VERSION"))),
        "{stdout}"
    );
    assert!(first.contains(" (herdr "), "{stdout}");
    assert!(first.ends_with("), 1 machine"), "{stdout}");
    assert_eq!(stdout.lines().nth(1), Some(""), "{stdout}");
    let lines: Vec<Vec<&str>> = stdout
        .lines()
        .skip(2)
        .map(|l| l.split_whitespace().collect())
        .collect();
    assert_eq!(
        lines[0],
        [
            "NAME", "HOST", "FLOCK", "CHANNEL", "HERDR", "PASTOR", "AGENTS", "ORPHANS", "TAGS",
            "ERROR"
        ],
        "{stdout}"
    );
    assert_eq!(
        lines[1][..4],
        ["fake", "fake-herdr", "default", "connected"],
        "{stdout}"
    );
    // A command bridge cannot say which pastor is behind it.
    assert_eq!(lines[1][5..7], ["-", "0/2"], "{stdout}");
    assert_eq!(lines.len(), 2, "{stdout}");

    let out = env.cmd(&["machine", "list", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["head"]["name"], "pastor");
    assert_eq!(v["head"]["channel"], "head");
    assert_eq!(v["head"]["pastor_version"], env!("CARGO_PKG_VERSION"));
    let ms = v["machines"].as_array().unwrap();
    assert_eq!(ms.len(), 1, "{v}");
    assert_eq!(ms[0]["name"], "fake");
    assert!(ms[0]["pastor_version"].is_null(), "{v}");
    assert_eq!(ms[0]["host"], "fake-herdr");
    assert_eq!(ms[0]["channel"], "connected");
    assert_eq!(ms[0]["flock"], "default");

    // --flock narrows the machines, and the line counts what it shows.
    ok(env.cmd(&["flock", "add", "work"]));
    let out = ok(env.cmd(&["machine", "list", "--flock", "work"]));
    assert!(
        out.lines().next().unwrap().ends_with("), 0 machines"),
        "{out}"
    );
    assert_eq!(out.lines().count(), 3, "the header only: {out}");
    let v: serde_json::Value = serde_json::from_str(&ok(
        env.cmd(&["machine", "list", "--json", "--flock", "default"])
    ))
    .unwrap();
    assert_eq!(v["machines"].as_array().unwrap().len(), 1, "{v}");
}

/// With no head, `machine list` probes each machine itself: a reachable one
/// reads `probed` with herdr's agent count, one that cannot be reached reads
/// `unreachable` with the reason. The note goes to stderr only on success, so
/// a failure still leaves exactly one JSON value there.
#[test]
fn machine_list_without_daemon_probes_each_machine() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();
    let socket = tmp.path().join("herdr.sock");
    // Killed on drop, so a failed assertion does not leave it running.
    struct Server(std::process::Child);
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _herdr = Server(
        Command::new(env!("CARGO_BIN_EXE_fake-herdr"))
            .arg("--listen")
            .arg(&socket)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + WAIT;
    while !socket.exists() {
        assert!(Instant::now() < deadline, "fake herdr never listened");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::fs::write(
        config.join("flock.toml"),
        format!(
            "[[machine]]\nname = \"fake\"\ncommand = [\"{}\", \"--connect\", \"{}\"]\nmax_agents = 2\ntags = [\"arm\"]\n\n\
             [[machine]]\nname = \"gone\"\ncommand = [\"{}\"]\nmax_agents = 1\n",
            env!("CARGO_BIN_EXE_fake-herdr"),
            socket.display(),
            tmp.path().join("no-such-bridge").display()
        ),
    )
    .unwrap();
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .output()
            .unwrap()
    };

    let out = run(&["machine", "list"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "pastor serve is not running; probed the machines directly"
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<Vec<&str>> = stdout
        .lines()
        .map(|l| l.split_whitespace().collect())
        .collect();
    // No head, no line about it: the stderr notice says so instead.
    assert_eq!(
        lines[0][..4],
        ["NAME", "HOST", "FLOCK", "CHANNEL"],
        "{stdout}"
    );
    assert_eq!(
        lines[1][..4],
        ["fake", "fake-herdr", "default", "probed"],
        "{stdout}"
    );
    assert_eq!(&lines[1][5..], ["-", "0/2", "-", "arm"], "{stdout}");
    assert_eq!(
        lines[2][..4],
        ["gone", "no-such-bridge", "default", "unreachable"],
        "{stdout}"
    );
    assert_eq!(lines[2][4..7], ["-", "-", "-/1"], "{stdout}");
    assert!(lines[2].len() > 9, "ERROR should say why: {stdout}");

    // The same JSON shape as with a head.
    let out = run(&["machine", "list", "--json"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["head"]["name"], "pastor");
    let ms = v["machines"].as_array().unwrap();
    assert_eq!(ms.len(), 2, "{v}");
    assert_eq!(ms[1]["name"], "gone");
    assert_eq!(ms[1]["channel"], "unreachable");
    assert!(ms[1]["error"].is_string(), "{v}");
    assert_eq!(ms[0]["channel"], "probed");
    assert_eq!(ms[0]["live"], 0);

    std::fs::write(config.join("flock.toml"), "[[machine]\n").unwrap();
    let out = run(&["machine", "list"]);
    assert_eq!(out.status.code(), Some(1));
    let err: serde_json::Value = serde_json::from_slice(&out.stderr)
        .unwrap_or_else(|e| panic!("stderr is not one JSON value ({e})"));
    assert!(err["code"].is_string(), "{err}");
}

/// A `--local` machine with no herdr listening fails to connect with a message
/// naming its own `herdr.sock`, which `probe_fields` reads as `server down`
/// rather than the generic `unreachable` a network-level failure gets.
/// `XDG_CONFIG_HOME` points `local_socket_path` at a directory with nothing
/// listening, so the connect fails the same way it would on a machine that has
/// never run `herdr server`.
#[test]
fn machine_list_without_daemon_reads_a_local_machine_with_no_server_as_down() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    let xdg = tmp.path().join("xdg");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("flock.toml"),
        "[[machine]]\nname = \"loopback\"\nlocal = true\nsession = \"default\"\nmax_agents = 1\n",
    )
    .unwrap();

    let out = pastor()
        .args(["machine", "list"])
        .env("PASTOR_CONFIG_DIR", &config)
        .env("PASTOR_STATE_DIR", &state)
        .env("XDG_CONFIG_HOME", &xdg)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<Vec<&str>> = stdout
        .lines()
        .map(|l| l.split_whitespace().collect())
        .collect();
    assert_eq!(lines[1][..2], ["loopback", "local"], "{stdout}");
    // "server down" has a space, so it lands split across two columns.
    assert_eq!(lines[1][3..5], ["server", "down"], "{stdout}");

    let out = pastor()
        .args(["machine", "list", "--json"])
        .env("PASTOR_CONFIG_DIR", &config)
        .env("PASTOR_STATE_DIR", &state)
        .env("XDG_CONFIG_HOME", &xdg)
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let ms = v["machines"].as_array().unwrap();
    assert_eq!(ms.len(), 1, "{v}");
    assert_eq!(ms[0]["channel"], "server down", "{v}");
    let error = ms[0]["error"].as_str().expect("error string");
    assert!(error.contains("herdr.sock"), "{error}");
}

/// With no head, a `--local` machine that answers is the head's own host, so
/// its PASTOR is this binary's version. `XDG_CONFIG_HOME` points
/// `local_socket_path` at a fake herdr listening where herdr's default
/// session would.
#[test]
fn machine_list_without_daemon_shows_a_local_machine_s_pastor_version() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    let xdg = tmp.path().join("xdg");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(xdg.join("herdr")).unwrap();
    let socket = xdg.join("herdr").join("herdr.sock");
    struct Server(std::process::Child);
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _herdr = Server(
        Command::new(env!("CARGO_BIN_EXE_fake-herdr"))
            .arg("--listen")
            .arg(&socket)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + WAIT;
    while !socket.exists() {
        assert!(Instant::now() < deadline, "fake herdr never listened");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::fs::write(
        config.join("flock.toml"),
        "[[machine]]\nname = \"here\"\nlocal = true\nsession = \"default\"\nmax_agents = 1\n",
    )
    .unwrap();
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .env("XDG_CONFIG_HOME", &xdg)
            .output()
            .unwrap()
    };

    let out = run(&["machine", "list"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<Vec<&str>> = stdout
        .lines()
        .map(|l| l.split_whitespace().collect())
        .collect();
    assert_eq!(lines[0][4..6], ["HERDR", "PASTOR"], "{stdout}");
    assert_eq!(
        lines[1][..6],
        [
            "here",
            "local",
            "default",
            "probed",
            "fake",
            env!("CARGO_PKG_VERSION")
        ],
        "{stdout}"
    );

    let out = run(&["machine", "list", "--json"]);
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        v["machines"][0]["pastor_version"],
        env!("CARGO_PKG_VERSION"),
        "{v}"
    );
}

/// A head at `socket` that records each request's op and answers every one
/// with `reply`.
fn fake_head(
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
const OLD_PONG: &[u8] = b"{\"kind\":\"pong\",\"data\":{\"version\":\"0.3.0\"}}\n";

/// A head that takes the connection but never answers ping with a pong, as
/// a busy or wedged one looks from outside.
const NO_PONG: &[u8] = b"{\"kind\":\"error\",\"data\":{\"code\":\"boom\",\"message\":\"nope\"}}\n";

const NAMED_FLOCKS: &str =
    "[[flock]]\nname = \"work\"\ndefault = true\n\n[[machine]]\nname = \"pi-1\"\nlocal = true\n";

/// A head from before flocks ignores the `flock` field of a `run` request
/// (serde skips unknown fields) and would queue the task in any flock. The
/// CLI asks the head's IPC protocol first and refuses `--flock` on an old
/// one, before anything is queued; `task list --flock` likewise, since an
/// old head would list every flock's tasks. Flock edits to flock.toml are
/// refused too, since the old head would reload them as one flock.
#[test]
fn flock_flags_refuse_a_head_from_before_flocks() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let ops = fake_head(&state.join("pastor.sock"), OLD_PONG);

    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .output()
            .unwrap()
    };
    for args in [
        &["task", "run", "hi", "--flock", "work"][..],
        &["task", "list", "--flock", "work"],
        // Copilot 4106353529: an old head's machines carry no flock.
        &["machine", "list", "--flock", "work"],
    ] {
        assert_eq!(error_code(&run(args)), "head_too_old", "{args:?}");
    }

    // Copilot 4106204671: flock.toml edits the old head would reload but not
    // honour (it reads every machine as one flock) are refused before the
    // file is written.
    let flock_file = config.join("flock.toml");
    let before = "[[machine]]\nname = \"pi-1\"\nlocal = true\n";
    std::fs::write(&flock_file, before).unwrap();
    for args in [
        &["flock", "add", "work"][..],
        &["flock", "add", "work", "--default"],
        &["flock", "default", "default"],
        &["flock", "remove", "default"],
        &["machine", "add", "pi-2", "--local"],
        &["machine", "move", "pi-1", "default"],
        &["machine", "remove", "pi-1"],
    ] {
        assert_eq!(error_code(&run(args)), "head_too_old", "{args:?}");
        assert_eq!(std::fs::read_to_string(&flock_file).unwrap(), before);
    }
    let ops = ops.lock().unwrap();
    assert!(ops.iter().all(|op| op == "ping"), "only pings: {ops:?}");
}

/// Copilot 4106353474: once flock.toml declares named flocks, a command with
/// no `--flock` still means "the default flock", which an old head would
/// read as every machine. So every path that talks to or reloads the head
/// asks its protocol first, not only the ones that take `--flock`.
#[test]
fn named_flocks_refuse_every_head_path_on_a_head_from_before_flocks() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(config.join("flock.toml"), NAMED_FLOCKS).unwrap();
    let ops = fake_head(&state.join("pastor.sock"), OLD_PONG);

    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .output()
            .unwrap()
    };
    for args in [
        &["task", "run", "hi"][..],
        &["task", "list"],
        &["task", "show", "t-1"],
        &["machine", "list"],
        &["flock", "list"],
        &["job", "list"],
        &["tick"],
    ] {
        assert_eq!(error_code(&run(args)), "head_too_old", "{args:?}");
    }
    let ops = ops.lock().unwrap();
    assert!(ops.iter().all(|op| op == "ping"), "only pings: {ops:?}");
}

/// Copilot 4106353416, 4106669969: a head that is listening but does not
/// answer ping is a hard error (`head_unresponsive`) on every path that talks
/// to or reloads the head, flocks or not. Taking it for no head would let
/// `tick` start a second scheduler next to it, or an edit or a prune go
/// offline behind it. The head is pinged once per command: no second probe
/// can read it differently later in the same command.
#[test]
fn an_unresponsive_head_is_a_hard_error_on_every_head_path() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let flock_file = config.join("flock.toml");
    let before = "[[machine]]\nname = \"pi-1\"\nlocal = true\n";
    std::fs::write(&flock_file, before).unwrap();
    let ops = fake_head(&state.join("pastor.sock"), NO_PONG);

    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .output()
            .unwrap()
    };
    let paths: [&[&str]; 14] = [
        &["tick"],
        &["job", "list"],
        &["job", "enable", "j"],
        &["task", "run", "hi"],
        &["task", "run", "hi", "--flock", "work"],
        &["task", "list"],
        &["task", "show", "t-1"],
        &["task", "prune", "--done", "--older-than", "3d"],
        &["machine", "list"],
        &["machine", "add", "pi-2", "--local"],
        &["machine", "move", "pi-1", "default"],
        &["flock", "list"],
        &["flock", "add", "work"],
        &["flock", "remove", "default"],
    ];
    for args in paths {
        let out = run(args);
        assert_eq!(error_code(&out), "head_unresponsive", "{args:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("not answering") && !stderr.contains("start pastor serve"),
            "{args:?}: {stderr}"
        );
        assert_eq!(std::fs::read_to_string(&flock_file).unwrap(), before);
    }
    assert!(!state.join("pastor.db").exists(), "nothing ran offline");
    // `task attach` goes to the machine directly, so a head that does not
    // answer must not stop it: it fails on the missing task, not the head.
    let out = run(&["task", "attach", "t-9"]);
    assert_ne!(error_code(&out), "head_unresponsive");
    let ops = ops.lock().unwrap();
    assert_eq!(ops.len(), paths.len(), "one ping per command: {ops:?}");
    assert!(ops.iter().all(|op| op == "ping"), "only pings: {ops:?}");
}

/// A socket that accepts connections but does not answer `Ping` in time is
/// `Unresponsive`, the same state a head busy mid-dispatch is in. `machine
/// list` must not probe the machines around it (which would also print the
/// "not running" note for a head that is only busy): the command stops with
/// `head_unresponsive`. The fake daemon answers the ping with something other
/// than `Pong`, which reads as `Unresponsive` at once, with no wait for the
/// real 2s ping timeout.
#[test]
fn machine_list_treats_an_unresponsive_daemon_as_running_not_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        config.join("flock.toml"),
        "[[machine]]\nname = \"pi-3\"\nssh = \"fleet@pi-3\"\nmax_agents = 1\n",
    )
    .unwrap();

    let socket = state.join("pastor.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || {
        use std::io::Write;
        if let Ok((mut stream, _)) = listener.accept() {
            let reply =
                serde_json::to_string(&pastor::ipc::IpcResponse::error("boom", "nope")).unwrap();
            let _ = stream.write_all(reply.as_bytes());
            let _ = stream.write_all(b"\n");
        }
        if let Ok((stream, _)) = listener.accept() {
            drop(stream);
        }
    });

    let out = pastor()
        .args(["machine", "list"])
        .env("PASTOR_CONFIG_DIR", &config)
        .env("PASTOR_STATE_DIR", &state)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "an unresponsive daemon must not be probed around: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap_or_else(|e| {
        panic!(
            "stderr is not one JSON value ({e}): {}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    assert_eq!(err["code"], "head_unresponsive");
    let message = err["message"].as_str().unwrap();
    assert!(message.contains("not answering"), "{message}");
    assert!(
        !message.contains("not running"),
        "an unresponsive daemon must not be reported as absent: {message}"
    );
}

impl Env {
    fn json(&self, args: &[&str]) -> serde_json::Value {
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
    fn fails_with(&self, args: &[&str], code: &str) {
        let out = self.cmd(args);
        assert_eq!(out.status.code(), Some(1), "pastor {args:?}");
        let err: serde_json::Value = serde_json::from_slice(&out.stderr)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stderr)));
        assert_eq!(err["code"], code, "pastor {args:?}: {err}");
    }

    fn wait_for(&self, what: &str, args: &[&str], ok: impl Fn(&str) -> bool) -> String {
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
    fn set_state(&self, id: i64, state: pastor::task::TaskState) {
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

#[test]
fn task_retry_close_and_prune_end_to_end() {
    let env = start();
    let t1 = env.json(&["task", "run", "first", "--json"]);
    assert_eq!(t1["state"], "running");
    env.wait_for("t-1 done", &["task", "show", "t-1", "--json"], |t| {
        t.contains("\"done\"")
    });
    // Only failed or stale tasks retry.
    env.fails_with(&["task", "retry", "t-1"], "not_retryable");
    env.fails_with(&["task", "retry", "t-99"], "task_not_found");

    // A failed row whose agent is still up: retry makes a new task, and the
    // old agent is an orphan until it is closed.
    env.set_state(1, pastor::task::TaskState::Failed);
    let t2 = env.json(&["task", "retry", "t-1", "--json"]);
    assert_eq!(t2["id"], 2);
    assert_eq!(t2["retry_of"], 1);
    assert_eq!(t2["state"], "running");
    let list = env.wait_for("the orphan in list", &["task", "list"], |t| {
        t.contains("orphan")
    });
    assert!(list.contains("t-1") && list.contains("fake"), "{list}");
    assert!(
        list.contains("retry of t-1"),
        "the new row points back: {list}"
    );
    let status = env.json(&["machine", "list", "--json"]);
    assert_eq!(
        status["machines"][0]["orphans"],
        serde_json::json!(["t-1"]),
        "{status}"
    );
    // An orphan has no row, so no state or job: a view narrowed by either
    // leaves it out, and --all keeps it.
    for args in [
        &["task", "list", "--done"][..],
        &["task", "list", "--blocked"],
        &["task", "list", "--job", "run"],
    ] {
        let out = env.cmd(args);
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(!text.contains("orphan"), "{args:?}: {text}");
    }
    let all = env.cmd(&["task", "list", "--all"]);
    assert!(String::from_utf8_lossy(&all.stdout).contains("orphan"));
    // --flock narrows the orphans to that flock's machines too.
    env.cmd(&["flock", "add", "work"]);
    let work = env.cmd(&["task", "list", "--all", "--flock", "work"]);
    let text = String::from_utf8_lossy(&work.stdout);
    assert!(!text.contains("orphan"), "{text}");
    let default = env.cmd(&["task", "list", "--all", "--flock", "default"]);
    assert!(String::from_utf8_lossy(&default.stdout).contains("orphan"));

    let closed = env.json(&["task", "close", "t-1", "--json"]);
    assert_eq!(closed["state"], "closed");
    env.wait_for("no orphan", &["task", "list"], |t| !t.contains("orphan"));

    // Close with and without --remove-worktree.
    env.fails_with(
        &["task", "close", "t-2", "--remove-worktree"],
        "no_worktree",
    );
    let out = env.cmd(&["task", "close", "t-2"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("closed"));
    let t3 = env.json(&[
        "task",
        "run",
        "third",
        "--worktree",
        "--repo",
        "/tmp/r",
        "--json",
    ]);
    assert_eq!(t3["state"], "running");
    let closed = env.json(&["task", "close", "t-3", "--remove-worktree", "--json"]);
    assert_eq!(closed["state"], "closed");
    let out = env.cmd(&["task", "run", "no repo", "--worktree"]);
    assert_eq!(out.status.code(), Some(2), "clap refuses --worktree alone");

    // Prune: every closed task finished before "now minus 0s".
    let out = env.cmd(&["task", "prune", "--older-than", "3d"]);
    assert_eq!(out.status.code(), Some(2), "a state flag is required");
    let pruned = env.json(&["task", "prune", "--done", "--older-than", "3d", "--json"]);
    assert_eq!(pruned["pruned"], 0);
    let pruned = env.json(&["task", "prune", "--closed", "--older-than", "0s", "--json"]);
    assert_eq!(
        pruned["pruned"], 2,
        "t-3, the newest, stays so its id is not reused"
    );
    env.fails_with(&["task", "show", "t-1"], "task_not_found");
    assert_eq!(
        env.json(&["task", "show", "t-3", "--json"])["state"],
        "closed"
    );
    let out = env.cmd(&[
        "task",
        "prune",
        "--failed",
        "--closed",
        "--older-than",
        "1d",
    ]);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("pruned 0"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// Without a head, prune still works on the database; retry and close need
/// the daemon and say so with a stable code.
#[test]
fn prune_works_without_a_daemon_and_retry_does_not() {
    let tmp = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", tmp.path().join("c"))
            .env("PASTOR_STATE_DIR", tmp.path().join("s"))
            .output()
            .unwrap()
    };
    let out = run(&["task", "prune", "--done", "--older-than", "1d", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["pruned"], 0);
    for args in [["task", "retry", "t-1"], ["task", "close", "t-1"]] {
        let out = run(&args);
        assert_eq!(out.status.code(), Some(1));
        let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
        assert_eq!(err["code"], "daemon_not_running", "{err}");
    }
    let out = run(&["task", "retry", "x-1"]);
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["code"], "usage_error");
}

fn wait_for_machine(env: &Env, name: &str, present: bool) {
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

/// `machine add` and `machine remove` reach a running head without a
/// restart, and a task left on a removed machine shows as such (Review
/// Focus 3).
#[test]
fn flock_edits_reach_a_running_head() {
    let env = start();
    let socket = env.config.parent().unwrap().join("herdr.sock");
    let bridge = format!(
        "{} --connect {}",
        env!("CARGO_BIN_EXE_fake-herdr"),
        socket.display()
    );
    let out = env.cmd(&[
        "machine",
        "add",
        "second",
        "--command",
        &bridge,
        "--max-agents",
        "1",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let said = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(said.contains("picked it up"), "{said}");
    wait_for_machine(&env, "second", true);

    let out = env.cmd(&["task", "run", "hello", "--machine", "second", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let task: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(task["machine"], "second", "{task}");

    let out = env.cmd(&["machine", "remove", "second"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("picked it up"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    wait_for_machine(&env, "second", false);
    // --all: the fake finishes agents on its own, and a task it finished
    // before the removal is done, which the default live view hides.
    let list = String::from_utf8_lossy(&env.cmd(&["task", "list", "--all"]).stdout).into_owned();
    assert!(list.contains("second (removed)"), "{list}");

    // Copilot 4103271200, 4103271289, 4103271156: a flock.toml that is
    // momentarily absent (an editor's delete-and-rename, a race with
    // `machine add|remove` rewriting it) must not be read as an empty
    // flock, which would mark every machine "(removed)". `machine remove`
    // above already rewrote the file without "second"; deleting it outright
    // stands in for that window.
    std::fs::remove_file(env.config.join("flock.toml")).unwrap();
    let list = String::from_utf8_lossy(&env.cmd(&["task", "list", "--all"]).stdout).into_owned();
    assert!(
        list.contains("second") && !list.contains("(removed)"),
        "{list}"
    );
}

/// The code of a runtime error (JSON on stderr, exit 1).
fn error_code(out: &std::process::Output) -> String {
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

fn ok(out: std::process::Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `flock` and `machine move` edit flock.toml in place: what they do not
/// touch, comments included, stays as the user wrote it.
#[test]
fn flock_commands_edit_the_file_and_keep_its_comments() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("c");
    let state = tmp.path().join("s");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("flock.toml"),
        "# the fleet\n\n[[machine]]\nname = \"pi-1\"   # desk\nlocal = true\n",
    )
    .unwrap();
    let run = |args: &[&str]| {
        pastor()
            .args(args)
            .env("PASTOR_CONFIG_DIR", &config)
            .env("PASTOR_STATE_DIR", &state)
            .output()
            .unwrap()
    };
    let file = || std::fs::read_to_string(config.join("flock.toml")).unwrap();
    ok(run(&["flock", "add", "work"]));
    let text = file();
    assert!(text.starts_with("# the fleet\n"), "{text}");
    assert!(text.contains("name = \"pi-1\"   # desk"), "{text}");
    ok(run(&[
        "machine",
        "add",
        "pi-3",
        "user@pi-3",
        "--flock",
        "work",
    ]));
    assert_eq!(
        error_code(&run(&[
            "machine",
            "add",
            "pi-4",
            "user@pi-4",
            "--flock",
            "nope"
        ])),
        "unknown_flock"
    );

    let list: serde_json::Value =
        serde_json::from_str(&ok(run(&["flock", "list", "--json"]))).unwrap();
    assert_eq!(list[0]["name"], "default");
    assert_eq!(list[0]["default"], true);
    assert_eq!(list[0]["machines"], serde_json::json!(["pi-1"]));
    assert_eq!(list[1]["name"], "work");
    assert_eq!(list[1]["default"], false);
    assert_eq!(list[1]["machines"], serde_json::json!(["pi-3"]));
    assert_eq!(list[1]["queued"], 0);
    let table = ok(run(&["flock", "list"]));
    let header: Vec<&str> = table.lines().next().unwrap().split_whitespace().collect();
    assert_eq!(header, ["NAME", "DEFAULT", "MACHINES", "AGENTS", "QUEUED"]);

    assert_eq!(
        error_code(&run(&["flock", "remove", "work"])),
        "flock_not_empty"
    );
    assert_eq!(
        error_code(&run(&["flock", "remove", "default"])),
        "flock_is_default"
    );
    assert_eq!(
        error_code(&run(&["machine", "move", "pi-1", "nope"])),
        "unknown_flock"
    );
    assert_eq!(
        error_code(&run(&["machine", "move", "nope", "work"])),
        "unknown_machine"
    );
    ok(run(&["machine", "move", "pi-3", "default"]));
    ok(run(&["flock", "remove", "work"]));
    assert_eq!(
        error_code(&run(&["flock", "remove", "work"])),
        "unknown_flock"
    );

    // A new default takes new work; the machines stay where they were.
    ok(run(&["flock", "add", "play", "--default"]));
    let list: serde_json::Value =
        serde_json::from_str(&ok(run(&["flock", "list", "--json"]))).unwrap();
    assert_eq!(list[1]["name"], "play");
    assert_eq!(list[1]["default"], true);
    assert_eq!(list[0]["machines"], serde_json::json!(["pi-1", "pi-3"]));
    ok(run(&["flock", "default", "default"]));
    assert_eq!(
        error_code(&run(&["flock", "default", "nope"])),
        "unknown_flock"
    );
    assert!(file().contains("# desk"), "{}", file());
}

/// `task run --flock` against a running head: the task waits for a machine
/// of its flock, the flock cannot be removed under it, `--machine` outside
/// it is refused, `task list --flock`
/// narrows to it, and moving a machine in hands it the task.
#[test]
fn a_task_waits_for_its_flock_end_to_end() {
    let env = start();
    ok(env.cmd(&["flock", "add", "work"]));
    let t: serde_json::Value = serde_json::from_str(&ok(
        env.cmd(&["task", "run", "hello", "--flock", "work", "--json"])
    ))
    .unwrap();
    assert_eq!(t["flock"], "work", "{t}");
    assert_eq!(t["state"], "queued", "no machine in work yet: {t}");
    assert_eq!(
        error_code(&env.cmd(&["task", "run", "x", "--flock", "work", "--machine", "fake"])),
        "flock_mismatch"
    );
    assert_eq!(
        error_code(&env.cmd(&["task", "run", "x", "--flock", "nope"])),
        "unknown_flock"
    );
    assert_eq!(
        error_code(&env.cmd(&["flock", "remove", "work"])),
        "flock_has_tasks",
        "a removed flock would strand its queued task"
    );
    let id = t["id"].as_i64().unwrap();
    let listed = |flock: &str| -> Vec<i64> {
        let out = ok(env.cmd(&["task", "list", "--all", "--flock", flock, "--json"]));
        let v: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
        v.iter().map(|t| t["id"].as_i64().unwrap()).collect()
    };
    assert_eq!(listed("work"), [id]);
    assert!(listed("default").is_empty());
    let table = ok(env.cmd(&["task", "list"]));
    let header: Vec<&str> = table.lines().next().unwrap().split_whitespace().collect();
    assert_eq!(header[..4], ["ID", "STATE", "MACHINE", "FLOCK"], "{table}");
    assert!(
        ok(env.cmd(&["task", "show", &format!("t-{id}")])).contains("flock:      work"),
        "task show names the flock"
    );

    ok(env.cmd(&["machine", "move", "fake", "work"]));
    let deadline = Instant::now() + WAIT;
    loop {
        let t: serde_json::Value = serde_json::from_str(&ok(env.cmd(&[
            "task",
            "show",
            &format!("t-{id}"),
            "--json",
        ])))
        .unwrap();
        if t["machine"] == "fake" {
            break;
        }
        assert!(Instant::now() < deadline, "never dispatched: {t}");
        std::thread::sleep(Duration::from_millis(100));
    }

    // The head removes an empty flock itself, and refuses a run in it at once.
    ok(env.cmd(&["flock", "add", "spare"]));
    assert!(
        ok(env.cmd(&["flock", "remove", "spare"])).contains("picked it up"),
        "the head did the removal"
    );
    assert_eq!(
        error_code(&env.cmd(&["task", "run", "x", "--flock", "spare"])),
        "unknown_flock"
    );
}

/// The params of every request for `method` the fake herdr has received.
fn herdr_calls(env: &Env, method: &str) -> Vec<serde_json::Value> {
    let text = std::fs::read_to_string(&env.herdr_log).unwrap_or_default();
    serde_json::from_str::<Vec<serde_json::Value>>(&text)
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r["method"] == method)
        .map(|r| r["params"].clone())
        .collect()
}

#[test]
fn task_send_types_into_a_live_task_and_refuses_a_finished_one() {
    let env = start();
    let out = env.cmd(&["task", "run", "hi", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let task: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let pane = task["pane_id"].clone();

    let out = env.cmd(&["task", "send", "t-1", "go on"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = env.cmd(&["task", "send", "t-1", "--key", "esc", "--key", "Down"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = env.cmd(&["task", "send", "t-1", "draft", "--no-enter"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        herdr_calls(&env, "pane.send_text"),
        vec![
            serde_json::json!({"pane_id": pane, "text": "go on"}),
            serde_json::json!({"pane_id": pane, "text": "draft"}),
        ]
    );
    assert_eq!(
        herdr_calls(&env, "pane.send_keys"),
        vec![
            serde_json::json!({"pane_id": pane, "keys": ["Enter"]}),
            serde_json::json!({"pane_id": pane, "keys": ["esc", "Down"]}),
        ]
    );
    // The events log has the input, never the text.
    let log = std::fs::read_to_string(env.state.join("events.jsonl")).unwrap();
    assert!(log.contains("task.input"), "{log}");
    assert!(!log.contains("go on") && !log.contains("draft"), "{log}");

    // Usage: nothing to send, or --no-enter without text.
    let out = env.cmd(&["task", "send", "t-1"]);
    assert_eq!(out.status.code(), Some(2));
    let out = env.cmd(&["task", "send", "t-1", "--key", "Enter", "--no-enter"]);
    assert_eq!(out.status.code(), Some(2));

    env.wait_done("t-1");
    let out = env.cmd(&["task", "send", "t-1", "more"]);
    assert_eq!(out.status.code(), Some(1));
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["code"], "task_not_live", "{err}");
}

/// Polls `task show` until `task` is in `state`.
fn wait_state(env: &Env, task: &str, state: &str) -> serde_json::Value {
    let deadline = Instant::now() + WAIT;
    loop {
        let out = env.cmd(&["task", "show", task, "--json"]);
        let t: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        if t["state"] == state {
            return t;
        }
        assert!(Instant::now() < deadline, "task never became {state}: {t}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn task_send_trust_answers_the_prompt_and_trust_list_and_remove_show_it() {
    let env = start_with(&[], &[("FAKE_HERDR_TRUST_PROMPT", "Down,Enter")]);
    let out = env.cmd(&["task", "run", "hi", "--repo", "/tmp/app", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    wait_state(&env, "t-1", "blocked");

    let out = env.cmd(&["task", "send", "t-1", "--trust", "--key", "Enter"]);
    assert_eq!(out.status.code(), Some(2), "--trust takes no other input");
    let out = env.cmd(&["task", "send", "t-1", "--trust"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("trusted"));
    // Answered, the agent gets the prompt it was started for.
    wait_state(&env, "t-1", "running");
    assert_eq!(
        herdr_calls(&env, "pane.send_keys")[0]["keys"],
        serde_json::json!(["Down", "Enter"])
    );

    let out = env.cmd(&["trust", "list", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let list: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1, "{list}");
    assert_eq!(list[0]["machine"], "fake");
    assert_eq!(list[0]["repo"], "/tmp/app");
    let out = env.cmd(&["trust", "list"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("/tmp/app"));

    // The next task of that repo on that machine is answered by the head.
    let out = env.cmd(&["task", "run", "again", "--repo", "/tmp/app", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    wait_state(&env, "t-2", "running");
    assert_eq!(herdr_calls(&env, "pane.send_keys").len(), 2);
    let log = std::fs::read_to_string(env.state.join("events.jsonl")).unwrap();
    assert!(log.contains("task.trusted"), "{log}");

    let out = env.cmd(&["trust", "remove", "fake", "/tmp/app"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = env.cmd(&["trust", "remove", "fake", "/tmp/app"]);
    assert_eq!(out.status.code(), Some(1));
    let err: serde_json::Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["code"], "not_trusted", "{err}");
    let out = env.cmd(&["trust", "list", "--json"]);
    let list: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(list, serde_json::json!([]));
}

#[test]
fn run_reads_the_prompt_from_a_file_or_stdin() {
    let env = start();
    // Quotes, a backtick, a dollar sign and a blank line: what one shell
    // argument cannot carry. The trailing newline is the editor's, not the
    // prompt's.
    let prompt = "say \"hi\" and 'bye'\n\n`date` costs $5";
    let file = env._tmp.path().join("prompt.md");
    std::fs::write(&file, format!("{prompt}\n")).unwrap();
    let out = env.cmd(&[
        "task",
        "run",
        "--prompt-file",
        file.to_str().unwrap(),
        "--json",
    ]);
    let t: serde_json::Value = serde_json::from_str(&ok(out)).unwrap();
    assert_eq!(t["prompt"], prompt, "{t}");

    let out = env.cmd_stdin(
        &["task", "run", "--prompt-file", "-", "--json"],
        "from stdin\r\n\n",
    );
    let t: serde_json::Value = serde_json::from_str(&ok(out)).unwrap();
    assert_eq!(t["prompt"], "from stdin", "{t}");
}

#[test]
fn run_prompt_file_errors_carry_stable_codes() {
    let env = start();
    let dir = env._tmp.path();
    let empty = dir.join("empty.md");
    std::fs::write(&empty, "").unwrap();
    let blank = dir.join("blank.md");
    std::fs::write(&blank, "\n \n").unwrap();
    let binary = dir.join("binary.md");
    std::fs::write(&binary, [0xff, 0xfe, 0x00]).unwrap();
    for (path, code) in [
        (empty.to_str().unwrap(), "prompt_file_empty"),
        (blank.to_str().unwrap(), "prompt_file_empty"),
        (binary.to_str().unwrap(), "prompt_file_unreadable"),
        (
            dir.join("missing.md").to_str().unwrap(),
            "prompt_file_unreadable",
        ),
        // A directory opens fine and fails on read.
        (dir.to_str().unwrap(), "prompt_file_unreadable"),
    ] {
        let out = env.cmd(&["task", "run", "--prompt-file", path]);
        assert_eq!(error_code(&out), code, "{path}");
    }
    let out = env.cmd_stdin(&["task", "run", "--prompt-file", "-"], "\n");
    assert_eq!(error_code(&out), "prompt_file_empty");

    // Nothing was dispatched.
    let listed = ok(env.cmd(&["task", "list", "--all", "--json"]));
    assert_eq!(listed.trim(), "[]", "{listed}");
}

#[test]
fn run_takes_exactly_one_of_prompt_and_prompt_file() {
    let env = start();
    let file = env._tmp.path().join("p.md");
    std::fs::write(&file, "hi\n").unwrap();
    // clap usage errors: plain text, exit 2.
    for args in [
        &["task", "run", "hi", "--prompt-file", file.to_str().unwrap()][..],
        &["task", "run"][..],
    ] {
        let out = env.cmd(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(serde_json::from_slice::<serde_json::Value>(&out.stderr).is_err());
    }
}
