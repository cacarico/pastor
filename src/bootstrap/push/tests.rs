use std::sync::Mutex;

use sha2::{Digest, Sha256};

use super::*;
use crate::bootstrap::StepResult;
use crate::config::PastorConfig;
use crate::config::flock::Flock;
use crate::daemon::Daemon;
use crate::herdr::fake::FakeHerdr;

/// One recorded call: the argv joined with spaces, and whether a file was
/// on stdin.
type Call = (String, bool);

/// A head-side runner that answers like a reachable Debian x86_64 box,
/// with `bootstrap --json` printing `outcome`. `ssh-keygen -t` makes the
/// key files it is asked for; the copy echoes the sum of what it was fed.
struct FakePush {
    calls: Mutex<Vec<Call>>,
    outcome: Mutex<Outcome>,
    host_key: String,
    fingerprint: String,
    /// Run when the bootstrap call comes, to look at the head mid-push.
    on_bootstrap: Mutex<Option<Box<dyn FnMut() + Send>>>,
}

impl FakePush {
    fn new(outcome: Outcome) -> Arc<FakePush> {
        Arc::new(FakePush {
            calls: Mutex::default(),
            outcome: Mutex::new(outcome),
            host_key: "ssh-ed25519 AAAAHOSTKEY".into(),
            fingerprint: "SHA256:goodfp".into(),
            on_bootstrap: Mutex::default(),
        })
    }

    fn calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(c, _)| c.clone())
            .collect()
    }

    fn bootstrap_calls(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|c| c.contains(" bootstrap --json"))
            .collect()
    }
}

fn answer(success: bool, stdout: impl Into<String>) -> CmdOutput {
    CmdOutput {
        success,
        stdout: stdout.into(),
        stderr: String::new(),
    }
}

