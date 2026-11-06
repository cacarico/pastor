//! Workdir sets of pastor's own tasks (`task run --workdir-set`, a job's
//! `[dispatch] workdir_set`).
//!
//! A task carries its set's document (`DispatchSpec::workdir`). The head
//! checks it when the task is queued (`settle`), and the machine's actor
//! places it in the agent's folder before every start, through the head
//! (`Gate`, `HeadGate`), so the head records it as any place and keeps it.
//! When the task's worktree goes, the set is taken out of it there.
//!
//! The head does not reach a pull machine. When its serve says it places
//! sets itself (`TaskClaim::workdir_sets`), the head hands each task it
//! claims with what that takes (`for_claim`): the own set settled and its
//! profile rendered, and the set each stored bundle gives the agent's kind.
//! The serve's actor places them with `LocalGate` and reports them to the
//! head (`workdir_report`).

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Weak};

use super::keep::Reach;
use super::workdir::{Set, WORKDIR_INVALID, WORKDIR_UNSUPPORTED};
use super::workdir_push::{self, Head, WORKDIR_FAILED};
use crate::cli::CliError;
use crate::config::flock::{Flock, MachineConfig};
use crate::config::{AgentRefusal, PastorConfig, Paths};
use crate::daemon::Fleet;
use crate::profile_render::Render;
use crate::task::DispatchSpec;

/// The code of a task whose set is for another kind of agent than its own.
pub const WORKDIR_KIND_MISMATCH: &str = "workdir_kind_mismatch";

/// The code of a task whose set names another profile than the task runs.
pub const WORKDIR_PROFILE_MISMATCH: &str = "workdir_profile_mismatch";

/// Whether the head can place a set on `m`: a push machine it reaches with
/// a shell. A pull machine's own serve would have to place it, and a
/// `command` machine runs no shell.
pub fn takes_sets(m: &MachineConfig) -> bool {
    !m.pull && m.command.is_none()
}

/// The set `doc` of a task whose agent is of `kind` and runs `profile`,
/// checked and with the task's profile in it when it named none: the
/// launch flags and the set's rules then come from one profile. Refused
/// when it does not read as a set (`workdir_invalid`), is for another kind
/// (`workdir_kind_mismatch`) or names another profile
/// (`workdir_profile_mismatch`).
pub fn settle(
    doc: &serde_json::Value,
    kind: &str,
    profile: Option<&str>,
) -> Result<serde_json::Value, AgentRefusal> {
    let refuse = |code, message| Err(AgentRefusal { code, message });
    let mut set = match Set::from_doc(doc) {
        Ok(set) => set,
        Err(e) => {
            // A kind sets are not built for yet keeps its own code.
            let (code, message) = match e.downcast_ref::<CliError>() {
                Some(c) if c.code == WORKDIR_UNSUPPORTED => {
                    (WORKDIR_UNSUPPORTED, c.message.clone())
                }
                Some(c) => (WORKDIR_INVALID, c.message.clone()),
                None => (WORKDIR_INVALID, format!("{e:#}")),
            };
            return refuse(code, format!("the workdir set: {message}"));
        }
    };
    if set.kind != kind {
        return refuse(
            WORKDIR_KIND_MISMATCH,
            format!(
                "workdir set {} is for {} agents, and the task's agent is {kind}",
                set.id, set.kind
            ),
        );
    }
    match (&set.profile, profile) {
        (Some(asked), Some(own)) if asked == own => {}
        (None, own) => set.profile = own.map(str::to_string),
        (Some(asked), own) => {
            let own = own.map_or("no profile".to_string(), |p| format!("profile {p}"));
            return refuse(
                WORKDIR_PROFILE_MISMATCH,
                format!(
                    "workdir set {} names profile {asked}, and the task runs {own}: name the task's profile in the set, or none",
                    set.id
                ),
            );
        }
    }
    Ok(set.to_doc())
}

/// Whether a task with a set may go to `m` at all: a push machine the head
/// places it on (`takes_sets`), or a pull machine, whose serve places it
/// once it says it can (`TaskClaim::workdir_sets`). A `command` machine has
/// neither a shell nor a serve.
pub fn may_take_sets(m: &MachineConfig) -> bool {
    m.command.is_none()
}

/// Why a task pinned to `m` with a set is refused, or `None` when its set
/// can be placed there.
pub fn refusal_for(m: &MachineConfig) -> Option<AgentRefusal> {
    if may_take_sets(m) {
        return None;
    }
    Some(AgentRefusal {
        code: WORKDIR_UNSUPPORTED,
        message: format!(
            "machine {} is reached through a command bridge, where pastor does not place a task's workdir set: run the task on a push or pull machine",
            m.name
        ),
    })
}

