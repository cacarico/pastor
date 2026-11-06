//! `pastor join`: the token is a secret, and the join has to leave a machine
//! the head can read about whatever happens on the way.

use std::sync::Mutex;

use sha2::{Digest, Sha256};

use super::*;
use crate::bootstrap::pull::{Fetched, HeadLink, JoinOpts, SshLink, join_with, read_token};
use crate::ipc::{IpcRequest, IpcResponse};
use crate::join::Token;

/// A word in the token's private key that must be found nowhere.
const SECRET: &str = "SECRETKEYMATERIAL";

fn token() -> Token {
    Token {
        head: "me@head".into(),
        head_host_key: "head ssh-ed25519 AAAAHEADHOSTKEY".into(),
        key: format!(
            "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXk{SECRET}AAAA\n-----END OPENSSH PRIVATE KEY-----"
        ),
    }
}

/// One thing the link was asked.
#[derive(Debug, Clone)]
struct Sent {
    head: String,
    key: PathBuf,
    /// The key file's text while the request ran.
    key_text: Option<String>,
    known_hosts: PathBuf,
    line: String,
    /// Whether `<state>/bootstrap.json` was there while the request ran.
    report_file: Option<String>,
}

impl Sent {
    fn op(&self) -> String {
        serde_json::from_str::<serde_json::Value>(&self.line).unwrap()["op"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

/// How the head answers.
struct FakeLink {
    state_dir: PathBuf,
    /// The join: the herdr version the head names, or why ssh failed.
    join: Result<Option<String>, String>,
    /// An answer to the join other than `Joined`, as the head's error.
    join_error: Option<(String, String)>,
    /// The herdr the cache holds.
    herdr: Vec<u8>,
    /// What the head says the sum of it is, when not the true one.
    herdr_sum: Option<String>,
    fetch: Result<(), String>,
    report: Result<(), String>,
    sent: Mutex<Vec<Sent>>,
}

impl FakeLink {
    fn new(m: &Machine) -> FakeLink {
        FakeLink {
            state_dir: m.paths.state_dir.clone(),
            join: Ok(Some("0.9.7".into())),
            join_error: None,
            herdr: b"herdr binary".to_vec(),
            herdr_sum: None,
            fetch: Ok(()),
            report: Ok(()),
            sent: Mutex::new(Vec::new()),
        }
    }

    fn sent(&self) -> Vec<Sent> {
        self.sent.lock().unwrap().clone()
    }

    fn record(&self, head: &str, key: &Path, known_hosts: &Path, line: &str) {
        let report = crate::bootstrap::report_file(&self.state_dir);
        self.sent.lock().unwrap().push(Sent {
            head: head.into(),
            key: key.into(),
            key_text: std::fs::read_to_string(key).ok(),
            known_hosts: known_hosts.into(),
            line: line.into(),
            report_file: std::fs::read_to_string(report).ok(),
        });
    }
}

impl HeadLink for FakeLink {
    fn request(
        &self,
        head: &str,
        key: &Path,
        known_hosts: &Path,
        line: &str,
    ) -> anyhow::Result<String> {
        self.record(head, key, known_hosts, line);
        let req: IpcRequest = serde_json::from_str(line).unwrap();
        let resp = match req {
            IpcRequest::Join { name, .. } => {
                if let Some((code, message)) = &self.join_error {
                    IpcResponse::Error {
                        code: code.clone(),
                        message: message.clone(),
                    }
                } else {
                    match &self.join {
                        Ok(herdr_version) => IpcResponse::Joined {
                            name,
                            herdr_version: herdr_version.clone(),
                        },
                        Err(why) => anyhow::bail!("{why}"),
                    }
                }
            }
            IpcRequest::BootstrapReport { .. } => match &self.report {
                Ok(()) => IpcResponse::Text("ok".into()),
                Err(why) => anyhow::bail!("{why}"),
            },
            other => panic!("unexpected request {other:?}"),
        };
        Ok(serde_json::to_string(&resp).unwrap())
    }

    fn fetch(
        &self,
        head: &str,
        key: &Path,
        known_hosts: &Path,
        line: &str,
        dest: &Path,
        _max: u64,
    ) -> anyhow::Result<Fetched> {
        self.record(head, key, known_hosts, line);
        if let Err(why) = &self.fetch {
            anyhow::bail!("{why}");
        }
        std::fs::write(dest, &self.herdr)?;
        Ok(Fetched {
            sha256: self
                .herdr_sum
                .clone()
                .unwrap_or_else(|| format!("{:x}", Sha256::digest(&self.herdr))),
            size: self.herdr.len() as u64,
        })
    }
}

/// A machine whose herdr answers once the join put it in `~/.local/bin`.
fn joining() -> (Machine, Fake) {
    let m = Machine::new();
    let mut r = Fake::linux(&m);
    r.set(
        &format!("{}/.local/bin/herdr --version", m.home.display()),
        Some("herdr 0.9.7"),
    );
    (m, r)
}

fn opts(m: &Machine) -> JoinOpts {
    JoinOpts {
        name: "m1".into(),
        pastor_from: m.pastor_from.clone(),
        skip_bootstrap: false,
    }
}

fn join(m: &Machine, r: &Fake, link: &FakeLink) -> anyhow::Result<Outcome> {
    join_as(m, r, link, &token())
}

fn join_as(m: &Machine, r: &Fake, link: &FakeLink, token: &Token) -> anyhow::Result<Outcome> {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(join_with(
            &m.paths,
            token,
            &opts(m),
            r,
            link,
            &FakeLocal::default(),
        ))
}

fn error_code(err: &anyhow::Error) -> String {
    err.downcast_ref::<CliError>().unwrap().code.clone()
}

/// Every file under `dir`, with its text.
fn files(dir: &Path) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            out.extend(files(&path));
        } else {
            out.push((
                path.clone(),
                String::from_utf8_lossy(&std::fs::read(&path).unwrap_or_default()).into_owned(),
            ));
        }
    }
    out
}

