//! The events log: every `PastorEvent` the daemon broadcasts, stamped and
//! expanded into an `EventRecord` and appended to `events.jsonl` under the
//! state dir. `pastor events` reads the file, not the daemon, so it works with
//! the daemon down; `--follow` tails it.
//!
//! The JSON of `EventRecord` is also what connector event hooks get on stdin, so
//! it is a connector-facing format: fields are only ever added, never renamed.
//!
//! Every record the daemon builds carries `seq`, a number from the head's
//! store that only grows, across restarts and rotations, so a client can ask
//! for the records after the last one it saw (`since`, `IpcRequest::EventsSince`).

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::config::Paths;
use crate::machine::{MachineHandle, MachineStatus, PastorEvent};
use crate::store::Store;
use crate::task::{Task, parse_task_id};

/// Size at which `events.jsonl` is rotated to `events.jsonl.1`. One old
/// generation is kept, so the log never takes more than twice this.
pub const DEFAULT_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// Capacity of the bounded channel `spawn_log` feeds to `hooks::spawn`.
/// `hooks::spawn`'s loop only queues onto `Dispatcher::deliver`, which is
/// synchronous and itself bounds each connector's queue, so in practice this
/// drains instantly; it exists for the same reason the old `broadcast::
/// channel(1024)` hooks read from did, as a backstop against a stuck
/// connector. `spawn_log` uses `try_send` and drops a record for hooks (never
/// for `events.jsonl`) rather than block or grow without bound.
pub const HOOK_QUEUE_CAPACITY: usize = 1024;

/// How often `--follow` looks for new lines.
const FOLLOW_POLL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    /// The record's sequence number (`Store::next_event_seq`), from 1. A
    /// line written before numbering reads as 0.
    #[serde(default)]
    pub seq: u64,
    pub at: DateTime<Utc>,
    /// `task.done`, `job.failed`, `machine.lost`, ...
    #[serde(rename = "type")]
    pub kind: String,
    /// The full task row at the time the record was built.
    pub task: Option<Task>,
    /// The `[models]` name the task runs (`Task::model`); absent on a task
    /// that runs none and on events with no task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub job: Option<String>,
    /// Set on `machine.*` events: the machine's status at that moment.
    pub machine: Option<MachineStatus>,
    /// The event's own detail (`PastorEvent::detail`); absent when it has
    /// none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

/// Where `build` finds a machine's current status. The daemon's machine set
/// implements it; a caller with no machines at hand passes `None`.
pub trait MachineLookup: Send + Sync {
    fn machine_status(&self, name: &str) -> Option<MachineStatus>;
}

impl MachineLookup for [MachineHandle] {
    fn machine_status(&self, name: &str) -> Option<MachineStatus> {
        self.iter().find(|h| h.name == name).map(|h| h.snapshot())
    }
}

impl MachineLookup for Vec<MachineHandle> {
    fn machine_status(&self, name: &str) -> Option<MachineStatus> {
        self.as_slice().machine_status(name)
    }
}

impl MachineLookup for crate::daemon::Fleet {
    fn machine_status(&self, name: &str) -> Option<MachineStatus> {
        self.statuses().into_iter().find(|m| m.name == name)
    }
}

impl EventRecord {
    /// Stamp `ev` with the next sequence number and the time of receipt, and
    /// expand its ids into records: the task row for task events, the machine
    /// status for `machine.*` events. A row or machine that cannot be found
    /// leaves that field empty; the event is still recorded, and so is one
    /// whose number cannot be taken, as 0.
    pub fn build(
        ev: &PastorEvent,
        store: &Store,
        fleet: Option<&dyn MachineLookup>,
    ) -> EventRecord {
        let task = ev.task_id.and_then(|id| match store.get_task(id) {
            Ok(t) => t,
            Err(err) => {
                tracing::warn!(%err, id, "event record: cannot read task row");
                None
            }
        });
        let job = ev
            .job
            .clone()
            .or_else(|| task.as_ref().map(|t| t.job.clone()));
        let machine = if ev.kind.starts_with("machine.") {
            ev.machine
                .as_deref()
                .zip(fleet)
                .and_then(|(name, fleet)| fleet.machine_status(name))
        } else {
            None
        };
        let seq = store.next_event_seq().unwrap_or_else(|err| {
            tracing::error!(%err, kind = %ev.kind, "event record: cannot take a sequence number");
            0
        });
        EventRecord {
            seq,
            detail: ev.detail.clone(),
            at: Utc::now(),
            kind: ev.kind.clone(),
            model: task.as_ref().and_then(|t| t.model().map(str::to_string)),
            task,
            job,
            machine,
        }
    }

    fn task_id(&self) -> Option<i64> {
        self.task.as_ref().map(|t| t.id)
    }

