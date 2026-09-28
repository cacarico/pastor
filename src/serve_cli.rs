//! `pastor serve` in the background, `serve stop` and `serve status`.
//!
//! A bare `pastor serve` starts `pastor serve --foreground` as its own
//! session, with `LOG_ENV` naming `serve.log` for its output, and returns once that head answers a
//! ping. One started by a service manager (systemd, launchd) stays in the
//! foreground instead, so a unit written before `--foreground` existed keeps
//! working. The head records its pid, its service manager and its log in
//! `serve.json` at start; `status` and `stop` find the running one by the
//! pid holding the socket and trust the record only when the two agree.

use std::io::{Read, Seek, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::cli::CliError;
use crate::config::Paths;
use crate::ipc::{HeadPing, SHEPHERD_ROLE, ping_head};

/// `serve.log` is rotated once it would pass this size.
pub const LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;
/// Rotated logs kept: `serve.log.1` (newest) to `serve.log.3`.
pub const LOG_KEEP: usize = 3;
/// How long a background `pastor serve` waits for its head to answer.
const START_WAIT: Duration = Duration::from_secs(30);
/// How long `serve stop` waits for the head to exit after SIGTERM.
const STOP_WAIT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(100);
/// Set by a background `pastor serve` for the `serve --foreground` it
/// starts: the file to log to. An environment variable rather than a flag,
/// so it stays out of `--help` and the completions; `main` takes it out of
/// the environment again at once.
pub const LOG_ENV: &str = "PASTOR_SERVE_LOG";

#[derive(clap::Subcommand, Debug)]
pub enum ServeCmd {
    /// Stop the pastor serve running here (SIGTERM); agents keep running
    Stop,
    /// Whether pastor serve runs here: head or headless, pid, version, service, log
    Status {
        /// Print as a JSON object
        #[arg(long)]
        json: bool,
    },
}

/// What a starting head writes to `serve.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub pid: u32,
    /// `service_manager()` at start.
    pub service: Option<String>,
    /// `serve.log` for a background head; none when it logs to stderr.
    pub log: Option<PathBuf>,
}

/// The service manager that started this process, from its parent: a
/// `systemd` manager (the user one, or pid 1), launchd (pid 1 on macOS), or
/// some other pid 1 (`init`, a container's). Not `INVOCATION_ID`: every
/// process under a systemd unit inherits it, a shell in a herdr pane
/// included.
pub fn service_manager() -> Option<&'static str> {
    let ppid = unsafe { libc::getppid() } as u32;
    let comm = std::fs::read_to_string(format!("/proc/{ppid}/comm")).ok();
    classify(
        ppid,
        comm.as_deref().map(str::trim),
        cfg!(target_os = "macos"),
    )
}

/// `service_manager` for a parent pid and its command name.
pub fn classify(ppid: u32, comm: Option<&str>, macos: bool) -> Option<&'static str> {
    if macos {
        return (ppid == 1).then_some("launchd");
    }
    match comm {
        Some("systemd") => Some("systemd"),
        _ if ppid == 1 => Some("init"),
        _ => None,
    }
}

/// For a head about to run in this process: refuse a socket another head
/// holds, then write `serve.json`. `log` is the background head's log.
pub async fn record_start(
    paths: &Paths,
    service: Option<&str>,
    log: Option<&Path>,
) -> anyhow::Result<()> {
    paths.ensure()?;
    crate::daemon::refuse_live_socket(&paths.socket_file()).await?;
    let record = Record {
        pid: std::process::id(),
        service: service.map(str::to_string),
        log: log.map(Path::to_path_buf),
    };
    let text = serde_json::to_string(&record)?;
    std::fs::write(paths.serve_record_file(), text + "\n")
        .with_context(|| format!("write {}", paths.serve_record_file().display()))
}

/// `serve.json` when it describes `pid`: a record left by a head that has
/// since exited, or by one an older pastor started (no record), is none.
fn record_for(paths: &Paths, pid: Option<u32>) -> Option<Record> {
    let text = std::fs::read_to_string(paths.serve_record_file()).ok()?;
    let record: Record = serde_json::from_str(&text).ok()?;
    (Some(record.pid) == pid).then_some(record)
}

/// The pid of the process that listens on `socket`, from the connection's
/// peer credentials; none when nothing does or the platform cannot say.
async fn socket_pid(socket: &Path) -> Option<u32> {
    let stream = tokio::net::UnixStream::connect(socket).await.ok()?;
    let pid = stream.peer_cred().ok()?.pid()?;
    u32::try_from(pid).ok()
}