/// No file under the machine's home holds the token's key.
fn assert_no_key_on_disk(m: &Machine) {
    for (path, text) in files(&m.home) {
        assert!(!text.contains(SECRET), "{} holds the key", path.display());
    }
    let left: Vec<String> = std::fs::read_dir(&m.paths.state_dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("join-") || n.starts_with("join_"))
        .collect();
    assert!(left.is_empty(), "left in the state dir: {left:?}");
}

/// Log lines, as a subscriber that keeps them all would write them.
#[derive(Clone, Default)]
struct Logs(std::sync::Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
    type Writer = Logs;
    fn make_writer(&'a self) -> Logs {
        self.clone()
    }
}

#[test]
fn token_from_stdin() {
    let encoded = token().encode();
    let mut tty = || -> anyhow::Result<Option<String>> { panic!("no terminal question") };
    // `--token-stdin` wins over the variable, and the text may end in a
    // newline.
    let got = read_token(
        true,
        Some("pjt_ignored".into()),
        &mut std::io::Cursor::new(format!("{encoded}\n")),
        &mut tty,
    )
    .unwrap();
    assert_eq!(got, token());
    // The variable when stdin is not asked for.
    let got = read_token(
        false,
        Some(format!(" {encoded} ")),
        &mut std::io::empty(),
        &mut tty,
    )
    .unwrap();
    assert_eq!(got, token());
    // The terminal last, and with none the refusal says where a token may
    // come from.
    let mut typed = || Ok(Some(format!("{encoded}\n")));
    let got = read_token(false, None, &mut std::io::empty(), &mut typed).unwrap();
    assert_eq!(got, token());
    let mut nobody = || Ok(None);
    let err = read_token(false, Some(" ".into()), &mut std::io::empty(), &mut nobody).unwrap_err();
    assert_eq!(error_code(&err), "invalid_args");
    assert!(err.to_string().contains("never an argument"), "{err}");
}

/// A token that does not decode is refused without quoting what was given.
#[test]
fn a_bad_token_is_refused_without_quoting_it() {
    let mut tty = || Ok(None);
    for given in [
        format!("pjt_{SECRET}!!"),
        format!("{SECRET} not a token"),
        format!("pjt_{}", "A".repeat(64)),
    ] {
        let err = read_token(true, None, &mut std::io::Cursor::new(given), &mut tty).unwrap_err();
        assert_eq!(error_code(&err), "join_token_invalid");
        assert!(!format!("{err:#}").contains(SECRET), "{err:#}");
    }
}