/// What a gate places in a task's folder.
#[derive(Debug, Clone, PartialEq)]
pub enum Placing {
    /// The task's own set, its document settled (`settle`), or a stored
    /// bundle's set the head built when a pull machine claimed the task
    /// (`WorkdirBundle::set`), which names no profile.
    Set(serde_json::Value),
    /// The task's own set with its profile as the head rendered it when a
    /// pull machine claimed the task (`TaskWorkdir::render`): that machine
    /// cannot render it itself. The head's own gate renders it again.
    Rendered {
        set: serde_json::Value,
        render: Box<Render>,
    },
    /// The set stored workdir bundle `id` gives an agent of `kind`, which
    /// the head builds from the newest version kept, with the vars of
    /// `flock` (`workdir_push::Head::place_bundle`).
    Bundle {
        id: String,
        kind: String,
        flock: Option<String>,
    },
}

/// What places a task's set before its agent starts, and takes it out when
/// its worktree goes.
pub trait Gate: Send + Sync {
    /// Place `what` in `dir` on `machine`. `Ok(false)`: a bundle with
    /// nothing for the agent's kind, so nothing was placed. `Err` says why
    /// the task must not start: the code's message, with what to run.
    fn place<'a>(
        &'a self,
        machine: &'a str,
        dir: &'a str,
        what: &'a Placing,
    ) -> Pin<Box<dyn Future<Output = Result<bool, String>> + Send + 'a>>;

    /// Take every set the head placed in `dir` on `machine` out of it, once
    /// the task's worktree there is gone. Never an error: a later read of
    /// the machine drops what is left.
    fn forget(&self, machine: &str, dir: &str);
}

/// Place each of `all` in `dir` on `machine` through `gate`, in order,
/// stopping at the first that does not go through, and waiting at most
/// `wait` for the whole (`None`: as long as it takes). Answers whether each
/// placed anything. The places run on their own task: one that outlasts
/// `wait` is not cut off, since a place stopped halfway could leave files
/// on the machine that the head never records. They go on and record their
/// outcome; only the wait ends, as `workdir_failed`.
pub async fn place_within(
    gate: Arc<dyn Gate>,
    machine: &str,
    dir: &str,
    all: Vec<Placing>,
    wait: Option<std::time::Duration>,
) -> Result<Vec<bool>, String> {
    let (m, d) = (machine.to_string(), dir.to_string());
    let place = tokio::spawn(async move {
        let mut placed = Vec::new();
        for what in &all {
            placed.push(gate.place(&m, &d, what).await?);
        }
        Ok(placed)
    });
    let joined = match wait {
        Some(wait) => match tokio::time::timeout(wait, place).await {
            Ok(joined) => joined,
            Err(_) => {
                return Err(failed(
                    machine,
                    dir,
                    &format!(
                        "the place did not answer within {wait:?}; it goes on, and its outcome is recorded"
                    ),
                ));
            }
        },
        None => place.await,
    };
    joined.unwrap_or_else(|e| Err(failed(machine, dir, &format!("the place stopped: {e}"))))
}

/// A machine actor's gate; none for a fleet that places no sets (a
/// headless serve, the tests).
#[derive(Clone, Default)]
pub struct GateHandle(pub Option<Arc<dyn Gate>>);

impl std::fmt::Debug for GateHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_some() { "Gate" } else { "no gate" })
    }
}

impl PartialEq for GateHandle {
    /// The same gate, so a settings change that is not the gate does not
    /// look like one.
    fn eq(&self, other: &GateHandle) -> bool {
        match (&self.0, &other.0) {
            (Some(a), Some(b)) => std::ptr::addr_eq(Arc::as_ptr(a), Arc::as_ptr(b)),
            (None, None) => true,
            _ => false,
        }
    }
}

/// The head's gate: `workdir_push::Head` over pastor.toml as it is on disk,
/// as `workdir place` reads it, the flock last applied and its `workdir.*`
/// events.
pub struct HeadGate {
    paths: Paths,
    fleet: Weak<dyn HeadSide>,
    reach: Reach,
}

/// What the head's gate needs of the head: the flock last applied, and
/// where its `workdir.*` events go. The head's `Fleet`.
pub trait HeadSide: Send + Sync {
    fn flock(&self) -> crate::config::flock::Flock;
    fn emit_machine(&self, kind: &str, machine: &str, detail: Option<serde_json::Value>);
}

impl HeadSide for Fleet {
    fn flock(&self) -> crate::config::flock::Flock {
        Fleet::flock(self)
    }

