//! `pastor bridge`: a remote CLI's way to this machine's head. It runs over
//! ssh, so the head never listens on a network port; each request line on
//! stdin goes to the head's socket unread, and each reply goes back on stdout.
//!
//! `pastor bridge --agent --machine <name>` is the same way in for the agents
//! on another machine, locked by the `authorized_keys` line
//! (`authorized_key_line`) to that machine's own tasks: it reads each request
//! and passes on only what such an agent may ask (`answer_agent`).

use std::path::Path;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::cli::{CliError, request_failure};
use crate::ipc::{
    IpcRequest, IpcResponse, RequestError, connect_error_means_no_daemon, parse_request_line,
    relay_line, request_line,
};

/// The code a request an agent's bridge will not pass on is answered with.
pub const NOT_ALLOWED_FOR_AGENT: &str = "not_allowed_for_agent";

/// Passes `input`'s lines to the head on `socket` and writes each reply to
/// `output`, until `input` ends. A request the head cannot take stops the
/// bridge with that error, and no later line is sent: the client reads one
/// reply per request, so it must see the failure where the reply would be.
/// No head at all is `no_head`, never a reason to start one.
pub async fn run(
    socket: &Path,
    input: impl AsyncBufRead + Unpin,
    output: impl AsyncWrite + Unpin,
) -> anyhow::Result<()> {
    serve(input, output, async |line: &[u8]| relay(socket, line).await).await
}

/// `run` for the agents on `machine`: each line goes through `answer_agent`.
pub async fn run_agent(
    socket: &Path,
    machine: &str,
    input: impl AsyncBufRead + Unpin,
    output: impl AsyncWrite + Unpin,
) -> anyhow::Result<()> {
    serve(input, output, async |line: &[u8]| {
        answer_agent(socket, machine, line).await
    })
    .await
}

async fn serve(
    mut input: impl AsyncBufRead + Unpin,
    mut output: impl AsyncWrite + Unpin,
    mut answer: impl AsyncFnMut(&[u8]) -> anyhow::Result<Vec<u8>>,
) -> anyhow::Result<()> {
    let mut line = Vec::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line).await? == 0 {
            return Ok(());
        }
        // A blank line holds no request; sending it would only draw an error.
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let reply = answer(&line).await?;
        output.write_all(&reply).await?;
        output.flush().await?;
    }
}

/// One line to the head and its reply, or the error that stops the bridge.
async fn relay(socket: &Path, line: &[u8]) -> anyhow::Result<Vec<u8>> {
    match relay_line(socket, line).await {
        Ok(reply) => Ok(reply),
        Err(RequestError::Connect(e)) if connect_error_means_no_daemon(&e) => Err(CliError::err(
            "no_head",
            format!("no pastor serve is running on this machine ({e})"),
        )),
        Err(err) => {
            let (code, message) = request_failure(&err);
            Err(CliError::err(&code, message))
        }
    }
}

/// `req` to the head, carrying `from_task` and nothing the caller sent.
async fn ask(socket: &Path, req: &IpcRequest, from_task: Option<&str>) -> anyhow::Result<Vec<u8>> {
    relay(socket, request_line(req, from_task)?.as_bytes()).await
}

fn reply_line(resp: &IpcResponse) -> anyhow::Result<Vec<u8>> {
    let mut line = serde_json::to_vec(resp)?;
    line.push(b'\n');
    Ok(line)
}

fn refuse(message: impl std::fmt::Display) -> anyhow::Result<Vec<u8>> {
    reply_line(&IpcResponse::error(NOT_ALLOWED_FOR_AGENT, message))
}