#[test]
fn temp_join_key_deleted_on_error() {
    // ssh refused, the head said no, the name is taken: the key goes each
    // time, and it was there while ssh ran.
    type Case = (
        &'static str,
        Result<Option<String>, String>,
        Option<(&'static str, &'static str)>,
    );
    let cases: [Case; 3] = [
        (
            "join_token_invalid",
            Err("me@head: Permission denied (publickey).".into()),
            None,
        ),
        (
            "head_unreachable",
            Err("ssh: connect to host head port 22: Connection timed out".into()),
            None,
        ),
        (
            "machine_name_taken",
            Ok(None),
            Some(("machine_name_taken", "m1 is in flock.toml")),
        ),
    ];
    for (code, joined, error) in cases {
        let (m, r) = joining();
        let mut link = FakeLink::new(&m);
        link.join = joined;
        link.join_error = error.map(|(c, m)| (c.into(), m.into()));
        let err = join(&m, &r, &link).unwrap_err();
        assert_eq!(error_code(&err), code, "{err:#}");
        let sent = link.sent();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(
            sent[0].key_text.as_deref(),
            Some(format!("{}\n", token().key).as_str())
        );
        assert!(!sent[0].key.exists(), "the key file is gone");
        assert_no_key_on_disk(&m);
        // Nothing was bootstrapped past the keys.
        assert_eq!(
            r.ran(&format!("{}/.local/bin/pastor setup", m.home.display())),
            0
        );
    }
}

/// The key is removed when the join panics too: `Drop` runs on unwinding.
#[test]
fn temp_join_key_deleted_on_panic() {
    struct Panics;
    impl HeadLink for Panics {
        fn request(&self, _: &str, key: &Path, _: &Path, _: &str) -> anyhow::Result<String> {
            assert!(key.exists());
            panic!("ssh blew up");
        }
        fn fetch(
            &self,
            _: &str,
            _: &Path,
            _: &Path,
            _: &str,
            _: &Path,
            _: u64,
        ) -> anyhow::Result<Fetched> {
            unreachable!()
        }
    }
    let (m, r) = joining();
    let link = Panics;
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(join_with(
                &m.paths,
                &token(),
                &opts(&m),
                &r,
                &link,
                &FakeLocal::default(),
            ));
    }));
    assert!(caught.is_err());
    assert_no_key_on_disk(&m);
}

/// Nothing the join runs, sends, prints or keeps holds the key, on the way
/// to a joined machine, and when the head cannot be told how it went.
#[test]
fn token_never_reaches_argv_logs_or_files() {
    for report in [Ok(()), Err("Connection timed out".to_string())] {
        let (m, r) = joining();
        let mut link = FakeLink::new(&m);
        link.report = report;
        let logs = Logs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_max_level(tracing::Level::TRACE)
            .finish();
        let outcome =
            tracing::subscriber::with_default(subscriber, || join(&m, &r, &link)).unwrap();
        assert!(outcome.ok, "{:?}", outcome.steps);
        let secret = token().key;
        // argv of every command the steps ran
        for call in r.calls() {
            assert!(!call.contains(SECRET), "{call}");
        }
        // what the link was given: the key is a file's path, never its text
        for s in link.sent() {
            let words = [
                &s.head,
                &s.line,
                &s.key.display().to_string(),
                &s.known_hosts.display().to_string(),
            ]
            .map(|w| w.contains(SECRET));
            assert_eq!(words, [false; 4], "{s:?}");
            assert!(s.key.display().to_string().contains("join") || s.op() != "join");
        }
        // the outcome, which is also what is printed and reported
        let shown = serde_json::to_string(&outcome).unwrap() + &super::human(&outcome);
        assert!(!shown.contains(SECRET), "{shown}");
        // the log
        let logged = String::from_utf8_lossy(&logs.0.lock().unwrap()).into_owned();
        assert!(
            !logged.contains(SECRET) && !logged.contains(&secret),
            "{logged}"
        );
        // every file left on the machine, the kept report among them
        assert_no_key_on_disk(&m);
    }
}

