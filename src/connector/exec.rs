//! Running a connector command: argv in the connector's directory, env from its
//! `.env` plus pastor's variables, one JSON object on stdin, stdout handed
//! back line by line, stderr captured to a run log. Shared by connectors and,
//! later, event hooks.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::env::Redactor;
use crate::config::create_private_dir;

/// One run log is cut here; a connector that floods stderr must not fill the
/// disk.
pub const LOG_MAX_BYTES: u64 = 256 * 1024;
/// Logs kept per job; older ones are deleted when a new one is created.
pub const LOG_KEEP: usize = 20;
/// Lines of stderr kept for the error message of a failed run.
const STDERR_TAIL: usize = 5;
/// The longest stdout or stderr line pastor holds in memory; the rest of a
/// longer line is read and dropped. The same as the run log cap, since no
/// longer line could be logged whole anyway, and a connector that writes
/// without newlines must not grow the daemon without bound.
pub const LINE_MAX_BYTES: usize = 256 * 1024;

/// `<runs>/<job>/<ts>.log`: what one run wrote to stderr, what pastor had to
/// say about its stdout, and how it ended. Every line goes through the
/// connector's `Redactor` first.
pub struct RunLog {
    path: PathBuf,
    file: std::fs::File,
    written: u64,
    max: u64,
    truncated: bool,
    redactor: Redactor,
}

pub type SharedLog = Arc<Mutex<RunLog>>;

impl RunLog {
    /// Create a new log in `dir` (made 0700) and prune the oldest beyond
    /// `LOG_KEEP`.
    pub fn create(dir: &Path, redactor: Redactor) -> anyhow::Result<RunLog> {
        RunLog::create_with(dir, redactor, LOG_MAX_BYTES, LOG_KEEP)
    }

