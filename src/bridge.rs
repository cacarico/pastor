//! `pastor bridge`: a remote CLI's way to this machine's head. It runs over
//! ssh, so the head never listens on a network port; each request line on
//! stdin goes to the head's socket unread, and each reply goes back on stdout.
//!
//! `pastor bridge --agent --machine <name>` is the same way in for the agents
//! on another machine, locked by the `authorized_keys` line
//! (`authorized_key_line`) to that machine's own tasks: it reads each request
//! and passes on only what such an agent may ask (`answer_agent`).
//!
//! `pastor bridge --shepherd --machine <name> --gen <n>` is the way in for
//! that machine's shepherd, a pull machine's headless `pastor serve`, locked
//! the same way by its own key's line. Before each request it asks the head
//! whether enrollment `n` still speaks for the machine (`check_generation`),
//! and it passes on only what a shepherd needs, for its own machine
//! (`answer_shepherd`). It may also fetch an artifact from the head's cache
//! (`ArtifactLocate`): the request is a line on stdin like any other, never
//! an argument, and the reply is one line, `Artifact` with the sum and size,
//! then that many raw bytes of the file (`fetch_artifact`).

use std::path::Path;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::cli::{CliError, request_error};
use crate::ipc::{
    IpcRequest, IpcResponse, RequestError, connect_error_means_no_daemon, parse_request_line,
    relay_line, request_line,
};

/// The code a request an agent's bridge will not pass on is answered with.
pub const NOT_ALLOWED_FOR_AGENT: &str = "not_allowed_for_agent";

/// The code a request a shepherd's bridge will not pass on is answered with.
pub const NOT_ALLOWED_FOR_SHEPHERD: &str = "not_allowed_for_shepherd";

/// The code a shepherd's request is refused with once its key's line is
/// from an enrollment the head has moved past, or its machine is gone.
pub const MACHINE_REVOKED: &str = "machine_revoked";

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

/// `run` for the shepherd on `machine`, enrolled as `generation` (the
/// forced command's `--gen`): each line goes through `answer_shepherd`, once
/// the head says that generation is still the machine's (`check_generation`).
/// A refused check is the last reply: the bridge stops there, and reads no
/// later line.
pub async fn run_shepherd(
    socket: &Path,
    cache: &Path,
    machine: &str,
    generation: i64,
    mut input: impl AsyncBufRead + Unpin,
    mut output: impl AsyncWrite + Unpin,
) -> anyhow::Result<()> {
    let mut line = Vec::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line).await? == 0 {
            return Ok(());
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let (reply, stop) = match check_generation(socket, machine, generation).await? {
            Ok(()) => match parse(&line) {
                Ok(IpcRequest::ArtifactLocate {
                    name,
                    version,
                    target,
                }) => {
                    // The reply line, then the file's bytes.
                    let (reply, file) =
                        fetch_artifact(socket, cache, &name, &version, &target).await?;
                    output.write_all(&reply).await?;
                    if let Some((mut file, size)) = file {
                        send_bytes(&mut file, size, &mut output).await?;
                    }
                    output.flush().await?;
                    continue;
                }
                _ => (answer_shepherd(socket, machine, &line).await?, false),
            },
            Err(refused) => (refused, true),
        };
        output.write_all(&reply).await?;
        output.flush().await?;
        if stop {
            return Ok(());
        }
    }
}

/// The reply to an `ArtifactLocate` from a shepherd, and the file to send
/// after it. The head says where its cache holds the artifact; the bridge
/// opens that file only when it is a plain file inside `cache` (symlinks
/// resolved), so a head's answer can never make it read anywhere else, and
/// sends the caller the reply with the path left out. A refusal from the head
/// is the reply, with no file. The caller sees the sum and size on the line
/// and checks the bytes against them.
async fn fetch_artifact(
    socket: &Path,
    cache: &Path,
    name: &str,
    version: &str,
    target: &str,
) -> anyhow::Result<(Vec<u8>, Option<(std::fs::File, u64)>)> {
    let req = IpcRequest::ArtifactLocate {
        name: name.to_string(),
        version: version.to_string(),
        target: target.to_string(),
    };
    let reply = ask(socket, &req, None).await?;
    let (path, sha256, size) = match serde_json::from_slice::<IpcResponse>(&reply) {
        Ok(IpcResponse::Artifact { path, sha256, size }) => (path, sha256, size),
        Ok(IpcResponse::Error { .. }) => return Ok((reply, None)),
        _ => {
            return Ok((
                refuse(
                    NOT_ALLOWED_FOR_SHEPHERD,
                    "the head answered an artifact lookup with something else",
                )?,
                None,
            ));
        }
    };
    let outside = || {
        refuse(
            "artifact_outside_cache",
            "the head named a file outside its artifact cache; nothing was sent",
        )
    };
    let (Ok(root), Ok(real)) = (std::fs::canonicalize(cache), std::fs::canonicalize(&path)) else {
        return Ok((outside()?, None));
    };
    if !real.starts_with(&root) {
        return Ok((outside()?, None));
    }
    let file = std::fs::File::open(&real)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() != size {
        return Ok((
            refuse(
                "artifact_changed",
                "the cached file is not the size the head said; try again",
            )?,
            None,
        ));
    }
    Ok((
        reply_line(&IpcResponse::Artifact {
            path: String::new(),
            sha256,
            size,
        })?,
        Some((file, size)),
    ))
}

/// Writes `size` bytes of `file` to `output`: a short file is an error, so
/// the caller never takes a truncated artifact for a whole one (it checks
/// the sum on the reply line too).
async fn send_bytes(
    file: &mut std::fs::File,
    size: u64,
    output: &mut (impl AsyncWrite + Unpin),
) -> anyhow::Result<()> {
    use std::io::Read;
    let mut buf = vec![0u8; 64 * 1024];
    let mut left = size;
    while left > 0 {
        let want = buf.len().min(usize::try_from(left).unwrap_or(usize::MAX));
        let n = file.read(&mut buf[..want])?;
        if n == 0 {
            anyhow::bail!("the cached file ended {left} bytes short of {size}");
        }
        output.write_all(&buf[..n]).await?;
        left -= n as u64;
    }
    Ok(())
}

