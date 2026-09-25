//! Connectors a plugin provides: its `[connector]` command behind the
//! `ItemSource` seam. A poll connector runs once per job run; a stream
//! connector is started once and kept alive, and each job run drains what it
//! emitted since the last one, plus whatever earlier batch the scheduler has
//! not acked yet.
//!
//! Both get the same handshake on stdin, one JSON line
//! `{"config": .., "cursor": .., "since": ..}`, and answer in JSON lines on
//! stdout: `item`, `cursor` and `log` objects. Anything else is skipped and
//! noted in the run log.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::SecondsFormat;
use serde_json::Value;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::{Item, ItemSource, RunFuture, RunInput, RunOutput};
use crate::config::Paths;
use crate::plugin::Plugin;
use crate::plugin::exec::{self, Invocation, RunLog, SharedLog};
use crate::plugin::manifest::Mode;

/// One stdout line, understood.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    Item(Item),
    Cursor(String),
    /// `level: message`.
    Log(String),
    /// Not a line pastor understands; the reason is for the run log.
    Bad(String),
}

/// `None` for a blank line, which is not worth a complaint.
pub fn parse_line(line: &str) -> Option<Line> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let mut obj = match serde_json::from_str::<Value>(line) {
        Ok(Value::Object(o)) => o,
        Ok(_) => return Some(Line::Bad("not a JSON object".into())),
        Err(e) => return Some(Line::Bad(format!("not JSON: {e}"))),
    };
    let kind = obj.remove("type");
    Some(match kind.as_ref().and_then(Value::as_str) {
        Some("item") => match obj.get("key") {
            Some(Value::String(k)) if !k.is_empty() => Line::Item(Item::new(k.clone(), obj)),
            _ => Line::Bad("item without a non-empty string key".into()),
        },
        Some("cursor") => match obj.get("value") {
            Some(Value::String(v)) => Line::Cursor(v.clone()),
            _ => Line::Bad("cursor without a string value".into()),
        },
        Some("log") => {
            let level = obj.get("level").and_then(Value::as_str).unwrap_or("info");
            let message = obj.get("message").and_then(Value::as_str).unwrap_or("");
            Line::Log(format!("{level}: {message}"))
        }
        Some(other) => Line::Bad(format!("unknown type {other:?}")),
        None => Line::Bad("no string \"type\"".into()),
    })
}

/// A poll connector's collected output, line by line.
#[derive(Default)]
struct Collected {
    out: RunOutput,
    n: usize,
}

impl Collected {
    fn take(&mut self, raw: &str, log: &SharedLog) {
        self.n += 1;
        match parse_line(raw) {
            None => {}
            Some(Line::Item(item)) => self.out.items.push(item),
            Some(Line::Cursor(c)) => self.out.cursor = Some(c),
            // Log records reach the scheduler's log and `plugin run`, so they
            // get the same redaction as stderr.
            Some(Line::Log(l)) => self.out.logs.push(lock(log).redact(&l)),
            Some(Line::Bad(why)) => {
                let note = format!("skipped stdout line {}: {why}", self.n);
                lock(log).line(&format!("[pastor: {note}]"));
                self.out.logs.push(format!("warn: {note}"));
            }
        }
    }
}

/// What a plugin's connector needs to run for one job.
#[derive(Clone)]
struct Runner {
    plugin: Arc<Plugin>,
    paths: Paths,
    /// `None` when resolved by id alone; the run log then goes under
    /// `runs/@<plugin id>/` and `PASTOR_JOB` is unset.
    job: Option<String>,
}

impl Runner {
    fn log_dir_name(&self) -> String {
        self.job
            .clone()
            .unwrap_or_else(|| format!("@{}", self.plugin.id))
    }

