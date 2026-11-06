//! A headless `pastor serve`, the shepherd: with a head set on another
//! machine, this one runs its own jobs and connector hooks, and the tasks
//! the head gives it as a pull machine. No queue: the items a job run finds
//! go to the head in one `IpcRequest::JobSubmit`, and the head's events come
//! back through `EventsSince` for the hooks here. Its own small database
//! (`Paths::shepherd_db_file`) keeps the jobs' state and seen keys, how far
//! it has read the head's events, and the rows of the tasks it runs.
//!
//! When the head's flock.toml has this machine as `pull = true`, each tick
//! also asks for tasks (`IpcRequest::TaskClaim`) and runs them with the same
//! machine actor the head runs for a local machine, on this machine's herdr;
//! every change the actor sees goes back as `IpcRequest::TaskReport`
//! (`Puller`). The folder trust the head saved for this machine comes with
//! `IpcRequest::PullTrust` into this store's `trusted_repos`, where the
//! actor's saved-trust check reads it, and each trust answer goes back on
//! the task's report.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc};

use crate::cli::CliError;
use crate::config::{PastorConfig, Paths};
use crate::daemon::{Answer, Daemon, Fleet, answer_on, jobs_answer};
use crate::events::{EventRecord, EventsPage};
use crate::head::RemoteHead;
use crate::herdr::{Connector, Endpoint};
use crate::ipc::{
    IPC_PROTOCOL, IpcRequest, IpcResponse, ProtocolGate, SHEPHERD_ROLE, request_line,
};
use crate::machine::{EndBy, MachineHandle, MachineSettings, PastorEvent};
use crate::scheduler::{ConfigFingerprint, Scheduler, SchedulerHandle};
use crate::store::Store;
use crate::task::{Task, TaskState};

/// The meta key that holds the last head event handed to the hooks.
const CURSOR_KEY: &str = "head_event_seq";

/// How many head events one `EventsSince` asks for.
const PAGE: u32 = 200;

/// How many tasks one `TaskClaim` asks for. The head's `max_agents` for the
/// machine is what bounds how many run here at once.
const CLAIM_AT_ONCE: u32 = 4;

/// How long the machine's herdr may take to answer the ping that reads its
/// version (`Puller::read_herdr_version`).
const VERSION_PING_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a request to the head may take. A `JobSubmit` waits for the
/// head's dispatch pass, agent readiness included.
const HEAD_TIMEOUT: Duration = Duration::from_secs(60);

/// One request to the head and its reply; an unreachable head, or an error
/// reply, is an `Err` with its code (`CliError`).
pub type Ask = Arc<
    dyn Fn(IpcRequest) -> Pin<Box<dyn Future<Output = anyhow::Result<IpcResponse>> + Send>>
        + Send
        + Sync,
>;

/// `Ask` over ssh to `head`, through `pastor bridge` there, `gated`.
pub fn ask_remote(head: RemoteHead) -> Ask {
    let head = Arc::new(head);
    gated(Arc::new(move |req| {
        let head = head.clone();
        Box::pin(async move {
            let line = request_line(&req, None)?;
            match head.request(&line, HEAD_TIMEOUT).await {
                Ok(IpcResponse::Error { code, message }) => Err(CliError::err(&code, message)),
                Ok(resp) => Ok(resp),
                Err(err) => Err(crate::head::failure(&err)),
            }
        })
    }))
}

/// `ask` that refuses a request the head is too old for, `head_too_old`,
/// before it is sent: the job submits, claims, reports and event reads a
/// headless serve sends get the check the CLI's requests get
/// (`ipc::ProtocolGate`), with one ping per `PONG_FRESH` at most.
pub fn gated(ask: Ask) -> Ask {
    let gate = Arc::new(ProtocolGate::default());
    Arc::new(move |req| {
        let (ask, gate) = (ask.clone(), gate.clone());
        Box::pin(async move {
            gate.check(&req, ask(IpcRequest::Ping)).await?;
            ask(req).await
        })
    })
}

/// The shepherd's side of the socket: its own jobs, and a refusal for the
/// rest, which is the head's.
pub struct Shepherd {
    scheduler: SchedulerHandle,
    head: String,
}

impl Answer for Shepherd {
    async fn answer(&self, req: IpcRequest, from: crate::ipc::Caller) -> IpcResponse {
        // The rule the head applies (`Daemon::handle_as`), from this
        // machine's pastor.toml. Nothing a headless serve answers is on the
        // orchestrator role's table, and orchestrators run on the head.
        let refusal = match (&from.task, &from.orchestrator) {
            (Some(task), _) => Some(crate::daemon::agent_refusal(task)),
            (None, Some(o)) => Some(crate::daemon::script_refusal(o)),
            (None, None) => None,
        };
        if let Some(refusal) = refusal
            && req.changes_fleet()
        {
            return IpcResponse::error("agent_refused", refusal);
        }
        if let IpcRequest::Ping = req {
            return IpcResponse::Pong {
                version: env!("CARGO_PKG_VERSION").into(),
                protocol: IPC_PROTOCOL,
                role: Some(SHEPHERD_ROLE.into()),
            };
        }
        match jobs_answer(&self.scheduler, req).await {
            Some(resp) => resp,
            None => IpcResponse::error(
                "shepherd_unsupported",
                format!(
                    "this is a headless pastor serve: it runs this machine's jobs and hooks, and answers only ping, tick and job list, run and reload for them; ask the head on {}",
                    self.head
                ),
            ),
        }
    }
}

/// `pastor serve` with a head set on another machine.
pub async fn serve(paths: Paths, head: RemoteHead) -> anyhow::Result<()> {
    let ssh = head.ssh.clone();
    run(paths, ssh, ask_remote(head)).await
}

/// The actor settings of a pull machine's serve: `pastor.toml`'s timings,
/// no limits (the head keeps them: a limit read here would be one it never
/// hears of, so this machine's tasks settle as before) and the env layers of
/// this machine's own config (`EnvLayers::of_pull`), read when serve starts:
/// the head's are not known here. A flock.toml that is there but does not
/// load is an error naming the file and why: serving without it would run
/// every task without the env its layers set. No flock.toml is the defaults.
fn pull_settings(paths: &Paths, config: &PastorConfig) -> anyhow::Result<MachineSettings> {
    let flock = crate::config::flock::Flock::load(&paths.flock_file()).map_err(|e| {
        anyhow::anyhow!("{e:#}; pastor serve is not started: its env layers come from that file")
    })?;
    Ok(MachineSettings {
        limits: None,
        env: crate::config::EnvLayers::of_pull(
            &config.defaults,
            &flock,
            &config.shepherd.machine_name(),
        ),
        ..crate::daemon::machine_settings(&PastorConfig {
            head_address: None,
            ..config.clone()
        })
    })
}

/// `serve`, given how to reach the head, which `head` names in messages.
pub async fn run(paths: Paths, head: String, ask: Ask) -> anyhow::Result<()> {
    // Before the load, as for the head: an edit after it must still read as
    // a change on the scheduler's first pass.
    let on_disk = ConfigFingerprint::sample(&paths);
    let config = PastorConfig::load(&paths.config_file())?;
    // Before the socket and the database, so a flock.toml that does not load
    // leaves nothing behind.
    let settings = pull_settings(&paths, &config)?;
    paths.ensure()?;
    // Before the database, as the head does: a second serve bails here.
    let listener = Daemon::bind_socket(&paths.socket_file()).await?;
    let store = Arc::new(Store::open(&paths.shepherd_db_file())?);
    tracing::info!(socket = %paths.socket_file().display(), %head, "pastor serve (headless): this machine's jobs and hooks, for the head");
    let (events, _) = broadcast::channel(64);
    let (to_hooks, hooks_rx) = mpsc::channel(crate::events::HOOK_QUEUE_CAPACITY);
    crate::hooks::spawn(paths.clone(), store.clone(), events.downgrade(), hooks_rx);
    let mut follow = Follower {
        head: head.clone(),
        ask: ask.clone(),
        store: store.clone(),
        to_hooks,
        reachable: None,
    };
    // Once before the first job run, so the cursor starts before any task
    // that run queues and the hooks hear about it.
    let first = follow.read().await;
    let mut puller = Puller::new(
        config.shepherd.machine_name(),
        config.shepherd.flock_work(),
        shepherd_connector(&config),
        settings,
        store.clone(),
        ask.clone(),
    );
    puller.report_file = Some(crate::bootstrap::report_file(&paths.state_dir));
    puller.workdir_paths = Some(paths.clone());
    // This serve places its tasks' workdir sets itself, with what the head
    // hands in each claim, and says so in the claim.
    puller.settings.workdir_gate = crate::bundle::workdir_task::GateHandle(
        crate::bundle::workdir_task::LocalGate::new(paths.clone(), puller.machine.clone())
            .map(|g| g as Arc<dyn crate::bundle::workdir_task::Gate>),
    );
    // A head that did not answer the first read is silent for the puller
    // too: it warns once, and the first pass waits for the next tick.
    puller.heard("event read", &first);
    let every = config.tick_duration();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        if puller.silent.is_none() {
            puller.pass().await;
        }
        loop {
            tokio::select! {
                _ = tick.tick() => self::tick(&mut follow, &mut puller).await,
                // The actor saw a task change: tell the head now, not at
                // the next tick, unless the head is not answering; the
                // next tick tries.
                () = puller.changed() => {
                    if puller.silent.is_none() && !follow.cursor_unset() {
                        puller.report(false).await;
                        // A start that placed sets left their reports.
                        puller.send_workdir_reports().await;
                    }
                }
            }
        }
    });
    let fleet = Arc::new(Fleet::headless(store.clone(), ask));
    let scheduler = Scheduler::new(paths.clone(), &config, store, fleet, events)
        .with_connectors()
        .with_config_baseline(on_disk)
        .headless()
        .spawn();
    let socket = paths.socket_file();
    let shepherd = Shepherd { scheduler, head };
    answer_on(Arc::new(shepherd), listener, socket).await
}

/// This machine's own herdr, where the tasks it claims run: the local
/// session `default`, or `[shepherd] command`.
fn shepherd_connector(config: &PastorConfig) -> Arc<dyn Connector> {
    Arc::new(match &config.shepherd.command {
        Some(argv) => Endpoint::Command { argv: argv.clone() },
        None => Endpoint::Local {
            session: "default".into(),
        },
    })
}

/// What was last told to the head about a task: its state, pane, note,
/// whether its trust answer went with it, and the workdir bundles its start
/// here left out.
type Reported = (
    TaskState,
    Option<String>,
    Option<String>,
    bool,
    Vec<crate::task::WorkdirSkip>,
);

/// Runs the tasks the head gives this machine as a pull machine: claims
/// them each tick, starts each with the machine actor, and reports every
/// change of their rows. The actor starts on the first claim the head
/// answers, or at once when the store still holds tasks from before a
/// restart: a machine the head does not have as a pull machine never
/// talks to its herdr.
struct Puller {
    machine: String,
    flock_work: bool,
    connector: Arc<dyn Connector>,
    settings: MachineSettings,
    store: Arc<Store>,
    ask: Ask,
    /// The actor's events; any of them means a row may have changed.
    events: broadcast::Sender<PastorEvent>,
    heard: broadcast::Receiver<PastorEvent>,
    actor: Option<MachineHandle>,
    /// Per task, what the head last accepted.
    sent: HashMap<i64, Reported>,
    /// Tasks whose dispatch is with the actor now, so a tick does not ask
    /// again while it waits in the actor's queue.
    dispatching: Arc<std::sync::Mutex<HashSet<i64>>>,
    /// The code of the last claim the head refused, so it is logged once.
    refused: Option<String>,
    /// The code of the last `PullTrust` the head refused, likewise.
    trust_refused: Option<String>,
    /// This machine's herdr version, sent with every claim once its
    /// connector answered a ping (`read_herdr_version`); pastor's own is
    /// always known.
    herdr_version: Option<String>,
    /// `<state>/bootstrap.json`: the outcome `pastor join` left to report.
    report_file: Option<PathBuf>,
    /// Where the workdir reports this machine owes the head wait
    /// (`bundle::workdir_report`).
    workdir_paths: Option<Paths>,
    /// Why the last delivery of them found the head not taking them, so it
    /// is logged once.
    workdir_waiting: Option<String>,
    /// While the head does not answer, the first failure of the silence, so
    /// it is logged once and the next answer is too.
    silent: Option<String>,
}

/// One tick: the pull machine's reports and claim, then the head's events
/// for the hooks. Each request waits up to `HEAD_TIMEOUT`, so once one finds
/// the head silent the rest are left for the next tick: a head that does
/// not answer costs a tick one timeout, not one per request. The reports go
/// first, as the requests that keep this machine's tasks from going stale on
/// the head, except once, before the cursor this machine reads the head's
/// events from is ever set: a report sent first could make the head emit
/// `task.running` or `task.done` for it, and with no cursor yet `catch_up`
/// would read that straight back as history and skip it.
async fn tick(follow: &mut Follower, puller: &mut Puller) {
    if follow.cursor_unset() {
        // The read is a request to the head like the others: a silence
        // warns once and ends the tick; a refusal is an answer, and the
        // tick goes on, since the cursor may never be set.
        let read = follow.read().await;
        if !puller.heard("event read", &read) {
            return;
        }
    }
    if puller.pass().await {
        follow.pass().await;
    } else if let Some(err) = puller.silent.clone() {
        follow.note(Err(err));
    }
}

/// Whether `err` is a request the head never answered: it could not be
/// reached, had no serve running, or took longer than `HEAD_TIMEOUT`. An
/// error reply is an answer.
fn head_silent(err: &anyhow::Error) -> bool {
    err.downcast_ref::<CliError>().is_some_and(|e| {
        matches!(
            e.code.as_str(),
            "head_unreachable" | "no_head" | "timeout" | "daemon_not_running"
        )
    })
}

/// Whether a request that failed with `code` failed on the way (no head, an
/// unreachable or restarting one, a timeout, a dropped connection, a head
/// not yet upgraded to take it), so the next pass sends it again; any other
/// code is the head refusing it.
pub(crate) fn failed_on_the_way(code: &str) -> bool {
    matches!(
        code,
        "head_unreachable"
            | "no_head"
            | "timeout"
            | "runtime_error"
            | "daemon_not_running"
            | "head_too_old"
    )
}

impl Puller {
    fn new(
        machine: String,
        flock_work: bool,
        connector: Arc<dyn Connector>,
        settings: MachineSettings,
        store: Arc<Store>,
        ask: Ask,
    ) -> Puller {
        let (events, heard) = broadcast::channel(256);
        Puller {
            machine,
            flock_work,
            connector,
            settings,
            store,
            ask,
            events,
            heard,
            actor: None,
            sent: HashMap::new(),
            dispatching: Default::default(),
            refused: None,
            trust_refused: None,
            herdr_version: None,
            report_file: None,
            workdir_paths: None,
            workdir_waiting: None,
            silent: None,
        }
    }

    /// `answer`, to a request for `what`, as far as the silence goes: false
    /// when the head did not answer (a warning on the first of a silence),
    /// true when it did, a refusal included (the end of one, logged).
    fn heard<T>(&mut self, what: &str, answer: &anyhow::Result<T>) -> bool {
        match answer {
            Err(err) if head_silent(err) => {
                if self.silent.is_none() {
                    tracing::warn!(
                        code = "pull_head_silent",
                        machine = %self.machine,
                        err = %format!("{err:#}"),
                        "the head did not answer this machine's {what}: its tasks' changes wait here, and if this lasts `pull_lost_after` (head's pastor.toml) the head counts this machine lost and marks its working tasks stale; asking again each tick"
                    );
                    self.silent = Some(format!("{err:#}"));
                }
                false
            }
            _ => {
                if self.silent.take().is_some() {
                    tracing::info!(machine = %self.machine, "the head answers this machine again; its reports go through");
                }
                true
            }
        }
    }