/// The reply for one request line from an agent on `machine`, asking the
/// head only what such an agent may ask. `Ping` always; `TaskShow`,
/// `TaskRead` and `TaskDone` for a task placed on `machine`, as the head
/// knows it; `List`, cut to the tasks of `machine`'s flock. Anything else is
/// refused here and never reaches the head. The bridge names the task a
/// request comes from itself (`ipc::FROM_TASK_FIELD`): the task the request
/// is about, whatever the caller claimed, and none when it names no task.
pub async fn answer_agent(socket: &Path, machine: &str, line: &[u8]) -> anyhow::Result<Vec<u8>> {
    let parsed = std::str::from_utf8(line)
        .map_err(|e| e.to_string())
        .and_then(|text| parse_request_line(text.trim()).map_err(|e| e.to_string()));
    let req = match parsed {
        Ok((req, _claimed)) => req,
        Err(err) => return reply_line(&IpcResponse::error("invalid_request", err)),
    };
    match req {
        IpcRequest::Ping => ask(socket, &req, None).await,
        IpcRequest::List { mut filter } => {
            let Some(flock) = machine_flock(socket, machine).await? else {
                return refuse(format!(
                    "the head has no machine {machine}, so its flock is unknown"
                ));
            };
            filter.flock = Some(flock.clone());
            let reply = ask(socket, &IpcRequest::List { filter }, None).await?;
            match serde_json::from_slice::<IpcResponse>(&reply) {
                Ok(IpcResponse::Tasks(mut tasks)) => {
                    tasks.retain(|t| t.flock.as_deref() == Some(flock.as_str()));
                    reply_line(&IpcResponse::Tasks(tasks))
                }
                Ok(IpcResponse::Error { .. }) => Ok(reply),
                _ => refuse("the head answered a task list with something else"),
            }
        }
        IpcRequest::TaskShow { id }
        | IpcRequest::TaskRead { id, .. }
        | IpcRequest::TaskDone { id } => {
            let task = format!("t-{id}");
            let shown = ask(socket, &IpcRequest::TaskShow { id }, Some(&task)).await?;
            let on_machine = matches!(
                serde_json::from_slice::<IpcResponse>(&shown),
                Ok(IpcResponse::Task(t)) if t.machine.as_deref() == Some(machine)
            );
            if !on_machine {
                return refuse(format!("{task} is not a task on machine {machine}"));
            }
            match req {
                IpcRequest::TaskShow { .. } => Ok(shown),
                _ => ask(socket, &req, Some(&task)).await,
            }
        }
        other => refuse(format!(
            "an agent on {machine} may ping, list its flock's tasks, and show, read or end its machine's tasks; not {}",
            op_name(&other)
        )),
    }
}

/// The request's `op`, as it crosses the socket.
fn op_name(req: &IpcRequest) -> String {
    serde_json::to_value(req)
        .ok()
        .and_then(|v| v.get("op").and_then(|op| op.as_str()).map(str::to_string))
        .unwrap_or_else(|| "this request".into())
}

/// `machine`'s flock, as the head reports it: never read from local files,
/// which the head may not share. `None` when the head has no such machine.
async fn machine_flock(socket: &Path, machine: &str) -> anyhow::Result<Option<String>> {
    let reply = ask(socket, &IpcRequest::FlockList, None).await?;
    Ok(match serde_json::from_slice::<IpcResponse>(&reply) {
        Ok(IpcResponse::Machines(ms)) => ms
            .into_iter()
            .find(|m| m.name == machine)
            .and_then(|m| m.flock),
        _ => None,
    })
}