#[test]
fn the_head_host_key_is_pinned_once_and_only_its_own() {
    let (m, r) = joining();
    let link = FakeLink::new(&m);
    join(&m, &r, &link).unwrap();
    join(&m, &r, &link).unwrap();
    let known = std::fs::read_to_string(m.paths.known_hosts_file()).unwrap();
    assert_eq!(known, "head ssh-ed25519 AAAAHEADHOSTKEY\n");
    // ssh was given that file, and the join asked for nothing before it.
    assert!(
        link.sent()
            .iter()
            .all(|s| s.known_hosts == m.paths.known_hosts_file())
    );

    // A line for other hosts, or a marker, is not a pin.
    for bad in [
        "other ssh-ed25519 AAAA",
        "head,other ssh-ed25519 AAAA",
        "@cert-authority head ssh-ed25519 AAAA",
        "*.example ssh-ed25519 AAAA",
        "head ssh-ed25519 AAAA\nother ssh-ed25519 BBBB",
        "head ssh-dss AAAA",
        "head ssh-ed25519",
    ] {
        let (m, r) = joining();
        let link = FakeLink::new(&m);
        let mut t = token();
        t.head_host_key = bad.into();
        let err = join_as(&m, &r, &link, &t).unwrap_err();
        assert_eq!(error_code(&err), "join_token_invalid", "{bad:?}");
        assert!(link.sent().is_empty(), "{bad:?}: no ssh before a pin");
        assert!(!m.paths.known_hosts_file().exists(), "{bad:?}");
    }
}

/// A machine that cannot join stops before the token is spent.
#[test]
fn a_machine_that_lacks_a_tool_spends_no_token() {
    let (m, mut r) = joining();
    r.set("sh -c command -v ssh-keygen", None);
    let link = FakeLink::new(&m);
    let err = join(&m, &r, &link).unwrap_err();
    assert_eq!(error_code(&err), BOOTSTRAP_MISSING);
    assert!(link.sent().is_empty());
    let (m, mut r) = joining();
    r.set("cat /etc/ssh/ssh_host_ed25519_key.pub", None);
    let link = FakeLink::new(&m);
    let err = join(&m, &r, &link).unwrap_err();
    assert_eq!(error_code(&err), BOOTSTRAP_MISSING);
    assert!(link.sent().is_empty());
}

#[test]
fn shepherd_name_written() {
    let (m, r) = joining();
    let link = FakeLink::new(&m);
    std::fs::create_dir_all(m.paths.config_dir.clone()).unwrap();
    std::fs::write(
        m.paths.config_file(),
        "# mine\ntick = \"5s\"\n\n[shepherd]\ntakes_flock_work = true\n",
    )
    .unwrap();
    let outcome = join(&m, &r, &link).unwrap();
    assert!(outcome.ok, "{:?}", outcome.steps);
    let text = std::fs::read_to_string(m.paths.config_file()).unwrap();
    let config: crate::config::PastorConfig = toml::from_str(&text).unwrap();
    assert_eq!(config.shepherd.machine.as_deref(), Some("m1"));
    assert!(config.shepherd.takes_flock_work, "{text}");
    assert!(text.contains("# mine"), "{text}");
    // The service comes after the name and after the head is named.
    let order: Vec<&str> = outcome.steps.iter().map(|s| s.step.as_str()).collect();
    let at = |n: &str| order.iter().position(|s| *s == n).unwrap();
    assert!(
        at("client") < at("shepherd_name")
            && at("shepherd_name") < at("services")
            && at("services") < at("shepherd"),
        "{order:?}"
    );
    assert_eq!(step(&outcome, "shepherd_name").status, StepStatus::Done);
    assert_eq!(step(&outcome, "shepherd").status, StepStatus::Done);
    let pastor = format!("{}/.local/bin/pastor setup systemd --yes", m.home.display());
    assert_eq!(r.ran(&pastor), 1, "{:?}", r.calls());
    // client.toml names the head the token came from.
    let head = crate::head::load(&crate::head::client_file(&m.paths))
        .unwrap()
        .unwrap();
    assert_eq!(head.ssh, "me@head");
    // Again: the file is as it was.
    join(&m, &r, &link).unwrap();
    assert_eq!(
        std::fs::read_to_string(m.paths.config_file()).unwrap(),
        text
    );
}