    /// Resolves when the actor has emitted an event since the last call.
    async fn changed(&mut self) {
        loop {
            match self.heard.recv().await {
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => return,
                // `self.events` keeps the channel open; never reached.
                Err(broadcast::error::RecvError::Closed) => std::future::pending::<()>().await,
            }
        }
    }

    /// The tasks this store holds: those claimed and not yet forgotten.
    fn tasks(&self) -> Vec<Task> {
        match self.store.list_tasks(&Default::default()) {
            Ok(tasks) => tasks,
            Err(err) => {
                tracing::error!(%err, "list the claimed tasks");
                Vec::new()
            }
        }
    }

    fn actor(&mut self) -> MachineHandle {
        self.actor
            .get_or_insert_with(|| {
                tracing::info!(machine = %self.machine, "running the head's tasks here, as pull machine {}", self.machine);
                crate::machine::spawn_machine(
                    self.machine.clone(),
                    CLAIM_AT_ONCE,
                    Vec::new(),
                    self.connector.clone(),
                    self.store.clone(),
                    self.settings.clone(),
                    self.events.clone(),
                )
            })
            .clone()
    }

    /// One tick: report every task, claim, fetch the saved trust, and start
    /// what is queued here. The head calls stop at the first the head does
    /// not answer, for the next tick (`tick`). Answers whether it did.
    async fn pass(&mut self) -> bool {
        if self.actor.is_none() && !self.tasks().is_empty() {
            self.actor();
        }
        // Before the claim: a machine whose join is pending gets no task
        // until its report lands, and this pass then claims for it.
        let answered = self.report(true).await
            && self.send_bootstrap_report().await
            && self.claim().await
            && self.fetch_trust().await;
        self.start_queued();
        answered && self.send_workdir_reports().await
    }

    /// Hand the head the workdir reports that `workdir place|remove --local`
    /// left here while it did not answer. Answers false only when the head
    /// is silent; a head that refuses a report loses it.
    async fn send_workdir_reports(&mut self) -> bool {
        let Some(paths) = self.workdir_paths.clone() else {
            return true;
        };
        if crate::bundle::workdir_report::pending(&paths) == 0 {
            return true;
        }
        let ask = self.ask.clone();
        let sent = crate::bundle::workdir_report::deliver(&paths, |req| ask(req)).await;
        for why in &sent.refused {
            tracing::warn!(machine = %self.machine, %why, "the head refused a workdir report; it is dropped");
        }
        match sent.waiting {
            Some(why) => {
                if self.workdir_waiting.as_deref() != Some(why.as_str()) {
                    tracing::warn!(machine = %self.machine, %why, "the head did not take this machine's workdir reports; asking again each tick");
                    self.workdir_waiting = Some(why);
                }
                // The claim and reports before it were answered: the tick
                // is not a silent one.
                true
            }
            None => {
                self.workdir_waiting = None;
                true
            }
        }
    }

    /// Copy the folder trust the head saved for this machine into this
    /// store, where the actor's `auto_answer` reads it, while a task here may
    /// need it: one that is queued (before its dispatch, so the answer after
    /// it is immediate, as on the head; a task with no repo asks for the
    /// folder it will start in too) or blocked during its
    /// startup with no trust keys sent yet (a `trust add` on the head after
    /// the block; the actor looked at that task when it blocked, so a copy
    /// that changed sends it `auto_answer` now rather than leaving it to its
    /// next reconcile). A head that refuses (one too old for `PullTrust`,
    /// say) leaves nothing here to answer with; one that cannot be reached
    /// or does not answer leaves the last copy. Answers false when the head
    /// did not answer.
    async fn fetch_trust(&mut self) -> bool {
        let tasks: Vec<Task> = self.tasks();
        let queued = tasks.iter().any(|t| t.state == TaskState::Queued);
        let blocked = tasks.iter().any(|t| {
            t.state == TaskState::Blocked
                && t.prompt_pending
                && !self.store.trust_sent(t.id).unwrap_or(true)
        });
        if !queued && !blocked {
            return true;
        }
        let req = IpcRequest::PullTrust {
            machine: self.machine.clone(),
        };
        let answer = (self.ask)(req).await;
        if !self.heard("trust fetch", &answer) {
            return false;
        }
        let list = match answer {
            Ok(IpcResponse::Trusted(list)) => {
                if self.trust_refused.take().is_some() {
                    tracing::info!(machine = %self.machine, "the head gives this machine its saved trust again");
                }
                list
            }
            Ok(other) => {
                tracing::warn!(?other, "the head answered PullTrust with something else");
                return true;
            }
            Err(err) => {
                let Some(e) = err.downcast_ref::<CliError>() else {
                    return true;
                };
                // Only a head that refuses this machine takes its trust
                // away. Anything else failed on the way (a dropped bridge, a
                // head restarting) and keeps the last copy.
                if !matches!(
                    e.code.as_str(),
                    "head_too_old" | "not_pull_machine" | "unknown_machine"
                ) {
                    return true;
                }
                if self.trust_refused.as_deref() != Some(e.code.as_str()) {
                    tracing::info!(
                        machine = %self.machine,
                        code = %e.code,
                        "the head gives this machine no saved trust; its tasks' trust prompts wait for a person: {}",
                        e.message
                    );
                    self.trust_refused = Some(e.code.clone());
                }
                Vec::new()
            }
        };
        // Only this machine's, whatever the head sent.
        let mine: Vec<_> = list
            .into_iter()
            .filter(|t| t.machine == self.machine)
            .collect();
        let pairs = |list: &[crate::store::TrustedRepo]| -> HashSet<(String, String)> {
            list.iter()
                .map(|t| (t.machine.clone(), t.repo.clone()))
                .collect()
        };
        let before = self.store.trusted_repos().map(|l| pairs(&l));
        if let Err(err) = self.store.replace_trusted_repos(&mine) {
            tracing::error!(%err, "keep the head's saved trust");
            return true;
        }
        if blocked && before.is_ok_and(|b| b != pairs(&mine)) {
            let actor = self.actor();
            tokio::spawn(async move {
                if let Err(err) = actor.auto_trust().await {
                    tracing::warn!(err = %format!("{err:#}"), "auto trust after a trust fetch");
                }
            });
        }
        true
    }

    /// Send what `pastor join` left in `<state>/bootstrap.json` and delete
    /// it once the head took it. A file that does not parse is logged and
    /// deleted, since no later pass could parse it either. One that fails on
    /// the way is sent again next pass; one the head refuses is logged and
    /// kept as `bootstrap.json.refused`, for a person, and not sent again.
    async fn send_bootstrap_report(&mut self) -> bool {
        let Some(file) = self.report_file.clone() else {
            return true;
        };
        let text = match std::fs::read_to_string(&file) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
            Err(err) => {
                tracing::warn!(file = %file.display(), %err, "read the bootstrap report");
                return true;
            }
        };
        let result: crate::bootstrap::Outcome = match serde_json::from_str(&text) {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!(file = %file.display(), %err, "the bootstrap report is not one pastor reads; dropped");
                let _ = std::fs::remove_file(&file);
                return true;
            }
        };
        let req = IpcRequest::BootstrapReport {
            machine: self.machine.clone(),
            result,
        };
        let answer = (self.ask)(req).await;
        if !self.heard("bootstrap report", &answer) {
            return false;
        }
        let err = match answer {
            Ok(_) => {
                tracing::info!(machine = %self.machine, "bootstrap reported to the head");
                if let Err(err) = std::fs::remove_file(&file) {
                    tracing::warn!(file = %file.display(), %err, "remove the sent bootstrap report");
                }
                return true;
            }
            Err(err) => err,
        };
        let refused = err
            .downcast_ref::<CliError>()
            .filter(|e| !failed_on_the_way(&e.code));
        let Some(e) = refused else {
            tracing::warn!(machine = %self.machine, err = %format!("{err:#}"), "report this machine's bootstrap; trying again next pass");
            return true;
        };
        let aside = file.with_extension("json.refused");
        tracing::warn!(
            machine = %self.machine,
            code = %e.code,
            file = %aside.display(),
            "the head refused this machine's bootstrap report, so it is not sent again: {}",
            e.message
        );
        if let Err(err) = std::fs::rename(&file, &aside) {
            tracing::warn!(file = %file.display(), %err, "keep the refused bootstrap report aside; removed instead");
            let _ = std::fs::remove_file(&file);
        }
        true
    }

    /// Ask the head for tasks and keep each as a queued row here.
    async fn claim(&mut self) -> bool {
        let places_sets = self.settings.workdir_gate.0.is_some();
        let req = |versions: bool, sets: bool| IpcRequest::TaskClaim {
            machine: self.machine.clone(),
            free_slots: CLAIM_AT_ONCE,
            flock_work: self.flock_work,
            herdr_version: self.herdr_version.clone().filter(|_| versions),
            pastor_version: versions.then(|| env!("CARGO_PKG_VERSION").to_string()),
            workdir_sets: sets,
        };
        // A head from before workdir sets on pull machines, or before the
        // versions, refuses them before the send (`head_too_old`); it still
        // hands out tasks to a claim without them, which matters more. The
        // claim then says this serve places no sets, and gets none.
        let too_old = |answer: &anyhow::Result<IpcResponse>| {
            answer.as_ref().is_err_and(|err| {
                err.downcast_ref::<CliError>()
                    .is_some_and(|e| e.code == "head_too_old")
            })
        };
        let mut answer = (self.ask)(req(true, places_sets)).await;
        if places_sets && too_old(&answer) {
            answer = (self.ask)(req(true, false)).await;
        }
        if too_old(&answer) {
            answer = (self.ask)(req(false, false)).await;
        }
        if !self.heard("claim", &answer) {
            return false;
        }
        let tasks = match answer {
            Ok(IpcResponse::Tasks(tasks)) => {
                if self.refused.take().is_some() {
                    tracing::info!(machine = %self.machine, "the head gives this machine tasks again");
                }
                tasks
            }
            Ok(other) => {
                tracing::warn!(?other, "the head answered TaskClaim with something else");
                return true;
            }
            Err(err) => {
                if let Some(e) = err.downcast_ref::<CliError>()
                    && self.refused.as_deref() != Some(e.code.as_str())
                {
                    tracing::info!(
                        machine = %self.machine,
                        code = %e.code,
                        "the head gives this machine no tasks: {}",
                        e.message
                    );
                    self.refused = Some(e.code.clone());
                }
                return true;
            }
        };
        self.actor();
        self.read_herdr_version().await;
        for t in tasks {
            match self.store.adopt_claimed(&t) {
                Ok(_) => tracing::info!(task = %t.display_id(), "claimed from the head"),
                Err(err) => tracing::error!(task = %t.display_id(), %err, "keep a claimed task"),
            }
        }
        true
    }

    /// Ask this machine's herdr, through the connector its tasks run on, for
    /// its version, until it answers once; the next claim carries it. Only
    /// after the head answered a claim, so a machine the head does not pull
    /// never talks to its herdr.
    async fn read_herdr_version(&mut self) {
        use crate::herdr::client::ConnectorExt;
        if self.herdr_version.is_some() {
            return;
        }
        match tokio::time::timeout(VERSION_PING_TIMEOUT, self.connector.ping()).await {
            Ok(Ok(pong)) => self.herdr_version = Some(pong.version),
            Ok(Err(err)) => tracing::debug!(%err, "read this machine's herdr version"),
            Err(_) => tracing::debug!("read this machine's herdr version: timed out"),
        }
    }

    /// Hand every queued row to the actor, each once while it waits there.
    /// A `--now` task goes first, on the actor's urgent channel, so it does
    /// not wait behind a task the actor is still starting.
    fn start_queued(&mut self) {
        let mut queued: Vec<(i64, bool)> = self
            .tasks()
            .into_iter()
            .filter(|t| t.state == TaskState::Queued)
            .map(|t| (t.id, t.spec.now))
            .collect();
        if queued.is_empty() {
            return;
        }
        queued.sort_by_key(|(_, now)| !now);
        let actor = self.actor();
        for (id, now) in queued {
            if !self.dispatching.lock().unwrap().insert(id) {
                continue;
            }
            let (actor, dispatching) = (actor.clone(), self.dispatching.clone());
            tokio::spawn(async move {
                let started = if now {
                    actor.dispatch_now(id).await
                } else {
                    actor.dispatch(id).await
                };
                if let Err(err) = started {
                    tracing::warn!(task = %Task::agent_name_for(id), err = %format!("{err:#}"), "dispatch failed");
                }
                dispatching.lock().unwrap().remove(&id);
            });
        }
    }

    /// Tell the head about every row that changed since it last heard, or
    /// with `all` every row: the head answers an unchanged report with its
    /// row and nothing else, which is how a `task close` there reaches the
    /// pane here (`follow_head`). A row still queued here, claimed and not
    /// started, is `starting` on the head since the claim and reported so,
    /// so what the head did to it meanwhile reaches here before an agent
    /// is started for it. Stops at the first report that does not reach
    /// the head, to try again next tick. The trust keys the actor pressed
    /// for a task go with its reports until one reaches the head, which
    /// emits `task.trusted` once; a head too old for them gets the report
    /// without.
    async fn report(&mut self, all: bool) -> bool {
        for t in self.tasks() {
            let trust_sent = self.store.trust_sent(t.id).unwrap_or(false);
            // Sent with every report: the head keeps the first reason it
            // hears for each, so one sent again changes nothing there.
            let skipped: Vec<crate::task::WorkdirSkip> = t
                .spec
                .workdir_bundles
                .iter()
                .filter_map(|b| {
                    Some(crate::task::WorkdirSkip {
                        id: b.id.clone(),
                        why: b.skipped.clone()?,
                    })
                })
                .collect();
            let now: Reported = (
                t.state,
                t.pane_id.clone(),
                t.error.clone(),
                trust_sent,
                skipped.clone(),
            );
            if !all && self.sent.get(&t.id) == Some(&now) {
                continue;
            }
            let state = match t.state {
                TaskState::Queued => TaskState::Starting,
                state => state,
            };
            // The keys as pressed, kept with the flag: after a restart the
            // config may name others, or none. The head emits once whatever
            // a restart sends again.
            let trusted = (trust_sent && !self.sent.get(&t.id).is_some_and(|r| r.3))
                .then(|| self.store.trust_keys(t.id).ok().flatten())
                .flatten();
            let req = |trusted: Option<Vec<String>>,
                       workdir_skipped: Vec<crate::task::WorkdirSkip>| {
                IpcRequest::TaskReport {
                    machine: self.machine.clone(),
                    id: t.id,
                    state,
                    pane: t.pane_id.clone(),
                    detail: t.error.clone(),
                    // Nothing here reads a limit from a pane yet.
                    limit: None,
                    trusted,
                    // Dispatched here, not by the head, so only this report
                    // tells it the worktree this task's own dispatch opened.
                    checkout: t.spec.checkout.as_deref().cloned(),
                    workdir_skipped,
                }
            };
            // A head too old for a field refuses it before the send: the
            // report goes again without the skips, then without the trust
            // answer, since the state matters more than either.
            let too_old = |answer: &anyhow::Result<IpcResponse>| {
                answer.as_ref().err().is_some_and(|err| {
                    err.downcast_ref::<CliError>()
                        .is_some_and(|e| e.code == "head_too_old")
                })
            };
            let mut answer = (self.ask)(req(trusted.clone(), skipped.clone())).await;
            if !skipped.is_empty() && too_old(&answer) {
                answer = (self.ask)(req(trusted.clone(), Vec::new())).await;
            }
            if trusted.is_some() && too_old(&answer) {
                answer = (self.ask)(req(None, Vec::new())).await;
            }
            if !self.heard("report", &answer) {
                return false;
            }
            let head_row = match answer {
                Ok(IpcResponse::Task(row)) => row,
                Ok(other) => {
                    tracing::warn!(?other, "the head answered TaskReport with something else");
                    return true;
                }
                Err(err) => {
                    let code = err.downcast_ref::<CliError>().map(|e| e.code.clone());
                    match code.as_deref() {
                        // The head has no such task here any more: nothing
                        // left to tell it.
                        Some("task_not_found" | "not_on_machine") => {
                            tracing::warn!(task = %t.display_id(), err = %format!("{err:#}"), "the head disowns a task this machine runs; forgetting it here");
                            self.forget(t.id);
                            continue;
                        }
                        _ => {
                            tracing::debug!(task = %t.display_id(), err = %format!("{err:#}"), "report not delivered; again next tick");
                            return true;
                        }
                    }
                }
            };
            tracing::info!(task = %t.display_id(), state = %state, "reported to the head");
            self.sent.insert(t.id, now);
            self.follow_head(&t, head_row).await;
        }
        true
    }

    /// Bring the row here (`t`) after the head's (`head`), which a report
    /// just answered with. One the head has closed (`task close`) is
    /// closed here, where the pane is. One the head has done and ended
    /// (the agent's `task done` goes to the head) is ended here too, with
    /// the head's summary, so its agent's idle is not read as idle without
    /// `task done` and auto-close takes its pane; one not started here yet
    /// is closed outright, nothing being here to end, and the head hears
    /// `closed` at the next report, since its row holds a seat until then.
    /// A closed or failed row the head has heard of is forgotten here.
    async fn follow_head(&mut self, t: &Task, head: Task) {
        let actor = self.actor();
        let (what, done) = match (head.state, head.ended) {
            (TaskState::Closed, _) if t.state.is_open() => (
                "close a task the head closed",
                actor.close(t.id, false).await,
            ),
            (TaskState::Done, true) if t.state == TaskState::Queued => (
                "close a task the head ended before it started here",
                actor.close(t.id, false).await,
            ),
            (TaskState::Done, true) if t.state.occupies_pane() && !t.ended => (
                "end a task the head ended",
                actor
                    .end_by(t.id, head.summary.map(|s| s.text), EndBy::Head)
                    .await,
            ),
            _ => {
                if matches!(t.state, TaskState::Closed | TaskState::Failed) {
                    self.forget(t.id);
                }
                return;
            }
        };
        if let Err(err) = done {
            tracing::warn!(task = %t.display_id(), err = %format!("{err:#}"), "{what}");
        }
    }

    fn forget(&mut self, id: i64) {
        self.sent.remove(&id);
        if let Err(err) = self.store.forget_task(id) {
            tracing::error!(task = %Task::agent_name_for(id), %err, "forget a claimed task");
        }
    }
}

