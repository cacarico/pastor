//! Connector protocol tests with real child processes: the shell fixtures in
//! tests/fixtures/connector/ emit items, cursors and bad lines, and fail, hang or
//! crash on demand.
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{TimeZone, Utc};
use pastor::config::Paths;
use pastor::connector::process::{self, StreamSource};
use pastor::connector::{Connector, Discovered, discover};
use pastor::connector::{ItemSource, RunInput};
use serde_json::json;

/// How long a test waits for the daemon or the fake herdr to do something.
/// Generous on purpose: a CI runner under load has taken more than 10s to
/// bring a daemon up, and a wait that ends early only ever fails a good run.
const WAIT: Duration = Duration::from_secs(60);

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/connector")
        .join(name)
}

struct Env {
    _tmp: tempfile::TempDir,
    paths: Paths,
}

/// Temp config/state/data dirs with the named fixtures linked in.
fn env_with(fixtures: &[&str]) -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let paths =
        Paths::new(tmp.path().join("c"), tmp.path().join("s")).with_data_dir(tmp.path().join("d"));
    std::fs::create_dir_all(paths.connectors_dir()).unwrap();
    for f in fixtures {
        std::os::unix::fs::symlink(fixture(f), paths.connectors_dir().join(f)).unwrap();
    }
    Env { _tmp: tmp, paths }
}

impl Env {
    fn connector(&self, id: &str) -> Arc<Connector> {
        match discover(&self.paths)
            .unwrap()
            .into_iter()
            .find(|d| d.id() == id)
            .unwrap()
        {
            Discovered::Valid(p) => Arc::from(p),
            other => panic!("{other:?}"),
        }
    }

    fn dotenv(&self, id: &str, text: &str) {
        let f = self.paths.connector_env_file(id);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, text).unwrap();
    }

    fn logs(&self, job: &str) -> Vec<String> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(self.paths.runs_dir(job))
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        files.sort();
        files
            .iter()
            .map(|p| std::fs::read_to_string(p).unwrap())
            .collect()
    }
}

fn input(config: serde_json::Value, cursor: Option<&str>) -> RunInput {
    let at = Utc.with_ymd_and_hms(2026, 9, 23, 9, 0, 0).unwrap();
    RunInput {
        config,
        cursor: cursor.map(str::to_string),
        since: at,
        now: at,
    }
}