/// Whether `pid` still runs. A zombie (exited, its parent has not reaped
/// it yet) does not: on Linux its `/proc` state says so.
fn alive(pid: u32) -> bool {
    if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
        return false;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    // The state follows the command name, which is in parentheses and may
    // hold spaces or parentheses itself.
    let state = stat
        .rsplit_once(") ")
        .and_then(|(_, rest)| rest.chars().next());
    state != Some('Z')
}

fn role_name(role: Option<&str>) -> &'static str {
    match role {
        Some(r) if r == SHEPHERD_ROLE => "headless",
        _ => "head",
    }
}

/// Bare `pastor serve` outside a service: start the head in its own session
/// with `--foreground` and `LOG_ENV`, and return once it answers. `head` is the
/// `--head` this command was given, passed on; the environment and
/// client.toml reach the child as they are.
pub async fn start_background(paths: &Paths, head: Option<&str>) -> anyhow::Result<()> {
    paths.ensure()?;
    crate::daemon::refuse_live_socket(&paths.socket_file()).await?;
    let log = paths.serve_log_file();
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log)
        .with_context(|| format!("open {}", log.display()))?;
    let from = file.metadata().map(|m| m.len()).unwrap_or(0);
    let exe = std::env::current_exe().context("locate the pastor binary")?;
    let mut cmd = std::process::Command::new(exe);
    if let Some(head) = head {
        cmd.args(["--head", head]);
    }
    cmd.args(["serve", "--foreground"])
        .env(LOG_ENV, &log)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(file);
    // Its own session: no controlling terminal, so closing this one does not
    // send it SIGHUP, and ctrl-c here does not reach it.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("start pastor serve")?;
    let pid = child.id();
    let deadline = Instant::now() + START_WAIT;
    loop {
        if let Some(status) = child.try_wait()? {
            return Err(CliError::err(
                "serve_failed",
                format!(
                    "pastor serve exited before it answered ({status}): {}; its log is {}",
                    last_error(&log, from),
                    log.display()
                ),
            ));
        }
        let socket = paths.socket_file();
        if let HeadPing::Pong { role, .. } = ping_head(&socket).await
            && socket_pid(&socket).await == Some(pid)
        {
            println!(
                "pastor serve is running in the background as the {} (pid {pid})",
                role_name(role.as_deref())
            );
            println!("log: {}", log.display());
            println!("stop it with `pastor serve stop`");
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(CliError::err(
                "serve_slow",
                format!(
                    "pastor serve (pid {pid}) has not answered in {}s; it may still be starting: see `pastor serve status` and {}",
                    START_WAIT.as_secs(),
                    log.display()
                ),
            ));
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Why a head that exited did: the message of the last error line it wrote
/// to `log` after byte `from`, or its last line.
fn last_error(log: &Path, from: u64) -> String {
    let mut text = String::new();
    if let Ok(mut f) = std::fs::File::open(log) {
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        // A rotation at start leaves a shorter file: read it all.
        let _ = f.seek(std::io::SeekFrom::Start(if len >= from { from } else { 0 }));
        let _ = f.read_to_string(&mut text);
    }
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines
        .iter()
        .rev()
        .find_map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).ok()?;
            v.get("code")?;
            Some(v.get("message")?.as_str()?.to_string())
        })
        .or_else(|| lines.last().map(|l| l.to_string()))
        .unwrap_or_else(|| "it wrote nothing".into())
}

