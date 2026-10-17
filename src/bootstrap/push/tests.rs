use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

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
    /// What `ssh-keyscan` finds; empty, it finds no ed25519 key.
    host_key: Mutex<String>,
    fingerprint: String,
    /// ssh cannot reach the machine (exit 255).
    unreachable: AtomicBool,
    /// The copy arrives with another sum.
    corrupt: AtomicBool,
    /// Run when the bootstrap call comes, to look at the head mid-push.
    on_bootstrap: Mutex<Option<Box<dyn FnMut() + Send>>>,
    /// What `ssh -G` prints; unset, `<host>.lan` on port 22 for the
    /// login's host.
    resolved: Mutex<Option<String>>,
}

impl FakePush {
    fn new(outcome: Outcome) -> Arc<FakePush> {
        Arc::new(FakePush {
            calls: Mutex::default(),
            outcome: Mutex::new(outcome),
            host_key: Mutex::new("ssh-ed25519 AAAAHOSTKEY".into()),
            fingerprint: "SHA256:goodfp".into(),
            unreachable: AtomicBool::new(false),
            corrupt: AtomicBool::new(false),
            on_bootstrap: Mutex::default(),
            resolved: Mutex::default(),
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
            "ssh-keyscan" => {
                let key = self.host_key.lock().unwrap().clone();
                if key.is_empty() {
                    answer(true, "")
                } else {
                    answer(true, format!("gpu-3.lan {key}\n"))
                }
            }
            "ssh" if args.first().map(String::as_str) == Some("-G") => {
                match self.resolved.lock().unwrap().clone() {
                    Some(out) => answer(true, out),
                    None => {
                        let host = remote.rsplit('@').next().unwrap_or(remote);
                        answer(true, format!("user me\nhostname {host}.lan\nport 22\n"))
                    }
                }
            }
            "ssh" if args.iter().any(|a| a == "-O") => answer(true, ""),
            "ssh" if self.unreachable.load(Ordering::SeqCst) => CmdOutput {
                success: false,
                stdout: String::new(),
                stderr: "ssh: connect to host gpu-3.lan port 22: Connection refused\n".into(),
            },
            "ssh" if remote.contains("cat >") && self.corrupt.load(Ordering::SeqCst) => {
                answer(false, "0badc0de\n")
            }
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
            repin: false,
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

/// The ssh calls to the machine itself, not `-G` or the master's `-O exit`.
fn machine_calls(runner: &FakePush) -> Vec<String> {
    runner
        .calls()
        .into_iter()
        .filter(|c| c.starts_with("ssh -") && !c.contains("ssh -G") && !c.contains("-O exit"))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn push_ssh_checks_the_pinned_ed25519_key_strictly_and_times_out() {
    let h = head().await;
    let runner = FakePush::new(ok_outcome(Mode::User));
    assert!(h.push(h.opts(&runner, false)).await.unwrap().ok);
    let calls = machine_calls(&runner);
    assert!(!calls.is_empty());
    for c in calls {
        // After the pin nothing new is accepted, and only the ed25519 key
        // that was pinned is asked for: a man in the middle offering an
        // RSA or ECDSA key is not "a new host".
        assert!(c.contains("-o StrictHostKeyChecking=yes"), "{c}");
        assert!(!c.contains("accept-new"), "{c}");
        assert!(c.contains("-o HostKeyAlgorithms=ssh-ed25519"), "{c}");
        for o in [
            "ConnectTimeout=20",
            "ServerAliveInterval=15",
            "ServerAliveCountMax=4",
        ] {
            assert!(c.contains(&format!("-o {o}")), "{o} in {c}");
        }
        assert!(!c.contains("ServerAliveCountMax=3"), "{c}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn no_ed25519_host_key_stops_with_a_clear_message() {
    let h = head().await;
    let runner = FakePush::new(ok_outcome(Mode::User));
    runner.host_key.lock().unwrap().clear();
    let out = h.push(h.opts(&runner, false)).await.unwrap();
    assert!(!out.ok);
    let step = out.steps.last().unwrap();
    assert_eq!(step.step, "host_key");
    let why = step.why.as_deref().unwrap();
    assert!(why.contains("no ed25519 host key"), "{why}");
    assert!(machine_calls(&runner).is_empty(), "{:?}", runner.calls());
}

#[tokio::test(flavor = "multi_thread")]
async fn unreachable_machine_is_a_connect_failure_not_a_platform() {
    let h = head().await;
    let runner = FakePush::new(ok_outcome(Mode::User));
    runner.unreachable.store(true, Ordering::SeqCst);
    let out = h.push(h.opts(&runner, false)).await.unwrap();
    assert!(!out.ok);
    let step = out.steps.last().unwrap();
    assert_eq!(step.step, "connect", "{out:?}");
    assert!(step.why.as_deref().unwrap().contains("Connection refused"));
    let err = crate::bootstrap::error(&out).expect("an error");
    assert_eq!(code(&err), "runtime_error", "{err:#}");
    assert_eq!(h.state().failed_step.as_deref(), Some("connect"));
}

#[tokio::test(flavor = "multi_thread")]
async fn corrupted_copy_runs_nothing() {
    let h = head().await;
    let runner = FakePush::new(ok_outcome(Mode::User));
    runner.corrupt.store(true, Ordering::SeqCst);
    let out = h.push(h.opts(&runner, false)).await.unwrap();
    assert!(!out.ok);
    let err = crate::bootstrap::error(&out).expect("an error");
    assert_eq!(code(&err), "artifact_checksum_mismatch", "{err:#}");
    // One copy tried, the second never sent, bootstrap never run.
    let copies = runner
        .calls()
        .iter()
        .filter(|c| c.contains("cat >"))
        .count();
    assert_eq!(copies, 1, "{:?}", runner.calls());
    assert!(runner.bootstrap_calls().is_empty());
    assert_eq!(h.state().bootstrap, BootstrapState::Failed);
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_fingerprints_do_not_share_a_temp_file() {
    let h = head().await;
    let fake = FakePush::new(ok_outcome(Mode::User));
    let runner: Arc<dyn PushRunner> = fake.clone();
    let (a, b) = tokio::join!(
        fingerprint(&h.paths, &runner, "a.lan", "ssh-ed25519 AAAA"),
        fingerprint(&h.paths, &runner, "b.lan", "ssh-ed25519 BBBB"),
    );
    assert!(a.is_ok() && b.is_ok());
    // The files `ssh-keygen -l -f` was pointed at.
    let files: Vec<String> = fake
        .calls()
        .into_iter()
        .filter(|c| c.starts_with("ssh-keygen -l"))
        .map(|c| c.rsplit(' ').next().unwrap().to_string())
        .collect();
    assert_eq!(files.len(), 2, "{files:?}");
    assert_ne!(files[0], files[1], "one temp file each");
    for f in files {
        assert!(!Path::new(&f).exists(), "{f} removed");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_key_the_machine_reports_otherwise_fails_the_push() {
    let h = head().await;
    let mut o = ok_outcome(Mode::User);
    o.host_key = Some("ssh-ed25519 AAAAOTHER".into());
    let runner = FakePush::new(o);
    let out = h.push(h.opts(&runner, false)).await.unwrap();
    assert!(!out.ok, "{out:?}");
    assert_eq!(out.steps.last().unwrap().step, "host_key");
    let state = h.state();
    assert_eq!(state.bootstrap, BootstrapState::Failed);
    assert_ne!(state.host_key.as_deref(), Some("ssh-ed25519 AAAAOTHER"));
    let keys =
        std::fs::read_to_string(h.paths.state_dir.join("authorized_keys")).unwrap_or_default();
    assert!(!keys.contains("pastor:agent:gpu-3"), "{keys}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_pinned_key_is_what_is_stored() {
    let h = head().await;
    let mut o = ok_outcome(Mode::User);
    o.host_key = Some("ssh-ed25519 AAAAHOSTKEY root@gpu-3".into());
    let runner = FakePush::new(o);
    assert!(h.push(h.opts(&runner, false)).await.unwrap().ok);
    assert_eq!(
        h.state().host_key.as_deref(),
        Some("ssh-ed25519 AAAAHOSTKEY")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ok_with_a_failed_step_is_not_success() {
    let h = head().await;
    let mut o = ok_outcome(Mode::User);
    let mut failed = StepResult::failed("no", "fix it");
    failed.step = "services".into();
    o.steps.push(failed);
    let runner = FakePush::new(o);
    let out = h.push(h.opts(&runner, false)).await.unwrap();
    assert!(!out.ok, "{out:?}");
    assert_eq!(h.state().bootstrap, BootstrapState::Failed);
    let keys =
        std::fs::read_to_string(h.paths.state_dir.join("authorized_keys")).unwrap_or_default();
    assert!(!keys.contains("pastor:agent:gpu-3"), "{keys}");
}

#[test]
fn not_ok_with_no_failed_step_is_an_error() {
    let o = Outcome {
        ok: false,
        ..ok_outcome(Mode::User)
    };
    let err = crate::bootstrap::error(&o).expect("not ok is an error");
    assert_eq!(code(&err), "runtime_error");
}

/// Take `gpu-3` out of the flock mid-push, as a `machine remove` would.
fn remove_on_bootstrap(h: &Head, runner: &FakePush) {
    let (fleet, file) = (h.d.fleet(), h.paths.flock_file());
    let rt = tokio::runtime::Handle::current();
    *runner.on_bootstrap.lock().unwrap() = Some(Box::new(move || {
        rt.block_on(
            fleet.edit_flock_file(&file, |f| crate::fleet_edit::remove_machine(f, "gpu-3")),
        )
        .unwrap();
    }));
}

#[tokio::test(flavor = "multi_thread")]
async fn root_mode_writes_the_agent_line_only_after_the_entry_moves() {
    let h = head().await;
    let runner = FakePush::new(ok_outcome(Mode::Root));
    remove_on_bootstrap(&h, &runner);
    let _ = h.push(h.opts(&runner, true)).await;
    let keys =
        std::fs::read_to_string(h.paths.state_dir.join("authorized_keys")).unwrap_or_default();
    assert!(!keys.contains("pastor:agent:gpu-3"), "{keys}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_report_takes_the_agent_line_back() {
    let h = head().await;
    let runner = FakePush::new(ok_outcome(Mode::User));
    remove_on_bootstrap(&h, &runner);
    let res = h.push(h.opts(&runner, false)).await;
    assert!(res.is_err(), "{res:?}");
    let keys =
        std::fs::read_to_string(h.paths.state_dir.join("authorized_keys")).unwrap_or_default();
    assert!(!keys.contains("pastor:agent:gpu-3"), "{keys}");
}

/// `machine bootstrap` of a machine already Ok.
fn rerun_opts(h: &Head, runner: &Arc<FakePush>) -> PushOpts {
    PushOpts {
        machine: None,
        ..h.opts(runner, false)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ok_machine_rerun_stays_ok_until_its_own_steps_start() {
    let h = head().await;
    let runner = FakePush::new(ok_outcome(Mode::User));
    assert!(h.push(h.opts(&runner, false)).await.unwrap().ok);
    let seen = Arc::new(Mutex::new(None));
    {
        let (store, seen) = (h.d.store(), seen.clone());
        *runner.on_bootstrap.lock().unwrap() = Some(Box::new(move || {
            *seen.lock().unwrap() = store.machine_state("gpu-3").unwrap().map(|s| s.bootstrap);
        }));
    }
    let out = h.push(rerun_opts(&h, &runner)).await.unwrap();
    assert!(out.ok, "{out:?}");
    assert_eq!(
        *seen.lock().unwrap(),
        Some(BootstrapState::Pending),
        "pending once the machine's own steps run"
    );
    assert_eq!(h.state().bootstrap, BootstrapState::Ok);
    assert_eq!(runner.bootstrap_calls().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn ok_machine_rerun_that_fails_before_its_steps_stays_ok() {
    let h = head().await;
    let runner = FakePush::new(ok_outcome(Mode::User));
    assert!(h.push(h.opts(&runner, false)).await.unwrap().ok);
    let mut events = h.d.subscribe();
    runner.unreachable.store(true, Ordering::SeqCst);
    let out = h.push(rerun_opts(&h, &runner)).await.unwrap();
    assert!(!out.ok);
    let state = h.state();
    assert_eq!(state.bootstrap, BootstrapState::Ok, "{state:?}");
    assert_eq!(state.host_key.as_deref(), Some("ssh-ed25519 AAAAHOSTKEY"));
    let ev = events.try_recv().expect("an event");
    assert_eq!(ev.kind, "machine.bootstrap_failed");
    assert_eq!(ev.detail.unwrap()["step"], "connect");
}

#[tokio::test(flavor = "multi_thread")]
async fn ok_machine_rerun_that_fails_in_its_steps_is_failed() {
    let h = head().await;
    let runner = FakePush::new(ok_outcome(Mode::User));
    assert!(h.push(h.opts(&runner, false)).await.unwrap().ok);
    *runner.outcome.lock().unwrap() = failed_at_packages();
    let out = h.push(rerun_opts(&h, &runner)).await.unwrap();
    assert!(!out.ok);
    let state = h.state();
    assert_eq!(state.bootstrap, BootstrapState::Failed);
    assert_eq!(state.failed_step.as_deref(), Some("packages"));
}

/// `ssh -G` of `login` under the ssh config `config`, from the real ssh;
/// `None` where there is no ssh to ask.
fn real_ssh_g(config: &str, login: &str) -> Option<String> {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config");
    std::fs::write(&file, config).unwrap();
    let out = std::process::Command::new("ssh")
        .arg("-F")
        .arg(&file)
        .args(["-G", "--", login])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The key is pinned under the very name ssh looks it up by, as `ssh -G`
/// resolves the login through the user's ssh config: the host off an
/// alias, `[host]:port` off port 22, a `HostKeyAlias` as it is. So the
/// pushes' `StrictHostKeyChecking=yes` finds it.
#[tokio::test(flavor = "multi_thread")]
async fn the_pinned_name_is_what_ssh_resolves_the_login_to() {
    let config = "Host gpu-3\n  HostName 10.1.2.3\n  Port 2222\n\
                  Host gpu-3-aliased\n  HostName 10.1.2.4\n  HostKeyAlias gpu-3-key\n\
                  Host gpu-3-plain\n  HostName GPU-3.Example\n";
    for (login, want, scanned) in [
        (
            "me@gpu-3",
            "[10.1.2.3]:2222",
            "-p 2222 -t ed25519 -- 10.1.2.3",
        ),
        (
            "me@gpu-3-aliased",
            "gpu-3-key",
            "-p 22 -t ed25519 -- 10.1.2.4",
        ),
        (
            "me@gpu-3-plain",
            "gpu-3.example",
            "-p 22 -t ed25519 -- gpu-3.example",
        ),
    ] {
        let Some(resolved) = real_ssh_g(config, login) else {
            eprintln!("no ssh here; skipped");
            return;
        };
        let h = head().await;
        let runner = FakePush::new(ok_outcome(Mode::User));
        *runner.resolved.lock().unwrap() = Some(resolved);
        let fleet = h.d.fleet();
        let mut opts = h.opts(&runner, false);
        opts.machine = Some(entry("gpu-3", login));
        let out = push(&h.paths, &h.d.store(), &fleet, "gpu-3", login, opts)
            .await
            .unwrap();
        assert!(out.ok, "{out:?}");
        let pinned = std::fs::read_to_string(h.paths.known_hosts_file()).unwrap();
        assert_eq!(
            pinned,
            format!("{want} ssh-ed25519 AAAAHOSTKEY\n"),
            "{login}"
        );
        assert!(
            runner.calls().contains(&format!("ssh-keyscan {scanned}")),
            "{login}: {:?}",
            runner.calls()
        );
    }
}

/// Pushes and a remove take turns on pastor's known_hosts: many machines
/// pinned at once, from as many threads, all keep their line.
#[test]
fn concurrent_pins_keep_every_line() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
    let runner = FakePush::new(ok_outcome(Mode::User));
    let barrier = std::sync::Barrier::new(16);
    std::thread::scope(|s| {
        for i in 0..16 {
            let (paths, runner, barrier) = (&paths, runner.clone(), &barrier);
            s.spawn(move || {
                let opts = PushOpts {
                    sudo: false,
                    service_user: None,
                    host_key: None,
                    repin: false,
                    machine: None,
                    head: "me@head".into(),
                    provision: ProvisionConfig::default(),
                    fleet_versions: vec![],
                    authorized_keys: paths.state_dir.join("authorized_keys"),
                    runner,
                    fetch: Arc::new(NoFetch),
                };
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                barrier.wait();
                let (name, login) = (format!("m{i}"), format!("me@m{i}"));
                rt.block_on(pin_host_key(paths, &name, &login, None, &opts))
                    .map_err(|Stop(s)| s.why)
                    .unwrap();
            });
        }
    });
    let text = std::fs::read_to_string(paths.known_hosts_file()).unwrap();
    for i in 0..16 {
        assert!(
            text.lines()
                .any(|l| l == format!("m{i}.lan ssh-ed25519 AAAAHOSTKEY")),
            "m{i}: {text}"
        );
    }
    assert_eq!(text.lines().count(), 16, "{text}");
}

/// A machine whose host key changed stops at the pin, naming `--repin`,
/// and keeps the key on record; `--repin` is the one way to trust the new
/// key, which then replaces the old one everywhere.
#[tokio::test(flavor = "multi_thread")]
async fn a_changed_host_key_needs_repin() {
    let h = head().await;
    let runner = FakePush::new(ok_outcome(Mode::User));
    assert!(h.push(h.opts(&runner, false)).await.unwrap().ok);
    *runner.host_key.lock().unwrap() = "ssh-ed25519 AAAANEWKEY".into();
    let mut o = ok_outcome(Mode::User);
    o.host_key = Some("ssh-ed25519 AAAANEWKEY".into());
    *runner.outcome.lock().unwrap() = o;

    let out = h.push(rerun_opts(&h, &runner)).await.unwrap();
    assert!(!out.ok);
    let step = out.steps.last().unwrap();
    assert_eq!(step.step, "host_key");
    let fix = step.fix.as_deref().unwrap();
    assert!(
        fix.contains("pastor machine bootstrap gpu-3 --repin"),
        "{fix}"
    );
    assert_eq!(
        h.state().host_key.as_deref(),
        Some("ssh-ed25519 AAAAHOSTKEY")
    );
    assert_eq!(h.state().bootstrap, BootstrapState::Ok);

    let out = h
        .push(PushOpts {
            repin: true,
            ..rerun_opts(&h, &runner)
        })
        .await
        .unwrap();
    assert!(out.ok, "{out:?}");
    assert_eq!(
        h.state().host_key.as_deref(),
        Some("ssh-ed25519 AAAANEWKEY")
    );
    assert_eq!(
        std::fs::read_to_string(h.paths.known_hosts_file()).unwrap(),
        "gpu-3.lan ssh-ed25519 AAAANEWKEY\n"
    );
}

/// A key on record that known_hosts no longer has (removed by hand) is
/// still the one the machine must offer.
#[tokio::test(flavor = "multi_thread")]
async fn the_key_on_record_is_checked_even_off_known_hosts() {
    let h = head().await;
    let runner = FakePush::new(ok_outcome(Mode::User));
    assert!(h.push(h.opts(&runner, false)).await.unwrap().ok);
    std::fs::remove_file(h.paths.known_hosts_file()).unwrap();
    *runner.host_key.lock().unwrap() = "ssh-ed25519 AAAANEWKEY".into();
    let out = h.push(rerun_opts(&h, &runner)).await.unwrap();
    assert!(!out.ok);
    assert_eq!(out.steps.last().unwrap().step, "host_key");
    assert!(!h.paths.known_hosts_file().exists());
}

/// A failed machine whose rerun stops before its pin keeps the key it had
/// on record: only a new pin replaces it.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_rerun_keeps_the_host_key() {
    let h = head().await;
    let runner = FakePush::new(failed_at_packages());
    assert!(!h.push(h.opts(&runner, false)).await.unwrap().ok);
    assert_eq!(
        h.state().host_key.as_deref(),
        Some("ssh-ed25519 AAAAHOSTKEY")
    );
    runner.host_key.lock().unwrap().clear();
    let out = h.push(h.opts(&runner, false)).await.unwrap();
    assert!(!out.ok);
    assert_eq!(out.steps.last().unwrap().step, "host_key");
    let state = h.state();
    assert_eq!(state.bootstrap, BootstrapState::Failed);
    assert_eq!(state.host_key.as_deref(), Some("ssh-ed25519 AAAAHOSTKEY"));
}

/// An agent key that already logs in under another line of the head's
/// authorized_keys is refused, as a join refuses it: no agent line, and a
/// root-mode entry keeps its login.
#[tokio::test(flavor = "multi_thread")]
async fn an_agent_key_already_authorized_elsewhere_is_refused() {
    let h = head().await;
    let keys = h.paths.state_dir.join("authorized_keys");
    std::fs::write(&keys, "ssh-ed25519 AAAAAGENT someone@else\n").unwrap();
    let runner = FakePush::new(ok_outcome(Mode::Root));
    let out = h.push(h.opts(&runner, true)).await.unwrap();
    assert!(!out.ok, "{out:?}");
    let step = out.steps.last().unwrap();
    assert_eq!(step.step, "register");
    assert!(step.why.as_deref().unwrap().contains("already"), "{step:?}");
    let text = std::fs::read_to_string(&keys).unwrap();
    assert_eq!(text, "ssh-ed25519 AAAAAGENT someone@else\n");
    assert_eq!(h.ssh().as_deref(), Some("me@gpu-3"));
}

/// The report after a root-mode finish fails: the entry goes back to the
/// login it had, and the agent line goes, so nothing points at a machine
/// pastor never recorded as set up.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_report_puts_the_root_mode_ssh_back() {
    let h = head().await;
    let runner = FakePush::new(ok_outcome(Mode::Root));
    {
        // Another key lands on record mid-push, so the report is refused.
        let store = h.d.store();
        *runner.on_bootstrap.lock().unwrap() = Some(Box::new(move || {
            let mut s = store.machine_state("gpu-3").unwrap().unwrap();
            s.host_key = Some("ssh-ed25519 AAAAOTHER".into());
            store.put_machine_state(&s).unwrap();
        }));
    }
    let err = h.push(h.opts(&runner, true)).await.unwrap_err();
    assert_eq!(code(&err), "host_key_mismatch", "{err:#}");
    assert_eq!(h.ssh().as_deref(), Some("me@gpu-3"));
    let keys =
        std::fs::read_to_string(h.paths.state_dir.join("authorized_keys")).unwrap_or_default();
    assert!(!keys.contains("pastor:agent:gpu-3"), "{keys}");
}

/// `machine add --bootstrap` of a root-mode machine already set up: its
/// entry is reached as the agent user by now, so the error names the
/// rerun with the login.
#[tokio::test(flavor = "multi_thread")]
async fn machine_exists_after_root_mode_names_the_rerun_with_its_login() {
    let h = head().await;
    let runner = FakePush::new(ok_outcome(Mode::Root));
    assert!(h.push(h.opts(&runner, true)).await.unwrap().ok);
    assert_eq!(h.ssh().as_deref(), Some("pastor-agent@gpu-3"));
    let err = h.push(h.opts(&runner, true)).await.unwrap_err();
    assert_eq!(code(&err), "machine_exists", "{err:#}");
    assert!(
        err.to_string()
            .contains("pastor machine bootstrap gpu-3 --ssh me@gpu-3"),
        "{err:#}"
    );
}
