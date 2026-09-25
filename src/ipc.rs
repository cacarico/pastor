use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::machine::MachineStatus;
use crate::scheduler::{JobRunReport, JobStatus};
use crate::store::TaskFilter;
use crate::task::{DispatchSpec, Task, TaskState};

/// The head's IPC protocol, answered in `Pong`. Bumped when a request gains a
/// field an older head would silently ignore (serde skips unknown fields), so
/// the CLI can refuse to send it there. A head that answers no protocol is 0.
/// 1: flocks (`Run::flock`, `TaskFilter::flock`).
pub const IPC_PROTOCOL: u32 = 1;

/// The first protocol whose head honours `flock` in a request.
pub const FLOCK_PROTOCOL: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum IpcRequest {
    Ping,
    Run {
        prompt: String,
        spec: DispatchSpec,
        /// `None`: the flock of the pinned machine, or the default flock.
        #[serde(default)]
        flock: Option<String>,
    },
    List {
        filter: TaskFilter,
    },
    TaskShow {
        id: i64,
    },
    TaskRead {
        id: i64,
        lines: u32,
    },
    FlockList,
    /// `flock remove`, done by the head so the queued-task check and the
    /// edit of flock.toml are one step against `Run`. Answers `Text`.
    FlockRemove {
        name: String,
    },
    /// One scheduler pass now; `job` forces that job regardless of schedule.
    Tick {
        job: Option<String>,
        dry_run: bool,
    },
    /// Re-read the jobs directory now.
    Reload,
    JobList,
    /// Fire a job now, ignoring schedule, overlap and `enabled`.
    JobRun {
        name: String,
    },
    /// Queue a new task copying a failed or stale one, then dispatch.
    /// Answers `Task` (the new row).
    TaskRetry {
        id: i64,
    },
    /// Close the task's pane (or, with `remove_worktree`, its worktree) and
    /// mark it closed. Answers `Task`, or `Text` for an orphaned agent with no
    /// row.
    TaskClose {
        id: i64,
        remove_worktree: bool,
    },
    /// Type into a live task's pane, through its machine's actor. Answers
    /// `Text`.
    TaskSend {
        id: i64,
        input: crate::machine::SendInput,
    },
    /// Delete rows in `states` that finished more than `older_than_secs`
    /// ago. Answers `Pruned`.
    TaskPrune {
        states: Vec<TaskState>,
        older_than_secs: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
// Adjacently tagged (`content = "data"`), not internally tagged: several variants
// here are newtypes wrapping a `Vec` or a `String` (`Tasks`, `Text`, `Machines`),
// and serde_json cannot serialize those under internal tagging (a sequence or a
// string cannot carry a `kind` field merged into it). Adjacent tagging works
// uniformly across struct and newtype variants alike.
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
// One response crosses the socket at a time (never in a hot loop), so the size
// difference between variants that clippy flags here doesn't matter in practice.
#[allow(clippy::large_enum_variant)]
pub enum IpcResponse {
    Pong {
        version: String,
        /// `IPC_PROTOCOL` of the head; missing from a head older than it.
        #[serde(default)]
        protocol: u32,
    },
    Task(Task),
    Tasks(Vec<Task>),
    Text(String),
    Machines(Vec<MachineStatus>),
    Error {
        code: String,
        message: String,
    },
    Runs(Vec<JobRunReport>),
    Jobs(Vec<JobStatus>),
    Pruned(crate::store::PruneOutcome),
}

impl IpcResponse {
    pub fn error(code: &str, message: impl std::fmt::Display) -> IpcResponse {
        IpcResponse::Error {
            code: code.into(),
            message: message.to_string(),
        }
    }
}

/// Default bound on a full request/reply round trip. Deliberately longer than
/// `MachineSettings::request_timeout`'s default (60s): a `Run` that dispatches
/// inline (see `Daemon::handle`) must have room to finish before this gives up.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// `daemon_running` only needs to know whether something answers `Ping`; it
/// shouldn't itself hang for a minute against a wedged daemon.
const PING_TIMEOUT: Duration = Duration::from_secs(2);

/// Why a request to the head failed. The CLI tells the user different things
/// for each: nothing listening means `pastor serve` is not running, while a
/// connection that was accepted and then left unanswered means the head is up
/// but busy (a long dispatch pass holds the accept loop).
#[derive(Debug, thiserror::Error)]
pub enum RequestError {
    /// The connect itself failed: no socket, connection refused, permission
    /// denied. Nothing answered.
    #[error("{0}")]
    Connect(std::io::Error),
    /// Something accepted the connection (or kept it in the backlog) but did
    /// not reply within the bound.
    #[error("the head did not answer within {0:?}")]
    Timeout(Duration),
    /// Connected, then the exchange broke: a dropped connection, an empty or
    /// malformed reply.
    #[error(transparent)]
    Exchange(anyhow::Error),
}

/// One request, one reply, then the connection closes. Bounded by
/// `DEFAULT_REQUEST_TIMEOUT`; use `request_with_timeout` to choose a different bound.
pub async fn request(socket: &Path, req: &IpcRequest) -> Result<IpcResponse, RequestError> {
    request_with_timeout(socket, req, DEFAULT_REQUEST_TIMEOUT).await
}

/// Like `request`, but bounds the whole round trip (connect + write + read) by
/// `timeout` instead of the default. A daemon that accepts the connection but
/// never replies (wedged on a hung herdr request, or otherwise stuck) must not
/// hang the caller forever.
pub async fn request_with_timeout(
    socket: &Path,
    req: &IpcRequest,
    timeout: Duration,
) -> Result<IpcResponse, RequestError> {
    let exchange = async {
        let stream = tokio::net::UnixStream::connect(socket)
            .await
            .map_err(RequestError::Connect)?;
        round_trip(stream, req)
            .await
            .map_err(RequestError::Exchange)
    };
    match tokio::time::timeout(timeout, exchange).await {
        Ok(result) => result,
        Err(_) => Err(RequestError::Timeout(timeout)),
    }
}

async fn round_trip(
    stream: tokio::net::UnixStream,
    req: &IpcRequest,
) -> anyhow::Result<IpcResponse> {
    let (r, mut w) = stream.into_split();
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
    w.flush().await?;
    let mut reply = String::new();
    BufReader::new(r).read_line(&mut reply).await?;
    anyhow::ensure!(
        !reply.trim().is_empty(),
        "daemon closed the connection without a reply"
    );
    Ok(serde_json::from_str(reply.trim())?)
}

/// What `probe_daemon` found at a socket path. Only `NotRunning` means it's safe to
/// unlink and replace the socket file: a connect refused (or a path that doesn't
/// exist) is the one signal that nothing is on the other end. `Running` and
/// `Unresponsive` both mean something is there — a busy daemon mid-request looks
/// exactly like a wedged one from the outside, so both must be left alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonProbe {
    /// Connected and got back a `Pong` within the probe window.
    Running,
    /// The connect failed the way an absent or abandoned socket does (refused, or
    /// the path doesn't exist).
    NotRunning,
    /// Something accepted the connection but didn't answer `Ping` within
    /// `PING_TIMEOUT`, or the connect failed some other way (e.g. permission
    /// denied). Either way, it is not safe to assume the socket is stale.
    Unresponsive,
}

/// Whether a failed connect to the daemon socket shows that no daemon is there.
/// Only a refused connect or a missing path does; anything else, such as
/// permission denied, may hide a live daemon. `probe_daemon` and the CLI's
/// "not running" advice both use this so they cannot disagree.
pub fn connect_error_means_no_daemon(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
    )
}