    /// One human-readable line: time, type, and whatever the event is about.
    pub fn line(&self) -> String {
        let mut cols = vec![
            self.at.format("%Y-%m-%d %H:%M:%S").to_string(),
            format!("{:<17}", self.kind),
        ];
        if let Some(t) = &self.task {
            cols.push(t.display_id());
            if let Some(m) = &t.machine {
                cols.push(m.clone());
            }
            cols.push(t.state.to_string());
        }
        if let Some(m) = &self.machine {
            cols.push(m.name.clone());
            cols.push(m.channel.to_string());
            if let Some(e) = &m.error {
                cols.push(e.clone());
            }
        }
        if let Some(j) = &self.job {
            cols.push(format!("job={j}"));
        }
        if let Some(e) = self.task.as_ref().and_then(|t| t.error.as_ref()) {
            cols.push(e.clone());
        }
        if let Some(d) = &self.detail {
            cols.push(d.to_string());
        }
        cols.iter()
            .map(|c| crate::cli::one_line(c))
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_string()
    }
}

fn rotated(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".1");
    PathBuf::from(p)
}

/// Appends records to the log, one JSON line each, rotating by size.
pub struct LogWriter {
    path: PathBuf,
    max_bytes: u64,
}

impl LogWriter {
    pub fn new(path: PathBuf, max_bytes: u64) -> LogWriter {
        LogWriter { path, max_bytes }
    }

    /// Write one record. If the line would take the file past `max_bytes`, the
    /// file is first moved to `<path>.1` (replacing the previous one) and a new
    /// one started, so a record is never split across files.
    pub fn append(&self, rec: &EventRecord) -> anyhow::Result<()> {
        let mut line = serde_json::to_string(rec)?;
        line.push('\n');
        let size = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        if size > 0 && size + line.len() as u64 > self.max_bytes {
            std::fs::rename(&self.path, rotated(&self.path))?;
        }
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)?;
        f.write_all(line.as_bytes())?;
        Ok(())
    }
}

/// The daemon's log task: every event on `rx`, built into a record and
/// appended to `path`, then passed to `forward` (the hook runner). It is the
/// one place records are built, so the log and the hooks see the same
/// sequence number. A write that fails is logged and the record still goes
/// on; the task ends when the broadcast channel closes.
///
/// `forward` is bounded (`HOOK_QUEUE_CAPACITY`): a `try_send` that finds it
/// full drops that record for hooks, with a warning, rather than block this
/// task or grow the queue without bound; the record is still in
/// `events.jsonl`. Once `try_send` reports the receiver closed, `forward` is
/// cleared so later records stop paying for the check.
///
/// The fleet is held weakly: it owns the machine actors' command senders, the
/// actors own event senders, and this task only ends when every event sender
/// is gone. A strong reference would keep all of them alive after the daemon
/// is dropped. Once the fleet is gone, records are written without machine
/// status.
pub fn spawn_log(
    path: PathBuf,
    max_bytes: u64,
    store: Arc<Store>,
    fleet: Option<Weak<dyn MachineLookup>>,
    mut rx: broadcast::Receiver<PastorEvent>,
    mut forward: Option<mpsc::Sender<EventRecord>>,
) -> JoinHandle<()> {
    let writer = LogWriter::new(path, max_bytes);
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    tracing::info!(kind = %ev.kind, task = ?ev.task_id, machine = ?ev.machine, job = ?ev.job, "pastor event");
                    let lookup = fleet.as_ref().and_then(Weak::upgrade);
                    let rec = EventRecord::build(&ev, &store, lookup.as_deref());
                    if let Err(err) = writer.append(&rec) {
                        tracing::error!(%err, "events log: write failed");
                    }
                    if let Some(tx) = &forward {
                        // A finish command hangs on these two: wait for room
                        // rather than lose them (the runner drains at once).
                        let vital = matches!(rec.kind.as_str(), "task.done" | "task.failed");
                        let sent = if vital {
                            tx.send(rec)
                                .await
                                .map_err(|e| mpsc::error::TrySendError::Closed(e.0))
                        } else {
                            tx.try_send(rec)
                        };
                        match sent {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(rec)) => {
                                tracing::warn!(
                                    seq = rec.seq,
                                    kind = %rec.kind,
                                    "events log: hooks queue full; dropping record for hooks"
                                );
                            }
                            // The hook runner gone is not the log's problem.
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                forward = None;
                            }
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(n, "events log lagged; events dropped")
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    })
}

fn matches(rec: &EventRecord, task: Option<i64>) -> bool {
    task.is_none() || rec.task_id() == task
}

/// Parse one log line. A line that is not a record (a torn write, a hand
/// edit) is skipped with a warning rather than failing the whole read.
fn parse_line(line: &str) -> Option<EventRecord> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    match serde_json::from_str(line) {
        Ok(r) => Some(r),
        Err(err) => {
            tracing::warn!(%err, "events log: skipping unreadable line");
            None
        }
    }
}

