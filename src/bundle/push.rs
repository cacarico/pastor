//! Applying bundles from the head to a push machine: render its plan, pack
//! the plan with the versions it links into, unpack it in a run directory
//! on the machine, and run `pastor bundle apply --local` (or `bundle adopt
//! --local`) there, which answers with its report as JSON. A workdir set
//! goes the same way (`run_plan`), to `pastor workdir apply --local`.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use super::apply::{Adopt, Report};
use super::plan::{PLAN_FILE, Plan, STORE_DIR};
use super::store;
use crate::config::{Paths, create_private_dir};
use crate::herdr::transport::shell_quote;
use crate::provision::run::{self, Ran, Shell};

/// The line the machine prints before its report, so whatever its rc
/// files print first is skipped.
const MARK: &str = "pastor-bundle-apply-7c2e";

/// How long the apply on the machine may take.
const APPLY_TIMEOUT: Duration = Duration::from_secs(120);

/// An apply that gave no report: why, and whether pastor on the machine
/// started it, so it may have written before it was cut short.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failed {
    pub why: String,
    /// The run reached the machine's pastor: it printed the mark and its
    /// answer was not `nopastor`, or it was cut short (a timeout, too much
    /// output) where no one can tell how far it got. Only an apply that
    /// never got that far placed nothing for sure.
    pub started: bool,
}

impl Failed {
    fn before(why: String) -> Failed {
        Failed {
            why,
            started: false,
        }
    }
}

/// Apply `plan` on the machine behind `shell`, with `pastor` the program to
/// run there; with `adopt`, adopt the homes it names (`bundle adopt`).
pub async fn apply(
    paths: &Paths,
    shell: Arc<dyn Shell>,
    plan: &Plan,
    pastor: &str,
    dry_run: bool,
    adopt: Option<&Adopt>,
) -> Result<Report, Failed> {
    let mut flags = String::new();
    if dry_run {
        flags.push_str(" --dry-run");
    }
    // An older pastor on the machine refuses the adopt it does not know,
    // rather than applying the plan as a plain apply.
    let verb = match adopt {
        Some(a) => {
            for h in &a.homes {
                flags.push_str(&format!(" --home {}", shell_quote(h)));
            }
            "bundle adopt"
        }
        None => "bundle apply",
    };
    let run = Run {
        machine: &plan.machine,
        pastor,
        verb,
        flags: &flags,
        what: "bundles",
    };
    run_plan(paths, shell, run, |dir| write_set(paths, dir, plan)).await
}

/// One run of pastor on a push machine over a packed run directory.
pub(crate) struct Run<'a> {
    pub machine: &'a str,
    /// The program that runs pastor there.
    pub pastor: &'a str,
    /// Its subcommand, which takes `--local <run dir> --json`.
    pub verb: &'a str,
    /// What follows, each part shell-quoted, with a leading space.
    pub flags: &'a str,
    /// What the run applies, for the message when pastor is missing.
    pub what: &'a str,
}

/// Fill a run directory with `write`, unpack it on the machine behind
/// `shell`, run `pastor <verb> --local <dir> --json<flags>` there, and read
/// the JSON it answers with after the mark.
pub(crate) async fn run_plan<T: serde::de::DeserializeOwned>(
    paths: &Paths,
    shell: Arc<dyn Shell>,
    run: Run<'_>,
    write: impl FnOnce(&Path) -> anyhow::Result<()>,
) -> Result<T, Failed> {
    let machine = run.machine;
    let set = Scratch::new(paths).map_err(|e| Failed::before(format!("{e:#}")))?;
    write(&set.0).map_err(|e| Failed::before(format!("{e:#}")))?;
    let tarball = run::tar_dir(&set.0).await.map_err(Failed::before)?;
    drop(set);
    let dir = run::unpack(shell.clone(), machine, tarball)
        .await
        .map_err(|e| Failed::before(e.to_string()))?;
    let script = format!(
        r#"PATH="$HOME/.local/bin:$PATH"; export PATH; printf '%s\n' {MARK}; p={pastor}; if ! command -v "$p" >/dev/null 2>&1; then echo nopastor; exit 3; fi; exec "$p" {verb} --local {dir} --json{flags}"#,
        pastor = shell_quote(run.pastor),
        verb = run.verb,
        dir = shell_quote(dir.path()),
        flags = run.flags,
    );
    let ran = shell.run(script, Vec::new(), APPLY_TIMEOUT).await;
    dir.remove().await;
    // A run with no output to read may have been cut short after pastor
    // started writing: a timeout says nothing of how far it got.
    let ran = ran.map_err(|e| Failed {
        why: format!("machine {machine}: {e}"),
        started: true,
    })?;
    read_answer(machine, run.what, &ran)
}