impl PushRunner for FakePush {
    fn run(
        &self,
        program: &str,
        args: &[String],
        stdin: Option<&Path>,
    ) -> std::io::Result<CmdOutput> {
        let call = std::iter::once(program.to_string())
            .chain(args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ");
        self.calls
            .lock()
            .unwrap()
            .push((call.clone(), stdin.is_some()));
        let remote = args.last().map(String::as_str).unwrap_or_default();
        Ok(match program {
            "ssh-keygen" if args.iter().any(|a| a == "-t") => {
                let f = args.last().unwrap();
                std::fs::write(f, "PRIVATE\n").unwrap();
                std::fs::write(format!("{f}.pub"), "ssh-ed25519 AAAAHEAD pastor-head\n").unwrap();
                answer(true, "")
            }
            "ssh-keygen" => answer(
                true,
                format!("256 {} gpu-3.lan (ED25519)\n", self.fingerprint),
            ),
            "ssh-keyscan" => answer(true, format!("gpu-3.lan {}\n", self.host_key)),
            "ssh" if args.first().map(String::as_str) == Some("-G") => {
                answer(true, "user me\nhostname gpu-3.lan\nport 22\n")
            }
            "ssh" if args.iter().any(|a| a == "-O") => answer(true, ""),
            "ssh" if remote.contains("uname -sm") => answer(true, "Linux x86_64\n64\n"),
            "ssh" if remote.contains("cat >") => {
                let bytes = std::fs::read(stdin.expect("the copy reads a file")).unwrap();
                answer(true, format!("{}\n", hex(&bytes)))
            }
            "ssh" if remote.contains(" bootstrap --json") => {
                if let Some(f) = self.on_bootstrap.lock().unwrap().as_mut() {
                    f();
                }
                let o = self.outcome.lock().unwrap().clone();
                CmdOutput {
                    success: o.ok,
                    stdout: format!("{}\n", serde_json::to_string(&o).unwrap()),
                    stderr: String::new(),
                }
            }
            _ => answer(true, ""),
        })
    }
}

/// Never reached: the tests seed the cache.
struct NoFetch;

impl Fetch for NoFetch {
    fn get(&self, url: &str, _to: &Path) -> anyhow::Result<()> {
        panic!("no download in tests: {url}")
    }
}

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Put a binary in the head's cache as `ensure` would leave it.
fn seed(paths: &Paths, name: &str, version: &str, target: &str) {
    let a = Artifact::new(name, version, target).unwrap();
    let dest = crate::artifacts::cache_path(paths, &a);
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    let body = format!("{name} binary");
    std::fs::write(&dest, &body).unwrap();
    std::fs::write(
        dest.with_file_name(format!("{name}.sha256")),
        format!("{}\n", hex(body.as_bytes())),
    )
    .unwrap();
}

fn ok_outcome(mode: Mode) -> Outcome {
    let mut done = StepResult::done();
    done.step = "keys".into();
    Outcome {
        ok: true,
        mode,
        isolation: match mode {
            Mode::Root => Isolation::Users,
            Mode::User => Isolation::None,
        },
        steps: vec![done],
        machine_key: Some("ssh-ed25519 AAAAMACHINE pastor@gpu-3".into()),
        agent_key: Some("ssh-ed25519 AAAAAGENT pastor-agent@gpu-3".into()),
        host_key: Some("ssh-ed25519 AAAAHOSTKEY".into()),
        herdr_version: Some("0.9.5".into()),
        pastor_version: Some(env!("CARGO_PKG_VERSION").into()),
        service_user: Some("me".into()),
    }
}

/// User mode, stopped at packages for want of git.
fn failed_at_packages() -> Outcome {
    let mut failed = StepResult::failed("git is missing", "rerun with --sudo or install it")
        .with_missing(vec!["git".into()]);
    failed.step = "packages".into();
    Outcome {
        ok: false,
        steps: vec![failed],
        machine_key: None,
        agent_key: None,
        ..ok_outcome(Mode::User)
    }
}

struct Head {
    d: Daemon,
    paths: Paths,
    _tmp: tempfile::TempDir,
}

async fn head() -> Head {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
    let flock = Flock::default();
    // A fresh head's flock.toml: nothing in it yet.
    std::fs::create_dir_all(&paths.config_dir).unwrap();
    std::fs::write(paths.flock_file(), "").unwrap();
    let config = PastorConfig {
        head_address: Some("me@head".into()),
        ..PastorConfig::default()
    };
    std::fs::write(paths.config_file(), toml::to_string(&config).unwrap()).unwrap();
    let on_disk = crate::scheduler::ConfigFingerprint::sample(&paths);
    let factory: crate::daemon::ConnectorFactory = Arc::new(|_: &MachineConfig| {
        Arc::new(FakeHerdr::new()) as Arc<dyn crate::herdr::Connector>
    });
    let d = Daemon::start(paths.clone(), config, flock, on_disk, Some(factory))
        .await
        .unwrap();
    seed(
        &paths,
        "pastor",
        env!("CARGO_PKG_VERSION"),
        "x86_64-unknown-linux-musl",
    );
    seed(
        &paths,
        "herdr",
        crate::artifacts::DEFAULT_HERDR_VERSION,
        "linux-x86_64",
    );
    Head {
        d,
        paths,
        _tmp: tmp,
    }
}

fn entry(name: &str, ssh: &str) -> MachineConfig {
    MachineConfig {
        name: name.into(),
        local: false,
        pull: false,
        ssh: Some(ssh.into()),
        command: None,
        session: "default".into(),
        max_agents: 2,
        job_slots: 1,
        burst: 1,
        tags: vec![],
        flock: None,
        agent: None,
        agent_args: None,
        model: None,
        fallback: None,
        priority: None,
        agents: Default::default(),
        profile: None,
        description: None,
    }
}

impl Head {
    fn opts(&self, runner: &Arc<FakePush>, sudo: bool) -> PushOpts {
        PushOpts {
            sudo,
            service_user: None,
            host_key: None,
            machine: Some(entry("gpu-3", "me@gpu-3")),
            head: "me@head".into(),
            provision: ProvisionConfig::default(),
            fleet_versions: vec![],
            authorized_keys: self.paths.state_dir.join("authorized_keys"),
            runner: runner.clone(),
            fetch: Arc::new(NoFetch),
        }
    }

    async fn push(&self, opts: PushOpts) -> anyhow::Result<Outcome> {
        let fleet = self.d.fleet();
        push(
            &self.paths,
            &self.d.store(),
            &fleet,
            "gpu-3",
            "me@gpu-3",
            opts,
        )
        .await
    }

    fn state(&self) -> MachineState {
        self.d
            .store()
            .machine_state("gpu-3")
            .unwrap()
            .expect("a machine_state row")
    }