/// Reads the head's events for this machine's hooks, from where the last
/// read stopped.
struct Follower {
    head: String,
    ask: Ask,
    store: Arc<Store>,
    to_hooks: mpsc::Sender<EventRecord>,
    /// Whether the last pass reached the head; `None` before the first.
    reachable: Option<bool>,
}

impl Follower {
    /// Whether this machine has never read any of the head's events: its
    /// first successful read, whenever it comes, treats everything it finds
    /// as history (`catch_up`) and skips it, so a report sent before that
    /// read would have the event it makes skipped with it (`tick`).
    fn cursor_unset(&self) -> bool {
        self.store.meta(CURSOR_KEY).ok().flatten().is_none()
    }

    /// One catch-up, logging when the head stops or starts answering.
    /// Answers how it went, for a caller that tells a silence from a
    /// refusal (`head_silent`).
    async fn read(&mut self) -> anyhow::Result<()> {
        let caught_up = self.catch_up().await;
        self.note(
            caught_up
                .as_ref()
                .map(|_| ())
                .map_err(|err| format!("{err:#}")),
        );
        caught_up
    }

    /// `read`, for a caller that does not need how it went.
    async fn pass(&mut self) {
        let _ = self.read().await;
    }

    /// Log a pass that reached the head, or did not (`Err`, with why), when
    /// that changes.
    fn note(&mut self, reached: Result<(), String>) {
        match reached {
            Ok(()) => {
                if self.reachable == Some(false) {
                    tracing::info!(head = %self.head, "the head answers again");
                }
                self.reachable = Some(true);
            }
            Err(err) => {
                if self.reachable != Some(false) {
                    tracing::warn!(
                        code = "shepherd_needs_head",
                        head = %self.head,
                        %err,
                        "the head does not answer: this machine's jobs cannot queue tasks and its hooks hear nothing until it does; asking again each tick"
                    );
                }
                self.reachable = Some(false);
            }
        }
    }

    /// Every head event past the cursor, to the hooks in order, the cursor
    /// saved after each. With no cursor yet, the head's history is skipped:
    /// the hooks hear what happens from the first time this machine reached
    /// the head on.
    async fn catch_up(&mut self) -> anyhow::Result<()> {
        let cursor = self
            .store
            .meta(CURSOR_KEY)?
            .and_then(|v| v.parse::<u64>().ok());
        let skip = cursor.is_none();
        let mut after = cursor.unwrap_or(0);
        loop {
            let page = self.events_since(after).await?;
            let full = page.events.len() >= PAGE as usize;
            if skip {
                after = page.events.last().map_or(after, |r| r.seq);
                self.store.set_meta(CURSOR_KEY, &after.to_string())?;
            } else if let Some(end) = page.ends_before(after) {
                // Never replayed: the hooks would fire again for records
                // they may have heard under the old numbers.
                tracing::warn!(
                    code = "head_events_reset",
                    head = %self.head,
                    after,
                    end,
                    "the head's events log ends before this machine's cursor (the head moved or its state was wiped); the hooks go on from its end"
                );
                self.store.set_meta(CURSOR_KEY, &end.to_string())?;
                return Ok(());
            } else {
                // The records left still go to the hooks, from the oldest
                // the head returned: a lost stretch is logged, not retried.
                if page.gap {
                    tracing::warn!(
                        code = "head_events_gap",
                        head = %self.head,
                        after,
                        oldest = ?page.oldest,
                        "head events past the cursor were rotated out of its log before this machine read them; the hooks missed them and go on from the oldest record left"
                    );
                }
                for rec in page.events {
                    after = rec.seq;
                    if self.to_hooks.send(rec).await.is_err() {
                        anyhow::bail!("the hook runner stopped");
                    }
                    self.store.set_meta(CURSOR_KEY, &after.to_string())?;
                }
            }
            if !full {
                return Ok(());
            }
        }
    }

    async fn events_since(&self, after: u64) -> anyhow::Result<EventsPage> {
        match (self.ask)(IpcRequest::EventsSince {
            after,
            limit: PAGE,
            task: None,
            machine: None,
        })
        .await?
        {
            IpcResponse::Events(page) => Ok(page),
            other => anyhow::bail!("the head answered EventsSince with {other:?}"),
        }
    }
}