/// The answer in what the run on the machine printed. With no mark (the
/// login shell or ssh failed first) or `nopastor`, pastor never ran; after
/// the mark, an answer that does not read comes from a run that may have
/// written.
fn read_answer<T: serde::de::DeserializeOwned>(
    machine: &str,
    what: &str,
    ran: &Ran,
) -> Result<T, Failed> {
    let stdout = String::from_utf8_lossy(&ran.stdout);
    let line = format!("{MARK}\n");
    let Some(answer) = stdout.find(&line).map(|i| &stdout[i + line.len()..]) else {
        return Err(Failed::before(format!(
            "machine {machine}: the apply gave no answer: {}",
            String::from_utf8_lossy(&ran.stderr).trim()
        )));
    };
    if answer.trim() == "nopastor" {
        return Err(Failed::before(format!(
            "machine {machine}: pastor is not on its PATH (nor in ~/.local/bin); install pastor there to apply {what}"
        )));
    }
    serde_json::from_str(answer).map_err(|_| {
        let err = String::from_utf8_lossy(&ran.stderr);
        Failed {
            why: format!(
                "machine {machine}: the apply failed: {}",
                err.trim().lines().last().unwrap_or(answer.trim())
            ),
            started: true,
        }
    })
}

/// The rendered set: the plan, and each version it links into under
/// `STORE_DIR/<bundle>/<hash>/`.
pub(crate) fn write_set(paths: &Paths, dir: &Path, plan: &Plan) -> anyhow::Result<()> {
    std::fs::write(dir.join(PLAN_FILE), serde_json::to_vec_pretty(plan)?)?;
    for v in &plan.versions {
        let from = paths.bundle_versions_dir(&v.bundle).join(&v.hash);
        let to = dir.join(STORE_DIR).join(&v.bundle).join(&v.hash);
        for rel in store::files(&from).map_err(|e| anyhow::anyhow!("{e}"))? {
            let dst = to.join(&rel);
            std::fs::create_dir_all(dst.parent().expect("a file has a parent"))?;
            std::fs::copy(from.join(&rel), &dst)?;
        }
    }
    Ok(())
}

/// A dot-named dir under the head's bundle versions root, removed when
/// dropped.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(paths: &Paths) -> anyhow::Result<Scratch> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = paths.bundle_versions_root();
        create_private_dir(&root)?;
        let dir = root.join(format!(
            ".render-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        create_private_dir(&dir)?;
        Ok(Scratch(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ran(stdout: &str) -> Ran {
        Ran {
            code: Some(0),
            stdout: stdout.as_bytes().to_vec(),
            stderr: b"boom".to_vec(),
        }
    }

    /// Only a run that never reached pastor counts as not started: no mark
    /// (the shell or ssh failed first), or `nopastor`. Anything else after
    /// the mark that is not a report may have written.
    #[test]
    fn an_answer_says_whether_the_apply_started() {
        let started = |stdout: &str| {
            read_answer::<Report>("m", "bundles", &ran(stdout))
                .unwrap_err()
                .started
        };
        assert!(!started("motd\n"));
        assert!(!started(&format!("{MARK}\nnopastor\n")));
        assert!(started(&format!("{MARK}\n{{\"machine\": \"m\", \"dry")));
        assert!(started(&format!("{MARK}\n")));
        let report: Report = read_answer(
            "m",
            "bundles",
            &ran(&format!(
                "motd\n{MARK}\n{{\"machine\": \"m\", \"dry_run\": false, \"rows\": []}}"
            )),
        )
        .unwrap();
        assert!(report.rows.is_empty());
    }
}
