//! What a pull or command machine owes the head after `workdir place|remove
//! --local`: a report of the set it placed or took out (`Reported`), sent
//! with the next request to the head. The head cannot reach these machines,
//! so this is how its record (`workdir list`) sees them.
//!
//! A report the head does not take yet waits in `<state>/workdir-reports/`,
//! one file each, and goes in order with the next send: a later `--local`
//! run, or, on a pull machine, the shepherd's next tick.

use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use super::workdir::{Placed, Set};
use super::workdir_push::{self, MAX_REPORTS, Reported};
use crate::cli::CliError;
use crate::config::Paths;
use crate::ipc::{IpcRequest, IpcResponse};
use crate::profile_render::Render;

/// The directory under the state directory where reports wait.
const DIR: &str = "workdir-reports";

/// A waiting report and the machine it is for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Queued {
    machine: String,
    report: Reported,
}

/// The report of a real `place`: the set, how it ended, when.
pub fn placed(set: &Set, render: Option<&Render>, p: &Placed) -> Reported {
    Reported::Placed {
        dir: p.dir.clone(),
        id: set.id.clone(),
        kind: set.kind.clone(),
        profile: set.profile.clone(),
        set_hash: set.hash(),
        render_hash: render.map(workdir_push::hash_of),
        at: Utc::now().to_rfc3339(),
        ok: p.ok,
        error: (!p.ok).then(|| workdir_push::problems(p)),
    }
}

/// The report of a real `remove`: the sets it took out, none when it took
/// out nothing.
pub fn removed(p: &Placed) -> Option<Reported> {
    let ids = workdir_push::taken_out(p, &[]);
    (!ids.is_empty()).then(|| Reported::Removed {
        dir: p.dir.clone(),
        ids,
        at: Utc::now().to_rfc3339(),
    })
}

fn queue_dir(paths: &Paths) -> PathBuf {
    paths.state_dir.join(DIR)
}

/// Keep `report` for `machine` until the head takes it.
pub fn queue(paths: &Paths, machine: &str, report: &Reported) -> anyhow::Result<PathBuf> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = queue_dir(paths);
    crate::config::create_private_dir(&dir)?;
    let nanos = Utc::now().timestamp_nanos_opt().unwrap_or_default();
    let name = format!(
        "{nanos:020}-{}-{}.json",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let file = dir.join(&name);
    let tmp = dir.join(format!("{name}.tmp"));
    let queued = Queued {
        machine: machine.to_string(),
        report: report.clone(),
    };
    std::fs::write(&tmp, serde_json::to_vec(&queued)?)?;
    std::fs::rename(&tmp, &file)?;
    Ok(file)
}

/// What waits, oldest first. A file that does not read is taken away: it
/// would block the ones behind it for good.
fn waiting(paths: &Paths) -> Vec<(PathBuf, Queued)> {
    let Ok(entries) = std::fs::read_dir(queue_dir(paths)) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    files
        .into_iter()
        .filter_map(|f| {
            let queued = std::fs::read_to_string(&f)
                .ok()
                .and_then(|t| serde_json::from_str::<Queued>(&t).ok());
            if queued.is_none() {
                let _ = std::fs::remove_file(&f);
            }
            queued.map(|q| (f, q))
        })
        .collect()
}

/// How many reports wait.
pub fn pending(paths: &Paths) -> usize {
    waiting(paths).len()
}

/// The lock one delivery holds over the queue, so two (the shepherd's tick
/// and a `--local` run, or two runs) never send the same files. `None` when
/// another holds it.
fn try_lock(paths: &Paths) -> anyhow::Result<Option<crate::edit::FileLock>> {
    use std::os::unix::io::AsRawFd;
    let dir = queue_dir(paths);
    crate::config::create_private_dir(&dir)?;
    let f = crate::edit::open_lock(&dir.join("delivery.lock"))?;
    loop {
        // SAFETY: flock on a descriptor this function owns.
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Some(crate::edit::FileLock::held(f)));
        }
        let e = std::io::Error::last_os_error();
        match e.kind() {
            std::io::ErrorKind::Interrupted => continue,
            std::io::ErrorKind::WouldBlock => return Ok(None),
            _ => return Err(anyhow::Error::from(e).context("lock the workdir reports")),
        }
    }
}