/// `pastor serve status`.
pub async fn status(paths: &Paths, json: bool) -> anyhow::Result<()> {
    let socket = paths.socket_file();
    let (version, protocol, role) = match ping_head(&socket).await {
        HeadPing::NotRunning => {
            return Err(CliError::err(
                "not_running",
                "pastor serve is not running here; start it with `pastor serve`",
            ));
        }
        HeadPing::Unresponsive => {
            let pid = socket_pid(&socket).await;
            return Err(CliError::err(
                "head_unresponsive",
                format!(
                    "something holds {}{} but did not answer a ping within 2s",
                    socket.display(),
                    pid.map(|p| format!(" (pid {p})")).unwrap_or_default()
                ),
            ));
        }
        HeadPing::Pong {
            version,
            protocol,
            role,
        } => (version, protocol, role),
    };
    let pid = socket_pid(&socket).await;
    let record = record_for(paths, pid);
    let role = role_name(role.as_deref());
    let service = record.as_ref().and_then(|r| r.service.clone());
    let log = record.as_ref().and_then(|r| r.log.clone());
    if json {
        println!(
            "{}",
            serde_json::json!({
                "running": true,
                "role": role,
                "pid": pid,
                "version": version,
                "protocol": protocol,
                "service": service,
                "log": log,
                "socket": socket,
            })
        );
        return Ok(());
    }
    let pid = pid.map(|p| format!("pid {p}, ")).unwrap_or_default();
    println!("pastor serve is running as the {role} ({pid}version {version})");
    match (&record, &service, &log) {
        (None, ..) => println!("service: unknown (started by an older pastor)"),
        (_, Some(s), _) => println!("service: {s}"),
        (_, None, Some(log)) => println!("in the background; log: {}", log.display()),
        (_, None, None) => println!("in the foreground, logging to its terminal"),
    }
    println!("socket: {}", socket.display());
    Ok(())
}

/// `pastor serve stop`: SIGTERM to the process holding the socket, then wait
/// for it to go. One a service manager runs is refused: systemd and launchd
/// would start it again, so it is stopped through them.
pub async fn stop(paths: &Paths) -> anyhow::Result<()> {
    let socket = paths.socket_file();
    if ping_head(&socket).await == HeadPing::NotRunning {
        println!("pastor serve is not running here; nothing to stop");
        return Ok(());
    }
    let Some(pid) = socket_pid(&socket).await else {
        return Err(CliError::err(
            "stop_failed",
            format!("could not tell which process holds {}", socket.display()),
        ));
    };
    if let Some(service) = record_for(paths, Some(pid)).and_then(|r| r.service) {
        let how = match service.as_str() {
            "systemd" => "`pastor setup systemd --stop`",
            "launchd" => "`pastor setup launchd --stop`",
            _ => "whatever started it",
        };
        return Err(CliError::err(
            "service_managed",
            format!(
                "pastor serve (pid {pid}) runs under {service}, which would start it again; stop it with {how}"
            ),
        ));
    }
    if unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) } != 0 {
        return Err(CliError::err(
            "stop_failed",
            format!(
                "could not signal pastor serve (pid {pid}): {}",
                std::io::Error::last_os_error()
            ),
        ));
    }
    let deadline = Instant::now() + STOP_WAIT;
    while alive(pid) {
        if Instant::now() >= deadline {
            return Err(CliError::err(
                "stop_timeout",
                format!(
                    "pastor serve (pid {pid}) is still running {}s after SIGTERM",
                    STOP_WAIT.as_secs()
                ),
            ));
        }
        tokio::time::sleep(POLL).await;
    }
    println!("stopped pastor serve (pid {pid}); agents keep running");
    Ok(())
}

/// A background head's log: appends each write to `path`, and once a write
/// would take it past `max` bytes, moves it to `path.1` (older ones up to
/// `path.<keep>`) and starts a new one. With `own_stderr`, file descriptor 2
/// follows the current file too, so a panic lands in the log.
pub struct RotatingLog {
    path: PathBuf,
    max: u64,
    keep: usize,
    own_stderr: bool,
    state: Mutex<(std::fs::File, u64)>,
}

impl RotatingLog {
    pub fn open(path: PathBuf, max: u64, keep: usize, own_stderr: bool) -> std::io::Result<Self> {
        let file = open_log(&path)?;
        let size = file.metadata()?.len();
        if own_stderr {
            redirect_stderr(&file);
        }
        Ok(RotatingLog {
            path,
            max,
            keep,
            own_stderr,
            state: Mutex::new((file, size)),
        })
    }

    fn append(&self, buf: &[u8]) -> std::io::Result<()> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.1 > 0 && state.1 + buf.len() as u64 > self.max {
            self.rotate()?;
            let file = open_log(&self.path)?;
            if self.own_stderr {
                redirect_stderr(&file);
            }
            *state = (file, 0);
        }
        state.0.write_all(buf)?;
        state.1 += buf.len() as u64;
        Ok(())
    }

    fn rotate(&self) -> std::io::Result<()> {
        for n in (1..self.keep).rev() {
            match std::fs::rename(numbered(&self.path, n), numbered(&self.path, n + 1)) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        if self.keep == 0 {
            return std::fs::remove_file(&self.path);
        }
        std::fs::rename(&self.path, numbered(&self.path, 1))
    }
}