    /// The invocation and a fresh run log for it. Err when the `.env` does
    /// not parse or a directory cannot be made: the run fails, visibly.
    fn prepare(
        &self,
        input: &RunInput,
        timeout: Option<Duration>,
    ) -> Result<(Invocation, SharedLog), String> {
        let spec = self
            .plugin
            .manifest
            .connector
            .as_ref()
            .ok_or_else(|| format!("plugin {:?} has no connector", self.plugin.id))?;
        let (env, redactor) = self
            .plugin
            .command_env(&self.paths, self.job.as_deref())
            .map_err(|e| format!("{e:#}"))?;
        let log = RunLog::create(&self.paths.runs_dir(&self.log_dir_name()), redactor)
            .map_err(|e| format!("run log: {e:#}"))?;
        let handshake = serde_json::json!({
            "config": input.config,
            "cursor": input.cursor,
            "since": input.since.to_rfc3339_opts(SecondsFormat::Secs, true),
        });
        let mut stdin = serde_json::to_vec(&handshake).expect("json");
        stdin.push(b'\n');
        Ok((
            Invocation {
                argv: spec.command.clone(),
                cwd: self.plugin.dir.clone(),
                env,
                stdin,
                timeout,
            },
            log.shared(),
        ))
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The `ItemSource` for a plugin's connector, poll or stream by its manifest.
pub fn source(plugin: Arc<Plugin>, paths: Paths, job: Option<String>) -> Arc<dyn ItemSource> {
    let mode = plugin
        .manifest
        .connector
        .as_ref()
        .map(|c| c.mode)
        .unwrap_or_default();
    let runner = Runner { plugin, paths, job };
    match mode {
        Mode::Poll => Arc::new(PollSource { runner }),
        Mode::Stream => Arc::new(StreamSource::new(runner, STREAM_BACKOFF_BASE)),
    }
}

pub struct PollSource {
    runner: Runner,
}

impl ItemSource for PollSource {
    fn id(&self) -> &str {
        &self.runner.plugin.id
    }

    /// Exit 0 is success with whatever it printed; non-zero, a timeout or a
    /// failure to start is an error naming the run log, and its output is
    /// discarded so neither its items nor its cursor are half-applied.
    fn run<'a>(&'a self, input: RunInput) -> RunFuture<'a> {
        Box::pin(async move {
            let timeout = self
                .runner
                .plugin
                .manifest
                .connector
                .as_ref()
                .map(|c| c.timeout);
            let (inv, log) = self.runner.prepare(&input, timeout)?;
            let mut got = Collected::default();
            let done = exec::run(inv, log.clone(), |l| got.take(l, &log)).await;
            if done.exit.success() {
                Ok(got.out)
            } else {
                Err(format!(
                    "{} (log: {})",
                    done.reason(),
                    lock(&log).path().display()
                ))
            }
        })
    }
}

/// First restart delay of a stream connector; doubles per crash.
pub const STREAM_BACKOFF_BASE: Duration = Duration::from_secs(1);
pub const STREAM_BACKOFF_MAX: Duration = Duration::from_secs(300);
/// A stream that stayed up this long starts its backoff over.
pub const STREAM_HEALTHY_AFTER: Duration = Duration::from_secs(60);
/// Items held between drains; beyond it the oldest are dropped and logged.
pub const STREAM_BUFFER_MAX: usize = 10_000;
/// Log lines held between drains; beyond it the oldest are dropped, counted
/// in one note.
pub const STREAM_LOG_MAX: usize = 1000;

#[derive(Default)]
struct Buffer {
    items: std::collections::VecDeque<Item>,
    items_dropped: usize,
    /// The newest cursor since the last drain, handed to the scheduler.
    cursor: Option<String>,
    /// The newest cursor ever seen: what a restart hands back.
    latest_cursor: Option<String>,
    logs: std::collections::VecDeque<String>,
    logs_dropped: usize,
    /// Why the stream is not running right now, if it is not.
    down: Option<String>,
    /// What drains handed out and nobody has acked yet, each with the first
    /// batch it went out in: the process will not emit it again, so it goes
    /// out again with every drain until it is persisted. Every drain carries
    /// all of it, so batch `n` held exactly the entries numbered `<= n`.
    pending: Vec<(u64, Item)>,
    pending_cursor: Option<(u64, String)>,
    /// The number of the last drain.
    batch: u64,
}

/// A stream can run for days between two drains of an infrequent job, so
/// both queues are bounded and overflow is one counter each, not a line per
/// dropped entry.
impl Buffer {
    fn push_item(&mut self, item: Item) {
        if self.items.len() >= STREAM_BUFFER_MAX {
            self.items.pop_front();
            self.items_dropped += 1;
        }
        self.items.push_back(item);
    }

    fn push_log(&mut self, line: String) {
        if self.logs.len() >= STREAM_LOG_MAX {
            self.logs.pop_front();
            self.logs_dropped += 1;
        }
        self.logs.push_back(line);
    }