#[tokio::test]
async fn a_poll_connector_gets_the_handshake_and_its_lines_are_parsed() {
    let env = env_with(&["echo"]);
    env.dotenv("echo", "FIXTURE_TOKEN=tok-sekrit-42\n");
    let src = process::source(
        env.connector("echo"),
        env.paths.clone(),
        Some("support".into()),
    );
    assert_eq!(src.id(), "echo");
    let out = src
        .run(input(json!({"channel": "C0123"}), Some("c-0")))
        .await
        .unwrap();

    let keys: Vec<&str> = out.items.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(keys, vec!["k1", "k2", "k1"], "dedup is the scheduler's job");
    assert_eq!(out.items[0].fields["author"], "ana");
    assert_eq!(out.items[1].fields["extra"]["n"], 2);
    assert_eq!(out.cursor.as_deref(), Some("c-2"), "the last cursor wins");

    let handshake = &out.logs[0];
    assert!(
        handshake.starts_with("info: handshake ")
            && handshake.contains(r#""config":{"channel":"C0123"}"#)
            && handshake.contains(r#""cursor":"c-0""#)
            && handshake.contains(r#""since":"2026-09-23T09:00:00Z""#),
        "{handshake}"
    );
    let cwd = std::fs::canonicalize(fixture("echo")).unwrap();
    assert_eq!(
        out.logs[1],
        format!("debug: env echo support {}", cwd.display())
    );
    assert_eq!(
        out.logs[2], "info: logging token [redacted:FIXTURE_TOKEN]",
        "protocol log records are redacted like stderr"
    );
    assert!(!out.logs.iter().any(|l| l.contains("tok-sekrit-42")));
    let skipped: Vec<&String> = out.logs.iter().filter(|l| l.starts_with("warn:")).collect();
    assert_eq!(skipped.len(), 3, "{:?}", out.logs);
    assert!(skipped[0].contains("line 5") && skipped[0].contains("not JSON"));

    let logs = env.logs("support");
    assert_eq!(logs.len(), 1);
    assert!(
        logs[0].contains("token is [redacted:FIXTURE_TOKEN]"),
        "{}",
        logs[0]
    );
    assert!(!logs[0].contains("tok-sekrit-42"));
    assert!(logs[0].contains("[pastor: skipped stdout line 5: not JSON"));
    assert!(logs[0].ends_with("[pastor: exit 0]\n"), "{}", logs[0]);
    assert!(env.paths.connector_state_dir("support").is_dir());
}

#[tokio::test]
async fn a_failing_poll_is_an_error_and_its_output_is_discarded() {
    let env = env_with(&["echo"]);
    env.dotenv("echo", "FIXTURE_MODE=fail\n");
    let src = process::source(env.connector("echo"), env.paths.clone(), Some("j".into()));
    let err = src.run(input(json!({}), None)).await.unwrap_err();
    assert!(
        err.starts_with("exit 4: failing on purpose (log: "),
        "{err}"
    );
    assert!(err.contains("/runs/j/"), "{err}");
    assert!(env.logs("j")[0].contains("failing on purpose"));
}

#[tokio::test]
async fn a_hanging_poll_times_out() {
    let env = env_with(&["echo"]);
    env.dotenv("echo", "FIXTURE_MODE=hang\n");
    let src = process::source(env.connector("echo"), env.paths.clone(), Some("j".into()));
    let started = Instant::now();
    let err = src.run(input(json!({}), None)).await.unwrap_err();
    assert!(err.starts_with("timed out after 5s"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(9));
}

#[tokio::test]
async fn a_broken_env_file_fails_the_run() {
    let env = env_with(&["echo"]);
    env.dotenv("echo", "not an assignment\n");
    let src = process::source(env.connector("echo"), env.paths.clone(), Some("j".into()));
    let err = src.run(input(json!({}), None)).await.unwrap_err();
    assert!(err.contains(".env") && err.contains("line 1"), "{err}");
}

#[tokio::test]
async fn without_a_job_logs_go_under_the_connector_id() {
    let env = env_with(&["echo"]);
    let src = process::source(env.connector("echo"), env.paths.clone(), None);
    let out = src.run(input(json!({}), None)).await.unwrap();
    assert!(
        out.logs[1].starts_with("debug: env echo none "),
        "{:?}",
        out.logs
    );
    assert_eq!(env.logs("@echo").len(), 1);
}

async fn drain_until(
    src: &dyn ItemSource,
    cfg: &serde_json::Value,
    want: usize,
) -> (Vec<String>, Vec<Option<String>>, Vec<String>) {
    let deadline = Instant::now() + WAIT;
    let mut keys = Vec::new();
    let mut cursors = Vec::new();
    let mut logs = Vec::new();
    while keys.len() < want {
        assert!(Instant::now() < deadline, "only got {keys:?}");
        if let Ok(out) = src.run(input(cfg.clone(), Some("cur-0"))).await {
            // As the scheduler does once it has persisted a batch.
            src.ack(out.batch);
            keys.extend(out.items.into_iter().map(|i| i.key));
            cursors.push(out.cursor);
            logs.extend(out.logs);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    (keys, cursors, logs)
}

#[tokio::test]
async fn a_stream_stays_up_and_each_run_drains_what_it_emitted() {
    let env = env_with(&["stream"]);
    env.dotenv("stream", "FIXTURE_TOKEN=tok-stream-77\n");
    let src = process::source(env.connector("stream"), env.paths.clone(), Some("s".into()));
    let cfg = json!({"room": "r1"});
    let (keys, cursors, logs) = drain_until(src.as_ref(), &cfg, 1).await;
    assert_eq!(keys, vec!["start-1"]);
    assert!(
        logs.iter()
            .any(|l| l == "info: logging token [redacted:FIXTURE_TOKEN]"),
        "stream log records are redacted: {logs:?}"
    );
    assert!(
        !logs.iter().any(|l| l.contains("tok-stream-77")),
        "{logs:?}"
    );
    assert!(
        logs.iter()
            .any(|l| l.contains(r#""config":{"room":"r1"}"#) && l.contains(r#""cursor":"cur-0""#)),
        "the first start gets the job's cursor: {logs:?}"
    );
    assert!(cursors.contains(&Some("cur-1".into())));
    // Nothing new: an empty, successful run, and the process is not restarted.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let out = src.run(input(cfg.clone(), None)).await.unwrap();
    assert!(out.items.is_empty());
    let starts =
        std::fs::read_to_string(env.paths.connector_state_dir("s").join("starts")).unwrap();
    assert_eq!(starts.trim(), "1");
    // A changed config restarts it, resuming from the newest cursor.
    let (keys, _, logs) = drain_until(src.as_ref(), &json!({"room": "r2"}), 1).await;
    assert_eq!(keys, vec!["start-2"]);
    assert!(
        logs.iter()
            .any(|l| l.contains(r#""config":{"room":"r2"}"#) && l.contains(r#""cursor":"cur-1""#)),
        "{logs:?}"
    );
    let logs = env.logs("s");
    assert!(logs.len() >= 2, "one log per start");
    drop(src);
}

#[tokio::test]
async fn a_crashing_stream_is_restarted_with_backoff() {
    let env = env_with(&["stream"]);
    env.dotenv("stream", "FIXTURE_MODE=crash\n");
    let src = StreamSource::with_backoff(
        env.connector("stream"),
        env.paths.clone(),
        Some("s".into()),
        Duration::from_millis(100),
    );
    let cfg = json!({});
    let (keys, _, logs) = drain_until(&src, &cfg, 3).await;
    assert!(
        logs.iter()
            .any(|l| l.contains("start 3 handshake") && l.contains(r#""cursor":"cur-2""#)),
        "a restart resumes from the newest cursor: {logs:?}"
    );
    assert_eq!(&keys[..3], &["start-1", "start-2", "start-3"]);
    // Down, nothing buffered: the run fails, naming why.
    let deadline = Instant::now() + WAIT;
    let err = loop {
        match src.run(input(cfg.clone(), None)).await {
            Err(e) => break e,
            Ok(_) => {
                assert!(Instant::now() < deadline);
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    };
    assert!(
        err.starts_with("stream connector stopped: exit 1: crashing on purpose"),
        "{err}"
    );
    let logs = env.logs("s");
    assert!(logs.iter().any(|l| l.contains("crashing on purpose")));
}

#[tokio::test]
async fn a_stream_whose_program_is_missing_fails_its_first_run() {
    let env = env_with(&[]);
    let dir = env.paths.connectors_dir().join("ghost");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("pastor-connector.toml"),
        "id = \"ghost\"\nversion = \"0.1.0\"\n[connector]\nmode = \"stream\"\ncommand = [\"./no-such-program\"]\n",
    )
    .unwrap();
    let src = process::source(env.connector("ghost"), env.paths.clone(), Some("g".into()));
    let err = src.run(input(json!({}), None)).await.unwrap_err();
    assert!(
        err.starts_with("stream connector stopped: could not start: ./no-such-program"),
        "{err}"
    );
}

#[tokio::test]
async fn a_stream_with_a_broken_env_file_fails_its_first_run() {
    let env = env_with(&["stream"]);
    env.dotenv("stream", "not an assignment\n");
    let src = process::source(env.connector("stream"), env.paths.clone(), Some("s".into()));
    let err = src.run(input(json!({}), None)).await.unwrap_err();
    assert!(err.contains(".env") && err.contains("line 1"), "{err}");
}

#[tokio::test]
async fn a_healthy_stream_first_run_succeeds() {
    let env = env_with(&["stream"]);
    let src = process::source(env.connector("stream"), env.paths.clone(), Some("s".into()));
    assert!(src.run(input(json!({}), None)).await.is_ok());
}

// ---- the `pastor connector` commands, through the binary ----

struct Cli {
    tmp: tempfile::TempDir,
}

impl Cli {
    fn new() -> Cli {
        Cli {
            tmp: tempfile::tempdir().unwrap(),
        }
    }

    fn dir(&self, d: &str) -> PathBuf {
        self.tmp.path().join(d)
    }

    fn pastor(&self, args: &[&str]) -> std::process::Output {
        std::process::Command::new(env!("CARGO_BIN_EXE_pastor"))
            .args(args)
            .env_remove("PASTOR_TASK")
            .env("PASTOR_CONFIG_DIR", self.dir("c"))
            .env("PASTOR_STATE_DIR", self.dir("s"))
            .env("PASTOR_DATA_DIR", self.dir("d"))
            .env(
                "PASTOR_CONNECTOR_GIT_BASE",
                format!("file://{}", self.dir("repos").display()),
            )
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> (String, String) {
        let out = self.pastor(args);
        let (o, e) = (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        );
        assert!(out.status.success(), "{args:?}\nstdout: {o}\nstderr: {e}");
        (o, e)
    }

    /// Exit 1 with a JSON error on stderr; returns its message.
    fn fails(&self, args: &[&str]) -> String {
        let out = self.pastor(args);
        assert_eq!(out.status.code(), Some(1), "{args:?} should fail");
        let stderr = String::from_utf8_lossy(&out.stderr);
        let last = stderr.lines().last().unwrap_or("");
        let err: serde_json::Value =
            serde_json::from_str(last).unwrap_or_else(|_| panic!("{stderr}"));
        assert_eq!(err["code"], "runtime_error", "{stderr}");
        err["message"].as_str().unwrap().to_string()
    }

    /// A git repo at repos/<owner>/<repo>.git holding the echo fixture under
    /// connectors/echo, with a tag v1 and a later commit that bumps the version.
    fn repo(&self, owner: &str, repo: &str) {
        let r = self.dir("repos").join(owner).join(format!("{repo}.git"));
        let sub = r.join("connectors/echo");
        std::fs::create_dir_all(&sub).unwrap();
        for f in ["pastor-connector.toml", "poll.sh", "hook.sh"] {
            std::fs::copy(fixture("echo").join(f), sub.join(f)).unwrap();
        }
        let git = |args: &[&str]| {
            let st = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "init.defaultBranch=main",
                    "-c",
                    "commit.gpgSign=false",
                    "-c",
                    "tag.gpgSign=false",
                ])
                .args(args)
                .current_dir(&r)
                .output()
                .unwrap();
            assert!(
                st.status.success(),
                "{}",
                String::from_utf8_lossy(&st.stderr)
            );
        };
        // Two symlinked subdirs: one to the connector inside the repo, one to a
        // connector outside it.
        std::os::unix::fs::symlink("echo", r.join("connectors/alias")).unwrap();
        let outside = self.dir("elsewhere/echo");
        std::fs::create_dir_all(&outside).unwrap();
        for f in ["pastor-connector.toml", "poll.sh", "hook.sh"] {
            std::fs::copy(fixture("echo").join(f), outside.join(f)).unwrap();
        }
        std::os::unix::fs::symlink(&outside, r.join("connectors/outside")).unwrap();
        git(&["init", "-q"]);
        git(&["add", "."]);
        git(&["commit", "-qm", "v1"]);
        git(&["tag", "v1"]);
        let m = sub.join("pastor-connector.toml");
        let text = std::fs::read_to_string(&m).unwrap();
        std::fs::write(
            &m,
            text.replace("\nversion = \"0.1.0\"", "\nversion = \"0.2.0\""),
        )
        .unwrap();
        git(&["commit", "-qam", "v2"]);
    }
}

/// A subdir that is a symlink installs what it points at inside the repo,
/// as a real directory that survives the clone's cleanup; one that points
/// outside the repo is refused.
#[test]
fn install_resolves_a_symlinked_subdir_and_refuses_one_outside_the_repo() {
    let cli = Cli::new();
    cli.repo("acme", "tools");
    let err = cli.fails(&[
        "connector",
        "install",
        "acme/tools/connectors/outside",
        "--yes",
    ]);
    assert!(err.contains("outside the repository"), "{err}");
    assert!(!cli.dir("d/connectors/echo").exists());

    let (out, _) = cli.ok(&[
        "connector",
        "install",
        "acme/tools/connectors/alias",
        "--yes",
    ]);
    assert!(out.contains("installed echo 0.2.0"), "{out}");
    let installed = cli.dir("d/connectors/echo");
    let md = std::fs::symlink_metadata(&installed).unwrap();
    assert!(md.is_dir(), "a directory, not a moved link");
    assert!(installed.join("pastor-connector.toml").exists());
    let (out, _) = cli.ok(&["connector", "list", "--json"]);
    let rows: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(rows[0]["error"], serde_json::Value::Null, "{out}");
    let (out, _) = cli.ok(&["connector", "run", "echo", "--job", "try"]);
    assert_eq!(
        out.lines().count(),
        3,
        "the installed connector runs: {out}"
    );
}

#[test]
fn install_list_and_uninstall_from_a_git_repo() {
    let cli = Cli::new();
    cli.repo("acme", "tools");
    let err = cli.fails(&["connector", "install", "acme/tools/connectors/echo"]);
    assert!(err.contains("pass --yes"), "no tty, no --yes: {err}");
    assert!(
        !cli.dir("d/connectors/echo").exists(),
        "nothing lands without confirmation"
    );

    let (out, err) = cli.ok(&[
        "connector",
        "install",
        "acme/tools/connectors/echo",
        "--ref",
        "v1",
        "--yes",
    ]);
    assert!(out.contains("installed echo 0.1.0"), "{out}");
    assert!(
        err.contains("connector (poll): sh poll.sh"),
        "shows what it runs: {err}"
    );
    assert!(err.contains("set FIXTURE_TOKEN in"), "{err}");
    let leftovers: Vec<_> = std::fs::read_dir(cli.dir("d/connectors"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(leftovers, vec!["echo"], "the scratch clone is gone");

    let err = cli.fails(&[
        "connector",
        "install",
        "acme/tools/connectors/echo",
        "--yes",
    ]);
    assert!(err.contains("already in"), "{err}");

    let (out, _) = cli.ok(&["connector", "list", "--json"]);
    let rows: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(rows[0]["id"], "echo");
    assert_eq!(rows[0]["version"], "0.1.0");
    assert_eq!(rows[0]["connector"], "poll");
    assert_eq!(rows[0]["linked"], false);
    assert_eq!(
        rows[0]["missing_secrets"],
        serde_json::json!(["FIXTURE_TOKEN"])
    );
    let (out, _) = cli.ok(&["connector", "list"]);
    assert!(out.starts_with("ID"), "{out}");
    assert!(out.contains("missing secrets: FIXTURE_TOKEN"), "{out}");

    let err = cli.fails(&["connector", "unlink", "echo"]);
    assert!(
        err.contains("use `pastor connector uninstall echo`"),
        "{err}"
    );
    cli.ok(&["connector", "uninstall", "echo"]);
    assert!(!cli.dir("d/connectors/echo").exists());
    let err = cli.fails(&["connector", "uninstall", "echo"]);
    assert!(err.contains("no connector \"echo\""), "{err}");

    // Without --ref: the branch tip.
    let (out, _) = cli.ok(&[
        "connector",
        "install",
        "acme/tools/connectors/echo",
        "--yes",
    ]);
    assert!(out.contains("installed echo 0.2.0"), "{out}");
    let err = cli.fails(&["connector", "install", "acme/missing", "--yes"]);
    assert!(err.contains("git clone"), "{err}");
}

/// The stream fixture, copied with a 1s timeout so `connector run` collects for
/// a second rather than the default minute, and linked.
fn link_quick_stream(cli: &Cli) {
    let dir = cli.dir("dev/stream");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(fixture("stream").join("stream.sh"), dir.join("stream.sh")).unwrap();
    let manifest =
        std::fs::read_to_string(fixture("stream").join("pastor-connector.toml")).unwrap();
    std::fs::write(
        dir.join("pastor-connector.toml"),
        manifest.replace("mode = \"stream\"", "mode = \"stream\"\ntimeout = \"1s\""),
    )
    .unwrap();
    cli.ok(&["connector", "link", dir.to_str().unwrap()]);
}

/// A stream is drained twice, at start and after its timeout; nothing is
/// acked in between, so each item must still print once.
#[test]
fn connector_run_collects_a_stream_and_prints_each_item_once() {
    let cli = Cli::new();
    link_quick_stream(&cli);
    let (out, err) = cli.ok(&["connector", "run", "stream", "--job", "try"]);
    assert_eq!(out, "{\"key\":\"start-1\"}\n", "{err}");
    assert!(err.contains("collecting for 1s"), "{err}");
    assert!(err.contains("1 items, cursor cur-1"), "{err}");
}

/// Without a daemon, `pastor tick` is a process that exits when the pass
/// does: a stream it started would die with it and take what it emitted
/// along. It refuses the job instead, and touches neither the stream nor
/// the job's state.
#[test]
fn a_standalone_tick_refuses_a_stream_job() {
    let cli = Cli::new();
    link_quick_stream(&cli);
    let jobs = cli.dir("c/jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    std::fs::write(
        jobs.join("live.toml"),
        "every = \"1m\"\n[connector]\nuse = \"stream\"\n[dispatch]\nprompt = \"p\"\n",
    )
    .unwrap();
    for args in [
        &["tick", "--json"][..],
        &["tick", "--job", "live", "--dry-run", "--json"],
    ] {
        let (out, _) = cli.ok(args);
        let runs: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(runs[0]["job"], "live", "{out}");
        assert_eq!(runs[0]["outcome"], "failed", "{out}");
        let err = runs[0]["error"].as_str().unwrap();
        assert!(
            err.contains("stream") && err.contains("pastor serve"),
            "{err}"
        );
    }
    assert!(
        !cli.dir("s/connectors/live/starts").exists(),
        "the stream was never started"
    );
    let (out, _) = cli.ok(&["job", "list", "--json"]);
    let jobs: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(jobs[0]["last_run_at"], serde_json::Value::Null, "{out}");
}

#[test]
fn link_run_and_unlink() {
    let cli = Cli::new();
    let (out, _) = cli.ok(&["connector", "link", fixture("echo").to_str().unwrap()]);
    assert!(out.contains("linked echo 0.1.0"), "{out}");
    let err = cli.fails(&["connector", "uninstall", "echo"]);
    assert!(err.contains("is linked"), "{err}");

    // No job file: an empty config, items on stdout as JSON lines.
    let (out, err) = cli.ok(&["connector", "run", "echo", "--job", "try"]);
    let items: Vec<serde_json::Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["key"], "k1");
    assert!(err.contains("no job file"), "{err}");
    assert!(err.contains("3 items, cursor c-2"), "{err}");
    assert!(cli.dir("s/runs/try").is_dir());

    // A job file that uses the connector: its config reaches the handshake.
    let jobs = cli.dir("c/jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    std::fs::write(
        jobs.join("support.toml"),
        "every = \"5m\"\n[connector]\nuse = \"echo\"\nchannel = \"C9\"\n[dispatch]\nprompt = \"{{ item.title }}\"\n",
    )
    .unwrap();
    let (_, err) = cli.ok(&[
        "connector",
        "run",
        "echo",
        "--job",
        "support",
        "--since",
        "1h",
    ]);
    assert!(err.contains(r#""config":{"channel":"C9"}"#), "{err}");
    // A job file missing the required key is invalid, and says why.
    std::fs::write(
        jobs.join("bare.toml"),
        "every = \"5m\"\n[connector]\nuse = \"echo\"\n[dispatch]\nprompt = \"p\"\n",
    )
    .unwrap();
    let err = cli.fails(&["connector", "run", "echo", "--job", "bare"]);
    assert!(err.contains("requires connector.channel"), "{err}");

    // A failing run is an error naming its log.
    let envf = cli.dir("c/connectors/echo/.env");
    std::fs::create_dir_all(envf.parent().unwrap()).unwrap();
    std::fs::write(&envf, "FIXTURE_MODE=fail\n").unwrap();
    let err = cli.fails(&["connector", "run", "echo", "--job", "try"]);
    assert!(err.contains("exit 4: failing on purpose"), "{err}");

    // The job name becomes paths under jobs/, runs/ and connectors/; one that
    // could walk out of them is refused before anything is touched.
    for bad in ["../x", "a/b", "Upper"] {
        let err = cli.fails(&["connector", "run", "echo", "--job", bad]);
        assert!(err.contains("must match"), "{bad}: {err}");
    }
    assert!(!cli.dir("s/x").exists() && !cli.dir("s/runs/../x").exists());
    assert!(!cli.dir("x").exists());
    let err = cli.fails(&["connector", "run", "nope", "--job", "try"]);
    assert!(err.contains("not available"), "{err}");
    cli.ok(&["connector", "unlink", "echo"]);
    assert!(
        fixture("echo").join("pastor-connector.toml").exists(),
        "the linked dir stays"
    );
    let (out, _) = cli.ok(&["connector", "list", "--json"]);
    assert_eq!(out.trim(), "[]");
}

/// A job on an installed connector: `pastor tick` (no daemon, so the pass
/// runs in the CLI) runs the fixture, dedups its items and queues one task
/// per new key, and the job's cursor is the fixture's last one.
#[test]
fn a_job_on_an_installed_connector_queues_tasks_through_tick() {
    let cli = Cli::new();
    cli.ok(&["connector", "link", fixture("echo").to_str().unwrap()]);
    let jobs = cli.dir("c/jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    std::fs::write(
        jobs.join("support.toml"),
        "every = \"1h\"\n[connector]\nuse = \"echo\"\nchannel = \"C9\"\n[dispatch]\nprompt = \"look at {{ item.title }}\"\nbranch = \"pastor/{{ item.key }}\"\nrepo = \"~/work\"\nworktree = true\n",
    )
    .unwrap();
    let (out, _) = cli.ok(&["job", "list", "--json"]);
    let jobs: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(jobs[0]["name"], "support");
    assert_eq!(
        jobs[0]["error"],
        serde_json::Value::Null,
        "valid against the connector: {out}"
    );
    assert_eq!(jobs[0]["connector"], "echo");
    let (out, _) = cli.ok(&["tick", "--json"]);
    let runs: serde_json::Value = serde_json::from_str(&out).unwrap();
    let run = &runs.as_array().unwrap()[0];
    assert_eq!(run["job"], "support");
    assert_eq!(run["outcome"], "ran", "{out}");
    assert_eq!(run["items"], 3);
    assert_eq!(
        run["created"].as_array().unwrap().len(),
        2,
        "k1 twice is one task"
    );

    let (out, _) = cli.ok(&["task", "list", "--job", "support", "--json"]);
    let tasks: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    let mut prompts: Vec<&str> = tasks
        .iter()
        .map(|t| t["prompt"].as_str().unwrap())
        .collect();
    prompts.sort();
    assert_eq!(prompts, vec!["look at first", "look at second"]);
    assert!(tasks.iter().all(|t| t["state"] == "queued"));
    assert!(tasks.iter().any(|t| t["spec"]["branch"] == "pastor/k1"));
    assert!(
        cli.dir("s/runs/support").is_dir(),
        "the run log is the job's"
    );

    // Seen keys are not queued again.
    let (out, _) = cli.ok(&["tick", "--job", "support", "--json"]);
    let runs: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(runs[0]["created"], serde_json::json!([]), "{out}");
    assert_eq!(runs[0]["skipped_seen"], 2, "{out}");
}

// ---- against a running daemon ----

/// `pastor serve` with one fake-herdr machine whose agents finish on their
/// own, and the connector env of `Cli`.
struct Serve {
    cli: Cli,
    children: Vec<std::process::Child>,
}

impl Drop for Serve {
    fn drop(&mut self) {
        for c in &mut self.children {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn serve() -> Serve {
    use std::process::{Command, Stdio};
    let cli = Cli::new();
    let config = cli.dir("c");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("pastor.toml"),
        "tick = \"1s\"\nsettle = \"1s\"\nreconcile_every = \"1s\"\n",
    )
    .unwrap();
    let socket = cli.dir("herdr.sock");
    let herdr = Command::new(env!("CARGO_BIN_EXE_fake-herdr"))
        .arg("--listen")
        .arg(&socket)
        .env("FAKE_HERDR_AUTO_DONE_MS", "300")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::fs::write(
        config.join("flock.toml"),
        format!(
            "[[machine]]\nname = \"fake\"\ncommand = [\"{}\", \"--connect\", \"{}\"]\nmax_agents = 4\n",
            env!("CARGO_BIN_EXE_fake-herdr"),
            socket.display()
        ),
    )
    .unwrap();
    let daemon = Command::new(env!("CARGO_BIN_EXE_pastor"))
        .arg("serve")
        .env_remove("PASTOR_TASK")
        .env("PASTOR_CONFIG_DIR", cli.dir("c"))
        .env("PASTOR_STATE_DIR", cli.dir("s"))
        .env("PASTOR_DATA_DIR", cli.dir("d"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let s = Serve {
        cli,
        children: vec![herdr, daemon],
    };
    wait(60, "the machine connects", || {
        let out = s.cli.pastor(&["machine", "list", "--json"]);
        String::from_utf8_lossy(&out.stdout).contains("\"connected\"")
    });
    s
}

fn wait(secs: u64, what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn records(path: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// The whole path: connectors linked while the daemon runs (link reloads it), a
/// job on the connector ticked through the daemon, its tasks
/// dispatched and done, and each connector's hooks hearing about it: `echo`
/// only about its own job's tasks, `notify` about every task, without their
/// item or prompt and in its own scratch dir.
#[test]
fn a_daemon_runs_connector_jobs_and_hooks_hear_their_events() {
    let s = serve();
    let cli = &s.cli;
    let (_, err) = cli.ok(&["connector", "link", fixture("echo").to_str().unwrap()]);
    assert!(err.contains("reloaded"), "{err}");
    cli.ok(&["connector", "link", fixture("notify").to_str().unwrap()]);
    let jobs = cli.dir("c/jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    std::fs::write(
        jobs.join("support.toml"),
        "every = \"1h\"\nenabled = false\n[connector]\nuse = \"echo\"\nchannel = \"C9\"\n[dispatch]\nrepo = \"/tmp\"\nprompt = \"look at {{ item.title }}\"\n",
    )
    .unwrap();
    let (out, _) = cli.ok(&["tick", "--job", "support", "--json"]);
    let runs: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(runs[0]["outcome"], "ran", "{out}");
    assert_eq!(runs[0]["created"].as_array().unwrap().len(), 2, "{out}");

    let echo = cli.dir("s/connectors/support/hook-records.jsonl");
    // notify does not own the job, so it writes to its own scratch.
    let notify = cli.dir("s/connectors/@notify/notify.jsonl");
    wait(60, "both tasks are done and every hook heard it", || {
        records(&echo)
            .iter()
            .filter(|r| r["type"] == "task.done")
            .count()
            == 2
            && records(&notify).len() == 2
    });
    let got = records(&echo);
    assert_eq!(
        got.iter().filter(|r| r["type"] == "task.queued").count(),
        2,
        "{got:?}"
    );
    for r in &got {
        assert_eq!(r["job"], "support");
        assert_eq!(r["task"]["job"], "support");
        assert!(r["at"].is_string());
    }
    let done = got.iter().find(|r| r["type"] == "task.done").unwrap();
    assert_eq!(done["task"]["state"], "done");
    assert_eq!(done["task"]["machine"], "fake");
    assert!(
        done["task"]["prompt"]
            .as_str()
            .unwrap()
            .starts_with("look at")
    );
    for r in records(&notify) {
        assert_eq!(r["job"], "support");
        assert_eq!(r["task"]["item"], serde_json::Value::Null, "{r}");
        assert_eq!(r["task"]["prompt"], "", "{r}");
    }
    assert!(!cli.dir("s/connectors/support/notify.jsonl").exists());

    // A one-off task is nobody's: notify hears of it, echo (only_own) not.
    cli.ok(&["task", "run", "one-off", "--repo", "/tmp"]);
    // `task run` is not a job, so the hook gets no PASTOR_JOB and its own scratch.
    wait(60, "notify hears the one-off task end", || {
        records(&notify).len() == 3
    });
    std::thread::sleep(Duration::from_millis(300));
    assert!(!cli.dir("s/connectors/@echo/hook-records.jsonl").exists());
    assert_eq!(records(&echo).len(), 4, "echo heard nothing more");
    assert_eq!(records(&notify)[2]["task"]["job"], "run");
}

/// A head that answers the first ping, then stops answering pings (a busy
/// head looks like this from outside) while it still takes a reload. It
/// records each request's op.
fn head_that_answers_one_ping(socket: &Path) -> Arc<std::sync::Mutex<Vec<String>>> {
    let listener = std::os::unix::net::UnixListener::bind(socket).unwrap();
    let ops = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen = ops.clone();
    std::thread::spawn(move || {
        use std::io::{BufRead, Write};
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut line = String::new();
            let _ = std::io::BufReader::new(&stream).read_line(&mut line);
            let req: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
            let op = req["op"].as_str().unwrap_or_default().to_string();
            let pings = {
                let mut seen = seen.lock().unwrap();
                seen.push(op.clone());
                seen.iter().filter(|o| *o == "ping").count()
            };
            let reply = match op.as_str() {
                "ping" if pings == 1 => json!({"kind": "pong", "data": {
                    "version": "test", "protocol": pastor::ipc::FLOCK_PROTOCOL}}),
                "reload" => json!({"kind": "jobs", "data": []}),
                _ => json!({"kind": "error", "data": {"code": "busy", "message": "busy"}}),
            };
            let _ = writeln!(stream, "{reply}");
        }
    });
    ops
}

/// The CLI pings the head once before a connector command; the reload after
/// the change acts on that ping. A head that stops answering pings after the
/// first still gets its reload, and is never probed a second time.
#[test]
fn connector_commands_reload_on_the_one_ping_the_cli_sent() {
    let cli = Cli::new();
    std::fs::create_dir_all(cli.dir("s")).unwrap();
    let ops = head_that_answers_one_ping(&cli.dir("s").join("pastor.sock"));

    let (_, err) = cli.ok(&["connector", "link", fixture("echo").to_str().unwrap()]);
    assert!(err.contains("reloaded its connectors"), "{err}");
    assert_eq!(*ops.lock().unwrap(), ["ping", "reload"]);

    // Every later ping goes unanswered, so the CLI's own check now stops the
    // command before anything changes.
    let out = cli.pastor(&["connector", "unlink", "echo"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("head_unresponsive"), "{stderr}");
    assert_eq!(*ops.lock().unwrap(), ["ping", "reload", "ping"]);
}