    fn emit_machine(&self, kind: &str, machine: &str, detail: Option<serde_json::Value>) {
        Fleet::emit_machine(self, kind, machine, detail);
    }
}

impl HeadGate {
    /// The head's paths: where its bundles are.
    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub fn new(paths: Paths, fleet: Weak<dyn HeadSide>, reach: Reach) -> Arc<HeadGate> {
        Arc::new(HeadGate {
            paths,
            fleet,
            reach,
        })
    }

    async fn place_now(&self, machine: &str, dir: &str, what: &Placing) -> anyhow::Result<bool> {
        let fleet = self
            .fleet
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("the head is stopping"))?;
        let config = PastorConfig::load(&self.paths.config_file())?;
        let flock = fleet.flock();
        let emit = |kind: &'static str, machine: &str, detail: serde_json::Value| {
            fleet.emit_machine(kind, machine, Some(detail));
        };
        let head = Head {
            paths: &self.paths,
            config: &config,
            flock: &flock,
            reach: &self.reach,
            emit: &emit,
        };
        let placed = match what {
            Placing::Set(set) | Placing::Rendered { set, .. } => {
                head.place(machine, dir, set, false).await?
            }
            Placing::Bundle { id, kind, flock } => {
                match head
                    .place_bundle(machine, dir, id, kind, flock.as_deref(), false)
                    .await?
                {
                    Some(placed) => placed,
                    None => return Ok(false),
                }
            }
        };
        if placed.ok {
            return Ok(true);
        }
        Err(CliError::err(
            WORKDIR_FAILED,
            workdir_push::problems(&placed),
        ))
    }
}

/// `workdir_failed: <why>`, with the command that names the files.
fn failed(machine: &str, dir: &str, why: &str) -> String {
    format!(
        "{WORKDIR_FAILED}: {why}; `pastor workdir check {machine} {dir}` names the files, and `pastor workdir place` there settles them"
    )
}

impl Gate for HeadGate {
    fn place<'a>(
        &'a self,
        machine: &'a str,
        dir: &'a str,
        what: &'a Placing,
    ) -> Pin<Box<dyn Future<Output = Result<bool, String>> + Send + 'a>> {
        Box::pin(async move {
            self.place_now(machine, dir, what).await.map_err(|e| {
                let why = match e.downcast_ref::<CliError>() {
                    Some(c) if c.code == WORKDIR_FAILED => c.message.clone(),
                    Some(c) => format!("{} ({})", c.message, c.code),
                    None => format!("{e:#}"),
                };
                failed(machine, dir, &why)
            })
        })
    }

    fn forget(&self, machine: &str, dir: &str) {
        let recorded = workdir_push::list(&self.paths, Some(machine))
            .is_ok_and(|all| all.iter().any(|r| crate::dispatch::same_dir(&r.dir, dir)));
        if !recorded {
            return;
        }
        let (paths, fleet, reach) = (self.paths.clone(), self.fleet.clone(), self.reach.clone());
        let (machine, dir) = (machine.to_string(), dir.to_string());
        tokio::spawn(async move {
            let Some(fleet) = fleet.upgrade() else { return };
            let removed = async {
                let config = PastorConfig::load(&paths.config_file())?;
                let flock = fleet.flock();
                let emit = |kind: &'static str, machine: &str, detail: serde_json::Value| {
                    fleet.emit_machine(kind, machine, Some(detail));
                };
                let head = Head {
                    paths: &paths,
                    config: &config,
                    flock: &flock,
                    reach: &reach,
                    emit: &emit,
                };
                head.remove(&machine, &dir, None, false).await
            };
            if let Err(e) = removed.await {
                tracing::warn!(%machine, %dir, error = %format!("{e:#}"), "take a task's workdir set out of its removed worktree");
            }
        });
    }
}

/// A gate for a command run by a person (`task attach`): it asks the head
/// to place the set, as `pastor workdir place <machine> <dir>` does.
pub struct ClientGate<'a> {
    pub client: &'a crate::ipc::Client,
}

