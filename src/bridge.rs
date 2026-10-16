//! `pastor bridge`: a remote CLI's way to this machine's head. It runs over
//! ssh, so the head never listens on a network port; each request line on
//! stdin goes to the head's socket unread, and each reply goes back on stdout.
//!
//! `pastor bridge --agent --machine <name>` is the same way in for the agents
//! on another machine, locked by the `authorized_keys` line
//! (`authorized_key_line`) to that machine's own tasks: it reads each request
//! and passes on only what such an agent may ask (`answer_agent`).
//!
//! `pastor bridge --shepherd --machine <name>` is the way in for that
//! machine's shepherd (a pull machine's headless `pastor serve`), locked by
//! an `authorized_keys` line of its own (`authkeys::line` with
//! `Marker::Machine`). It passes on everything `answer_agent` does, plus
//! claiming and reporting that machine's tasks, its saved trust, job
//! submits and its own events (`answer_shepherd`); it never lets the
//! request choose another machine to act for.

use std::path::Path;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::artifacts::Artifact;
use crate::cli::{CliError, request_error};
use crate::ipc::{
    IpcRequest, IpcResponse, RequestError, connect_error_means_no_daemon, parse_request_line,
    relay_line, request_line,
};

/// The code a request an agent's bridge will not pass on is answered with.
pub const NOT_ALLOWED_FOR_AGENT: &str = "not_allowed_for_agent";

/// The code a request a shepherd's bridge will not pass on is answered with.
pub const NOT_ALLOWED_FOR_SHEPHERD: &str = "not_allowed_for_shepherd";

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