    /// Everything not yet acked: the unacked batch, then what arrived since,
    /// overflow notes first. Logs go out once.
    fn drain(&mut self) -> RunOutput {
        self.batch += 1;
        let batch = self.batch;
        let mut pending = std::mem::take(&mut self.pending);
        pending.extend(self.items.drain(..).map(|i| (batch, i)));
        if pending.len() > STREAM_BUFFER_MAX {
            let over = pending.len() - STREAM_BUFFER_MAX;
            pending.drain(..over);
            self.items_dropped += over;
        }
        if let Some(c) = self.cursor.take() {
            self.pending_cursor = Some((batch, c));
        }
        let items = pending.iter().map(|(_, i)| i.clone()).collect();
        self.pending = pending;
        let cursor = self.pending_cursor.as_ref().map(|(_, c)| c.clone());
        let mut logs = Vec::with_capacity(self.logs.len() + 2);
        if self.logs_dropped > 0 {
            logs.push(format!(
                "warn: {} stream log lines dropped (buffer holds {STREAM_LOG_MAX})",
                std::mem::take(&mut self.logs_dropped)
            ));
        }
        if self.items_dropped > 0 {
            logs.push(format!(
                "warn: stream buffer full ({STREAM_BUFFER_MAX}); dropped the {} oldest items",
                std::mem::take(&mut self.items_dropped)
            ));
        }
        logs.extend(self.logs.drain(..));
        RunOutput {
            items,
            cursor,
            logs,
            batch,
        }
    }

    /// Batch `batch` is persisted: forget what it held, and keep what a
    /// later drain added, since that run may still fail or be a dry run.
    fn ack(&mut self, batch: u64) {
        self.pending.retain(|(first, _)| *first > batch);
        if self
            .pending_cursor
            .as_ref()
            .is_some_and(|(first, _)| *first <= batch)
        {
            self.pending_cursor = None;
        }
    }
}

struct Running {
    config: Value,
    task: JoinHandle<()>,
}

/// A stream connector: one long-lived process per source, restarted with
/// backoff when it exits. `run` starts it on first use and afterwards
/// returns what it emitted since the previous call, so the job's schedule
/// sets how often items become tasks.
pub struct StreamSource {
    runner: Runner,
    base: Duration,
    buffer: Arc<Mutex<Buffer>>,
    running: Mutex<Option<Running>>,
}

impl StreamSource {
    fn new(runner: Runner, base: Duration) -> StreamSource {
        StreamSource {
            runner,
            base,
            buffer: Arc::default(),
            running: Mutex::new(None),
        }
    }

    /// A stream with a custom first backoff, for tests that watch restarts.
    pub fn with_backoff(
        plugin: Arc<Plugin>,
        paths: Paths,
        job: Option<String>,
        base: Duration,
    ) -> StreamSource {
        StreamSource::new(Runner { plugin, paths, job }, base)
    }

    /// (Re)start the supervisor when nothing runs or the job's config
    /// changed since it was started. A fresh start returns the receiver
    /// that turns true once the supervisor knows whether the process is up.
    fn ensure_started(&self, input: &RunInput) -> Option<watch::Receiver<bool>> {
        let mut running = lock(&self.running);
        if let Some(r) = running.as_ref()
            && r.config == input.config
            && !r.task.is_finished()
        {
            return None;
        }
        if let Some(old) = running.take() {
            old.task.abort();
        }
        {
            let mut b = lock(&self.buffer);
            if b.latest_cursor.is_none() {
                b.latest_cursor = input.cursor.clone();
            }
        }
        let (reported, known) = watch::channel(false);
        let task = tokio::spawn(supervise(
            self.runner.clone(),
            input.clone(),
            self.buffer.clone(),
            self.base,
            reported,
        ));
        *running = Some(Running {
            config: input.config.clone(),
            task,
        });
        Some(known)
    }
}

impl Drop for StreamSource {
    fn drop(&mut self) {
        if let Some(r) = lock(&self.running).take() {
            r.task.abort();
        }
    }
}

impl ItemSource for StreamSource {
    fn id(&self) -> &str {
        &self.runner.plugin.id
    }