impl Gate for ClientGate<'_> {
    fn place<'a>(
        &'a self,
        machine: &'a str,
        dir: &'a str,
        what: &'a Placing,
    ) -> Pin<Box<dyn Future<Output = Result<bool, String>> + Send + 'a>> {
        Box::pin(async move {
            let (set, bundle, kind, flock) = match what {
                Placing::Set(set) | Placing::Rendered { set, .. } => {
                    (Some(set.clone()), None, None, None)
                }
                Placing::Bundle { id, kind, flock } => {
                    (None, Some(id.clone()), Some(kind.clone()), flock.clone())
                }
            };
            let req = crate::ipc::IpcRequest::WorkdirPlace {
                machine: machine.to_string(),
                dir: dir.to_string(),
                set,
                bundle,
                kind,
                flock,
                dry_run: false,
            };
            let got = crate::ipc::request_head_with_timeout(
                self.client,
                &req,
                super::workdir_cli::HEAD_WAIT,
            )
            .await;
            let why = match crate::cli::reply(got) {
                Ok(crate::ipc::IpcResponse::Workdir(placed)) => {
                    match placed.iter().find(|p| !p.ok) {
                        None => return Ok(!placed.is_empty()),
                        Some(p) => workdir_push::problems(p),
                    }
                }
                Ok(other) => format!("the head answered {other:?}"),
                Err(e) => format!("{} ({})", e.message, e.code),
            };
            Err(failed(machine, dir, &why))
        })
    }

    /// The head takes the set out when it removes the worktree.
    fn forget(&self, _machine: &str, _dir: &str) {}
}

/// The most a claim reply carries of sets, counted as JSON: a task whose
/// sets would take it past waits for the next claim, unless it is the
/// first, so a large one still goes.
pub const MAX_CLAIM_SETS: usize = 8 * 1024 * 1024;

/// Fill `spec`, a task pull machine `machine` claims for an agent of
/// `kind` in flock `flock_name`, with what that machine needs to place its
/// sets itself: its own set settled (`settle`), with the profile it names
/// rendered for the machine (`TaskWorkdir::render`), and the set each
/// stored bundle gives the kind, built from the newest version kept with
/// the flock's vars (`WorkdirBundle::set`; none when the bundle has nothing
/// for the kind). For a kind pastor places no sets for, the bundles are
/// marked skipped, and so are they for a task that is not a worktree task,
/// whose folder others share (`shared_folder`): none is built for either.
/// Answers how many bytes of sets it added. `Err` says why
/// the task must not start: a bundle that cannot be built, or a profile
/// that does not render.
pub fn for_claim(
    paths: &Paths,
    flock: &Flock,
    spec: &mut DispatchSpec,
    kind: &str,
    machine: &str,
    flock_name: &str,
) -> Result<usize, String> {
    let mut bytes = 0;
    let size = |v: &serde_json::Value| serde_json::to_vec(v).map_or(0, |b| b.len());
    let profile = spec.profile().map(str::to_string);
    if let Some(w) = spec.workdir.as_mut() {
        let doc = settle(&w.set, kind, profile.as_deref())
            .map_err(|r| format!("{} ({})", r.message, r.code))?;
        let set = Set::from_doc(&doc).map_err(|e| format!("{e:#}"))?;
        w.render = if set.profile.is_some() {
            let config = PastorConfig::load(&paths.config_file()).map_err(|e| format!("{e:#}"))?;
            workdir_push::render_for(&config, flock, &set, machine)
                .map_err(|e| match e.downcast_ref::<CliError>() {
                    Some(c) => format!("{} ({})", c.message, c.code),
                    None => format!("{e:#}"),
                })?
                .map(Box::new)
        } else {
            None
        };
        bytes += size(&doc);
        w.set = doc;
    }
    if !super::workdir_stored::places(kind) {
        let why = format!("pastor places no workdir sets for {kind} agents yet");
        for b in &mut spec.workdir_bundles {
            b.skipped = Some(why.clone());
        }
        return Ok(bytes);
    }
    if !spec.worktree {
        let why = shared_folder(spec.repo.as_deref().unwrap_or("its working directory"));
        for b in &mut spec.workdir_bundles {
            b.skipped = Some(why.clone());
        }
        return Ok(bytes);
    }
    for b in &mut spec.workdir_bundles {
        b.set = super::workdir_stored::stored_set(paths, flock, &b.id, kind, machine, flock_name)
            .map_err(|why| format!("bundle {}: {why}", b.id))?;
        bytes += b.set.as_ref().map_or(0, size);
    }
    Ok(bytes)
}

/// Why a task's stored workdir bundles are left out of `folder`, one other
/// tasks may share: every task places a bundle under the bundle's own id,
/// so each start would replace the last one's version there.
pub fn shared_folder(folder: &str) -> String {
    format!("shared folder ({folder}): stored workdir bundles go only in a task's own worktree")
}

/// A pull machine's own gate: it places a claimed task's sets on this
/// machine, as `workdir place --local` does, with what the head handed in
/// the claim (`for_claim`), and queues the report the head's record needs
/// (`workdir_report`), which the serve sends.
#[derive(Clone)]
pub struct LocalGate {
    paths: Paths,
    home: PathBuf,
    machine: String,
}