#[test]
fn write_shepherd_keeps_the_rest_and_says_when_nothing_changed() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("c/pastor.toml");
    assert!(crate::bootstrap::pull::write_shepherd(&file, "pi-3").unwrap());
    assert!(!crate::bootstrap::pull::write_shepherd(&file, "pi-3").unwrap());
    std::fs::write(&file, "x = [\n").unwrap();
    let err = crate::bootstrap::pull::write_shepherd(&file, "pi-3").unwrap_err();
    assert_eq!(error_code(&err), "config_error");
}

#[test]
fn fetch_uses_joined_herdr_version() {
    for (named, want) in [
        (Some("0.9.7"), "0.9.7"),
        (None, crate::artifacts::DEFAULT_HERDR_VERSION),
    ] {
        let (m, r) = joining();
        let mut link = FakeLink::new(&m);
        link.join = Ok(named.map(str::to_string));
        let outcome = join(&m, &r, &link).unwrap();
        let sent = link.sent();
        let ask = sent.iter().find(|s| s.op() == "artifact_locate").unwrap();
        let v: serde_json::Value = serde_json::from_str(&ask.line).unwrap();
        assert_eq!(v["name"], "herdr");
        assert_eq!(v["version"], want);
        assert_eq!(v["target"], "linux-x86_64");
        // Over the machine key the join made, not the token's.
        assert_eq!(ask.key, m.paths.machine_key());
        // Installed, executable, and what the herdr step found.
        let herdr = m.home.join(".local/bin/herdr");
        assert_eq!(std::fs::read(&herdr).unwrap(), b"herdr binary");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&herdr).unwrap().permissions().mode() & 0o777,
            0o755
        );
        assert!(outcome.ok, "{:?}", outcome.steps);
        assert_eq!(outcome.herdr_version.as_deref(), Some("0.9.7"));
        // Left nowhere else.
        assert_no_key_on_disk(&m);
        assert!(
            !files(&m.paths.state_dir)
                .iter()
                .any(|(_, t)| t == "herdr binary")
        );
    }
}

/// A file that is not what the head said it is installs nothing.
#[test]
fn a_herdr_that_does_not_match_its_sum_is_not_installed() {
    let (m, r) = joining();
    let mut link = FakeLink::new(&m);
    link.herdr_sum = Some("0".repeat(64));
    let outcome = join(&m, &r, &link).unwrap();
    assert!(!outcome.ok);
    let failed = outcome.steps.last().unwrap();
    assert_eq!(failed.step, "herdr");
    assert!(
        failed.why.as_deref().unwrap().contains("sha256"),
        "{failed:?}"
    );
    assert!(!m.home.join(".local/bin/herdr").exists());
    assert_no_key_on_disk(&m);
    assert!(
        files(&m.paths.state_dir)
            .iter()
            .all(|(p, _)| !p.ends_with("herdr"))
    );
}

#[test]
fn failure_before_services_is_reported_directly() {
    let (m, r) = joining();
    let mut link = FakeLink::new(&m);
    link.fetch = Err("artifact_not_cached: herdr 0.9.7 linux-x86_64".into());
    let outcome = join(&m, &r, &link).unwrap();
    assert!(!outcome.ok);
    let failed = outcome.steps.last().unwrap();
    assert_eq!(
        (failed.step.as_str(), failed.status),
        ("herdr", StepStatus::Failed)
    );
    assert!(
        failed
            .why
            .as_deref()
            .unwrap()
            .contains("artifact_not_cached"),
        "{failed:?}"
    );
    // No service started, so no shepherd will tell the head: the join did.
    assert_eq!(
        r.ran(&format!("{}/.local/bin/pastor setup", m.home.display())),
        0
    );
    let sent = link.sent();
    let report = sent.iter().find(|s| s.op() == "bootstrap_report").unwrap();
    let req: IpcRequest = serde_json::from_str(&report.line).unwrap();
    let IpcRequest::BootstrapReport { machine, result } = req else {
        panic!()
    };
    assert_eq!(machine, "m1");
    assert!(!result.ok);
    assert_eq!(result.steps.last().unwrap().step, "herdr");
    // Over the machine key, which the token's line is gone for.
    assert_eq!(report.key, m.paths.machine_key());
    // The head has it: nothing is left for the shepherd.
    assert!(!crate::bootstrap::report_file(&m.paths.state_dir).exists());
}