    /// Drain the buffer. A stream that is down with nothing buffered is a
    /// failed run, so `job.failed` and the job's backoff apply to it too.
    /// The run that starts the process first waits, up to the connector's
    /// timeout, to hear whether it came up: otherwise a missing program or
    /// a broken `.env` would read as an empty, successful first run and
    /// only fail the next one.
    fn run<'a>(&'a self, input: RunInput) -> RunFuture<'a> {
        Box::pin(async move {
            if let Some(mut known) = self.ensure_started(&input) {
                let bound = self
                    .runner
                    .plugin
                    .manifest
                    .connector
                    .as_ref()
                    .map_or(STREAM_START_WAIT, |c| c.timeout);
                let _ = tokio::time::timeout(bound, known.wait_for(|k| *k)).await;
            }
            let mut b = lock(&self.buffer);
            if b.items.is_empty()
                && b.pending.is_empty()
                && let Some(why) = b.down.clone()
            {
                b.logs.clear();
                b.logs_dropped = 0;
                return Err(why);
            }
            Ok(b.drain())
        })
    }

    fn ack(&self, batch: u64) {
        lock(&self.buffer).ack(batch);
    }

    fn long_lived(&self) -> bool {
        true
    }
}

/// How long a stream's first run waits to hear whether it started, when
/// the plugin has no connector table to take the timeout from.
const STREAM_START_WAIT: Duration = Duration::from_secs(60);