    pub fn create_with(
        dir: &Path,
        redactor: Redactor,
        max: u64,
        keep: usize,
    ) -> anyhow::Result<RunLog> {
        create_private_dir(dir)?;
        let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ").to_string();
        let (path, file) = open_unique(dir, &stamp)?;
        prune(dir, keep)?;
        Ok(RunLog {
            path,
            file,
            written: 0,
            max,
            truncated: false,
            redactor,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn shared(self) -> SharedLog {
        Arc::new(Mutex::new(self))
    }

    /// Append one line, redacted. Past the cap, one marker line and then
    /// nothing; a write error is logged once and otherwise ignored, since a
    /// full disk must not fail the run it is recording.
    pub fn line(&mut self, text: &str) {
        use std::io::Write;
        if self.truncated {
            return;
        }
        let text = self.redactor.redact(text);
        let len = text.len() as u64 + 1;
        let result = if self.written + len > self.max {
            self.truncated = true;
            writeln!(self.file, "[pastor: log truncated at {} bytes]", self.max)
        } else {
            self.written += len;
            writeln!(self.file, "{text}")
        };
        if let Err(e) = result {
            tracing::warn!(log = %self.path.display(), %e, "run log write failed");
            self.truncated = true;
        }
    }

    pub fn redact(&self, text: &str) -> String {
        self.redactor.redact(text)
    }
}

/// Create `<stamp>.log`, or `<stamp>-1.log`, `-2`, ... if it is taken. Two
/// runs of one job in the same millisecond (a stream restarting at once)
/// race for the name; `create_new` makes claiming it atomic, so the loser
/// takes the next suffix instead of failing.
fn open_unique(dir: &Path, stamp: &str) -> anyhow::Result<(PathBuf, std::fs::File)> {
    for n in 0u32.. {
        let path = match n {
            0 => dir.join(format!("{stamp}.log")),
            n => dir.join(format!("{stamp}-{n}.log")),
        };
        match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("create {}", path.display())),
        }
    }
    unreachable!("u32 suffixes run out before a directory does")
}

/// Keep the newest `keep` `*.log` files in `dir`. Names sort by time.
pub fn prune(dir: &Path, keep: usize) -> anyhow::Result<()> {
    let mut logs: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("read {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("log"))
        .collect();
    if logs.len() <= keep {
        return Ok(());
    }
    logs.sort();
    for old in &logs[..logs.len() - keep] {
        if let Err(e) = std::fs::remove_file(old) {
            tracing::warn!(log = %old.display(), %e, "prune run log");
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Invocation {
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    /// Added to pastor's own environment, in order; later entries win.
    pub env: Vec<(String, String)>,
    pub stdin: Vec<u8>,
    /// `None` runs until the command exits (stream connectors).
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    /// Killed by a signal pastor did not send.
    Signal(i32),
    TimedOut(Duration),
    SpawnFailed(String),
}

impl Exit {
    pub fn success(&self) -> bool {
        *self == Exit::Code(0)
    }
}

impl std::fmt::Display for Exit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Exit::Code(c) => write!(f, "exit {c}"),
            Exit::Signal(s) => write!(f, "killed by signal {s}"),
            Exit::TimedOut(d) => write!(f, "timed out after {}s", d.as_secs()),
            Exit::SpawnFailed(e) => write!(f, "could not start: {e}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Finished {
    pub exit: Exit,
    /// The last few stderr lines, redacted, for an error message.
    pub stderr_tail: Vec<String>,
}

impl Finished {
    /// `exit 3: <last stderr line>`, the one-line reason a failed run reports.
    pub fn reason(&self) -> String {
        match self.stderr_tail.last() {
            Some(l) => format!("{}: {l}", self.exit),
            None => self.exit.to_string(),
        }
    }
}

/// Read one line into `buf`, without its newline, keeping at most `max`
/// bytes and consuming the rest. `None` at EOF; otherwise whether the line
/// was cut. A last line with no newline still counts, as with `split`.
async fn read_line_capped<R: AsyncBufRead + Unpin>(
    r: &mut R,
    buf: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<Option<bool>> {
    buf.clear();
    let mut cut = false;
    let mut any = false;
    loop {
        let chunk = r.fill_buf().await?;
        if chunk.is_empty() {
            return Ok(any.then_some(cut));
        }
        any = true;
        let (line, used, done) = match chunk.iter().position(|&b| b == b'\n') {
            Some(i) => (&chunk[..i], i + 1, true),
            None => (chunk, chunk.len(), false),
        };
        let room = max.saturating_sub(buf.len());
        if line.len() > room {
            cut = true;
        }
        buf.extend_from_slice(&line[..line.len().min(room)]);
        r.consume(used);
        if done {
            return Ok(Some(cut));
        }
    }
}

/// Kills the command's whole process group if the run is abandoned (timeout,
/// or the future dropped): a shell script's children would otherwise keep
/// stdout open and outlive it. Disarmed once the command has exited on its
/// own, so a reused process group id is never signalled.
struct GroupGuard(Option<i32>);

impl GroupGuard {
    fn kill(&mut self) {
        if let Some(pgid) = self.0.take() {
            // SAFETY: killpg only sends a signal; the group was created by
            // `process_group(0)` for this child and its leader not yet reaped.
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
        }
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Run `inv`, calling `on_line` for each stdout line as it arrives. Never
/// fails: a command that cannot start is an `Exit::SpawnFailed`. The end of
/// the run is written to `log` too.
pub async fn run(inv: Invocation, log: SharedLog, on_line: impl FnMut(&str)) -> Finished {
    run_started(inv, log, || {}, on_line).await
}

/// `run`, calling `on_spawn` once the command has started: a stream's
/// supervisor reports it running then, and not a moment before.
pub async fn run_started(
    inv: Invocation,
    log: SharedLog,
    on_spawn: impl FnOnce(),
    mut on_line: impl FnMut(&str),
) -> Finished {
    let Some(program) = inv.argv.first() else {
        return Finished {
            exit: Exit::SpawnFailed("empty command".into()),
            stderr_tail: Vec::new(),
        };
    };
    // A relative path with a slash (`./poll`, `bin/poll`) means the connector's
    // file, whatever pastor's own cwd is.
    let program = if program.contains('/') && Path::new(program).is_relative() {
        inv.cwd.join(program).into_os_string()
    } else {
        program.into()
    };
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(&inv.argv[1..])
        .current_dir(&inv.cwd)
        .envs(inv.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let exit = Exit::SpawnFailed(format!("{}: {e}", inv.argv[0]));
            lock(&log).line(&format!("[pastor: {exit}]"));
            return Finished {
                exit,
                stderr_tail: Vec::new(),
            };
        }
    };
    let mut guard = GroupGuard(child.id().map(|p| p as i32));
    on_spawn();

    let mut stdin = child.stdin.take().expect("piped");
    let input = inv.stdin;
    let writer = tokio::spawn(async move {
        // A command that never reads stdin closes it; that is its business.
        let _ = stdin.write_all(&input).await;
        let _ = stdin.shutdown().await;
    });
    let stderr = child.stderr.take().expect("piped");
    let err_log = log.clone();
    let mut stderr_task = tokio::spawn(async move {
        let mut tail: Vec<String> = Vec::new();
        let mut reader = BufReader::new(stderr);
        let mut raw = Vec::new();
        while let Ok(Some(cut)) = read_line_capped(&mut reader, &mut raw, LINE_MAX_BYTES).await {
            let mut line = String::from_utf8_lossy(&raw).into_owned();
            if cut {
                line.push_str(&format!(" [pastor: line cut at {LINE_MAX_BYTES} bytes]"));
            }
            let mut log = lock(&err_log);
            log.line(&line);
            if tail.len() == STDERR_TAIL {
                tail.remove(0);
            }
            tail.push(log.redact(&line));
        }
        tail
    });
    let stdout = child.stdout.take().expect("piped");

    let body = async {
        let mut reader = BufReader::new(stdout);
        let mut raw = Vec::new();
        let mut n = 0usize;
        while let Ok(Some(cut)) = read_line_capped(&mut reader, &mut raw, LINE_MAX_BYTES).await {
            n += 1;
            if cut {
                // The cut line still goes to `on_line`, where it fails to
                // parse and is skipped with a note of its own.
                lock(&log).line(&format!(
                    "[pastor: stdout line {n} cut at {LINE_MAX_BYTES} bytes]"
                ));
            }
            on_line(&String::from_utf8_lossy(&raw));
        }
        child.wait().await
    };
    let status = match inv.timeout {
        Some(t) => tokio::time::timeout(t, body).await.map_err(|_| t),
        None => Ok(body.await),
    };
    let exit = match status {
        Ok(Ok(s)) => {
            guard.0 = None;
            use std::os::unix::process::ExitStatusExt;
            match (s.code(), s.signal()) {
                (Some(c), _) => Exit::Code(c),
                (None, Some(sig)) => Exit::Signal(sig),
                (None, None) => Exit::Code(-1),
            }
        }
        Ok(Err(e)) => Exit::SpawnFailed(format!("wait: {e}")),
        Err(t) => {
            guard.kill();
            let _ = child.kill().await;
            Exit::TimedOut(t)
        }
    };
    writer.abort();
    // The group is dead or exited; stderr reaches EOF promptly unless a
    // detached grandchild still holds it, which is not worth waiting for.
    let stderr_tail = match tokio::time::timeout(Duration::from_secs(2), &mut stderr_task).await {
        Ok(Ok(tail)) => tail,
        _ => {
            stderr_task.abort();
            Vec::new()
        }
    };
    lock(&log).line(&format!("[pastor: {exit}]"));
    Finished { exit, stderr_tail }
}

fn lock(log: &SharedLog) -> std::sync::MutexGuard<'_, RunLog> {
    log.lock().unwrap_or_else(|p| p.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str, timeout: Option<Duration>) -> Invocation {
        Invocation {
            argv: vec!["sh".into(), "-c".into(), script.into()],
            cwd: std::env::temp_dir(),
            env: vec![("GREETING".into(), "hello".into())],
            stdin: b"{\"in\":1}".to_vec(),
            timeout,
        }
    }

    fn log_in(dir: &Path) -> SharedLog {
        RunLog::create(dir, Redactor::default()).unwrap().shared()
    }

    fn text(log: &SharedLog) -> String {
        std::fs::read_to_string(lock(log).path()).unwrap()
    }

    #[tokio::test]
    async fn stdout_lines_stdin_env_stderr_and_exit_code() {
        let tmp = tempfile::tempdir().unwrap();
        let log = log_in(tmp.path());
        let mut lines = Vec::new();
        let done = run(
            sh(
                "read -r x; echo \"got $x\"; echo \"$GREETING\"; echo oops >&2; exit 3",
                Some(Duration::from_secs(10)),
            ),
            log.clone(),
            |l| lines.push(l.to_string()),
        )
        .await;
        assert_eq!(lines, vec!["got {\"in\":1}", "hello"]);
        assert_eq!(done.exit, Exit::Code(3));
        assert_eq!(done.stderr_tail, vec!["oops"]);
        assert_eq!(done.reason(), "exit 3: oops");
        assert_eq!(text(&log), "oops\n[pastor: exit 3]\n");
    }

    #[tokio::test]
    async fn a_timeout_kills_the_whole_group() {
        let tmp = tempfile::tempdir().unwrap();
        let log = log_in(tmp.path());
        let started = std::time::Instant::now();
        // The background sleep holds stdout open; only killing the group
        // lets the run end at the timeout.
        let done = run(
            sh(
                "sleep 30 & echo early; wait",
                Some(Duration::from_millis(300)),
            ),
            log.clone(),
            |_| {},
        )
        .await;
        assert_eq!(done.exit, Exit::TimedOut(Duration::from_millis(300)));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        assert!(text(&log).contains("timed out"));
    }

    #[tokio::test]
    async fn a_missing_program_is_a_spawn_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let log = log_in(tmp.path());
        let mut inv = sh("", None);
        inv.argv = vec!["./no-such-program".into()];
        let done = run(inv, log.clone(), |_| {}).await;
        assert!(matches!(&done.exit, Exit::SpawnFailed(e) if e.contains("no-such-program")));
        assert!(!done.exit.success());
    }

    #[tokio::test]
    async fn a_line_without_a_newline_is_held_to_the_cap() {
        let mut input = vec![b'x'; 4 * 1024 * 1024];
        input.extend_from_slice(b"\nnext\nlast");
        let mut reader = BufReader::new(&input[..]);
        let mut buf = Vec::new();
        let cut = read_line_capped(&mut reader, &mut buf, LINE_MAX_BYTES)
            .await
            .unwrap();
        assert_eq!(cut, Some(true));
        assert_eq!(buf.len(), LINE_MAX_BYTES);
        assert!(buf.capacity() <= 2 * LINE_MAX_BYTES, "{}", buf.capacity());
        let mut rest = Vec::new();
        while let Some(cut) = read_line_capped(&mut reader, &mut buf, LINE_MAX_BYTES)
            .await
            .unwrap()
        {
            assert!(!cut);
            rest.push(String::from_utf8(buf.clone()).unwrap());
        }
        assert_eq!(rest, vec!["next", "last"]);
    }

    #[tokio::test]
    async fn megabytes_without_a_newline_do_not_lose_the_rest_of_the_run() {
        let tmp = tempfile::tempdir().unwrap();
        let log = log_in(tmp.path());
        let mut lines = Vec::new();
        let done = run(
            sh(
                "head -c 4194304 /dev/zero | tr '\\0' x >&2; echo >&2; echo after >&2; \
                 head -c 4194304 /dev/zero | tr '\\0' y; echo; echo '{\"ok\":1}'",
                Some(Duration::from_secs(20)),
            ),
            log.clone(),
            |l| lines.push(l.to_string()),
        )
        .await;
        assert_eq!(done.exit, Exit::Code(0));
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].len(), LINE_MAX_BYTES, "the long line is cut");
        assert_eq!(lines[1], "{\"ok\":1}");
        assert_eq!(done.stderr_tail.last().map(String::as_str), Some("after"));
        let body = text(&log);
        // The cut stderr line alone reaches the log's cap.
        assert!(body.contains("[pastor: log truncated at"), "{}", body.len());
        assert!(body.len() as u64 <= LOG_MAX_BYTES + 1024, "{}", body.len());
    }

    #[test]
    fn a_taken_log_name_gets_the_next_suffix() {
        let tmp = tempfile::tempdir().unwrap();
        let stamp = "20260924T120000.000Z";
        std::fs::write(tmp.path().join(format!("{stamp}.log")), "first").unwrap();
        std::fs::write(tmp.path().join(format!("{stamp}-1.log")), "second").unwrap();
        let (path, _file) = open_unique(tmp.path(), stamp).unwrap();
        assert_eq!(path, tmp.path().join(format!("{stamp}-2.log")));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(format!("{stamp}.log"))).unwrap(),
            "first",
            "an existing log is never reopened"
        );
    }

    #[test]
    fn the_log_is_redacted_capped_and_pruned() {
        let tmp = tempfile::tempdir().unwrap();
        let r = Redactor::new(["TOKEN"], &[("TOKEN".into(), "s3cr3t".into())]);
        let mut log = RunLog::create_with(tmp.path(), r, 40, 3).unwrap();
        log.line("token is s3cr3t");
        log.line("0123456789012345678901234567890");
        log.line("never written");
        let body = std::fs::read_to_string(log.path()).unwrap();
        assert_eq!(
            body,
            "token is [redacted:TOKEN]\n[pastor: log truncated at 40 bytes]\n"
        );
        for _ in 0..5 {
            RunLog::create_with(tmp.path(), Redactor::default(), 40, 3).unwrap();
        }
        let n = std::fs::read_dir(tmp.path()).unwrap().count();
        assert_eq!(n, 3, "only the newest three are kept");
    }
}