#[test]
fn unreachable_head_keeps_report_file() {
    let (m, r) = joining();
    let mut link = FakeLink::new(&m);
    link.report = Err("ssh: connect to host head port 22: Connection timed out".into());
    let outcome = join(&m, &r, &link).unwrap();
    assert!(outcome.ok, "{:?}", outcome.steps);
    let file = crate::bootstrap::report_file(&m.paths.state_dir);
    let text = std::fs::read_to_string(&file).unwrap();
    let kept: Outcome = serde_json::from_str(&text).unwrap();
    assert_eq!(kept, outcome);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!text.contains(SECRET));
    // No half-written copy beside it.
    let others: Vec<_> = files(&m.paths.state_dir)
        .into_iter()
        .filter(|(p, _)| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("bootstrap.json.")
        })
        .collect();
    assert!(others.is_empty(), "{others:?}");
}

#[test]
fn bootstrap_report_file_kept_until_acknowledged() {
    let (m, r) = joining();
    let link = FakeLink::new(&m);
    let outcome = join(&m, &r, &link).unwrap();
    let sent = link.sent();
    let report = sent.iter().find(|s| s.op() == "bootstrap_report").unwrap();
    // It was on disk, whole, while the head had not answered.
    let during: Outcome = serde_json::from_str(report.report_file.as_deref().unwrap()).unwrap();
    assert_eq!(during, outcome);
    // And gone once it had.
    assert!(!crate::bootstrap::report_file(&m.paths.state_dir).exists());
}

/// A report neither sent nor kept is the one failure that leaves a joined
/// machine pending with nobody to tell: it is an error, naming how to go on.
#[test]
fn a_report_that_went_nowhere_is_an_error() {
    let (m, r) = joining();
    let mut link = FakeLink::new(&m);
    link.report = Err("Connection timed out".into());
    // A file where the state dir's report must go.
    std::fs::create_dir_all(crate::bootstrap::report_file(&m.paths.state_dir)).unwrap();
    let err = join(&m, &r, &link).unwrap_err();
    assert_eq!(error_code(&err), "runtime_error");
    assert!(
        err.to_string().contains("pastor join create --replace m1"),
        "{err}"
    );
}

/// A dir an earlier join left, of a process that is gone, goes; one of a
/// live process stays.
#[test]
fn stale_join_dirs_of_dead_processes_are_removed() {
    let (m, r) = joining();
    let state = &m.paths.state_dir;
    std::fs::create_dir_all(state).unwrap();
    let dead = state.join("join-4194303-1");
    let herdr = state.join("join-herdr-4194303-2");
    let alive = state.join(format!("join-{}-3", std::process::id()));
    let other = state.join("join-notapid-1");
    for d in [&dead, &herdr, &alive, &other] {
        std::fs::create_dir_all(d).unwrap();
        std::fs::write(d.join("join_key"), SECRET).unwrap();
    }
    join(&m, &r, &FakeLink::new(&m)).unwrap();
    assert!(!dead.exists() && !herdr.exists());
    assert!(alive.exists() && other.exists());
}

#[test]
fn ssh_gets_the_key_as_a_file_and_the_pinned_host_strictly() {
    let (m, _) = joining();
    let key = m.paths.state_dir.join("join-1-1/join_key");
    let argv = crate::bootstrap::pull::ssh_argv("me@head", &key, &m.paths.known_hosts_file());
    let at = |w: &str| argv.iter().position(|a| a == w).unwrap();
    assert_eq!(argv[at("-i") + 1], key.display().to_string());
    for want in [
        "IdentitiesOnly=yes".to_string(),
        "StrictHostKeyChecking=yes".to_string(),
        "BatchMode=yes".to_string(),
        "ControlPath=none".to_string(),
        format!(
            "UserKnownHostsFile={}",
            m.paths.known_hosts_file().display()
        ),
    ] {
        assert!(argv.contains(&want), "{want}: {argv:?}");
    }
    assert_eq!(&argv[argv.len() - 3..], ["--", "me@head", "bridge"]);
}