/// Keep the stream's process running. `reported` turns true the first time
/// the process is up or has failed to start, whichever comes first.
async fn supervise(
    runner: Runner,
    first: RunInput,
    buffer: Arc<Mutex<Buffer>>,
    base: Duration,
    reported: watch::Sender<bool>,
) {
    let mut backoff = base;
    loop {
        let input = RunInput {
            cursor: lock(&buffer).latest_cursor.clone(),
            ..first.clone()
        };
        let started = Instant::now();
        let reason = match runner.prepare(&input, None) {
            Err(e) => e,
            Ok((inv, log)) => {
                let mut n = 0usize;
                let line_log = log.clone();
                let up = || {
                    lock(&buffer).down = None;
                    reported.send_replace(true);
                };
                let done = exec::run_started(inv, log.clone(), up, |raw| {
                    n += 1;
                    let mut b = lock(&buffer);
                    match parse_line(raw) {
                        None => {}
                        Some(Line::Item(item)) => b.push_item(item),
                        Some(Line::Cursor(c)) => {
                            b.latest_cursor = Some(c.clone());
                            b.cursor = Some(c);
                        }
                        Some(Line::Log(l)) => b.push_log(lock(&line_log).redact(&l)),
                        Some(Line::Bad(why)) => {
                            let note = format!("skipped stdout line {n}: {why}");
                            lock(&line_log).line(&format!("[pastor: {note}]"));
                            b.push_log(format!("warn: {note}"));
                        }
                    }
                })
                .await;
                format!(
                    "stream connector stopped: {} (log: {})",
                    done.reason(),
                    lock(&log).path().display()
                )
            }
        };
        if started.elapsed() >= STREAM_HEALTHY_AFTER {
            backoff = base;
        }
        tracing::warn!(plugin = %runner.plugin.id, job = ?runner.job, %reason, retry_in = ?backoff, "stream connector restarting");
        {
            let mut b = lock(&buffer);
            b.push_log(format!(
                "warn: {reason}; restarting in {}s",
                backoff.as_secs_f32()
            ));
            b.down = Some(reason);
        }
        reported.send_replace(true);
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(STREAM_BACKOFF_MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stream_buffer_caps_logs_and_items_with_one_note_each() {
        let mut b = Buffer::default();
        for i in 0..STREAM_LOG_MAX + 5 {
            b.push_log(format!("info: line {i}"));
        }
        for i in 0..STREAM_BUFFER_MAX + 3 {
            b.push_item(Item::new(format!("k{i}"), serde_json::Map::new()));
        }
        let out = b.drain();
        assert_eq!(out.items.len(), STREAM_BUFFER_MAX);
        assert_eq!(out.items[0].key, "k3", "the oldest items go");
        assert_eq!(out.logs.len(), STREAM_LOG_MAX + 2, "the cap plus two notes");
        assert_eq!(
            out.logs[0],
            "warn: 5 stream log lines dropped (buffer holds 1000)"
        );
        assert_eq!(
            out.logs[1],
            "warn: stream buffer full (10000); dropped the 3 oldest items"
        );
        assert_eq!(out.logs[2], "info: line 5", "the newest logs are kept");
        assert_eq!(
            out.logs.last().unwrap(),
            &format!("info: line {}", STREAM_LOG_MAX + 4)
        );
        // The counters start over after a drain.
        b.push_log("info: again".into());
        assert_eq!(b.drain().logs, vec!["info: again"]);
    }

    #[test]
    fn a_drained_batch_comes_back_until_it_is_acked() {
        let mut b = Buffer::default();
        b.push_item(Item::new("k1", serde_json::Map::new()));
        b.cursor = Some("c1".into());
        b.push_log("info: once".into());
        let first = b.drain();
        assert_eq!(first.items.len(), 1);
        b.push_item(Item::new("k2", serde_json::Map::new()));
        let again = b.drain();
        let keys: Vec<&str> = again.items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, vec!["k1", "k2"], "unacked first, then the new");
        assert_eq!(again.cursor.as_deref(), Some("c1"));
        assert!(again.logs.is_empty(), "logs go out once");
        b.ack(again.batch);
        let out = b.drain();
        assert!(out.items.is_empty() && out.cursor.is_none());
    }

    #[test]
    fn an_ack_clears_only_the_batch_it_names() {
        let mut b = Buffer::default();
        b.push_item(Item::new("a", serde_json::Map::new()));
        b.cursor = Some("c-a".into());
        let run_a = b.drain();
        b.push_item(Item::new("b", serde_json::Map::new()));
        b.cursor = Some("c-b".into());
        let run_b = b.drain();
        let keys: Vec<&str> = run_b.items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, vec!["a", "b"]);
        assert_ne!(run_a.batch, run_b.batch);
        // Run A persisted its batch; run B has not (a dry run, say).
        b.ack(run_a.batch);
        let next = b.drain();
        let keys: Vec<&str> = next.items.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, vec!["b"], "B's newer item stays pending");
        assert_eq!(next.cursor.as_deref(), Some("c-b"));
        b.ack(next.batch);
        let out = b.drain();
        assert!(out.items.is_empty() && out.cursor.is_none());
    }

    #[test]
    fn acking_the_later_batch_clears_the_earlier_too() {
        let mut b = Buffer::default();
        b.push_item(Item::new("a", serde_json::Map::new()));
        let run_a = b.drain();
        b.push_item(Item::new("b", serde_json::Map::new()));
        let run_b = b.drain();
        b.ack(run_b.batch);
        b.ack(run_a.batch);
        let out = b.drain();
        assert!(out.items.is_empty(), "{:?}", out.items);
    }

    #[test]
    fn parses_the_three_line_types() {
        let Some(Line::Item(item)) = parse_line(
            r#"{"type":"item","key":"1727000123.000200","title":"login broken","author":"ana","n":{"x":1}}"#,
        ) else {
            panic!()
        };
        assert_eq!(item.key, "1727000123.000200");
        assert_eq!(item.fields["title"], "login broken");
        assert_eq!(item.fields["n"]["x"], 1);
        assert!(
            item.fields.get("type").is_none(),
            "type is framing, not a field"
        );
        assert_eq!(
            parse_line(r#"{"type":"cursor","value":"c1"}"#),
            Some(Line::Cursor("c1".into()))
        );
        assert_eq!(
            parse_line(r#"{"type":"log","level":"info","message":"fetched 12"}"#),
            Some(Line::Log("info: fetched 12".into()))
        );
        assert_eq!(parse_line("   "), None);
    }

    #[test]
    fn bad_lines_say_why() {
        for (line, needle) in [
            ("not json", "not JSON"),
            ("[1,2]", "not a JSON object"),
            (r#"{"key":"k"}"#, "no string \"type\""),
            (r#"{"type":"item"}"#, "non-empty string key"),
            (r#"{"type":"item","key":""}"#, "non-empty string key"),
            (r#"{"type":"item","key":7}"#, "non-empty string key"),
            (r#"{"type":"cursor","value":1}"#, "string value"),
            (r#"{"type":"nope"}"#, "unknown type"),
        ] {
            let Some(Line::Bad(why)) = parse_line(line) else {
                panic!("{line}")
            };
            assert!(why.contains(needle), "{line}: {why}");
        }
    }
}