    fn ssh(&self) -> Option<String> {
        Flock::load(&self.paths.flock_file())
            .unwrap()
            .get("gpu-3")
            .and_then(|m| m.ssh.clone())
    }
}

fn code(err: &anyhow::Error) -> &str {
    err.downcast_ref::<CliError>()
        .map_or("", |e| e.code.as_str())
}

#[tokio::test(flavor = "multi_thread")]
async fn push_writes_pending_then_ok() {
    let h = head().await;
    let runner = FakePush::new(ok_outcome(Mode::User));
    let seen = Arc::new(Mutex::new(None));
    {
        let (store, paths, seen) = (h.d.store(), h.paths.clone(), seen.clone());
        *runner.on_bootstrap.lock().unwrap() = Some(Box::new(move || {
            let state = store.machine_state("gpu-3").unwrap().map(|s| s.bootstrap);
            let listed = Flock::load(&paths.flock_file())
                .unwrap()
                .get("gpu-3")
                .is_some();
            *seen.lock().unwrap() = Some((state, listed));
        }));
    }
    let mut events = h.d.subscribe();
    let out = h.push(h.opts(&runner, false)).await.unwrap();
    assert!(out.ok, "{out:?}");
    assert_eq!(
        *seen.lock().unwrap(),
        Some((Some(BootstrapState::Pending), true)),
        "pending, and in the flock, while bootstrap ran"
    );
    assert_eq!(h.state().bootstrap, BootstrapState::Ok);
    let ev = events.try_recv().expect("an event");
    assert_eq!(ev.kind, "machine.bootstrapped");
    // The binaries went over ssh's stdin, never through the daemon.
    let copies: Vec<Call> = runner
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(c, _)| c.contains("cat >"))
        .cloned()
        .collect();
    assert_eq!(copies.len(), 2, "{copies:?}");
    assert!(copies.iter().all(|(_, stdin)| *stdin));
    // The head key was made on first use, private.
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(h.paths.head_key())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    // The old master is closed at the end.
    assert!(
        runner
            .calls()
            .last()
            .unwrap()
            .contains("-O exit -- me@gpu-3")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rerun_after_failure_resumes() {
    let h = head().await;
    let runner = FakePush::new(failed_at_packages());
    let mut events = h.d.subscribe();
    let out = h.push(h.opts(&runner, false)).await.unwrap();
    assert!(!out.ok);
    let state = h.state();
    assert_eq!(state.bootstrap, BootstrapState::Failed);
    assert_eq!(state.failed_step.as_deref(), Some("packages"));
    let ev = events.try_recv().unwrap();
    assert_eq!(ev.kind, "machine.bootstrap_failed");
    assert_eq!(ev.detail.unwrap()["step"], "packages");

    *runner.outcome.lock().unwrap() = ok_outcome(Mode::Root);
    let out = h
        .push(h.opts(&runner, true))
        .await
        .expect("no machine_exists");
    assert!(out.ok, "{out:?}");
    assert_eq!(h.state().bootstrap, BootstrapState::Ok);
    let f = Flock::load(&h.paths.flock_file()).unwrap();
    assert_eq!(f.machines.iter().filter(|m| m.name == "gpu-3").count(), 1);
    let runs = runner.bootstrap_calls();
    assert_eq!(runs.len(), 2);
    assert!(
        !runs[0].contains("--sudo") && runs[1].contains("--sudo"),
        "{runs:?}"
    );
    // One agent line, however many runs.
    let keys = std::fs::read_to_string(h.paths.state_dir.join("authorized_keys")).unwrap();
    assert_eq!(keys.matches("pastor:agent:gpu-3").count(), 1, "{keys}");
}

#[tokio::test(flavor = "multi_thread")]
async fn host_key_mismatch_stops() {
    let h = head().await;
    let runner = FakePush::new(ok_outcome(Mode::User));
    let mut opts = h.opts(&runner, false);
    opts.host_key = Some("SHA256:otherfp".into());
    let out = h.push(opts).await.unwrap();
    assert!(!out.ok);
    assert_eq!(out.steps.last().unwrap().step, "host_key");
    assert!(runner.bootstrap_calls().is_empty(), "{:?}", runner.calls());
    assert!(!runner.calls().iter().any(|c| c.contains("cat >")));
    let state = h.state();
    assert_eq!(state.bootstrap, BootstrapState::Failed);
    assert_eq!(state.failed_step.as_deref(), Some("host_key"));
    let pinned = std::fs::read_to_string(h.paths.known_hosts_file()).unwrap_or_default();
    assert!(pinned.is_empty(), "{pinned}");
}

#[tokio::test(flavor = "multi_thread")]
async fn host_key_that_matches_is_pinned() {
    let h = head().await;
    let runner = FakePush::new(ok_outcome(Mode::User));
    let mut opts = h.opts(&runner, false);
    opts.host_key = Some("SHA256:goodfp".into());
    assert!(h.push(opts).await.unwrap().ok);
    let pinned = std::fs::read_to_string(h.paths.known_hosts_file()).unwrap();
    assert_eq!(pinned, "gpu-3.lan ssh-ed25519 AAAAHOSTKEY\n");
    // Every ssh to the machine checks against that file.
    for c in runner.calls().iter().filter(|c| c.starts_with("ssh -")) {
        if c.contains("-G") || c.contains("-O exit") {
            continue;
        }
        assert!(
            c.contains(&format!(
                "UserKnownHostsFile={}",
                h.paths.known_hosts_file().display()
            )),
            "{c}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn root_mode_rewrites_ssh_to_agent_user() {
    let h = head().await;
    let runner = FakePush::new(ok_outcome(Mode::Root));
    assert!(h.push(h.opts(&runner, true)).await.unwrap().ok);
    assert_eq!(h.ssh().as_deref(), Some("pastor-agent@gpu-3"));
}

#[tokio::test(flavor = "multi_thread")]
async fn user_mode_keeps_login() {
    let h = head().await;
    let runner = FakePush::new(ok_outcome(Mode::User));
    assert!(h.push(h.opts(&runner, false)).await.unwrap().ok);
    assert_eq!(h.ssh().as_deref(), Some("me@gpu-3"));
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_line_installed() {
    let h = head().await;
    let keys = h.paths.state_dir.join("authorized_keys");
    std::fs::write(&keys, "ssh-ed25519 AAAAMINE me@laptop\n").unwrap();
    let runner = FakePush::new(ok_outcome(Mode::Root));
    assert!(h.push(h.opts(&runner, true)).await.unwrap().ok);
    let text = std::fs::read_to_string(&keys).unwrap();
    assert!(
        text.starts_with("ssh-ed25519 AAAAMINE me@laptop\n"),
        "{text}"
    );
    let line = text
        .lines()
        .find(|l| l.ends_with("pastor:agent:gpu-3"))
        .unwrap_or_else(|| panic!("no agent line in {text}"));
    assert!(line.contains("ssh-ed25519 AAAAAGENT"), "{line}");
    assert!(line.contains("bridge --agent --machine gpu-3"), "{line}");
}

#[tokio::test(flavor = "multi_thread")]
async fn add_of_a_machine_that_exists_is_refused() {
    let h = head().await;
    let runner = FakePush::new(ok_outcome(Mode::User));
    assert!(h.push(h.opts(&runner, false)).await.unwrap().ok);
    // Ok already: a second add is machine_exists, and runs nothing.
    let before = runner.calls().len();
    let err = h.push(h.opts(&runner, false)).await.unwrap_err();
    assert_eq!(code(&err), "machine_exists", "{err:#}");
    assert_eq!(runner.calls().len(), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn add_with_another_login_is_refused() {
    let h = head().await;
    let runner = FakePush::new(failed_at_packages());
    assert!(!h.push(h.opts(&runner, false)).await.unwrap().ok);
    let mut opts = h.opts(&runner, true);
    opts.machine = Some(entry("gpu-3", "root@gpu-3"));
    let fleet = h.d.fleet();
    let err = push(&h.paths, &h.d.store(), &fleet, "gpu-3", "root@gpu-3", opts)
        .await
        .unwrap_err();
    assert_eq!(code(&err), "machine_exists", "{err:#}");
    assert_eq!(h.state().bootstrap, BootstrapState::Failed);
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_values_are_quoted() {
    let h = head().await;
    let runner = FakePush::new(ok_outcome(Mode::Root));
    let mut opts = h.opts(&runner, true);
    opts.service_user = Some("pastor".into());
    opts.head = "me@head; rm -rf ~".into();
    assert!(h.push(opts).await.unwrap().ok);
    let run = &runner.bootstrap_calls()[0];
    // The remote line is itself quoted once more for `sh -c`, so each
    // quoted value shows with its quotes escaped.
    assert!(run.contains(r#"--head '\''me@head; rm -rf ~'\''"#), "{run}");
    assert!(
        run.contains(r#"--head-key '\''ssh-ed25519 AAAAHEAD pastor-head'\''"#),
        "{run}"
    );
    assert!(run.contains("--service-user pastor"), "{run}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bad_service_user_is_refused_before_anything_runs() {
    let h = head().await;
    let runner = FakePush::new(ok_outcome(Mode::Root));
    let mut opts = h.opts(&runner, true);
    opts.service_user = Some("x; reboot".into());
    let err = h.push(opts).await.unwrap_err();
    assert_eq!(code(&err), "invalid_user", "{err:#}");
    assert!(runner.calls().is_empty());
    assert!(h.d.store().machine_state("gpu-3").unwrap().is_none());
}

#[test]
fn login_for_a_root_mode_entry_needs_ssh() {
    let e = entry("gpu-3", "pastor-agent@gpu-3");
    let err = login_for(&e, None).unwrap_err();
    assert_eq!(code(&err), "usage_error");
    assert_eq!(login_for(&e, Some("me@gpu-3")).unwrap(), "me@gpu-3");
    assert_eq!(
        login_for(&entry("gpu-3", "me@gpu-3"), None).unwrap(),
        "me@gpu-3"
    );
}

#[test]
fn copy_checks_the_sum_before_it_moves() {
    let cmd = copy_command("pastor", "abc");
    let check = cmd.find("|| exit 3").unwrap();
    let mv = cmd.find("mv -f").unwrap();
    assert!(check < mv, "{cmd}");
}