/// The real link, against an `ssh` of the test's own: the line goes on
/// stdin, the key is only a path in the argv, and the reply comes back.
#[test]
fn ssh_link_sends_one_line_and_reads_the_reply_and_the_file() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let argv_log = dir.path().join("argv");
    let stdin_log = dir.path().join("stdin");
    let ssh = dir.path().join("ssh");
    let body = b"herdr bytes".to_vec();
    let sum = format!("{:x}", Sha256::digest(&body));
    std::fs::write(dir.path().join("body"), &body).unwrap();
    let artifact = serde_json::to_string(&IpcResponse::Artifact {
        path: String::new(),
        sha256: sum.clone(),
        size: body.len() as u64,
    })
    .unwrap();
    std::fs::write(
        &ssh,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nIFS= read -r line\nprintf '%s\\n' \"$line\" > '{}'\ncase \"$line\" in\n*artifact_locate*) printf '%s\\n' '{}'; cat '{}' ;;\n*) printf '\\n%s\\n' '{{\"kind\":\"text\",\"data\":\"hello\"}}' ;;\nesac\n",
            argv_log.display(),
            stdin_log.display(),
            artifact,
            dir.path().join("body").display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let link = SshLink::new(&ssh);
    let key = dir.path().join("join_key");
    let kh = dir.path().join("known_hosts");
    let got = link
        .request("me@head", &key, &kh, "{\"op\":\"ping\"}")
        .unwrap();
    assert_eq!(got, "{\"kind\":\"text\",\"data\":\"hello\"}");
    assert_eq!(
        std::fs::read_to_string(&stdin_log).unwrap(),
        "{\"op\":\"ping\"}\n"
    );
    let argv = std::fs::read_to_string(&argv_log).unwrap();
    assert!(argv.contains(&format!("{}\n", key.display())), "{argv}");

    let dest = dir.path().join("herdr");
    let fetched = link
        .fetch(
            "me@head",
            &key,
            &kh,
            "{\"op\":\"artifact_locate\"}",
            &dest,
            1 << 20,
        )
        .unwrap();
    assert_eq!(
        (fetched.sha256.as_str(), fetched.size),
        (sum.as_str(), body.len() as u64)
    );
    assert_eq!(std::fs::read(&dest).unwrap(), body);
    // Past the cap: refused, and nothing is left at dest.
    std::fs::remove_file(&dest).unwrap();
    let err = link
        .fetch(
            "me@head",
            &key,
            &kh,
            "{\"op\":\"artifact_locate\"}",
            &dest,
            4,
        )
        .unwrap_err();
    assert_eq!(error_code(&err), "artifact_checksum_mismatch");
    assert!(!dest.exists());
}

/// A machine set up beforehand joins and names its serve, and that is all:
/// no herdr fetched, no service installed, and the head told at once.
#[test]
fn skip_bootstrap_joins_and_names_the_serve_only() {
    let (m, r) = joining();
    let link = FakeLink::new(&m);
    let mut o = opts(&m);
    o.skip_bootstrap = true;
    let outcome = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(join_with(
            &m.paths,
            &token(),
            &o,
            &r,
            &link,
            &FakeLocal::default(),
        ))
        .unwrap();
    assert!(outcome.ok, "{:?}", outcome.steps);
    let order: Vec<&str> = outcome.steps.iter().map(|s| s.step.as_str()).collect();
    assert_eq!(order, ["facts", "keys", "client", "shepherd_name"]);
    let ops: Vec<String> = link.sent().iter().map(Sent::op).collect();
    assert_eq!(ops, ["join", "bootstrap_report"]);
    assert!(!m.home.join(".local/bin/herdr").exists());
    assert!(!m.home.join(".local/bin/pastor").exists());
    assert_eq!(r.ran(&format!("{}/.local/bin/pastor", m.home.display())), 0);
    let config = std::fs::read_to_string(m.paths.config_file()).unwrap();
    assert!(config.contains("machine = \"m1\""), "{config}");
    assert_no_key_on_disk(&m);
}