/// What `deliver` did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Delivery {
    /// Another delivery was under way and has the queue: nothing was sent.
    pub busy: bool,
    /// Reports the head took.
    pub sent: usize,
    /// Why the head was not reached, when it was not: the rest wait.
    pub waiting: Option<String>,
    /// What the head refused, one line each. A refused report is dropped:
    /// sending it again would be refused again.
    pub refused: Vec<String>,
}

/// Send the waiting reports to the head with `send`, oldest first and a
/// machine's together. A head that does not answer leaves them all, in
/// order, for the next time; one that refuses a batch loses it.
pub async fn deliver<F, Fut>(paths: &Paths, send: F) -> Delivery
where
    F: Fn(IpcRequest) -> Fut,
    Fut: Future<Output = anyhow::Result<IpcResponse>>,
{
    let mut out = Delivery::default();
    // Held until the queue is done, files deleted. A place queued meanwhile
    // waits for the next delivery.
    let _lock = match try_lock(paths) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            out.busy = true;
            return out;
        }
        Err(err) => {
            out.waiting = Some(format!("{err:#}"));
            return out;
        }
    };
    let mut queue = waiting(paths);
    while !queue.is_empty() {
        let machine = queue[0].1.machine.clone();
        let take = queue
            .iter()
            .take(MAX_REPORTS)
            .take_while(|(_, q)| q.machine == machine)
            .count();
        let batch: Vec<_> = queue.drain(..take).collect();
        let req = IpcRequest::WorkdirReport {
            machine: machine.clone(),
            reports: batch.iter().map(|(_, q)| q.report.clone()).collect(),
        };
        match send(req).await {
            Ok(_) => out.sent += batch.len(),
            Err(err) => {
                let code = err
                    .downcast_ref::<CliError>()
                    .map(|e| e.code.as_str())
                    .unwrap_or("runtime_error");
                if crate::shepherd::failed_on_the_way(code) {
                    out.waiting = Some(format!("{err:#}"));
                    return out;
                }
                out.refused.push(format!("{machine}: {err:#}"));
            }
        }
        for (file, _) in batch {
            let _ = std::fs::remove_file(file);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn paths() -> (tempfile::TempDir, Paths) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        (tmp, paths)
    }

    fn report(id: &str) -> Reported {
        Reported::Removed {
            dir: "/w".into(),
            ids: vec![id.into()],
            at: "2026-11-02T10:00:00Z".into(),
        }
    }

    fn ids(req: &IpcRequest) -> Vec<String> {
        let IpcRequest::WorkdirReport { reports, .. } = req else {
            panic!("{req:?}")
        };
        reports
            .iter()
            .map(|r| match r {
                Reported::Removed { ids, .. } => ids.join(","),
                other => panic!("{other:?}"),
            })
            .collect()
    }

    fn run<T>(f: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(f)
    }

    /// A head that is not reached leaves every report in place, in order;
    /// the next delivery sends them oldest first, a machine's together.
    #[test]
    fn reports_wait_for_a_silent_head_and_go_in_order() {
        let (_tmp, paths) = paths();
        for (machine, id) in [("a", "one"), ("a", "two"), ("b", "three"), ("a", "four")] {
            queue(&paths, machine, &report(id)).unwrap();
        }
        assert_eq!(pending(&paths), 4);

        let silent = run(deliver(&paths, |_| async {
            Err(CliError::err("head_unreachable", "ssh: no route"))
        }));
        assert_eq!(silent.sent, 0);
        assert!(silent.waiting.as_deref().unwrap().contains("no route"));
        assert_eq!(pending(&paths), 4);

        let seen = Mutex::new(Vec::new());
        let done = run(deliver(&paths, |req| {
            let IpcRequest::WorkdirReport { machine, .. } = &req else {
                panic!()
            };
            seen.lock().unwrap().push((machine.clone(), ids(&req)));
            async { Ok(IpcResponse::Text("recorded".into())) }
        }));
        assert_eq!(
            done,
            Delivery {
                sent: 4,
                ..Default::default()
            }
        );
        assert_eq!(
            *seen.lock().unwrap(),
            [
                ("a".to_string(), vec!["one".to_string(), "two".to_string()]),
                ("b".to_string(), vec!["three".to_string()]),
                ("a".to_string(), vec!["four".to_string()]),
            ]
        );
        assert_eq!(pending(&paths), 0);
    }

    /// Two deliveries do not send the same files: while one holds the queue,
    /// another sends nothing and says so.
    #[test]
    fn a_second_delivery_leaves_the_queue_to_the_first() {
        let (_tmp, paths) = paths();
        queue(&paths, "a", &report("one")).unwrap();
        let held = try_lock(&paths).unwrap().expect("free");
        let seen = Mutex::new(0);
        let busy = run(deliver(&paths, |_| {
            *seen.lock().unwrap() += 1;
            async { Ok(IpcResponse::Text("recorded".into())) }
        }));
        assert!(busy.busy, "{busy:?}");
        assert_eq!((busy.sent, *seen.lock().unwrap()), (0, 0));
        assert_eq!(pending(&paths), 1);
        drop(held);
        let done = run(deliver(&paths, |_| async {
            Ok(IpcResponse::Text("recorded".into()))
        }));
        assert_eq!((done.sent, done.busy), (1, false));
    }

    /// A delivery frees the queue when it ends, even while another copy of
    /// its lock's descriptor is open: the copy a child forked by another
    /// thread holds until it runs its program. A duplicate shares the open
    /// file the same way.
    #[test]
    fn the_queue_is_free_once_a_delivery_ends_though_a_copy_is_open() {
        let (_tmp, paths) = paths();
        let held = try_lock(&paths).unwrap().expect("free");
        let copy = held.file().try_clone().unwrap();
        drop(held);
        assert!(try_lock(&paths).unwrap().is_some(), "still locked");
        drop(copy);
    }

    /// A head that refuses a batch loses it, and the batches behind it still
    /// go; one that goes silent half way keeps what it did not take.
    #[test]
    fn a_refused_batch_is_dropped_and_the_rest_goes_on() {
        let (_tmp, paths) = paths();
        for (machine, id) in [("a", "one"), ("b", "two"), ("c", "three")] {
            queue(&paths, machine, &report(id)).unwrap();
        }
        let done = run(deliver(&paths, |req| {
            let IpcRequest::WorkdirReport { machine, .. } = &req else {
                panic!()
            };
            let answer = match machine.as_str() {
                "a" => Err(CliError::err(
                    "workdir_unsupported",
                    "the head places there",
                )),
                "b" => Ok(IpcResponse::Text("recorded".into())),
                _ => Err(CliError::err("timeout", "no answer")),
            };
            async { answer }
        }));
        assert_eq!(done.sent, 1);
        assert_eq!(done.refused.len(), 1, "{done:?}");
        assert!(done.refused[0].starts_with("a: "), "{done:?}");
        assert!(done.waiting.is_some());
        assert_eq!(pending(&paths), 1, "only c waits");
    }

    /// A file that does not read is dropped, not left to block the rest.
    #[test]
    fn a_broken_file_is_taken_away() {
        let (_tmp, paths) = paths();
        queue(&paths, "a", &report("one")).unwrap();
        std::fs::write(queue_dir(&paths).join("00000000000000000000-0-0.json"), "{").unwrap();
        assert_eq!(pending(&paths), 1);
        assert_eq!(std::fs::read_dir(queue_dir(&paths)).unwrap().count(), 1);
    }
}