impl LocalGate {
    /// `None` when `$HOME` is not an absolute path: nothing could be placed.
    pub fn new(paths: Paths, machine: String) -> Option<Arc<LocalGate>> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|h| h.is_absolute())?;
        Some(LocalGate::at(paths, home, machine))
    }

    /// The gate of `machine`, whose home is `home`.
    pub fn at(paths: Paths, home: PathBuf, machine: String) -> Arc<LocalGate> {
        Arc::new(LocalGate {
            paths,
            home,
            machine,
        })
    }

    /// Place `doc` in `dir` now, with `render` for the profile it names,
    /// and queue the report. A problem row is the error, as on the head.
    fn place_here(
        &self,
        dir: &Path,
        doc: &serde_json::Value,
        render: Option<&Render>,
    ) -> anyhow::Result<()> {
        let set = Set::from_doc(doc)?;
        let placed = super::workdir::place_with(&self.paths, &self.home, dir, &set, render, false)?;
        let report = super::workdir_report::placed(&set, render, &placed);
        if let Err(e) = super::workdir_report::queue(&self.paths, &self.machine, &report) {
            tracing::warn!(dir = %dir.display(), id = %set.id, error = %format!("{e:#}"), "keep the report of a placed workdir set for the head");
        }
        if placed.ok {
            Ok(())
        } else {
            Err(CliError::err(
                WORKDIR_FAILED,
                workdir_push::problems(&placed),
            ))
        }
    }
}