/// Asks the head whether `generation` still speaks for `machine`
/// (`IpcRequest::ShepherdCheck`). OpenSSH fixes a connection's forced
/// command when it authenticates, so editing `authorized_keys` cannot reach
/// a connection already open; this check, before every request, does.
/// `Err` is the reply that refuses the request: the head's own error
/// (`machine_revoked`, or what an older head says of a request it does not
/// know), or `machine_revoked` for an answer that is not the one the check
/// expects.
pub async fn check_generation(
    socket: &Path,
    machine: &str,
    generation: i64,
) -> anyhow::Result<Result<(), Vec<u8>>> {
    let req = IpcRequest::ShepherdCheck {
        machine: machine.to_string(),
        generation,
    };
    let reply = ask(socket, &req, None).await?;
    Ok(match serde_json::from_slice::<IpcResponse>(&reply) {
        Ok(IpcResponse::Text(_)) => Ok(()),
        Ok(IpcResponse::Error { .. }) => Err(reply),
        _ => Err(refuse(
            MACHINE_REVOKED,
            "the head answered the generation check with something else",
        )?),
    })
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
        Err(err) => Err(request_error(&err).into()),
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

fn refuse(code: &str, message: impl std::fmt::Display) -> anyhow::Result<Vec<u8>> {
    reply_line(&IpcResponse::error(code, message))
}

/// `line` as a request, or the reply that answers it unread: not UTF-8, or
/// not a request line (`invalid_request`).
fn parse(line: &[u8]) -> Result<IpcRequest, anyhow::Result<Vec<u8>>> {
    let parsed = std::str::from_utf8(line)
        .map_err(|e| e.to_string())
        .and_then(|text| parse_request_line(text.trim()).map_err(|e| e.to_string()));
    match parsed {
        Ok((req, _claimed)) => Ok(req),
        Err(err) => Err(reply_line(&IpcResponse::error("invalid_request", err))),
    }
}

/// What both `answer_agent` and `answer_shepherd` pass on: `Ping` always;
/// `TaskShow`, `TaskRead` and `TaskDone` for a task placed on `machine`, as
/// the head knows it; `List`, cut to the tasks of `machine`'s flock. `None`
/// for anything else, which each caller refuses in its own words, with
/// `not_allowed`. The bridge names the task a request comes from itself
/// (`ipc::FROM_TASK_FIELD`): the task the request is about, whatever the
/// caller claimed, and none when it names no task.
async fn answer_common(
    socket: &Path,
    machine: &str,
    not_allowed: &str,
    req: &IpcRequest,
) -> anyhow::Result<Option<Vec<u8>>> {
    Ok(Some(match req {
        IpcRequest::Ping => ask(socket, req, None).await?,
        IpcRequest::List { filter } => {
            let mut filter = filter.clone();
            let Some(flocks) = machine_flocks(socket, machine).await? else {
                return Ok(Some(refuse(
                    not_allowed,
                    format!("the head has no machine {machine}, so its flocks are unknown"),
                )?));
            };
            if let [one] = &flocks[..] {
                filter.flock = Some(one.clone());
            }
            let reply = ask(socket, &IpcRequest::List { filter }, None).await?;
            match serde_json::from_slice::<IpcResponse>(&reply) {
                Ok(IpcResponse::Tasks(mut tasks)) => {
                    tasks.retain(|t| t.flock.as_ref().is_some_and(|f| flocks.contains(f)));
                    reply_line(&IpcResponse::Tasks(tasks))?
                }
                Ok(IpcResponse::Error { .. }) => reply,
                _ => refuse(
                    not_allowed,
                    "the head answered a task list with something else",
                )?,
            }
        }
        IpcRequest::TaskShow { id }
        | IpcRequest::TaskRead { id, .. }
        | IpcRequest::TaskDone { id, .. } => {
            let id = *id;
            let task = format!("t-{id}");
            let shown = ask(socket, &IpcRequest::TaskShow { id }, Some(&task)).await?;
            let on_machine = matches!(
                serde_json::from_slice::<IpcResponse>(&shown),
                Ok(IpcResponse::Task(t)) if t.machine.as_deref() == Some(machine)
            );
            if !on_machine {
                return Ok(Some(refuse(
                    not_allowed,
                    format!("{task} is not a task on machine {machine}"),
                )?));
            }
            match req {
                IpcRequest::TaskShow { .. } => shown,
                _ => ask(socket, req, Some(&task)).await?,
            }
        }
        _ => return Ok(None),
    }))
}

/// The reply for one request line from an agent on `machine`, asking the
/// head only what such an agent may ask (`answer_common`). Anything else is
/// refused here and never reaches the head.
pub async fn answer_agent(socket: &Path, machine: &str, line: &[u8]) -> anyhow::Result<Vec<u8>> {
    let req = match parse(line) {
        Ok(req) => req,
        Err(reply) => return reply,
    };
    if let Some(reply) = answer_common(socket, machine, NOT_ALLOWED_FOR_AGENT, &req).await? {
        return Ok(reply);
    }
    refuse(
        NOT_ALLOWED_FOR_AGENT,
        format!(
            "an agent on {machine} may ping, list its flock's tasks, and show, read or end its machine's tasks; not {}",
            op_name(&req)
        ),
    )
}

/// The reply for one request line from the shepherd on `machine`: what
/// `answer_agent` allows (`answer_common`), plus claiming and reporting its
/// tasks, reading its saved trust, reporting its bootstrap, submitting jobs
/// and reading its events, all for `machine` alone. `TaskClaim`,
/// `TaskReport`, `PullTrust`, `BootstrapReport` and `WorkdirReport` get their
/// `machine` overwritten with `machine`, whatever the request said, so a
/// hijacked shepherd cannot take or report another machine's tasks. A
/// `JobSubmit`'s dispatch is pinned to `machine` the same way, its flock
/// left for the head to take from the machine (`pin_dispatch`), and
/// `EventsSince` is cut to the records about `machine` by the head, before
/// its limit. Anything else is refused here and never reaches the head.
pub async fn answer_shepherd(socket: &Path, machine: &str, line: &[u8]) -> anyhow::Result<Vec<u8>> {
    let req = match parse(line) {
        Ok(req) => req,
        Err(reply) => return reply,
    };
    if let Some(reply) = answer_common(socket, machine, NOT_ALLOWED_FOR_SHEPHERD, &req).await? {
        return Ok(reply);
    }
    let me = machine.to_string();
    let req = match req {
        IpcRequest::TaskClaim {
            free_slots,
            flock_work,
            herdr_version,
            pastor_version,
            workdir_sets,
            ..
        } => IpcRequest::TaskClaim {
            machine: me,
            free_slots,
            flock_work,
            herdr_version,
            pastor_version,
            workdir_sets,
        },
        IpcRequest::TaskReport {
            id,
            state,
            pane,
            detail,
            limit,
            trusted,
            checkout,
            workdir_skipped,
            ..
        } => IpcRequest::TaskReport {
            machine: me,
            id,
            state,
            pane,
            detail,
            limit,
            trusted,
            checkout,
            workdir_skipped,
        },
        IpcRequest::PullTrust { .. } => IpcRequest::PullTrust { machine: me },
        IpcRequest::BootstrapReport { result, .. } => IpcRequest::BootstrapReport {
            machine: me,
            result,
        },
        IpcRequest::WorkdirReport { reports, .. } => IpcRequest::WorkdirReport {
            machine: me,
            reports,
        },
        IpcRequest::JobSubmit {
            job,
            dispatch,
            prompt,
            items,
        } => match pin_dispatch(machine, dispatch) {
            Ok(dispatch) => IpcRequest::JobSubmit {
                job,
                dispatch,
                prompt,
                items,
            },
            Err(why) => return refuse(NOT_ALLOWED_FOR_SHEPHERD, why),
        },
        IpcRequest::EventsSince {
            after, limit, task, ..
        } => {
            let reply = ask(
                socket,
                &IpcRequest::EventsSince {
                    after,
                    limit,
                    task,
                    machine: Some(me),
                },
                None,
            )
            .await?;
            // The head has cut the page already; the cut here keeps anything
            // else from passing should it not have.
            return match serde_json::from_slice::<IpcResponse>(&reply) {
                Ok(IpcResponse::Events(mut page)) => {
                    page.events.retain(|e| e.is_about(machine));
                    reply_line(&IpcResponse::Events(page))
                }
                Ok(IpcResponse::Error { .. }) => Ok(reply),
                _ => refuse(
                    NOT_ALLOWED_FOR_SHEPHERD,
                    "the head answered events with something else",
                ),
            };
        }
        IpcRequest::ArtifactLocate { .. } => {
            return refuse(
                NOT_ALLOWED_FOR_SHEPHERD,
                "an artifact comes with its bytes: ask through the running shepherd bridge",
            );
        }
        other => {
            return refuse(
                NOT_ALLOWED_FOR_SHEPHERD,
                format!(
                    "the shepherd on {machine} may ping, list its flock's tasks, show, read or end its machine's tasks, claim and report its tasks, read its saved trust, report its bootstrap and its workdir sets, fetch an artifact the head has cached, submit jobs and read its own events; not {}",
                    op_name(&other)
                ),
            );
        }
    };
    ask(socket, &req, None).await
}

/// A shepherd's job `dispatch` table, pinned to `machine`: `machine` set to
/// it and `flock` taken out, so the head files its tasks under the
/// machine's own flock (`Flock::task_flock`). A table that is not one is
/// refused.
fn pin_dispatch(machine: &str, dispatch: serde_json::Value) -> Result<serde_json::Value, String> {
    let mut table = match dispatch {
        serde_json::Value::Null => serde_json::Map::new(),
        serde_json::Value::Object(table) => table,
        _ => {
            return Err(format!(
                "the shepherd on {machine} sent a job whose dispatch is not a table"
            ));
        }
    };
    table.insert("machine".into(), machine.into());
    table.remove("flock");
    Ok(serde_json::Value::Object(table))
}

/// The request's `op`, as it crosses the socket.
fn op_name(req: &IpcRequest) -> String {
    serde_json::to_value(req)
        .ok()
        .and_then(|v| v.get("op").and_then(|op| op.as_str()).map(str::to_string))
        .unwrap_or_else(|| "this request".into())
}

/// `machine`'s flocks, as the head reports them: never read from local files,
/// which the head may not share. `None` when the head has no such machine,
/// or when a reload has already taken it out of the flock and is only
/// waiting for its actor to stop (`shutting_down`): its reported flock is a
/// fallback for display, not proof it still belongs there, and this key was
/// scoped to the machine, not to whichever flock a stale record falls into.
/// A head from before many flocks reports one, and one from before flocks
/// none, which leaves the machine unknown here as before.
async fn machine_flocks(socket: &Path, machine: &str) -> anyhow::Result<Option<Vec<String>>> {
    let reply = ask(socket, &IpcRequest::FlockList, None).await?;
    Ok(match serde_json::from_slice::<IpcResponse>(&reply) {
        Ok(IpcResponse::Machines(ms)) => ms
            .into_iter()
            .find(|m| m.name == machine)
            .filter(|m| !m.shutting_down)
            .filter(|m| m.flock.is_some() || !m.flocks.is_empty())
            .map(|m| m.flock_names()),
        _ => None,
    })
}

/// The `authorized_keys` line that lets `key` in only to run
/// `<pastor> bridge --agent --machine <machine>`, with no terminal and no
/// forwarding. `key` is one public key line (`ssh-ed25519 AAAA... comment`).
pub fn authorized_key_line(pastor: &Path, machine: &str, key: &str) -> anyhow::Result<String> {
    // sshd hands `command=` to the user's shell, so what goes in it is
    // checked against what the shell reads as nothing but words: the name as
    // pastor names machines, the path as `authkeys` allows one. The key is a
    // kind and base64 and may not bring options of its own.
    let (kind, b64) = crate::authkeys::check_public_key(key)?;
    let comment = key
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.split_whitespace().skip(2).collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    let pastor = crate::authkeys::check_pastor_path(pastor)?;
    if !crate::authkeys::valid_name(machine) {
        return Err(CliError::err(
            "invalid_machine",
            format!(
                "{machine:?} cannot go in an authorized_keys command: start with a letter or digit, then letters, digits, '.', '_' and '-'"
            ),
        ));
    }
    let key = if comment.is_empty() {
        format!("{kind} {b64}")
    } else {
        format!("{kind} {b64} {comment}")
    };
    Ok(format!(
        "command=\"{pastor} bridge --agent --machine {machine}\",{} {key}",
        crate::authkeys::restrict_options()
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
        /// What the head answers `artifact_locate` with: a reply to give
        /// back, or `artifact_not_cached` when none is set.
        artifact: Arc<Mutex<Option<serde_json::Value>>>,
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
        shutting_down_machine_json(name, flock, false)
    }

    fn shutting_down_machine_json(
        name: &str,
        flock: &str,
        shutting_down: bool,
    ) -> serde_json::Value {
        serde_json::json!({
            "name": name, "endpoint": "", "channel": "connected", "herdr_version": null,
            "protocol": null, "error": null, "live": 0, "max_agents": 2, "tags": [],
            "flock": flock, "shutting_down": shutting_down,
        })
    }

    /// A machine reporting several flocks and no single `flock`, as
    /// `Fleet::statuses` fills a head with many flocks.
    fn many_flocks_machine_json(name: &str, flocks: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "name": name, "endpoint": "", "channel": "connected", "herdr_version": null,
            "protocol": null, "error": null, "live": 0, "max_agents": 2, "tags": [],
            "flock": null, "shutting_down": false,
            "flocks": flocks.iter().map(|f| serde_json::json!({"name": f})).collect::<Vec<_>>(),
        })
    }

    /// A machine from a head that predates flocks: neither field is set.
    fn no_flock_info_machine_json(name: &str) -> serde_json::Value {
        serde_json::json!({
            "name": name, "endpoint": "", "channel": "connected", "herdr_version": null,
            "protocol": null, "error": null, "live": 0, "max_agents": 2, "tags": [],
            "flock": null, "shutting_down": false,
        })
    }

    fn fake_head() -> FakeHead {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("head.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let artifact: Arc<Mutex<Option<serde_json::Value>>> = Arc::default();
        let artifact_reply = artifact.clone();
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
                        shutting_down_machine_json("pi-3", "home", true),
                        many_flocks_machine_json("pi-4", &["a", "b"]),
                        no_flock_info_machine_json("pi-5"),
                    ]}),
                    "list" if v["filter"]["job"] == "boom" => {
                        serde_json::json!({"kind": "error", "data": {"code": "boom_error", "message": "no"}})
                    }
                    "list" => serde_json::json!({"kind": "tasks", "data": tasks}),
                    "task_show" if (1..=2).contains(&id) => {
                        serde_json::json!({"kind": "task", "data": tasks[id as usize - 1]})
                    }
                    "task_show" | "task_read" | "task_done" if !(1..=2).contains(&id) => {
                        serde_json::json!({"kind": "error", "data": {"code": "task_not_found", "message": "no"}})
                    }
                    "task_read" => serde_json::json!({"kind": "text", "data": "pane text"}),
                    "artifact_locate" => artifact_reply.lock().unwrap().clone().unwrap_or_else(|| {
                        serde_json::json!({"kind": "error", "data": {
                            "code": "artifact_not_cached", "message": "the cache does not hold it",
                        }})
                    }),
                    "shepherd_check" if v["machine"] == "pi-1" && v["generation"] == 3 => {
                        serde_json::json!({"kind": "text", "data": "current"})
                    }
                    "shepherd_check" => {
                        serde_json::json!({"kind": "error", "data": {"code": MACHINE_REVOKED, "message": "moved past"}})
                    }
                    // A head that cut nothing: the bridge's own cut shows.
                    "events_since" => {
                        let events: Vec<serde_json::Value> = tasks
                            .iter()
                            .zip(1..)
                            .map(|(t, seq)| serde_json::json!({
                                "seq": seq, "at": "2026-01-01T00:00:00Z", "type": "task.running",
                                "task": t, "job": null, "machine": null,
                            }))
                            .collect();
                        serde_json::json!({"kind": "events", "data": {
                            "gap": false, "oldest": 1, "newest": 2, "events": events,
                        }})
                    }
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
            artifact,
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

        /// What the shepherd bridge for `pi-1` answers `req`.
        async fn shepherd(&self, req: &IpcRequest) -> IpcResponse {
            self.shepherd_line(&request_line(req, None).unwrap()).await
        }

        async fn shepherd_line(&self, line: &str) -> IpcResponse {
            let reply = answer_shepherd(&self.socket, "pi-1", line.as_bytes())
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
        let resp = head
            .ask(
                &IpcRequest::TaskDone {
                    id: 1,
                    summary: None,
                },
                None,
            )
            .await;
        assert!(
            matches!(&resp, IpcResponse::Task(t) if t.id == 1),
            "{resp:?}"
        );
        assert_eq!(head.last()["op"], "task_done");
        head.ask(
            &IpcRequest::TaskDone {
                id: 1,
                summary: Some("done: pushed".into()),
            },
            None,
        )
        .await;
        assert_eq!(head.last()["summary"], "done: pushed", "passed on as sent");
        let resp = head.ask(&IpcRequest::Ping, None).await;
        assert!(matches!(resp, IpcResponse::Pong { .. }), "{resp:?}");
    }

    #[tokio::test]
    async fn an_agent_may_not_touch_a_task_on_another_machine() {
        let head = fake_head();
        for req in [
            IpcRequest::TaskShow { id: 2 },
            IpcRequest::TaskRead { id: 2, lines: 5 },
            IpcRequest::TaskDone {
                id: 2,
                summary: None,
            },
            IpcRequest::TaskDone {
                id: 9,
                summary: None,
            },
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
        for (req, op) in [
            (
                IpcRequest::TaskClose {
                    id: 1,
                    remove_worktree: false,
                },
                "task_close",
            ),
            (IpcRequest::TaskRetry { id: 1, place: None }, "task_retry"),
            (
                IpcRequest::TaskSend {
                    id: 1,
                    input: crate::machine::SendInput::default(),
                },
                "task_send",
            ),
            (
                IpcRequest::TaskPrune {
                    states: vec![],
                    older_than_secs: 0,
                },
                "task_prune",
            ),
            (IpcRequest::FlockList, "flock_list"),
            (
                IpcRequest::FlockRemove {
                    name: "home".into(),
                },
                "flock_remove",
            ),
            (IpcRequest::JobList, "job_list"),
            (IpcRequest::JobRun { name: "j".into() }, "job_run"),
            (IpcRequest::Reload, "reload"),
            (
                IpcRequest::WorkdirReport {
                    machine: "pi-1".into(),
                    reports: vec![],
                },
                "workdir_report",
            ),
            (
                IpcRequest::Tick {
                    job: None,
                    dry_run: true,
                },
                "tick",
            ),
        ] {
            let resp = head.ask(&req, Some("t-1")).await;
            assert!(refused(&resp), "{req:?}: {resp:?}");
            // The refusal names the actual op the agent tried, not a
            // placeholder: the message is how an agent tells requests apart.
            let IpcResponse::Error { message, .. } = &resp else {
                panic!("{resp:?}")
            };
            assert!(message.contains(op), "{op}: {message}");
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
    async fn a_machine_shutting_down_gets_no_flock_from_a_stale_key() {
        let head = fake_head();
        let line = request_line(
            &IpcRequest::List {
                filter: TaskFilter::default(),
            },
            None,
        )
        .unwrap();
        let reply = answer_agent(&head.socket, "pi-3", line.as_bytes())
            .await
            .unwrap();
        let resp: IpcResponse = serde_json::from_slice(&reply).unwrap();
        assert!(refused(&resp), "{resp:?}");
    }

    #[tokio::test]
    async fn machine_flocks_falls_back_to_flocks_then_excludes_a_head_with_neither() {
        let head = fake_head();
        // No single `flock`, but `flocks` carries several: read from there.
        let many = machine_flocks(&head.socket, "pi-4").await.unwrap();
        assert_eq!(many, Some(vec!["a".into(), "b".into()]));
        // Neither field set: a head from before flocks, machine unknown here.
        let neither = machine_flocks(&head.socket, "pi-5").await.unwrap();
        assert_eq!(neither, None, "{neither:?}");
    }

    #[tokio::test]
    async fn an_error_from_the_head_passes_through_a_list_unchanged() {
        let head = fake_head();
        let filter = TaskFilter {
            job: Some("boom".into()),
            ..TaskFilter::default()
        };
        let resp = head.ask(&IpcRequest::List { filter }, None).await;
        assert!(
            matches!(&resp, IpcResponse::Error { code, .. } if code == "boom_error"),
            "{resp:?}"
        );
    }

    #[tokio::test]
    async fn showing_a_task_asks_the_head_only_once() {
        let head = fake_head();
        head.ask(&IpcRequest::TaskShow { id: 1 }, None).await;
        assert_eq!(head.ops(), ["task_show"], "{:?}", head.ops());
    }

    #[tokio::test]
    async fn the_bridge_names_the_task_a_request_comes_from() {
        let head = fake_head();
        head.ask(
            &IpcRequest::TaskDone {
                id: 1,
                summary: None,
            },
            Some("t-2"),
        )
        .await;
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

    #[tokio::test]
    async fn a_connect_error_that_is_not_no_daemon_is_not_reported_as_one() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("head.sock");
        // A live socket with no permissions: connecting fails with a
        // permission error, not the "nothing is listening here" kind
        // `relay`'s guard checks for.
        let _listener = tokio::net::UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o000)).unwrap();
        let err = run(&socket, b"{}\n".as_slice(), &mut Vec::new())
            .await
            .unwrap_err();
        let err = err.downcast::<CliError>().unwrap();
        assert_eq!(err.code, "socket_denied", "{}", err.message);
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
            "command=\"/opt/pastor/bin/pastor bridge --agent --machine pi-1\",no-pty,no-user-rc,no-port-forwarding,no-agent-forwarding,no-X11-forwarding ssh-ed25519 AAAAC3Nza key@pi-1"
        );
        for bad in ["", "ssh-ed25519", "ssh-ed25519 A\nssh-rsa B"] {
            assert!(
                authorized_key_line(Path::new("/p"), "pi-1", bad).is_err(),
                "{bad:?}"
            );
        }
        assert!(authorized_key_line(Path::new("/a b/pastor"), "pi-1", "ssh-ed25519 A").is_err());
    }

    #[test]
    fn the_authorized_key_line_accepts_a_minimal_two_token_key() {
        // Exactly a type and a key, with no comment: the boundary the count
        // check must let through, not just anything above it.
        let line =
            authorized_key_line(Path::new("/opt/pastor"), "pi-1", "ssh-ed25519 AAAA").unwrap();
        assert!(line.ends_with("ssh-ed25519 AAAA"), "{line}");
    }

    fn line_refused(pastor: &str, machine: &str, key: &str) -> String {
        authorized_key_line(Path::new(pastor), machine, key)
            .unwrap_err()
            .downcast::<CliError>()
            .unwrap()
            .code
    }

    /// The command goes through the user's shell, so a name that holds
    /// anything it reads as syntax would run a second command with the key.
    #[test]
    fn the_authorized_key_line_rejects_a_machine_name_the_shell_would_read() {
        for bad in [
            "", "pi 1", "pi\"1", "pi\\1", "pi\n1", "pi;id", "pi$(id)", "pi`id`", "pi|id", "pi&id",
            "pi'1", "pi>x", "pi*", "pi\t1", "-pi", "--agent", ".", "..", "pi/1", "é",
        ] {
            assert_eq!(
                line_refused("/opt/pastor", bad, "ssh-ed25519 A"),
                "invalid_machine",
                "{bad:?}"
            );
        }
        for ok in ["pi-1", "Build_01", "a.b", "9"] {
            authorized_key_line(Path::new("/opt/pastor"), ok, "ssh-ed25519 A").unwrap();
        }
    }

    #[test]
    fn the_authorized_key_line_rejects_a_pastor_path_the_shell_would_read() {
        for bad in [
            "/opt/pa;stor",
            "/opt/$(id)/pastor",
            "/opt/`id`/pastor",
            "/opt/pa|stor",
            "/opt/pa&stor",
            "/opt/pa\nstor",
            "/opt/pa stor",
            "/opt/pa'stor",
            "/opt/pa\"stor",
            "/opt/pa\\stor",
            "/opt/pa>stor",
            "/opt/$HOME/pastor",
            "/opt/pa*",
            "pastor",
            "~/pastor",
        ] {
            assert_eq!(
                line_refused(bad, "pi-1", "ssh-ed25519 A"),
                "invalid_path",
                "{bad:?}"
            );
        }
    }

    /// A key is a kind and base64 and an optional comment: one that starts
    /// with options would bring its own `command=` or lift a restriction.
    #[test]
    fn the_authorized_key_line_rejects_a_key_that_brings_options() {
        for bad in [
            "command=\"id\" ssh-ed25519 AAAA",
            "no-pty ssh-ed25519 AAAA",
            "restrict ssh-ed25519 AAAA",
            "environment=\"X=1\" ssh-ed25519 AAAA",
            "from=\"*\",command=\"x\" ssh-ed25519 AAAA me",
            "rm ssh-ed25519 AAAA",
            "restrict AAAA",
            "rm AAAA",
            "command=\"id\" AAAA",
            "ssh-ed25519;id AAAA",
            "ssh-ed25519\"x AAAA",
            "SSH-ED25519 AAAA",
            "sk-ssh-ed25519 AAAA",
            "foo@openssh.com AAAA",
            "ssh-ed25519 AA;AA",
            "ssh-ed25519 AA$(id)",
            "ssh-ed25519 ====",
            "ssh-ed25519 AAA===",
            "ssh-ed25519 AAAA c\u{7}c",
            "ssh-ed25519 AAAA c\rc",
            "ssh-ed25519 AAAA\nssh-ed25519 BBBB",
        ] {
            assert_eq!(
                line_refused("/opt/pastor", "pi-1", bad),
                "invalid_key",
                "{bad:?}"
            );
        }
        let line = authorized_key_line(
            Path::new("/opt/pastor"),
            "pi-1",
            "sk-ssh-ed25519@openssh.com AAAAGnNr+/== my key comment",
        )
        .unwrap();
        assert!(
            line.ends_with(" sk-ssh-ed25519@openssh.com AAAAGnNr+/== my key comment"),
            "{line}"
        );
        for kind in ["ssh-rsa", "ecdsa-sha2-nistp256", "ssh-ed25519"] {
            authorized_key_line(Path::new("/opt/pastor"), "pi-1", &format!("{kind} AAAA="))
                .unwrap();
        }
    }

    fn refused_shepherd(resp: &IpcResponse) -> bool {
        matches!(resp, IpcResponse::Error { code, .. } if code == NOT_ALLOWED_FOR_SHEPHERD)
    }

    /// A claim, a report and a trust fetch go to the head for the bridge's
    /// own machine, whatever machine they name; the rest of each passes as
    /// sent, the claim's versions too.
    #[tokio::test]
    async fn shepherd_machine_field_is_overwritten() {
        let head = fake_head();
        head.shepherd(&IpcRequest::TaskClaim {
            machine: "pi-2".into(),
            free_slots: 3,
            flock_work: true,
            herdr_version: Some("0.9.1".into()),
            pastor_version: Some("0.10.0".into()),
            workdir_sets: true,
        })
        .await;
        let sent = head.last();
        assert_eq!(
            (&sent["op"], &sent["machine"], &sent["free_slots"]),
            (&"task_claim".into(), &"pi-1".into(), &3.into())
        );
        assert_eq!(sent["herdr_version"], "0.9.1");
        assert_eq!(sent["pastor_version"], "0.10.0");
        assert_eq!(sent["workdir_sets"], true, "{sent}");
        let report: IpcRequest = serde_json::from_value(serde_json::json!({
            "op": "task_report", "machine": "pi-2", "id": 2, "state": "done",
        }))
        .unwrap();
        head.shepherd(&report).await;
        assert_eq!(
            (
                &head.last()["op"],
                &head.last()["machine"],
                &head.last()["id"]
            ),
            (&"task_report".into(), &"pi-1".into(), &2.into())
        );
        head.shepherd(&IpcRequest::PullTrust {
            machine: "pi-2".into(),
        })
        .await;
        assert_eq!(head.last()["machine"], "pi-1");
        head.shepherd(&IpcRequest::BootstrapReport {
            machine: "pi-2".into(),
            result: crate::bootstrap::Outcome {
                ok: true,
                isolation: crate::bootstrap::Isolation::None,
                steps: vec![],
                machine_key: None,
                agent_key: None,
                host_key: None,
                herdr_version: None,
                pastor_version: None,
                check: None,
            },
        })
        .await;
        assert_eq!(
            (&head.last()["op"], &head.last()["machine"]),
            (&"bootstrap_report".into(), &"pi-1".into())
        );
        head.shepherd(&IpcRequest::WorkdirReport {
            machine: "pi-2".into(),
            reports: vec![],
        })
        .await;
        assert_eq!(
            (&head.last()["op"], &head.last()["machine"]),
            (&"workdir_report".into(), &"pi-1".into())
        );
    }

    /// A shepherd may not queue, change or end tasks, or change the fleet,
    /// beyond its own claims and reports: each is refused here and never
    /// reaches the head.
    #[tokio::test]
    async fn shepherd_refuses_task_run() {
        let head = fake_head();
        let run = r#"{"op":"run","id":1,"prompt":"p","spec":{"agent":"claude"}}"#;
        let resp = head.shepherd_line(run).await;
        assert!(refused_shepherd(&resp), "{resp:?}");
        for req in [
            IpcRequest::TaskClose {
                id: 1,
                remove_worktree: false,
            },
            IpcRequest::TaskRetry { id: 1, place: None },
            IpcRequest::FlockList,
            IpcRequest::Reload,
            IpcRequest::ShepherdCheck {
                machine: "pi-1".into(),
                generation: 3,
            },
        ] {
            let resp = head.shepherd(&req).await;
            assert!(refused_shepherd(&resp), "{req:?}: {resp:?}");
        }
        assert!(head.ops().is_empty(), "{:?}", head.ops());
        // What an agent there may ask, the shepherd may too.
        let resp = head.shepherd(&IpcRequest::TaskShow { id: 1 }).await;
        assert!(matches!(resp, IpcResponse::Task(_)), "{resp:?}");
        let resp = head.shepherd(&IpcRequest::TaskShow { id: 2 }).await;
        assert!(refused_shepherd(&resp), "another machine's task: {resp:?}");
    }

    /// A job the shepherd submits is pinned to its machine: `machine` set,
    /// and `flock` taken out so the head files its tasks under the
    /// machine's own flock. The rest of its dispatch passes as sent; one
    /// that is not a table is refused.
    #[tokio::test]
    async fn shepherd_job_submit_is_scoped_to_self() {
        let head = fake_head();
        head.shepherd(&IpcRequest::JobSubmit {
            job: "issues".into(),
            dispatch: serde_json::json!({"machine": "pi-2", "flock": "work", "agent": "claude"}),
            prompt: "p".into(),
            items: vec![serde_json::json!({"key": "a"})],
        })
        .await;
        let sent = head.last();
        assert_eq!(sent["op"], "job_submit");
        assert_eq!(
            sent["dispatch"],
            serde_json::json!({"machine": "pi-1", "agent": "claude"})
        );
        assert_eq!(sent["job"], "issues");
        head.shepherd(&IpcRequest::JobSubmit {
            job: "bare".into(),
            dispatch: serde_json::Value::Null,
            prompt: "p".into(),
            items: vec![],
        })
        .await;
        assert_eq!(
            head.last()["dispatch"],
            serde_json::json!({"machine": "pi-1"})
        );
        let before = head.ops().len();
        let resp = head
            .shepherd(&IpcRequest::JobSubmit {
                job: "odd".into(),
                dispatch: serde_json::json!(["pi-2"]),
                prompt: "p".into(),
                items: vec![],
            })
            .await;
        assert!(refused_shepherd(&resp), "{resp:?}");
        assert_eq!(head.ops().len(), before);
    }

    /// `EventsSince` asks the head for the machine's own records, and what
    /// comes back is cut to them here too.
    #[tokio::test]
    async fn shepherd_events_ask_for_the_machine() {
        let head = fake_head();
        let resp = head
            .shepherd(&IpcRequest::EventsSince {
                after: 0,
                limit: 10,
                task: None,
                machine: Some("pi-2".into()),
            })
            .await;
        assert_eq!(head.last()["machine"], "pi-1");
        let IpcResponse::Events(page) = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(page.events.iter().map(|e| e.seq).collect::<Vec<_>>(), [1]);
    }

    /// The bridge asks the head about its generation before each request,
    /// and a refusal is its last reply: the request is not passed on, and
    /// no later line is read.
    #[tokio::test]
    async fn shepherd_stale_generation_refused() {
        let head = fake_head();
        let ping = request_line(&IpcRequest::Ping, None).unwrap();
        let mut out = Vec::new();
        run_shepherd(
            &head.socket,
            std::path::Path::new("/nonexistent-cache"),
            "pi-1",
            3,
            BufReader::new(format!("{ping}\n{ping}\n").as_bytes()),
            &mut out,
        )
        .await
        .unwrap();
        assert_eq!(String::from_utf8(out).unwrap().lines().count(), 2);
        assert_eq!(
            head.ops(),
            ["shepherd_check", "ping", "shepherd_check", "ping"]
        );

        let head = fake_head();
        let mut out = Vec::new();
        run_shepherd(
            &head.socket,
            std::path::Path::new("/nonexistent-cache"),
            "pi-1",
            2,
            BufReader::new(format!("{ping}\n{ping}\n").as_bytes()),
            &mut out,
        )
        .await
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
        let resp: IpcResponse = serde_json::from_str(text.trim()).unwrap();
        assert!(
            matches!(&resp, IpcResponse::Error { code, .. } if code == MACHINE_REVOKED),
            "{resp:?}"
        );
        assert_eq!(head.ops(), ["shepherd_check"]);
        assert_eq!(head.last()["generation"], 2);
    }

    /// A machine the head has no enrollment for is refused the same way.
    #[tokio::test]
    async fn shepherd_unknown_machine_refused() {
        let head = fake_head();
        let ping = request_line(&IpcRequest::Ping, None).unwrap();
        let mut out = Vec::new();
        run_shepherd(
            &head.socket,
            std::path::Path::new("/nonexistent-cache"),
            "pi-9",
            3,
            BufReader::new(format!("{ping}\n").as_bytes()),
            &mut out,
        )
        .await
        .unwrap();
        let resp: IpcResponse = serde_json::from_slice(&out).unwrap();
        assert!(
            matches!(&resp, IpcResponse::Error { code, .. } if code == MACHINE_REVOKED),
            "{resp:?}"
        );
        assert_eq!(head.ops(), ["shepherd_check"]);
        assert_eq!(head.last()["machine"], "pi-9");
    }

    /// An artifact cache with one file of `bytes`, and the reply the head
    /// gives for it.
    fn cached(bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf, serde_json::Value) {
        use sha2::{Digest, Sha256};
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("artifacts");
        let file = cache.join("pastor/0.10.0/x86_64-unknown-linux-musl/pastor");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, bytes).unwrap();
        let sum: String = Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let reply = serde_json::json!({"kind": "artifact", "data": {
            "path": file, "sha256": sum, "size": bytes.len(),
        }});
        (dir, cache, reply)
    }

    fn locate_line() -> String {
        request_line(
            &IpcRequest::ArtifactLocate {
                name: "pastor".into(),
                version: "0.10.0".into(),
                target: "x86_64-unknown-linux-musl".into(),
            },
            None,
        )
        .unwrap()
    }

    /// The shepherd bridge's output for one `ArtifactLocate` line: the reply
    /// line, and what follows it.
    async fn fetched(head: &FakeHead, cache: &Path, line: &str) -> (IpcResponse, Vec<u8>) {
        let mut out = Vec::new();
        run_shepherd(
            &head.socket,
            cache,
            "pi-1",
            3,
            BufReader::new(format!("{line}\n").as_bytes()),
            &mut out,
        )
        .await
        .unwrap();
        let at = out.iter().position(|b| *b == b'\n').unwrap();
        let resp = serde_json::from_slice(&out[..at]).unwrap();
        (resp, out[at + 1..].to_vec())
    }

    /// A shepherd asks for an artifact with a line on stdin, the bridge
    /// takes no argument for it, and gets one reply line then the file's
    /// bytes, which hash to the sum on the line. The path the head gave stays
    /// out of the reply.
    #[tokio::test]
    async fn shepherd_fetch_reads_request_from_stdin_not_argv() {
        use sha2::{Digest, Sha256};
        // More than one buffer's worth, not a multiple of it.
        let bytes: Vec<u8> = (0..200_003u32).map(|i| (i % 251) as u8).collect();
        let (_dir, cache, reply) = cached(&bytes);
        let head = fake_head();
        *head.artifact.lock().unwrap() = Some(reply);
        let (resp, body) = fetched(&head, &cache, &locate_line()).await;
        let IpcResponse::Artifact { path, sha256, size } = resp else {
            panic!("not an artifact")
        };
        assert_eq!(path, "", "the head's path stays here");
        assert_eq!(size as usize, bytes.len());
        assert_eq!(body, bytes);
        let sum: String = Sha256::digest(&body)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(sum, sha256);
        assert_eq!(head.ops(), ["shepherd_check", "artifact_locate"]);
        let asked = head.last();
        assert_eq!(
            (&asked["name"], &asked["version"], &asked["target"]),
            (
                &serde_json::json!("pastor"),
                &serde_json::json!("0.10.0"),
                &serde_json::json!("x86_64-unknown-linux-musl")
            )
        );
        // A line, then bytes: a second request on the same stream is read
        // after the bytes, and its reply follows them.
        let mut out = Vec::new();
        let two = format!(
            "{}\n{}\n",
            locate_line(),
            request_line(&IpcRequest::Ping, None).unwrap()
        );
        run_shepherd(
            &head.socket,
            &cache,
            "pi-1",
            3,
            BufReader::new(two.as_bytes()),
            &mut out,
        )
        .await
        .unwrap();
        assert!(
            out.ends_with(b"\"kind\":\"pong\"}\n"),
            "{}",
            String::from_utf8_lossy(&out[out.len() - 80..])
        );
    }

    /// An artifact the cache does not hold is the head's error and nothing
    /// follows it.
    #[tokio::test]
    async fn shepherd_fetch_of_a_missing_artifact_is_an_error_without_bytes() {
        let (_dir, cache, _) = cached(b"x");
        let head = fake_head();
        let (resp, body) = fetched(&head, &cache, &locate_line()).await;
        assert!(
            matches!(&resp, IpcResponse::Error { code, .. } if code == "artifact_not_cached"),
            "{resp:?}"
        );
        assert!(body.is_empty());
    }

    /// A file outside the cache, named by the head or reached through a link
    /// in it, is never read: the reply is an error and nothing follows it.
    #[tokio::test]
    async fn shepherd_fetch_of_a_path_outside_the_cache_is_refused() {
        let (dir, cache, reply) = cached(b"inside");
        let outside = dir.path().join("secret");
        std::fs::write(&outside, b"not for the shepherd").unwrap();
        let named = |path: &Path| {
            serde_json::json!({"kind": "artifact", "data": {
                "path": path, "sha256": "00", "size": 20,
            }})
        };
        let head = fake_head();
        *head.artifact.lock().unwrap() = Some(named(&outside));
        let (resp, body) = fetched(&head, &cache, &locate_line()).await;
        assert!(
            matches!(&resp, IpcResponse::Error { code, .. } if code == "artifact_outside_cache"),
            "{resp:?}"
        );
        assert!(body.is_empty());
        // `..` out of the cache.
        *head.artifact.lock().unwrap() = Some(named(&cache.join("../secret")));
        let (resp, body) = fetched(&head, &cache, &locate_line()).await;
        assert!(
            matches!(&resp, IpcResponse::Error { code, .. } if code == "artifact_outside_cache")
        );
        assert!(body.is_empty());
        // A link inside the cache to a file outside it.
        let link = cache.join("pastor/0.10.0/link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        *head.artifact.lock().unwrap() = Some(named(&link));
        let (resp, body) = fetched(&head, &cache, &locate_line()).await;
        assert!(
            matches!(&resp, IpcResponse::Error { code, .. } if code == "artifact_outside_cache")
        );
        assert!(body.is_empty());
        // The honest reply still works.
        *head.artifact.lock().unwrap() = Some(reply);
        let (_, body) = fetched(&head, &cache, &locate_line()).await;
        assert_eq!(body, b"inside");
    }

    /// A file that is not the size the head said (it changed since) is not
    /// sent.
    #[tokio::test]
    async fn shepherd_fetch_of_a_file_that_changed_is_refused() {
        let (_dir, cache, reply) = cached(b"short");
        let mut reply = reply;
        reply["data"]["size"] = serde_json::json!(999);
        let head = fake_head();
        *head.artifact.lock().unwrap() = Some(reply);
        let (resp, body) = fetched(&head, &cache, &locate_line()).await;
        assert!(matches!(&resp, IpcResponse::Error { code, .. } if code == "artifact_changed"));
        assert!(body.is_empty());
    }

    /// Only a shepherd may ask, and only through its streaming bridge: an
    /// agent's bridge refuses the request, and the shepherd's line-only
    /// answer says where to ask.
    #[tokio::test]
    async fn only_the_shepherd_bridge_fetches_an_artifact() {
        let head = fake_head();
        let req = IpcRequest::ArtifactLocate {
            name: "pastor".into(),
            version: "0.10.0".into(),
            target: "t".into(),
        };
        assert!(refused(&head.ask(&req, None).await));
        let resp = head.shepherd(&req).await;
        assert!(
            matches!(&resp, IpcResponse::Error { code, .. } if code == NOT_ALLOWED_FOR_SHEPHERD),
            "{resp:?}"
        );
        assert!(head.ops().is_empty(), "never reached the head");
    }
}