fn numbered(path: &Path, n: usize) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(format!(".{n}"));
    PathBuf::from(p)
}

fn open_log(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

fn redirect_stderr(file: &std::fs::File) {
    use std::os::fd::AsRawFd;
    unsafe { libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO) };
}

/// One tracing event per `write`: the fmt layer formats an event into one
/// buffer and writes it whole, so a line is never split across files.
pub struct LogWriter<'a>(&'a RotatingLog);

impl Write for LogWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.append(buf)?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for RotatingLog {
    type Writer = LogWriter<'a>;
    fn make_writer(&'a self) -> LogWriter<'a> {
        LogWriter(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_parent_systemd_or_pid_1_is_a_service() {
        assert_eq!(classify(812, Some("systemd"), false), Some("systemd"));
        assert_eq!(classify(1, Some("systemd"), false), Some("systemd"));
        assert_eq!(classify(1, Some("tini"), false), Some("init"));
        assert_eq!(classify(1, None, true), Some("launchd"));
        // A shell, herdr, a test harness: not a service, whatever it inherited.
        assert_eq!(classify(4242, Some("fish"), false), None);
        assert_eq!(classify(4242, Some("herdr"), false), None);
        assert_eq!(classify(4242, None, true), None);
    }

    fn write(log: &RotatingLog, line: &str) {
        LogWriter(log).write_all(line.as_bytes()).unwrap();
    }

    #[test]
    fn the_log_rotates_by_size_and_keeps_three() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("serve.log");
        let log = RotatingLog::open(path.clone(), 10, 3, false).unwrap();
        let read = |p: PathBuf| std::fs::read_to_string(p).unwrap_or_default();
        write(&log, "aaaaaaa\n");
        assert_eq!(read(path.clone()), "aaaaaaa\n");
        // 8 + 8 > 10: the first file moves to .1, whole.
        write(&log, "bbbbbbb\n");
        assert_eq!(read(numbered(&path, 1)), "aaaaaaa\n");
        assert_eq!(read(path.clone()), "bbbbbbb\n");
        for line in ["ccccccc\n", "ddddddd\n", "eeeeeee\n"] {
            write(&log, line);
        }
        assert_eq!(read(path.clone()), "eeeeeee\n");
        assert_eq!(read(numbered(&path, 1)), "ddddddd\n");
        assert_eq!(read(numbered(&path, 2)), "ccccccc\n");
        assert_eq!(read(numbered(&path, 3)), "bbbbbbb\n");
        assert!(!numbered(&path, 4).exists());
        // One line bigger than the limit still goes in, into a file of its own.
        write(&log, "a line longer than ten bytes\n");
        assert_eq!(read(path.clone()), "a line longer than ten bytes\n");
    }

    #[test]
    fn the_log_picks_up_an_existing_file_at_its_size() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("serve.log");
        std::fs::write(&path, "0123456789").unwrap();
        let log = RotatingLog::open(path.clone(), 10, 3, false).unwrap();
        write(&log, "x\n");
        assert_eq!(
            std::fs::read_to_string(numbered(&path, 1)).unwrap(),
            "0123456789"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x\n");
    }

    #[test]
    fn last_error_reads_the_heads_error_line() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("serve.log");
        std::fs::write(
            &path,
            "old run\n2026 INFO pastor: starting\n{\"code\":\"runtime_error\",\"message\":\"flock is empty\"}\n",
        )
        .unwrap();
        assert_eq!(last_error(&path, 8), "flock is empty");
        std::fs::write(&path, "old run\nthread main panicked\n").unwrap();
        assert_eq!(last_error(&path, 8), "thread main panicked");
        assert_eq!(last_error(&path, 100), "thread main panicked");
        std::fs::write(&path, "old run\n").unwrap();
        assert_eq!(last_error(&path, 8), "it wrote nothing");
    }

    #[test]
    fn a_record_counts_only_for_its_own_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        paths.ensure().unwrap();
        assert_eq!(record_for(&paths, Some(7)), None);
        let r = Record {
            pid: 7,
            service: Some("systemd".into()),
            log: None,
        };
        std::fs::write(
            paths.serve_record_file(),
            serde_json::to_string(&r).unwrap(),
        )
        .unwrap();
        assert_eq!(record_for(&paths, Some(7)), Some(r));
        assert_eq!(record_for(&paths, Some(8)), None);
        assert_eq!(record_for(&paths, None), None);
    }
}