/// `run` for the shepherd on `machine`: each line goes through
/// `answer_shepherd`.
pub async fn run_shepherd(
    socket: &Path,
    machine: &str,
    input: impl AsyncBufRead + Unpin,
    output: impl AsyncWrite + Unpin,
) -> anyhow::Result<()> {
    serve(input, output, async |line: &[u8]| {
        answer_shepherd(socket, machine, line).await
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

/// What both `answer_agent` and `answer_shepherd` pass on as it is: `Ping`
/// always; `List`, cut to `machine`'s flock; and `TaskShow`, `TaskRead` and
/// `TaskDone` for a task placed on `machine`, as the head knows it. `None`
/// for anything else, which each caller refuses in its own words, with its
/// own code (`not_allowed`). The bridge names the task a request comes from
/// itself (`ipc::FROM_TASK_FIELD`): the task the request is about, whatever
/// the caller claimed, and none when it names no task.
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
                _ => {
                    return Ok(Some(refuse(
                        not_allowed,
                        "the head answered a task list with something else",
                    )?));
                }
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

/// The reply for one request line from the shepherd on `machine`: everything
/// `answer_agent` allows (`answer_common`), plus claiming and reporting its
/// own tasks, reading its saved trust, submitting jobs, and reading its own
/// events, reporting its bootstrap and locating artifacts. `TaskClaim`,
/// `TaskReport`, `PullTrust` and `BootstrapReport` get `machine` overwritten
/// with `machine`, whatever the request asked for, so a pull machine's
/// shepherd can only ever claim and report for itself (a hijacked one cannot
/// take another machine's tasks). A `JobSubmit`'s dispatch is pinned to
/// `machine` the same way, and refused when it picks the agent's arguments
/// or permissions or another flock (`pin_dispatch`); its job name is put
/// under `machine`'s (`shepherd_job_name`). `EventsSince` is cut to the
/// records about `machine`. Anything else is refused here and never reaches
/// the head.
pub async fn answer_shepherd(socket: &Path, machine: &str, line: &[u8]) -> anyhow::Result<Vec<u8>> {
    let req = match parse(line) {
        Ok(req) => req,
        Err(reply) => return reply,
    };
    if let Some(reply) = answer_common(socket, machine, NOT_ALLOWED_FOR_SHEPHERD, &req).await? {
        return Ok(reply);
    }
    match req {
        IpcRequest::TaskClaim {
            free_slots,
            flock_work,
            herdr_version,
            pastor_version,
            ..
        } => {
            ask(
                socket,
                &IpcRequest::TaskClaim {
                    machine: machine.to_string(),
                    free_slots,
                    flock_work,
                    herdr_version,
                    pastor_version,
                },
                None,
            )
            .await
        }
        IpcRequest::TaskReport {
            id,
            state,
            pane,
            detail,
            limit,
            trusted,
            ..
        } => {
            ask(
                socket,
                &IpcRequest::TaskReport {
                    machine: machine.to_string(),
                    id,
                    state,
                    pane,
                    detail,
                    limit,
                    trusted,
                },
                None,
            )
            .await
        }
        IpcRequest::PullTrust { .. } => {
            ask(
                socket,
                &IpcRequest::PullTrust {
                    machine: machine.to_string(),
                },
                None,
            )
            .await
        }
        IpcRequest::BootstrapReport { result, .. } => {
            ask(
                socket,
                &IpcRequest::BootstrapReport {
                    machine: machine.to_string(),
                    result,
                },
                None,
            )
            .await
        }
        // Read-only, and every part is checked by the head before it
        // becomes a path (`artifacts::Artifact::new`).
        IpcRequest::ArtifactLocate { .. } => ask(socket, &req, None).await,
        IpcRequest::JobSubmit {
            job,
            dispatch,
            prompt,
            items,
        } => {
            let job = match shepherd_job_name(machine, &job) {
                Ok(job) => job,
                Err(why) => return refuse(NOT_ALLOWED_FOR_SHEPHERD, why),
            };
            let dispatch = match pin_dispatch(socket, machine, dispatch).await? {
                Ok(dispatch) => dispatch,
                Err(why) => return refuse(NOT_ALLOWED_FOR_SHEPHERD, why),
            };
            ask(
                socket,
                &IpcRequest::JobSubmit {
                    job,
                    dispatch,
                    prompt,
                    items,
                },
                None,
            )
            .await
        }
        IpcRequest::EventsSince { after, limit, task } => {
            let reply = ask(
                socket,
                &IpcRequest::EventsSince { after, limit, task },
                None,
            )
            .await?;
            match serde_json::from_slice::<IpcResponse>(&reply) {
                Ok(IpcResponse::Events(mut page)) => {
                    page.events.retain(|e| event_machine(e) == Some(machine));
                    reply_line(&IpcResponse::Events(page))
                }
                Ok(IpcResponse::Error { .. }) => Ok(reply),
                _ => refuse(
                    NOT_ALLOWED_FOR_SHEPHERD,
                    "the head answered events with something else",
                ),
            }
        }
        other => refuse(
            NOT_ALLOWED_FOR_SHEPHERD,
            format!(
                "the shepherd on {machine} may ping, list its flock's tasks, show, read or end its machine's tasks, claim and report its tasks, read its saved trust, submit jobs, read its own events, report its bootstrap and locate artifacts; not {}",
                op_name(&other)
            ),
        ),
    }
}

/// `pastor bridge --shepherd --fetch`: asks the head on `socket` where its
/// cache keeps the artifact (`ArtifactLocate`), then streams that file to
/// `output` raw; the binary never crosses the head's socket, which caps a
/// request at 1 MiB. The path must resolve under `<state_dir>/artifacts`
/// (`invalid_artifact` otherwise), and the file is hashed and measured
/// again, from the same open handle that is then streamed, against what the
/// head answered (`artifact_checksum_mismatch`), so nothing is written to
/// `output` unless it is the file the head vouched for.
pub async fn fetch(
    socket: &Path,
    state_dir: &Path,
    name: &str,
    version: &str,
    target: &str,
    output: impl AsyncWrite + Unpin,
) -> anyhow::Result<()> {
    let req = IpcRequest::ArtifactLocate {
        name: name.into(),
        version: version.into(),
        target: target.into(),
    };
    let reply = ask(socket, &req, None).await?;
    let (path, sha256, size) = match serde_json::from_slice::<IpcResponse>(&reply) {
        Ok(IpcResponse::Artifact { path, sha256, size }) => (path, sha256, size),
        Ok(IpcResponse::Error { code, message }) => return Err(CliError::err(&code, message)),
        _ => {
            return Err(CliError::err(
                "runtime_error",
                "the head answered an artifact request with something else",
            ));
        }
    };
    let what = format!("{name} {version} {target}");
    let cache = state_dir.join("artifacts");
    let file =
        tokio::task::spawn_blocking(move || checked_file(&cache, &path, &sha256, size, &what))
            .await??;
    stream_exact(file, size, output).await
}

/// Writes exactly `size` bytes of `file` to `output`; a file that ends
/// sooner (cut short after it was hashed) is `artifact_checksum_mismatch`,
/// so the fetch exits non-zero instead of passing a short binary for the
/// whole one.
async fn stream_exact(
    file: impl std::io::Read,
    size: u64,
    mut output: impl AsyncWrite + Unpin,
) -> anyhow::Result<()> {
    // Plain std reads, a chunk at a time: tokio's `fs` is not built in, and
    // this process serves only this one stream, so a short blocking read
    // holds up nothing else.
    let mut file = std::io::Read::take(file, size);
    let mut buf = vec![0u8; 64 * 1024];
    let mut written = 0u64;
    loop {
        let n = std::io::Read::read(&mut file, &mut buf)?;
        if n == 0 {
            break;
        }
        output.write_all(&buf[..n]).await?;
        written += n as u64;
    }
    output.flush().await?;
    if written != size {
        return Err(CliError::err(
            "artifact_checksum_mismatch",
            format!("the head said {size} bytes, only {written} were sent"),
        ));
    }
    Ok(())
}

/// Opens `path` for reading, refusing a symlink in its last component: the
/// path was resolved first, so one swapped in since is never followed.
fn open_nofollow(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// What a pull machine's key runs. Its forced command is the fixed `bridge
/// --shepherd --machine X`, so whatever the shepherd asked for arrives in
/// `SSH_ORIGINAL_COMMAND` (`original`), split on whitespace and never run
/// through a shell. `Ok(None)` is the line bridge: `original` unset or
/// blank, or `bridge` alone. `Ok(Some)` is a fetch: exactly `bridge --fetch
/// NAME VERSION TARGET`, each part checked by `Artifact::new`. Either may
/// start with the pastor program (`pastor`, or a path ending `/pastor`) and
/// come wrapped as `sh -c '...'`, which is how the shepherd's own ssh sends
/// them (`ssh::Ssh::args`, `posix_command`); the program is never run, the
/// forced command picks it. Anything else is `Err` with the reason.
pub fn forced_fetch(original: Option<&str>) -> Result<Option<Artifact>, String> {
    let Some(original) = original.map(str::trim).filter(|o| !o.is_empty()) else {
        return Ok(None);
    };
    let inner = original
        .strip_prefix("sh -c '")
        .and_then(|rest| rest.strip_suffix('\''))
        .filter(|inner| !inner.contains('\''))
        .unwrap_or(original);
    let words: Vec<&str> = inner.split_whitespace().collect();
    let is_pastor = |w: &str| {
        (w == "pastor" || w.ends_with("/pastor"))
            && w.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/' | b'~'))
    };
    let words = match words.as_slice() {
        [first, rest @ ..] if is_pastor(first) => rest,
        all => all,
    };
    match words {
        ["bridge"] => Ok(None),
        ["bridge", "--fetch", name, version, target] => Artifact::new(name, version, target)
            .map(Some)
            .map_err(|e| e.to_string()),
        _ => Err(format!(
            "a shepherd's key runs only the bridge or `bridge --fetch NAME VERSION TARGET`, not {original:?}"
        )),
    }
}

/// The file at `path`, opened and rewound, once it is known to resolve under
/// `cache` and to hash to `sha256` at `size` bytes. One handle does both, so
/// a file swapped after the check is never the one sent.
fn checked_file(
    cache: &Path,
    path: &Path,
    sha256: &str,
    size: u64,
    what: &str,
) -> anyhow::Result<std::fs::File> {
    use sha2::{Digest, Sha256};
    use std::io::{Read, Seek};

    let invalid = |why: String| CliError::err("invalid_artifact", why);
    let cache = std::fs::canonicalize(cache)
        .map_err(|e| invalid(format!("no artifact cache at {}: {e}", cache.display())))?;
    let real =
        std::fs::canonicalize(path).map_err(|e| invalid(format!("{}: {e}", path.display())))?;
    if !real.starts_with(&cache) {
        return Err(invalid(format!(
            "{} is outside the artifact cache {}",
            path.display(),
            cache.display()
        )));
    }
    let mut file = open_nofollow(&real).map_err(|e| invalid(format!("{}: {e}", real.display())))?;
    let mismatch =
        |why: String| CliError::err("artifact_checksum_mismatch", format!("{what}: {why}"));
    let mut hasher = Sha256::new();
    let read = std::io::copy(&mut (&mut file).take(size + 1), &mut hasher)?;
    if read != size {
        return Err(mismatch(format!(
            "the head said {size} bytes, the file has {}",
            if read > size {
                "more".to_string()
            } else {
                read.to_string()
            }
        )));
    }
    let got: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if !got.eq_ignore_ascii_case(sha256) {
        return Err(mismatch(format!(
            "the head said sha256 {sha256}, the file has {got}"
        )));
    }
    file.rewind()?;
    Ok(file)
}

/// The `[dispatch]` keys a shepherd's `JobSubmit` may not set: they choose
/// the agent's arguments or permissions (`agent_args`, `allow`, `deny`,
/// `agent_source`) or the machines its tasks may land on (`tags`, `tag`).
/// `machine` is overwritten instead, and `flock` checked against the
/// machine's own (`pin_dispatch`). `profile` may be set: the head never
/// lets one go past what the machine's own profile allows (`unrestricted`
/// runs only where that is `unrestricted` too).
const SHEPHERD_DISPATCH_REFUSED: &[&str] =
    &["agent_args", "allow", "deny", "agent_source", "tags", "tag"];

/// The name a shepherd's job goes to the head under: `<machine>.<job>`, so
/// the head's `seen` keys (kept by job name) of one pull machine's jobs are
/// never another's, nor a head job's. A job name cannot hold `/`
/// (`config::job::check_name`), so the separator is `.`, and a `.` in the
/// job's own name is refused: a machine name may hold one, and `pi` with
/// `1.x` would otherwise be `pi.1` with `x`. A machine name that makes no
/// job name (upper case, too long) is left to the head to refuse.
fn shepherd_job_name(machine: &str, job: &str) -> Result<String, String> {
    if job.contains('.') {
        return Err(format!(
            "the shepherd on {machine} may not submit job {job:?}: a shepherd's job name holds no '.', which joins it to its machine's"
        ));
    }
    Ok(format!("{machine}.{job}"))
}

/// A shepherd's job `dispatch` table, pinned to `machine`: `machine` set to
/// it whatever was sent, and `Err` with the reason for a table that is not
/// one, sets a key in `SHEPHERD_DISPATCH_REFUSED`, or names a `flock`
/// `machine` is not in (as the head reports it, `machine_flocks`). A key set
/// to null is left for the head, which reads it as unset.
async fn pin_dispatch(
    socket: &Path,
    machine: &str,
    dispatch: serde_json::Value,
) -> anyhow::Result<Result<serde_json::Value, String>> {
    let serde_json::Value::Object(mut table) = dispatch else {
        return Ok(Err("a job's dispatch must be a table".into()));
    };
    let set = |v: &serde_json::Value| !v.is_null();
    if let Some(key) = SHEPHERD_DISPATCH_REFUSED
        .iter()
        .find(|k| table.get(**k).is_some_and(set))
    {
        return Ok(Err(format!(
            "the shepherd on {machine} may not set {key} in a job's dispatch"
        )));
    }
    if let Some(flock) = table.get("flock").filter(|v| set(v)) {
        let flocks = machine_flocks(socket, machine).await?.unwrap_or_default();
        if !flock
            .as_str()
            .is_some_and(|f| flocks.iter().any(|m| m == f))
        {
            return Ok(Err(format!(
                "the shepherd on {machine} may not send a job to flock {flock}, which {machine} is not in"
            )));
        }
    }
    table.insert("machine".into(), machine.into());
    Ok(Ok(serde_json::Value::Object(table)))
}

/// The machine an event record is about, for `EventsSince` filtering: a
/// `machine.*` event's own machine, else its task's. A record about neither
/// (a job event with no task) matches no machine.
fn event_machine(e: &crate::events::EventRecord) -> Option<&str> {
    e.machine
        .as_ref()
        .map(|m| m.name.as_str())
        .or_else(|| e.task.as_ref().and_then(|t| t.machine.as_deref()))
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
/// `machine` is checked the same way a marker is
/// (`authkeys::Marker::check`, code `invalid_machine`): `[A-Za-z0-9._-]+`
/// only, so nothing that goes unquoted into `command="..."` can break out
/// of it or run a second command (`$(id)`, `a;b`).
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
    let pastor = crate::authkeys::check_pastor_path(pastor)?;
    crate::authkeys::Marker::Agent(machine.to_string()).check()?;
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
    }

    fn task_json(id: i64, machine: &str, flock: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id, "job": "run", "item": null, "prompt": "p",
            "spec": {"agent": "claude"}, "machine": machine, "state": "running",
            "flock": flock,
            "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z",
        })
    }

    /// One `EventRecord`, about `task` when given (a `task.*` event).
    fn event_json(
        seq: u64,
        kind: &str,
        task: Option<serde_json::Value>,
        machine: Option<serde_json::Value>,
    ) -> serde_json::Value {
        serde_json::json!({
            "seq": seq, "at": "2026-01-01T00:00:00Z", "type": kind,
            "task": task, "job": null, "machine": machine,
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
                    "task_report" if tasks[id as usize - 1]["machine"] != v["machine"] => {
                        serde_json::json!({"kind": "error", "data": {"code": "not_on_machine", "message": "no"}})
                    }
                    "task_done" => {
                        serde_json::json!({"kind": "task", "data": tasks[id as usize - 1]})
                    }
                    "events_since" => serde_json::json!({"kind": "events", "data": {
                        "events": [
                            event_json(1, "task.done", Some(tasks[0].clone()), None),
                            event_json(2, "task.done", Some(tasks[1].clone()), None),
                            serde_json::json!({
                                "seq": 3, "at": "2026-01-01T00:00:00Z", "type": "job.failed",
                                "task": null, "job": "nightly", "machine": null,
                            }),
                        ],
                        "gap": false, "oldest": 1, "newest": 3,
                    }}),
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

        /// What the shepherd bridge for `machine` answers `req`.
        async fn ask_as_shepherd(&self, machine: &str, req: &IpcRequest) -> IpcResponse {
            let line = request_line(req, None).unwrap();
            let reply = answer_shepherd(&self.socket, machine, line.as_bytes())
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

    #[test]
    fn the_authorized_key_line_rejects_a_machine_name_it_cannot_quote() {
        for bad in [
            "", "pi 1", "pi\"1", "pi\\1", "pi\n1", "$(id)", "a;b", "`id`", "a|b", "..",
        ] {
            assert!(
                authorized_key_line(Path::new("/opt/pastor"), bad, "ssh-ed25519 A").is_err(),
                "{bad:?}"
            );
        }
    }

    fn refused_shepherd(resp: &IpcResponse) -> bool {
        matches!(resp, IpcResponse::Error { code, .. } if code == NOT_ALLOWED_FOR_SHEPHERD)
    }

    /// A pull machine's shepherd whose `TaskClaim` names another machine
    /// claims only for itself: the bridge overwrites `machine` with its own
    /// before the head ever sees the request, whatever the line asked for.
    #[tokio::test]
    async fn shepherd_machine_field_is_overwritten() {
        let head = fake_head();
        head.ask_as_shepherd(
            "pi-1",
            &IpcRequest::TaskClaim {
                machine: "other".into(),
                free_slots: 2,
                flock_work: false,
                herdr_version: None,
                pastor_version: None,
            },
        )
        .await;
        assert_eq!(head.last()["op"], "task_claim");
        assert_eq!(head.last()["machine"], "pi-1");
        assert_eq!(head.last()["free_slots"], 2);
    }

    /// A shepherd may not start work: `task run` is a person's, never a
    /// pull machine's.
    #[tokio::test]
    async fn shepherd_refuses_task_run() {
        let head = fake_head();
        let req: IpcRequest = serde_json::from_value(serde_json::json!({
            "op": "run", "prompt": "hi", "spec": {"agent": "claude"},
        }))
        .unwrap();
        let resp = head.ask_as_shepherd("pi-1", &req).await;
        let IpcResponse::Error { code, message } = &resp else {
            panic!("{resp:?}")
        };
        assert_eq!(code, NOT_ALLOWED_FOR_SHEPHERD);
        assert!(message.contains("run"), "{message}");
        assert!(head.ops().is_empty(), "{:?}", head.ops());
    }

    /// `EventsSince` through a shepherd's bridge is cut to the records
    /// about its own machine, whatever the head's log holds of others'.
    #[tokio::test]
    async fn shepherd_events_filtered() {
        let head = fake_head();
        let req = IpcRequest::EventsSince {
            after: 0,
            limit: 50,
            task: None,
        };
        let IpcResponse::Events(page) = head.ask_as_shepherd("pi-1", &req).await else {
            panic!()
        };
        assert_eq!(
            page.events
                .iter()
                .map(|e| e.task.as_ref().unwrap().id)
                .collect::<Vec<_>>(),
            [1]
        );
        let IpcResponse::Events(page) = head.ask_as_shepherd("pi-2", &req).await else {
            panic!()
        };
        assert_eq!(
            page.events
                .iter()
                .map(|e| e.task.as_ref().unwrap().id)
                .collect::<Vec<_>>(),
            [2]
        );
    }

    /// A pull machine runs its own jobs, so its shepherd may submit them.
    #[tokio::test]
    async fn shepherd_allows_job_submit() {
        let head = fake_head();
        let req = IpcRequest::JobSubmit {
            job: "nightly".into(),
            dispatch: serde_json::json!({}),
            prompt: "p".into(),
            items: vec![],
        };
        let resp = head.ask_as_shepherd("pi-1", &req).await;
        assert!(!refused_shepherd(&resp), "{resp:?}");
        assert_eq!(head.last()["op"], "job_submit");
        assert_eq!(head.last()["job"], "pi-1.nightly");
    }

    /// A shepherd's job runs on its own machine: whatever `machine` the
    /// submitted `[dispatch]` names, the head sees the shepherd's own.
    #[tokio::test]
    async fn shepherd_job_submit_pinned_to_self() {
        let head = fake_head();
        let req = IpcRequest::JobSubmit {
            job: "nightly".into(),
            dispatch: serde_json::json!({"agent": "claude", "machine": "other"}),
            prompt: "p".into(),
            items: vec![],
        };
        let resp = head.ask_as_shepherd("pi-1", &req).await;
        assert!(!refused_shepherd(&resp), "{resp:?}");
        assert_eq!(head.last()["op"], "job_submit");
        assert_eq!(head.last()["dispatch"]["machine"], "pi-1");
        assert_eq!(head.last()["dispatch"]["agent"], "claude");
        // No `machine` at all is pinned too.
        let req = IpcRequest::JobSubmit {
            job: "nightly".into(),
            dispatch: serde_json::json!({}),
            prompt: "p".into(),
            items: vec![],
        };
        head.ask_as_shepherd("pi-2", &req).await;
        assert_eq!(head.last()["dispatch"]["machine"], "pi-2");
    }

    /// A shepherd may not choose its job's agent arguments, permissions,
    /// tags or another flock: each is refused before the head sees it.
    #[tokio::test]
    async fn shepherd_job_submit_refuses_agent_args() {
        let head = fake_head();
        for dispatch in [
            serde_json::json!({"agent_args": ["--dangerously-skip-permissions"]}),
            serde_json::json!({"agent_args": []}),
            serde_json::json!({"allow": ["Bash(*)"]}),
            serde_json::json!({"deny": ["Read"]}),
            serde_json::json!({"agent_source": {"agent": "x"}}),
            serde_json::json!({"tags": ["gpu"]}),
            serde_json::json!({"tag": "gpu"}),
            serde_json::json!({"flock": "work"}),
            serde_json::json!("not a table"),
        ] {
            let req = IpcRequest::JobSubmit {
                job: "nightly".into(),
                dispatch: dispatch.clone(),
                prompt: "p".into(),
                items: vec![],
            };
            let resp = head.ask_as_shepherd("pi-1", &req).await;
            assert!(refused_shepherd(&resp), "{dispatch}: {resp:?}");
        }
        assert!(
            !head.ops().contains(&"job_submit".to_string()),
            "{:?}",
            head.ops()
        );
        // Its own flock is fine.
        let req = IpcRequest::JobSubmit {
            job: "nightly".into(),
            dispatch: serde_json::json!({"flock": "home"}),
            prompt: "p".into(),
            items: vec![],
        };
        let resp = head.ask_as_shepherd("pi-1", &req).await;
        assert!(!refused_shepherd(&resp), "{resp:?}");
        assert_eq!(head.last()["dispatch"]["flock"], "home");
        assert_eq!(head.last()["dispatch"]["machine"], "pi-1");
    }

    /// `TaskReport` and `PullTrust` act for the shepherd's own machine,
    /// whatever `machine` the line names.
    #[tokio::test]
    async fn shepherd_report_and_trust_machine_overwritten() {
        let head = fake_head();
        head.ask_as_shepherd(
            "pi-1",
            &IpcRequest::TaskReport {
                machine: "other".into(),
                id: 1,
                state: crate::task::TaskState::Running,
                pane: None,
                detail: None,
                limit: None,
                trusted: None,
            },
        )
        .await;
        assert_eq!(head.last()["op"], "task_report");
        assert_eq!(head.last()["machine"], "pi-1");
        head.ask_as_shepherd(
            "pi-1",
            &IpcRequest::PullTrust {
                machine: "other".into(),
            },
        )
        .await;
        assert_eq!(head.last()["op"], "pull_trust");
        assert_eq!(head.last()["machine"], "pi-1");
    }

    /// A report on another machine's task, even one that names that
    /// machine, reaches the head as the shepherd's own and is refused.
    #[tokio::test]
    async fn shepherd_report_on_another_machines_task_is_not_on_machine() {
        let head = fake_head();
        let resp = head
            .ask_as_shepherd(
                "pi-1",
                &IpcRequest::TaskReport {
                    machine: "pi-2".into(),
                    id: 2,
                    state: crate::task::TaskState::Done,
                    pane: None,
                    detail: None,
                    limit: None,
                    trusted: None,
                },
            )
            .await;
        assert!(
            matches!(&resp, IpcResponse::Error { code, .. } if code == "not_on_machine"),
            "{resp:?}"
        );
    }

    /// A job event names no machine, so no shepherd reads it.
    #[tokio::test]
    async fn shepherd_events_drop_a_job_event() {
        let head = fake_head();
        let req = IpcRequest::EventsSince {
            after: 0,
            limit: 50,
            task: None,
        };
        let IpcResponse::Events(page) = head.ask_as_shepherd("pi-1", &req).await else {
            panic!()
        };
        assert!(page.events.iter().all(|e| e.job.is_none()), "{page:?}");
        assert_eq!(page.events.len(), 1);
    }

    fn outcome() -> crate::bootstrap::Outcome {
        crate::bootstrap::Outcome {
            ok: true,
            mode: crate::bootstrap::Mode::User,
            isolation: crate::bootstrap::Isolation::None,
            steps: vec![],
            machine_key: None,
            agent_key: None,
            host_key: None,
            herdr_version: Some("0.9.5".into()),
            pastor_version: None,
        }
    }

    /// A shepherd's bootstrap report records only its own machine's state,
    /// whatever `machine` the line names.
    #[tokio::test]
    async fn shepherd_bootstrap_report_pinned_to_self() {
        let head = fake_head();
        let resp = head
            .ask_as_shepherd(
                "pi-1",
                &IpcRequest::BootstrapReport {
                    machine: "pi-2".into(),
                    result: outcome(),
                },
            )
            .await;
        assert!(!refused_shepherd(&resp), "{resp:?}");
        assert_eq!(head.last()["op"], "bootstrap_report");
        assert_eq!(head.last()["machine"], "pi-1");
        assert_eq!(head.last()["result"]["herdr_version"], "0.9.5");
    }

    /// A profile cannot widen what the machine's own allows (the head
    /// checks `unrestricted` against it), so a shepherd may name one.
    #[tokio::test]
    async fn shepherd_job_submit_allows_profile() {
        let head = fake_head();
        let req = IpcRequest::JobSubmit {
            job: "nightly".into(),
            dispatch: serde_json::json!({"profile": "review"}),
            prompt: "p".into(),
            items: vec![],
        };
        let resp = head.ask_as_shepherd("pi-1", &req).await;
        assert!(!refused_shepherd(&resp), "{resp:?}");
        assert_eq!(head.last()["op"], "job_submit");
        assert_eq!(head.last()["dispatch"]["profile"], "review");
        assert_eq!(head.last()["dispatch"]["machine"], "pi-1");
    }

    /// A shepherd's job reaches the head under its machine's name, so one
    /// pull machine cannot mark another's seen keys. A `.` in the job's own
    /// name would make `pi.1` + `x` and `pi` + `1.x` the same job, so it is
    /// refused.
    #[tokio::test]
    async fn shepherd_job_name_prefixed() {
        let head = fake_head();
        let submit = |job: &str| IpcRequest::JobSubmit {
            job: job.into(),
            dispatch: serde_json::json!({}),
            prompt: "p".into(),
            items: vec![],
        };
        head.ask_as_shepherd("pi-2", &submit("nightly")).await;
        assert_eq!(head.last()["job"], "pi-2.nightly");
        let resp = head.ask_as_shepherd("pi-1", &submit("1.nightly")).await;
        assert!(refused_shepherd(&resp), "{resp:?}");
        assert_eq!(head.ops(), ["job_submit"], "the refused one never went");
    }

    /// A pull machine fetches pastor and herdr from the head's cache.
    #[tokio::test]
    async fn shepherd_allows_artifact_locate() {
        let head = fake_head();
        let req = IpcRequest::ArtifactLocate {
            name: "herdr".into(),
            version: "0.9.5".into(),
            target: "linux-x86_64".into(),
        };
        let resp = head.ask_as_shepherd("pi-1", &req).await;
        assert!(!refused_shepherd(&resp), "{resp:?}");
        assert_eq!(head.last()["op"], "artifact_locate");
        assert_eq!(head.last()["target"], "linux-x86_64");
    }

    /// A head that answers every request with `reply`.
    fn fixed_head(reply: serde_json::Value) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("head.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (r, mut w) = stream.into_split();
                let mut line = String::new();
                BufReader::new(r).read_line(&mut line).await.unwrap();
                let mut out = serde_json::to_string(&reply).unwrap();
                out.push('\n');
                w.write_all(out.as_bytes()).await.unwrap();
            }
        });
        (dir, socket)
    }

    fn sha(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn located(path: &Path, sum: &str, size: u64) -> serde_json::Value {
        serde_json::json!({"kind": "artifact", "data": {
            "path": path, "sha256": sum, "size": size,
        }})
    }

    /// The file at the path the head names goes to the output raw, after
    /// its sum and size are checked again.
    #[tokio::test]
    async fn fetch_streams_the_file() {
        let state = tempfile::tempdir().unwrap();
        let dir = state.path().join("artifacts/herdr/0.9.5/linux-x86_64");
        std::fs::create_dir_all(&dir).unwrap();
        let body: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.join("herdr"), &body).unwrap();
        let (_d, socket) = fixed_head(located(&dir.join("herdr"), &sha(&body), body.len() as u64));
        let mut out = Vec::new();
        fetch(
            &socket,
            state.path(),
            "herdr",
            "0.9.5",
            "linux-x86_64",
            &mut out,
        )
        .await
        .unwrap();
        assert_eq!(out, body);

        // A sum or size that does not match is refused, and nothing written.
        for (sum, size) in [(sha(b"other"), body.len() as u64), (sha(&body), 3)] {
            let (_d, socket) = fixed_head(located(&dir.join("herdr"), &sum, size));
            let mut out = Vec::new();
            let err = fetch(
                &socket,
                state.path(),
                "herdr",
                "0.9.5",
                "linux-x86_64",
                &mut out,
            )
            .await
            .unwrap_err();
            let err = err.downcast::<CliError>().unwrap();
            assert_eq!(err.code, "artifact_checksum_mismatch", "{}", err.message);
            assert!(out.is_empty());
        }

        // An error from the head comes back with its own code.
        let (_d, socket) = fixed_head(serde_json::json!({"kind": "error", "data": {
            "code": "invalid_artifact", "message": "no",
        }}));
        let err = fetch(&socket, state.path(), "herdr", "..", "x", &mut Vec::new())
            .await
            .unwrap_err();
        assert_eq!(err.downcast::<CliError>().unwrap().code, "invalid_artifact");
    }

    /// Only a file under `<state>/artifacts` is ever streamed, however the
    /// path the head names gets there (a `..`, a symlink).
    #[tokio::test]
    async fn fetch_refuses_path_outside_cache() {
        let state = tempfile::tempdir().unwrap();
        let cache = state.path().join("artifacts");
        std::fs::create_dir_all(cache.join("herdr")).unwrap();
        let secret = state.path().join("pastor.db");
        std::fs::write(&secret, b"secret").unwrap();
        std::os::unix::fs::symlink(&secret, cache.join("herdr/link")).unwrap();
        for path in [
            secret.clone(),
            cache.join("herdr/../../pastor.db"),
            cache.join("herdr/link"),
        ] {
            let (_d, socket) = fixed_head(located(&path, &sha(b"secret"), 6));
            let mut out = Vec::new();
            let err = fetch(&socket, state.path(), "herdr", "0.9.5", "x", &mut out)
                .await
                .unwrap_err();
            let err = err.downcast::<CliError>().unwrap();
            assert_eq!(err.code, "invalid_artifact", "{path:?}: {}", err.message);
            assert!(out.is_empty(), "{path:?}");
        }
    }

    /// A stream that ends before `size` bytes is an error, so a file cut
    /// short after it was hashed never passes for the whole artifact.
    #[tokio::test]
    async fn a_short_stream_is_an_error() {
        let mut out = Vec::new();
        let err = stream_exact(&b"abc"[..], 10, &mut out).await.unwrap_err();
        let err = err.downcast::<CliError>().unwrap();
        assert_eq!(err.code, "artifact_checksum_mismatch", "{}", err.message);
        let mut out = Vec::new();
        stream_exact(&b"abcdef"[..], 3, &mut out).await.unwrap();
        assert_eq!(out, b"abc");
    }

    /// The cached file is opened without following a symlink, so one
    /// swapped in after the path was resolved is refused.
    #[test]
    fn the_cached_file_is_opened_without_following_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real"), b"x").unwrap();
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("link")).unwrap();
        assert!(open_nofollow(&dir.path().join("real")).is_ok());
        assert!(open_nofollow(&dir.path().join("link")).is_err());
    }

    /// SSH_ORIGINAL_COMMAND under the shepherd's forced command: unset or
    /// blank runs the line bridge, exactly `[pastor] bridge --fetch NAME
    /// VERSION TARGET` a fetch, and anything else is refused.
    #[test]
    fn forced_fetch_parses_only_the_fetch_line() {
        assert_eq!(forced_fetch(None), Ok(None));
        assert_eq!(forced_fetch(Some(" \t")), Ok(None));
        let want = Some(Artifact::new("herdr", "0.9.5", "linux-x86_64").unwrap());
        assert_eq!(
            forced_fetch(Some("bridge --fetch herdr 0.9.5 linux-x86_64")),
            Ok(want.clone())
        );
        assert_eq!(
            forced_fetch(Some("pastor bridge --fetch herdr 0.9.5 linux-x86_64")),
            Ok(want.clone())
        );
        assert_eq!(
            forced_fetch(Some(
                "sh -c '/opt/bin/pastor bridge --fetch herdr 0.9.5 linux-x86_64'"
            )),
            Ok(want)
        );
        // The shepherd's own line bridge, as its ssh sends it.
        for line in [
            "bridge",
            "pastor bridge",
            "sh -c 'pastor bridge'",
            "sh -c '~/.cargo/bin/pastor bridge'",
        ] {
            assert_eq!(forced_fetch(Some(line)), Ok(None), "{line}");
        }
        for bad in [
            "bridge --fetch herdr 0.9.5 linux-x86_64 x",
            "bridge --fetch herdr 0.9.5",
            "bridge --agent --machine pi-1",
            "bridge --machine pi-2 --fetch herdr 0.9.5",
            "bridge --fetch herdr .. linux-x86_64",
            "bridge --fetch herdr 0.9.5 x;id",
            "sh -c 'pastor bridge;id'",
            "sh -c 'pastor bridge' ; id",
            "sh -c 'pastor bridge' 'x'",
            "pastor pastor bridge",
            "/bin/sh bridge",
            "tick",
        ] {
            assert!(forced_fetch(Some(bad)).is_err(), "{bad}");
        }
    }
}