fn open_existing(path: &Path) -> std::io::Result<Option<File>> {
    match File::open(path) {
        Ok(f) => Ok(Some(f)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn ino(f: &File) -> std::io::Result<u64> {
    Ok(f.metadata()?.ino())
}

fn read_file(f: File, task: Option<i64>, out: &mut Vec<EventRecord>) -> anyhow::Result<()> {
    for line in BufReader::new(f).lines() {
        if let Some(r) = parse_line(&line?)
            && matches(&r, task)
        {
            out.push(r);
        }
    }
    Ok(())
}

/// Every record in the log, oldest first, including the rotated generation.
/// A missing log is an empty one.
///
/// The current file is opened before `.1`. The writer only renames current
/// to `.1`, so `.1` opened second is either older than the handle we hold
/// or, if a rotation fell between the two opens, the very same file. Opening
/// `.1` first instead would miss a whole generation in that case.
pub fn read(path: &Path, task: Option<i64>) -> anyhow::Result<Vec<EventRecord>> {
    let current = open_existing(path)?;
    let old = open_existing(&rotated(path))?;
    read_generations(path, current, old, task)
}

/// `read` with both handles already open, in that order.
fn read_generations(
    path: &Path,
    current: Option<File>,
    old: Option<File>,
    task: Option<i64>,
) -> anyhow::Result<Vec<EventRecord>> {
    let mut out = Vec::new();
    let held = match (current, old) {
        // Rotated between the opens: our current handle is now `.1`, and
        // `path` is a newer generation.
        (Some(cur), Some(old)) if ino(&cur)? == ino(&old)? => Some(cur),
        (Some(cur), old) => {
            if let Some(old) = old {
                read_file(old, task, &mut out)?;
            }
            read_file(cur, task, &mut out)?;
            return Ok(out);
        }
        // No current file when we looked: it may have been renamed away just
        // before, with the new one not created yet.
        (None, old) => old,
    };
    let Some(held) = held else {
        return Ok(out);
    };
    let held_ino = ino(&held)?;
    read_file(held, task, &mut out)?;
    if let Some(new) = open_existing(path)?
        && ino(&new)? != held_ino
    {
        read_file(new, task, &mut out)?;
    }
    Ok(out)
}

/// What `since` found: the records after the cursor, and whether some were
/// lost to rotation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventsPage {
    /// Records with `seq` past the cursor, oldest first, at most the limit.
    pub events: Vec<EventRecord>,
    /// Records after the cursor were rotated out of both log files, so
    /// `events` does not start right after it.
    pub gap: bool,
    /// The oldest sequence number still in the log; `None` when it holds no
    /// numbered record.
    pub oldest: Option<u64>,
}

/// The records numbered after `after`, oldest first, at most `limit`, only
/// those about `task` if given. Lines from before numbering (seq 0) are never
/// returned. `gap` is decided on the whole log, whatever `task` is: it is set
/// when the oldest numbered record left is past `after + 1`.
pub fn since(path: &Path, after: u64, limit: u32, task: Option<i64>) -> anyhow::Result<EventsPage> {
    let all = read(path, None)?;
    let oldest = all.iter().map(|r| r.seq).filter(|&s| s > 0).min();
    let gap = oldest.is_some_and(|o| o > after.saturating_add(1));
    let events = all
        .into_iter()
        .filter(|r| r.seq > after && matches(r, task))
        .take(limit as usize)
        .collect();
    Ok(EventsPage {
        events,
        gap,
        oldest,
    })
}

/// An open log file being tailed, with the partial line read so far.
struct Tail {
    file: File,
    ino: u64,
    partial: String,
}

impl Tail {
    fn open(path: &Path) -> std::io::Result<Option<Tail>> {
        match File::open(path) {
            Ok(file) => {
                let ino = file.metadata()?.ino();
                Ok(Some(Tail {
                    file,
                    ino,
                    partial: String::new(),
                }))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Complete lines appended since the last call. A trailing line without a
    /// newline is held back until the writer finishes it.
    fn drain(&mut self) -> std::io::Result<Vec<String>> {
        let mut buf = String::new();
        self.file.read_to_string(&mut buf)?;
        self.partial.push_str(&buf);
        let mut lines = Vec::new();
        while let Some(i) = self.partial.find('\n') {
            lines.push(self.partial[..i].to_string());
            self.partial.drain(..=i);
        }
        Ok(lines)
    }
}

/// Print the log from the start (rotated generation first) and then every
/// record appended to it, until `on` returns false. Survives rotation: the
/// file being read is finished before the new one is opened, since the writer
/// only ever renames it. A log that does not exist yet is waited for.
pub async fn follow(
    path: &Path,
    task: Option<i64>,
    mut on: impl FnMut(&EventRecord) -> bool,
) -> anyhow::Result<()> {
    let mut emit = |line: &str| -> bool {
        match parse_line(line) {
            Some(r) if matches(&r, task) => on(&r),
            _ => true,
        }
    };
    let mut tail = Tail::open(path)?;
    // History from the rotated file, unless it is the very file we just
    // opened (a rotation between the two opens).
    if let Some(mut old) = Tail::open(&rotated(path))?
        && tail.as_ref().is_none_or(|t| t.ino != old.ino)
    {
        for line in old.drain()? {
            if !emit(&line) {
                return Ok(());
            }
        }
    }
    loop {
        if let Some(t) = tail.as_mut() {
            for line in t.drain()? {
                if !emit(&line) {
                    return Ok(());
                }
            }
            let current = std::fs::metadata(path).map(|m| m.ino()).ok();
            if current != Some(t.ino) {
                // Rotated (or removed): finish the old file, then switch.
                for line in t.drain()? {
                    if !emit(&line) {
                        return Ok(());
                    }
                }
                tail = Tail::open(path)?;
                continue;
            }
        } else {
            tail = Tail::open(path)?;
            if tail.is_some() {
                continue;
            }
        }
        tokio::time::sleep(FOLLOW_POLL).await;
    }
}

#[derive(clap::Args, Debug)]
pub struct EventsArgs {
    /// Keep printing new events as they are written (reads the file; works
    /// with the daemon down)
    #[arg(long)]
    pub follow: bool,
    /// Only events about this task, like t-12 or 12
    #[arg(long)]
    pub task: Option<String>,
    /// Print one JSON record per line, the same shape hooks get on stdin
    #[arg(long)]
    pub json: bool,
}

/// `pastor events`.
pub async fn cli(paths: &Paths, args: EventsArgs) -> anyhow::Result<()> {
    let task = args
        .task
        .as_deref()
        .map(|t| parse_task_id(t).ok_or_else(|| anyhow::anyhow!(crate::task::bad_task_id(t))))
        .transpose()?;
    let json = args.json;
    let print = move |r: &EventRecord| -> bool {
        let line = if json {
            serde_json::to_string(r).unwrap_or_default()
        } else {
            r.line()
        };
        // A closed stdout (`pastor events | head`) ends the command quietly.
        writeln!(std::io::stdout(), "{line}").is_ok()
    };
    let path = paths.events_file();
    if args.follow {
        follow(&path, task, print).await
    } else {
        for r in read(&path, task)? {
            if !print(&r) {
                break;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::RwLock;

    use tokio::sync::mpsc;

    use super::*;
    use crate::machine::ChannelState;
    use crate::store::NewTask;
    use crate::task::DispatchSpec;

    fn store_with_task(job: &str) -> (Store, Task) {
        let store = Store::open_in_memory().unwrap();
        let t = store
            .insert_task(NewTask {
                job: job.into(),
                item: serde_json::json!({"key": "k1", "title": "fix it"}),
                prompt: "do it".into(),
                spec: DispatchSpec {
                    agent: "claude".into(),
                    agent_args: vec![],
                    allow: vec![],
                    deny: vec![],
                    repo: None,
                    worktree: false,
                    branch: None,
                    machine: None,
                    tags: vec![],
                    timeout_secs: 60,
                    checkout: None,
                    reopen: None,
                    agent_source: None,
                    place: Default::default(),
                    session_id: None,
                },
                flock: "work".into(),
            })
            .unwrap();
        (store, t)
    }

    /// A hook routes work and personal notifications apart on the flock the
    /// record's task row carries.
    #[test]
    fn a_task_event_carries_the_flock() {
        let (store, t) = store_with_task("run");
        let rec = EventRecord::build(&ev("task.done", Some(t.id), None, None), &store, None);
        let v = serde_json::to_value(&rec).unwrap();
        assert_eq!(v["task"]["flock"], "work");
    }

    fn ev(
        kind: &str,
        task_id: Option<i64>,
        machine: Option<&str>,
        job: Option<&str>,
    ) -> PastorEvent {
        PastorEvent {
            detail: None,
            kind: kind.into(),
            task_id,
            machine: machine.map(Into::into),
            job: job.map(Into::into),
        }
    }

    fn handle(name: &str, channel: ChannelState) -> MachineHandle {
        let (tx, _rx) = mpsc::channel(1);
        MachineHandle {
            name: name.into(),
            max_agents: 2,
            tags: vec![],
            tx,
            status: Arc::new(RwLock::new(MachineStatus {
                name: name.into(),
                host: name.into(),
                endpoint: format!("ssh {name}"),
                channel,
                herdr_version: Some("0.9.1".into()),
                pastor_version: None,
                protocol: Some(22),
                error: Some("ssh: connection refused".into()),
                live: 1,
                max_agents: 2,
                tags: vec![],
                orphans: vec![],
                flock: None,
                shutting_down: false,
            })),
            task: None,
        }
    }

    #[test]
    fn a_task_event_carries_the_row_and_its_job() {
        let (store, t) = store_with_task("triage");
        let rec = EventRecord::build(&ev("task.queued", Some(t.id), None, None), &store, None);
        assert_eq!(rec.kind, "task.queued");
        assert_eq!(rec.task.as_ref().unwrap().id, t.id);
        assert_eq!(rec.job.as_deref(), Some("triage"));
        assert!(rec.machine.is_none());
        assert!((Utc::now() - rec.at).num_seconds() < 5);
    }

    #[test]
    fn a_missing_task_row_still_records_the_event() {
        let (store, _) = store_with_task("triage");
        let rec = EventRecord::build(&ev("task.done", Some(999), Some("m"), None), &store, None);
        assert!(rec.task.is_none());
        assert!(rec.job.is_none());
    }

    #[test]
    fn a_job_event_keeps_its_job() {
        let (store, _) = store_with_task("x");
        let rec = EventRecord::build(&ev("job.failed", None, None, Some("nightly")), &store, None);
        assert_eq!(rec.job.as_deref(), Some("nightly"));
        assert!(rec.task.is_none());
    }

    #[test]
    fn a_machine_event_carries_the_machine_status() {
        let (store, _) = store_with_task("x");
        let fleet = vec![
            handle("a", ChannelState::Connected),
            handle("b", ChannelState::Reconnecting),
        ];
        let rec = EventRecord::build(
            &ev("machine.lost", None, Some("b"), None),
            &store,
            Some(&fleet),
        );
        let m = rec.machine.unwrap();
        assert_eq!(m.name, "b");
        assert_eq!(m.channel, ChannelState::Reconnecting);
        // A task event on a machine does not repeat the machine record.
        let (store, t) = store_with_task("x");
        let rec = EventRecord::build(
            &ev("task.done", Some(t.id), Some("a"), None),
            &store,
            Some(&fleet),
        );
        assert!(rec.machine.is_none());
    }

    #[test]
    fn the_json_shape_is_stable() {
        let (store, t) = store_with_task("triage");
        let rec = EventRecord::build(&ev("task.queued", Some(t.id), None, None), &store, None);
        let v = serde_json::to_value(&rec).unwrap();
        let mut keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["at", "job", "machine", "seq", "task", "type"]);
        assert_eq!(v["seq"], 1);
        assert_eq!(v["type"], "task.queued");
        assert_eq!(v["task"]["item"]["title"], "fix it");
        assert_eq!(v["task"]["state"], "queued");
        let back: EventRecord = serde_json::from_value(v).unwrap();
        assert_eq!(back.kind, "task.queued");
    }

    fn record(kind: &str, task: Option<&Task>) -> EventRecord {
        EventRecord {
            seq: 0,
            detail: None,
            at: Utc::now(),
            kind: kind.into(),
            task: task.cloned(),
            job: task.map(|t| t.job.clone()),
            machine: None,
            model: None,
        }
    }

    #[test]
    fn the_writer_rotates_by_size_and_read_sees_both_generations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let (_, t) = store_with_task("j");
        let line_len = serde_json::to_string(&record("task.queued", Some(&t)))
            .unwrap()
            .len() as u64
            + 1;
        // Room for two records per file.
        let w = LogWriter::new(path.clone(), line_len * 2 + 10);
        for kind in ["task.queued", "task.started", "task.running"] {
            w.append(&record(kind, Some(&t))).unwrap();
        }
        let old = std::fs::read_to_string(rotated(&path)).unwrap();
        assert_eq!(old.lines().count(), 2);
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 1);
        let kinds: Vec<_> = read(&path, None)
            .unwrap()
            .into_iter()
            .map(|r| r.kind)
            .collect();
        assert_eq!(kinds, ["task.queued", "task.started", "task.running"]);
        let mode = std::fs::metadata(&path).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600);

        // A second rotation replaces the old generation: at most two files.
        for kind in ["task.done", "task.closed"] {
            w.append(&record(kind, Some(&t))).unwrap();
        }
        let kinds: Vec<_> = read(&path, None)
            .unwrap()
            .into_iter()
            .map(|r| r.kind)
            .collect();
        assert_eq!(kinds, ["task.running", "task.done", "task.closed"]);
    }

    #[test]
    fn read_filters_by_task_and_skips_bad_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        assert!(
            read(&path, None).unwrap().is_empty(),
            "no log is an empty log"
        );
        let (store, t1) = store_with_task("j");
        let mut t2 = t1.clone();
        t2.id = t1.id + 1;
        let w = LogWriter::new(path.clone(), DEFAULT_MAX_BYTES);
        w.append(&record("task.queued", Some(&t1))).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{not json\n\n")
            .unwrap();
        w.append(&record("task.queued", Some(&t2))).unwrap();
        w.append(&EventRecord::build(
            &ev("job.failed", None, None, Some("j")),
            &store,
            None,
        ))
        .unwrap();
        w.append(&record("task.done", Some(&t1))).unwrap();
        assert_eq!(read(&path, None).unwrap().len(), 4);
        let kinds: Vec<_> = read(&path, Some(t1.id))
            .unwrap()
            .into_iter()
            .map(|r| r.kind)
            .collect();
        assert_eq!(kinds, ["task.queued", "task.done"]);
    }

    #[test]
    fn the_human_line_names_the_subject() {
        let (_, mut t) = store_with_task("triage");
        t.machine = Some("pi-3".into());
        t.error = Some("agent exited".into());
        let line = record("task.failed", Some(&t)).line();
        for part in [
            &t.display_id(),
            "pi-3",
            "task.failed",
            "job=triage",
            "agent exited",
        ] {
            assert!(line.contains(part), "{line} lacks {part}");
        }
        let fleet = vec![handle("pi-3", ChannelState::Reconnecting)];
        let (store, _) = store_with_task("x");
        let line = EventRecord::build(
            &ev("machine.lost", None, Some("pi-3"), None),
            &store,
            Some(&fleet),
        )
        .line();
        for part in ["machine.lost", "pi-3", "reconnecting", "connection refused"] {
            assert!(line.contains(part), "{line} lacks {part}");
        }
    }

    #[tokio::test]
    async fn follow_prints_history_then_new_lines_across_a_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let (_, t) = store_with_task("j");
        let line_len = serde_json::to_string(&record("task.queued", Some(&t)))
            .unwrap()
            .len() as u64
            + 1;
        let w = LogWriter::new(path.clone(), line_len * 2 + 10);
        w.append(&record("task.queued", Some(&t))).unwrap();

        let (tx, mut seen) = mpsc::unbounded_channel();
        let p = path.clone();
        let follower = tokio::spawn(async move {
            follow(&p, None, |r| {
                tx.send(r.kind.clone()).unwrap();
                r.kind != "task.closed"
            })
            .await
        });
        let next = async |seen: &mut mpsc::UnboundedReceiver<String>| {
            tokio::time::timeout(Duration::from_secs(5), seen.recv())
                .await
                .unwrap()
                .unwrap()
        };
        assert_eq!(next(&mut seen).await, "task.queued");
        // Two more force a rotation mid-follow; then a line written in halves.
        for kind in ["task.started", "task.running", "task.done"] {
            w.append(&record(kind, Some(&t))).unwrap();
        }
        for want in ["task.started", "task.running", "task.done"] {
            assert_eq!(next(&mut seen).await, want);
        }
        let mut last = serde_json::to_string(&record("task.closed", Some(&t))).unwrap();
        last.push('\n');
        let (a, b) = last.split_at(10);
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(a.as_bytes()).unwrap();
        tokio::time::sleep(FOLLOW_POLL * 2).await;
        f.write_all(b.as_bytes()).unwrap();
        assert_eq!(next(&mut seen).await, "task.closed");
        tokio::time::timeout(Duration::from_secs(5), follower)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn follow_waits_for_a_log_that_does_not_exist_yet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let p = path.clone();
        let follower = tokio::spawn(async move {
            let mut got = None;
            follow(&p, None, |r| {
                got = Some(r.kind.clone());
                false
            })
            .await
            .unwrap();
            got
        });
        tokio::time::sleep(FOLLOW_POLL * 2).await;
        LogWriter::new(path, DEFAULT_MAX_BYTES)
            .append(&record("machine.connected", None))
            .unwrap();
        let got = tokio::time::timeout(Duration::from_secs(5), follower)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.as_deref(), Some("machine.connected"));
    }

    #[tokio::test]
    async fn the_log_task_writes_every_broadcast_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let (store, t) = store_with_task("triage");
        let store = Arc::new(store);
        let (tx, rx) = broadcast::channel(16);
        let fleet: Arc<dyn MachineLookup> = Arc::new(vec![handle("m", ChannelState::Connected)]);
        let log = spawn_log(
            path.clone(),
            DEFAULT_MAX_BYTES,
            store,
            Some(Arc::downgrade(&fleet)),
            rx,
            None,
        );
        tx.send(ev("task.queued", Some(t.id), None, None)).unwrap();
        tx.send(ev("machine.connected", None, Some("m"), None))
            .unwrap();
        drop(tx);
        tokio::time::timeout(Duration::from_secs(5), log)
            .await
            .unwrap()
            .unwrap();
        let recs = read(&path, None).unwrap();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].job.as_deref(), Some("triage"));
        assert_eq!(
            recs[1].machine.as_ref().unwrap().channel,
            ChannelState::Connected
        );
        drop(fleet);
    }

    /// A hooks queue that never drains (no `hooks::spawn` reading it, as when
    /// a connector hook is stuck) must not block the log task or panic it:
    /// `try_send` drops the record for hooks and the write to `events.jsonl`
    /// still goes through.
    #[tokio::test]
    async fn a_full_hooks_queue_is_dropped_without_blocking_the_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let (store, t) = store_with_task("triage");
        let store = Arc::new(store);
        let (tx, rx) = broadcast::channel(16);
        let (fwd, mut hooks_rx) = mpsc::channel(1);
        // Fill the one slot up front, so the log task's own send finds it full.
        fwd.try_send(record("task.queued", Some(&t))).unwrap();
        let log = spawn_log(path.clone(), DEFAULT_MAX_BYTES, store, None, rx, Some(fwd));
        tx.send(ev("task.blocked", Some(t.id), None, None)).unwrap();
        drop(tx);
        tokio::time::timeout(Duration::from_secs(5), log)
            .await
            .unwrap()
            .unwrap();
        // The write to events.jsonl happened regardless of the full queue.
        let recs = read(&path, None).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].kind, "task.blocked");
        // Only the pre-filled record made it to the hooks side; the log
        // task's own record was dropped for hooks, not queued or blocked on.
        assert_eq!(hooks_rx.try_recv().unwrap().kind, "task.queued");
        assert!(hooks_rx.try_recv().is_err());
    }

    /// A record a finish command hangs on is not dropped for a full queue:
    /// the log task waits for room and the record reaches the hooks side.
    #[tokio::test]
    async fn a_finishing_record_waits_for_room_in_the_hooks_queue() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let (store, t) = store_with_task("triage");
        let (tx, rx) = broadcast::channel(16);
        let (fwd, mut hooks_rx) = mpsc::channel(1);
        fwd.try_send(record("task.queued", Some(&t))).unwrap();
        let log = spawn_log(
            path,
            DEFAULT_MAX_BYTES,
            Arc::new(store),
            None,
            rx,
            Some(fwd),
        );
        tx.send(ev("task.done", Some(t.id), None, None)).unwrap();
        drop(tx);
        assert_eq!(hooks_rx.recv().await.unwrap().kind, "task.queued");
        assert_eq!(hooks_rx.recv().await.unwrap().kind, "task.done");
        tokio::time::timeout(Duration::from_secs(5), log)
            .await
            .unwrap()
            .unwrap();
    }

    /// The log task must not keep the fleet alive: the fleet holds the machine
    /// actors' command senders and the actors hold event senders, so a strong
    /// reference here is a cycle that keeps a dropped daemon's tasks running.
    #[tokio::test]
    async fn the_log_task_holds_the_fleet_weakly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let (store, _) = store_with_task("triage");
        let (tx, rx) = broadcast::channel(16);
        let fleet: Arc<dyn MachineLookup> = Arc::new(vec![handle("m", ChannelState::Connected)]);
        let weak = Arc::downgrade(&fleet);
        let log = spawn_log(
            path.clone(),
            DEFAULT_MAX_BYTES,
            Arc::new(store),
            Some(weak.clone()),
            rx,
            None,
        );
        drop(fleet);
        assert!(
            weak.upgrade().is_none(),
            "the log task kept the fleet alive"
        );
        // With the fleet gone, events are still written, without machine status.
        tx.send(ev("machine.lost", None, Some("m"), None)).unwrap();
        drop(tx);
        tokio::time::timeout(Duration::from_secs(5), log)
            .await
            .unwrap()
            .unwrap();
        let recs = read(&path, None).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].kind, "machine.lost");
        assert!(recs[0].machine.is_none());
    }

    #[test]
    fn the_human_line_keeps_a_multiline_error_on_one_line() {
        let (_, mut t) = store_with_task("triage");
        t.error = Some("ssh failed:\nPermission denied\r\nbye".into());
        let line = record("task.failed", Some(&t)).line();
        assert!(!line.contains('\n') && !line.contains('\r'), "{line:?}");
        assert!(
            line.contains(r"ssh failed:\nPermission denied\r\nbye"),
            "{line}"
        );
        let fleet = vec![handle("pi-3", ChannelState::Reconnecting)];
        fleet[0].status.write().unwrap().error = Some("lost\nstderr: boom".into());
        let (store, _) = store_with_task("x");
        let line = EventRecord::build(
            &ev("machine.lost", None, Some("pi-3"), None),
            &store,
            Some(&fleet),
        )
        .line();
        assert!(!line.contains('\n'), "{line:?}");
        assert!(line.contains(r"lost\nstderr: boom"), "{line}");
    }

    /// A rotation between `read`'s two opens: the handle taken on the current
    /// file now is `.1`. Both generations come back, in order, once each.
    #[test]
    fn read_across_a_rotation_between_the_opens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let (_, t) = store_with_task("j");
        let w = LogWriter::new(path.clone(), DEFAULT_MAX_BYTES);
        // An older generation that the rotation replaces.
        w.append(&record("task.queued", Some(&t))).unwrap();
        std::fs::rename(&path, rotated(&path)).unwrap();
        for kind in ["task.running", "task.done"] {
            w.append(&record(kind, Some(&t))).unwrap();
        }
        let current = open_existing(&path).unwrap();
        // The writer rotates now, and writes to a new file.
        std::fs::rename(&path, rotated(&path)).unwrap();
        w.append(&record("task.closed", Some(&t))).unwrap();
        let old = open_existing(&rotated(&path)).unwrap();
        let kinds: Vec<_> = read_generations(&path, current, old, None)
            .unwrap()
            .into_iter()
            .map(|r| r.kind)
            .collect();
        assert_eq!(kinds, ["task.running", "task.done", "task.closed"]);

        // No race: `.1` then the current file, as `read` returns them.
        let kinds: Vec<_> = read(&path, None)
            .unwrap()
            .into_iter()
            .map(|r| r.kind)
            .collect();
        assert_eq!(kinds, ["task.running", "task.done", "task.closed"]);
    }

    /// Push `kinds` through a log task writing to `path` with `store`, and
    /// wait for it to finish.
    async fn log_events(path: &Path, max_bytes: u64, store: Arc<Store>, kinds: &[&str]) {
        let (tx, rx) = broadcast::channel(16);
        let log = spawn_log(path.to_path_buf(), max_bytes, store, None, rx, None);
        for kind in kinds {
            tx.send(ev(kind, None, None, Some("j"))).unwrap();
        }
        drop(tx);
        tokio::time::timeout(Duration::from_secs(5), log)
            .await
            .unwrap()
            .unwrap();
    }

    /// Sequence numbers keep growing when the head restarts (a new store
    /// handle on the same database, a new log task) and when the log
    /// rotates; none is given twice.
    #[tokio::test]
    async fn seq_grows_across_a_restart_and_a_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let db = dir.path().join("pastor.db");
        let line_len = serde_json::to_string(&record("job.failed", None))
            .unwrap()
            .len() as u64
            + 40;
        // Room for about two records per file.
        let max = line_len * 2;
        log_events(
            &path,
            max,
            Arc::new(Store::open(&db).unwrap()),
            &["job.failed", "job.failed", "job.failed"],
        )
        .await;
        assert!(rotated(&path).exists(), "no rotation");
        // The head restarts.
        log_events(
            &path,
            max,
            Arc::new(Store::open(&db).unwrap()),
            &["job.failed", "job.failed"],
        )
        .await;
        let seqs: Vec<u64> = read(&path, None).unwrap().iter().map(|r| r.seq).collect();
        assert!(seqs.len() >= 2, "{seqs:?}");
        assert_eq!(*seqs.last().unwrap(), 5, "{seqs:?}");
        assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "{seqs:?}");
    }

    /// A line written before records were numbered still reads, as seq 0.
    #[test]
    fn a_line_without_seq_still_reads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        std::fs::write(
            &path,
            "{\"at\":\"2026-01-01T00:00:00Z\",\"type\":\"job.failed\",\"task\":null,\"job\":\"j\",\"machine\":null}\n",
        )
        .unwrap();
        let recs = read(&path, None).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].kind, "job.failed");
        assert_eq!(recs[0].seq, 0);
        // `since` never returns it: it has no number to be after.
        let page = since(&path, 0, 10, None).unwrap();
        assert!(page.events.is_empty());
        assert_eq!(page.oldest, None);
        assert!(!page.gap);
    }

    fn numbered(seq: u64, kind: &str, task: Option<&Task>) -> EventRecord {
        EventRecord {
            seq,
            ..record(kind, task)
        }
    }

    /// `since` returns what follows the cursor, oldest first, bounded by the
    /// limit and filtered by task, and says when rotation lost records after
    /// the cursor.
    #[test]
    fn since_reads_after_the_cursor_and_reports_a_gap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let (_, t1) = store_with_task("j");
        let mut t2 = t1.clone();
        t2.id = t1.id + 1;
        let line_len = serde_json::to_string(&numbered(10, "task.queued", Some(&t1)))
            .unwrap()
            .len() as u64
            + 1;
        let w = LogWriter::new(path.clone(), DEFAULT_MAX_BYTES);
        // A line from before numbering, then numbered ones.
        w.append(&record("task.queued", Some(&t1))).unwrap();
        for (seq, t) in [(1, &t1), (2, &t2), (3, &t1), (4, &t2), (5, &t1)] {
            w.append(&numbered(seq, "task.running", Some(t))).unwrap();
        }
        let seqs = |p: &EventsPage| p.events.iter().map(|r| r.seq).collect::<Vec<_>>();

        let page = since(&path, 0, 100, None).unwrap();
        assert_eq!(seqs(&page), [1, 2, 3, 4, 5]);
        assert!(!page.gap);
        assert_eq!(page.oldest, Some(1));
        let page = since(&path, 2, 2, None).unwrap();
        assert_eq!(seqs(&page), [3, 4], "oldest first, at most the limit");
        let page = since(&path, 1, 100, Some(t1.id)).unwrap();
        assert_eq!(seqs(&page), [3, 5]);
        let page = since(&path, 5, 100, None).unwrap();
        assert!(page.events.is_empty());
        assert!(!page.gap);

        // Two rotations push 1..=5 out of both files.
        let w = LogWriter::new(path.clone(), line_len * 2 + 10);
        for seq in 6..=10 {
            w.append(&numbered(seq, "task.running", Some(&t2))).unwrap();
        }
        let page = since(&path, 3, 100, None).unwrap();
        assert!(page.gap, "4 and 5 are gone");
        let oldest = page.oldest.unwrap();
        assert!(oldest > 5, "{oldest}");
        assert_eq!(page.events.first().unwrap().seq, oldest);
        assert_eq!(page.events.last().unwrap().seq, 10);
        // The gap is about the log, not the filter: t1 has nothing left.
        let page = since(&path, 3, 100, Some(t1.id)).unwrap();
        assert!(page.gap && page.events.is_empty());
        // A cursor right before the oldest record lost nothing.
        let page = since(&path, oldest - 1, 100, None).unwrap();
        assert!(!page.gap);
    }
}