impl Gate for LocalGate {
    fn place<'a>(
        &'a self,
        machine: &'a str,
        dir: &'a str,
        what: &'a Placing,
    ) -> Pin<Box<dyn Future<Output = Result<bool, String>> + Send + 'a>> {
        Box::pin(async move {
            let (doc, render) = match what {
                Placing::Set(doc) => (doc.clone(), None),
                Placing::Rendered { set, render } => (set.clone(), Some((**render).clone())),
                // The head builds a stored bundle's set when this machine
                // claims the task; a bundle with none had nothing for the
                // agent's kind.
                Placing::Bundle { .. } => return Ok(false),
            };
            let this = self.clone();
            let at = PathBuf::from(dir);
            let done =
                tokio::task::spawn_blocking(move || this.place_here(&at, &doc, render.as_ref()))
                    .await
                    .map_err(|e| anyhow::anyhow!("the place stopped: {e}"))
                    .and_then(|r| r);
            done.map(|()| true).map_err(|e| {
                let why = match e.downcast_ref::<CliError>() {
                    Some(c) if c.code == WORKDIR_FAILED => c.message.clone(),
                    Some(c) => format!("{} ({})", c.message, c.code),
                    None => format!("{e:#}"),
                };
                format!("{WORKDIR_FAILED}: {why}; placed on {machine} itself, in {dir}")
            })
        })
    }

    fn forget(&self, _machine: &str, dir: &str) {
        let (paths, home, machine) = (self.paths.clone(), self.home.clone(), self.machine.clone());
        let dir = PathBuf::from(dir);
        tokio::task::spawn_blocking(move || {
            let removed = super::workdir::remove(&paths, &home, &dir, None, false)
                .map(|p| super::workdir_report::removed(&p));
            match removed {
                Ok(Some(report)) => {
                    if let Err(e) = super::workdir_report::queue(&paths, &machine, &report) {
                        tracing::warn!(dir = %dir.display(), error = %format!("{e:#}"), "keep the report of removed workdir sets for the head");
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(dir = %dir.display(), error = %format!("{e:#}"), "take a task's workdir sets out of its removed worktree");
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn set(profile: Option<&str>) -> serde_json::Value {
        let mut doc = serde_json::json!({"id": "reviewer", "kind": "claude", "files": [
            {"path": ".claude/agents/checker.md", "content": "Check.\n"}]});
        if let Some(p) = profile {
            doc["profile"] = p.into();
        }
        doc
    }

    /// A set with no profile takes the task's; one naming the task's is
    /// kept; another profile, another kind or no set at all is refused,
    /// each with its code.
    #[test]
    fn a_tasks_set_is_settled_against_its_agent_and_profile() {
        let doc = settle(&set(None), "claude", Some("review")).unwrap();
        assert_eq!(doc["profile"], "review");
        let doc = settle(&set(None), "claude", None).unwrap();
        assert!(doc.get("profile").is_none());
        let doc = settle(&set(Some("review")), "claude", Some("review")).unwrap();
        assert_eq!(doc["profile"], "review");
        let code = |r: Result<serde_json::Value, AgentRefusal>| r.unwrap_err().code;
        assert_eq!(
            code(settle(&set(Some("review")), "claude", Some("develop"))),
            WORKDIR_PROFILE_MISMATCH
        );
        assert_eq!(
            code(settle(&set(Some("review")), "claude", None)),
            WORKDIR_PROFILE_MISMATCH
        );
        assert_eq!(
            code(settle(&set(None), "codex", None)),
            WORKDIR_KIND_MISMATCH
        );
        assert_eq!(
            code(settle(&serde_json::json!({"id": "x"}), "claude", None)),
            WORKDIR_INVALID
        );
        assert_eq!(
            code(settle(
                &serde_json::json!({"id": "x", "kind": "codex"}),
                "codex",
                None
            )),
            WORKDIR_UNSUPPORTED
        );
    }

    /// Only push machines the head reaches with a shell take sets.
    #[test]
    fn only_push_machines_with_a_shell_take_sets() {
        let flock = Flock::parse(
            Path::new("flock.toml"),
            "[[machine]]\nname = \"m\"\nlocal = true\n\n\
             [[machine]]\nname = \"s\"\nssh = \"user@pi-1\"\n\n\
             [[machine]]\nname = \"p\"\npull = true\n\n\
             [[machine]]\nname = \"c\"\ncommand = [\"true\"]\n",
        )
        .unwrap();
        let takes = |n: &str| takes_sets(flock.get(n).unwrap());
        assert!(takes("m") && takes("s"));
        assert!(!takes("p") && !takes("c"));
        assert!(refusal_for(flock.get("m").unwrap()).is_none());
        // A pull machine's serve places a task's set itself.
        assert!(refusal_for(flock.get("p").unwrap()).is_none());
        let r = refusal_for(flock.get("c").unwrap()).unwrap();
        assert_eq!(r.code, WORKDIR_UNSUPPORTED);
        assert!(r.message.contains("command bridge"), "{}", r.message);
    }
    /// The head side of a gate under test: the fixture's flock, and the
    /// events it raises.
    struct Side {
        flock: Flock,
        events: std::sync::Mutex<Vec<String>>,
    }

    impl HeadSide for Side {
        fn flock(&self) -> Flock {
            self.flock.clone()
        }

        fn emit_machine(&self, kind: &str, _machine: &str, _detail: Option<serde_json::Value>) {
            self.events.lock().unwrap().push(kind.to_string());
        }
    }

    fn head_gate(f: &workdir_push::tests::Fixture) -> (Arc<HeadGate>, Arc<Side>) {
        let side = Arc::new(Side {
            flock: f.flock.clone(),
            events: Default::default(),
        });
        let weak = Arc::downgrade(&(side.clone() as Arc<dyn HeadSide>));
        (HeadGate::new(f.paths.clone(), weak, f.reach.clone()), side)
    }

    fn doc() -> serde_json::Value {
        serde_json::from_str(workdir_push::tests::SET).unwrap()
    }

    fn placing() -> Placing {
        Placing::Set(doc())
    }

    /// The head's gate places through `workdir_push::Head`: a place that
    /// goes through is recorded and raises `workdir.placed`; one with a
    /// problem row is `workdir_failed`, naming the rows and the check to
    /// run.
    #[tokio::test]
    async fn the_head_gate_places_and_records_the_set() {
        let f = workdir_push::tests::Fixture::new();
        let (gate, side) = head_gate(&f);
        assert!(gate.place("m", "/w", &placing()).await.unwrap());
        let records = workdir_push::list(&f.paths, Some("m")).unwrap();
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].dir, "/w");
        assert_eq!(*side.events.lock().unwrap(), ["workdir.placed"]);

        f.answer("real", &[("conflict", ".claude/agents/checker.md")]);
        let err = gate.place("m", "/w", &placing()).await.unwrap_err();
        assert!(err.starts_with("workdir_failed: "), "{err}");
        assert!(err.contains(".claude/agents/checker.md"), "{err}");
        assert!(err.contains("pastor workdir check m /w"), "{err}");

        // A machine the head cannot reach with a shell.
        let err = gate.place("c", "/w", &placing()).await.unwrap_err();
        assert!(err.starts_with("workdir_failed: "), "{err}");
    }

    /// Wait until `f` has `want` records on machine `m`.
    async fn until_records(f: &workdir_push::tests::Fixture, want: usize) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while workdir_push::list(&f.paths, Some("m")).unwrap().len() != want {
            assert!(
                std::time::Instant::now() < deadline,
                "records never reached {want}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// `forget` takes out what the head placed in a directory it recorded,
    /// detached, dropping the record with `workdir.removed`; a directory it
    /// placed nothing in asks the machine nothing: once the recorded one is
    /// done, the machine was asked exactly once.
    #[tokio::test]
    async fn the_head_gate_forgets_only_a_recorded_dir() {
        let f = workdir_push::tests::Fixture::new();
        let (gate, side) = head_gate(&f);
        gate.place("m", "/w", &placing()).await.unwrap();
        let calls = f.calls().len();
        gate.forget("m", "/elsewhere");
        f.answer("real", &[("removed", ".claude/agents/checker.md")]);
        gate.forget("m", "/w");
        until_records(&f, 0).await;
        assert_eq!(f.calls().len(), calls + 1, "{:?}", f.calls());
        assert_eq!(
            *side.events.lock().unwrap(),
            ["workdir.placed", "workdir.removed"]
        );
    }

    /// A place that outlasts its wait is not cut off: the wait ends with
    /// `workdir_failed`, and the place goes on to record its outcome, so a
    /// later `forget` of that directory still takes the set out.
    #[tokio::test]
    async fn a_place_past_its_wait_still_records_its_outcome() {
        let f = workdir_push::tests::Fixture::new();
        let (gate, side) = head_gate(&f);
        f.delay(Some("0.5"));
        let wait = Some(std::time::Duration::from_millis(50));
        let err = place_within(gate.clone(), "m", "/w", vec![placing()], wait)
            .await
            .unwrap_err();
        assert!(err.starts_with("workdir_failed: "), "{err}");
        assert!(err.contains("did not answer within"), "{err}");
        until_records(&f, 1).await;
        assert_eq!(*side.events.lock().unwrap(), ["workdir.placed"]);

        f.delay(None);
        let calls = f.calls().len();
        f.answer("real", &[("removed", ".claude/agents/checker.md")]);
        gate.forget("m", "/w");
        until_records(&f, 0).await;
        assert_eq!(f.calls().len(), calls + 1, "the machine was asked");
    }

    /// A stored workdir bundle `repo-notes` kept in `paths`, with a file
    /// for claude agents only.
    fn keep_bundle(paths: &Paths, root: &Path) {
        let src = root.join("src/repo-notes");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("f.md"), "Notes for {flock}.\n").unwrap();
        std::fs::write(
            src.join(crate::bundle::MANIFEST_FILE),
            "id = \"repo-notes\"\nversion = \"1\"\ntarget = \"workdir\"\n[[file]]\nsrc = \"f.md\"\npath = \".claude/agents/notes.md\"\nkinds = [\"claude\"]\nrender = true\n",
        )
        .unwrap();
        crate::bundle::store::add(paths, &crate::bundle::store::Source::Dir(src)).unwrap();
    }

    fn pull_flock() -> Flock {
        Flock::parse(
            Path::new("flock.toml"),
            "[[flock]]\nname = \"ops\"\ndefault = true\nmachines = { p = 1 }\nworkdir_bundles = [\"repo-notes\"]\n\n\
             [[machine]]\nname = \"p\"\npull = true\n",
        )
        .unwrap()
    }

    /// A worktree task's spec, with its own set and `bundles`.
    fn claimed_spec(profile: Option<&str>, bundles: &[&str]) -> DispatchSpec {
        let mut spec = crate::profile_render::base_spec("claude");
        spec.repo = Some("~/work/app".into());
        spec.worktree = true;
        spec.agent_source = profile.map(|p| {
            Box::new(crate::task::AgentSource {
                profile: Some(p.into()),
                ..Default::default()
            })
        });
        spec.workdir = Some(Box::new(crate::task::TaskWorkdir {
            set: set(None),
            placed_in: None,
            render: None,
        }));
        spec.workdir_bundles = bundles
            .iter()
            .map(|id| crate::task::WorkdirBundle::named(id))
            .collect();
        spec
    }

    /// What a pull machine gets with a task it claims: its own set settled
    /// with the task's profile and that profile rendered for the machine,
    /// and the set each stored bundle gives the agent's kind, with the
    /// flock's vars. A kind pastor places no sets for skips the bundles; a
    /// bundle that cannot be built, or a profile that does not render, is
    /// an error.
    #[test]
    fn a_claimed_task_carries_its_sets_built_and_rendered() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        keep_bundle(&paths, tmp.path());
        let flock = pull_flock();

        let mut spec = claimed_spec(Some("review"), &["repo-notes"]);
        let bytes = for_claim(&paths, &flock, &mut spec, "claude", "p", "ops").unwrap();
        let w = spec.workdir.as_deref().unwrap();
        assert_eq!(w.set["profile"], "review");
        let render = w.render.as_deref().unwrap();
        assert_eq!(
            (render.profile.as_str(), render.kind.as_str()),
            ("review", "claude")
        );
        let built = spec.workdir_bundles[0].set.as_ref().unwrap();
        assert_eq!(built["id"], "repo-notes");
        assert_eq!(built["vars"]["flock"], "ops");
        assert!(
            built.get("profile").is_none(),
            "a stored set names no profile"
        );
        assert!(spec.workdir_bundles[0].skipped.is_none());
        assert!(bytes > 0);

        // A task with no worktree shares its folder: its bundles are
        // skipped, none built, and its own set still goes.
        let mut shared = claimed_spec(None, &["repo-notes"]);
        shared.worktree = false;
        for_claim(&paths, &flock, &mut shared, "claude", "p", "ops").unwrap();
        assert!(shared.workdir_bundles[0].set.is_none());
        let why = shared.workdir_bundles[0].skipped.clone().unwrap();
        assert!(why.starts_with("shared folder (~/work/app)"), "{why}");
        assert_eq!(shared.workdir.as_deref().unwrap().set["id"], "reviewer");

        // No profile: nothing to render.
        let mut spec = claimed_spec(None, &[]);
        for_claim(&paths, &flock, &mut spec, "claude", "p", "ops").unwrap();
        assert!(spec.workdir.as_deref().unwrap().render.is_none());

        // A bundle with nothing for the kind gets no set and no skip; a
        // kind pastor places none for skips them.
        let mut agy = claimed_spec(None, &["repo-notes"]);
        agy.workdir = None;
        for_claim(&paths, &flock, &mut agy, "agy", "p", "ops").unwrap();
        assert!(agy.workdir_bundles[0].set.is_none());
        assert!(agy.workdir_bundles[0].skipped.is_none());
        let mut codex = agy.clone();
        for_claim(&paths, &flock, &mut codex, "codex", "p", "ops").unwrap();
        let why = codex.workdir_bundles[0].skipped.clone().unwrap();
        assert!(why.contains("no workdir sets for codex agents"), "{why}");

        let mut gone = claimed_spec(None, &["nothing"]);
        let err = for_claim(&paths, &flock, &mut gone, "claude", "p", "ops").unwrap_err();
        assert!(
            err.contains("bundle nothing") && err.contains("no version kept"),
            "{err}"
        );
        let mut unknown = claimed_spec(Some("no-such-profile"), &[]);
        assert!(for_claim(&paths, &flock, &mut unknown, "claude", "p", "ops").is_err());
    }

    /// A pull machine's own gate places a set in the folder on this
    /// machine, queues the report the head's record needs, and on the
    /// worktree's removal takes it out and queues that too. A set naming a
    /// profile places only with the head's rendering of it.
    #[tokio::test]
    async fn a_pull_machine_places_and_reports_its_own_sets() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        let home = tmp.path().join("home");
        let dir = home.join("wt/app-t-1");
        std::fs::create_dir_all(&dir).unwrap();
        let gate = LocalGate::at(paths.clone(), home.clone(), "p".into());
        let d = dir.to_str().unwrap();
        assert_eq!(gate.place("p", d, &Placing::Set(set(None))).await, Ok(true));
        assert_eq!(
            std::fs::read_to_string(dir.join(".claude/agents/checker.md")).unwrap(),
            "Check.\n"
        );
        assert_eq!(super::super::workdir_report::pending(&paths), 1);
        // A stored bundle's set comes built in the claim, never by id.
        let bundle = Placing::Bundle {
            id: "repo-notes".into(),
            kind: "claude".into(),
            flock: None,
        };
        assert_eq!(gate.place("p", d, &bundle).await, Ok(false));

        let err = gate
            .place("p", d, &Placing::Set(set(Some("review"))))
            .await
            .unwrap_err();
        assert!(err.starts_with("workdir_failed: "), "{err}");
        assert!(err.contains("placed on p itself"), "{err}");

        let config = PastorConfig::default();
        let profile = config.profiles.resolve("review").unwrap();
        let render = crate::profile_render::render(&config, &profile, "claude", None).unwrap();
        let rendered = Placing::Rendered {
            set: set(Some("review")),
            render: Box::new(render),
        };
        assert_eq!(gate.place("p", d, &rendered).await, Ok(true));
        let local: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join(".claude/settings.local.json")).unwrap(),
        )
        .unwrap();
        assert!(local["permissions"]["allow"].is_array(), "{local}");

        gate.forget("p", d);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while dir.join(".claude/agents/checker.md").exists() {
            assert!(std::time::Instant::now() < deadline, "not taken out");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        while super::super::workdir_report::pending(&paths) < 3 {
            assert!(std::time::Instant::now() < deadline, "no removal report");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}