/// What one ping at the head's socket found, with the pong's fields when
/// there was one: `probe_daemon`, keeping what the head said about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadPing {
    NotRunning,
    Unresponsive,
    Pong { version: String, protocol: u32 },
}

pub async fn ping_head(socket: &Path) -> HeadPing {
    let stream = match tokio::net::UnixStream::connect(socket).await {
        Ok(s) => s,
        Err(err) if connect_error_means_no_daemon(&err) => return HeadPing::NotRunning,
        Err(_) => return HeadPing::Unresponsive,
    };
    match tokio::time::timeout(PING_TIMEOUT, round_trip(stream, &IpcRequest::Ping)).await {
        Ok(Ok(IpcResponse::Pong { version, protocol })) => HeadPing::Pong { version, protocol },
        _ => HeadPing::Unresponsive,
    }
}

pub async fn probe_daemon(socket: &Path) -> DaemonProbe {
    match ping_head(socket).await {
        HeadPing::NotRunning => DaemonProbe::NotRunning,
        HeadPing::Unresponsive => DaemonProbe::Unresponsive,
        HeadPing::Pong { .. } => DaemonProbe::Running,
    }
}

pub async fn daemon_running(socket: &Path) -> bool {
    matches!(probe_daemon(socket).await, DaemonProbe::Running)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[tokio::test]
    async fn request_with_timeout_bounds_a_daemon_that_never_replies() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("hung.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            // Accept the connection and then never write a reply.
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            std::future::pending::<()>().await;
            drop(stream);
        });

        let start = Instant::now();
        let err = request_with_timeout(&socket, &IpcRequest::Ping, Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "did not bound the round trip: took {:?}",
            start.elapsed()
        );
        assert!(matches!(err, RequestError::Timeout(_)), "{err:?}");
        assert!(err.to_string().contains("did not answer"), "{err}");
    }

    #[tokio::test]
    async fn request_to_a_missing_socket_is_a_connect_error_not_a_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let err = request_with_timeout(
            &tmp.path().join("absent.sock"),
            &IpcRequest::Ping,
            Duration::from_millis(100),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RequestError::Connect(_)), "{err:?}");
    }

    /// Minimal, directly-constructed `Task`. `task::tests` has its own builder but
    /// it lives in a private `mod tests` that isn't reachable from here, so this
    /// mirrors its shape instead of trying to reuse it.
    fn minimal_task() -> Task {
        let now = chrono::Utc::now();
        Task {
            id: 1,
            job: "run".into(),
            item: serde_json::Value::Null,
            prompt: "hi".into(),
            spec: DispatchSpec {
                agent: "claude".into(),
                agent_args: vec![],
                repo: None,
                worktree: false,
                branch: None,
                machine: None,
                tags: vec![],
                timeout_secs: 60,
            },
            machine: None,
            workspace_id: None,
            pane_id: None,
            agent_name: None,
            state: crate::task::TaskState::Queued,
            error: None,
            last_completion_seq: None,
            prompt_pending: false,
            activity_seen: false,
            retry_of: None,
            created_at: now,
            started_at: None,
            finished_at: None,
            updated_at: now,
            flock: None,
        }
    }

    /// Every `IpcResponse` variant must round-trip through JSON: the newtype
    /// variants wrapping a `Vec` or `String` (`Tasks`, `Text`, `Machines`) broke
    /// under internal tagging (serde_json refuses to merge a `kind` field into a
    /// sequence or a string), which the adjacently-tagged `content = "data"`
    /// representation fixes for every shape uniformly. `Task(Task)` is included
    /// too: it's what `run` and `task show` actually return over the wire.
    #[test]
    fn every_response_variant_round_trips_through_json() {
        let responses = vec![
            IpcResponse::Pong {
                version: "1".into(),
                protocol: IPC_PROTOCOL,
            },
            IpcResponse::Task(minimal_task()),
            IpcResponse::Tasks(vec![minimal_task()]),
            IpcResponse::Text("hello".into()),
            IpcResponse::Machines(vec![]),
            IpcResponse::Runs(vec![]),
            IpcResponse::Jobs(vec![]),
            IpcResponse::Pruned(crate::store::PruneOutcome {
                pruned: 2,
                kept_worktrees: vec![3],
            }),
            IpcResponse::error("some_code", "some message"),
        ];
        for resp in responses {
            let json = serde_json::to_string(&resp).unwrap();
            let back: IpcResponse = serde_json::from_str(&json).unwrap();
            assert_eq!(format!("{resp:?}"), format!("{back:?}"), "{json}");
        }
    }

    #[test]
    fn scheduler_requests_round_trip() {
        for req in [
            IpcRequest::Tick {
                job: Some("j".into()),
                dry_run: true,
            },
            IpcRequest::Reload,
            IpcRequest::JobList,
            IpcRequest::JobRun { name: "j".into() },
        ] {
            let json = serde_json::to_string(&req).unwrap();
            let back: IpcRequest = serde_json::from_str(&json).unwrap();
            assert_eq!(format!("{req:?}"), format!("{back:?}"), "{json}");
        }
    }

    /// Pin the adjacent-tagging wire shape itself for the two variants that used
    /// to be impossible to serialize at all: `Task`'s payload is a JSON object,
    /// `Tasks`'s is a JSON array, both carried under a `"data"` key alongside
    /// `"kind"`.
    #[test]
    fn task_and_tasks_carry_kind_and_data_with_the_expected_shape() {
        let v: serde_json::Value = serde_json::from_str(
            &serde_json::to_string(&IpcResponse::Task(minimal_task())).unwrap(),
        )
        .unwrap();
        assert_eq!(v["kind"], "task");
        assert!(v["data"].is_object(), "{v}");
        assert_eq!(v["data"]["id"], 1);

        let v: serde_json::Value = serde_json::from_str(
            &serde_json::to_string(&IpcResponse::Tasks(vec![minimal_task()])).unwrap(),
        )
        .unwrap();
        assert_eq!(v["kind"], "tasks");
        assert!(v["data"].is_array(), "{v}");
        assert_eq!(v["data"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn task_lifecycle_requests_round_trip() {
        for req in [
            IpcRequest::TaskRetry { id: 4 },
            IpcRequest::TaskClose {
                id: 4,
                remove_worktree: true,
            },
            IpcRequest::TaskPrune {
                states: vec![crate::task::TaskState::Done, crate::task::TaskState::Closed],
                older_than_secs: 3 * 86400,
            },
        ] {
            let json = serde_json::to_string(&req).unwrap();
            let back: IpcRequest = serde_json::from_str(&json).unwrap();
            assert_eq!(format!("{req:?}"), format!("{back:?}"), "{json}");
        }
        let v = serde_json::to_value(IpcRequest::TaskPrune {
            states: vec![crate::task::TaskState::Failed],
            older_than_secs: 1,
        })
        .unwrap();
        assert_eq!(v["op"], "task_prune");
        assert_eq!(v["states"][0], "failed");
    }

    #[tokio::test]
    async fn probe_daemon_reports_not_running_for_a_missing_socket() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("nothing-here.sock");
        assert_eq!(probe_daemon(&socket).await, DaemonProbe::NotRunning);
        assert!(!daemon_running(&socket).await);
    }
}