#[cfg(all(test, feature = "fake-herdr"))]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A pull machine's serve applies the env layers of its own config: its
    /// `[defaults]`, and its flock.toml's flocks (for a claimed task to pick
    /// by the name of its flock) or its own entry's; with no flock.toml, the
    /// defaults alone. The head's layers are not known to it.
    #[test]
    fn a_pull_machine_applies_the_layers_of_its_own_config() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        let config = PastorConfig::parse(
            std::path::Path::new("pastor.toml"),
            "[shepherd]\nmachine = \"far\"\n[defaults]\nenv = { A = \"defaults\" }\n",
        )
        .unwrap();
        let none = pull_settings(&paths, &config).unwrap().env;
        assert_eq!(none.defaults["A"], "defaults");
        assert!(none.flocks.is_empty() && none.machine.is_empty());
        assert!(pull_settings(&paths, &config).unwrap().limits.is_none());

        // Its flock.toml names the flocks tasks may come from.
        std::fs::write(
            paths.flock_file(),
            "[[flock]]\nname = \"work\"\ndefault = true\nenv = { B = \"work\" }\n",
        )
        .unwrap();
        let layers = pull_settings(&paths, &config).unwrap().env;
        assert_eq!(layers.merged(Some("work"))["B"], "work");
        assert!(!layers.merged(Some("other")).contains_key("B"));

        // Or its own entry, with its own env.
        std::fs::write(
            paths.flock_file(),
            "[[flock]]\nname = \"work\"\ndefault = true\nmachines = { far = 1 }\nenv = { B = \"work\" }\n\
             [[machine]]\nname = \"far\"\npull = true\nenv = { B = \"far\" }\n",
        )
        .unwrap();
        let layers = pull_settings(&paths, &config).unwrap().env;
        assert_eq!(layers.merged(Some("work"))["B"], "far");
        assert_eq!(layers.merged(Some("work"))["A"], "defaults");
    }

    /// A flock.toml that is there but does not load stops the serve with the
    /// file and the reason, not a serve that runs tasks without the env
    /// layers it sets: a syntax error, an env name that is not valid, a
    /// value that looks like a secret.
    #[test]
    fn a_pull_machines_flock_that_does_not_load_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        std::fs::create_dir_all(&paths.config_dir).unwrap();
        let config = PastorConfig::parse(
            std::path::Path::new("pastor.toml"),
            "[shepherd]\nmachine = \"far\"\n",
        )
        .unwrap();
        // Put together here so that no line of this file looks like a token to
        // a secret scanner.
        let token_env = format!(
            "[[flock]]\nname = \"work\"\ndefault = true\nenv = {{ TOKEN = \"{}\" }}\n",
            ["ghp_", "abcdefghijklmnopqrstuvwxyz0123456789"].concat()
        );
        for (text, reason) in [
            ("[[flock]\nname = ", "parse"),
            (
                "[[flock]]\nname = \"work\"\ndefault = true\nenv = { \"BAD NAME\" = \"x\" }\n",
                "BAD NAME",
            ),
            (token_env.as_str(), "TOKEN"),
        ] {
            std::fs::write(paths.flock_file(), text).unwrap();
            let Err(err) = pull_settings(&paths, &config) else {
                panic!("{text} loaded");
            };
            let err = err.to_string();
            assert!(
                err.contains(&paths.flock_file().display().to_string()),
                "{err}"
            );
            assert!(err.contains(reason), "{err}");
            assert!(err.contains("not started"), "{err}");
        }
    }

    fn rec(seq: u64) -> EventRecord {
        EventRecord {
            summary: None,
            seq,
            at: chrono::Utc::now(),
            kind: "task.done".into(),
            task: None,
            model: None,
            job: None,
            machine: None,
            detail: None,
        }
    }

    /// A head whose events log is `log`, or that cannot be reached while
    /// `down` is set.
    fn head(log: Arc<Mutex<Vec<EventRecord>>>, down: Arc<Mutex<bool>>) -> Ask {
        Arc::new(move |req| {
            let log = log.clone();
            let down = *down.lock().unwrap();
            Box::pin(async move {
                if down {
                    return Err(CliError::err("head_unreachable", "ssh: no route"));
                }
                let IpcRequest::EventsSince { after, limit, .. } = req else {
                    panic!("{req:?}")
                };
                // As `events::since` answers: `gap` when the oldest record
                // left is past `after + 1`.
                let log = log.lock().unwrap();
                let newest = log.iter().map(|r| r.seq).max();
                let oldest = log.first().map(|r| r.seq);
                let events: Vec<EventRecord> = log
                    .iter()
                    .filter(|r| r.seq > after)
                    .take(limit as usize)
                    .cloned()
                    .collect();
                Ok(IpcResponse::Events(EventsPage {
                    events,
                    gap: oldest.is_some_and(|o| o > after + 1),
                    oldest,
                    newest,
                }))
            })
        })
    }

    fn follower(ask: Ask) -> (Follower, mpsc::Receiver<EventRecord>) {
        let (to_hooks, rx) = mpsc::channel(1024);
        let f = Follower {
            head: "user@pi-1".into(),
            ask,
            store: Arc::new(Store::open_in_memory().unwrap()),
            to_hooks,
            reachable: None,
        };
        (f, rx)
    }

    fn seqs(rx: &mut mpsc::Receiver<EventRecord>) -> Vec<u64> {
        std::iter::from_fn(|| rx.try_recv().ok())
            .map(|r| r.seq)
            .collect()
    }

    /// The first reach skips the head's history, however many pages; after
    /// it, every new event goes to the hooks once, in order, and the cursor
    /// survives a head that stops answering.
    #[tokio::test]
    async fn the_hooks_hear_head_events_from_the_first_reach_on() {
        let log = Arc::new(Mutex::new((1..=450).map(rec).collect::<Vec<_>>()));
        let down = Arc::new(Mutex::new(true));
        let (mut f, mut rx) = follower(head(log.clone(), down.clone()));
        f.pass().await;
        assert_eq!(f.reachable, Some(false));
        assert_eq!(f.store.meta(CURSOR_KEY).unwrap(), None);

        *down.lock().unwrap() = false;
        f.pass().await;
        assert_eq!(f.reachable, Some(true));
        assert!(seqs(&mut rx).is_empty(), "history skipped");
        assert_eq!(f.store.meta(CURSOR_KEY).unwrap().as_deref(), Some("450"));

        log.lock().unwrap().extend((451..=700).map(rec));
        *down.lock().unwrap() = true;
        f.pass().await;
        *down.lock().unwrap() = false;
        f.pass().await;
        assert_eq!(seqs(&mut rx), (451..=700).collect::<Vec<_>>());
        f.pass().await;
        assert!(seqs(&mut rx).is_empty(), "each event once");
    }

    /// An empty head log still sets the cursor, so its first event is heard.
    #[tokio::test]
    async fn an_empty_head_log_starts_the_cursor_at_zero() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let (mut f, mut rx) = follower(head(log.clone(), Arc::new(Mutex::new(false))));
        f.pass().await;
        assert_eq!(f.store.meta(CURSOR_KEY).unwrap().as_deref(), Some("0"));
        log.lock().unwrap().push(rec(1));
        f.pass().await;
        assert_eq!(seqs(&mut rx), vec![1]);
    }

    /// Waits for `path`, one record per line, to hold as many records as
    /// `want`, and checks their sequence numbers.
    async fn heard(path: &std::path::Path, want: &[u64]) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let got: Vec<u64> = std::fs::read_to_string(path)
                .unwrap_or_default()
                .lines()
                .filter_map(|l| serde_json::from_str::<EventRecord>(l).ok())
                .map(|r| r.seq)
                .collect();
            if got.len() >= want.len() {
                assert_eq!(got, want, "{}", path.display());
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{}: {got:?}",
                path.display()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Records rotated out of the head's log past the cursor are lost to
    /// the hooks; the follower goes on from the oldest record the head
    /// returned, and each one after it is heard once.
    #[tokio::test]
    async fn a_gap_goes_on_from_the_oldest_record_returned() {
        let log = Arc::new(Mutex::new((1..=10).map(rec).collect::<Vec<_>>()));
        let (mut f, mut rx) = follower(head(log.clone(), Arc::new(Mutex::new(false))));
        f.pass().await;
        assert_eq!(f.store.meta(CURSOR_KEY).unwrap().as_deref(), Some("10"));

        // 11..=299 are written and rotated out before the next tick.
        *log.lock().unwrap() = (300..=320).map(rec).collect();
        f.pass().await;
        assert_eq!(f.reachable, Some(true), "a gap is not a failure");
        assert_eq!(seqs(&mut rx), (300..=320).collect::<Vec<_>>());
        assert_eq!(f.store.meta(CURSOR_KEY).unwrap().as_deref(), Some("320"));
        log.lock().unwrap().push(rec(321));
        f.pass().await;
        assert_eq!(seqs(&mut rx), vec![321]);
    }

    /// A cursor past the head's newest event (the head moved, or its state
    /// dir was wiped) moves to the head's end: nothing up to it is heard
    /// again, what comes after is.
    #[tokio::test]
    async fn a_cursor_past_the_heads_log_goes_on_from_its_end() {
        let log = Arc::new(Mutex::new((1..=20).map(rec).collect::<Vec<_>>()));
        let (mut f, mut rx) = follower(head(log.clone(), Arc::new(Mutex::new(false))));
        f.store.set_meta(CURSOR_KEY, "500").unwrap();
        f.pass().await;
        assert_eq!(f.reachable, Some(true));
        assert!(seqs(&mut rx).is_empty(), "no replay");
        assert_eq!(f.store.meta(CURSOR_KEY).unwrap().as_deref(), Some("20"));
        f.pass().await;
        assert!(seqs(&mut rx).is_empty());
        log.lock().unwrap().extend((21..=22).map(rec));
        f.pass().await;
        assert_eq!(seqs(&mut rx), vec![21, 22]);
    }

    #[tokio::test]
    async fn an_empty_head_log_under_an_old_cursor_goes_on_from_zero() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let (mut f, mut rx) = follower(head(log.clone(), Arc::new(Mutex::new(false))));
        f.store.set_meta(CURSOR_KEY, "500").unwrap();
        f.pass().await;
        assert_eq!(f.store.meta(CURSOR_KEY).unwrap().as_deref(), Some("0"));
        log.lock().unwrap().push(rec(1));
        f.pass().await;
        assert_eq!(seqs(&mut rx), vec![1]);
    }

    /// Head events reach this machine's hooks through the hook runner: an
    /// `only_own` hook hears only tasks of a job in this machine's `jobs/`
    /// that uses its connector, and records about no job; a hook without
    /// `only_own` hears every record in its `on`.
    #[tokio::test]
    async fn only_own_is_decided_by_this_machines_job_files() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        paths.ensure().unwrap();
        let out = tmp.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        let dir = paths.connectors_dir().join("note");
        std::fs::create_dir_all(&dir).unwrap();
        // Each hook appends the record it got, one JSON line.
        let hook = |name: &str| format!("cat >> '{}'", out.join(name).display());
        std::fs::write(dir.join("own.sh"), hook("own")).unwrap();
        std::fs::write(dir.join("all.sh"), hook("all")).unwrap();
        std::fs::write(
            dir.join(crate::connector::manifest::MANIFEST_FILE),
            "id = \"note\"\nversion = \"0.1.0\"\n[connector]\ncommand = [\"true\"]\n\
             [[events]]\non = [\"task.done\", \"machine.lost\"]\nonly_own = true\ncommand = [\"sh\", \"own.sh\"]\n\
             [[events]]\non = [\"task.done\"]\ncommand = [\"sh\", \"all.sh\"]\n",
        )
        .unwrap();
        std::fs::create_dir_all(paths.jobs_dir()).unwrap();
        std::fs::write(
            crate::config::job::job_path(&paths.jobs_dir(), "mine"),
            "every = \"1h\"\n[connector]\nuse = \"note\"\n[dispatch]\nprompt = \"p\"\n",
        )
        .unwrap();

        let about = |seq: u64, kind: &str, job: Option<&str>| EventRecord {
            summary: None,
            kind: kind.into(),
            job: job.map(str::to_string),
            ..rec(seq)
        };
        let log = Arc::new(Mutex::new(vec![about(1, "task.done", None)]));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (events, _) = broadcast::channel(4);
        let (to_hooks, hooks_rx) = mpsc::channel(16);
        let runner =
            crate::hooks::spawn(paths.clone(), store.clone(), events.downgrade(), hooks_rx);
        let mut f = Follower {
            head: "user@pi-1".into(),
            ask: head(log.clone(), Arc::new(Mutex::new(false))),
            store,
            to_hooks,
            reachable: None,
        };
        f.pass().await;
        log.lock().unwrap().extend([
            about(2, "task.done", Some("mine")),
            about(3, "task.done", Some("theirs")),
            about(4, "machine.lost", None),
            about(5, "task.queued", Some("mine")),
        ]);
        f.pass().await;
        drop(f);
        tokio::time::timeout(Duration::from_secs(10), runner)
            .await
            .unwrap()
            .unwrap();
        // Queued hooks outlive the runner.
        heard(&out.join("own"), &[2, 4]).await;
        heard(&out.join("all"), &[2, 3]).await;
    }

    /// A head in this process, with `laptop` as a pull machine, and an
    /// `Ask` that reaches it as the bridge would.
    async fn pull_head(tmp: &std::path::Path) -> (Arc<Daemon>, Ask) {
        let paths = Paths::new(tmp.join("head/c"), tmp.join("head/s"));
        paths.ensure().unwrap();
        let flock: crate::config::flock::Flock =
            toml::from_str("[[machine]]\nname = \"laptop\"\npull = true\n").unwrap();
        flock.save(&paths.flock_file()).unwrap();
        let config = PastorConfig::default();
        std::fs::write(paths.config_file(), toml::to_string(&config).unwrap()).unwrap();
        let on_disk = ConfigFingerprint::sample(&paths);
        let head = Arc::new(
            Daemon::start(paths, config, flock, on_disk, None)
                .await
                .unwrap(),
        );
        let to = head.clone();
        let ask: Ask = Arc::new(move |req| {
            let head = to.clone();
            Box::pin(async move {
                match head.handle(req).await {
                    IpcResponse::Error { code, message } => Err(CliError::err(&code, message)),
                    resp => Ok(resp),
                }
            })
        });
        (head, ask)
    }

    fn fast() -> MachineSettings {
        MachineSettings {
            settle: Duration::from_millis(100),
            reconcile_every: Duration::from_millis(200),
            initial_backoff: Duration::from_millis(50),
            max_backoff: Duration::from_millis(200),
            request_timeout: Duration::from_secs(5),
            agent_ready_timeout: Duration::from_millis(500),
            poll_every: Duration::from_millis(200),
            close_done_after: None,
            close_failed_after: None,
            ..Default::default()
        }
    }

    async fn run_pinned(head: &Daemon, machine: Option<&str>) -> Task {
        run_in(head, machine, None).await
    }

    /// `run_pinned`, in `repo` when given.
    async fn run_in(head: &Daemon, machine: Option<&str>, repo: Option<&str>) -> Task {
        run_task(head, machine, repo, "claude").await
    }

    /// `run_in`, with `agent`, and a summary the agent must give.
    async fn run_task(
        head: &Daemon,
        machine: Option<&str>,
        repo: Option<&str>,
        agent: &str,
    ) -> Task {
        match head.handle(run_request(machine, repo, agent)).await {
            IpcResponse::Task(t) => t,
            other => panic!("{other:?}"),
        }
    }

    /// The `task run` request `run_task` sends.
    fn run_request(machine: Option<&str>, repo: Option<&str>, agent: &str) -> IpcRequest {
        IpcRequest::Run {
            now: false,
            preempt: false,
            prompt: "fix it".into(),
            spec: crate::task::DispatchSpec {
                now: false,
                agent: agent.into(),
                agent_args: vec![],
                allow: vec![],
                deny: vec![],
                repo: repo.map(str::to_string),
                worktree: false,
                branch: None,
                machine: machine.map(str::to_string),
                tags: vec![],
                timeout_secs: 3600,
                checkout: None,
                reopen: None,
                agent_source: None,
                place: Default::default(),
                session_id: None,
                label: Default::default(),
                summary: crate::task::SummaryMode::Require,
                cwd: None,
                keep_pane: None,
                keep_pane_from: None,
                remove_worktree: None,
                remove_worktree_from: None,
                load: None,
                load_from: None,
                fetch: Default::default(),
                start_prompt: None,
                workdir: None,
                workdir_bundles: Vec::new(),
                rounds: Default::default(),
            },
            flock: None,
            agent: None,
            priority: None,
            role: Default::default(),
            description: None,
            summary: None,
        }
    }

    /// `task run --now` to a connected pull machine waits for the machine's
    /// next claim, which takes it first, and the machine starts it at once.
    #[tokio::test]
    async fn a_now_task_starts_on_the_pull_machine_s_next_claim() {
        let tmp = tempfile::tempdir().unwrap();
        let (head, ask) = pull_head(tmp.path()).await;
        let fake = crate::herdr::fake::FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut p = Puller::new(
            "laptop".into(),
            false,
            Arc::new(fake.clone()),
            fast(),
            store.clone(),
            ask,
        );
        // Heard from: connected.
        p.pass().await;
        let mut req = run_request(Some("laptop"), None, "claude");
        if let IpcRequest::Run { now, .. } = &mut req {
            *now = true;
        }
        let (resp, ()) = tokio::join!(head.handle(req), async {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !head
                .store()
                .queued_tasks()
                .unwrap()
                .iter()
                .any(|t| t.spec.now)
            {
                assert!(std::time::Instant::now() < deadline, "never queued");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            p.pass().await;
        });
        let IpcResponse::Task(t) = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(t.machine.as_deref(), Some("laptop"));
        assert!(store.get_task(t.id).unwrap().unwrap().spec.now);
        head_reaches(&mut p, &head, &t, TaskState::Running).await;
    }

    /// Like `run_pinned`, but a worktree task: the pull machine's own
    /// dispatch opens the checkout, which only its `TaskReport` can tell
    /// the head.
    async fn run_pinned_worktree(head: &Daemon, machine: Option<&str>) -> Task {
        let req = IpcRequest::Run {
            now: false,
            preempt: false,
            prompt: "fix it".into(),
            spec: crate::task::DispatchSpec {
                now: false,
                agent: "claude".into(),
                agent_args: vec![],
                allow: vec![],
                deny: vec![],
                repo: Some("/r".into()),
                worktree: true,
                branch: None,
                machine: machine.map(str::to_string),
                tags: vec![],
                timeout_secs: 3600,
                checkout: None,
                reopen: None,
                agent_source: None,
                place: Default::default(),
                session_id: None,
                label: Default::default(),
                summary: Default::default(),
                cwd: None,
                keep_pane: None,
                keep_pane_from: None,
                remove_worktree: None,
                remove_worktree_from: None,
                load: None,
                load_from: None,
                fetch: Default::default(),
                start_prompt: None,
                workdir: None,
                workdir_bundles: Vec::new(),
                rounds: Default::default(),
            },
            flock: None,
            agent: None,
            priority: None,
            role: Default::default(),
            description: None,
            summary: None,
        };
        match head.handle(req).await {
            IpcResponse::Task(t) => t,
            other => panic!("{other:?}"),
        }
    }

    /// Passes and reports until the head's row of `t` is `want`.
    async fn head_reaches(p: &mut Puller, head: &Daemon, t: &Task, want: TaskState) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let state = head.store().get_task(t.id).unwrap().unwrap().state;
            if state == want {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "{state} not {want}");
            let _ = tokio::time::timeout(Duration::from_millis(100), p.changed()).await;
            p.pass().await;
        }
    }

    /// The workdir reports `place --local` left while the head did not
    /// answer go with the next tick: a silent head keeps them, and an
    /// answering one records them under this machine's name.
    #[tokio::test]
    async fn a_pull_machine_hands_the_head_the_workdir_reports_it_owes() {
        let tmp = tempfile::tempdir().unwrap();
        let (head, ask) = pull_head(tmp.path()).await;
        let mine = Paths::new(tmp.path().join("pull/c"), tmp.path().join("pull/s"));
        let report = crate::bundle::workdir_push::Reported::Removed {
            dir: "/w".into(),
            ids: vec!["reviewer".into()],
            at: "2026-11-02T10:00:00Z".into(),
        };
        // The head records the place, so the removal after it has a set to
        // take out.
        let placed = crate::bundle::workdir_push::Reported::Placed {
            dir: "/w".into(),
            id: "reviewer".into(),
            kind: "claude".into(),
            profile: None,
            set_hash: "a".repeat(64),
            render_hash: None,
            at: "2026-11-02T09:00:00Z".into(),
            ok: true,
            error: None,
        };
        crate::bundle::workdir_report::queue(&mine, "laptop", &placed).unwrap();
        crate::bundle::workdir_report::queue(&mine, "laptop", &report).unwrap();
        let silent: Ask = Arc::new(|_| {
            Box::pin(async { Err(CliError::err("head_unreachable", "ssh: no route")) })
        });
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut p = Puller::new(
            "laptop".into(),
            false,
            Arc::new(crate::herdr::fake::FakeHerdr::new()),
            fast(),
            store.clone(),
            silent,
        );
        p.workdir_paths = Some(mine.clone());
        assert!(
            p.send_workdir_reports().await,
            "a silent head is not a failed tick"
        );
        assert_eq!(crate::bundle::workdir_report::pending(&mine), 2);

        let mut events = head.subscribe();
        p.ask = ask;
        assert!(p.send_workdir_reports().await);
        assert_eq!(crate::bundle::workdir_report::pending(&mine), 0);
        let seen: Vec<String> = std::iter::from_fn(|| events.try_recv().ok())
            .filter(|e| e.machine.as_deref() == Some("laptop"))
            .map(|e| e.kind)
            .collect();
        assert_eq!(seen, ["workdir.placed", "workdir.removed"]);
    }

    /// A shepherd that is a pull machine claims the task pinned to it, runs
    /// it with a machine actor on its own herdr and reports each change: the
    /// head's row and events follow it, as for a machine the head runs. A
    /// task nobody pinned is left alone until it asks for flock work, and
    /// the rows here are forgotten once the head heard the task close.
    #[tokio::test]
    async fn a_pull_machine_runs_the_heads_task_and_reports_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (head, ask) = pull_head(tmp.path()).await;
        let mut events = head.subscribe();
        let pinned = run_pinned(&head, Some("laptop")).await;
        let loose = run_pinned(&head, None).await;
        assert_eq!(pinned.state, TaskState::Queued);
        let fake = crate::herdr::fake::FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut p = Puller::new(
            "laptop".into(),
            false,
            Arc::new(fake.clone()),
            fast(),
            store.clone(),
            ask,
        );
        head_reaches(&mut p, &head, &pinned, TaskState::Running).await;
        let row = head.store().get_task(pinned.id).unwrap().unwrap();
        assert_eq!(row.machine.as_deref(), Some("laptop"));
        let pane = row.pane_id.clone().expect("the pane is reported");
        assert_eq!(
            head.store().get_task(loose.id).unwrap().unwrap().state,
            TaskState::Queued,
            "no flock work unasked"
        );

        fake.set_status(&pane, crate::herdr::AgentStatus::Idle);
        head_reaches(&mut p, &head, &pinned, TaskState::Done).await;
        let kinds: Vec<(String, Option<String>)> = std::iter::from_fn(|| events.try_recv().ok())
            .filter(|e| e.task_id == Some(pinned.id))
            .map(|e| (e.kind, e.machine))
            .collect();
        let lap = |k: &str| (k.to_string(), Some("laptop".to_string()));
        assert_eq!(
            kinds,
            [
                ("task.queued".to_string(), None),
                lap("task.running"),
                lap("task.done")
            ],
            "as the head's own actor would"
        );

        // Closed on the head: the pane here goes, and so does the row.
        let IpcResponse::Task(_) = head
            .handle(IpcRequest::TaskClose {
                id: pinned.id,
                remove_worktree: false,
            })
            .await
        else {
            panic!()
        };
        // The head closes the row alone; the pane goes at this machine's
        // next pass, which reports every task it runs.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while store.get_task(pinned.id).unwrap().is_some() {
            assert!(std::time::Instant::now() < deadline, "never forgotten");
            let _ = tokio::time::timeout(Duration::from_millis(100), p.changed()).await;
            p.pass().await;
        }
        assert!(fake.agents().is_empty(), "{:?}", fake.agents());

        p.flock_work = true;
        head_reaches(&mut p, &head, &loose, TaskState::Running).await;
    }

    /// An agent's `pastor task done` reaches the head, which ends its row
    /// there; the row here learns nothing of it on its own. An agent that
    /// is not Claude, idle after it, would read here as idle without `task
    /// done` and sit `blocked` with its pane and the head's seat for good.
    /// A head row that is done and ended ends the row here too, with the
    /// head's summary (the task requires one, and the head took it), so
    /// auto-close takes the pane once the agent is idle, the head hears
    /// `closed`, and the row is forgotten, as a task closed on the head.
    #[tokio::test]
    async fn a_task_its_agent_ended_on_the_head_is_closed_here_once_idle() {
        let tmp = tempfile::tempdir().unwrap();
        let (head, ask) = pull_head(tmp.path()).await;
        let mut events = head.subscribe();
        let t = run_task(&head, Some("laptop"), None, "agy").await;
        let fake = crate::herdr::fake::FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut p = Puller::new(
            "laptop".into(),
            false,
            Arc::new(fake.clone()),
            MachineSettings {
                close_done_after: Some(Duration::from_millis(50)),
                ..fast()
            },
            store.clone(),
            ask,
        );
        head_reaches(&mut p, &head, &t, TaskState::Running).await;
        let pane = head
            .store()
            .get_task(t.id)
            .unwrap()
            .unwrap()
            .pane_id
            .expect("the pane is reported");
        // The agent ends its task on the head, prints its last line and
        // goes idle at its prompt.
        let IpcResponse::Task(ended) = head
            .handle(IpcRequest::TaskDone {
                id: t.id,
                summary: Some("done: pushed".into()),
            })
            .await
        else {
            panic!()
        };
        assert!(ended.ended);
        fake.set_pane_text(&pane, "DONE\n> ");
        fake.set_status(&pane, crate::herdr::AgentStatus::Idle);

        let mut here_events = p.events.subscribe();
        let mut here_round = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while let Some(here) = store.get_task(t.id).unwrap() {
            if here.ended {
                here_round = here.summary.clone();
            }
            assert!(
                std::time::Instant::now() < deadline,
                "never forgotten: here {:?}, head {}",
                (here.state, here.ended),
                head.store().get_task(t.id).unwrap().unwrap().state
            );
            let _ = tokio::time::timeout(Duration::from_millis(100), p.changed()).await;
            p.pass().await;
        }
        assert!(fake.agents().is_empty(), "{:?}", fake.agents());
        // Both stores tell the same story: the agent's own summary, not a
        // round ended by hand.
        let round = here_round.expect("the row here was ended with a round");
        assert_eq!(round.text, "done: pushed");
        assert_eq!(round.outcome, crate::task::Outcome::Done);
        let here_done = std::iter::from_fn(|| here_events.try_recv().ok())
            .find(|e| e.kind == "task.done")
            .expect("the actor here sent task.done");
        assert_eq!(
            here_done.summary.map(|s| s.text),
            Some("done: pushed".to_string())
        );
        let row = head.store().get_task(t.id).unwrap().unwrap();
        assert_eq!(row.state, TaskState::Closed);
        assert_eq!(
            row.finished_at, ended.finished_at,
            "the finish is the agent's"
        );
        assert_eq!(
            row.summary.as_ref().map(|s| s.text.as_str()),
            Some("done: pushed"),
            "the summary is what the agent said"
        );
        let kinds: Vec<String> = std::iter::from_fn(|| events.try_recv().ok())
            .filter(|e| e.task_id == Some(t.id))
            .map(|e| e.kind)
            .collect();
        assert_eq!(
            kinds,
            ["task.queued", "task.running", "task.done", "task.closed"]
        );
    }

    /// A pull machine whose serve places workdir sets says so in its
    /// claim, gets a task with a set, places it in the task's folder
    /// before its agent starts, and reports it: the head's record then
    /// lists the set on that machine.
    #[tokio::test]
    async fn a_pull_machine_places_a_claimed_task_s_set_and_reports_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (head, ask) = pull_head(tmp.path()).await;
        let home = tmp.path().join("home");
        let repo = home.join("src/app");
        std::fs::create_dir_all(&repo).unwrap();
        let mut spec = crate::profile_render::base_spec("claude");
        spec.repo = Some(repo.to_string_lossy().into_owned());
        spec.machine = Some("laptop".into());
        spec.workdir = Some(Box::new(crate::task::TaskWorkdir {
            set: serde_json::json!({"id": "reviewer", "kind": "claude", "files": [
                {"path": ".claude/agents/checker.md", "content": "Check.\n"}]}),
            placed_in: None,
            render: None,
        }));
        let IpcResponse::Task(t) = head
            .handle(IpcRequest::Run {
                now: false,
                preempt: false,
                prompt: "fix it".into(),
                spec,
                flock: None,
                agent: None,
                priority: None,
                role: Default::default(),
                description: None,
                summary: None,
            })
            .await
        else {
            panic!("a task with a set pinned to a pull machine is queued")
        };
        let mine = Paths::new(tmp.path().join("pull/c"), tmp.path().join("pull/s"));
        mine.ensure().unwrap();
        let mut settings = fast();
        settings.workdir_gate = crate::bundle::workdir_task::GateHandle(Some(
            crate::bundle::workdir_task::LocalGate::at(mine.clone(), home.clone(), "laptop".into()),
        ));
        let mut p = Puller::new(
            "laptop".into(),
            false,
            Arc::new(crate::herdr::fake::FakeHerdr::new()),
            settings,
            Arc::new(Store::open_in_memory().unwrap()),
            ask,
        );
        p.workdir_paths = Some(mine.clone());
        p.pass().await;
        assert_eq!(
            head.store().get_task(t.id).unwrap().unwrap().state,
            TaskState::Starting,
            "claimed"
        );
        let placed = repo.join(".claude/agents/checker.md");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !placed.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "the set was never placed"
            );
            let _ = tokio::time::timeout(Duration::from_millis(100), p.changed()).await;
        }
        loop {
            p.send_workdir_reports().await;
            let IpcResponse::Workdirs(records) = head
                .handle(IpcRequest::WorkdirList {
                    machine: Some("laptop".into()),
                })
                .await
            else {
                panic!()
            };
            if let Some(r) = records.iter().find(|r| r.id == "reviewer") {
                assert!(
                    crate::dispatch::same_dir(&r.dir, &repo.to_string_lossy()),
                    "{r:?}"
                );
                break;
            }
            assert!(std::time::Instant::now() < deadline, "never reported");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// A serve with its own workdir gate says in its claim that it places
    /// sets. A head from before that refuses the flag before the send
    /// (`head_too_old`), so the claim goes again without it, versions kept,
    /// and this machine gets no tasks with sets.
    #[tokio::test]
    async fn a_claim_says_it_places_sets_unless_the_head_is_older() {
        let claims = |protocol: u32| async move {
            let tmp = tempfile::tempdir().unwrap();
            let sent = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
            let log = sent.clone();
            let head: Ask = Arc::new(move |req| {
                let log = log.clone();
                Box::pin(async move {
                    let v = serde_json::to_value(&req).unwrap();
                    let ping = v["op"] == "ping";
                    log.lock().unwrap().push(v);
                    Ok(if ping {
                        IpcResponse::Pong {
                            version: "0.10.0".into(),
                            protocol,
                            role: None,
                        }
                    } else {
                        IpcResponse::Tasks(Vec::new())
                    })
                })
            });
            let mut settings = fast();
            settings.workdir_gate = crate::bundle::workdir_task::GateHandle(Some(
                crate::bundle::workdir_task::LocalGate::at(
                    Paths::new(tmp.path().join("c"), tmp.path().join("s")),
                    tmp.path().to_path_buf(),
                    "laptop".into(),
                ),
            ));
            let mut p = Puller::new(
                "laptop".into(),
                false,
                Arc::new(crate::herdr::fake::FakeHerdr::new()),
                settings,
                Arc::new(Store::open_in_memory().unwrap()),
                gated(head),
            );
            assert!(p.claim().await);
            let sent: Vec<serde_json::Value> = sent
                .lock()
                .unwrap()
                .iter()
                .filter(|v| v["op"] == "task_claim")
                .cloned()
                .collect();
            sent
        };
        let now = claims(crate::ipc::IPC_PROTOCOL).await;
        assert_eq!(now.len(), 1);
        assert_eq!(now[0]["workdir_sets"], true);
        let older = claims(crate::ipc::WORKDIR_PULL_PROTOCOL - 1).await;
        assert_eq!(older.len(), 1, "{older:?}");
        assert!(older[0].get("workdir_sets").is_none(), "{older:?}");
        assert_eq!(older[0]["pastor_version"], env!("CARGO_PKG_VERSION"));
    }

    /// A task claimed here and not started yet is queued here and
    /// `starting` on the head. One a person ends on the head meanwhile
    /// (`task done t-N`) is closed here, no agent ever started for it, and
    /// the head hears `closed` and frees its seat, with the finish the
    /// person's end gave it.
    #[tokio::test]
    async fn a_task_the_head_ended_before_it_started_here_is_never_started() {
        let tmp = tempfile::tempdir().unwrap();
        let (head, ask) = pull_head(tmp.path()).await;
        let mut events = head.subscribe();
        let t = run_pinned(&head, Some("laptop")).await;
        let fake = crate::herdr::fake::FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut p = Puller::new(
            "laptop".into(),
            false,
            Arc::new(fake.clone()),
            fast(),
            store.clone(),
            ask,
        );
        // Claimed, and not yet handed to the actor.
        p.claim().await;
        let here = store.get_task(t.id).unwrap().expect("claimed");
        assert_eq!(here.state, TaskState::Queued);
        assert_eq!(
            head.store().get_task(t.id).unwrap().unwrap().state,
            TaskState::Starting
        );
        let IpcResponse::Task(ended) = head
            .handle(IpcRequest::TaskDone {
                id: t.id,
                summary: None,
            })
            .await
        else {
            panic!()
        };
        assert!(ended.ended && ended.state == TaskState::Done);

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while let Some(here) = store.get_task(t.id).unwrap() {
            assert!(
                std::time::Instant::now() < deadline,
                "never forgotten: here {}, head {}",
                here.state,
                head.store().get_task(t.id).unwrap().unwrap().state
            );
            let _ = tokio::time::timeout(Duration::from_millis(100), p.changed()).await;
            p.pass().await;
            assert!(
                fake.agents().is_empty(),
                "started anyway: {:?}",
                fake.agents()
            );
        }
        let row = head.store().get_task(t.id).unwrap().unwrap();
        assert_eq!(row.state, TaskState::Closed);
        assert_eq!(row.finished_at, ended.finished_at);
        assert_eq!(row.error, None);
        let kinds: Vec<String> = std::iter::from_fn(|| events.try_recv().ok())
            .filter(|e| e.task_id == Some(t.id))
            .map(|e| e.kind)
            .collect();
        assert_eq!(kinds, ["task.queued", "task.done", "task.closed"]);
    }

    /// The keys pressed on `pane`, each send as one list.
    fn keys_sent(fake: &crate::herdr::fake::FakeHerdr, pane: &str) -> Vec<Vec<String>> {
        fake.pane_input(pane)
            .into_iter()
            .filter_map(|i| match i {
                crate::herdr::fake::PaneInput::Keys(k) => Some(k),
                _ => None,
            })
            .collect()
    }

    /// The saved (machine, repo) pairs a pull machine's store holds.
    fn trust_here(store: &Store) -> Vec<(String, String)> {
        store
            .trusted_repos()
            .unwrap()
            .into_iter()
            .map(|t| (t.machine, t.repo))
            .collect()
    }

    /// A task of a repo the head saved as trusted on this pull machine
    /// blocks on its folder-trust prompt and is answered here, once, as
    /// the head answers another machine's: the head emits `task.trusted`
    /// with the keys, then the task runs. Another machine's saved trust
    /// never reaches this one.
    #[tokio::test]
    async fn a_pull_machine_answers_the_trust_prompt_of_a_repo_saved_for_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (head, ask) = pull_head(tmp.path()).await;
        head.store().trust_repo("laptop", "~/work/app").unwrap();
        head.store().trust_repo("pi-2", "~/work/lib").unwrap();
        let mut events = head.subscribe();
        let t = run_in(&head, Some("laptop"), Some("~/work/app")).await;
        let fake = crate::herdr::fake::FakeHerdr::new();
        fake.set_trust_prompt(Some(vec!["Down".into(), "Enter".into()]));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut p = Puller::new(
            "laptop".into(),
            false,
            Arc::new(fake.clone()),
            fast(),
            store.clone(),
            ask,
        );
        head_reaches(&mut p, &head, &t, TaskState::Running).await;
        assert_eq!(
            trust_here(&store),
            [("laptop".to_string(), "~/work/app".to_string())],
            "only this machine's"
        );
        // Some passes later, still once.
        for _ in 0..3 {
            let _ = tokio::time::timeout(Duration::from_millis(100), p.changed()).await;
            p.pass().await;
        }
        let pane = head
            .store()
            .get_task(t.id)
            .unwrap()
            .unwrap()
            .pane_id
            .unwrap();
        assert_eq!(keys_sent(&fake, &pane), [vec!["Down", "Enter"]]);
        let evs: Vec<PastorEvent> = std::iter::from_fn(|| events.try_recv().ok())
            .filter(|e| e.task_id == Some(t.id))
            .collect();
        let kinds: Vec<&str> = evs.iter().map(|e| e.kind.as_str()).collect();
        let trusted: Vec<&PastorEvent> = evs.iter().filter(|e| e.kind == "task.trusted").collect();
        assert_eq!(trusted.len(), 1, "{kinds:?}");
        assert_eq!(trusted[0].machine.as_deref(), Some("laptop"));
        assert_eq!(
            trusted[0].detail,
            Some(serde_json::json!({"keys": ["Down", "Enter"]}))
        );
        let at = |k: &str| kinds.iter().position(|x| *x == k).unwrap();
        assert!(at("task.trusted") < at("task.running"), "{kinds:?}");
        assert!(head.store().trust_sent(t.id).unwrap());
    }

    /// A task with no repo starts in the folder the pull machine's dispatch
    /// picks (`~/pastor-tasks`, absolute), and the trust the head saved for
    /// that folder reaches the machine although no task of it has a repo:
    /// the serve asks for the saved trust while such a task waits, answers
    /// its prompt once, and the head emits `task.trusted`. Another
    /// machine's trust for the same folder never reaches this one.
    #[tokio::test]
    async fn a_pull_machine_answers_the_trust_prompt_of_a_task_with_no_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let (head, ask) = pull_head(tmp.path()).await;
        head.store()
            .trust_repo("laptop", "/home/fake/pastor-tasks")
            .unwrap();
        head.store()
            .trust_repo("pi-2", "/home/fake/elsewhere")
            .unwrap();
        let mut events = head.subscribe();
        let t = run_in(&head, Some("laptop"), None).await;
        let fake = crate::herdr::fake::FakeHerdr::new();
        fake.set_trust_prompt(Some(vec!["Down".into(), "Enter".into()]));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut p = Puller::new(
            "laptop".into(),
            false,
            Arc::new(fake.clone()),
            fast(),
            store.clone(),
            ask,
        );
        head_reaches(&mut p, &head, &t, TaskState::Running).await;
        assert_eq!(
            trust_here(&store),
            [("laptop".to_string(), "/home/fake/pastor-tasks".to_string())],
            "only this machine's"
        );
        // The machine's own copy of the task recorded the folder it started
        // in, which is what the saved trust is matched with.
        let here = p.tasks();
        assert_eq!(here.len(), 1);
        assert_eq!(here[0].spec.repo, None);
        assert_eq!(here[0].spec.cwd.as_deref(), Some("/home/fake/pastor-tasks"));
        let pane = head
            .store()
            .get_task(t.id)
            .unwrap()
            .unwrap()
            .pane_id
            .unwrap();
        assert_eq!(keys_sent(&fake, &pane), [vec!["Down", "Enter"]]);
        let evs: Vec<PastorEvent> = std::iter::from_fn(|| events.try_recv().ok())
            .filter(|e| e.task_id == Some(t.id))
            .collect();
        let kinds: Vec<&str> = evs.iter().map(|e| e.kind.as_str()).collect();
        let trusted: Vec<&PastorEvent> = evs.iter().filter(|e| e.kind == "task.trusted").collect();
        assert_eq!(trusted.len(), 1, "{kinds:?}");
        assert_eq!(trusted[0].machine.as_deref(), Some("laptop"));
        let at = |k: &str| kinds.iter().position(|x| *x == k).unwrap();
        assert!(at("task.trusted") < at("task.running"), "{kinds:?}");
        assert!(head.store().trust_sent(t.id).unwrap());
    }

    /// A task of a repo saved only for another machine is left blocked on
    /// its trust prompt, for a person: nothing is pressed and the head
    /// emits no `task.trusted`. Once the repo is saved for this machine,
    /// the next pass answers it, without waiting for the actor's
    /// reconcile (an hour here).
    #[tokio::test]
    async fn a_pull_machine_leaves_a_repo_not_saved_for_it_blocked() {
        let tmp = tempfile::tempdir().unwrap();
        let (head, ask) = pull_head(tmp.path()).await;
        head.store().trust_repo("pi-2", "~/work/app").unwrap();
        let mut events = head.subscribe();
        let t = run_in(&head, Some("laptop"), Some("~/work/app")).await;
        let fake = crate::herdr::fake::FakeHerdr::new();
        fake.set_trust_prompt(Some(vec!["Down".into(), "Enter".into()]));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut p = Puller::new(
            "laptop".into(),
            false,
            Arc::new(fake.clone()),
            MachineSettings {
                reconcile_every: Duration::from_secs(3600),
                ..fast()
            },
            store.clone(),
            ask,
        );
        head_reaches(&mut p, &head, &t, TaskState::Blocked).await;
        for _ in 0..3 {
            let _ = tokio::time::timeout(Duration::from_millis(100), p.changed()).await;
            p.pass().await;
        }
        assert_eq!(
            head.store().get_task(t.id).unwrap().unwrap().state,
            TaskState::Blocked
        );
        assert!(trust_here(&store).is_empty(), "{:?}", trust_here(&store));
        let pane = head
            .store()
            .get_task(t.id)
            .unwrap()
            .unwrap()
            .pane_id
            .unwrap();
        assert!(keys_sent(&fake, &pane).is_empty());
        assert!(
            !std::iter::from_fn(|| events.try_recv().ok()).any(|e| e.kind == "task.trusted"),
            "no trust answer"
        );

        // Saved on the head now: the next pass brings it, and the actor
        // answers the prompt at once.
        head.store().trust_repo("laptop", "~/work/app").unwrap();
        head_reaches(&mut p, &head, &t, TaskState::Running).await;
        assert_eq!(keys_sent(&fake, &pane), [vec!["Down", "Enter"]]);
    }

    /// A head from before saved trust on pull machines (`PULL_TRUST_PROTOCOL`)
    /// is not asked for it (`head_too_old` before the send), so nothing here
    /// answers a trust prompt; the tasks still run and report. A trust
    /// answer that reaches its report anyway goes without the keys, rather
    /// than holding every report back.
    #[tokio::test]
    async fn a_head_that_predates_pull_trust_still_gets_the_reports() {
        let tmp = tempfile::tempdir().unwrap();
        let (head, ask) = pull_head(tmp.path()).await;
        head.store().trust_repo("laptop", "~/work/app").unwrap();
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let log = seen.clone();
        let old: Ask = Arc::new(move |req| {
            let (ask, log) = (ask.clone(), log.clone());
            Box::pin(async move {
                let op = serde_json::to_value(&req).unwrap()["op"]
                    .as_str()
                    .unwrap()
                    .to_string();
                log.lock().unwrap().push(op);
                if let IpcRequest::Ping = req {
                    return Ok(IpcResponse::Pong {
                        version: "0.9.0".into(),
                        protocol: crate::ipc::PULL_TRUST_PROTOCOL - 1,
                        role: None,
                    });
                }
                ask(req).await
            })
        });
        let t = run_in(&head, Some("laptop"), Some("~/work/app")).await;
        let fake = crate::herdr::fake::FakeHerdr::new();
        fake.set_trust_prompt(Some(vec!["Down".into(), "Enter".into()]));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut p = Puller::new(
            "laptop".into(),
            false,
            Arc::new(fake.clone()),
            fast(),
            store.clone(),
            gated(old),
        );
        head_reaches(&mut p, &head, &t, TaskState::Blocked).await;
        assert_eq!(p.trust_refused.as_deref(), Some("head_too_old"));
        assert!(trust_here(&store).is_empty());
        assert!(
            !seen.lock().unwrap().iter().any(|op| op == "pull_trust"),
            "{:?}",
            seen.lock().unwrap()
        );

        // Keys pressed all the same (a copy from a newer head before the
        // old one came back): the report goes without them.
        assert!(
            store
                .claim_trust_sent(t.id, &["Down".into(), "Enter".into()])
                .unwrap()
        );
        let mut events = head.subscribe();
        p.report(true).await;
        assert_eq!(p.sent.get(&t.id).map(|r| r.3), Some(true), "delivered");
        assert!(!head.store().trust_sent(t.id).unwrap());
        assert!(!std::iter::from_fn(|| events.try_recv().ok()).any(|e| e.kind == "task.trusted"));
    }

    /// A `PullTrust` that fails on the way (a timeout, a dropped bridge, a
    /// head mid-restart) keeps the last copy, so the trust prompt is still
    /// answered; only a head that refuses the machine clears it.
    #[tokio::test]
    async fn a_failed_trust_fetch_keeps_the_last_copy_a_refusal_clears_it() {
        for code in ["timeout", "runtime_error", "daemon_not_running"] {
            let tmp = tempfile::tempdir().unwrap();
            let (head, ask) = pull_head(tmp.path()).await;
            head.store().trust_repo("laptop", "~/work/app").unwrap();
            let fails = Arc::new(Mutex::new(code));
            let failing = fails.clone();
            let flaky: Ask = Arc::new(move |req| {
                let (ask, code) = (ask.clone(), *failing.lock().unwrap());
                Box::pin(async move {
                    if let IpcRequest::PullTrust { .. } = req {
                        return Err(CliError::err(code, "on the way"));
                    }
                    ask(req).await
                })
            });
            let store = Arc::new(Store::open_in_memory().unwrap());
            // The copy an earlier fetch left.
            store
                .replace_trusted_repos(&[crate::store::TrustedRepo {
                    machine: "laptop".into(),
                    repo: "~/work/app".into(),
                    trusted_at: chrono::Utc::now(),
                }])
                .unwrap();
            let t = run_in(&head, Some("laptop"), Some("~/work/app")).await;
            let fake = crate::herdr::fake::FakeHerdr::new();
            fake.set_trust_prompt(Some(vec!["Down".into(), "Enter".into()]));
            let mut p = Puller::new(
                "laptop".into(),
                false,
                Arc::new(fake.clone()),
                fast(),
                store.clone(),
                flaky,
            );
            head_reaches(&mut p, &head, &t, TaskState::Running).await;
            assert_eq!(
                trust_here(&store),
                [("laptop".to_string(), "~/work/app".to_string())],
                "{code}"
            );
            assert_eq!(p.trust_refused, None, "{code}");

            // Refused now: nothing left to answer with.
            *fails.lock().unwrap() = "not_pull_machine";
            let t2 = run_in(&head, Some("laptop"), Some("~/work/app")).await;
            head_reaches(&mut p, &head, &t2, TaskState::Blocked).await;
            assert!(trust_here(&store).is_empty(), "{code}");
            assert_eq!(p.trust_refused.as_deref(), Some("not_pull_machine"));
        }
    }

    /// `pastor trust remove` on the head reaches the pull machine at its
    /// next fetch: a later task of that repo is left blocked for a person.
    #[tokio::test]
    async fn trust_removed_on_the_head_is_removed_here() {
        let tmp = tempfile::tempdir().unwrap();
        let (head, ask) = pull_head(tmp.path()).await;
        head.store().trust_repo("laptop", "~/work/app").unwrap();
        let t = run_in(&head, Some("laptop"), Some("~/work/app")).await;
        let fake = crate::herdr::fake::FakeHerdr::new();
        fake.set_trust_prompt(Some(vec!["Down".into(), "Enter".into()]));
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut p = Puller::new(
            "laptop".into(),
            false,
            Arc::new(fake.clone()),
            fast(),
            store.clone(),
            ask,
        );
        head_reaches(&mut p, &head, &t, TaskState::Running).await;
        assert_eq!(trust_here(&store).len(), 1);

        assert!(head.store().untrust("laptop", "~/work/app").unwrap());
        let t2 = run_in(&head, Some("laptop"), Some("~/work/app")).await;
        head_reaches(&mut p, &head, &t2, TaskState::Blocked).await;
        assert!(trust_here(&store).is_empty(), "{:?}", trust_here(&store));
        let pane = head
            .store()
            .get_task(t2.id)
            .unwrap()
            .unwrap()
            .pane_id
            .unwrap();
        assert!(keys_sent(&fake, &pane).is_empty());
    }

    /// Trust keys pressed before a restart of the serve go to the head as
    /// they were pressed, even when the config now names other keys or
    /// none, and the head emits `task.trusted` once however many restarts
    /// report them.
    #[tokio::test]
    async fn trust_keys_pressed_before_a_restart_are_reported_as_pressed_once() {
        let tmp = tempfile::tempdir().unwrap();
        let (head, ask) = pull_head(tmp.path()).await;
        let t = run_in(&head, Some("laptop"), Some("~/work/app")).await;
        let IpcResponse::Tasks(claimed) = head
            .handle(IpcRequest::TaskClaim {
                machine: "laptop".into(),
                free_slots: 1,
                flock_work: false,
                herdr_version: None,
                pastor_version: None,
                workdir_sets: false,
            })
            .await
        else {
            panic!()
        };
        // What the serve before the restart left: the task blocked on its
        // prompt, the keys pressed, no report of them yet.
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut row = store.adopt_claimed(&claimed[0]).unwrap();
        row.state = TaskState::Blocked;
        row.pane_id = Some("p-1".into());
        row.prompt_pending = true;
        store.update_task(&mut row).unwrap();
        let pressed = vec!["Down".to_string(), "Enter".to_string()];
        assert!(store.claim_trust_sent(t.id, &pressed).unwrap());
        let with_keys = |keys: Vec<String>| {
            let mut agents = crate::config::Agents::default();
            agents.0.insert(
                "claude".into(),
                crate::config::AgentDef {
                    trust_keys: Some(keys),
                    ..Default::default()
                },
            );
            MachineSettings { agents, ..fast() }
        };
        let mut events = head.subscribe();
        // Restarted with the trust keys removed, then with others.
        for settings in [with_keys(vec![]), with_keys(vec!["Escape".into()])] {
            let mut p = Puller::new(
                "laptop".into(),
                false,
                Arc::new(crate::herdr::fake::FakeHerdr::new()),
                settings,
                store.clone(),
                ask.clone(),
            );
            p.report(true).await;
            assert_eq!(p.sent.get(&t.id).map(|r| r.3), Some(true), "delivered");
        }
        let trusted: Vec<PastorEvent> = std::iter::from_fn(|| events.try_recv().ok())
            .filter(|e| e.kind == "task.trusted")
            .collect();
        assert_eq!(trusted.len(), 1, "{trusted:?}");
        assert_eq!(
            trusted[0].detail,
            Some(serde_json::json!({ "keys": pressed }))
        );
    }

    /// A pull machine dispatches a worktree task itself, never through the
    /// head's own `dispatch`: the checkout it opens is recorded only here,
    /// in this machine's own store, until its `TaskReport` carries it to
    /// the head. The head's row should pick up the same checkout, not stay
    /// without one.
    #[tokio::test]
    async fn a_pull_machine_s_worktree_report_carries_its_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let (head, ask) = pull_head(tmp.path()).await;
        let pinned = run_pinned_worktree(&head, Some("laptop")).await;
        let fake = crate::herdr::fake::FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut p = Puller::new(
            "laptop".into(),
            false,
            Arc::new(fake.clone()),
            fast(),
            store.clone(),
            ask,
        );
        head_reaches(&mut p, &head, &pinned, TaskState::Running).await;
        let local = store.get_task(pinned.id).unwrap().unwrap();
        let checkout = local
            .spec
            .checkout
            .expect("the pull machine's own dispatch opened a checkout");
        let row = head.store().get_task(pinned.id).unwrap().unwrap();
        assert_eq!(
            row.spec.checkout,
            Some(checkout),
            "the head's row carries the checkout only its report could tell it"
        );
    }

    /// The workdir bundles a start here left out go to the head with the
    /// task's report, so the head's row says what was placed; a head too
    /// old to take them still gets the report, without them.
    #[tokio::test]
    async fn a_report_carries_the_workdir_bundles_left_out_here() {
        for protocol in [
            crate::ipc::IPC_PROTOCOL,
            crate::ipc::WORKDIR_PULL_PROTOCOL - 1,
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let (head, ask) = pull_head(tmp.path()).await;
            let pinned = run_pinned_worktree(&head, Some("laptop")).await;
            let seen = Arc::new(Mutex::new(Vec::<Vec<crate::task::WorkdirSkip>>::new()));
            let log = seen.clone();
            let ask: Ask = Arc::new(move |req| {
                let (ask, log) = (ask.clone(), log.clone());
                Box::pin(async move {
                    if let IpcRequest::Ping = req {
                        return Ok(IpcResponse::Pong {
                            version: "0.10.0".into(),
                            protocol,
                            role: None,
                        });
                    }
                    if let IpcRequest::TaskReport {
                        workdir_skipped, ..
                    } = &req
                    {
                        log.lock().unwrap().push(workdir_skipped.clone());
                    }
                    ask(req).await
                })
            });
            let fake = crate::herdr::fake::FakeHerdr::new();
            let store = Arc::new(Store::open_in_memory().unwrap());
            let mut p = Puller::new(
                "laptop".into(),
                false,
                Arc::new(fake.clone()),
                fast(),
                store.clone(),
                gated(ask),
            );
            head_reaches(&mut p, &head, &pinned, TaskState::Running).await;
            let mut local = store.get_task(pinned.id).unwrap().unwrap();
            local.spec.workdir_bundles = vec![crate::task::WorkdirBundle {
                skipped: Some("shared folder (/w)".into()),
                ..crate::task::WorkdirBundle::named("repo-notes")
            }];
            store.update_task(&mut local).unwrap();
            seen.lock().unwrap().clear();
            p.report(false).await;
            let sent = seen.lock().unwrap().clone();
            let skip = crate::task::WorkdirSkip {
                id: "repo-notes".into(),
                why: "shared folder (/w)".into(),
            };
            if protocol >= crate::ipc::WORKDIR_PULL_PROTOCOL {
                assert_eq!(sent, [vec![skip]], "a change of skips is a report");
            } else {
                assert_eq!(sent, [Vec::new()], "refused before the send, then without");
            }
            assert_eq!(
                p.sent.get(&pinned.id).map(|r| r.4.len()),
                Some(1),
                "delivered"
            );
        }
    }

    /// A machine the head does not have as a pull machine never starts an
    /// actor, so its herdr is never asked anything.
    #[tokio::test]
    async fn a_machine_the_head_refuses_starts_no_actor() {
        let tmp = tempfile::tempdir().unwrap();
        let (_head, ask) = pull_head(tmp.path()).await;
        let fake = crate::herdr::fake::FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut p = Puller::new(
            "desk".into(),
            true,
            Arc::new(fake.clone()),
            fast(),
            store,
            ask,
        );
        p.pass().await;
        p.pass().await;
        assert!(p.actor.is_none());
        assert_eq!(p.refused.as_deref(), Some("unknown_machine"));
        assert!(fake.requests().is_empty());
    }

    /// Its own jobs' requests, a ping that says what it is, and a refusal
    /// for the rest, which is the head's; an agent pastor started may not
    /// run its jobs.
    #[tokio::test]
    async fn a_shepherd_answers_only_for_its_jobs() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        paths.ensure().unwrap();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let (events, _) = broadcast::channel(4);
        let ask = head(Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(true)));
        let fleet = Arc::new(Fleet::headless(store.clone(), ask));
        let config = PastorConfig::default();
        let scheduler = Scheduler::new(paths.clone(), &config, store, fleet, events)
            .headless()
            .spawn();
        let s = Shepherd {
            scheduler,
            head: "user@pi-1".into(),
        };
        let IpcResponse::Pong { role, protocol, .. } =
            s.answer(IpcRequest::Ping, Default::default()).await
        else {
            panic!()
        };
        assert_eq!(role.as_deref(), Some(SHEPHERD_ROLE));
        assert_eq!(protocol, IPC_PROTOCOL);
        assert!(matches!(
            s.answer(IpcRequest::JobList, Default::default()).await,
            IpcResponse::Jobs(j) if j.is_empty()
        ));
        let code = |r: IpcResponse| match r {
            IpcResponse::Error { code, message } => (code, message),
            other => panic!("{other:?}"),
        };
        let (c, m) = code(
            s.answer(
                IpcRequest::List {
                    filter: Default::default(),
                },
                Default::default(),
            )
            .await,
        );
        assert_eq!(c, "shepherd_unsupported");
        assert!(m.contains("user@pi-1"), "{m}");
        let (c, _) = code(
            s.answer(
                IpcRequest::JobRun { name: "x".into() },
                crate::ipc::Caller::task(Some("t-3")),
            )
            .await,
        );
        assert_eq!(c, "agent_refused");
    }

    /// A headless `pastor serve`'s `submit_to_head` refuses a head older
    /// than `PROFILE_PROTOCOL` before it ever sends the `JobSubmit`: such a
    /// head's `serde` drops `AgentChoice.profile` from it instead of
    /// refusing the request, so a job with a profile would start unenforced
    /// rather than fail loudly.
    #[tokio::test]
    async fn submit_to_head_refuses_a_head_that_predates_profiles() {
        let old_head: Ask = Arc::new(|req| {
            Box::pin(async move {
                assert!(matches!(req, IpcRequest::Ping), "{req:?}");
                Ok(IpcResponse::Pong {
                    version: "0.5.0".into(),
                    protocol: crate::ipc::PROFILE_PROTOCOL - 1,
                    role: None,
                })
            })
        });
        let store = Arc::new(Store::open_in_memory().unwrap());
        let fleet = Fleet::headless(store, gated(old_head));
        let job = crate::config::job::Job::parse(
            "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nprofile = \"ci\"\nprompt = \"p\"\n",
            "x",
            &crate::config::Defaults::default(),
            &crate::connector::Builtins,
        )
        .unwrap();
        let err = fleet
            .submit_to_head(&job, vec![serde_json::json!({})])
            .await
            .unwrap_err();
        assert_eq!(
            err.downcast_ref::<CliError>().map(|e| e.code.as_str()),
            Some("head_too_old"),
            "{err}"
        );
    }

    /// The same refusal for `[dispatch] preempt = true`: a head before
    /// `PREEMPT_PROTOCOL` does not know the field and its `DispatchTable`
    /// would refuse the whole `JobSubmit` as `invalid_dispatch` instead of
    /// this clear `head_too_old`.
    #[tokio::test]
    async fn submit_to_head_refuses_a_head_that_predates_preempt() {
        let old_head: Ask = Arc::new(|req| {
            Box::pin(async move {
                assert!(matches!(req, IpcRequest::Ping), "{req:?}");
                Ok(IpcResponse::Pong {
                    version: "0.5.0".into(),
                    protocol: crate::ipc::PREEMPT_PROTOCOL - 1,
                    role: None,
                })
            })
        });
        let store = Arc::new(Store::open_in_memory().unwrap());
        let fleet = Fleet::headless(store, gated(old_head));
        let job = crate::config::job::Job::parse(
            "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\npreempt = true\nprompt = \"p\"\n",
            "x",
            &crate::config::Defaults::default(),
            &crate::connector::Builtins,
        )
        .unwrap();
        let err = fleet
            .submit_to_head(&job, vec![serde_json::json!({})])
            .await
            .unwrap_err();
        assert_eq!(
            err.downcast_ref::<CliError>().map(|e| e.code.as_str()),
            Some("head_too_old"),
            "{err}"
        );
    }

    /// The same guard for a label: a head before `LABEL_PROTOCOL` drops
    /// `[dispatch] label` (serde skips the unknown field) and names the
    /// workspace by its own default instead of the job's chosen name.
    #[tokio::test]
    async fn submit_to_head_refuses_a_head_that_predates_labels() {
        let old_head: Ask = Arc::new(|req| {
            Box::pin(async move {
                assert!(matches!(req, IpcRequest::Ping), "{req:?}");
                Ok(IpcResponse::Pong {
                    version: "0.5.0".into(),
                    protocol: crate::ipc::LABEL_PROTOCOL - 1,
                    role: None,
                })
            })
        });
        let store = Arc::new(Store::open_in_memory().unwrap());
        let fleet = Fleet::headless(store, gated(old_head));
        let job = crate::config::job::Job::parse(
            "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nlabel = \"{{ item.key }}\"\nprompt = \"p\"\n",
            "x",
            &crate::config::Defaults::default(),
            &crate::connector::Builtins,
        )
        .unwrap();
        let err = fleet
            .submit_to_head(&job, vec![serde_json::json!({})])
            .await
            .unwrap_err();
        assert_eq!(
            err.downcast_ref::<CliError>().map(|e| e.code.as_str()),
            Some("head_too_old"),
            "{err}"
        );
    }

    /// A head at `protocol` that records each request's op and answers every
    /// one with its pong.
    fn pong_head(protocol: u32) -> (Ask, Arc<std::sync::Mutex<Vec<String>>>) {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let ask: Ask = Arc::new(move |req| {
            let log = log.clone();
            Box::pin(async move {
                let op = serde_json::to_value(&req).unwrap()["op"]
                    .as_str()
                    .unwrap()
                    .to_string();
                log.lock().unwrap().push(op);
                Ok(IpcResponse::Pong {
                    version: "0.5.0".into(),
                    protocol,
                    role: None,
                })
            })
        });
        (ask, seen)
    }

    /// The claim a pull machine sends each tick is checked like any CLI
    /// request: a head from before pull machines gets only the ping, and
    /// the claim is refused `head_too_old` naming them.
    #[tokio::test]
    async fn a_claim_refuses_a_head_that_predates_pull_machines() {
        let (head, seen) = pong_head(crate::ipc::PULL_PROTOCOL - 1);
        let err = gated(head)(IpcRequest::TaskClaim {
            machine: "m".into(),
            free_slots: 1,
            flock_work: false,
            herdr_version: None,
            pastor_version: None,
            workdir_sets: false,
        })
        .await
        .unwrap_err();
        let err = err.downcast_ref::<CliError>().unwrap();
        assert_eq!(err.code, "head_too_old");
        assert!(err.message.contains("pull machines"), "{}", err.message);
        assert_eq!(*seen.lock().unwrap(), ["ping"]);
    }

    /// A job naming a model, a profile and a label is checked with one
    /// ping, and a head new enough for all three gets the submit.
    #[tokio::test]
    async fn submit_to_head_pings_once() {
        let (head, seen) = pong_head(crate::ipc::IPC_PROTOCOL);
        let fleet = Fleet::headless(Arc::new(Store::open_in_memory().unwrap()), gated(head));
        let job = crate::config::job::Job::parse(
            "every = \"1h\"\n[connector]\nuse = \"clock\"\n[dispatch]\nprofile = \"ci\"\nlabel = \"x\"\npreempt = true\nprompt = \"p\"\n",
            "x",
            &crate::config::Defaults::default(),
            &crate::connector::Builtins,
        )
        .unwrap();
        // The fake head answers the submit with a pong, which is not what a
        // submit gets back: only what was sent matters here.
        let _ = fleet
            .submit_to_head(&job, vec![serde_json::json!({})])
            .await;
        assert_eq!(*seen.lock().unwrap(), ["ping", "job_submit"]);
    }

    /// What `pastor join` leaves for the shepherd: an outcome that says ok.
    fn left_outcome(ok: bool) -> crate::bootstrap::Outcome {
        crate::bootstrap::Outcome {
            ok,
            isolation: crate::bootstrap::Isolation::None,
            steps: if ok {
                vec![crate::bootstrap::StepResult::done("keys")]
            } else {
                vec![crate::bootstrap::StepResult::failed(
                    "herdr",
                    "no herdr",
                    "install it",
                )]
            },
            machine_key: None,
            agent_key: None,
            host_key: None,
            herdr_version: Some("0.9.5".into()),
            pastor_version: Some("1.0.0".into()),
            check: None,
        }
    }

    /// A joined pull machine on the head, pending until its report lands.
    fn pending(head: &Daemon) {
        head.store()
            .put_machine_state(&crate::store::MachineState {
                machine: "laptop".into(),
                bootstrap: crate::store::BootstrapState::Pending,
                isolation: None,
                failed_step: None,
                why: None,
                host_key: None,
                herdr_version: None,
                pastor_version: None,
                generation: 1,
                updated_at: chrono::Utc::now(),
            })
            .unwrap();
    }

    fn puller_with_report(ask: Ask, file: &std::path::Path) -> Puller {
        let mut p = Puller::new(
            "laptop".into(),
            false,
            Arc::new(crate::herdr::fake::FakeHerdr::new()),
            fast(),
            Arc::new(Store::open_in_memory().unwrap()),
            ask,
        );
        p.report_file = Some(file.to_path_buf());
        p
    }

    /// The report `pastor join` left is sent on the first pass and deleted
    /// once the head took it: the machine is no longer pending.
    #[tokio::test]
    async fn the_bootstrap_report_join_left_is_sent_then_deleted() {
        let tmp = tempfile::tempdir().unwrap();
        let (head, ask) = pull_head(tmp.path()).await;
        pending(&head);
        let file = crate::bootstrap::report_file(tmp.path());
        std::fs::write(&file, serde_json::to_string(&left_outcome(true)).unwrap()).unwrap();
        let mut p = puller_with_report(ask, &file);
        p.send_bootstrap_report().await;
        assert!(!file.exists(), "deleted once the head has it");
        let state = head.store().machine_state("laptop").unwrap().unwrap();
        assert_eq!(state.bootstrap, crate::store::BootstrapState::Ok);
        assert_eq!(state.herdr_version.as_deref(), Some("0.9.5"));
    }

    /// A head that cannot be reached, or that is restarting, leaves the file
    /// for the next pass, which then sends it.
    #[tokio::test]
    async fn the_bootstrap_report_is_kept_until_the_head_answers() {
        for code in [
            "head_unreachable",
            "no_head",
            "timeout",
            "runtime_error",
            "daemon_not_running",
            "head_too_old",
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let (head, ask) = pull_head(tmp.path()).await;
            pending(&head);
            let file = crate::bootstrap::report_file(tmp.path());
            let text = serde_json::to_string(&left_outcome(false)).unwrap();
            std::fs::write(&file, &text).unwrap();
            let down: Ask =
                Arc::new(move |_| Box::pin(async move { Err(CliError::err(code, "on the way")) }));
            let mut p = puller_with_report(down, &file);
            p.send_bootstrap_report().await;
            assert_eq!(std::fs::read_to_string(&file).unwrap(), text, "{code}");
            assert_eq!(
                head.store()
                    .machine_state("laptop")
                    .unwrap()
                    .unwrap()
                    .bootstrap,
                crate::store::BootstrapState::Pending
            );
            // The head back: the same file goes.
            p.ask = ask;
            p.send_bootstrap_report().await;
            assert!(!file.exists(), "{code}");
            assert_eq!(
                head.store()
                    .machine_state("laptop")
                    .unwrap()
                    .unwrap()
                    .bootstrap,
                crate::store::BootstrapState::Failed
            );
        }
    }

    /// A report the head refuses is kept aside for a person and not sent
    /// again; one this pastor cannot read is dropped.
    #[tokio::test]
    async fn a_refused_or_unreadable_bootstrap_report_is_not_sent_again() {
        let tmp = tempfile::tempdir().unwrap();
        let file = crate::bootstrap::report_file(tmp.path());
        let asked = Arc::new(Mutex::new(0));
        let count = asked.clone();
        let refusing: Ask = Arc::new(move |_| {
            *count.lock().unwrap() += 1;
            Box::pin(async move { Err(CliError::err("host_key_mismatch", "not that host")) })
        });
        std::fs::write(&file, serde_json::to_string(&left_outcome(true)).unwrap()).unwrap();
        let mut p = puller_with_report(refusing, &file);
        p.send_bootstrap_report().await;
        p.send_bootstrap_report().await;
        assert_eq!(*asked.lock().unwrap(), 1);
        assert!(!file.exists());
        assert!(tmp.path().join("bootstrap.json.refused").exists());

        std::fs::write(&file, "{ not an outcome").unwrap();
        p.send_bootstrap_report().await;
        assert_eq!(*asked.lock().unwrap(), 1, "never asked the head");
        assert!(!file.exists());
    }

    /// A pass sends the report before it claims, so the machine's first
    /// claim already finds it past pending.
    #[tokio::test]
    async fn a_pass_sends_the_bootstrap_report_before_the_claim() {
        let tmp = tempfile::tempdir().unwrap();
        let (_head, ask) = pull_head(tmp.path()).await;
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let log = seen.clone();
        let logged: Ask = Arc::new(move |req| {
            let (ask, log) = (ask.clone(), log.clone());
            Box::pin(async move {
                let op = serde_json::to_value(&req).unwrap()["op"]
                    .as_str()
                    .unwrap()
                    .to_string();
                log.lock().unwrap().push(op);
                ask(req).await
            })
        });
        let file = crate::bootstrap::report_file(tmp.path());
        std::fs::write(&file, serde_json::to_string(&left_outcome(true)).unwrap()).unwrap();
        let mut p = puller_with_report(logged, &file);
        p.pass().await;
        let ops = seen.lock().unwrap().clone();
        let report = ops.iter().position(|o| o == "bootstrap_report").unwrap();
        let claim = ops.iter().position(|o| o == "task_claim").unwrap();
        assert!(report < claim, "{ops:?}");
    }

    /// What the tests below log, as the serve's log would read.
    #[derive(Clone, Default)]
    struct Logs(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Logs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
        type Writer = Logs;
        fn make_writer(&'a self) -> Logs {
            self.clone()
        }
    }

    impl Logs {
        /// Captures this thread's logs at `info` and up while the guard
        /// lives; a `#[tokio::test]` runs its tasks on this thread.
        fn capture() -> (Logs, tracing::subscriber::DefaultGuard) {
            let logs = Logs::default();
            let sub = tracing_subscriber::fmt()
                .with_writer(logs.clone())
                .with_ansi(false)
                .with_max_level(tracing::Level::INFO)
                .finish();
            (logs, tracing::subscriber::set_default(sub))
        }

        /// The lines at `level` that hold `text`.
        fn count(&self, level: &str, text: &str) -> usize {
            String::from_utf8_lossy(&self.0.lock().unwrap())
                .lines()
                .filter(|l| l.contains(level) && l.contains(text))
                .count()
        }
    }

    /// `ask`, except that while `down` is set every request times out on
    /// the way, as through a head too slow to answer.
    fn silent_while(ask: Ask, down: Arc<Mutex<bool>>) -> Ask {
        Arc::new(move |req| {
            let (ask, down) = (ask.clone(), *down.lock().unwrap());
            Box::pin(async move {
                if down {
                    return Err(CliError::err("timeout", "no answer within 60s"));
                }
                ask(req).await
            })
        })
    }

    /// A request the head does not answer is a warning, once however many
    /// ticks the silence lasts, and the head's next answer is logged; a
    /// second silence warns again. A refusal is an answer.
    #[tokio::test(start_paused = true)]
    async fn a_report_the_head_drops_warns_once_per_outage() {
        let tmp = tempfile::tempdir().unwrap();
        let (head, ask) = pull_head(tmp.path()).await;
        let down = Arc::new(Mutex::new(false));
        let t = run_pinned(&head, Some("laptop")).await;
        let fake = crate::herdr::fake::FakeHerdr::new();
        let mut p = Puller::new(
            "laptop".into(),
            false,
            Arc::new(fake.clone()),
            fast(),
            Arc::new(Store::open_in_memory().unwrap()),
            silent_while(ask, down.clone()),
        );
        head_reaches(&mut p, &head, &t, TaskState::Running).await;
        let (logs, _guard) = Logs::capture();

        *down.lock().unwrap() = true;
        for _ in 0..3 {
            assert!(!p.pass().await, "the head did not answer");
        }
        assert_eq!(logs.count("WARN", "pull_head_silent"), 1);
        assert_eq!(logs.count("INFO", "answers this machine again"), 0);

        *down.lock().unwrap() = false;
        assert!(p.pass().await);
        p.pass().await;
        assert_eq!(logs.count("INFO", "answers this machine again"), 1);

        *down.lock().unwrap() = true;
        p.pass().await;
        assert_eq!(logs.count("WARN", "pull_head_silent"), 2);
    }

    /// A head that does not answer costs a tick one timeout: the reports,
    /// the bootstrap report, the claim and the event read after the first
    /// unanswered request are left for the next tick, not each waited out
    /// in turn.
    #[tokio::test(start_paused = true)]
    async fn a_tick_with_a_silent_head_waits_out_one_timeout() {
        const WAIT: Duration = Duration::from_secs(60);
        let tmp = tempfile::tempdir().unwrap();
        let (head, ask) = pull_head(tmp.path()).await;
        let t = run_pinned(&head, Some("laptop")).await;
        let fake = crate::herdr::fake::FakeHerdr::new();
        let store = Arc::new(Store::open_in_memory().unwrap());
        let down = Arc::new(Mutex::new(false));
        let asked = Arc::new(Mutex::new(Vec::<String>::new()));
        let (log, flag) = (asked.clone(), down.clone());
        let slow: Ask = Arc::new(move |req| {
            let (ask, log, down) = (ask.clone(), log.clone(), *flag.lock().unwrap());
            Box::pin(async move {
                if !down {
                    return ask(req).await;
                }
                let op = serde_json::to_value(&req).unwrap()["op"]
                    .as_str()
                    .unwrap()
                    .to_string();
                log.lock().unwrap().push(op);
                tokio::time::sleep(WAIT).await;
                Err(CliError::err("timeout", "no answer"))
            })
        });
        let mut p = Puller::new(
            "laptop".into(),
            false,
            Arc::new(fake.clone()),
            fast(),
            store,
            gated(slow.clone()),
        );
        head_reaches(&mut p, &head, &t, TaskState::Running).await;
        // A bootstrap report left for the head, after the head took the rest.
        let dir = tempfile::tempdir().unwrap();
        let file = crate::bootstrap::report_file(dir.path());
        std::fs::write(&file, serde_json::to_string(&left_outcome(true)).unwrap()).unwrap();
        p.report_file = Some(file.clone());
        // A gate with no pong kept, as after a minute: the first request
        // pings.
        let ask = gated(slow);
        p.ask = ask.clone();
        let (mut f, _rx) = follower(ask);
        // Some events were read already, as on a machine that has run.
        f.store.set_meta(CURSOR_KEY, "0").unwrap();

        *down.lock().unwrap() = true;
        let started = tokio::time::Instant::now();
        tick(&mut f, &mut p).await;
        assert_eq!(started.elapsed(), WAIT, "one wait, not one per request");
        assert_eq!(*asked.lock().unwrap(), ["ping"]);
        assert_eq!(f.reachable, Some(false), "the hooks' side knows too");
        assert!(file.exists(), "the bootstrap report waits for the head");
    }

    /// The bootstrap report is a request to the head like the others: left
    /// unanswered it stops the tick before the claim, and is kept.
    #[tokio::test(start_paused = true)]
    async fn a_silent_head_stops_the_tick_at_the_bootstrap_report() {
        let tmp = tempfile::tempdir().unwrap();
        let (_head, ask) = pull_head(tmp.path()).await;
        let down = Arc::new(Mutex::new(true));
        let asked = Arc::new(Mutex::new(Vec::<String>::new()));
        let log = asked.clone();
        let inner = silent_while(ask, down.clone());
        let logged: Ask = Arc::new(move |req| {
            let (inner, log) = (inner.clone(), log.clone());
            Box::pin(async move {
                let op = serde_json::to_value(&req).unwrap()["op"]
                    .as_str()
                    .unwrap()
                    .to_string();
                log.lock().unwrap().push(op);
                inner(req).await
            })
        });
        let file = crate::bootstrap::report_file(tmp.path());
        std::fs::write(&file, serde_json::to_string(&left_outcome(true)).unwrap()).unwrap();
        let mut p = puller_with_report(logged, &file);
        assert!(!p.pass().await);
        assert_eq!(*asked.lock().unwrap(), ["bootstrap_report"]);
        assert!(file.exists());
        *down.lock().unwrap() = false;
        assert!(p.pass().await);
        assert!(!file.exists());
    }

    /// The head counts a silent pull machine lost and marks its running
    /// task stale; the machine's next report of the agent at work puts it
    /// back to running.
    #[tokio::test(start_paused = true)]
    async fn a_task_staled_by_a_silence_runs_again_on_the_next_report() {
        let tmp = tempfile::tempdir().unwrap();
        let (head, ask) = pull_head(tmp.path()).await;
        let t = run_pinned(&head, Some("laptop")).await;
        let fake = crate::herdr::fake::FakeHerdr::new();
        let mut p = Puller::new(
            "laptop".into(),
            false,
            Arc::new(fake.clone()),
            fast(),
            Arc::new(Store::open_in_memory().unwrap()),
            ask,
        );
        head_reaches(&mut p, &head, &t, TaskState::Running).await;
        assert_eq!(
            head.fleet().check_pull_lost(Duration::ZERO).await,
            ["laptop"]
        );
        assert_eq!(
            head.store().get_task(t.id).unwrap().unwrap().state,
            TaskState::Stale
        );
        p.report(true).await;
        let row = head.store().get_task(t.id).unwrap().unwrap();
        assert_eq!(row.state, TaskState::Running);
        assert_eq!(row.error, None);
    }

    /// A claim the head leaves unanswered is a silence too: the warning
    /// names it, and the tick reports it unanswered.
    #[tokio::test(start_paused = true)]
    async fn a_claim_the_head_leaves_unanswered_is_a_silence() {
        let tmp = tempfile::tempdir().unwrap();
        let (_head, ask) = pull_head(tmp.path()).await;
        let down = Arc::new(Mutex::new(true));
        let mut p = Puller::new(
            "laptop".into(),
            false,
            Arc::new(crate::herdr::fake::FakeHerdr::new()),
            fast(),
            Arc::new(Store::open_in_memory().unwrap()),
            silent_while(ask, down.clone()),
        );
        let (logs, _guard) = Logs::capture();
        assert!(!p.pass().await);
        assert_eq!(logs.count("WARN", "this machine's claim"), 1);
        assert!(p.silent.is_some());
        *down.lock().unwrap() = false;
        assert!(p.pass().await);
        assert!(p.silent.is_none());
    }

    /// A head that serves the events in `log` and, for a `TaskReport`,
    /// writes the named task's row in `tasks` and appends its state's event
    /// to `log` when the state changes, as the head's report does.
    fn head_with_reports(
        log: Arc<Mutex<Vec<EventRecord>>>,
        tasks: Arc<Mutex<HashMap<i64, Task>>>,
        down: Arc<Mutex<bool>>,
    ) -> Ask {
        Arc::new(move |req| {
            let (log, tasks, down) = (log.clone(), tasks.clone(), *down.lock().unwrap());
            Box::pin(async move {
                if down {
                    return Err(CliError::err("head_unreachable", "ssh: no route"));
                }
                match req {
                    IpcRequest::EventsSince { after, limit, .. } => {
                        let log = log.lock().unwrap();
                        let events: Vec<EventRecord> = log
                            .iter()
                            .filter(|r| r.seq > after)
                            .take(limit as usize)
                            .cloned()
                            .collect();
                        Ok(IpcResponse::Events(EventsPage {
                            events,
                            gap: false,
                            oldest: log.first().map(|r| r.seq),
                            newest: log.iter().map(|r| r.seq).max(),
                        }))
                    }
                    IpcRequest::TaskClaim { .. } => Ok(IpcResponse::Tasks(Vec::new())),
                    IpcRequest::TaskReport {
                        id, state, detail, ..
                    } => {
                        let mut tasks = tasks.lock().unwrap();
                        let task = tasks.get_mut(&id).expect("a known task");
                        let from = task.state;
                        task.state = state;
                        task.error = detail;
                        if from != state {
                            let mut log = log.lock().unwrap();
                            let seq = log.last().map_or(1, |r| r.seq + 1);
                            log.push(EventRecord {
                                kind: format!("task.{state}"),
                                ..rec(seq)
                            });
                        }
                        Ok(IpcResponse::Task(task.clone()))
                    }
                    other => panic!("unexpected in this fake: {other:?}"),
                }
            })
        })
    }

    /// A restart that finds this machine's cursor never set (the head was
    /// unreachable on the way up) must not let a tick's report go to the
    /// head before its one read of the head's events: that read would
    /// otherwise be the first ever, which treats everything it finds as
    /// history and so would skip the very `task.running` the report makes
    /// the head emit.
    #[tokio::test(start_paused = true)]
    async fn a_report_before_the_first_read_does_not_lose_its_own_event() {
        let tmp = tempfile::tempdir().unwrap();
        let (head, head_ask) = pull_head(tmp.path()).await;
        let t = run_pinned(&head, Some("laptop")).await;
        let store = Arc::new(Store::open_in_memory().unwrap());
        let mut p = Puller::new(
            "laptop".into(),
            false,
            Arc::new(crate::herdr::fake::FakeHerdr::new()),
            fast(),
            store.clone(),
            head_ask,
        );
        // Claimed before the restart this simulates: starting on the head,
        // running here, the head not told.
        p.claim().await;
        let starting = head.store().get_task(t.id).unwrap().unwrap();
        assert_eq!(starting.state, TaskState::Starting);
        let mut row = store.get_task(t.id).unwrap().unwrap();
        row.state = TaskState::Running;
        store.update_task(&mut row).unwrap();

        let log = Arc::new(Mutex::new(Vec::new()));
        let tasks = Arc::new(Mutex::new(HashMap::from([(starting.id, starting)])));
        let down = Arc::new(Mutex::new(true));
        let ask = head_with_reports(log, tasks.clone(), down.clone());
        p.ask = ask.clone();
        let (to_hooks, mut hooks_rx) = mpsc::channel(16);
        let mut follow = Follower {
            head: "user@pi-1".into(),
            ask,
            store,
            to_hooks,
            reachable: None,
        };
        follow.pass().await;
        assert_eq!(follow.reachable, Some(false));
        assert!(follow.cursor_unset());

        *down.lock().unwrap() = false;
        tick(&mut follow, &mut p).await;
        assert_eq!(tasks.lock().unwrap()[&t.id].state, TaskState::Running);
        let heard: Vec<String> = std::iter::from_fn(|| hooks_rx.try_recv().ok())
            .map(|r| r.kind)
            .collect();
        assert!(
            heard.contains(&"task.running".to_string()),
            "the report's own event must not be skipped as history: {heard:?}"
        );
    }

    /// Before the serve has ever read the head's events, that read is the
    /// tick's first request and is held to the same rule: a silent head
    /// warns once however many ticks, nothing else is asked, and the head's
    /// return is logged.
    #[tokio::test(start_paused = true)]
    async fn a_silent_head_before_the_first_read_warns_once() {
        let tmp = tempfile::tempdir().unwrap();
        let (_head, ask) = pull_head(tmp.path()).await;
        let down = Arc::new(Mutex::new(true));
        let ask = silent_while(ask, down.clone());
        let mut p = Puller::new(
            "laptop".into(),
            false,
            Arc::new(crate::herdr::fake::FakeHerdr::new()),
            fast(),
            Arc::new(Store::open_in_memory().unwrap()),
            ask.clone(),
        );
        let (mut f, _rx) = follower(ask);
        assert!(f.cursor_unset());
        let (logs, _guard) = Logs::capture();
        for _ in 0..3 {
            tick(&mut f, &mut p).await;
        }
        assert_eq!(logs.count("WARN", "pull_head_silent"), 1);
        assert!(p.silent.is_some());

        *down.lock().unwrap() = false;
        tick(&mut f, &mut p).await;
        assert!(p.silent.is_none());
        assert_eq!(logs.count("INFO", "answers this machine again"), 1);
        assert!(!f.cursor_unset(), "the head's events were read");
    }

    /// A head that refuses the event read is answering: the tick goes on to
    /// the puller's requests rather than stopping on every tick for good.
    #[tokio::test(start_paused = true)]
    async fn a_refused_first_read_does_not_stop_the_tick() {
        let tmp = tempfile::tempdir().unwrap();
        let (_head, ask) = pull_head(tmp.path()).await;
        let asked = Arc::new(Mutex::new(Vec::<String>::new()));
        let log = asked.clone();
        let refusing: Ask = Arc::new(move |req| {
            let (ask, log) = (ask.clone(), log.clone());
            Box::pin(async move {
                let op = serde_json::to_value(&req).unwrap()["op"]
                    .as_str()
                    .unwrap()
                    .to_string();
                log.lock().unwrap().push(op.clone());
                if op == "events_since" {
                    return Err(CliError::err("head_too_old", "predates events"));
                }
                ask(req).await
            })
        });
        let mut p = Puller::new(
            "laptop".into(),
            false,
            Arc::new(crate::herdr::fake::FakeHerdr::new()),
            fast(),
            Arc::new(Store::open_in_memory().unwrap()),
            refusing.clone(),
        );
        let (mut f, _rx) = follower(refusing);
        tick(&mut f, &mut p).await;
        assert!(
            asked.lock().unwrap().iter().any(|op| op == "task_claim"),
            "{:?}",
            asked.lock().unwrap()
        );
        assert!(p.silent.is_none());
    }
}