/// The `authorized_keys` line that lets `key` in only to run
/// `<pastor> bridge --agent --machine <machine>`, with no terminal and no
/// forwarding. `key` is one public key line (`ssh-ed25519 AAAA... comment`).
pub fn authorized_key_line(pastor: &Path, machine: &str, key: &str) -> anyhow::Result<String> {
    let lines: Vec<&str> = key.lines().filter(|l| !l.trim().is_empty()).collect();
    let [key] = lines.as_slice() else {
        return Err(CliError::err(
            "invalid_key",
            "the key must be exactly one public key line",
        ));
    };
    let key = key.trim();
    if key.split_whitespace().count() < 2 {
        return Err(CliError::err(
            "invalid_key",
            "a public key line is its type and its key, like ssh-ed25519 AAAA...",
        ));
    }
    let pastor = pastor.display().to_string();
    if pastor.contains(['"', '\\']) || pastor.chars().any(char::is_whitespace) {
        return Err(CliError::err(
            "invalid_path",
            format!("{pastor} cannot go in an authorized_keys command unquoted"),
        ));
    }
    Ok(format!(
        "command=\"{pastor} bridge --agent --machine {machine}\",no-pty,no-port-forwarding,no-agent-forwarding,no-X11-forwarding {key}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::TaskFilter;
    use std::sync::{Arc, Mutex};
    use tokio::io::BufReader;

    /// A head with two machines in two flocks and a task on each, that
    /// answers every `List` with every task whatever the filter, and keeps
    /// each line it was sent.
    struct FakeHead {
        _dir: tempfile::TempDir,
        socket: std::path::PathBuf,
        seen: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    fn task_json(id: i64, machine: &str, flock: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id, "job": "run", "item": null, "prompt": "p",
            "spec": {"agent": "claude"}, "machine": machine, "state": "running",
            "flock": flock,
            "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z",
        })
    }

    fn machine_json(name: &str, flock: &str) -> serde_json::Value {
        serde_json::json!({
            "name": name, "endpoint": "", "channel": "connected", "herdr_version": null,
            "protocol": null, "error": null, "live": 0, "max_agents": 2, "tags": [],
            "flock": flock,
        })
    }

    fn fake_head() -> FakeHead {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("head.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (r, mut w) = stream.into_split();
                let mut line = String::new();
                BufReader::new(r).read_line(&mut line).await.unwrap();
                let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
                log.lock().unwrap().push(v.clone());
                let id = v["id"].as_i64().unwrap_or(0);
                let tasks = [task_json(1, "pi-1", "home"), task_json(2, "pi-2", "work")];
                let reply = match v["op"].as_str().unwrap() {
                    "ping" => {
                        serde_json::json!({"kind": "pong", "data": {"version": "t", "protocol": 3}})
                    }
                    "flock_list" => serde_json::json!({"kind": "machines", "data": [
                        machine_json("pi-1", "home"), machine_json("pi-2", "work"),
                    ]}),
                    "list" => serde_json::json!({"kind": "tasks", "data": tasks}),
                    "task_show" if (1..=2).contains(&id) => {
                        serde_json::json!({"kind": "task", "data": tasks[id as usize - 1]})
                    }
                    "task_show" | "task_read" | "task_done" if !(1..=2).contains(&id) => {
                        serde_json::json!({"kind": "error", "data": {"code": "task_not_found", "message": "no"}})
                    }
                    "task_read" => serde_json::json!({"kind": "text", "data": "pane text"}),
                    "task_done" => {
                        serde_json::json!({"kind": "task", "data": tasks[id as usize - 1]})
                    }
                    op => serde_json::json!({"kind": "text", "data": format!("did {op}")}),
                };
                let mut out = serde_json::to_string(&reply).unwrap();
                out.push('\n');
                w.write_all(out.as_bytes()).await.unwrap();
            }
        });
        FakeHead {
            _dir: dir,
            socket,
            seen,
        }
    }

    impl FakeHead {
        /// What the agent bridge for `pi-1` answers `req`, sent as if from
        /// `claimed`.
        async fn ask(&self, req: &IpcRequest, claimed: Option<&str>) -> IpcResponse {
            let line = request_line(req, claimed).unwrap();
            let reply = answer_agent(&self.socket, "pi-1", line.as_bytes())
                .await
                .unwrap();
            assert!(reply.ends_with(b"\n"));
            serde_json::from_slice(&reply).unwrap()
        }

        fn ops(&self) -> Vec<String> {
            let seen = self.seen.lock().unwrap();
            seen.iter()
                .map(|v| v["op"].as_str().unwrap().to_string())
                .collect()
        }

        fn last(&self) -> serde_json::Value {
            self.seen.lock().unwrap().last().cloned().unwrap()
        }
    }

    fn refused(resp: &IpcResponse) -> bool {
        matches!(resp, IpcResponse::Error { code, .. } if code == NOT_ALLOWED_FOR_AGENT)
    }

    #[tokio::test]
    async fn an_agent_may_show_read_and_end_a_task_on_its_machine() {
        let head = fake_head();
        let resp = head.ask(&IpcRequest::TaskShow { id: 1 }, None).await;
        assert!(
            matches!(&resp, IpcResponse::Task(t) if t.id == 1),
            "{resp:?}"
        );
        let resp = head
            .ask(&IpcRequest::TaskRead { id: 1, lines: 5 }, None)
            .await;
        assert!(
            matches!(&resp, IpcResponse::Text(t) if t == "pane text"),
            "{resp:?}"
        );
        assert_eq!(head.last()["op"], "task_read");
        assert_eq!(head.last()["lines"], 5);
        let resp = head.ask(&IpcRequest::TaskDone { id: 1 }, None).await;
        assert!(
            matches!(&resp, IpcResponse::Task(t) if t.id == 1),
            "{resp:?}"
        );
        assert_eq!(head.last()["op"], "task_done");
        let resp = head.ask(&IpcRequest::Ping, None).await;
        assert!(matches!(resp, IpcResponse::Pong { .. }), "{resp:?}");
    }

    #[tokio::test]
    async fn an_agent_may_not_touch_a_task_on_another_machine() {
        let head = fake_head();
        for req in [
            IpcRequest::TaskShow { id: 2 },
            IpcRequest::TaskRead { id: 2, lines: 5 },
            IpcRequest::TaskDone { id: 2 },
            IpcRequest::TaskDone { id: 9 },
        ] {
            let resp = head.ask(&req, Some("t-1")).await;
            assert!(refused(&resp), "{req:?}: {resp:?}");
        }
        // Only the lookups reached the head, never the reads or the ending.
        assert!(
            head.ops().iter().all(|op| op == "task_show"),
            "{:?}",
            head.ops()
        );
    }

    #[tokio::test]
    async fn an_agent_may_not_change_the_fleet() {
        let head = fake_head();
        for req in [
            IpcRequest::TaskClose {
                id: 1,
                remove_worktree: false,
            },
            IpcRequest::TaskRetry { id: 1, place: None },
            IpcRequest::TaskSend {
                id: 1,
                input: crate::machine::SendInput::default(),
            },
            IpcRequest::TaskPrune {
                states: vec![],
                older_than_secs: 0,
            },
            IpcRequest::FlockList,
            IpcRequest::FlockRemove {
                name: "home".into(),
            },
            IpcRequest::JobList,
            IpcRequest::JobRun { name: "j".into() },
            IpcRequest::Reload,
            IpcRequest::Tick {
                job: None,
                dry_run: true,
            },
        ] {
            let resp = head.ask(&req, Some("t-1")).await;
            assert!(refused(&resp), "{req:?}: {resp:?}");
        }
        assert!(head.ops().is_empty(), "{:?}", head.ops());
    }

    #[tokio::test]
    async fn an_agents_list_is_cut_to_its_machines_flock() {
        let head = fake_head();
        let filter = TaskFilter {
            flock: Some("work".into()),
            ..TaskFilter::default()
        };
        let IpcResponse::Tasks(tasks) = head.ask(&IpcRequest::List { filter }, None).await else {
            panic!()
        };
        let ids: Vec<i64> = tasks.iter().map(|t| t.id).collect();
        assert_eq!(ids, [1]);
        assert_eq!(head.last()["op"], "list");
        assert_eq!(head.last()["filter"]["flock"], "home");
    }

    #[tokio::test]
    async fn the_bridge_names_the_task_a_request_comes_from() {
        let head = fake_head();
        head.ask(&IpcRequest::TaskDone { id: 1 }, Some("t-2")).await;
        assert_eq!(head.last()["op"], "task_done");
        assert_eq!(head.last()[crate::ipc::FROM_TASK_FIELD], "t-1");
        head.ask(&IpcRequest::Ping, Some("t-2")).await;
        assert!(head.last().get(crate::ipc::FROM_TASK_FIELD).is_none());
    }

    #[tokio::test]
    async fn a_line_that_is_no_request_is_answered_without_the_head() {
        let head = fake_head();
        let reply = answer_agent(&head.socket, "pi-1", b"not json\n")
            .await
            .unwrap();
        let resp: IpcResponse = serde_json::from_slice(&reply).unwrap();
        assert!(matches!(resp, IpcResponse::Error { code, .. } if code == "invalid_request"));
        assert!(head.ops().is_empty());
    }

    #[tokio::test]
    async fn the_agent_bridge_answers_line_by_line() {
        let head = fake_head();
        let input = format!(
            "{}\n{}",
            request_line(&IpcRequest::Reload, None).unwrap(),
            request_line(&IpcRequest::Ping, None).unwrap()
        );
        let mut out = Vec::new();
        run_agent(&head.socket, "pi-1", input.as_bytes(), &mut out)
            .await
            .unwrap();
        let replies: Vec<IpcResponse> = out
            .split(|b| *b == b'\n')
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_slice(l).unwrap())
            .collect();
        assert_eq!(replies.len(), 2);
        assert!(refused(&replies[0]));
        assert!(matches!(replies[1], IpcResponse::Pong { .. }));
    }

    #[test]
    fn the_authorized_key_line_locks_the_key_to_the_agent_bridge() {
        let line = authorized_key_line(
            Path::new("/opt/pastor/bin/pastor"),
            "pi-1",
            "ssh-ed25519 AAAAC3Nza key@pi-1\n",
        )
        .unwrap();
        assert_eq!(
            line,
            "command=\"/opt/pastor/bin/pastor bridge --agent --machine pi-1\",no-pty,no-port-forwarding,no-agent-forwarding,no-X11-forwarding ssh-ed25519 AAAAC3Nza key@pi-1"
        );
        for bad in ["", "ssh-ed25519", "ssh-ed25519 A\nssh-rsa B"] {
            assert!(
                authorized_key_line(Path::new("/p"), "pi-1", bad).is_err(),
                "{bad:?}"
            );
        }
        assert!(authorized_key_line(Path::new("/a b/pastor"), "pi-1", "ssh-ed25519 A").is_err());
    }
}
