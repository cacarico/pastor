use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::herdr::fake::{FakeHerdr, PaneInput};
use crate::provision::run::AgentEnv;
use crate::provision::run::testing::Fake;

/// What a shell profile does to the env a pane starts with: a name set to
/// a value, or dropped (`None`).
type Profile = Vec<(&'static str, Option<&'static str>)>;

/// A machine: its home, its shell, and a herdr whose panes run what is
/// typed into them through `sh`, with the pane's env after `profile`.
struct Machine {
    _dir: tempfile::TempDir,
    home: std::path::PathBuf,
    shell: Fake,
    herdr: FakeHerdr,
    stop: Arc<AtomicBool>,
}

impl Machine {
    fn new() -> Machine {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let herdr = FakeHerdr::new();
        herdr.set_home(Some(&home.to_string_lossy()));
        Machine {
            shell: Fake::new(&home, None),
            home,
            herdr,
            stop: Arc::new(AtomicBool::new(false)),
            _dir: dir,
        }
    }

    /// Run each pane's typed line once, as its login shell would after
    /// reading a profile that does `profile`.
    fn shells_run(&self, profile: Profile) {
        let (herdr, home, stop) = (self.herdr.clone(), self.home.clone(), self.stop.clone());
        std::thread::spawn(move || {
            let mut done = std::collections::HashSet::new();
            while !stop.load(Ordering::Relaxed) {
                for ws in herdr.workspaces() {
                    for pane in herdr.panes(&ws) {
                        let typed: Vec<String> = herdr
                            .pane_input(&pane)
                            .into_iter()
                            .filter_map(|i| match i {
                                PaneInput::Text(t) => Some(t),
                                PaneInput::Keys(_) => None,
                            })
                            .collect();
                        if typed.is_empty() || !done.insert(pane.clone()) {
                            continue;
                        }
                        let mut env: BTreeMap<String, String> = herdr
                            .pane_env(&pane)
                            .as_object()
                            .map(|o| {
                                o.iter()
                                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                                    .collect()
                            })
                            .unwrap_or_default();
                        env.insert("HOME".into(), home.to_string_lossy().into_owned());
                        env.entry("PATH".into()).or_insert("/usr/bin:/bin".into());
                        for (k, v) in &profile {
                            match v {
                                Some(v) => env.insert(k.to_string(), v.to_string()),
                                None => env.remove(*k),
                            };
                        }
                        let ok = std::process::Command::new("/bin/sh")
                            .arg("-c")
                            .arg(typed.join(""))
                            .env_clear()
                            .envs(&env)
                            .status()
                            .unwrap();
                        assert!(ok.success());
                    }
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
    }

    async fn rows(&self, wants: Vec<Want>, timeout: Duration) -> Vec<Row> {
        let conn: Arc<dyn Connector> = Arc::new(self.herdr.clone());
        let rows = rows(&self.shell, conn, "pi-3", wants, timeout).await;
        self.stop.store(true, Ordering::Relaxed);
        rows
    }

    /// Every pane the probes opened is closed.
    fn no_pane_left(&self) {
        for ws in self.herdr.workspaces() {
            assert!(
                self.herdr.panes(&ws).is_empty(),
                "{ws}: {:?}",
                self.herdr.panes(&ws)
            );
        }
        let left: Vec<_> = std::fs::read_dir(self.home.join(".cache/pastor-check"))
            .map(|d| d.flatten().map(|e| e.file_name()).collect())
            .unwrap_or_default();
        assert!(left.is_empty(), "{left:?}");
    }
}

/// A Claude agent with its own config dir, on a machine whose flock sets a
/// notes URL and a token, as a task on it gets them.
fn task_env() -> Vec<Want> {
    let layers: BTreeMap<String, String> = [
        ("NOTES_URL", "http://notes.example:8790"),
        ("NOTES_TOKEN", "s3cret-value"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let agent = AgentEnv {
        name: "claude-main".into(),
        kind: "claude".into(),
        env: [(
            "CLAUDE_CONFIG_DIR".to_string(),
            "~/.claude-main".to_string(),
        )]
        .into(),
    };
    wants(&layers, &[agent], Some("me@head"))
}

#[tokio::test]
async fn unchanged_vars_pass() {
    let m = Machine::new();
    m.shells_run(vec![("SHELL_OWN", Some("x"))]);
    let rows = m.rows(task_env(), Duration::from_secs(10)).await;
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.status, Status::Pass, "{row:?}");
    assert_eq!(row.agent.as_deref(), Some("claude-main"));
    assert!(row.first_line.contains("5 variables"), "{row:?}");
    m.no_pane_left();
}

#[tokio::test]
async fn changed_vars_warn() {
    let m = Machine::new();
    m.shells_run(vec![("CLAUDE_CONFIG_DIR", Some("/home/u/.claude"))]);
    let rows = m.rows(task_env(), Duration::from_secs(10)).await;
    let row = &rows[0];
    assert_eq!(row.status, Status::Warn, "{row:?}");
    let reason = row.reason.as_deref().unwrap();
    assert!(reason.contains("changes CLAUDE_CONFIG_DIR"), "{reason}");
    assert!(
        !reason.contains("NOTES_URL") && !reason.contains("drops"),
        "{reason}"
    );
    m.no_pane_left();
}

#[tokio::test]
async fn dropped_vars_warn_and_secret_values_never_leave_the_pane() {
    let m = Machine::new();
    m.shells_run(vec![
        ("NOTES_URL", None),
        ("NOTES_TOKEN", None),
        ("PASTOR_HEAD", None),
    ]);
    let rows = m.rows(task_env(), Duration::from_secs(10)).await;
    let row = &rows[0];
    assert_eq!(row.status, Status::Warn, "{row:?}");
    let reason = row.reason.as_deref().unwrap();
    assert!(
        reason.contains("drops NOTES_TOKEN, NOTES_URL, PASTOR_HEAD"),
        "{reason}"
    );
    assert!(!reason.contains("changes"), "{reason}");
    // The probe asks of a secret-looking name only whether it is set.
    let names = ["NOTES_TOKEN", "NOTES_URL"];
    let p = probe(&names, "f");
    assert!(p.contains("echo \"NOTES_TOKEN set\""), "{p}");
    assert!(!p.contains("\"$NOTES_TOKEN\""), "{p}");
    assert!(p.contains("\"$NOTES_URL\""), "{p}");
    assert!(!format!("{rows:?}").contains("s3cret"));
    m.no_pane_left();
}

#[tokio::test]
async fn the_pane_is_closed_on_timeout() {
    let m = Machine::new();
    // No shell ever runs the probe: a profile waiting on a prompt.
    let rows = m.rows(task_env(), Duration::from_millis(800)).await;
    let row = &rows[0];
    assert_eq!(row.status, Status::Error, "{row:?}");
    assert!(
        row.reason
            .as_deref()
            .unwrap()
            .contains("did not run the probe"),
        "{row:?}"
    );
    let methods: Vec<String> = m.herdr.requests().into_iter().map(|r| r.method).collect();
    assert!(
        methods.iter().any(|x| x == "workspace.create")
            && methods.iter().any(|x| x == "pane.close"),
        "{methods:?}"
    );
    m.no_pane_left();
}

#[test]
fn compare_reads_values_and_set_lines() {
    let env: BTreeMap<String, String> = [("A", "1"), ("A_KEY", "k"), ("AB", "2")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let found = "A=1\nA_KEY set\nAB=3\n";
    assert_eq!(compare(&env, found), (vec!["AB".to_string()], vec![]));
    let found = "A unset\nA_KEY unset\n";
    assert_eq!(
        compare(&env, found),
        (vec![], vec!["A".into(), "AB".into(), "A_KEY".into()])
    );
}

/// With no agent env of its own, a task still gets the layers and
/// pastor's marks, last and over all of them.
#[test]
fn wants_put_pastors_marks_last() {
    let layers: BTreeMap<String, String> = [("PASTOR_TASK".to_string(), "x".to_string())].into();
    let w = wants(&layers, &[], None);
    assert_eq!(w.len(), 1);
    assert_eq!(w[0].agent, None);
    assert_eq!(w[0].env["PASTOR_TASK"], PROBE_TASK);
    assert!(!w[0].env.contains_key("PASTOR_HEAD"));
}
